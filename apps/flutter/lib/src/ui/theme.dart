/// The one place colours and type are decided.
library;

import 'package:flutter/material.dart';

/// The client's palette.
///
/// Dark, and quiet. The whole window is either a remote desktop or an empty
/// surface waiting to be one, and anything the client draws around that is in
/// the way.
abstract final class Shade {
  /// Behind everything.
  static const Color ground = Color(0xFF0B0C0E);

  /// Panels that sit over the surface.
  static const Color panel = Color(0xE615171B);

  /// Lines between things.
  static const Color edge = Color(0xFF2A2E36);

  /// Ordinary text.
  static const Color text = Color(0xFFE6E8EC);

  /// Labels and units.
  static const Color faint = Color(0xFF8A909C);

  /// Something is working.
  static const Color good = Color(0xFF4ADE80);

  /// Something failed and the user has to read it.
  static const Color bad = Color(0xFFF87171);
}

/// Numbers that change while the user watches them.
///
/// Fixed pitch, so a counter does not shuffle its neighbours sideways every
/// time it ticks over a digit.
const TextStyle readout = TextStyle(
  fontFamily: 'monospace',
  fontFamilyFallback: <String>['Courier'],
  fontSize: 12,
  height: 1.5,
  color: Shade.text,
);

/// The client's theme.
ThemeData tachyxTheme() {
  final ThemeData base = ThemeData.dark(useMaterial3: true);
  return base.copyWith(
    scaffoldBackgroundColor: Shade.ground,
    colorScheme: base.colorScheme.copyWith(
      surface: Shade.ground,
      primary: Shade.text,
      error: Shade.bad,
    ),
  );
}
