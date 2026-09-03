import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/session/pairing.dart';
import 'package:tachyx/src/ui/pair_screen.dart';

void main() {
  // No camera throughout: the viewfinder needs one the test binding does not
  // have, and it is the typed path this exercises.
  Future<List<String>> pump(WidgetTester tester, Pairing state) async {
    final List<String> given = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: PairScreen(
          state: state,
          canScan: false,
          onCode: given.add,
          onDone: () {},
        ),
      ),
    );
    return given;
  }

  test('a code the host would have shown is recognised', () {
    expect(looksLikeCode('7-4921-8306'), isTrue);
    expect(looksLikeCode('  412-49218306 '), isTrue);
    expect(looksLikeCode('7-4921 8306'), isTrue);
    expect(looksLikeCode('${pairingScheme}AA7NI5IZ6JYD'), isTrue);
    expect(
      looksLikeCode('TACHYX://PAIR?C=AA7NI5IZ'),
      isTrue,
      reason: 'a scanner may hand back the scheme in the case it read it in',
    );
  });

  test('text that is not a code is not offered to the host', () {
    for (final String written in <String>[
      '',
      '7-4921-830',
      '7-4921-83067',
      '7-4921-830a',
      '49218306',
      'https://example.com/pair',
      pairingScheme,
    ]) {
      expect(looksLikeCode(written), isFalse, reason: written);
    }
  });

  testWidgets('a machine with no camera asks for the code to be typed', (
    WidgetTester tester,
  ) async {
    await pump(tester, const Unpaired());
    expect(find.text('7-4921-8306'), findsOneWidget, reason: 'the hint');
    expect(find.text('scan the code instead'), findsNothing);
  });

  testWidgets('the pair button waits until the code is shaped like one', (
    WidgetTester tester,
  ) async {
    final List<String> given = await pump(tester, const Unpaired());

    expect(tester.widget<FilledButton>(find.byType(FilledButton)).enabled, isFalse);
    await tester.enterText(find.byType(TextField), '7-4921-83');
    await tester.pump();
    expect(tester.widget<FilledButton>(find.byType(FilledButton)).enabled, isFalse);

    await tester.enterText(find.byType(TextField), '7-4921-8306');
    await tester.pump();
    await tester.tap(find.byType(FilledButton));
    expect(given, <String>['7-4921-8306']);
  });

  testWidgets('a code leaves this screen once, however many times it is given', (
    WidgetTester tester,
  ) async {
    final List<String> given = await pump(tester, const Unpaired());
    await tester.enterText(find.byType(TextField), '7-4921-8306');
    await tester.pump();
    await tester.tap(find.byType(FilledButton), warnIfMissed: false);
    await tester.tap(find.byType(FilledButton), warnIfMissed: false);
    expect(
      given,
      <String>['7-4921-8306'],
      reason: 'a code is good for one attempt and a double tap must not burn it',
    );
  });

  testWidgets('a finished pairing shows the words both ends show', (
    WidgetTester tester,
  ) async {
    await pump(tester, const Paired('desk', 'AB12-CD34-EF56', '🐝 🍎 ⚓'));
    expect(find.text('paired with desk'), findsOneWidget);
    expect(find.text('AB12-CD34-EF56'), findsOneWidget);
    expect(find.text('🐝 🍎 ⚓'), findsOneWidget);
  });

  testWidgets('a refusal is read in the words it came in, and can be retried', (
    WidgetTester tester,
  ) async {
    await pump(tester, const PairingRefused('the pairing peer did not have the code'));
    expect(
      find.text('the pairing peer did not have the code'),
      findsOneWidget,
      reason: 'the host said it, and rewording it would lose what it said',
    );
    await tester.tap(find.text('try again'));
    await tester.pump();
  });

  testWidgets('a running exchange says so and offers nothing to press', (
    WidgetTester tester,
  ) async {
    await pump(tester, const Exchanging());
    expect(find.text('exchanging keys'), findsOneWidget);
    expect(find.byType(TextField), findsNothing);
  });
}
