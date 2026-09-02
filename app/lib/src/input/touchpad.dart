/// Turning raw pointer events into remote input.
///
/// The touch surface is a trackpad, not a touch screen: the pointer does not
/// follow the finger, the finger pushes it. That is what makes the whole remote
/// screen reachable without the hand covering it.
library;

import 'dart:math' as math;

import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';

import 'package:tachyx/src/input/accel.dart';
import 'package:tachyx/src/rust/api/input.dart';

/// How a displacement on the glass maps to a displacement on the streamed
/// image.
///
/// Motion and scroll travel the wire in pixels of the image the host is
/// encoding. The host scales those by whatever ratio its capture bears to the
/// resolution it is currently encoding, so the pointer keeps the same feel when
/// congestion steps the resolution down, and a client never has to know the
/// host's own pixels.
@immutable
class ImageScale {
  /// A scale of this many image pixels per logical pixel on each axis.
  const ImageScale({required this.x, required this.y});

  /// The streamed image of [image] pixels drawn into a box of [box] logical
  /// pixels.
  factory ImageScale.of({required Size image, required Size box}) => ImageScale(
    x: box.width > 0 ? image.width / box.width : 1,
    y: box.height > 0 ? image.height / box.height : 1,
  );

  /// One image pixel per logical pixel, which is what a client has before the
  /// host has said how it is encoding.
  static const ImageScale identity = ImageScale(x: 1, y: 1);

  /// Image pixels per logical pixel, horizontally.
  final double x;

  /// Image pixels per logical pixel, vertically.
  final double y;

  /// [delta], in logical pixels, expressed in image pixels.
  Offset apply(Offset delta) => Offset(delta.dx * x, delta.dy * y);

  @override
  bool operator ==(Object other) =>
      other is ImageScale && other.x == x && other.y == y;

  @override
  int get hashCode => Object.hash(x, y);

  @override
  String toString() => 'ImageScale($x, $y)';
}

/// A finger, from the moment it landed.
class _Finger {
  _Finger(this.origin, this.downAt);

  /// Where it landed, in logical pixels.
  final Offset origin;

  /// The event clock when it landed.
  final Duration downAt;

  /// The furthest it has been from [origin]. A running maximum rather than the
  /// current distance, so a drag that returns to where it started is still a
  /// drag, and rather than a sum of steps, so that jitter during a long press
  /// cannot add up to a drag.
  double reach = 0;
}

/// How fast the finger on the glass is travelling, smoothed.
///
/// A single report is a poor speedometer: a touch digitiser quantises position,
/// so two samples a frame apart can differ by a pixel that was never there, and
/// feeding that straight to [pointerGain] would make the pointer jump while the
/// finger crawls. The estimate is a first-order low pass instead, so one bad
/// sample moves it by a fraction of itself.
class _FingerSpeed {
  /// How long the estimate takes to follow a change in speed, in seconds.
  ///
  /// 30 milliseconds is about two frames at 60 Hz: long enough that a single
  /// jittery sample moves the estimate less than half way, short enough that a
  /// flick is at full gain within the first 30 milliseconds of it.
  static const double _tau = 0.030;

  double _speed = 0;
  Duration _at = Duration.zero;

  /// Starts again from rest at [at], the event clock when the finger landed.
  ///
  /// A gesture that inherited the speed of the one before it would accelerate
  /// its own first millimetre for something the user has already finished.
  void reset(Duration at) {
    _speed = 0;
    _at = at;
  }

  /// Folds a step of [delta] logical pixels seen at [at] into the estimate, and
  /// reports the speed in logical pixels per second.
  ///
  /// The smoothing weight is derived from the interval rather than fixed, so a
  /// 120 Hz digitiser and a 60 Hz one settle in the same wall-clock time. It
  /// also makes a long gap behave: the weight approaches 1, and a finger that
  /// rested keeps nothing of the speed it had before the rest.
  double sample(Duration at, Offset delta) {
    final double dt =
        (at - _at).inMicroseconds / Duration.microsecondsPerSecond;
    _at = at;
    // Coalesced reports share a timestamp. A step that took no time says
    // nothing about speed, so the estimate stands rather than dividing by zero.
    if (dt <= 0) return _speed;
    final double weight = 1 - math.exp(-dt / _tau);
    _speed += weight * (delta.distance / dt - _speed);
    return _speed;
  }
}

