/// Picture-in-picture, on the one platform that has it.
///
/// A session is a live stream, and the moment the user leaves the app the
/// window stops drawing while the host goes on encoding for nobody. Android can
/// fold the window into a thumbnail that keeps drawing instead, so a glance at a
/// message costs the session nothing and the user comes back to a stream that
/// never stopped. No other platform this client builds for offers the same
/// thing, so off Android every call here does nothing at all.
library;

import 'dart:io' show Platform;

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

/// Where the Android half answers.
const MethodChannel _pip = MethodChannel('tachyx/pip');

/// Whether the window is a thumbnail, and the two ways to say it may become
/// one.
///
/// There is one instance, because there is one window. A method channel holds a
/// single call handler, so a second instance would take the platform's reports
/// away from the first and say nothing about it.
class Pip {
  Pip._() {
    if (Platform.isAndroid) {
      _pip.setMethodCallHandler(_onPlatformCall);
    }
  }

  /// The one instance.
  static final Pip instance = Pip._();

  final ValueNotifier<bool> _active = ValueNotifier<bool>(false);

  /// Whether the window is a picture-in-picture thumbnail right now.
  ///
  /// A thumbnail is a few hundred pixels across, with no room for a control and
  /// no touch to spend on one, so the session shows the picture and nothing
  /// else while this holds. Only the platform sets it, because the system folds
  /// and unfolds the window on its own as well, and a client that tracked its
  /// own requests instead would be guessing.
  ValueNotifier<bool> get active => _active;

  /// Lets the system fold the app into a thumbnail when the user leaves.
  ///
  /// [aspectWidth] and [aspectHeight] is the video picture's shape, which the
  /// thumbnail is given so that the picture fills it without bars. There is no
  /// event for the user leaving that arrives in time to answer, so the
  /// permission is granted ahead of time and stands until [disarm].
  ///
  /// [source] is where the picture sits in the window, in physical pixels.
  /// The system shrinks that rectangle into the thumbnail instead of
  /// crossfading the whole window, so the fold reads as the picture moving
  /// rather than the app being replaced. Null leaves the crossfade.
  Future<void> arm({
    required int aspectWidth,
    required int aspectHeight,
    Rect? source,
  }) => _tell('arm', <String, int>{
    'width': aspectWidth,
    'height': aspectHeight,
    if (source != null) ...<String, int>{
      'left': source.left.round(),
      'top': source.top.round(),
      'right': source.right.round(),
      'bottom': source.bottom.round(),
    },
  });

  /// Stops that, for when there is no live session.
  ///
  /// A menu has nothing to keep drawing, and folding it into a thumbnail would
  /// leave the user holding a picture of a button.
  Future<void> disarm() => _tell('disarm', null);

  Future<void> _tell(String method, Object? arguments) async {
    if (!Platform.isAndroid) return;
    await _pip.invokeMethod<void>(method, arguments);
  }

  /// The platform reporting that the window has folded or unfolded.
  Future<void> _onPlatformCall(MethodCall call) async {
    if (call.method == 'changed') {
      _active.value = call.arguments as bool? ?? false;
    }
  }
}
