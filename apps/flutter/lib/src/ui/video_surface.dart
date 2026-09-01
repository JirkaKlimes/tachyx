/// Where the decoded picture will be presented.
library;

import 'package:flutter/widgets.dart';

import 'package:tachyx/src/ui/theme.dart';

/// The surface a decoded frame will be drawn on.
///
/// Deliberately empty, and deliberately still. The present path is a GL texture
/// on Linux and a surface producer on Android, and neither is built. An
/// animated placeholder here would be indistinguishable from a stream that has
/// stalled, which is precisely the thing this client exists to tell apart.
class VideoSurface extends StatelessWidget {
  /// An empty surface.
  const VideoSurface({super.key});

  @override
  Widget build(BuildContext context) => const ColoredBox(
    color: Color(0xFF000000),
    child: Center(
      child: Padding(
        padding: EdgeInsets.all(24),
        child: Text(
          'no video\n\n'
          'this surface is empty on purpose.\n'
          'the decode and present path is not built yet.',
          textAlign: TextAlign.center,
          style: TextStyle(
            fontSize: 13,
            height: 1.7,
            color: Shade.faint,
            decoration: TextDecoration.none,
          ),
        ),
      ),
    ),
  );
}
