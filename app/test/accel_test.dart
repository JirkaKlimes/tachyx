import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/input/accel.dart';

/// Speeds from a resting finger to well past any flick, in logical pixels per
/// second, close enough together to catch a step in the curve.
Iterable<double> _sweep() sync* {
  for (double speed = 0; speed <= 4000; speed += 5) {
    yield speed;
  }
}

void main() {
  group('the acceleration curve', () {
    test('off is exactly flat, at every speed', () {
      for (final double speed in _sweep()) {
        expect(
          pointerGain(speed: speed, accel: 0),
          1.0,
          reason: '$speed px/s must be untouched with the setting off',
        );
      }
    });

    test('a resting or careful finger is never accelerated', () {
      for (double speed = 0; speed <= slowSpeed; speed += 5) {
        expect(pointerGain(speed: speed, accel: 1), 1.0);
      }
    });

    test('the gain never falls as the finger goes faster', () {
      for (final double accel in <double>[0.1, 0.3, 0.5, 1]) {
        double last = 0;
        for (final double speed in _sweep()) {
          final double gain = pointerGain(speed: speed, accel: accel);
          expect(gain, greaterThanOrEqualTo(last));
          last = gain;
        }
      }
    });

    test('the gain is capped, however hard the finger is flung', () {
      expect(pointerGain(speed: fastSpeed, accel: 1), closeTo(3, 1e-12));
      expect(pointerGain(speed: 100000, accel: 1), closeTo(3, 1e-12));
      for (final double speed in _sweep()) {
        expect(pointerGain(speed: speed, accel: 1), lessThanOrEqualTo(3));
        expect(pointerGain(speed: speed, accel: 1), greaterThanOrEqualTo(1));
      }
    });

    test('neighbouring speeds give all but the same gain', () {
      // A step of 1 logical pixel per second is finer than any digitiser
      // reports, so no jitter the hardware can produce reaches a gain the user
      // would feel as a jump.
      double last = pointerGain(speed: 0, accel: 1);
      for (double speed = 1; speed <= 4000; speed += 1) {
        final double gain = pointerGain(speed: speed, accel: 1);
        expect(
          (gain - last).abs(),
          lessThan(0.002),
          reason: 'a step at $speed px/s',
        );
        last = gain;
      }
    });

    test('the slope is zero at both knees, so neither one kinks', () {
      const double step = 1;
      double slopeAt(double speed) =>
          (pointerGain(speed: speed + step, accel: 1) -
              pointerGain(speed: speed - step, accel: 1)) /
          (2 * step);

      expect(slopeAt(slowSpeed).abs(), lessThan(1e-4));
      expect(slopeAt(fastSpeed).abs(), lessThan(1e-4));
      expect(
        slopeAt((slowSpeed + fastSpeed) / 2),
        greaterThan(1e-3),
        reason: 'flat ends, but it does rise in between',
      );
    });

    test('the setting scales the whole boost', () {
      final double full = pointerGain(speed: fastSpeed, accel: 1) - 1;
      expect(
        pointerGain(speed: fastSpeed, accel: 0.5) - 1,
        closeTo(full / 2, 1e-12),
      );
      expect(
        pointerGain(speed: fastSpeed, accel: 0.3) - 1,
        closeTo(full * 0.3, 1e-12),
      );
    });

    test('a setting outside its range is pulled back into it', () {
      expect(
        pointerGain(speed: 5000, accel: 9),
        pointerGain(speed: 5000, accel: 1),
      );
      expect(pointerGain(speed: 5000, accel: -1), 1.0);
    });
  });
}
