/// The hosts this client has paired with, and the pairing that adds one.
library;

import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:path_provider/path_provider.dart';

import 'package:tachyx/src/rust/api/pairing.dart' as rust;
import 'package:tachyx/src/session/link.dart' show HostLink;
import 'package:tachyx/src/session/pairing.dart';

/// Where this client's own key and its list of hosts live.
///
/// It must outlive the run. A directory the system may clear would take the
/// device key with it, and a key that is gone is a device the host no longer
/// knows: every session would need pairing again.
Future<String> clientState() async =>
    (await getApplicationSupportDirectory()).path;

/// The hosts the client knows, and the pairing that adds one.
///
/// The list comes from the client core rather than from the settings store.
/// After mutual authentication an address alone cannot connect: the host has to
/// hold this client's key, and only a pairing puts it there. So the paired
/// devices are the list, and an address is something a host told us about
/// itself.
class HostBook extends ChangeNotifier {
  /// A book that reads the client core, which is every book but a test's.
  HostBook();

  /// A book that already holds [hosts] and never reads the client core.
  ///
  /// A widget test has no Rust library behind the bridge, and what these
  /// screens are being tested for is what they draw and which identifier they
  /// hand back.
  @visibleForTesting
  HostBook.of(List<rust.KnownHostView> hosts) : _hosts = hosts;

  List<rust.KnownHostView> _hosts = <rust.KnownHostView>[];
  Pairing _pairing = const Unpaired();
  String? _fault;

  /// The paired hosts, in the order they were learned.
  List<rust.KnownHostView> get hosts => List<rust.KnownHostView>.unmodifiable(_hosts);

  /// How far a pairing has got.
  Pairing get pairing => _pairing;

  /// Why the list could not be read, if it could not.
  String? get fault => _fault;

  /// Reads the list back from the client core.
  Future<void> load() async {
    try {
      _hosts = await rust.knownHosts(stateDirectory: await clientState());
      _fault = null;
    } on Object catch (error) {
      // An unreadable list is not an empty one. Showing it as empty would send
      // the user through pairing again for a fault that is not theirs.
      _fault = HostLink.describe(error);
    }
    notifyListeners();
  }

  /// Forgets [device] at this end.
  ///
  /// The host keeps its own record until somebody revokes this client on it, so
  /// this stops the client dialling and does not stop the host answering.
  Future<void> forget(String device) async {
    try {
      await rust.forgetHost(stateDirectory: await clientState(), device: device);
    } on Object catch (error) {
      _fault = HostLink.describe(error);
    }
    await load();
  }

  /// The codes this client has already made an attempt with.
  ///
  /// The client's half of *one attempt per code*, and against a hostile relay
  /// the only half that works: the party that would report a guess to the host
  /// is the attacker, so the host cannot count what it never sees. What the
  /// attacker actually farms is the user retyping the code that is still on
  /// the host's screen after a failure, each retype one more free guess. So a
  /// code that carried a guess out of this device is never sent again, and the
  /// user is told to get a fresh one instead.
  ///
  /// In memory only. A restarted app forgets, which reopens nothing: the
  /// mailbox behind a spent code is gone within its five-minute life anyway,
  /// and the codes take twenty-something bytes each for as long as the app
  /// runs.
  final Set<String> _spent = <String>{};

  /// Runs one pairing from [code], which is either the typed code or a whole
  /// `tachyx://pair?c=...` link.
  ///
  /// A code that already carried a guess out of this device is refused here,
  /// before anything is dialled. Retyping it would hand whoever is in the
  /// middle of the mailbox another guess at it.
  Future<void> pair(String code, List<String> relays) async {
    if (_pairing is Exchanging) return;
    final String offered = code.trim();
    if (_spent.contains(offered)) {
      _set(
        const PairingRefused(
          'that code has already been used for an attempt. a code is good for '
          'one, so run `tachyx-hostd pair` again and use the fresh one.',
        ),
      );
      return;
    }
    _set(const Exchanging());
    try {
      final rust.PairReport report = await rust.pair(
        code: offered,
        relays: relays,
        stateDirectory: await clientState(),
        name: await _thisDevice(),
      );
      if (report.spent) _spent.add(offered);
      final rust.PairedHost? paired = report.host;
      if (paired == null) {
        _set(PairingRefused(report.fault ?? 'the pairing failed'));
        return;
      }
      await load();
      _set(Paired(paired.name, paired.short, paired.words));
    } on Object catch (error) {
      // Only a fault in the bridge itself lands here; the exchange reports
      // through the report. Nothing left this device, so the code is not
      // marked spent.
      _set(PairingRefused(HostLink.describe(error)));
    }
  }

  /// Clears whatever the pairing screen was showing, for the next one.
  void forgetPairing() => _set(const Unpaired());

  void _set(Pairing state) {
    _pairing = state;
    notifyListeners();
  }

  /// What this client calls itself in the host's list.
  ///
  /// The device's own name where the platform has one, because a person reading
  /// `tachyx-hostd pairings list` wants to know which phone this is. It is the
  /// peer's word at the far end and decides nothing there.
  static Future<String> _thisDevice() async {
    final String name = Platform.localHostname.trim();
    return name.isEmpty ? 'a tachyx client' : name;
  }
}
