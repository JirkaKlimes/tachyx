/// What the client actually knows, on screen.
library;

import 'package:flutter/widgets.dart';

import 'package:tachyx/src/rust/api/input.dart';
import 'package:tachyx/src/rust/api/session.dart' show VideoStats;
import 'package:tachyx/src/ui/theme.dart';

/// One input event, short enough to sit on a line that changes every frame.
String describeEvent(InputEvent event) => switch (event) {
  InputEvent_PointerMotion(:final double dx, :final double dy) =>
    'motion ${_px(dx)} ${_px(dy)}',
  InputEvent_PointerMotionAbsolute(:final double x, :final double y) =>
    'move to ${x.toStringAsFixed(3)} ${y.toStringAsFixed(3)}',
  InputEvent_PointerButton(:final PointerButton button, :final Press press) =>
    '${button.name} ${press.name}',
  InputEvent_Wheel(:final double dx, :final double dy) =>
    'wheel ${_px(dx)} ${_px(dy)}',
  InputEvent_FingerScroll(:final double dx, :final double dy) =>
    'scroll ${_px(dx)} ${_px(dy)}',
  InputEvent_FingerScrollEnd() => 'scroll end',
  InputEvent_Key(:final HidUsage usage, :final Press press) =>
    'key 0x${usage.id.toRadixString(16).padLeft(2, '0')} ${press.name}',
  InputEvent_Text(:final String field0) => 'text ${_quote(field0)}',
};

String _px(double value) => value.toStringAsFixed(1).padLeft(7);

String _quote(String text) =>
    '"${text.length > 16 ? '${text.substring(0, 16)}…' : text}"';

/// What the client knows, and nothing else.
///
/// Every row here is measured by this client. The stream group is absent rather
/// than zeroed when there is no session, because a panel built to measure
/// latency that shows a made-up number is worse than one that shows none.
class StatsOverlay extends StatelessWidget {
  /// A panel over the surface.
  const StatsOverlay({
    required this.host,
    required this.protocolVersion,
    required this.eventsSent,
    required this.batchesSent,
    required this.lastEvent,
    required this.keyboardUp,
    this.video,
    this.fault,
    super.key,
  });

  /// Where the host is, or null when running with none.
  final String? host;

  /// The protocol version this build speaks.
  final int protocolVersion;

  /// Input events handed over since the client started.
  final int eventsSent;

  /// Batches ended, or null when nothing is on the other end to batch for.
  final int? batchesSent;

  /// The most recent event, which is what shows the gesture machine is alive.
  final InputEvent? lastEvent;

  /// Whether the on-screen keyboard is up, read from the window's own insets.
  final bool keyboardUp;

  /// What the video path has seen, or null when there is no session.
  final VideoStats? video;

  /// What the session last complained about.
  final String? fault;

  @override
  Widget build(BuildContext context) {
    final VideoStats? seen = video;
    return DecoratedBox(
      decoration: BoxDecoration(
        color: Shade.panel,
        border: Border.all(color: Shade.edge),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 10, 12, 10),
        child: DefaultTextStyle(
          style: readout,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: <Widget>[
              _Row('host', host ?? 'none', tone: host == null ? Shade.faint : null),
              _Row('protocol', 'v$protocolVersion'),
              _Row('keyboard', keyboardUp ? 'up' : 'down'),
              const _Gap(),
              _Row('events sent', '$eventsSent'),
              _Row('batches', batchesSent == null ? '—' : '$batchesSent'),
              _Row(
                'last',
                lastEvent == null ? 'nothing yet' : describeEvent(lastEvent!),
              ),
              const _Gap(),
              if (seen == null)
                const Text(
                  'no session, so nothing has been received.\n'
                  'frames, loss and round trip appear here\n'
                  'once the transport is connected.',
                  style: TextStyle(color: Shade.faint, fontSize: 11, height: 1.5),
                )
              else ...<Widget>[
                _Row('frames in', '${seen.framesDelivered}'),
                _Row('frames dropped', '${seen.framesDropped}'),
                _Row('frames undecodable', '${seen.framesUndecodable}'),
                _Row('packets in', '${seen.packetsReceived}'),
                _Row('packets lost', '${seen.packetsLost}'),
                _Row('packets late', '${seen.packetsLate}'),
              ],
              if (fault != null) ...<Widget>[
                const _Gap(),
                Text(
                  fault!,
                  style: const TextStyle(color: Shade.bad, fontSize: 11),
                ),
              ],
            ],
          ),
        ),
      ),
    );
  }
}

class _Row extends StatelessWidget {
  const _Row(this.label, this.value, {this.tone});

  final String label;
  final String value;
  final Color? tone;

  @override
  Widget build(BuildContext context) => Row(
    mainAxisSize: MainAxisSize.min,
    crossAxisAlignment: CrossAxisAlignment.start,
    children: <Widget>[
      SizedBox(
        width: 130,
        child: Text(label, style: const TextStyle(color: Shade.faint)),
      ),
      Text(value, style: TextStyle(color: tone ?? Shade.text)),
    ],
  );
}

class _Gap extends StatelessWidget {
  const _Gap();

  @override
  Widget build(BuildContext context) => const SizedBox(height: 8);
}
