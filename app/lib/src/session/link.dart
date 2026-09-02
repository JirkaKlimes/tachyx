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

/// A connection to [address] is being opened.
final class Dialling extends Link {
  /// Dialling [address].
  const Dialling(this.address);

  /// Where the client is calling.
  final String address;
}

/// A session is open with the host at [address].
final class Live extends Link {
  /// [session] is open with [address].
  const Live(this.address, this.session);

  /// Where the host is.
  final String address;

  /// The open session.
  final Session session;
}

/// The host at [address] could not be reached, and this is what went wrong.
final class Refused extends Link {
  /// [address] refused the connection, saying [reason].
  const Refused(this.address, this.reason);

  /// Where the client called.
  final String address;

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

  /// Opens a session with the host at [address], for a video surface of
  /// [viewport] physical pixels on a display that refreshes [refreshHz] times a
  /// second, spending at most [bitrateCeiling] bits a second.
  ///
  /// A dial while one is already under way is ignored here rather than in
  /// every screen that can ask for one.
  Future<void> dial(
    String address, {
    required (int, int) viewport,
    required int refreshHz,
    required int bitrateCeiling,
  }) async {
    if (_state is Dialling) return;
    _set(Dialling(address));
    try {
      // The host's key is pinned here, so it must outlive the run. Anywhere the
      // system may clear would silently turn every reconnection into a first
      // meeting, which is the one moment trust on first use cannot defend.
      final Directory state = await getApplicationSupportDirectory();
      final Session session = await connect(
        address: address,
        viewportWidth: viewport.$1,
        viewportHeight: viewport.$2,
        refreshHz: refreshHz,
        stateDirectory: state.path,
        bitrateCeiling: bitrateCeiling,
      );
      _set(Live(address, session));
    } on Object catch (error) {
      _set(Refused(address, describe(error)));
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
  /// of the message tells the reader nothing they can act on.
  @visibleForTesting
  static String describe(Object error) => switch (error) {
    AnyhowException(:final String message) => message,
    PanicException(:final String message) => 'the client panicked: $message',
    _ => '$error',
  };
}
