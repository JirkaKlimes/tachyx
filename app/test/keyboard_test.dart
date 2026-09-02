import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/input/keyboard.dart';
import 'package:tachyx/src/rust/api/input.dart';

/// A physical key on the keyboard page: the letter A.
const PhysicalKeyboardKey _keyA = PhysicalKeyboardKey(0x00070004);

/// A physical key that is not on the keyboard page. Android reports something
/// like this when an input method takes a key for itself.
const PhysicalKeyboardKey _volumeUp = PhysicalKeyboardKey(0x000c00e9);

InputEvent _key(int usage, Press press) =>
    InputEvent.key(usage: HidUsage(page: keyboardPage, id: usage), press: press);

TextEditingDeltaInsertion _insert(
  String into,
  String text, {
  TextRange composing = TextRange.empty,
}) => TextEditingDeltaInsertion(
  oldText: into,
  textInserted: text,
  insertionOffset: into.length,
  selection: TextSelection.collapsed(offset: into.length + text.length),
  composing: composing,
);

void main() {
  group('a real keyboard', () {
    late List<InputEvent> seen;
    late HardwareKeys keys;

    setUp(() {
      seen = <InputEvent>[];
      keys = HardwareKeys((Duration at, InputEvent event) => seen.add(event));
    });

    test('a key on the keyboard page is forwarded and claimed', () {
      final bool claimed = keys.handle(
        const KeyDownEvent(
          physicalKey: _keyA,
          logicalKey: LogicalKeyboardKey.keyA,
          timeStamp: Duration.zero,
        ),
      );

      expect(claimed, isTrue, reason: 'claiming it stops Android redispatching');
      expect(seen, <InputEvent>[_key(0x04, Press.down)]);
    });

    test('a usage outside the keyboard page is not sent anywhere', () {
      final bool claimed = keys.handle(
        const KeyDownEvent(
          physicalKey: _volumeUp,
          logicalKey: LogicalKeyboardKey.audioVolumeUp,
          timeStamp: Duration.zero,
        ),
      );

      expect(claimed, isFalse, reason: 'the platform still owns it');
      expect(seen, isEmpty);
    });

    test('a repeat is claimed but not sent, because the host repeats', () {
      keys
        ..handle(
          const KeyDownEvent(
            physicalKey: _keyA,
            logicalKey: LogicalKeyboardKey.keyA,
            timeStamp: Duration.zero,
          ),
        )
        ..handle(
          const KeyRepeatEvent(
            physicalKey: _keyA,
            logicalKey: LogicalKeyboardKey.keyA,
            timeStamp: Duration(milliseconds: 500),
          ),
        );

      expect(seen, <InputEvent>[_key(0x04, Press.down)]);
    });

    test('everything held is released at once', () {
      keys
        ..handle(
          const KeyDownEvent(
            physicalKey: _keyA,
            logicalKey: LogicalKeyboardKey.keyA,
            timeStamp: Duration.zero,
          ),
        )
        ..releaseAll(const Duration(milliseconds: 10))
        ..releaseAll(const Duration(milliseconds: 20));

      expect(seen, <InputEvent>[
        _key(0x04, Press.down),
        _key(0x04, Press.up),
      ], reason: 'a key is released once, not once per sweep');
    });
  });

  group('the on-screen keyboard', () {
    late List<InputEvent> seen;
    late SoftKeyboard soft;

    setUp(() {
      seen = <InputEvent>[];
      soft = SoftKeyboard(seen.add);
    });

    test('a plain insertion is sent as text', () {
      soft.updateEditingValueWithDeltas(<TextEditingDelta>[_insert('', 'hi')]);

      expect(seen, <InputEvent>[const InputEvent.text('hi')]);
    });

    test('a composition is sent once, when it is committed', () {
      soft.updateEditingValueWithDeltas(<TextEditingDelta>[
        _insert('', 'n', composing: const TextRange(start: 0, end: 1)),
      ]);
      expect(seen, isEmpty, reason: 'the input method is still deciding');

      soft.updateEditingValueWithDeltas(<TextEditingDelta>[
        const TextEditingDeltaReplacement(
          oldText: 'n',
          replacementText: '你',
          replacedRange: TextRange(start: 0, end: 1),
          selection: TextSelection.collapsed(offset: 1),
          composing: TextRange.empty,
        ),
      ]);

      expect(seen, <InputEvent>[const InputEvent.text('你')]);
    });

    test('a newline goes through the key path, not the text path', () {
      soft.updateEditingValueWithDeltas(<TextEditingDelta>[
        _insert('', 'ab\ncd'),
      ]);

      expect(seen, <InputEvent>[
        const InputEvent.text('ab'),
        _key(0x28, Press.down),
        _key(0x28, Press.up),
        const InputEvent.text('cd'),
      ]);
    });

    test('backspace with nothing to delete goes through the key path', () {
      soft.updateEditingValueWithDeltas(<TextEditingDelta>[
        const TextEditingDeltaDeletion(
          oldText: '',
          deletedRange: TextRange(start: 0, end: 0),
          selection: TextSelection.collapsed(offset: 0),
          composing: TextRange.empty,
        ),
      ]);

      expect(seen, <InputEvent>[
        _key(0x2A, Press.down),
        _key(0x2A, Press.up),
      ]);
    });

    test('the field is emptied after a commit, so the next key is a key', () {
      soft
        ..updateEditingValueWithDeltas(<TextEditingDelta>[_insert('', 'a')])
        ..updateEditingValueWithDeltas(<TextEditingDelta>[
          const TextEditingDeltaDeletion(
            oldText: 'a',
            deletedRange: TextRange(start: 0, end: 1),
            selection: TextSelection.collapsed(offset: 0),
            composing: TextRange.empty,
          ),
        ]);

      expect(seen, <InputEvent>[
        const InputEvent.text('a'),
        _key(0x2A, Press.down),
        _key(0x2A, Press.up),
      ]);
    });
  });
}
