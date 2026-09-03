/// How far the client has got with a host.
library;

import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge.dart'
    show AnyhowException, PanicException;
import 'package:path_provider/path_provider.dart';

import 'package:tachyx/src/rust/api/session.dart';

/// Where the client is with a host.
///
/// A sealed hierarchy rather than a state field beside a nullable session and a
/// nullable error, so that "connected with no session" cannot be written down.
@immutable
sealed class Link {
  const Link();
}

/// Nothing has been attempted.
final class Offline extends Link {
  /// The state a freshly started client is in.
  const Offline();
}

/// A connection to [host] is being opened.
final class Dialling extends Link {
  /// Dialling [host], which is called [name].
  const Dialling(this.host, this.name);

  /// The host's identifier, which is what is dialled. Not an address: a host is
  /// reached by trying what it gave at pairing and then by asking a relay.
  final String host;

  /// What that host calls itself, for the screen.
  final String name;
}

/// A session is open with [host].
final class Live extends Link {
  /// [session] is open with [host], which is called [name].
  const Live(this.host, this.name, this.session);

  /// The host's identifier.
  final String host;

  /// What it calls itself.
  final String name;

  /// The open session.
  final Session session;
}

/// [host] could not be reached, and this is what went wrong.
final class Refused extends Link {
  /// [host] refused the connection, saying [reason].
  const Refused(this.host, this.name, this.reason);

  /// The host's identifier.
  final String host;

  /// What it calls itself.
  final String name;

  /// What the client was told, in the words it was told in.
  final String reason;
}

/// Running with no host at all, so that input can be exercised on a device.
///
/// Nothing is sent anywhere. It is a separate state rather than a flag on
/// [Live] because a session that is not there cannot be asked for statistics,
/// and the overlay has to say so rather than show zeros.
final class Detached extends Link {
  /// Running with no host.
  const Detached();
}

/// Opens sessions.
///
/// Which hosts are worth remembering is the settings store's business; this
/// holds only where the client is with the one it is talking to now.
class HostLink extends ChangeNotifier {
  /// The port a host listens on unless told otherwise.
  ///
  /// Must match `tachyx_hostd::config::DEFAULT_PORT`. A client whose default
  /// differs from the daemon's leaves every first connection failing for a
  /// reason the person typing the address cannot see.
  static const int defaultPort = 47989;

  Link _state = const Offline();

  /// Where the client is with a host.
  Link get state => _state;

  /// Opens a session with [host], for a video surface of [viewport] physical
  /// pixels on a display that refreshes [refreshHz] times a second, spending at
  /// most [bitrateCeiling] bits a second.
  ///
  /// [host] is a device identifier, not an address. The client tries the
  /// addresses that host gave at pairing and then asks a [relays] relay, and
  /// either way the identifier is the only half that is checked.
  ///
  /// A dial while one is already under way is ignored here rather than in every
  /// screen that can ask for one.
  Future<void> dial(
    String host, {
    required String name,
    required (int, int) viewport,
    required int refreshHz,
    required int bitrateCeiling,
    required List<String> relays,
  }) async {
    if (_state is Dialling) return;
    _set(Dialling(host, name));
    try {
      // This client's own key lives here, and it is the key the host paired
      // with. A directory the system may clear would take it, and a host that no
      // longer knows this device refuses every session until it is paired again.
      final Directory state = await getApplicationSupportDirectory();
      final Session session = await connect(
        device: host,
        viewportWidth: viewport.$1,
        viewportHeight: viewport.$2,
        refreshHz: refreshHz,
        stateDirectory: state.path,
        relays: relays,
        bitrateCeiling: bitrateCeiling,
      );
      _set(Live(host, name, session));
    } on Object catch (error) {
      _set(Refused(host, name, describe(error)));
    }
  }

  /// Runs with no host, for exercising input on a device.
  void detach() => _set(const Detached());

  /// Ends whatever the client was doing.
  void hangUp() {
    final Link was = _state;
    if (was is Live) was.session.dispose();
    _set(const Offline());
  }

  void _set(Link state) {
    _state = state;
    notifyListeners();
  }

  /// What went wrong, in the words the thing that failed used.
  ///
  /// A failure from Rust arrives wrapped, and the wrapper's class name in front
  /// of the message tells the reader nothing they can act on. Shared with
  /// [HostBook], because a pairing that failed has to say the same kind of
  /// sentence a dial that failed says.
  static String describe(Object error) => switch (error) {
    AnyhowException(:final String message) => message,
    PanicException(:final String message) => 'the client panicked: $message',
    _ => '$error',
  };
}
