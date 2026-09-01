import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/input/touchpad.dart';
import 'package:tachyx/src/rust/api/input.dart';

/// Everything one test saw the touchpad decide.
class _Recorder {
  final List<InputEvent> events = <InputEvent>[];
  int keyboardToggles = 0;
  int dragLocks = 0;
  int controlsShown = 0;
  int controlsHidden = 0;

  void emit(Duration at, InputEvent event) => events.add(event);

  List<InputEvent> get motions =>
      events.whereType<InputEvent_PointerMotion>().toList();
  List<InputEvent> get scrolls =>
      events.whereType<InputEvent_FingerScroll>().toList();
  List<InputEvent> get buttons =>
      events.whereType<InputEvent_PointerButton>().toList();

  Offset get travelled => motions.fold(Offset.zero, (Offset sum, InputEvent e) {
    final InputEvent_PointerMotion m = e as InputEvent_PointerMotion;
    return sum + Offset(m.dx, m.dy);
  });

  Offset get scrolled => scrolls.fold(Offset.zero, (Offset sum, InputEvent e) {
    final InputEvent_FingerScroll s = e as InputEvent_FingerScroll;
    return sum + Offset(s.dx, s.dy);
  });
}

InputEvent _press(PointerButton button) =>
    InputEvent.pointerButton(button: button, press: Press.down);
InputEvent _release(PointerButton button) =>
    InputEvent.pointerButton(button: button, press: Press.up);

/// A touchpad wired to a full-window surface, and the recorder behind it.
Future<(Touchpad, _Recorder)> _surface(
  WidgetTester tester, {
  ImageScale scale = ImageScale.identity,
}) async {
  final _Recorder log = _Recorder();
  final Touchpad pad = Touchpad(
    emit: log.emit,
    toggleKeyboard: () => log.keyboardToggles++,
    showControls: () => log.controlsShown++,
    hideControls: () => log.controlsHidden++,
    onDragLock: () => log.dragLocks++,
    scale: scale,
  );
  await tester.pumpWidget(
    TouchSurface(
      pad: pad,
      child: const ColoredBox(color: Color(0xFF000000)),
    ),
  );
  return (pad, log);
}

/// Three fingers on the glass, all down before any of them moves.
Future<List<TestGesture>> _threeFingers(WidgetTester tester) async =>
    <TestGesture>[
      await tester.startGesture(const Offset(100, 200), pointer: 4),
      await tester.startGesture(const Offset(160, 200), pointer: 5),
      await tester.startGesture(const Offset(220, 200), pointer: 6),
    ];

Future<void> _lift(List<TestGesture> fingers) async {
  for (final TestGesture finger in fingers) {
    await finger.up();
  }
}

