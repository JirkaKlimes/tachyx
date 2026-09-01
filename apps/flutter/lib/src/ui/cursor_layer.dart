/// The remote pointer, drawn over the video.
library;

import 'package:flutter/foundation.dart';
import 'package:flutter/widgets.dart';

import 'package:tachyx/src/session/cursor.dart';

/// The pointer this client draws when the host has sent no picture.
///
/// A host whose compositor offers no cursor session can say where the pointer
/// is or nothing at all, and on this one it says nothing. Drawing something is
/// still right: a touch surface with no pointer on it is a trackpad the user
/// cannot see, which is the whole fault this path exists to fix. It is drawn
/// differently on purpose, so that a pointer nothing corrects does not look
/// like one that is confirmed.
@immutable
class FallbackPointer {
  const FallbackPointer._();

  /// How tall the drawn arrow is, in logical pixels.
  ///
  /// A fixed size rather than a fraction of the video: the host's own cursor
  /// scales with its screen because it is a picture of one, and this is not a
  /// picture of anything. It is what the client knows, drawn at a size a thumb
  /// can find.
  static const double height = 22;

  /// The arrow, with its tip at the origin.
  static Path path() => Path()
    ..moveTo(0, 0)
    ..lineTo(0, height)
    ..lineTo(height * 0.28, height * 0.72)
    ..lineTo(height * 0.48, height * 1.06)
    ..lineTo(height * 0.62, height * 0.99)
    ..lineTo(height * 0.42, height * 0.66)
    ..lineTo(height * 0.7, height * 0.64)
    ..close();
}

/// Draws the pointer over whatever it is stacked on.
///
/// It repaints from the notifier rather than from a rebuild: the pointer moves
/// every frame and the video under it does not need laying out again for that.
class CursorLayer extends StatelessWidget {
  /// Draws whatever [cursor] holds, filling its parent.
  const CursorLayer({required this.cursor, super.key});

  /// Where the pointer is, updated once a frame.
  final ValueListenable<RemoteCursor?> cursor;

  @override
  Widget build(BuildContext context) => IgnorePointer(
    child: RepaintBoundary(
      child: CustomPaint(
        painter: CursorPainter(cursor),
        size: Size.infinite,
      ),
    ),
  );
}

/// Paints one pointer.
class CursorPainter extends CustomPainter {
  /// Repaints whenever [cursor] changes.
  CursorPainter(this.cursor) : super(repaint: cursor);

  /// Where the pointer is.
  final ValueListenable<RemoteCursor?> cursor;

  @override
  void paint(Canvas canvas, Size size) {
    final RemoteCursor? drawn = cursor.value;
    if (drawn == null || size.isEmpty) return;
    final Rect? rect = drawn.rectIn(size);
    final CursorArt? art = drawn.art;
    if (rect == null || art == null) {
      _fallback(canvas, drawn.hotspotIn(size));
      return;
    }
    canvas.drawImageRect(
      art.image,
      Rect.fromLTWH(
        0,
        0,
        art.image.width.toDouble(),
        art.image.height.toDouble(),
      ),
      rect,
      Paint()..filterQuality = FilterQuality.medium,
    );
  }

  /// The client's own arrow, at [hotspot].
  void _fallback(Canvas canvas, Offset hotspot) {
    canvas.save();
    canvas.translate(hotspot.dx, hotspot.dy);
    final Path arrow = FallbackPointer.path();
    canvas.drawPath(
      arrow,
      Paint()
        ..color = const Color(0xFF000000)
        ..style = PaintingStyle.stroke
        ..strokeWidth = 2.5
        ..strokeJoin = StrokeJoin.round,
    );
    canvas.drawPath(arrow, Paint()..color = const Color(0xFFFFFFFF));
    canvas.restore();
  }

  @override
  bool shouldRepaint(CursorPainter old) => old.cursor != cursor;
}