/// Which pointer button each mouse button bit stands for.
const List<(int, PointerButton)> _mouseButtons = <(int, PointerButton)>[
  (kPrimaryMouseButton, PointerButton.left),
  (kSecondaryMouseButton, PointerButton.right),
  (kMiddleMouseButton, PointerButton.middle),
  (kBackMouseButton, PointerButton.back),
  (kForwardMouseButton, PointerButton.forward),
];

/// Turns raw pointer events into remote input.
///
/// Gestures are told apart by how many fingers touch, never by how many times.
/// That is what lets every one of them resolve the instant the fingers lift: a
/// tap already sent cannot be taken back, so telling a single tap from a double
/// tap would mean either waiting to find out whether a second one follows, or
/// preceding every right click with a left click the remote desktop acts on.
///
/// | Gesture | Effect |
/// |---|---|
/// | One-finger tap | Left click |
/// | Two-finger tap | Right click |
/// | Three-finger tap | Show or hide the on-screen keyboard |
/// | One-finger drag | Move the pointer |
/// | Two-finger drag | Scroll |
/// | Long press, then drag | Move the pointer with the left button held |
/// | Three-finger swipe up | Hide the client's own controls |
///
/// There are no timers. The largest number of fingers that were down at once is
/// latched on the way down, and the gesture is decided when the last one
/// leaves. The one row that needs elapsed time, the long press, reads it from
/// the event clock on the move that would start the drag, so the cost of
/// spending no timer is that the drag lock announces itself when the drag
/// starts rather than at the moment the press became long enough.
class Touchpad {
  /// A touchpad that sends what the user does to [emit] and asks [toggleKeyboard]
  /// for the on-screen keyboard.
  Touchpad({
    required this.emit,
    required this.toggleKeyboard,
    this.hideControls,
    this.onDragLock,
    this.scale = ImageScale.identity,
    this.sensitivity = 1,
    this.pointerAccel = 0,
    this.slop = kTouchSlop,
    this.holdDelay = const Duration(milliseconds: 500),
  });

  /// Where remote input goes, with the event clock reading at which the client
  /// saw the user act. Taken from the pointer event rather than from the moment
  /// this was called, so that batching cannot hide inside a latency
  /// measurement.
  final void Function(Duration at, InputEvent event) emit;

  /// Asked for by a three-finger tap.
  final VoidCallback toggleKeyboard;

  /// Asked for by a three-finger swipe up.
  ///
  /// There is no gesture the other way. The controls are opened by the corner
  /// mark, which can be seen; a gesture that opens something is invisible at
  /// the one moment somebody is looking for it.
  final VoidCallback? hideControls;

  /// Called when a long press turns into a held button, for haptic feedback.
  final VoidCallback? onDragLock;

  /// How the glass maps to the streamed image. Replaced when the host changes
  /// the resolution it encodes, or when the surface is resized.
  ImageScale scale;

  /// The user's own multiplier on pointer travel, from settings.
  ///
  /// Applied to motion and to nothing else: scrolling follows the content
  /// under the fingers and a click has no distance, so neither has a speed
  /// the user could mean by this.
  double sensitivity;

  /// How much a fast finger outruns a slow one, from settings. See
  /// [pointerGain].
  ///
  /// Fingers only. A real mouse already reports travel in the units its own
  /// driver decided on, after whatever curve the operating system applies, so a
  /// second curve here would be arguing with a decision the user made once for
  /// the whole desktop.
  double pointerAccel;

  /// How far a finger may travel and still count as a tap.
  final double slop;

  /// How long one finger must rest before a drag holds the left button.
  final Duration holdDelay;

  final Map<int, _Finger> _fingers = <int, _Finger>{};

  final _FingerSpeed _pace = _FingerSpeed();

  /// The most fingers that were down at once during this gesture.
  int _peak = 0;

  /// The furthest any of them has been from where it landed.
  double _reach = 0;

  bool _dragLocked = false;
  bool _scrolled = false;

  /// Whether this gesture has already been read as a three-finger swipe, so
  /// that a swipe asks once rather than on every move that follows it.
  bool _swiped = false;

  /// Mouse buttons currently held, as a [PointerEvent.buttons] mask.
  int _held = 0;

