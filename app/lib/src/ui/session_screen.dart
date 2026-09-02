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
import 'package:tachyx/src/session/pip.dart';
import 'package:tachyx/src/session/pipe.dart';
import 'package:tachyx/src/session/present.dart';
import 'package:tachyx/src/session/viewport.dart';
import 'package:tachyx/src/settings.dart';
import 'package:tachyx/src/ui/stats_overlay.dart';
import 'package:tachyx/src/ui/theme.dart';
import 'package:tachyx/src/ui/video_surface.dart';
import 'package:tachyx/src/ui/video_texture_view.dart';

/// The remote desktop, and the touch surface that drives it.
///
/// The picture is the whole window, under the system bars and all. The only
/// thing the client keeps on the glass is a corner mark, which fades to a
/// ghost when it is left alone and opens the controls when it is tapped: the
/// glass is a trackpad, so a tap is a click wherever the pointer is and the
/// corner costs nothing to give up. The mark is the one way in; a
/// three-finger swipe up puts everything away, and the first session ever
/// says all of this once. See [Touchpad] for why that gesture and not an
/// edge swipe.
///
/// There is no [Scaffold] here on purpose. The Android template asks for
/// `adjustResize`, and a scaffold would take the on-screen keyboard's inset out
/// of the surface, shrinking the picture every time the user typed. The
/// keyboard is left to overlay instead.
class SessionScreen extends StatefulWidget {
  /// A session with the host at [address], or with none when [session] is null.
  const SessionScreen({
    required this.protocolVersion,
    required this.settings,
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

  /// The user's choices, read live: a cap or a sensitivity moved on the
  /// settings screen reaches a running session.
  final Settings settings;

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

  /// How long the open controls stay before they fade back to the corner
  /// mark. Long enough to read the address and reach a button, short enough
  /// that nobody has to dismiss them.
  static const Duration _linger = Duration(seconds: 5);

  /// The most a stream may cost while the window is a picture-in-picture
  /// thumbnail. A thumbnail a few hundred pixels wide cannot show what a
  /// full-rate stream costs, so the cap is dropped with the viewport and
  /// restored with it.
  static const int _pipBitrate = 2000000;

  final InputPipe _pipe = InputPipe();
  final Pip _pip = Pip.instance;
  late final CursorFeed _cursor;
  late final Touchpad _pad;
  late final HardwareKeys _keys;
  late final SoftKeyboard _soft;
  Timer? _sampler;
  Timer? _lingerTimer;
  bool _showStats = false;
  bool _expanded = true;

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
      hideControls: _dismiss,
      onDragLock: _buzz,
      sensitivity: widget.settings.sensitivity,
      pointerAccel: widget.settings.pointerAccel,
    );
    _keys = HardwareKeys(_pipe.send)..attach();
    WidgetsBinding.instance.addObserver(this);
    widget.settings.addListener(_settingsMoved);
    _pip.active.addListener(_pipMoved);
    _sampler = Timer.periodic(_sampleEvery, (_) => _sample());
    // A session opens with its controls showing. They say which host was
    // reached, they put the corner mark in front of the user once, and then
    // they get out of the way without being asked.
    _lingerTimer = Timer(_linger, _retire);
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    _rescale();
    // Also the first place the view is reachable, so the first arm is here
    // rather than in initState, and a media change re-arms with the rectangle
    // the picture has moved to.
    _armPip();
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
    // A rotation, a resize, or the window folding into a picture-in-picture
    // thumbnail. The host chose its rung from the viewport the hello named,
    // so it has to be told or it keeps encoding for a screen that is no
    // longer there.
    _renegotiate();
  }

  /// Tells the host what the window can take now, and what it may cost.
  ///
  /// The ceiling travels with every renegotiation because it moves for the
  /// same reasons the viewport does: a thumbnail is capped low, and a cap
  /// moved on the settings screen reaches the live session here.
  void _renegotiate() {
    final Session? session = widget.session;
    if (session == null || !mounted) return;
    final FlutterView view = View.of(context);
    final (int width, int height) = viewportOf(view);
    final int ceiling = _pip.active.value
        ? _pipBitrate
        : widget.settings.streamBitrate;
    session.viewportChanged(
      viewportWidth: width,
      viewportHeight: height,
      refreshHz: refreshHz(view),
      bitrateCeiling: ceiling,
    );
  }

  void _settingsMoved() {
    _pad.sensitivity = widget.settings.sensitivity;
    _pad.pointerAccel = widget.settings.pointerAccel;
    _renegotiate();
    // The build also reads settings: the onboarding hint stands on it.
    if (mounted) setState(() {});
  }

