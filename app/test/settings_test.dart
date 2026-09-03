import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:tachyx/src/settings.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  Future<Settings> restored(Map<String, Object> stored) async {
    SharedPreferences.setMockInitialValues(stored);
    final Settings settings = Settings();
    await settings.restore();
    return settings;
  }

  test('a fresh store has working defaults', () async {
    final Settings settings = await restored(<String, Object>{});
    expect(settings.hosts, isEmpty);
    expect(
      settings.relays,
      <String>[Settings.defaultRelay],
      reason: 'a client with no relay cannot reach a host off this network',
    );
    expect(settings.streamBitrate, Settings.defaultMaxBitrate);
    expect(settings.sensitivity, 1);
    expect(settings.pointerAccel, Settings.defaultPointerAccel);
    expect(settings.onboarded, isFalse);
  });

  test('the address a run before the hosts list kept becomes the first host',
      () async {
    final Settings settings = await restored(<String, Object>{
      'tachyx.host.address': '10.0.0.4:4433',
    });
    expect(settings.hosts, <String>['10.0.0.4:4433']);

    final SharedPreferences store = await SharedPreferences.getInstance();
    expect(
      store.getString('tachyx.host.address'),
      isNull,
      reason: 'migrated once, not on every start',
    );
  });

  test('a relay the user removed stays removed across a restart', () async {
    final Settings settings = await restored(<String, Object>{});
    await settings.forgetRelay(Settings.defaultRelay);
    expect(settings.relays, isEmpty);

    final SharedPreferences store = await SharedPreferences.getInstance();
    final Settings again = Settings();
    SharedPreferences.setMockInitialValues(<String, Object>{
      'tachyx.relays': store.getStringList('tachyx.relays') ?? <String>[],
    });
    await again.restore();
    expect(
      again.relays,
      isEmpty,
      reason: 'an empty list is a decision, and only a missing one is a default',
    );
  });

  test('keeping a relay again moves it up rather than doubling it', () async {
    final Settings settings = await restored(<String, Object>{});
    await settings.keepRelay('a:1#K');
    await settings.keepRelay('b:2#K');
    await settings.keepRelay('a:1#K');
    expect(settings.relays, <String>['a:1#K', 'b:2#K', Settings.defaultRelay]);
  });

  test('keeping a host again moves it up rather than doubling it', () async {
    final Settings settings = await restored(<String, Object>{});
    await settings.keepHost('a:1');
    await settings.keepHost('b:2');
    expect(settings.hosts, <String>['b:2', 'a:1']);

    await settings.keepHost('a:1');
    expect(settings.hosts, <String>['a:1', 'b:2']);
  });

  test('a blank address is not a host', () async {
    final Settings settings = await restored(<String, Object>{});
    await settings.keepHost('   ');
    expect(settings.hosts, isEmpty);
  });

  test('forgetting removes exactly the named host', () async {
    final Settings settings = await restored(<String, Object>{
      'tachyx.hosts': <String>['a:1', 'b:2'],
    });
    await settings.forgetHost('a:1');
    expect(settings.hosts, <String>['b:2']);
    await settings.forgetHost('never-kept');
    expect(settings.hosts, <String>['b:2']);
  });

  test('a stored value outside the range is pulled back into it', () async {
    final Settings settings = await restored(<String, Object>{
      'tachyx.stream.maxBitrate': 999000000,
      'tachyx.pad.sensitivity': 40.0,
      'tachyx.pad.acceleration': 9.0,
    });
    expect(settings.streamBitrate, Settings.maxBitrate);
    expect(settings.sensitivity, Settings.maxSensitivity);
    expect(settings.pointerAccel, Settings.maxPointerAccel);
  });

  test('acceleration turned off stays off across a restart', () async {
    // Zero has to survive as zero rather than falling back to the default,
    // because off is a choice and the default is not off.
    SharedPreferences.setMockInitialValues(<String, Object>{});
    final Settings first = Settings();
    await first.restore();
    await first.setPointerAccel(-3);
    expect(first.pointerAccel, Settings.minPointerAccel);

    final Settings second = Settings();
    await second.restore();
    expect(second.pointerAccel, 0);
  });

  test('choices survive a restart', () async {
    SharedPreferences.setMockInitialValues(<String, Object>{});
    final Settings first = Settings();
    await first.restore();
    await first.setStreamBitrate(8000000);
    await first.setSensitivity(1.5);
    await first.setPointerAccel(0.7);
    await first.keepHost('desk.local:47989');
    await first.markOnboarded();

    final Settings second = Settings();
    await second.restore();
    expect(second.streamBitrate, 8000000);
    expect(second.sensitivity, 1.5);
    expect(second.pointerAccel, 0.7);
    expect(second.hosts, <String>['desk.local:47989']);
    expect(second.onboarded, isTrue);
  });
}
