/// What the client actually knows, on screen.
library;

import 'package:flutter/widgets.dart';

import 'package:tachyx/src/rust/api/input.dart';
import 'package:tachyx/src/rust/api/session.dart'
    show Encoding, HostReport, Liveness, SessionStats, VideoStats;
import 'package:tachyx/src/rust/api/video.dart' show Latency, PresentStats;
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

/// Microseconds as milliseconds, which is the unit the budget is written in.
String describeMicros(int micros) => '${(micros / 1000).toStringAsFixed(2)} ms';

/// A latency distribution, as the two figures worth reading at a glance.
String describeLatency(Latency latency) =>
    '${describeMicros(latency.medianUs)} '
    '/ ${describeMicros(latency.p95Us)} p95';

/// What the link is doing, in the words the client means.
///
/// Never "0 fps". The host's capture blocks on a screen nothing is drawing to,
/// so no video is a still picture far more often than it is a fault.
String describeLiveness(Liveness liveness) => switch (liveness) {
  Liveness.streaming => 'streaming',
  Liveness.idle => 'idle — the screen is not changing',
  Liveness.lost => 'lost — no heartbeat',
};

/// A colour for each of those, so a lost host is visible without reading.
Color _tone(Liveness liveness) => switch (liveness) {
  Liveness.streaming => Shade.good,
  Liveness.idle => Shade.text,
  Liveness.lost => Shade.bad,
};

/// What the client knows, and nothing else.
///
/// Every row here was measured by this client, except the group labelled as the
/// host's own report. The stream group is absent rather than zeroed when there
/// is no session, because a panel built to measure latency that shows a made-up
/// number is worse than one that shows none.
class StatsOverlay extends StatelessWidget {
  /// A panel over the surface.
  const StatsOverlay({
    required this.host,
    required this.protocolVersion,
    required this.eventsSent,
    required this.lastEvent,
    required this.keyboardUp,
    this.stats,
    super.key,
  });

  /// Where the host is, or null when running with none.
  final String? host;

  /// The protocol version this build speaks.
  final int protocolVersion;

  /// Input events handed over since the client started.
  final int eventsSent;

  /// The most recent event, which is what shows the gesture machine is alive.
  final InputEvent? lastEvent;

  /// Whether the on-screen keyboard is up, read from the window's own insets.
  final bool keyboardUp;

  /// What the session has seen, or null when there is no session.
  final SessionStats? stats;

  @override
  Widget build(BuildContext context) {
    final SessionStats? seen = stats;
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
              _Row(
                'host',
                host ?? 'none',
                tone: host == null ? Shade.faint : null,
              ),
              _Row('protocol', 'v$protocolVersion'),
              _Row('keyboard', keyboardUp ? 'up' : 'down'),
              const _Gap(),
              _Row('events sent', '$eventsSent'),
              _Row('batches', seen == null ? '—' : '${seen.batches}'),
              _Row(
                'last',
                lastEvent == null ? 'nothing yet' : describeEvent(lastEvent!),
              ),
              const _Gap(),
              if (seen == null)
                const Text(
                  'no session, so nothing has been received.\n'
                  'frames, loss and latency appear here\n'
                  'once a host is connected.',
                  style: TextStyle(
                    color: Shade.faint,
                    fontSize: 11,
                    height: 1.5,
                  ),
                )
              else ...<Widget>[
                _Row(
                  'link',
                  describeLiveness(seen.liveness),
                  tone: _tone(seen.liveness),
                ),
                _Row('round trip', describeMicros(seen.rttUs)),
                _Row('frames', describeFrames(seen.video)),
                _Row('packets', describePackets(seen.video)),
                if (seen.present case final PresentStats present) ...<Widget>[
                  _Row('decode', describeLatency(present.decode)),
                  _Row('present', describeLatency(present.present)),
                  _Row('shown', '${present.presented} frames'),
                ],
                if (seen.encoding case final Encoding encoding) ...<Widget>[
                  const _Gap(),
                  _Row('stream', describeEncoding(encoding)),
                ],
                if (seen.host case final HostReport report) ...<Widget>[
                  _Row('host says', describeHost(report)),
                ],
              ],
              if (seen?.videoFault case final String fault) ...<Widget>[
                const _Gap(),
                Text(
                  fault,
                  style: const TextStyle(color: Shade.bad, fontSize: 11),
                ),
              ],
              if (seen?.ended case final String ended) ...<Widget>[
                const _Gap(),
                Text(
                  ended,
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

/// Every way a frame can end, on one line, because the ways are only
/// meaningful next to each other.
String describeFrames(VideoStats video) =>
    '${video.framesDelivered} in, ${video.framesDropped} dropped, '
    '${video.framesMissing} missing, ${video.framesUndecodable} undecodable';

/// The same for datagrams.
String describePackets(VideoStats video) =>
    '${video.packetsReceived} in, ${video.packetsLost} lost, '
    '${video.packetsLate} late';

/// What the host says about itself, marked as its word rather than ours.
String describeHost(HostReport report) =>
    '${report.framesSent} sent, ${report.framesGated} gated, '
    '${describeMicros(report.pipelineUs)} pipeline';

/// The codec, the geometry and the cost, on one line.
String describeEncoding(Encoding encoding) {
  final String rate = (encoding.bitrate / 1000000).toStringAsFixed(1);
  return '${encoding.codec} ${encoding.visibleWidth}x${encoding.visibleHeight} '
      '@${encoding.framerate} $rate Mbit/s';
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
