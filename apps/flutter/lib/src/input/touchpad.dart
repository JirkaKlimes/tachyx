/// Turning raw pointer events into remote input.
///
/// The touch surface is a trackpad, not a touch screen: the pointer does not
/// follow the finger, the finger pushes it. That is what makes the whole remote
/// screen reachable without the hand covering it.
library;

import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';

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
    this.onDragLock,
    this.scale = ImageScale.identity,
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

  /// Called when a long press turns into a held button, for haptic feedback.
  final VoidCallback? onDragLock;

  /// How the glass maps to the streamed image. Replaced when the host changes
  /// the resolution it encodes, or when the surface is resized.
  ImageScale scale;

  /// How far a finger may travel and still count as a tap.
  final double slop;

  /// How long one finger must rest before a drag holds the left button.
  final Duration holdDelay;

  final Map<int, _Finger> _fingers = <int, _Finger>{};

  /// The most fingers that were down at once during this gesture.
  int _peak = 0;

  /// The furthest any of them has been from where it landed.
  double _reach = 0;

  bool _dragLocked = false;
  bool _scrolled = false;

  /// Mouse buttons currently held, as a [PointerEvent.buttons] mask.
  int _held = 0;

  /// Whether a long press is currently holding the left button down.
  bool get isDragLocked => _dragLocked;

  /// A finger or a mouse button went down.
  void down(PointerDownEvent event) {
    if (event.kind == PointerDeviceKind.mouse) {
      _mouse(event.timeStamp, event.buttons);
      return;
    }
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
      _motion(event.timeStamp, event.delta);
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
        _motion(event.timeStamp, event.delta);
      case 2:
        // Each finger contributes its share of the movement of the pair, so two
        // fingers moving together scroll by what one of them travelled.
        _scroll(event.timeStamp, event.delta / _fingers.length.toDouble());
      default:
        // Three fingers or more say nothing until they lift.
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
  void signal(PointerSignalEvent event) {
    if (event is! PointerScrollEvent) return;
    final Offset delta = scale.apply(event.scrollDelta);
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
  }

  void _motion(Duration at, Offset delta) {
    final Offset moved = scale.apply(delta);
    if (moved == Offset.zero) return;
    emit(at, InputEvent.pointerMotion(dx: moved.dx, dy: moved.dy));
  }

  void _scroll(Duration at, Offset delta) {
    final Offset moved = scale.apply(delta);
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
