/// Where input goes once a gesture has decided what it was.
library;

import 'package:tachyx/src/rust/api/input.dart';
import 'package:tachyx/src/rust/api/session.dart';

/// Carries input to a session, and counts what it carried.
///
/// The counters are here rather than in the overlay because they are the only
/// thing the client knows for certain about its own input: they count events
/// handed over, not events it hopes arrived.
///
/// Nothing is batched here. Handing an event over is a synchronous call into
/// the client core, which merges it with the one before and sends a batch on
/// its own tick. A timer on this side would be a second one measuring the same
/// thing.
class InputPipe {
  Session? _session;

  /// The event clock reading the session's own clock is measured from.
  Duration? _origin;

  /// The latest reading seen, for the events that carry none of their own.
  Duration _latest = Duration.zero;

  int _sent = 0;
  InputEvent? _last;

  /// Input events handed to a session, or counted while there was none.
  int get sent => _sent;

  /// The most recent event, which is what tells a reader the gesture machine is
  /// alive.
  InputEvent? get last => _last;

  /// Whether anything is on the other end.
  bool get isAttached => _session != null;

  /// The latest event clock reading the pipe has seen.
  ///
  /// What a release-everything sweep is stamped with, because the reason for
  /// the sweep is that the platform took the input away without reporting a
  /// clock reading for it.
  Duration get latest => _latest;

  /// Sends everything from now on to [session].
  void attach(Session session) => _session = session;

  /// Stops sending. The counters stay, because they describe the run.
  void detach() => _session = null;

  /// Records what the user did at [at] on the event clock.
  ///
  /// [at] comes from the pointer or key event rather than from the moment this
  /// was called, so that the batching tick cannot hide inside a measurement of
  /// how long the client took.
  void send(Duration at, InputEvent event) {
    if (at > _latest) _latest = at;
    _sent++;
    _last = event;
    _session?.input(atMicros: _sessionMicros(at), event: event);
  }

  /// Records something the platform reported with no clock reading of its own,
  /// which is everything the on-screen keyboard produces.
  void sendUnstamped(InputEvent event) => send(_latest, event);

  BigInt _sessionMicros(Duration at) {
    final Duration origin = _origin ??= at;
    final int micros = (at - origin).inMicroseconds;
    return BigInt.from(micros < 0 ? 0 : micros);
  }
}