  /// A finger or a mouse button went down.
  void down(PointerDownEvent event) {
    if (event.kind == PointerDeviceKind.mouse) {
      _mouse(event.timeStamp, event.buttons);
      return;
    }
    // Every gesture begins with a finger landing, so this is the one place the
    // speed estimate has to start again from rest.
    _pace.reset(event.timeStamp);
    _fingers[event.pointer] = _Finger(event.position, event.timeStamp);
    if (_fingers.length > _peak) _peak = _fingers.length;
  }

  /// A finger or the mouse moved while something was down.
  void move(PointerMoveEvent event) {
    if (event.kind == PointerDeviceKind.mouse) {
      _mouse(event.timeStamp, event.buttons);
      _motion(event.timeStamp, event.delta);
      return;
    }
    final _Finger? finger = _fingers[event.pointer];
    if (finger == null) return;

    final bool stillPlaced = _reach <= slop;
    final double reach = (event.position - finger.origin).distance;
    if (reach > finger.reach) finger.reach = reach;
    if (reach > _reach) _reach = reach;

    if (_dragLocked) {
      _motion(event.timeStamp, event.delta, gain: _gain(event));
      return;
    }
    switch (_peak) {
      case 1:
        // The finger has rested long enough and has not yet gone anywhere, so
        // this move is the start of a drag with the button held rather than a
        // plain drag.
        if (stillPlaced && event.timeStamp - finger.downAt >= holdDelay) {
          _dragLocked = true;
          onDragLock?.call();
          _button(event.timeStamp, PointerButton.left, Press.down);
        }
        // No slop gate: motion is relative, so the few pixels of jitter before a
        // click cost nothing and waiting for them would cost latency.
        _motion(event.timeStamp, event.delta, gain: _gain(event));
      case 2:
        // Each finger contributes its share of the movement of the pair, so two
        // fingers moving together scroll by what one of them travelled.
        _scroll(event.timeStamp, event.delta / _fingers.length.toDouble());
      case 3:
        _swipe(event.position - finger.origin);
      default:
        // Four fingers or more say nothing until they lift.
        break;
    }
  }

  /// A finger or a mouse button came up.
  void up(PointerUpEvent event) {
    if (event.kind == PointerDeviceKind.mouse) {
      _mouse(event.timeStamp, 0);
      return;
    }
    if (_fingers.remove(event.pointer) == null) return;
    // The gesture is decided when the last finger leaves, not the first.
    if (_fingers.isNotEmpty) return;
    _resolve(event.timeStamp, tapped: _reach <= slop);
  }

  /// The system took the gesture away, so nothing was meant by it.
  void cancel(PointerCancelEvent event) {
    if (event.kind == PointerDeviceKind.mouse) {
      _mouse(event.timeStamp, 0);
      return;
    }
    if (_fingers.remove(event.pointer) == null) return;
    if (_fingers.isNotEmpty) return;
    _resolve(event.timeStamp, tapped: false);
  }

  /// The mouse moved with no button down.
  void hover(PointerHoverEvent event) => _motion(event.timeStamp, event.delta);

  /// A wheel turned.
  ///
  /// Passed through with its sign and without [scale]. Flutter reports a wheel
  /// the way the wire carries scrolling, positive downwards, and a detent is a
  /// notch of a wheel rather than a distance on the glass: a window half the
  /// size must not scroll half as far, and a step of the host's resolution
  /// ladder must not change it at all.
  void signal(PointerSignalEvent event) {
    if (event is! PointerScrollEvent) return;
    final Offset delta = event.scrollDelta;
    if (delta == Offset.zero) return;
    emit(
      event.timeStamp,
      InputEvent.wheel(dx: delta.dx, dy: delta.dy),
    );
  }

  /// Releases whatever this touchpad is still holding.
  ///
  /// Sent when the client stops being the thing the user is looking at, so that
  /// a button does not stay down on the remote desktop for as long as the
  /// application is in the background.
  void releaseAll(Duration at) {
    _fingers.clear();
    _resolve(at, tapped: false);
    _mouse(at, 0);
  }

  void _resolve(Duration at, {required bool tapped}) {
    if (_dragLocked) {
      _button(at, PointerButton.left, Press.up);
    } else {
      // Without this a flung surface on the host never settles, because its
      // kinetic scrolling has nothing to tell it the fingers have gone.
      if (_scrolled) emit(at, const InputEvent.fingerScrollEnd());
      if (tapped) {
        switch (_peak) {
          case 1:
            _click(at, PointerButton.left);
          case 2:
            _click(at, PointerButton.right);
          case 3:
            toggleKeyboard();
          default:
            break;
        }
      }
    }
    _peak = 0;
    _reach = 0;
    _dragLocked = false;
    _scrolled = false;
    _swiped = false;
  }

