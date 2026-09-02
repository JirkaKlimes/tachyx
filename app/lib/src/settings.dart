/// What the user has decided, kept between runs.
library;

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// The client's persistent choices: the hosts it knows, what a stream may
/// cost, and how far a finger moves the pointer.
///
/// One object rather than reads scattered over the screens, so that every
/// screen sees the same values and a change made in settings reaches a live
/// session without a restart.
class Settings extends ChangeNotifier {
  static const String _hostsKey = 'tachyx.hosts';
  static const String _bitrateKey = 'tachyx.stream.maxBitrate';
  static const String _sensitivityKey = 'tachyx.pad.sensitivity';
  static const String _pointerAccelKey = 'tachyx.pad.acceleration';
  static const String _onboardedKey = 'tachyx.session.onboarded';

  /// Where a run before the hosts list existed kept its one address.
  static const String _legacyAddressKey = 'tachyx.host.address';

  /// The default cap, matching the client core's own ceiling.
  ///
  /// 1080p60 out of the host's encoder was measured at 8 Mbit/s, so this is
  /// roughly twice a full-rate stream and leaves room for key frame bursts.
  static const int defaultMaxBitrate = 20000000;

  /// The least the slider offers. Below this the resolution ladder bottoms
  /// out and the session is frozen stills.
  static const int minBitrate = 2000000;

  /// The most it offers, well past what any rung spends.
  static const int maxBitrate = 50000000;

  /// The slowest the pointer can be made.
  static const double minSensitivity = 0.5;

  /// The fastest.
  static const double maxSensitivity = 3.0;

  /// Acceleration off: the pointer travels the same distance per finger pixel
  /// however fast the finger is going.
  static const double minPointerAccel = 0;

  /// Acceleration at full, which is the cap in `pointerGain`.
  static const double maxPointerAccel = 1;

  /// How much the pointer accelerates before the user has said.
  ///
  /// Mild rather than off. Everyone who used the trackpad before this setting
  /// existed is used to flat, so a default that flung the pointer would read as
  /// a regression, while this one is felt only on a deliberate flick.
  static const double defaultPointerAccel = 0.3;

  List<String> _hosts = <String>[];
  int _streamBitrate = defaultMaxBitrate;
  double _sensitivity = 1;
  double _pointerAccel = defaultPointerAccel;
  bool _onboarded = false;

  /// The hosts the user has kept, most recently used first.
  List<String> get hosts => List<String>.unmodifiable(_hosts);

  /// The most a stream may cost, in bits per second.
  int get streamBitrate => _streamBitrate;

  /// Image pixels of pointer travel per image pixel of finger travel.
  double get sensitivity => _sensitivity;

  /// How much further a flick carries the pointer than a slow drag, from 0
  /// (off) to 1.
  double get pointerAccel => _pointerAccel;

  /// Whether the user has already been shown where the session controls live.
  bool get onboarded => _onboarded;

  /// Reads everything back from storage.
  Future<void> restore() async {
    final SharedPreferences store = await SharedPreferences.getInstance();
    _hosts = store.getStringList(_hostsKey) ?? <String>[];
    // A run before the hosts list existed remembered one address. It becomes
    // the first entry rather than being forgotten.
    final String legacy = store.getString(_legacyAddressKey) ?? '';
    if (legacy.isNotEmpty && !_hosts.contains(legacy)) {
      _hosts.insert(0, legacy);
      await store.setStringList(_hostsKey, _hosts);
      await store.remove(_legacyAddressKey);
    }
    _streamBitrate = (store.getInt(_bitrateKey) ?? defaultMaxBitrate).clamp(
      minBitrate,
      maxBitrate,
    );
    _sensitivity = (store.getDouble(_sensitivityKey) ?? 1).clamp(
      minSensitivity,
      maxSensitivity,
    );
    _pointerAccel = (store.getDouble(_pointerAccelKey) ?? defaultPointerAccel)
        .clamp(minPointerAccel, maxPointerAccel);
    _onboarded = store.getBool(_onboardedKey) ?? false;
    notifyListeners();
  }

  /// Caps what a stream may cost, in bits per second.
  Future<void> setStreamBitrate(int bitsPerSecond) async {
    _streamBitrate = bitsPerSecond.clamp(minBitrate, maxBitrate);
    notifyListeners();
    final SharedPreferences store = await SharedPreferences.getInstance();
    await store.setInt(_bitrateKey, _streamBitrate);
  }

  /// Sets how far a finger moves the pointer.
  Future<void> setSensitivity(double factor) async {
    _sensitivity = factor.clamp(minSensitivity, maxSensitivity);
    notifyListeners();
    final SharedPreferences store = await SharedPreferences.getInstance();
    await store.setDouble(_sensitivityKey, _sensitivity);
  }

  /// Sets how much a fast finger outruns a slow one.
  Future<void> setPointerAccel(double amount) async {
    _pointerAccel = amount.clamp(minPointerAccel, maxPointerAccel);
    notifyListeners();
    final SharedPreferences store = await SharedPreferences.getInstance();
    await store.setDouble(_pointerAccelKey, _pointerAccel);
  }

  /// Keeps [address], at the top of the list.
  ///
  /// Adding an address the list holds moves it up rather than doubling it,
  /// which is also what a successful dial calls to keep the list in
  /// most-recent order.
  Future<void> keepHost(String address) async {
    final String trimmed = address.trim();
    if (trimmed.isEmpty) return;
    _hosts
      ..remove(trimmed)
      ..insert(0, trimmed);
    notifyListeners();
    final SharedPreferences store = await SharedPreferences.getInstance();
    await store.setStringList(_hostsKey, _hosts);
  }

  /// Forgets [address].
  Future<void> forgetHost(String address) async {
    if (!_hosts.remove(address)) return;
    notifyListeners();
    final SharedPreferences store = await SharedPreferences.getInstance();
    await store.setStringList(_hostsKey, _hosts);
  }

  /// Records that the user has seen where the session controls live, so the
  /// hint is shown on the first session and never again.
  Future<void> markOnboarded() async {
    if (_onboarded) return;
    _onboarded = true;
    notifyListeners();
    final SharedPreferences store = await SharedPreferences.getInstance();
    await store.setBool(_onboardedKey, true);
  }
}
