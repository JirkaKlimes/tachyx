import 'package:flutter/foundation.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/session/cursor.dart';
import 'package:tachyx/src/ui/cursor_layer.dart';

void main() {
  // Twice as many screen pixels per logical pixel across as down, which is what
  // catches a placement that scales both axes by one number.
  const Size box = Size(1000, 500);

  /// A 48-pixel cursor with a hotspot 5 across and 1 down, on a 2000x1000
  /// screen. This is the shape the host actually reports.
  const CursorFit arrow = CursorFit(
    width: 48 / 2000,
    height: 48 / 1000,
    hotspotX: 5 / 2000,
    hotspotY: 1 / 1000,
  );

  test('the hotspot lands where the fraction says', () {
    const RemoteCursor cursor = RemoteCursor(x: 0.25, y: 0.5, confirmed: true);
    expect(cursor.hotspotIn(box), const Offset(250, 250));
  });

  test('the picture is placed by its hotspot, not by its corner', () {
    final Rect rect = arrow.rectAt(const Offset(500, 250), box);
    expect(rect.left, closeTo(500 - 5 * 0.5, 1e-9));
    expect(rect.top, closeTo(250 - 1 * 0.5, 1e-9));
  });

  test('a square cursor stays square when the axes scale apart', () {
    final Rect rect = arrow.rectAt(Offset.zero, box);
    expect(rect.width, closeTo(24, 1e-9));
    expect(
      rect.height,
      closeTo(24, 1e-9),
      reason: 'one scale for both axes would flatten it to half',
    );
  });

  test('the picture grows with the box it is drawn in', () {
    final Rect small = arrow.rectAt(Offset.zero, box);
    final Rect large = arrow.rectAt(Offset.zero, box * 2);
    expect(large.width, closeTo(small.width * 2, 1e-9));
    expect(large.height, closeTo(small.height * 2, 1e-9));
  });

  // The pointer has to be the size it would be on the remote desktop, seen
  // through the video: the same fraction of the drawn picture as it is of the
  // screen it was captured on. Anything else leaves it out of scale with the
  // interface it is pointing at, which on a phone is what makes it look huge.
  group('the size the remote desktop would draw it', () {
    test('a phone shrinks it exactly as much as it shrinks the desktop', () {
      // The 2000-pixel-wide screen drawn 400 logical pixels wide: a fifth.
      final Rect rect = arrow.rectAt(const Offset(200, 100), const Size(400, 200));
      expect(rect.width, closeTo(48 / 5, 1e-9));
      expect(
        rect.width / 400,
        closeTo(48 / 2000, 1e-9),
        reason: 'the same share of the picture as of the screen it came from',
      );
    });

    test('a viewport larger than the desktop grows it by the same factor', () {
      final Rect rect = arrow.rectAt(
        const Offset(2000, 1000),
        const Size(4000, 2000),
      );
      expect(rect.width, closeTo(48 * 2, 1e-9));
      expect(rect.height, closeTo(48 * 2, 1e-9));
    });

    test('the hotspot keeps its place inside the picture under any scale', () {
      for (final Size drawn in <Size>[
        const Size(400, 200),
        const Size(2000, 1000),
        const Size(4000, 2000),
      ]) {
        final Rect rect = arrow.rectAt(
          Offset(drawn.width / 2, drawn.height / 2),
          drawn,
        );
        expect(
          (drawn.width / 2 - rect.left) / rect.width,
          closeTo(5 / 48, 1e-9),
          reason: 'the hotspot sits 5 pixels into a 48-pixel picture, always',
        );
      }
    });
  });

  group('a picture on whole device pixels', () {
    /// The painter for a screen of [ratio] device pixels to the logical pixel.
    CursorPainter painter(double ratio) => CursorPainter(
      ValueNotifier<RemoteCursor?>(null),
      pixelRatio: ratio,
    );

    test('a rectangle between pixels is moved onto them', () {
      final Rect snapped = painter(
        3,
      ).onWholePixels(const Rect.fromLTWH(10.1, 20.9, 11.7, 11.7));
      for (final double edge in <double>[
        snapped.left,
        snapped.top,
        snapped.width,
        snapped.height,
      ]) {
        expect(
          (edge * 3 - (edge * 3).roundToDouble()).abs(),
          lessThan(1e-9),
          reason: 'every edge has to land on a whole device pixel',
        );
      }
    });

    test('nothing moves by more than half a device pixel', () {
      const Rect wanted = Rect.fromLTWH(10.1, 20.9, 11.7, 4.4);
      final Rect snapped = painter(3).onWholePixels(wanted);
      expect((snapped.left - wanted.left).abs(), lessThan(0.5 / 3 + 1e-9));
      expect((snapped.top - wanted.top).abs(), lessThan(0.5 / 3 + 1e-9));
    });

    test('a picture too small to round to a pixel is still drawn', () {
      final Rect snapped = painter(
        1,
      ).onWholePixels(const Rect.fromLTWH(4, 4, 0.2, 0.2));
      expect(snapped.width, greaterThan(0));
      expect(snapped.height, greaterThan(0));
    });

    test('a screen of whole pixels is left where it was', () {
      const Rect already = Rect.fromLTWH(10, 20, 12, 12);
      expect(painter(1).onWholePixels(already), already);
    });
  });

  test('a pointer with no picture has no rectangle to draw', () {
    const RemoteCursor cursor = RemoteCursor(x: 0.5, y: 0.5, confirmed: false);
    expect(
      cursor.rectIn(box),
      isNull,
      reason: 'the client draws its own arrow at the hotspot instead',
    );
  });

  test('a pointer is the same pointer until it moves', () {
    const RemoteCursor first = RemoteCursor(x: 0.5, y: 0.5, confirmed: true);
    const RemoteCursor same = RemoteCursor(x: 0.5, y: 0.5, confirmed: true);
    const RemoteCursor moved = RemoteCursor(x: 0.6, y: 0.5, confirmed: true);
    const RemoteCursor unconfirmed = RemoteCursor(
      x: 0.5,
      y: 0.5,
      confirmed: false,
    );
    expect(first, same);
    expect(first, isNot(moved));
    expect(first, isNot(unconfirmed));
  });

  testWidgets('a layer with nothing to draw paints nothing', (
    WidgetTester tester,
  ) async {
    final ValueNotifier<RemoteCursor?> cursor = ValueNotifier<RemoteCursor?>(
      null,
    );
    addTearDown(cursor.dispose);
    await tester.pumpWidget(_layer(cursor));
    expect(tester.takeException(), isNull);
  });

  testWidgets('a pointer the host cannot confirm is still drawn', (
    WidgetTester tester,
  ) async {
    final ValueNotifier<RemoteCursor?> cursor = ValueNotifier<RemoteCursor?>(
      const RemoteCursor(x: 0.5, y: 0.5, confirmed: false),
    );
    addTearDown(cursor.dispose);
    await tester.pumpWidget(_layer(cursor));
    expect(tester.takeException(), isNull);
    expect(
      FallbackPointer.path().getBounds().isEmpty,
      isFalse,
      reason: 'a touch surface with no pointer on it is the fault this fixes',
    );
  });

  testWidgets('the layer repaints when the pointer moves, without a rebuild', (
    WidgetTester tester,
  ) async {
    final ValueNotifier<RemoteCursor?> cursor = ValueNotifier<RemoteCursor?>(
      const RemoteCursor(x: 0.1, y: 0.1, confirmed: true),
    );
    addTearDown(cursor.dispose);
    await tester.pumpWidget(_layer(cursor));
    final CustomPaint painted = tester.widget<CustomPaint>(
      find.byType(CustomPaint).first,
    );
    cursor.value = const RemoteCursor(x: 0.9, y: 0.9, confirmed: true);
    await tester.pump();
    expect(
      identical(
        tester.widget<CustomPaint>(find.byType(CustomPaint).first).painter,
        painted.painter,
      ),
      isTrue,
      reason: 'the painter listens to the notifier rather than being rebuilt',
    );
  });
}

Widget _layer(ValueListenable<RemoteCursor?> cursor) => Directionality(
  textDirection: TextDirection.ltr,
  child: Center(
    child: SizedBox(
      width: 200,
      height: 100,
      child: CursorLayer(cursor: cursor),
    ),
  ),
);