  /// Reads three fingers swiping up as a request to clear the glass.
  ///
  /// Three fingers that move are the one thing on this surface that meant
  /// nothing, so the client's own chrome can be hung on them without making a
  /// single row of the table above ambiguous, and without costing the remote
  /// desktop a pixel of the glass. It needs no timer either: [from] is where
  /// the finger has got to since it landed, so the swipe is decided by distance
  /// exactly as every other gesture here is.
  ///
  /// Only up. Opening is the corner mark's job, so a swipe down means
  /// nothing. A three-finger tap still toggles the keyboard, because a tap is
  /// a gesture that stayed inside [slop] and this one has left it.
  void _swipe(Offset from) {
    if (_swiped || from.dy >= -slop || from.dy.abs() <= from.dx.abs()) {
      return;
    }
    _swiped = true;
    hideControls?.call();
  }

  /// How much this finger has earned by how fast it is going.
  ///
  /// Called on every move a finger makes that reaches [_motion], because the
  /// estimate is only as good as the samples it was given: skipping the slow
  /// ones would leave it reading a speed the finger no longer has.
  double _gain(PointerMoveEvent event) => pointerGain(
    speed: _pace.sample(event.timeStamp, event.delta),
    accel: pointerAccel,
  );

  /// Sends [delta], the travel on the glass, as travel of the remote pointer.
  ///
  /// [gain] is acceleration and defaults to none, so a caller that has no
  /// finger speed to offer, which is every caller holding a mouse, gets the
  /// flat mapping without having to ask for it.
  void _motion(Duration at, Offset delta, {double gain = 1}) {
    final Offset moved = scale.apply(delta) * (sensitivity * gain);
    if (moved == Offset.zero) return;
    emit(at, InputEvent.pointerMotion(dx: moved.dx, dy: moved.dy));
  }

  /// Turns [delta], the travel of the fingers, into a scroll of the view.
  ///
  /// Negated, and this is the only place in the client that negates anything.
  /// The content follows the fingers, as it does on every touch surface made
  /// since the trackpad stopped being a mouse: fingers pushed down carry the
  /// content down, which is the view moving up towards the start of it. The
  /// wire carries the movement of the view. See [InputEvent].
  void _scroll(Duration at, Offset delta) {
    final Offset moved = scale.apply(-delta);
    if (moved == Offset.zero) return;
    _scrolled = true;
    emit(at, InputEvent.fingerScroll(dx: moved.dx, dy: moved.dy));
  }

  void _click(Duration at, PointerButton button) {
    _button(at, button, Press.down);
    _button(at, button, Press.up);
  }

  void _button(Duration at, PointerButton button, Press press) =>
      emit(at, InputEvent.pointerButton(button: button, press: press));

  /// Sends whichever mouse buttons changed between what was held and [buttons].
  void _mouse(Duration at, int buttons) {
    for (final (int mask, PointerButton button) in _mouseButtons) {
      final bool now = buttons & mask != 0;
      if (now == (_held & mask != 0)) continue;
      _button(at, button, now ? Press.down : Press.up);
    }
    _held = buttons;
  }
}

/// The whole window as one touch surface.
///
/// One [Listener] and no recognisers. Flutter's own gesture arena is not usable
/// here: asking it for a single and a double tap together makes it hold the
/// arena until the double-tap timer expires, a measured 310 milliseconds before
/// a single tap is reported and no single tap at all when a double tap happens.
class TouchSurface extends StatelessWidget {
  /// A surface that feeds [pad] and draws [child] under it.
  const TouchSurface({required this.pad, required this.child, super.key});

  /// The state machine the pointer events go to.
  final Touchpad pad;

  /// What the user is pointing at.
  final Widget child;

  @override
  Widget build(BuildContext context) => Listener(
    behavior: HitTestBehavior.opaque,
    onPointerDown: pad.down,
    onPointerMove: pad.move,
    onPointerUp: pad.up,
    onPointerCancel: pad.cancel,
    onPointerHover: pad.hover,
    onPointerSignal: pad.signal,
    child: child,
  );
}
