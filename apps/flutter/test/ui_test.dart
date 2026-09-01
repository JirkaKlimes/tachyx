import 'package:flutter/material.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge.dart'
    show AnyhowException;
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/rust/api/input.dart';
import 'package:tachyx/src/rust/api/session.dart' show VideoStats;
import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/ui/connect_screen.dart';
import 'package:tachyx/src/ui/stats_overlay.dart';

Widget _wrap(Widget child) => MaterialApp(home: child);

VideoStats _stats() => VideoStats(
  packetsReceived: BigInt.from(2000),
  packetsLost: BigInt.from(7),
  packetsLate: BigInt.from(1),
  framesDelivered: BigInt.from(120),
  framesDropped: BigInt.from(3),
  framesUndecodable: BigInt.from(2),
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
            batchesSent: null,
            lastEvent: null,
            keyboardUp: false,
          ),
        ),
      );

      expect(find.textContaining('no session'), findsOneWidget);
      expect(find.textContaining('frames in'), findsNothing);
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
            batchesSent: 9,
            lastEvent: const InputEvent.fingerScrollEnd(),
            keyboardUp: true,
            video: _stats(),
          ),
        ),
      );

      expect(find.textContaining('no session'), findsNothing);
      expect(find.text('120'), findsOneWidget);
      expect(find.text('3'), findsOneWidget);
      expect(find.text('42'), findsOneWidget);
      expect(find.text('scroll end'), findsOneWidget);
      expect(find.text('up'), findsOneWidget);
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

      expect(find.text('${HostLink.defaultPort}'), findsOneWidget);
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
