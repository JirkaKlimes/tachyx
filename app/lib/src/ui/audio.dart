/// The controls for the host's sound: where it plays, and how loud.
library;

import 'dart:async';

import 'package:flutter/material.dart';

import 'package:tachyx/src/rust/api/audio.dart';
import 'package:tachyx/src/ui/theme.dart';

/// The toolbar's one control for sound.
///
/// It cycles rather than opening a menu. There are three states, the icon says
/// which one is in force, and a menu for three states costs more taps than the
/// states are worth.
///
/// What it shows is what the host says it is really doing, not what it was last
/// asked for: a host may refuse. [asked] is the one exception, and it stands
/// only for the moment between the tap and the answer, so that a second tap
/// follows the first instead of repeating it.
class AudioButton extends StatelessWidget {
  /// A control over the sound [audio] describes.
  const AudioButton({
    required this.audio,
    required this.onMode,
    this.asked,
    super.key,
  });

  /// What the last sample said about sound, or null before there was one.
  final AudioStats? audio;

  /// A mode asked for and not yet confirmed, shown while it stands.
  final AudioMode? asked;

  /// Asks the host for a mode.
  final ValueChanged<AudioMode> onMode;

  @override
  Widget build(BuildContext context) {
    final AudioStats? said = audio;
    // A host that cannot stream its sound is said to be unable to, rather than
    // offered a control that does nothing: a control that is offered and then
    // does nothing cannot be told from a fault.
    if (said == null || !said.hostHasAudio) {
      return const IconButton(
        onPressed: null,
        icon: Icon(Icons.volume_off_outlined, size: 18),
        disabledColor: Shade.dim,
        tooltip: 'audio: unavailable',
      );
    }
    final AudioMode mode = asked ?? said.mode;
    return IconButton(
      onPressed: () => onMode(_next(mode)),
      icon: Icon(_icon(mode), size: 18),
      color: mode == AudioMode.off ? Shade.faint : Shade.good,
      tooltip: _describe(mode),
    );
  }
}

/// The mode a tap asks for next.
AudioMode _next(AudioMode mode) => switch (mode) {
  AudioMode.off => AudioMode.exclusive,
  AudioMode.exclusive => AudioMode.shared,
  AudioMode.shared => AudioMode.off,
};

/// The icon for a mode: silence, sound in this user's ears alone, or sound in
/// both rooms.
IconData _icon(AudioMode mode) => switch (mode) {
  AudioMode.off => Icons.volume_off_outlined,
  AudioMode.exclusive => Icons.headphones_outlined,
  AudioMode.shared => Icons.volume_up_outlined,
};

/// What the toolbar says the session is doing about sound.
///
/// Said as where the sound comes out rather than in the protocol's own words:
/// 'here' and 'both' are what a listener hears, where 'exclusive' and 'shared'
/// describe what the host does with its own speakers.
String _describe(AudioMode mode) => switch (mode) {
  AudioMode.off => 'audio: off',
  AudioMode.exclusive => 'audio: here',
  AudioMode.shared => 'audio: both',
};

/// The host's volume, shown while it is being changed and not otherwise.
///
/// A column against the right edge, clear of the corner dock. It is not a
/// permanent control: it comes up when [nudged] ticks, stays while it is being
/// worked, and leaves. The picture is the point of the screen, and a volume
/// slider parked over it is in the way.
///
/// This is the **host's** sink, and the only volume the session has. The
/// capture tap sits ahead of it, so the same knob is what the host plays at and
/// what this client hears.
class VolumeOverlay extends StatefulWidget {
  /// A column over the volume [audio] describes.
  const VolumeOverlay({
    required this.audio,
    required this.nudged,
    required this.onVolume,
    required this.onMuted,
    this.asked,
    super.key,
  });

  /// What the last sample said about sound, or null before there was one.
  final AudioStats? audio;

  /// A volume asked for and not yet confirmed, shown while it stands.
  final int? asked;

  /// Ticks whenever something outside this column moved the volume, which is a
  /// volume key. What the value is means nothing; the change is the message,
  /// and how long the column then stays is this column's own business.
  final Listenable nudged;

  /// Asks the host for a sink volume, in permille.
  final ValueChanged<int> onVolume;

  /// Asks the host to mute or unmute its own sink.
  final ValueChanged<bool> onMuted;

  @override
  State<VolumeOverlay> createState() => _VolumeOverlayState();
}

class _VolumeOverlayState extends State<VolumeOverlay> {
  /// How long the column stays after the last thing that moved it. Long enough
  /// to see where the volume landed, short enough that nobody has to dismiss
  /// it.
  static const Duration _linger = Duration(milliseconds: 1500);

  /// Long enough to read as a movement, short enough not to be waited on.
  static const Duration _fade = Duration(milliseconds: 200);

  /// The full range of the host's volume, in permille.
  static const double _full = 1000;

  Timer? _timer;
  bool _shown = false;

  @override
  void initState() {
    super.initState();
    widget.nudged.addListener(_touched);
  }

  @override
  void didUpdateWidget(VolumeOverlay old) {
    super.didUpdateWidget(old);
    if (old.nudged != widget.nudged) {
      old.nudged.removeListener(_touched);
      widget.nudged.addListener(_touched);
    }
  }

  @override
  void dispose() {
    widget.nudged.removeListener(_touched);
    _timer?.cancel();
    super.dispose();
  }

  /// Shows the column, on a fresh countdown.
  void _touched() {
    _timer?.cancel();
    _timer = Timer(_linger, _retire);
    if (!_shown) setState(() => _shown = true);
  }

  void _retire() {
    if (mounted) setState(() => _shown = false);
  }

  void _moved(double permille) {
    widget.onVolume(permille.round());
    _touched();
  }

  void _mute(bool muted) {
    widget.onMuted(muted);
    _touched();
  }

  @override
  Widget build(BuildContext context) {
    final AudioStats? said = widget.audio;
    final int volume = (widget.asked ?? said?.volume ?? 0)
        .clamp(0, _full)
        .toInt();
    return AnimatedSwitcher(
      duration: _fade,
      switchInCurve: Curves.easeOut,
      switchOutCurve: Curves.easeIn,
      transitionBuilder: (Widget child, Animation<double> animation) =>
          FadeTransition(opacity: animation, child: child),
      child: _shown
          ? _column(volume, said?.muted ?? false)
          : const SizedBox.shrink(),
    );
  }

  Widget _column(int volume, bool muted) => DecoratedBox(
    decoration: BoxDecoration(
      color: Shade.panel,
      border: Border.all(color: Shade.edge),
      borderRadius: BorderRadius.circular(kCorner),
    ),
    child: Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          Text(
            '${(volume / 10).round()}%',
            style: readout.copyWith(color: Shade.faint),
          ),
          // Turned rather than drawn again: this is the same instrument the
          // settings screen uses, so it reads the same and follows the same
          // theme. A quarter turn anticlockwise puts the loud end at the top,
          // and carries the touches round with it.
          SizedBox(
            width: 44,
            height: 160,
            child: RotatedBox(
              quarterTurns: 3,
              child: Slider(
                value: volume.toDouble(),
                max: _full,
                onChanged: _moved,
              ),
            ),
          ),
          IconButton(
            onPressed: () => _mute(!muted),
            icon: Icon(
              muted ? Icons.volume_off_outlined : Icons.volume_up_outlined,
              size: 18,
            ),
            color: muted ? Shade.bad : Shade.faint,
            tooltip: muted ? 'host muted' : 'mute host',
          ),
        ],
      ),
    ),
  );
}