  void _pipMoved() {
    // The window's own resize renegotiates the viewport; this renegotiates
    // the ceiling, which no metrics change carries by itself.
    _renegotiate();
    if (mounted) setState(() {});
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
    _lingerTimer?.cancel();
    WidgetsBinding.instance.removeObserver(this);
    widget.settings.removeListener(_settingsMoved);
    _pip.active.removeListener(_pipMoved);
    unawaited(_pip.disarm());
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
    _armPip();
  }

  /// Keeps the fold armed with the picture's current shape and place.
  ///
  /// The thumbnail the system folds the window into should be the shape of
  /// the picture, not of the phone, and the fold should shrink from where the
  /// picture really is, which moves on every rotation and every change of
  /// aspect.
  void _armPip() {
    if (widget.session == null || !mounted) return;
    final Encoding? encoding = _encoding;
    final int aspectWidth = encoding?.aspectWidth ?? 16;
    final int aspectHeight = encoding?.aspectHeight ?? 9;
    unawaited(
      _pip.arm(
        aspectWidth: aspectWidth,
        aspectHeight: aspectHeight,
        source: _pictureRect(View.of(context), aspectWidth / aspectHeight),
      ),
    );
  }

  /// Where the picture sits in the window, in physical pixels.
  ///
  /// Computed rather than measured: the picture is drawn centred at its
  /// aspect ratio inside the whole window, so the letterboxed rectangle
  /// follows from the window and the aspect alone.
  static Rect _pictureRect(FlutterView view, double aspect) {
    final Size window = view.physicalSize;
    if (window.isEmpty) return Rect.zero;
    final Size fitted = window.width / window.height > aspect
        ? Size(window.height * aspect, window.height)
        : Size(window.width, window.width / aspect);
    return Offset(
          (window.width - fitted.width) / 2,
          (window.height - fitted.height) / 2,
        ) &
        fitted;
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

  /// Opens the controls, on their own countdown.
  void _reveal() {
    _lingerTimer?.cancel();
    _lingerTimer = Timer(_linger, _retire);
    setState(() => _expanded = true);
  }

  /// Takes everything the client draws back to the corner mark.
  void _dismiss() {
    _lingerTimer?.cancel();
    _lingerTimer = null;
    setState(() {
      _expanded = false;
      _showStats = false;
    });
  }

  /// The controls' own countdown, which leaves the panel where it is.
  void _retire() {
    if (!mounted) return;
    setState(() => _expanded = false);
  }

  /// Drops or restores the panel without losing the controls that asked.
  void _toggleStats() {
    _reveal();
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
    // A thumbnail shows the picture and nothing of the client's own: there is
    // no room, and nothing in it can be touched anyway.
    final bool thumbnail = _pip.active.value;
    return Material(
      color: Shade.ground,
      child: Stack(
        children: <Widget>[
          // The picture takes the window whole. Nothing above it is laid out
          // against it, so the viewport the hello named is still the area the
          // video occupies, and the host keeps encoding for the screen that is
          // really there.
          Positioned.fill(child: TouchSurface(pad: _pad, child: _picture())),
          // The panel keeps the top left corner and the controls the bottom
          // right, so the two never cover each other on a screen as short as
          // a phone held sideways.
          if (_showStats && !thumbnail)
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
          if (!thumbnail)
            Positioned(
              right: 0,
              // Above the on-screen keyboard rather than behind it, so that
              // the way out of the session is reachable while typing.
              bottom: media.viewInsets.bottom,
              child: SafeArea(
                top: false,
                left: false,
                child: _CornerDock(
                  expanded: _expanded,
                  address: widget.address,
                  showingStats: _showStats,
                  onOpen: _reveal,
                  onKeyboard: _toggleKeyboard,
                  onToggleStats: _toggleStats,
                  onLeave: widget.onLeave,
                ),
              ),
            ),
          if (!widget.settings.onboarded && !thumbnail)
            _Onboarding(onDone: widget.settings.markOnboarded),
        ],
      ),
    );
  }
}

/// The controls, folded into a corner mark whenever they are left alone.
///
/// The mark never leaves the glass entirely: it fades to a ghost that still
/// answers a tap, because a control that has to be summoned by a gesture
/// nobody can see is a control that does not exist. The glass is a trackpad,
/// so the corner it occupies costs the remote desktop nothing: a tap is a
/// click wherever the pointer is, and the pointer is never under the finger.
class _CornerDock extends StatelessWidget {
  const _CornerDock({
    required this.expanded,
    required this.address,
    required this.showingStats,
    required this.onOpen,
    required this.onKeyboard,
    required this.onToggleStats,
    required this.onLeave,
  });

  /// Long enough to read as a movement, short enough not to be waited on.
  static const Duration _fade = Duration(milliseconds: 200);

