/// What the client tells a host about the screen it will draw on.
///
/// Both the hello and the renegotiation that follows a rotation are built from
/// these two, so they are read from the view rather than from a widget: the
/// answer is a property of the window and not of where in the tree it is asked
/// for.
library;

import 'dart:ui' show FlutterView;

import 'package:flutter/widgets.dart';

/// The surface, in physical pixels.
///
/// Physical rather than logical, because the host encodes pixels and a logical
/// pixel is a different number on every device.
(int, int) viewportOf(FlutterView view) {
  final Size surface = view.physicalSize;
  return (
    surface.width.round().clamp(1, 0xFFFF),
    surface.height.round().clamp(1, 0xFFFF),
  );
}

/// What the display refreshes at, in hertz.
///
/// It bounds the frame rate the client asks for, because a frame the display
/// never shows costs everything a shown one costs. An embedder that does not
/// know reports zero, and asking a host for zero frames a second would be
/// asking it for none, so the fallback is sixty.
int refreshHz(FlutterView view) {
  final double reported = view.display.refreshRate;
  return reported.isFinite && reported >= 1
      ? reported.round().clamp(1, 0xFFFF)
      : 60;
}
