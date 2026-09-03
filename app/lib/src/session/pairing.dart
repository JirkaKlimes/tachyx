/// How far a pairing has got.
library;

import 'package:flutter/foundation.dart';

/// Where the client is in a pairing.
///
/// A sealed hierarchy rather than a state field beside a nullable name and a
/// nullable error, so that "paired with nobody" cannot be written down. The
/// same shape as [Link], for the same reason.
@immutable
sealed class Pairing {
  const Pairing();
}

/// Nothing has been attempted.
final class Unpaired extends Pairing {
  /// The state a freshly opened pairing screen is in.
  const Unpaired();
}

/// A code has been given and the exchange is running.
///
/// It carries no code. A pairing secret on a screen is one a shoulder can read
/// and a screenshot can keep, and there is nothing the user can do with it once
/// it has been handed over.
final class Exchanging extends Pairing {
  /// The exchange is running.
  const Exchanging();
}

/// The two devices know each other.
final class Paired extends Pairing {
  /// [host] is now paired, and both screens show [words].
  const Paired(this.host, this.device, this.words);

  /// What the host calls itself.
  final String host;

  /// The host's identifier, in the short form a person reads.
  final String device;

  /// The three symbols both ends show.
  ///
  /// A check for somebody who worries the code was overheard, and not one the
  /// protocol makes: the exchange has already decided by the time these can be
  /// compared. So they are shown, and nothing waits on them.
  final String words;
}

/// The pairing did not happen, and this is what went wrong.
final class PairingRefused extends Pairing {
  /// The pairing failed, saying [reason].
  const PairingRefused(this.reason);

  /// What the client was told, in the words it was told in.
  final String reason;
}