void main() {
  group('the gesture table', () {
    testWidgets('one-finger tap is a left click', (WidgetTester tester) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );
      expect(log.events, isEmpty, reason: 'nothing is decided on the way down');
      await finger.up();

      // Asserted before any pump: the click is already out, so nothing is
      // waiting on a frame or a timer to release it.
      expect(log.events, <InputEvent>[
        _press(PointerButton.left),
        _release(PointerButton.left),
      ]);
    });

    testWidgets('two-finger tap is a right click', (WidgetTester tester) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture first = await tester.startGesture(
        const Offset(100, 100),
        pointer: 1,
      );
      final TestGesture second = await tester.startGesture(
        const Offset(160, 100),
        pointer: 2,
      );
      await first.up();
      expect(
        log.events,
        isEmpty,
        reason: 'the gesture is decided when the last finger leaves',
      );
      await second.up();

      expect(log.events, <InputEvent>[
        _press(PointerButton.right),
        _release(PointerButton.right),
      ]);
    });

    testWidgets('three-finger tap toggles the keyboard and clicks nothing', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final List<TestGesture> fingers = <TestGesture>[
        await tester.startGesture(const Offset(100, 100), pointer: 1),
        await tester.startGesture(const Offset(160, 100), pointer: 2),
        await tester.startGesture(const Offset(220, 100), pointer: 3),
      ];
      for (final TestGesture finger in fingers) {
        await finger.up();
      }

      expect(log.keyboardToggles, 1);
      expect(log.events, isEmpty, reason: 'three fingers are not a click');
    });

    testWidgets('one-finger drag moves the pointer and clicks nothing', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );
      for (int i = 0; i < 8; i++) {
        await finger.moveBy(const Offset(10, 5));
      }
      expect(
        log.motions.length,
        8,
        reason: 'motion is relative, so it starts on the first move with no '
            'slop to wait out',
      );
      await finger.up();

      expect(log.buttons, isEmpty);
      expect(log.travelled, const Offset(80, 40));
    });

    testWidgets('two-finger drag scrolls and ends the scroll on lift', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture left = await tester.startGesture(
        const Offset(100, 300),
        pointer: 1,
      );
      final TestGesture right = await tester.startGesture(
        const Offset(160, 300),
        pointer: 2,
      );
      for (int i = 0; i < 5; i++) {
        await left.moveBy(const Offset(0, -12));
        await right.moveBy(const Offset(0, -12));
      }
      expect(log.motions, isEmpty, reason: 'two fingers do not move a pointer');
      expect(
        log.scrolled,
        const Offset(0, -60),
        reason: 'the pair scrolls by what one finger travelled',
      );

      await left.up();
      await right.up();

      expect(log.buttons, isEmpty);
      expect(
        log.events.last,
        const InputEvent.fingerScrollEnd(),
        reason: 'without a terminal event kinetic scrolling never settles',
      );
      expect(
        log.events.whereType<InputEvent_FingerScrollEnd>().length,
        1,
        reason: 'exactly one, at the end',
      );
    });

    testWidgets('long press then drag holds the left button', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );

      // The event clock, not a timer: the press became long enough while
      // nothing was being dispatched, and the move that follows is what reads
      // the elapsed time and locks the drag.
      await finger.moveBy(
        const Offset(4, 0),
        timeStamp: const Duration(milliseconds: 600),
      );
      expect(log.dragLocks, 1);
      expect(log.events.first, _press(PointerButton.left));

      for (int i = 0; i < 4; i++) {
        await finger.moveBy(
          const Offset(10, 0),
          timeStamp: const Duration(milliseconds: 620),
        );
      }
      await finger.up();

      expect(log.events.last, _release(PointerButton.left));
      expect(log.travelled, const Offset(44, 0));
      expect(
        log.buttons.length,
        2,
        reason: 'one press and one release, never a click on top',
      );
    });
  });

  group('what a gesture is not', () {
    testWidgets('a drag that ends where it started is still a drag', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );
      await finger.moveBy(const Offset(60, 0));
      await finger.moveBy(const Offset(-60, 0));
      await finger.up();

      expect(log.buttons, isEmpty);
    });

    testWidgets('jitter under the slop is still a tap', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );
      await finger.moveBy(const Offset(2, 1));
      await finger.moveBy(const Offset(-1, 1));
      await finger.up();

      expect(log.buttons, <InputEvent>[
        _press(PointerButton.left),
        _release(PointerButton.left),
      ]);
      expect(
        log.motions,
        isNotEmpty,
        reason: 'the pointer still moved by the jitter, which costs nothing',
      );
    });

    testWidgets('a cancelled gesture decides nothing', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );
      await finger.cancel();

      expect(log.events, isEmpty);
      expect(log.keyboardToggles, 0);
    });

    testWidgets('four fingers say nothing', (WidgetTester tester) async {
      final (_, _Recorder log) = await _surface(tester);
      final List<TestGesture> fingers = <TestGesture>[];
      for (int i = 0; i < 4; i++) {
        fingers.add(
          await tester.startGesture(Offset(100 + 60.0 * i, 100), pointer: i + 1),
        );
      }
      for (final TestGesture finger in fingers) {
        await finger.up();
      }

      expect(log.events, isEmpty);
      expect(log.keyboardToggles, 0);
    });
  });

  group("the client's own controls", () {
    testWidgets('a three-finger swipe down asks for them', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final List<TestGesture> fingers = await _threeFingers(tester);
      for (final TestGesture finger in fingers) {
        await finger.moveBy(const Offset(0, 40));
      }

      // Asserted before the fingers lift: the swipe is read from how far they
      // have got, so nothing is waiting on the end of the gesture.
      expect(log.controlsShown, 1);
      expect(log.controlsHidden, 0);

      await _lift(fingers);
      expect(log.events, isEmpty, reason: 'a swipe is not a click');
      expect(
        log.keyboardToggles,
        0,
        reason: 'a swipe left the slop, so it was never a tap',
      );
    });

    testWidgets('a three-finger swipe up puts them away', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final List<TestGesture> fingers = await _threeFingers(tester);
      for (final TestGesture finger in fingers) {
        await finger.moveBy(const Offset(0, -40));
      }
      await _lift(fingers);

      expect(log.controlsHidden, 1);
      expect(log.controlsShown, 0);
      expect(log.events, isEmpty);
    });

    testWidgets('one swipe asks once, however far the fingers travel', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final List<TestGesture> fingers = await _threeFingers(tester);
      for (int i = 0; i < 6; i++) {
        for (final TestGesture finger in fingers) {
          await finger.moveBy(const Offset(0, 30));
        }
      }
      await _lift(fingers);

      expect(log.controlsShown, 1);
    });

    testWidgets('three fingers travelling sideways say nothing', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final List<TestGesture> fingers = await _threeFingers(tester);
      for (final TestGesture finger in fingers) {
        await finger.moveBy(const Offset(120, 0));
      }
      await _lift(fingers);

      expect(log.controlsShown, 0);
      expect(log.controlsHidden, 0);
      expect(log.events, isEmpty);
      expect(log.keyboardToggles, 0);
    });

    testWidgets('no row of the table above asks for them', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);

      final TestGesture tap = await tester.startGesture(const Offset(100, 100));
      await tap.up();
      expect(log.buttons, <InputEvent>[
        _press(PointerButton.left),
        _release(PointerButton.left),
      ], reason: 'one-finger tap is still a left click');

      final TestGesture second = await tester.startGesture(
        const Offset(100, 100),
        pointer: 2,
      );
      final TestGesture third = await tester.startGesture(
        const Offset(160, 100),
        pointer: 3,
      );
      await second.up();
      await third.up();
      expect(log.buttons.last, _release(PointerButton.right),
          reason: 'two-finger tap is still a right click');

      final List<TestGesture> resting = await _threeFingers(tester);
      await _lift(resting);
      expect(
        log.keyboardToggles,
        1,
        reason: 'three fingers that stay put still toggle the keyboard',
      );

      final TestGesture drag = await tester.startGesture(
        const Offset(100, 100),
        pointer: 7,
      );
      await drag.moveBy(const Offset(0, 40));
      await drag.up();
      expect(
        log.travelled,
        const Offset(0, 40),
        reason: 'one finger still moves the pointer, downwards included',
      );

      final TestGesture pairLeft = await tester.startGesture(
        const Offset(100, 300),
        pointer: 8,
      );
      final TestGesture pairRight = await tester.startGesture(
        const Offset(160, 300),
        pointer: 9,
      );
      await pairLeft.moveBy(const Offset(0, 40));
      await pairRight.moveBy(const Offset(0, 40));
      await pairLeft.up();
      await pairRight.up();
      expect(
        log.scrolled,
        const Offset(0, 40),
        reason: 'two fingers still scroll, downwards included',
      );

      final TestGesture held = await tester.startGesture(
        const Offset(400, 300),
        pointer: 10,
      );
      await held.moveBy(
        const Offset(0, 40),
        timeStamp: const Duration(milliseconds: 600),
      );
      await held.up();
      expect(log.dragLocks, 1, reason: 'a long press still locks the drag');

      expect(
        <int>[log.controlsShown, log.controlsHidden],
        <int>[0, 0],
        reason: 'not one row of the table reached the controls',
      );
    });
  });

  group('streamed-image pixels', () {
    testWidgets('motion is scaled out of logical pixels', (
      WidgetTester tester,
    ) async {
      // A 2400-pixel-wide image drawn 800 logical pixels wide: one finger pixel
      // is three image pixels.
      final (_, _Recorder log) = await _surface(
        tester,
        scale: ImageScale.of(
          image: const Size(2400, 1080),
          box: const Size(800, 360),
        ),
      );
      final TestGesture finger = await tester.startGesture(
        const Offset(100, 100),
      );
      await finger.moveBy(const Offset(10, 10));
      await finger.up();

      expect(log.travelled, const Offset(30, 30));
    });

    test('a box of zero width does not divide by it', () {
      expect(
        ImageScale.of(image: const Size(2400, 1080), box: Size.zero),
        ImageScale.identity,
      );
    });
  });

  group('a real mouse', () {
    testWidgets('the secondary button is a right click with no counting', (
      WidgetTester tester,
    ) async {
      final (_, _Recorder log) = await _surface(tester);
      final TestGesture mouse = await tester.startGesture(
        const Offset(100, 100),
        kind: PointerDeviceKind.mouse,
        buttons: kSecondaryMouseButton,
      );
      expect(
        log.events,
        <InputEvent>[_press(PointerButton.right)],
        reason: 'a mouse says which button it is, so nothing waits for a lift',
      );
      await mouse.up();

      expect(log.events.last, _release(PointerButton.right));
    });

    testWidgets('the wheel is a wheel, not a finger scroll', (
      WidgetTester tester,
    ) async {
      final (Touchpad pad, _Recorder log) = await _surface(tester);
      pad.signal(
        const PointerScrollEvent(
          position: Offset(100, 100),
          scrollDelta: Offset(0, 53),
        ),
      );

      expect(log.events, <InputEvent>[const InputEvent.wheel(dx: 0, dy: 53)]);
    });
  });
}
