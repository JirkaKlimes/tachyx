import 'package:flutter/material.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge.dart'
    show AnyhowException;
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:tachyx/src/input/touchpad.dart';
import 'package:tachyx/src/rust/api/audio.dart' show AudioMode, AudioStats;
import 'package:tachyx/src/rust/api/input.dart';
import 'package:tachyx/src/rust/api/session.dart'
    show Encoding, HostReport, Liveness, SessionStats, VideoStats;
import 'package:tachyx/src/rust/api/pairing.dart' show KnownHostView;
import 'package:tachyx/src/rust/api/video.dart' show Latency, PresentStats;
import 'package:tachyx/src/session/hosts.dart';
import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/settings.dart';
import 'package:tachyx/src/ui/audio.dart';
import 'package:tachyx/src/ui/connect_screen.dart';
import 'package:tachyx/src/ui/session_screen.dart';
import 'package:tachyx/src/ui/settings_screen.dart';
import 'package:tachyx/src/ui/stats_overlay.dart';

Widget _wrap(Widget child) => MaterialApp(home: child);

/// A paired host, as the client core would report one.
KnownHostView _host({String name = 'desk', String device = 'ABCD1234'}) =>
    KnownHostView(
      device: device,
      short: 'ABCD-1234-EFGH',
      name: name,
      endpoints: const <String>[],
      control: true,
      unattended: true,
    );

/// A settings store over mocked preferences, restored so its listeners work.
Future<Settings> _settings({
  List<String> hosts = const <String>[],
  bool onboarded = true,
}) async {
  SharedPreferences.setMockInitialValues(<String, Object>{
    'tachyx.hosts': hosts,
    'tachyx.session.onboarded': onboarded,
  });
  final Settings settings = Settings();
  await settings.restore();
  return settings;
}

VideoStats _video() => VideoStats(
  packetsReceived: BigInt.from(2000),
  packetsLost: BigInt.from(7),
  packetsLate: BigInt.from(1),
  framesDelivered: BigInt.from(120),
  framesDropped: BigInt.from(3),
  framesMissing: BigInt.zero,
  framesUndecodable: BigInt.from(2),
);

/// A host that can stream its sound, with the play path counting normally.
AudioStats _audio({
  bool hostHasAudio = true,
  AudioMode mode = AudioMode.off,
  int volume = 500,
  bool muted = false,
}) => AudioStats(
  hostHasAudio: hostHasAudio,
  mode: mode,
  volume: volume,
  muted: muted,
  packets: BigInt.from(900),
  lost: BigInt.from(2),
  played: BigInt.from(896),
  concealed: BigInt.from(2),
  recovered: BigInt.one,
  dropped: BigInt.zero,
  buffered: 2,
);

Latency _latency(int median) => Latency(
  minUs: median - 100,
  medianUs: median,
  p95Us: median + 200,
  maxUs: median + 700,
);

