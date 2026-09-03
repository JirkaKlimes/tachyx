import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/input/keyboard.dart';
import 'package:tachyx/src/rust/api/input.dart';

/// A physical key on the keyboard page: the letter A.
const PhysicalKeyboardKey _keyA = PhysicalKeyboardKey(0x00070004);

/// The volume keys, which are on the consumer page rather than the keyboard's.
const PhysicalKeyboardKey _volumeUp = PhysicalKeyboardKey(0x000c00e9);
const PhysicalKeyboardKey _volumeDown = PhysicalKeyboardKey(0x000c00ea);

/// A consumer key that is none of this client's business.
const PhysicalKeyboardKey _playPause = PhysicalKeyboardKey(0x000c00cd);

KeyDownEvent _down(PhysicalKeyboardKey key, LogicalKeyboardKey logical) =>
    KeyDownEvent(
      physicalKey: key,
      logicalKey: logical,
      timeStamp: Duration.zero,
    );

InputEvent _key(int usage, Press press) => InputEvent.key(
  usage: HidUsage(page: keyboardPage, id: usage),
  press: press,
);

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
    late List<int> steps;
    late HardwareKeys keys;

    /// Whether the session is playing the host's sound, which is the one thing
    /// that decides whether the volume keys belong to the host or to the phone.
    late bool listening;

    setUp(() {
      seen = <InputEvent>[];
      steps = <int>[];
      listening = false;
      keys = HardwareKeys(
        (Duration at, InputEvent event) => seen.add(event),
        stepVolume: (int step) {
          if (!listening) return false;
          steps.add(step);
          return true;
        },
      );
    });

    test('a key on the keyboard page is forwarded and claimed', () {
      final bool claimed = keys.handle(
        const KeyDownEvent(
          physicalKey: _keyA,
          logicalKey: LogicalKeyboardKey.keyA,
          timeStamp: Duration.zero,
        ),
      );

      expect(
        claimed,
        isTrue,
        reason: 'claiming it stops Android redispatching',
      );
      expect(seen, <InputEvent>[_key(0x04, Press.down)]);
    });

    test('a usage outside the keyboard page is not sent anywhere', () {
      final bool claimed = keys.handle(
        _down(_playPause, LogicalKeyboardKey.mediaPlayPause),
      );

      expect(claimed, isFalse, reason: 'the platform still owns it');
      expect(seen, isEmpty);
    });

    test('a volume key is left to the phone while nothing is playing', () {
      final bool claimed = keys.handle(
        _down(_volumeUp, LogicalKeyboardKey.audioVolumeUp),
      );

      expect(
        claimed,
        isFalse,
        reason: 'a silent client must not take the phone\'s own volume keys',
      );
      expect(steps, isEmpty);
      expect(
        seen,
        isEmpty,
        reason: 'the host is told about keyboard keys only',
      );
    });

    test('a volume key moves the host while it is playing', () {
      listening = true;

      final bool claimed = keys.handle(
        _down(_volumeUp, LogicalKeyboardKey.audioVolumeUp),
      );

      expect(
        claimed,
        isTrue,
        reason: 'claiming it stops the phone moving its own volume as well',
      );
      expect(steps, <int>[50]);
      expect(seen, isEmpty);
    });

    test('holding a volume key keeps moving the host', () {
      listening = true;
      keys
        ..handle(_down(_volumeDown, LogicalKeyboardKey.audioVolumeDown))
        ..handle(
          const KeyRepeatEvent(
            physicalKey: _volumeDown,
            logicalKey: LogicalKeyboardKey.audioVolumeDown,
            timeStamp: Duration(milliseconds: 500),
          ),
        );

      expect(steps, <int>[
        -50,
        -50,
      ], reason: 'nothing at the far end repeats a key it never saw');
    });

    test('a volume key the platform kept is released to the platform', () {
      const KeyUpEvent up = KeyUpEvent(
        physicalKey: _volumeUp,
        logicalKey: LogicalKeyboardKey.audioVolumeUp,
        timeStamp: Duration(milliseconds: 40),
      );

      expect(keys.handle(up), isFalse, reason: 'the press was not taken');

      listening = true;
      keys.handle(_down(_volumeUp, LogicalKeyboardKey.audioVolumeUp));

      expect(keys.handle(up), isTrue, reason: 'this press was');
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

      expect(seen, <InputEvent>[_key(0x2A, Press.down), _key(0x2A, Press.up)]);
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
