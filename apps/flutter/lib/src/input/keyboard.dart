/// Keys, from the hardware keyboard and from the one on the glass.
library;

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import 'package:tachyx/src/rust/api/input.dart';

/// The USB HID usage page the host injects keys from.
///
/// Checked before a usage is sent: Android reports a value outside any HID page
/// when an input method intercepts a key, and forwarding that would inject a
/// key nobody pressed.
const int keyboardPage = 0x0007;

/// The HID usage of the return key.
const int _returnUsage = 0x28;

/// The HID usage of the backspace key.
const int _backspaceUsage = 0x2A;

/// Forwards a real keyboard to the host.
///
/// Keys are read through [HardwareKeyboard], which runs before any text field
/// can consume them. Returning true from the handler also stops Android from
/// redispatching the key, which is what keeps the back gesture from leaving the
/// session and keeps tab and the arrows from moving focus instead of reaching
/// the remote desktop.
class HardwareKeys {
  /// A forwarder that sends what it reads to [emit].
  HardwareKeys(this.emit);

  /// Where key events go, with the event clock reading at which the client saw
  /// the key change state.
  final void Function(Duration at, InputEvent event) emit;

  /// HID usages currently held, so that they can all be released at once.
  final Set<int> _held = <int>{};

  /// Starts reading keys.
  void attach() => HardwareKeyboard.instance.addHandler(handle);

  /// Stops reading keys, releasing anything still held.
  void detach() {
    HardwareKeyboard.instance.removeHandler(handle);
    releaseAll(Duration.zero);
  }

  /// Takes one key event, reporting whether the host has taken it.
  ///
  /// Public because it is the whole of this class's behaviour and a test that
  /// has to install a global handler to reach it is testing Flutter rather than
  /// this.
  bool handle(KeyEvent event) {
    // The host repeats a held key itself, so forwarding the platform's repeat
    // would double it. Still claimed, so that the platform does not act on it.
    if (event is KeyRepeatEvent) return true;

    final int packed = event.physicalKey.usbHidUsage;
    final int page = packed >> 16;
    if (page != keyboardPage) return false;
    final int id = packed & 0xFFFF;

    final Press press = event is KeyUpEvent ? Press.up : Press.down;
    if (press == Press.down) {
      _held.add(id);
    } else {
      _held.remove(id);
    }
    emit(
      event.timeStamp,
      InputEvent.key(
        usage: HidUsage(page: keyboardPage, id: id),
        press: press,
      ),
    );
    return true;
  }

  /// Releases every key the host still believes is held.
  ///
  /// Sent when the application stops being what the user is looking at.
  /// Otherwise a key held at the moment of a switch stays down on the remote
  /// desktop until the client comes back.
  void releaseAll(Duration at) {
    for (final int id in _held) {
      emit(
        at,
        InputEvent.key(
          usage: HidUsage(page: keyboardPage, id: id),
          press: Press.up,
        ),
      );
    }
    _held.clear();
  }
}

/// The on-screen keyboard, as a text input client with no text field.
///
/// A hidden [EditableText] would give the same characters, but only after the
/// widget tree had already applied them: composition in a language that needs
/// it would arrive as a stream of half-formed guesses rather than one committed
/// string. Edit deltas say directly what the input method decided.
///
/// The field is kept empty. Everything committed is sent and cleared, so the
/// next backspace has nothing local to delete and is forwarded as the key it
/// really is.
class SoftKeyboard implements DeltaTextInputClient {
  /// A keyboard that sends what the user types to [emit].
  SoftKeyboard(this.emit);

  /// Where text and the two keys that are not text go.
  final void Function(InputEvent event) emit;

  /// What the input method believes the field holds.
  ///
  /// Only ever the composition in flight: once a commit lands it is sent and
  /// this returns to empty.
  TextEditingValue _value = TextEditingValue.empty;

  TextInputConnection? _connection;

  /// Whether the keyboard has been asked for.
  bool get isOpen => _connection?.attached ?? false;

  /// Asks for the keyboard.
  void show() {
    final TextInputConnection connection =
        _connection ?? TextInput.attach(this, _configuration);
    _connection = connection;
    _value = TextEditingValue.empty;
    connection
      ..setEditingState(_value)
      ..show();
  }

  /// Puts the keyboard away.
  void hide() {
    _connection?.close();
    _connection = null;
    _value = TextEditingValue.empty;
  }

  /// Shows the keyboard if it is away, and puts it away if it is showing.
  void toggle() => isOpen ? hide() : show();

  @override
  void updateEditingValueWithDeltas(List<TextEditingDelta> textEditingDeltas) {
    for (final TextEditingDelta delta in textEditingDeltas) {
      // Nothing left to delete locally, so the user meant the key.
      if (delta is TextEditingDeltaDeletion &&
          _value.text.isEmpty &&
          !_value.composing.isValid) {
        _key(_backspaceUsage);
        continue;
      }
      _value = delta.apply(_value);
    }
    _flush();
  }

  /// Sends whatever the input method has finished with.
  void _flush() {
    // A valid composing range means the input method is still deciding.
    if (_value.composing.isValid) return;
    final String text = _value.text;
    if (text.isEmpty) return;

    // Newlines have a real HID usage, so they go through the key path and pass
    // through the host's own layout untouched. Text is insertion only.
    final List<String> lines = text.split('\n');
    for (int i = 0; i < lines.length; i++) {
      if (i > 0) _key(_returnUsage);
      if (lines[i].isNotEmpty) emit(InputEvent.text(lines[i]));
    }

    _value = TextEditingValue.empty;
    _connection?.setEditingState(_value);
  }

  void _key(int usage) {
    emit(
      InputEvent.key(
        usage: HidUsage(page: keyboardPage, id: usage),
        press: Press.down,
      ),
    );
    emit(
      InputEvent.key(
        usage: HidUsage(page: keyboardPage, id: usage),
        press: Press.up,
      ),
    );
  }

  static const TextInputConfiguration _configuration = TextInputConfiguration(
    // Anything but multiline turns the return key into an action the client
    // never sees. With multiline it arrives as a newline in the deltas.
    inputType: TextInputType.multiline,
    inputAction: TextInputAction.newline,
    enableDeltaModel: true,
    autocorrect: false,
    enableSuggestions: false,
    // The user is typing into someone else's machine, possibly a password.
    // Their input method should not learn from it.
    enableIMEPersonalizedLearning: false,
  );

  @override
  TextEditingValue? get currentTextEditingValue => _value;

  @override
  AutofillScope? get currentAutofillScope => null;

  @override
  void updateEditingValue(TextEditingValue value) {
    // Never called: the connection asked for the delta model.
  }

  @override
  void performAction(TextInputAction action) {
    // The configuration asks for a newline, which arrives as a delta instead.
  }

  @override
  void performPrivateCommand(String action, Map<String, dynamic> data) {}

  @override
  void updateFloatingCursor(RawFloatingCursorPoint point) {}

  @override
  void showAutocorrectionPromptRect(int start, int end) {}

  @override
  bool onFocusReceived() => isOpen;

  @override
  void connectionClosed() {
    _connection = null;
    _value = TextEditingValue.empty;
  }

  @override
  void insertTextPlaceholder(Size size) {}

  @override
  void removeTextPlaceholder() {}

  @override
  void showToolbar() {}

  @override
  void performSelector(String selectorName) {}

  @override
  void insertContent(KeyboardInsertedContent content) {}

  @override
  void didChangeInputControl(
    TextInputControl? oldControl,
    TextInputControl? newControl,
  ) {}
}
