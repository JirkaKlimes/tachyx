/// The screen the remote desktop is driven from.
library;

import 'dart:async';

import 'dart:ui' show FlutterView;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:tachyx/src/input/keyboard.dart';
import 'package:tachyx/src/input/touchpad.dart';
import 'package:tachyx/src/rust/api/session.dart';
import 'package:tachyx/src/session/cursor.dart';
import 'package:tachyx/src/session/pipe.dart';
import 'package:tachyx/src/session/present.dart';
import 'package:tachyx/src/session/viewport.dart';
import 'package:tachyx/src/ui/stats_overlay.dart';
import 'package:tachyx/src/ui/theme.dart';
import 'package:tachyx/src/ui/video_surface.dart';
import 'package:tachyx/src/ui/video_texture_view.dart';

/// The remote desktop, and the touch surface that drives it.
///
/// The picture is the whole window, under the system bars and all. Everything
/// the client draws of its own is an overlay that arrives when it is asked for:
/// a bar that says where the session is and offers a way out of it, and the
/// statistics panel. A three-finger swipe down brings both back, a three-finger
/// swipe up puts both away, and the bar retires by itself. See [Touchpad] for
/// why that gesture and not an edge swipe.
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
    with WidgetsBindingObserver, SingleTickerProviderStateMixin {
  /// How often the panel reads the counters. Often enough that the last event
  /// looks live, rarely enough that a panel of text is not redrawn per touch
  /// sample.
  static const Duration _sampleEvery = Duration(milliseconds: 100);

  /// The shape a picture is drawn at before the host has said what it is.
  static const double _unknownAspect = 16 / 9;

  /// How long the bar stays before it retires on its own. Long enough to read
  /// the address and reach a button, short enough that nobody has to dismiss
  /// it. The statistics panel is not on this clock: it is read for as long as
  /// the reader wants it, and there is a gesture that puts it away.
  static const Duration _barLingers = Duration(seconds: 5);

  final InputPipe _pipe = InputPipe();
  late final CursorFeed _cursor;
  late final Touchpad _pad;
  late final HardwareKeys _keys;
  late final SoftKeyboard _soft;
  Timer? _sampler;
  Timer? _linger;
  bool _showStats = true;
  bool _showBar = true;

  /// The counters, held apart from the element tree.
  ///
  /// They are read ten times a second and the panel is the only thing that
  /// wants them. Kept as state they would lay the picture, the touch surface
  /// and the pointer out again on every sample, for text nobody may even be
  /// looking at.
  final ValueNotifier<SessionStats?> _stats = ValueNotifier<SessionStats?>(
    null,
  );

  /// What the host says it is coding, which is the one part of a sample the
  /// picture itself is built from. It moves when the host changes rung, not on
  /// the sampling clock, so it stays state and rebuilds when it really changes.
  Encoding? _encoding;

  Presentation? _presentation;
  bool _presenting = false;
  Encoding? _sized;
  String? _videoFault;

  @override
  void initState() {
    super.initState();
    final Session? session = widget.session;
    if (session != null) {
      _pipe.attach(session);
    }
    _cursor = CursorFeed(session: session, vsync: this);
    _soft = SoftKeyboard(_pipe.sendUnstamped);
    _pad = Touchpad(
      emit: _pipe.send,
      toggleKeyboard: _toggleKeyboard,
      showControls: _reveal,
      hideControls: _dismiss,
      onDragLock: _buzz,
    );
    _keys = HardwareKeys(_pipe.send)..attach();
    WidgetsBinding.instance.addObserver(this);
    _sampler = Timer.periodic(_sampleEvery, (_) => _sample());
    // A session opens with its own chrome showing. It says which host was
    // reached, it puts the gesture that brings it back in front of the user
    // once, and then it gets out of the way without being asked.
    _linger = Timer(_barLingers, _retire);
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    _rescale();
    // Not in `initState`: opening a presentation needs the size of the view,
    // and the view is only reachable once the widget has its dependencies.
    final Session? session = widget.session;
    if (session != null && !_presenting) {
      _presenting = true;
      unawaited(_present(session));
    }
  }

  @override
  void didChangeMetrics() {
    // A rotation or a resize. The host chose its rung from the viewport the
    // hello named, so it has to be told the viewport changed or it keeps
    // encoding for a screen that is no longer there.
    final Session? session = widget.session;
    if (session == null || !mounted) return;
    final FlutterView view = View.of(context);
    final (int width, int height) = viewportOf(view);
    session.viewportChanged(
      viewportWidth: width,
      viewportHeight: height,
      refreshHz: refreshHz(view),
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
    _linger?.cancel();
    WidgetsBinding.instance.removeObserver(this);
    _keys.detach();
    _soft.hide();
    _cursor.dispose();
    _stats.dispose();
    _pipe.detach();
    unawaited(_presentation?.close() ?? Future<void>.value());
    super.dispose();
  }

  /// Points the session's decoder at something this screen draws.
  ///
  /// Neither half of it can be done while connecting: the engine handle arrives
  /// asynchronously, and the registration has to happen on the thread that
  /// asked for it.
  Future<void> _present(Session session) async {
    try {
      // The size the host says it is coding, when it has said, so that the
      // first picture lands in a surface already the right shape. It says so on
      // the control stream, which does not wait for a decoder to exist, so this
      // is usually already known. The client's own surface is the fallback, and
      // the next sample corrects it.
      final Encoding? declared = session.stats().encoding;
      _sized = declared;
      final (int width, int height) = declared == null
          ? viewportOf(View.of(context))
          : (declared.codedWidth, declared.codedHeight);
      final Presentation opened = await Presentation.open(
        session,
        width: width,
        height: height,
      );
      if (!mounted) {
        unawaited(opened.close());
        return;
      }
      setState(() => _presentation = opened);
    } on Object catch (refused) {
      if (mounted) setState(() => _videoFault = '$refused');
    }
  }

  void _sample() {
    if (!mounted) return;
    final SessionStats? stats = widget.session?.stats();
    _stats.value = stats;
    final Encoding? encoding = stats?.encoding;
    if (encoding == _encoding) return;
    setState(() => _encoding = encoding);
    _rescale();
    _resize();
  }

  /// Keeps the presentation the size the decoder is writing.
  ///
  /// The host says so before the first picture and again whenever it changes
  /// rung, and the surface the decoder draws into has to be built for the same
  /// size or the compositor reads a buffer that is not the shape it thinks.
  void _resize() {
    final Encoding? encoding = _encoding;
    final Presentation? presentation = _presentation;
    if (encoding == null || presentation == null) return;
    if (_sized?.codedWidth == encoding.codedWidth &&
        _sized?.codedHeight == encoding.codedHeight) {
      return;
    }
    _sized = encoding;
    unawaited(presentation.resize(encoding));
  }

  /// Puts pointer deltas into the units the host injects them in.
  ///
  /// Until the host has said how it is encoding, the best the client knows is
  /// what it asked for: a stream of its own viewport in physical pixels. After
  /// that it is the ratio of the visible picture to the box it is drawn in.
  void _rescale() {
    final MediaQueryData media = MediaQuery.of(context);
    final Encoding? encoding = _encoding;
    final Size image = encoding == null
        ? media.size * media.devicePixelRatio
        : Size(
            encoding.visibleWidth.toDouble(),
            encoding.visibleHeight.toDouble(),
          );
    _pad.scale = ImageScale.of(image: image, box: media.size);
  }

  void _toggleKeyboard() {
    _soft.toggle();
    setState(() {});
  }

  /// Brings the client's own chrome back, statistics and all.
  void _reveal() {
    _linger?.cancel();
    _linger = Timer(_barLingers, _retire);
    setState(() {
      _showBar = true;
      _showStats = true;
    });
  }

  /// Takes everything the client draws off the picture.
  void _dismiss() {
    _linger?.cancel();
    _linger = null;
    setState(() {
      _showBar = false;
      _showStats = false;
    });
  }

  /// The bar's own countdown, which leaves the panel where it is.
  void _retire() {
    if (!mounted) return;
    setState(() => _showBar = false);
  }

  /// Drops or restores the panel without losing the bar that asked for it.
  void _toggleStats() {
    _linger?.cancel();
    _linger = Timer(_barLingers, _retire);
    setState(() => _showStats = !_showStats);
  }

  void _buzz() =>
      unawaited(HapticFeedback.selectionClick().catchError((Object _) {}));

  /// The picture, or the reason there is none.
  Widget _picture() {
    final Presentation? presentation = _presentation;
    if (presentation == null) {
      return VideoSurface(reason: _videoFault ?? _noSessionReason());
    }
    final Encoding? encoding = _encoding;
    return VideoTextureView(
      textureId: presentation.textureId,
      aspectRatio: encoding == null
          ? _unknownAspect
          : encoding.aspectWidth / encoding.aspectHeight,
      crop: presentation.crop(encoding),
      cursor: _cursor.cursor,
    );
  }

  String? _noSessionReason() => widget.session == null
      ? 'no host — this surface reports input and nothing else'
      : null;

  @override
  Widget build(BuildContext context) {
    final MediaQueryData media = MediaQuery.of(context);
    final bool keyboardUp = media.viewInsets.bottom > 0;
    return Material(
      color: Shade.ground,
      child: Stack(
        children: <Widget>[
          // The picture takes the window whole. Nothing above it is laid out
          // against it, so the viewport the hello named is still the area the
          // video occupies, and the host keeps encoding for the screen that is
          // really there.
          Positioned.fill(child: TouchSurface(pad: _pad, child: _picture())),
          // The panel keeps the top left corner and the bar takes the bottom
          // right, so that the two never cover each other on a screen as short
          // as a phone held sideways.
          if (_showStats)
            Positioned(
              left: 0,
              top: 0,
              child: SafeArea(
                bottom: false,
                child: Padding(
                  padding: const EdgeInsets.fromLTRB(12, 12, 12, 0),
                  child: IgnorePointer(
                    child: ValueListenableBuilder<SessionStats?>(
                      valueListenable: _stats,
                      builder:
                          (
                            BuildContext context,
                            SessionStats? stats,
                            Widget? _,
                          ) => StatsOverlay(
                            host: widget.address,
                            protocolVersion: widget.protocolVersion,
                            eventsSent: _pipe.sent,
                            lastEvent: _pipe.last,
                            keyboardUp: keyboardUp,
                            stats: stats,
                          ),
                    ),
                  ),
                ),
              ),
            ),
          Positioned(
            left: 0,
            right: 0,
            // Above the on-screen keyboard rather than behind it, so that the
            // way out of the session is reachable while the user is typing.
            bottom: media.viewInsets.bottom,
            child: SafeArea(
              top: false,
              child: _ControlBar(
                showing: _showBar,
                address: widget.address,
                showingStats: _showStats,
                onToggleStats: _toggleStats,
                onLeave: widget.onLeave,
              ),
            ),
          ),
        ],
      ),
    );
  }
}

/// The controls, which are on the glass only while they are wanted.
///
/// Built whether or not it is [showing], so that it can slide off the bottom
/// edge rather than blink out, and made deaf to touch while it is away so that
/// none of the surface under it stops being a trackpad.
class _ControlBar extends StatelessWidget {
  const _ControlBar({
    required this.showing,
    required this.address,
    required this.showingStats,
    required this.onToggleStats,
    required this.onLeave,
  });

  /// Long enough to read as a movement, short enough not to be waited on.
  static const Duration _slide = Duration(milliseconds: 160);

  final bool showing;
  final String? address;
  final bool showingStats;
  final VoidCallback onToggleStats;
  final VoidCallback onLeave;

  @override
  Widget build(BuildContext context) => IgnorePointer(
    ignoring: !showing,
    child: AnimatedSlide(
      offset: showing ? Offset.zero : const Offset(0, 1),
      duration: _slide,
      curve: Curves.easeOut,
      child: AnimatedOpacity(
        opacity: showing ? 1 : 0,
        duration: _slide,
        child: Align(
          alignment: Alignment.bottomRight,
          child: Padding(
            padding: const EdgeInsets.all(8),
            child: DecoratedBox(
              decoration: BoxDecoration(
                color: Shade.panel,
                border: Border.all(color: Shade.edge),
                borderRadius: BorderRadius.circular(6),
              ),
              child: SizedBox(height: 44, child: _row()),
            ),
          ),
        ),
      ),
    ),
  );

  Widget _row() => Row(
    mainAxisSize: MainAxisSize.min,
    children: <Widget>[
      IconButton(
        onPressed: onLeave,
        icon: const Icon(Icons.arrow_back, size: 18),
        color: Shade.faint,
        tooltip: 'leave',
      ),
      Flexible(
        child: Text(
          address ?? 'no host — input monitor',
          style: readout.copyWith(
            color: address == null ? Shade.faint : Shade.text,
          ),
          overflow: TextOverflow.ellipsis,
        ),
      ),
      // The only place the gesture is written down. The bar shows itself once
      // when a session opens, which is the one moment it can say how to ask
      // for it again.
      const Padding(
        padding: EdgeInsets.symmetric(horizontal: 10),
        child: Text(
          'three fingers ↕',
          style: TextStyle(color: Shade.faint, fontSize: 11),
        ),
      ),
      IconButton(
        onPressed: onToggleStats,
        icon: const Icon(Icons.data_usage, size: 18),
        color: showingStats ? Shade.good : Shade.faint,
        tooltip: 'statistics',
      ),
    ],
  );
}
