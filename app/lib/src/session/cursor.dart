/// The remote pointer, drawn here rather than sent in the picture.
///
/// The host captures without the cursor painted in, and reports where it is on
/// the control stream instead. That is what lets the pointer move the instant
/// the finger does: the client steers its own pointer and the host's reports
/// correct it, so no round trip sits inside a gesture. See
/// `crates/tachyx-client/src/cursor.rs`, which does the steering and the
/// correcting; this file only draws the answer.
library;

import 'dart:async';
import 'dart:ui' as ui;

import 'package:flutter/scheduler.dart';
import 'package:flutter/widgets.dart';

import 'package:tachyx/src/rust/api/session.dart' as rust;

/// How large the pointer's picture is against the video it sits on, and where
/// its hotspot is inside it.
///
/// Every measurement is a fraction of the video picture, so neither a step of
/// the host's resolution ladder nor a resize of the window changes what one
/// means, and a capture whose aspect ratio the encoder corrected still comes
/// out square: the two axes are scaled apart.
@immutable
class CursorFit {
  /// A picture [width] by [height] of the video, whose hotspot sits [hotspotX]
  /// and [hotspotY] in from its top left corner.
  const CursorFit({
    required this.width,
    required this.height,
    required this.hotspotX,
    required this.hotspotY,
  });

  /// The picture's width, as a fraction of the video's width.
  final double width;

  /// Its height, as a fraction of the video's height.
  final double height;

  /// How far left of the hotspot the picture starts, as a fraction of the
  /// video's width.
  final double hotspotX;

  /// How far above the hotspot it starts, as a fraction of the video's height.
  final double hotspotY;

  /// Where the picture goes when its hotspot lands at [hotspot] in a video box
  /// of [box] logical pixels.
  Rect rectAt(Offset hotspot, Size box) => Rect.fromLTWH(
    hotspot.dx - hotspotX * box.width,
    hotspot.dy - hotspotY * box.height,
    width * box.width,
    height * box.height,
  );

  @override
  bool operator ==(Object other) =>
      other is CursorFit &&
      other.width == width &&
      other.height == height &&
      other.hotspotX == hotspotX &&
      other.hotspotY == hotspotY;

  @override
  int get hashCode => Object.hash(width, height, hotspotX, hotspotY);

  @override
  String toString() => 'CursorFit($width, $height, $hotspotX, $hotspotY)';
}

/// The pointer's decoded picture, and how it fits the video.
@immutable
class CursorArt {
  /// The picture [image], drawn to [fit].
  const CursorArt({
    required this.serial,
    required this.image,
    required this.fit,
  });

  /// Which picture this is. It changes only when the picture does.
  final int serial;

  /// The decoded rows.
  final ui.Image image;

  /// How large it is against the video.
  final CursorFit fit;
}

/// Where the pointer is and what to draw there.
@immutable
class RemoteCursor {
  /// A pointer whose hotspot is at [x], [y] of the video picture.
  const RemoteCursor({
    required this.x,
    required this.y,
    required this.confirmed,
    this.art,
  });

  /// The hotspot, from the left edge of the video picture.
  final double x;

  /// The hotspot, from its top edge.
  final double y;

  /// Whether the host confirms this position.
  ///
  /// False on a host whose compositor offers no cursor session. The pointer is
  /// then the client's own reckoning and it drifts, which the drawing says out
  /// loud rather than hiding.
  final bool confirmed;

  /// The host's picture, or null when it has sent none and a pointer of the
  /// client's own is drawn instead.
  final CursorArt? art;

  /// Where the hotspot lands in a video box of [box] logical pixels.
  Offset hotspotIn(Size box) => Offset(x * box.width, y * box.height);

  /// Where the picture goes in that box, or null when there is no picture.
  Rect? rectIn(Size box) => art?.fit.rectAt(hotspotIn(box), box);

  @override
  bool operator ==(Object other) =>
      other is RemoteCursor &&
      other.x == x &&
      other.y == y &&
      other.confirmed == confirmed &&
      other.art?.serial == art?.serial;

