/// The remote pointer, drawn over the video.
library;

import 'dart:math' as math;

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
        painter: CursorPainter(
          cursor,
          pixelRatio: MediaQuery.devicePixelRatioOf(context),
        ),
        size: Size.infinite,
      ),
    ),
  );
}

/// Paints one pointer.
class CursorPainter extends CustomPainter {
  /// Repaints whenever [cursor] changes, onto a grid of [pixelRatio] device
  /// pixels to the logical pixel.
  CursorPainter(this.cursor, {this.pixelRatio = 1}) : super(repaint: cursor);

  /// Where the pointer is.
  final ValueListenable<RemoteCursor?> cursor;

  /// Device pixels to the logical pixel, which is the grid the picture is put
  /// on. See [onWholePixels].
  final double pixelRatio;

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
      onWholePixels(rect),
      Paint()..filterQuality = FilterQuality.medium,
    );
  }

  /// [rect] moved and sized onto whole device pixels.
  ///
  /// The pointer is a few dozen pixels of hard edges, and the prediction puts
  /// it wherever the finger asked for, which is almost never a whole pixel of
  /// anything. Drawn there, all four edges fall between two device pixels and
  /// the filter smears the picture across both. That is what makes a remote
  /// pointer look soft beside the one the platform draws, and it is worse the
  /// smaller the picture is, which on a phone is very small indeed.
  ///
  /// A cursor the hardware draws sits on whole device pixels. This puts this
  /// one there. The cost is up to half a device pixel of position, which is
  /// less than the width of the blur it buys back.
  @visibleForTesting
  Rect onWholePixels(Rect rect) {
    if (pixelRatio <= 0) return rect;
    double whole(double logical) =>
        (logical * pixelRatio).roundToDouble() / pixelRatio;
    // A picture rounded away to nothing is a pointer that vanishes, which is
    // worse than one drawn a fraction too large.
    final double least = 1 / pixelRatio;
    return Rect.fromLTWH(
      whole(rect.left),
      whole(rect.top),
      math.max(least, whole(rect.width)),
      math.max(least, whole(rect.height)),
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
  bool shouldRepaint(CursorPainter old) =>
      old.cursor != cursor || old.pixelRatio != pixelRatio;
}
