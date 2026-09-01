/// Where the decoded picture is presented, when it cannot be.
library;

import 'package:flutter/widgets.dart';

import 'package:tachyx/src/ui/theme.dart';

/// The surface shown while there is no decoded frame to draw.
///
/// Deliberately empty, and deliberately still. An animated placeholder would be
/// indistinguishable from a stream that has stalled, which is precisely the
/// thing this client exists to tell apart. [reason] is what the decode path
/// said, in its own words, because a client showing no picture otherwise gives
/// no reason for it.
class VideoSurface extends StatelessWidget {
  /// An empty surface, explained by [reason] when there is one.
  const VideoSurface({this.reason, super.key});

  /// Why there is no picture, if anything has said.
  final String? reason;

  @override
  Widget build(BuildContext context) => ColoredBox(
    color: const Color(0xFF000000),
    child: Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Text(
          reason ?? 'no video yet',
          textAlign: TextAlign.center,
          style: const TextStyle(
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
