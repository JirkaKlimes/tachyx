/// The decoded picture, drawn straight from the decoder's own texture.
library;

import 'package:flutter/foundation.dart';
import 'package:flutter/widgets.dart';
import 'package:irondash_engine_context/irondash_engine_context.dart';

import 'package:tachyx/src/rust/api/session.dart' show Encoding;
import 'package:tachyx/src/rust/api/video.dart';
import 'package:tachyx/src/session/cursor.dart';
import 'package:tachyx/src/ui/cursor_layer.dart';

/// How much of a decoded texture is the picture.
///
/// H.264 codes whole macroblocks, so a picture 1080 rows tall travels as 1088
/// rows with a crop rectangle that says which eight to throw away, and 2580
/// columns travel as 2592. Android's surface producer draws the decoder's buffer
/// as it stands and applies no such rectangle, so the client applies it.
///
/// It is applied by scale rather than by layout: the texture is drawn to fill
/// its box and then enlarged about its top left corner until the part that is
/// the picture is the part the box shows. That is one matrix on a layer already
/// being composited, and no second pass over the pixels.
@immutable
class VideoCrop {
  /// A picture whose visible size is [scaleX] and [scaleY] times smaller than
  /// the buffer it was decoded into.
  const VideoCrop({required this.scaleX, required this.scaleY});

  /// Every row and column of the texture is picture, which is what a decoder
  /// that crops for itself hands over.
  static const VideoCrop whole = VideoCrop(scaleX: 1, scaleY: 1);

  /// What the host says it is coding against what it says is visible.
  factory VideoCrop.of(Encoding encoding) => VideoCrop(
    scaleX: _ratio(encoding.codedWidth, encoding.visibleWidth),
    scaleY: _ratio(encoding.codedHeight, encoding.visibleHeight),
  );

  static double _ratio(int coded, int visible) =>
      visible <= 0 || coded <= visible ? 1 : coded / visible;

  /// How far the texture is enlarged across.
  final double scaleX;

  /// How far it is enlarged down.
  final double scaleY;

  /// Whether there is anything to take off at all.
  bool get isWhole => scaleX == 1 && scaleY == 1;

  @override
  bool operator ==(Object other) =>
      other is VideoCrop && other.scaleX == scaleX && other.scaleY == scaleY;

  @override
  int get hashCode => Object.hash(scaleX, scaleY);

  @override
  String toString() => 'VideoCrop($scaleX, $scaleY)';
}

/// Owns the native texture and the decode pipeline behind it.
///
/// Nothing decodes until the [VideoTextureView] is on screen. The pipeline needs
/// the embedder's OpenGL context, and the only place that context can be read is
/// inside the callback the engine makes when it first draws the texture.
class VideoTextureController {
  VideoTextureController._(this._texture) : textureId = _texture.id();

  /// Registers a texture with the engine.
  ///
  /// Must be awaited on the platform thread, which is where the engine keeps
  /// its texture registry.
  static Future<VideoTextureController> open() async {
    final handle = await EngineContext.instance.getEngineHandle();
    return VideoTextureController._(openVideoTexture(engineHandle: handle));
  }

  final VideoTexture _texture;

  /// What a [VideoTextureView] draws.
  final int textureId;

  /// Hands one encoded access unit to the decoder.
  void push(Uint8List accessUnit) => _texture.push(accessUnit: accessUnit);

  /// Tells the decoder the stream has ended.
  void finish() => _texture.finish();

  /// What the path has counted so far.
  PresentStats get stats => _texture.stats();

  /// The first thing that went wrong, if anything has.
  String? get failure => _texture.failure();

  /// Releases the texture and stops the pipeline.
  void dispose() {
    _texture.finish();
    _texture.dispose();
  }
}

/// Draws the texture a [Presentation] owns.
class VideoTextureView extends StatelessWidget {
  /// Draws `textureId`, letterboxed to `aspectRatio` and cropped by `crop`,
  /// with the host's pointer over it.
  const VideoTextureView({
    required this.textureId,
    required this.aspectRatio,
    this.crop = VideoCrop.whole,
    this.cursor,
    super.key,
  });

  /// The texture to draw.
  final int textureId;

  /// The stream's shape, so the picture is not stretched to the window.
  final double aspectRatio;

  /// How much of the texture is picture.
  final VideoCrop crop;

  /// Where the host's pointer is, when there is a session to ask.
  ///
  /// It is drawn inside the letterbox rather than over the whole window, so its
  /// fractions are fractions of the picture and nothing has to know how wide the
  /// black bars are. The crop is not applied to it: the crop enlarges the
  /// texture to hide the rows the macroblock grid added, and the pointer is
  /// placed against the picture those rows are not part of.
  final ValueListenable<RemoteCursor?>? cursor;

  @override
  Widget build(BuildContext context) => ColoredBox(
    color: const Color(0xFF000000),
    child: Center(
      child: AspectRatio(
        aspectRatio: aspectRatio,
        child: _pointed(
          _cropped(
            Texture(textureId: textureId, filterQuality: FilterQuality.low),
          ),
        ),
      ),
    ),
  );

  Widget _pointed(Widget picture) {
    final ValueListenable<RemoteCursor?>? cursor = this.cursor;
    return cursor == null
        ? picture
        : Stack(
            fit: StackFit.expand,
            children: <Widget>[picture, CursorLayer(cursor: cursor)],
          );
  }

  Widget _cropped(Widget texture) => crop.isWhole
      ? texture
      : ClipRect(
          child: Transform.scale(
            scaleX: crop.scaleX,
            scaleY: crop.scaleY,
            alignment: Alignment.topLeft,
            filterQuality: FilterQuality.low,
            child: texture,
          ),
        );
}