  @override
  int get hashCode => Object.hash(x, y, confirmed, art?.serial);

  @override
  String toString() =>
      'RemoteCursor($x, $y, confirmed: $confirmed, art: ${art?.serial})';
}

/// Reads the pointer out of the session once a frame.
///
/// A frame, not a timer: the pointer is drawn on top of the video and there is
/// nothing to be gained by moving it between two frames nobody sees. The
/// picture is fetched only on the frame its serial changes, which is once every
/// few seconds against sixty reads of the position a second.
class CursorFeed {
  /// Polls [session] on every frame of [vsync].
  CursorFeed({required rust.Session? session, required TickerProvider vsync})
    : _session = session {
    if (session != null) _ticker = vsync.createTicker(_tick)..start();
  }

  final rust.Session? _session;
  Ticker? _ticker;

  /// What to draw, or null when there is nothing to draw.
  final ValueNotifier<RemoteCursor?> cursor = ValueNotifier<RemoteCursor?>(
    null,
  );

  /// The picture last decoded, kept until one replaces it.
  CursorArt? _art;

  /// The serial being decoded, so that a slow decode is not started twice.
  int? _decoding;

  void _tick(Duration _) {
    final rust.Cursor? reading = _session?.cursor();
    if (reading == null) {
      cursor.value = null;
      return;
    }
    final rust.CursorShape? shape = reading.image;
    if (shape != null && shape.serial.toInt() != (_art?.serial ?? -1)) {
      unawaited(_decode(shape));
    }
    if (_drawn(reading)) return;
    cursor.value = RemoteCursor(
      x: reading.x,
      y: reading.y,
      confirmed: reading.confirmed,
      // The picture in hand until a new one finishes decoding. Dropping the old
      // one meanwhile would blink the pointer on every widget it crosses.
      art: _art,
    );
  }

  /// Whether [reading] is already what the notifier holds.
  ///
  /// The notifier drops a value equal to the one it has, so this changes no
  /// behaviour. It only keeps a pointer that is standing still from allocating
  /// a reading a frame to be thrown away.
  bool _drawn(rust.Cursor reading) {
    final RemoteCursor? held = cursor.value;
    return held != null &&
        held.x == reading.x &&
        held.y == reading.y &&
        held.confirmed == reading.confirmed &&
        identical(held.art, _art);
  }

  /// Decodes the picture behind [shape], if the session still has it.
  ///
  /// The rows cross the boundary here and nowhere else. A picture whose serial
  /// has already moved on is not worth decoding, and the session says so by
  /// answering nothing.
  Future<void> _decode(rust.CursorShape shape) async {
    final int serial = shape.serial.toInt();
    if (_decoding == serial) return;
    _decoding = serial;
    final rust.CursorImage? rows = _session?.cursorImage(serial: shape.serial);
    if (rows == null) return;
    final ui.ImmutableBuffer buffer = await ui.ImmutableBuffer.fromUint8List(
      rows.pixels,
    );
    final ui.ImageDescriptor descriptor = ui.ImageDescriptor.raw(
      buffer,
      width: rows.width,
      height: rows.height,
      // The host sends what the compositor gave it: one little-endian word a
      // pixel, so blue, green, red then alpha in memory, premultiplied.
      pixelFormat: ui.PixelFormat.bgra8888,
    );
    final ui.Codec codec = await descriptor.instantiateCodec();
    final ui.FrameInfo frame = await codec.getNextFrame();
    codec.dispose();
    descriptor.dispose();
    _art?.image.dispose();
    _art = CursorArt(
      serial: serial,
      image: frame.image,
      fit: CursorFit(
        width: shape.width,
        height: shape.height,
        hotspotX: shape.hotspotX,
        hotspotY: shape.hotspotY,
      ),
    );
  }

  /// Stops polling and releases the picture.
  void dispose() {
    _ticker?.dispose();
    _ticker = null;
    _art?.image.dispose();
    _art = null;
    cursor.dispose();
  }
}
