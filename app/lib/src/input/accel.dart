/// Letting the speed of the finger say how far the pointer goes.
///
/// The glass is a few hundred logical pixels wide and the desktop under the
/// pointer is several thousand image pixels wide. One flat multiplier cannot
/// serve both ends of that: set high enough to cross the remote screen in one
/// stroke, a checkbox takes three tries; set low enough to hit the checkbox,
/// the far corner takes four strokes. Acceleration reads how fast the finger is
/// going and lets the same glass mean both.
library;

/// The speed, in logical pixels per second, under which the pointer is never
/// accelerated.
///
/// A finger placing the pointer on a checkbox travels tens of pixels per
/// second, so this leaves the whole precise band alone: 200 logical pixels per
/// second crosses a 400-pixel-wide phone in two seconds, which is already a
/// deliberate move rather than a careful one.
const double slowSpeed = 200;

/// The speed at which acceleration has given everything it has.
///
/// 2000 logical pixels per second crosses the same phone in 200 milliseconds,
/// which is a flick. There is nothing above this the user could be asking for
/// that the cap does not already give.
const double fastSpeed = 2000;

/// The most a full acceleration setting adds to the gain.
///
/// The gain is capped at one plus this, so 3.0. A flick then covers three
/// times what it would flat, which reaches the far corner of a 4K desktop in
/// one stroke. Much more and the pointer overshoots the edge before the finger
/// has stopped.
const double maxBoost = 2.0;

/// The multiplier on pointer travel for a finger moving at [speed] logical
/// pixels per second, with acceleration set to [accel] between 0 and 1.
///
/// Never below 1. Sensitivity already says how fast the pointer is overall, and
/// a curve that shrank slow motion would make the two settings fight over the
/// same number. Acceleration only ever adds.
///
/// The rise between [slowSpeed] and [fastSpeed] is a smoothstep. Its slope is
/// zero at both thresholds, so a finger crossing either one sees neither a step
/// in gain nor a kink, and jitter around a threshold cannot buzz.
///
/// An [accel] of 0 returns exactly 1 at every speed, so the setting turns the
/// whole curve off rather than making it small.
double pointerGain({required double speed, required double accel}) {
  final double amount = accel.clamp(0.0, 1.0);
  if (amount == 0) return 1;
  final double t = ((speed - slowSpeed) / (fastSpeed - slowSpeed)).clamp(
    0.0,
    1.0,
  );
  return 1 + amount * maxBoost * t * t * (3 - 2 * t);
}
