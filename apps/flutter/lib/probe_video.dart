/// A measuring harness for the Linux decode and present path.
///
/// It answers one question: does a decoded frame reach Flutter's rasteriser as
/// an OpenGL texture without a CPU copy, and how long does that take? A file of
/// H.264 access units stands in for the network, because the transport is not
/// what is under test here.
///
/// Run it with the clip and the pace given on the command line:
///
/// ```sh
/// flutter run -t lib/probe_video.dart \
///   --dart-define=CLIP=/path/to/clip.h264 \
///   --dart-define=FPS=60
/// ```
///
/// `FPS=0` floods: it hands over access units as fast as the decoder takes them,
/// which measures throughput rather than latency.
library;

import 'dart:async';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/widgets.dart';

import 'package:tachyx/src/rust/api/video.dart';
import 'package:tachyx/src/rust/frb_generated.dart';
import 'package:tachyx/src/ui/video_texture_view.dart';

const String _clipPath = String.fromEnvironment('CLIP');
const int _fps = int.fromEnvironment('FPS', defaultValue: 60);

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  runApp(const _Probe());
}

class _Probe extends StatefulWidget {
  const _Probe();

  @override
  State<_Probe> createState() => _ProbeState();
}

class _ProbeState extends State<_Probe> {
  VideoTextureController? _video;
  List<Uint8List> _units = const [];
  Timer? _feed;
  Timer? _report;
  final Stopwatch _clock = Stopwatch();
  int _sent = 0;
  String _error = '';
  PresentStats? _stats;

  @override
  void initState() {
    super.initState();
    unawaited(_start());
  }

  Future<void> _start() async {
    if (_clipPath.isEmpty) {
      setState(() => _error = 'pass --dart-define=CLIP=/path/to/clip.h264');
      return;
    }
    try {
      _units = _accessUnits(await File(_clipPath).readAsBytes());
      _video = await VideoTextureController.open();
    } on Object catch (failure) {
      setState(() => _error = '$failure');
      return;
    }
    debugPrint('probe: ${_units.length} access units from $_clipPath');
    _clock.start();
    // A steady tick with a catch-up loop rather than one push per tick: Dart's
    // timers wake late under load, and a pace that silently slowed would look
    // like a decoder that had sped up.
    _feed = Timer.periodic(const Duration(milliseconds: 2), (_) => _pump());
    _report = Timer.periodic(const Duration(seconds: 1), (_) => _tally());
    setState(() {});
  }

  void _pump() {
    final video = _video;
    if (video == null) {
      return;
    }
    final due = _fps == 0
        ? _sent + 1
        : (_clock.elapsedMicroseconds * _fps) ~/ Duration.microsecondsPerSecond;
    while (_sent < due) {
      video.push(_units[_sent % _units.length]);
      _sent++;
    }
  }

  void _tally() {
    final stats = _video?.stats;
    if (stats == null) {
      return;
    }
    final seconds = _clock.elapsedMicroseconds / Duration.microsecondsPerSecond;
    debugPrint(
      'probe: ${seconds.toStringAsFixed(1)}s '
      'pushed=${stats.pushed} decoded=${stats.decoded} '
      'presented=${stats.presented} superseded=${stats.superseded} '
      'decode_fps=${(stats.decoded.toInt() / seconds).toStringAsFixed(1)} '
      'present_fps=${(stats.presented.toInt() / seconds).toStringAsFixed(1)} '
      'decode_us=${_span(stats.decode)} present_us=${_span(stats.present)}',
    );
    setState(() => _stats = stats);
  }

  static String _span(Latency latency) =>
      '${latency.minUs}/${latency.medianUs}/${latency.p95Us}/${latency.maxUs}';

  @override
  void dispose() {
    _feed?.cancel();
    _report?.cancel();
    _video?.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final video = _video;
    return Directionality(
      textDirection: TextDirection.ltr,
      child: Stack(
        fit: StackFit.expand,
        children: [
          if (video != null)
            VideoTextureView(
              textureId: video.textureId,
              aspectRatio: 1920 / 1088,
            )
          else
            const ColoredBox(color: Color(0xFF101010)),
          Positioned(
            left: 12,
            top: 12,
            child: Text(
              _error.isNotEmpty ? _error : _summary(),
              style: const TextStyle(
                fontSize: 12,
                height: 1.5,
                color: Color(0xFF7CFF9B),
                decoration: TextDecoration.none,
              ),
            ),
          ),
        ],
      ),
    );
  }

  String _summary() {
    final stats = _stats;
    if (stats == null) {
      return 'starting';
    }
    return 'pushed ${stats.pushed}  decoded ${stats.decoded}  '
        'presented ${stats.presented}  superseded ${stats.superseded}\n'
        'decode  min/med/p95/max ${_span(stats.decode)} us\n'
        'present min/med/p95/max ${_span(stats.present)} us';
  }
}

/// Cuts an Annex B stream into access units.
///
/// A unit ends after a coded slice, so a key frame arrives with the parameter
/// sets that precede it in one push. That is the shape the network path will
/// deliver, and a stream pushed in any other shape would measure the wrong
/// thing.
///
/// One slice per picture is assumed, which is what the host encoder produces.
List<Uint8List> _accessUnits(Uint8List stream) {
  const slice = 1;
  const idrSlice = 5;
  final codes = <int>[];
  final headers = <int>[];
  var index = 0;
  while (index + 2 < stream.length) {
    if (stream[index] != 0 || stream[index + 1] != 0 || stream[index + 2] != 1) {
      index++;
      continue;
    }
    codes.add(index > 0 && stream[index - 1] == 0 ? index - 1 : index);
    headers.add(index + 3);
    index += 3;
  }
  if (codes.isEmpty) {
    return const [];
  }

  final units = <Uint8List>[];
  var unitStart = codes.first;
  for (var nal = 0; nal < codes.length; nal++) {
    if (headers[nal] >= stream.length) {
      break;
    }
    final type = stream[headers[nal]] & 0x1F;
    if (type != slice && type != idrSlice) {
      continue;
    }
    final end = nal + 1 < codes.length ? codes[nal + 1] : stream.length;
    units.add(Uint8List.sublistView(stream, unitStart, end));
    unitStart = end;
  }
  return units;
}
