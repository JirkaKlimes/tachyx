/// The decoded picture, drawn straight from the decoder's own texture.
library;

import 'package:flutter/foundation.dart';
import 'package:flutter/widgets.dart';

import 'package:tachyx/src/session/cursor.dart';
import 'package:tachyx/src/session/present.dart';
import 'package:tachyx/src/ui/cursor_layer.dart';

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
