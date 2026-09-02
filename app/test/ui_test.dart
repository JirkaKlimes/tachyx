import 'package:flutter/material.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge.dart'
    show AnyhowException;
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/input/touchpad.dart';
import 'package:tachyx/src/rust/api/input.dart';
import 'package:tachyx/src/rust/api/session.dart'
    show Encoding, HostReport, Liveness, SessionStats, VideoStats;
import 'package:tachyx/src/rust/api/video.dart' show Latency, PresentStats;
import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/ui/connect_screen.dart';
import 'package:tachyx/src/ui/session_screen.dart';
import 'package:tachyx/src/ui/stats_overlay.dart';

Widget _wrap(Widget child) => MaterialApp(home: child);

VideoStats _video() => VideoStats(
  packetsReceived: BigInt.from(2000),
  packetsLost: BigInt.from(7),
  packetsLate: BigInt.from(1),
  framesDelivered: BigInt.from(120),
  framesDropped: BigInt.from(3),
  framesMissing: BigInt.zero,
  framesUndecodable: BigInt.from(2),
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
}) => SessionStats(
  liveness: liveness,
  rttUs: 1234,
  batches: BigInt.from(9),
  video: _video(),
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
    testWidgets('starts from the address the last run used', (
      WidgetTester tester,
    ) async {
      String? dialledHost;
      int? dialledPort;
      await tester.pumpWidget(
        _wrap(
          ConnectScreen(
            state: const Offline(),
            protocolVersion: 1,
            remembered: '10.0.0.4:9999',
            onDial: (String host, int port) {
              dialledHost = host;
              dialledPort = port;
            },
            onDetach: () {},
          ),
        ),
      );

      expect(find.text('10.0.0.4'), findsOneWidget);
      expect(find.text('9999'), findsOneWidget);

      await tester.tap(find.text('connect'));
      await tester.pump();

      expect(dialledHost, '10.0.0.4');
      expect(dialledPort, 9999);
    });

    testWidgets('an address with no port takes the default one', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          ConnectScreen(
            state: const Offline(),
            protocolVersion: 1,
            remembered: '10.0.0.4',
            onDial: (String host, int port) {},
            onDetach: () {},
          ),
        ),
      );

      // Read the field rather than the text, because the hint says the same
      // thing and matching on it would pass whether or not the field was filled.
      final TextField port = tester.widget<TextField>(
        find.byType(TextField).last,
      );
      expect(port.controller?.text, '${HostLink.defaultPort}');
    });

    testWidgets('a refusal is shown in the words the client was given', (
      WidgetTester tester,
    ) async {
      await tester.pumpWidget(
        _wrap(
          ConnectScreen(
            state: const Refused(
              '10.0.0.4:4433',
              'the QUIC transport is not implemented yet',
            ),
            protocolVersion: 1,
            remembered: '',
            onDial: (String host, int port) {},
            onDetach: () {},
          ),
        ),
      );

      expect(find.text('10.0.0.4:4433 refused'), findsOneWidget);
      expect(
        find.text('the QUIC transport is not implemented yet'),
        findsOneWidget,
      );
    });

    testWidgets('dialling cannot be started twice', (
      WidgetTester tester,
    ) async {
      int dials = 0;
      await tester.pumpWidget(
        _wrap(
          ConnectScreen(
            state: const Dialling('10.0.0.4:4433'),
            protocolVersion: 1,
            remembered: '10.0.0.4:4433',
            onDial: (String host, int port) => dials++,
            onDetach: () {},
          ),
        ),
      );

      await tester.tap(find.text('connecting…'));
      await tester.pump();

      expect(dials, 0);
    });
  });

  group('the session screen', () {
    // Pointer identifiers are never reused, because a run may leave one down
    // in the framework's own bookkeeping and the next gesture would then be
    // read as a continuation of it.
    int pointer = 0;

    /// A session with no host, which is the one shape a widget test can build:
    /// anything else needs the client core on the other end.
    Future<void> pumpSession(WidgetTester tester, {VoidCallback? onLeave}) =>
        tester.pumpWidget(
          _wrap(SessionScreen(protocolVersion: 1, onLeave: onLeave ?? () {})),
        );

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

    /// Where the bar has been told to sit, in multiples of its own height.
    Offset barOffset(WidgetTester tester) =>
        tester.widget<AnimatedSlide>(find.byType(AnimatedSlide)).offset;

    testWidgets('the picture is the whole window', (WidgetTester tester) async {
      await pumpSession(tester);

      expect(
        tester.getSize(find.byType(TouchSurface)),
        tester.view.physicalSize / tester.view.devicePixelRatio,
        reason: 'the host encodes for the viewport the client named, so the '
            'video has to be the whole of it',
      );

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('a three-finger swipe puts the chrome away and brings it back', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);
      expect(find.byType(StatsOverlay), findsOneWidget);

      await swipe(tester, const Offset(0, -60));

      expect(find.byType(StatsOverlay), findsNothing);
      expect(barOffset(tester), const Offset(0, 1), reason: 'off the bottom');

      await swipe(tester, const Offset(0, 60));

      expect(find.byType(StatsOverlay), findsOneWidget);
      expect(barOffset(tester), Offset.zero);

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('the bar retires on its own and leaves the panel', (
      WidgetTester tester,
    ) async {
      await pumpSession(tester);
      expect(barOffset(tester), Offset.zero);

      await tester.pump(const Duration(seconds: 6));
      await tester.pump(const Duration(milliseconds: 250));

      expect(barOffset(tester), const Offset(0, 1));
      expect(
        find.byType(StatsOverlay),
        findsOneWidget,
        reason: 'the panel is read for as long as its reader wants it',
      );

      await tester.pumpWidget(const SizedBox());
    });

    testWidgets('a bar that is away takes no touch from the pad', (
      WidgetTester tester,
    ) async {
      int left = 0;
      await pumpSession(tester, onLeave: () => left++);
      await swipe(tester, const Offset(0, -60));

      await tester.tap(find.byTooltip('leave'), warnIfMissed: false);
      await tester.pump();

      expect(left, 0);

      await tester.pumpWidget(const SizedBox());
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
      expect(HostLink.describe(const FormatException('bad port')), contains('bad port'));
    });
  });
}