  final bool expanded;
  final String? address;
  final bool showingStats;
  final VoidCallback onOpen;
  final VoidCallback onKeyboard;
  final VoidCallback onToggleStats;
  final VoidCallback onLeave;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.all(10),
    child: AnimatedSwitcher(
      duration: _fade,
      switchInCurve: Curves.easeOut,
      switchOutCurve: Curves.easeIn,
      transitionBuilder: (Widget child, Animation<double> animation) =>
          FadeTransition(opacity: animation, child: child),
      layoutBuilder: (Widget? current, List<Widget> previous) => Stack(
        alignment: Alignment.bottomRight,
        children: <Widget>[...previous, ?current],
      ),
      child: expanded ? _controls() : _mark(),
    ),
  );

  /// The ghost in the corner: a bracket the shape of the corner it keeps.
  Widget _mark() => GestureDetector(
    key: const ValueKey<String>('mark'),
    onTap: onOpen,
    behavior: HitTestBehavior.opaque,
    child: AnimatedOpacity(
      opacity: 0.35,
      duration: _fade,
      child: const SizedBox(
        width: 44,
        height: 44,
        child: CustomPaint(painter: _CornerMark()),
      ),
    ),
  );

  Widget _controls() => DecoratedBox(
    key: const ValueKey<String>('controls'),
    decoration: BoxDecoration(
      color: Shade.panel,
      border: Border.all(color: Shade.edge),
      borderRadius: BorderRadius.circular(kCorner),
    ),
    child: SizedBox(
      height: 44,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          IconButton(
            onPressed: onLeave,
            icon: const Icon(Icons.arrow_back, size: 18),
            color: Shade.faint,
            tooltip: 'leave',
          ),
          Flexible(
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 2),
              child: Text(
                address ?? 'no host — input monitor',
                style: readout.copyWith(
                  color: address == null ? Shade.faint : Shade.text,
                ),
                overflow: TextOverflow.ellipsis,
              ),
            ),
          ),
          IconButton(
            onPressed: onKeyboard,
            icon: const Icon(Icons.keyboard_outlined, size: 18),
            color: Shade.faint,
            tooltip: 'keyboard',
          ),
          IconButton(
            onPressed: onToggleStats,
            icon: const Icon(Icons.monitor_heart_outlined, size: 18),
            color: showingStats ? Shade.good : Shade.faint,
            tooltip: 'statistics',
          ),
        ],
      ),
    ),
  );
}

/// The corner bracket the dock folds into.
class _CornerMark extends CustomPainter {
  const _CornerMark();

  @override
  void paint(Canvas canvas, Size size) {
    final Paint stroke = Paint()
      ..color = Shade.text
      ..strokeWidth = 1.5
      ..strokeCap = StrokeCap.square
      ..style = PaintingStyle.stroke;
    const double inset = 12;
    final double right = size.width - inset;
    final double bottom = size.height - inset;
    final Path bracket = Path()
      ..moveTo(right - 14, bottom)
      ..lineTo(right, bottom)
      ..lineTo(right, bottom - 14);
    canvas.drawPath(bracket, stroke);
  }

  @override
  bool shouldRepaint(_CornerMark oldDelegate) => false;
}

/// The one-time hint that says where the controls live.
///
/// Shown over the first session ever and never again. It has to exist because
/// the whole design hides the controls: a corner mark that fades and a
/// three-finger gesture are invisible precisely when someone is looking for
/// them the first time.
class _Onboarding extends StatelessWidget {
  const _Onboarding({required this.onDone});

  /// Marks the hint as seen, for good.
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) => Positioned.fill(
    child: GestureDetector(
      onTap: onDone,
      behavior: HitTestBehavior.opaque,
      child: ColoredBox(
        color: const Color(0xCC000000),
        child: Padding(
          padding: const EdgeInsets.all(32),
          child: Column(
            mainAxisAlignment: MainAxisAlignment.end,
            crossAxisAlignment: CrossAxisAlignment.end,
            children: <Widget>[
              const Text(
                'this screen is a trackpad',
                textAlign: TextAlign.right,
                style: TextStyle(fontSize: 16, color: Shade.text),
              ),
              const SizedBox(height: 16),
              Text(
                'tap to click, two fingers to scroll,\n'
                'hold then drag to grab.\n\n'
                'the mark in this corner opens the controls.\n'
                'three fingers: swipe up to clear the glass,\n'
                'tap for keyboard.',
                textAlign: TextAlign.right,
                style: readout.copyWith(color: Shade.faint, height: 1.8),
              ),
              const SizedBox(height: 24),
              OutlinedButton(
                onPressed: onDone,
                style: OutlinedButton.styleFrom(
                  foregroundColor: Shade.text,
                  side: const BorderSide(color: Shade.edgeLit),
                  shape: RoundedRectangleBorder(
                    borderRadius: BorderRadius.circular(kCorner),
                  ),
                ),
                child: const Text('got it'),
              ),
              const SizedBox(height: 40),
            ],
          ),
        ),
      ),
    ),
  );
}
