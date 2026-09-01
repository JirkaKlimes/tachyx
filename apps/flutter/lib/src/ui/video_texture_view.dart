/// The decoded picture, drawn straight from the decoder's own texture.
library;

import 'dart:typed_data';

import 'package:flutter/widgets.dart';
import 'package:irondash_engine_context/irondash_engine_context.dart';

import 'package:tachyx/src/rust/api/video.dart';

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

  /// Releases the texture and stops the pipeline.
  void dispose() {
    _texture.finish();
    _texture.dispose();
  }
}

/// Draws the texture a [VideoTextureController] owns.
class VideoTextureView extends StatelessWidget {
  /// Draws `textureId`, letterboxed to `aspectRatio`.
  const VideoTextureView({
    required this.textureId,
    required this.aspectRatio,
    super.key,
  });

  /// The texture to draw.
  final int textureId;

  /// The stream's shape, so the picture is not stretched to the window.
  final double aspectRatio;

  @override
  Widget build(BuildContext context) => ColoredBox(
    color: const Color(0xFF000000),
    child: Center(
      child: AspectRatio(
        aspectRatio: aspectRatio,
        child: Texture(textureId: textureId, filterQuality: FilterQuality.low),
      ),
    ),
  );
}