SessionStats _stats({
  Liveness liveness = Liveness.streaming,
  bool presenting = true,
  AudioStats? audio,
}) => SessionStats(
  liveness: liveness,
  rttUs: 1234,
  batches: BigInt.from(9),
  video: _video(),
  audio: audio ?? _audio(),
  encoding: const Encoding(
    codec: 'AV1',
    codedWidth: 1920,
    codedHeight: 1088,
    visibleWidth: 1920,
    visibleHeight: 1080,
    aspectWidth: 16,
    aspectHeight: 9,
    framerate: 60,
    bitrate: 8000000,
  ),
  host: HostReport(
    framesSent: BigInt.from(130),
    framesGated: BigInt.from(4),
    pacingRate: BigInt.from(240000000),
    pipelineUs: 4200,
  ),
  present: presenting
      ? PresentStats(
          pushed: BigInt.from(120),
          decoded: BigInt.from(119),
          presented: BigInt.from(118),
          superseded: BigInt.from(1),
          decode: _latency(630),
          present: _latency(1870),
        )
      : null,
);

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('the statistics overlay', () {
    testWidgets('says there is no session rather than showing zeros', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          const StatsOverlay(
            host: null,
            protocolVersion: 1,
            eventsSent: 0,
            lastEvent: null,
            keyboardUp: false,
          ),
        ),
      );

      expect(find.textContaining('no session'), findsOneWidget);
      expect(find.textContaining('frames in'), findsNothing);
      expect(find.textContaining('round trip'), findsNothing);
      expect(find.text('nothing yet'), findsOneWidget);
    });

    testWidgets('shows the counters the client measured', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          StatsOverlay(
            host: '10.0.0.4:4433',
            protocolVersion: 1,
            eventsSent: 42,
            lastEvent: const InputEvent.fingerScrollEnd(),
            keyboardUp: true,
            stats: _stats(),
          ),
        ),
      );

      expect(find.textContaining('no session'), findsNothing);
      expect(
        find.text('120 in, 3 dropped, 0 missing, 2 undecodable'),
        findsOneWidget,
      );
      expect(find.text('2000 in, 7 lost, 1 late'), findsOneWidget);
      expect(find.text('42'), findsOneWidget, reason: 'events sent');
      expect(find.text('9'), findsOneWidget, reason: 'batches');
      expect(find.text('scroll end'), findsOneWidget);
      expect(find.text('up'), findsOneWidget);
      expect(find.text('streaming'), findsOneWidget);
      expect(find.text('1.23 ms'), findsOneWidget, reason: 'the round trip');
      expect(find.text('AV1 1920x1080 @60 8.0 Mbit/s'), findsOneWidget);
    });

    testWidgets('reports the stages it can measure without the host clock', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          StatsOverlay(
            host: '10.0.0.4:4433',
            protocolVersion: 1,
            eventsSent: 0,
            lastEvent: null,
            keyboardUp: false,
            stats: _stats(),
          ),
        ),
      );

      expect(find.text('0.63 ms / 0.83 ms p95'), findsOneWidget);
      expect(find.text('1.87 ms / 2.07 ms p95'), findsOneWidget);
      expect(
        find.text('130 sent, 4 gated, 4.20 ms pipeline'),
        findsOneWidget,
        reason: "the host's own report, marked as its word",
      );
    });

    testWidgets('a still screen reads as idle rather than as no frames', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          StatsOverlay(
            host: '10.0.0.4:4433',
            protocolVersion: 1,
            eventsSent: 0,
            lastEvent: null,
            keyboardUp: false,
            stats: _stats(liveness: Liveness.idle, presenting: false),
          ),
        ),
      );

      expect(find.textContaining('idle'), findsOneWidget);
      expect(find.textContaining('0 fps'), findsNothing);
      expect(
        find.text(describeLiveness(Liveness.lost)),
        findsNothing,
        reason: 'a still screen is not a lost host',
      );
      expect(
        find.text('decode'),
        findsNothing,
        reason: 'nothing has decoded, so there is no decode latency to show',
      );
    });

    test('a lost host is the only state that reads as a fault', () {
      expect(describeLiveness(Liveness.streaming), 'streaming');
      expect(describeLiveness(Liveness.idle), contains('idle'));
      expect(describeLiveness(Liveness.lost), contains('no heartbeat'));
    });

    test('every kind of event has a description', () {
      final List<InputEvent> all = <InputEvent>[
        const InputEvent.pointerMotion(dx: 1.5, dy: -2.5),
        const InputEvent.pointerMotionAbsolute(x: 0.5, y: 0.25),
        const InputEvent.pointerButton(
          button: PointerButton.right,
          press: Press.down,
        ),
        const InputEvent.wheel(dx: 0, dy: 53),
        const InputEvent.fingerScroll(dx: 0, dy: -6),
        const InputEvent.fingerScrollEnd(),
        const InputEvent.key(
          usage: HidUsage(page: 7, id: 0x2A),
          press: Press.up,
        ),
        const InputEvent.text('hello'),
      ];

      for (final InputEvent event in all) {
        expect(describeEvent(event), isNotEmpty);
      }
      expect(describeEvent(all[2]), 'right down');
      expect(describeEvent(all[6]), 'key 0x2a up');
      expect(describeEvent(all[7]), 'text "hello"');
    });
  });

  group('the connect screen', () {
    Widget screen({
      required HostBook book,
      Link state = const Offline(),
      ValueChanged<KnownHostView>? onDial,
      VoidCallback? onPair,
      required Settings settings,
    }) => _wrap(
      ConnectScreen(
        state: state,
        protocolVersion: 1,
        settings: settings,
        book: book,
        onDial: onDial ?? (KnownHostView _) {},
        onPair: onPair ?? () {},
        onDetach: () {},
      ),
    );

    testWidgets('a paired host connects with one tap', (
      WidgetTester tester,
    ) async {
      final List<String> dialled = <String>[];
      await tester.pumpWidget(
        screen(
          book: HostBook.of(<KnownHostView>[_host()]),
          settings: await _settings(),
          onDial: (KnownHostView host) => dialled.add(host.device),
        ),
      );

      expect(find.text('desk'), findsOneWidget);
      expect(
        find.text('ABCD-1234-EFGH'),
        findsOneWidget,
        reason: 'a name is the peer\'s word, so the identifier is shown too',
      );
      await tester.tap(find.text('desk'));
      expect(dialled, <String>['ABCD1234']);
    });

    testWidgets('an empty list says how to begin, which is pairing', (
      WidgetTester tester,
    ) async {
      bool asked = false;
      await tester.pumpWidget(
        screen(
          book: HostBook.of(<KnownHostView>[]),
          settings: await _settings(),
          onPair: () => asked = true,
        ),
      );

      expect(find.textContaining('no hosts yet'), findsOneWidget);
      await tester.tap(find.text('+ pair with a host'));
      expect(
        asked,
        isTrue,
        reason:
            'an address cannot be typed in: only a pairing puts this '
            'client\'s key on the host',
      );
    });

    testWidgets('a refusal is shown in the words the client was given', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        screen(
          book: HostBook.of(<KnownHostView>[_host()]),
          settings: await _settings(),
          state: const Refused('ABCD1234', 'desk', 'peer speaks version 7'),
        ),
      );

      expect(find.text('peer speaks version 7'), findsOneWidget);
    });

    testWidgets('the row being dialled says so', (WidgetTester tester) async {
      await tester.pumpWidget(
        screen(
          book: HostBook.of(<KnownHostView>[_host()]),
          settings: await _settings(),
          state: const Dialling('ABCD1234', 'desk'),
        ),
      );

      expect(find.text('dialling…'), findsOneWidget);
    });

    testWidgets('dialling cannot be started twice', (
      WidgetTester tester,
    ) async {
      int dials = 0;
      await tester.pumpWidget(
        screen(
          book: HostBook.of(<KnownHostView>[_host()]),
          settings: await _settings(),
          state: const Dialling('ABCD1234', 'desk'),
          onDial: (KnownHostView _) => dials += 1,
        ),
      );

      await tester.tap(find.text('desk'), warnIfMissed: false);
      await tester.pump();

      expect(dials, 0);
    });
  });

  group('the settings screen', () {
    testWidgets('moving a slider stores the choice', (
      WidgetTester tester,
    ) async {
      final Settings settings = await _settings();
      await tester.pumpWidget(_wrap(SettingsScreen(settings: settings)));

      expect(find.text('20 Mbit/s'), findsOneWidget, reason: 'the default cap');
      expect(find.text('1.0×'), findsOneWidget, reason: 'default sensitivity');

      await tester.drag(find.byType(Slider).first, const Offset(-400, 0));
      await tester.pump();

      expect(
        settings.streamBitrate,
        Settings.minBitrate,
        reason: 'dragged to the floor of the range',
      );
    });

    testWidgets('relays can be added and removed here', (
      WidgetTester tester,
    ) async {
      final Settings settings = await _settings();
      await tester.pumpWidget(_wrap(SettingsScreen(settings: settings)));

      // The screen scrolls, so the relays at the foot of it need bringing into
      // view before they can be touched. A test window is smaller than a phone.
      await tester.ensureVisible(find.byTooltip('remove'));
      await tester.tap(find.byTooltip('remove'));
      await tester.pump();
      expect(settings.relays, isEmpty);

      await tester.enterText(find.byType(TextField).last, '10.0.0.9:47990#KEY');
      await tester.ensureVisible(find.text('+ add relay'));
      await tester.tap(find.text('+ add relay'));
      await tester.pump();
      expect(settings.relays, <String>['10.0.0.9:47990#KEY']);
    });
  });

  group('the session screen', () {
    // Pointer identifiers are never reused, because a run may leave one down
    // in the framework's own bookkeeping and the next gesture would then be
    // read as a continuation of it.
    int pointer = 0;

    /// A session with no host, which is the one shape a widget test can build:
    /// anything else needs the client core on the other end.
    Future<Settings> pumpSession(
      WidgetTester tester, {
      VoidCallback? onLeave,
      bool onboarded = true,
    }) async {
      final Settings settings = await _settings(onboarded: onboarded);
      await tester.pumpWidget(
        _wrap(
          SessionScreen(
            protocolVersion: 1,
            settings: settings,
            onLeave: onLeave ?? () {},
          ),
        ),
      );
      return settings;
    }

    /// Three fingers travelling [by] on the pad, and the frames that follow.
    Future<void> swipe(WidgetTester tester, Offset by) async {
      final List<TestGesture> fingers = <TestGesture>[
        for (final double x in <double>[300, 360, 420])
          await tester.startGesture(Offset(x, 300), pointer: ++pointer),
      ];
      for (final TestGesture finger in fingers) {
        await finger.moveBy(by);
      }
      for (final TestGesture finger in fingers) {
        await finger.up();
      }
      await tester.pump();
      // Not pumpAndSettle: the counters are sampled on a periodic timer, so
      // there is always another frame coming and settling never arrives.
      await tester.pump(const Duration(milliseconds: 250));
    }

    Finder controls() => find.byKey(const ValueKey<String>('controls'));
    Finder mark() => find.byKey(const ValueKey<String>('mark'));

    testWidgets('the picture is the whole window', (WidgetTester tester) async {
      await pumpSession(tester);

      expect(
        tester.getSize(find.byType(TouchSurface)),
        tester.view.physicalSize / tester.view.devicePixelRatio,
        reason:
            'the host encodes for the viewport the client named, so the '
            'video has to be the whole of it',
      );

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('the controls fold into the corner mark and come back', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);
      expect(controls(), findsOneWidget, reason: 'a session opens showing');

      await swipe(tester, const Offset(0, -60));
      await tester.pump(const Duration(milliseconds: 250));

      expect(controls(), findsNothing);
      expect(mark(), findsOneWidget, reason: 'the ghost stays reachable');

      // The mark is the one way back in: a downward swipe means nothing.
      await swipe(tester, const Offset(0, 60));
      await tester.pump(const Duration(milliseconds: 250));
      expect(controls(), findsNothing);

      await tester.tapAt(tester.getCenter(mark()));
      await tester.pump(const Duration(milliseconds: 250));

      expect(controls(), findsOneWidget);

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('the controls retire on their own', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);
      expect(controls(), findsOneWidget);

      await tester.pump(const Duration(seconds: 6));
      await tester.pump(const Duration(milliseconds: 250));

      expect(controls(), findsNothing);
      expect(mark(), findsOneWidget);

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('tapping the mark opens the controls', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);
      await tester.pump(const Duration(seconds: 6));
      await tester.pump(const Duration(milliseconds: 250));
      expect(mark(), findsOneWidget);

      await tester.tap(mark());
      await tester.pump(const Duration(milliseconds: 250));

      expect(controls(), findsOneWidget);

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('the statistics panel is asked for, not default', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);
      expect(find.byType(StatsOverlay), findsNothing);

      await tester.tap(find.byTooltip('statistics'));
      await tester.pump();

      expect(find.byType(StatsOverlay), findsOneWidget);

      await swipe(tester, const Offset(0, -60));

      expect(
        find.byType(StatsOverlay),
        findsNothing,
        reason: 'a swipe up clears the glass entirely',
      );

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('the first session ever explains itself, once', (
      WidgetTester tester,
    ) async {
      final Settings settings = await pumpSession(tester, onboarded: false);
      expect(find.textContaining('this screen is a trackpad'), findsOneWidget);

      await tester.tap(find.text('got it'));
      await tester.pump();

      expect(find.textContaining('this screen is a trackpad'), findsNothing);
      expect(settings.onboarded, isTrue);

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('a retired dock takes no touch but the mark itself', (
      WidgetTester tester,
    ) async {
      int left = 0;
      await pumpSession(tester, onLeave: () => left++);
      await swipe(tester, const Offset(0, -60));
      await tester.pump(const Duration(milliseconds: 250));

      // The buttons are out of the tree entirely, not merely invisible, so
      // a tap where the leave button sat cannot leave the session.
      expect(find.byTooltip('leave'), findsNothing);
      await tester.tapAt(tester.getCenter(mark()));
      await tester.pump(const Duration(milliseconds: 250));

      expect(left, 0);
      expect(controls(), findsOneWidget, reason: 'the tap opened the dock');

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('with no host there is no sound to offer', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);

      expect(
        find.byTooltip('audio: unavailable'),
        findsOneWidget,
        reason: 'the control says it cannot be used rather than doing nothing',
      );
      expect(find.byTooltip('audio: off'), findsNothing);

      await tester.pumpWidget(const SizedBox());
    });
  });

  group('the sound controls', () {
    testWidgets('the toolbar button cycles off, here, both', (
      WidgetTester tester,
    ) async {
      AudioMode mode = AudioMode.off;
      await tester.pumpWidget(
        _wrap(
          StatefulBuilder(
            builder: (BuildContext context, StateSetter rebuild) => AudioButton(
              // What the host says it is doing, as the next sample would
              // report it: the button shows the host, not the tap.
              audio: _audio(mode: mode),
              onMode: (AudioMode wanted) => rebuild(() => mode = wanted),
            ),
          ),
        ),
      );

      expect(find.byTooltip('audio: off'), findsOneWidget);

      await tester.tap(find.byTooltip('audio: off'));
      await tester.pump();
      expect(find.byTooltip('audio: here'), findsOneWidget);
      expect(mode, AudioMode.exclusive);

      await tester.tap(find.byTooltip('audio: here'));
      await tester.pump();
      expect(find.byTooltip('audio: both'), findsOneWidget);
      expect(mode, AudioMode.shared);

      await tester.tap(find.byTooltip('audio: both'));
      await tester.pump();
      expect(find.byTooltip('audio: off'), findsOneWidget);
      expect(mode, AudioMode.off, reason: 'three states and back round');
    });

    testWidgets('a host with no sound is asked for none', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          AudioButton(
            audio: _audio(hostHasAudio: false),
            onMode: (AudioMode _) => fail('a host with no sound cannot play'),
          ),
        ),
      );

      expect(find.byTooltip('audio: unavailable'), findsOneWidget);
      await tester.tap(
        find.byTooltip('audio: unavailable'),
        warnIfMissed: false,
      );
      await tester.pump();
    });

    testWidgets('a tap that is not answered stands, then gives way', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          AudioButton(
            audio: _audio(),
            asked: AudioMode.shared,
            onMode: (AudioMode _) {},
          ),
        ),
      );

      expect(
        find.byTooltip('audio: both'),
        findsOneWidget,
        reason: 'the tap is shown until the host has answered it',
      );
    });

    testWidgets('the volume column comes up when a key moves it, then goes', (
      WidgetTester tester,
    ) async {
      final ValueNotifier<int> nudged = ValueNotifier<int>(0);
      addTearDown(nudged.dispose);
      await tester.pumpWidget(
        // Under a Material, as the session screen puts it: the column sits
        // over the picture, which is drawn on one.
        _wrap(
          Material(
            child: Center(
              child: VolumeOverlay(
                audio: _audio(mode: AudioMode.shared, volume: 640),
                nudged: nudged,
                onVolume: (int _) {},
                onMuted: (bool _) {},
              ),
            ),
          ),
        ),
      );

      expect(find.text('64%'), findsNothing, reason: 'nothing has moved it');

      nudged.value += 1;
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 250));
      expect(find.text('64%'), findsOneWidget);

      await tester.pump(const Duration(milliseconds: 1500));
      await tester.pump(const Duration(milliseconds: 250));
      expect(
        find.text('64%'),
        findsNothing,
        reason: 'the picture is the point of the screen, so nothing parks here',
      );
    });

    testWidgets('dragging the column up asks for more, not less', (
      WidgetTester tester,
    ) async {
      final ValueNotifier<int> nudged = ValueNotifier<int>(0);
      addTearDown(nudged.dispose);
      final List<int> asked = <int>[];
      await tester.pumpWidget(
        _wrap(
          Material(
            child: Center(
              child: VolumeOverlay(
                audio: _audio(mode: AudioMode.shared, volume: 500),
                nudged: nudged,
                onVolume: asked.add,
                onMuted: (bool _) {},
              ),
            ),
          ),
        ),
      );

      nudged.value += 1;
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 250));

      await tester.drag(find.byType(Slider), const Offset(0, -40));
      await tester.pump();

      expect(
        asked.last,
        greaterThan(asked.first),
        reason: 'the column is turned so that the loud end is the top one',
      );
    });

    testWidgets('the column mutes the host, and says so', (
      WidgetTester tester,
    ) async {
      final ValueNotifier<int> nudged = ValueNotifier<int>(0);
      addTearDown(nudged.dispose);
      bool? asked;
      await tester.pumpWidget(
        // Under a Material, as the session screen puts it: the column sits
        // over the picture, which is drawn on one.
        _wrap(
          Material(
            child: Center(
              child: VolumeOverlay(
                audio: _audio(mode: AudioMode.shared, volume: 300),
                nudged: nudged,
                onVolume: (int _) {},
                onMuted: (bool muted) => asked = muted,
              ),
            ),
          ),
        ),
      );

      nudged.value += 1;
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 250));

      expect(find.text('30%'), findsOneWidget);
      await tester.tap(find.byTooltip('mute host'));
      await tester.pump();

      expect(asked, isTrue, reason: 'it is the host that goes quiet, not this');
    });
  });

  group('what went wrong', () {
    test('a failure from Rust is shown without its wrapper', () {
      expect(
        HostLink.describe(AnyhowException('the host refused the connection')),
        'the host refused the connection',
      );
    });

    test('anything else is shown as it describes itself', () {
      expect(
        HostLink.describe(const FormatException('bad port')),
        contains('bad port'),
      );
    });
  });
}
