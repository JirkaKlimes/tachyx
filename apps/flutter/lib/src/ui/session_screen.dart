/// The screen the remote desktop is driven from.
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:tachyx/src/input/keyboard.dart';
import 'package:tachyx/src/input/touchpad.dart';
import 'package:tachyx/src/rust/api/session.dart';
import 'package:tachyx/src/session/pipe.dart';
import 'package:tachyx/src/ui/stats_overlay.dart';
import 'package:tachyx/src/ui/theme.dart';
import 'package:tachyx/src/ui/video_surface.dart';

/// The remote desktop, and the touch surface that drives it.
///
/// There is no [Scaffold] here on purpose. The Android template asks for
/// `adjustResize`, and a scaffold would take the on-screen keyboard's inset out
/// of the surface, shrinking the picture every time the user typed. The
/// keyboard is left to overlay instead.
class SessionScreen extends StatefulWidget {
  /// A session with the host at [address], or with none when [session] is null.
  const SessionScreen({
    required this.protocolVersion,
    required this.onLeave,
    this.address,
    this.session,
    super.key,
  });

  /// Where the host is, or null when running with none.
  final String? address;

  /// The open session, or null when running with none.
  final Session? session;

  /// The protocol version this build speaks.
  final int protocolVersion;

  /// Called when the user is done.
  final VoidCallback onLeave;

  @override
  State<SessionScreen> createState() => _SessionScreenState();
}

class _SessionScreenState extends State<SessionScreen>
    with WidgetsBindingObserver {
  /// How often the panel reads the counters. Often enough that the last event
  /// looks live, rarely enough that a panel of text is not redrawn per touch
  /// sample.
  static const Duration _sampleEvery = Duration(milliseconds: 100);

  final InputPipe _pipe = InputPipe();
  late final Touchpad _pad;
  late final HardwareKeys _keys;
  late final SoftKeyboard _soft;
  Timer? _sampler;
  bool _showStats = true;
  VideoStats? _video;

  @override
  void initState() {
    super.initState();
    final Session? session = widget.session;
    if (session != null) _pipe.attach(session);
    _soft = SoftKeyboard(_pipe.sendUnstamped);
    _pad = Touchpad(
      emit: _pipe.send,
      toggleKeyboard: _toggleKeyboard,
      onDragLock: _buzz,
    );
    _keys = HardwareKeys(_pipe.send)..attach();
    WidgetsBinding.instance.addObserver(this);
    _sampler = Timer.periodic(_sampleEvery, (_) => _sample());
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final MediaQueryData media = MediaQuery.of(context);
    // Until the host has said how it is encoding, the best the client knows is
    // what it asked for: a stream of its own viewport in physical pixels. So
    // one logical pixel is one device pixel ratio of image pixels. This becomes
    // the ratio of the stream configuration to the surface once a stream
    // configuration exists.
    _pad.scale = ImageScale.of(
      image: media.size * media.devicePixelRatio,
      box: media.size,
    );
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.resumed) return;
    // The client is no longer what the user is looking at. Anything held now
    // would stay held on the remote desktop until the client came back.
    final Duration at = _pipe.latest;
    _keys.releaseAll(at);
    _pad.releaseAll(at);
  }

  @override
  void dispose() {
    _sampler?.cancel();
    WidgetsBinding.instance.removeObserver(this);
    _keys.detach();
    _soft.hide();
    _pipe.detach();
    super.dispose();
  }

  void _sample() {
    if (!mounted) return;
    setState(() => _video = widget.session?.videoStats());
  }

  void _toggleKeyboard() {
    _soft.toggle();
    setState(() {});
  }

  void _buzz() => unawaited(
    HapticFeedback.selectionClick().catchError((Object _) {}),
  );

  @override
  Widget build(BuildContext context) {
    final bool keyboardUp = MediaQuery.viewInsetsOf(context).bottom > 0;
    return Material(
      color: Shade.ground,
      child: SafeArea(
        bottom: false,
        child: Column(
          children: <Widget>[
            _TopBar(
              address: widget.address,
              showingStats: _showStats,
              onToggleStats: () => setState(() => _showStats = !_showStats),
              onLeave: widget.onLeave,
            ),
            Expanded(
              child: Stack(
                children: <Widget>[
                  Positioned.fill(
                    child: TouchSurface(
                      pad: _pad,
                      child: const VideoSurface(),
                    ),
                  ),
                  if (_showStats)
                    Positioned(
                      left: 12,
                      top: 12,
                      child: IgnorePointer(
                        child: StatsOverlay(
                          host: widget.address,
                          protocolVersion: widget.protocolVersion,
                          eventsSent: _pipe.sent,
                          batchesSent: _pipe.isAttached ? _pipe.batches : null,
                          lastEvent: _pipe.last,
                          keyboardUp: keyboardUp,
                          video: _video,
                          fault: _pipe.fault,
                        ),
                      ),
                    ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _TopBar extends StatelessWidget {
  const _TopBar({
    required this.address,
    required this.showingStats,
    required this.onToggleStats,
    required this.onLeave,
  });

  final String? address;
  final bool showingStats;
  final VoidCallback onToggleStats;
  final VoidCallback onLeave;

  @override
  Widget build(BuildContext context) => DecoratedBox(
    decoration: const BoxDecoration(
      border: Border(bottom: BorderSide(color: Shade.edge)),
    ),
    child: SizedBox(
      height: 44,
      child: Row(
        children: <Widget>[
          IconButton(
            onPressed: onLeave,
            icon: const Icon(Icons.arrow_back, size: 18),
            color: Shade.faint,
            tooltip: 'leave',
          ),
          Expanded(
            child: Text(
              address ?? 'no host — input monitor',
              style: readout.copyWith(
                color: address == null ? Shade.faint : Shade.text,
              ),
              overflow: TextOverflow.ellipsis,
            ),
          ),
          IconButton(
            onPressed: onToggleStats,
            icon: const Icon(Icons.data_usage, size: 18),
            color: showingStats ? Shade.good : Shade.faint,
            tooltip: 'statistics',
          ),
        ],
      ),
    ),
  );
}
