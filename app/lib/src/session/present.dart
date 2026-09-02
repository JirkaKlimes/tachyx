/// Where a decoded picture is presented, which is not one mechanism.
library;

import 'dart:io' show Platform;

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';
import 'package:irondash_engine_context/irondash_engine_context.dart';

import 'package:tachyx/src/rust/api/session.dart';

/// Where the Android half answers.
const MethodChannel _video = MethodChannel('tachyx/video');

/// What a decoded picture is drawn through.
///
/// Linux registers a texture with the engine and the decode pipeline draws into
/// it; Android opens a surface producer and the platform decoder writes into
/// that. The difference reaches as far as the widget, because only one of the
/// two applies the crop rectangle the bitstream carries, so it is written down
/// here rather than tested for at each use.
sealed class Presentation {
  const Presentation(this.textureId);

  /// Opens what this platform presents through and points [session] at it.
  ///
  /// [width] and [height] are the client's own surface in physical pixels,
  /// which is the best guess at the size the decoder will write before the host
  /// has said. [resize] corrects it as soon as the host has.
  static Future<Presentation> open(
    Session session, {
    required int width,
    required int height,
  }) => Platform.isAndroid
      ? _Produced.open(session, width: width, height: height)
      : _Registered.open(session);

  /// What a `Texture` widget draws.
  final int textureId;

  /// How much of that texture is the picture.
  VideoCrop crop(Encoding? encoding);

  /// Tells the platform what size the decoder is writing.
  Future<void> resize(Encoding encoding);

  /// Gives it back.
  Future<void> close();
}

/// A texture the engine registered, which the decode pipeline draws into.
///
/// The pipeline hands over frames the decoder has already cropped, so the whole
/// texture is the picture.
final class _Registered extends Presentation {
  const _Registered(super.textureId);

  static Future<Presentation> open(Session session) async {
    final int handle = await EngineContext.instance.getEngineHandle();
    return _Registered(session.presentOn(engineHandle: handle));
  }

  @override
  VideoCrop crop(Encoding? encoding) => VideoCrop.whole;

  @override
  Future<void> resize(Encoding encoding) async {}

  @override
  Future<void> close() async {}
}

/// A surface producer the platform decoder writes into.
///
/// The image reader behind it draws the decoder's buffer as it stands and
/// applies no crop, so the rows and columns the macroblock grid added have to be
/// taken off here. The surface texture the engine falls back to on a device
/// whose hardware buffers are defective does apply it, and says so, in which
/// case taking them off again would enlarge the picture past its own edges.
final class _Produced extends Presentation {
  const _Produced(super.textureId, {required this.appliesCrop});

  static Future<Presentation> open(
    Session session, {
    required int width,
    required int height,
  }) async {
    final Map<String, Object?> opened =
        await _video.invokeMapMethod<String, Object?>('open', <String, int>{
          'width': width,
          'height': height,
        }) ??
        const <String, Object?>{};
    final int id = opened['id'] as int? ?? -1;
    try {
      session.presentOn(engineHandle: id);
    } on Object {
      await _video.invokeMethod<void>('close', <String, int>{'id': id});
      rethrow;
    }
    return _Produced(id, appliesCrop: opened['appliesCrop'] as bool? ?? false);
  }

  /// Whether the producer has already taken the crop off.
  final bool appliesCrop;

  @override
  VideoCrop crop(Encoding? encoding) => encoding == null || appliesCrop
      ? VideoCrop.whole
      : VideoCrop.of(encoding);

  @override
  Future<void> resize(Encoding encoding) => _video.invokeMethod<void>(
    'resize',
    <String, int>{
      'id': textureId,
      'width': encoding.codedWidth,
      'height': encoding.codedHeight,
    },
  );

  @override
  Future<void> close() =>
      _video.invokeMethod<void>('close', <String, int>{'id': textureId});
}

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
