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
