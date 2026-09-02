/// The one place colours, type and shape are decided.
library;

import 'package:flutter/material.dart';

/// The client's palette.
///
/// True black and greys, nothing else. The whole window is either a remote
/// desktop or an empty surface waiting to be one, and anything the client
/// draws around that is in the way. Colour is reserved for meaning: green says
/// the link is alive, red says something failed and has to be read. Nothing is
/// coloured to decorate.
abstract final class Shade {
  /// Behind everything. True black, so the chrome and the letterbox around
  /// the picture are the same surface.
  static const Color ground = Color(0xFF000000);

  /// Panels that sit over the picture: black glass, not grey card.
  static const Color panel = Color(0xF20B0C0F);

  /// Fields and pressed things, one step off the ground.
  static const Color raised = Color(0xFF101216);

  /// Hairlines between things.
  static const Color edge = Color(0xFF24272E);

  /// A hairline with attention on it.
  static const Color edgeLit = Color(0xFF464B55);

  /// Ordinary text.
  static const Color text = Color(0xFFF2F3F5);

  /// Labels and units.
  static const Color faint = Color(0xFF8A909C);

  /// Text that is present but not being offered: hints, disabled things.
  static const Color dim = Color(0xFF4C515B);

  /// Something is working.
  static const Color good = Color(0xFF4ADE80);

  /// Something failed and the user has to read it.
  static const Color bad = Color(0xFFF87171);
}

/// The corner every box in the client is cut at. Near-sharp: these are
/// instruments, not cards.
const double kCorner = 3;

/// The wordmark: thin and wide, and the one place the client is loud.
const TextStyle wordmark = TextStyle(
  fontSize: 32,
  fontWeight: FontWeight.w200,
  letterSpacing: 12,
  color: Shade.text,
);

/// Section labels: small, spaced, lowercase, out of the way.
const TextStyle label = TextStyle(
  fontSize: 11,
  letterSpacing: 2.4,
  color: Shade.faint,
);

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
      onPrimary: Shade.ground,
      error: Shade.bad,
    ),
    splashFactory: InkSparkle.splashFactory,
    sliderTheme: SliderThemeData(
      activeTrackColor: Shade.text,
      inactiveTrackColor: Shade.edge,
      thumbColor: Shade.text,
      overlayColor: Shade.text.withValues(alpha: 0.08),
      trackHeight: 2,
      showValueIndicator: ShowValueIndicator.never,
    ),
  );
}
