/// Reading a pairing code off the host's screen.
library;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

import 'package:tachyx/src/session/pairing.dart';
import 'package:tachyx/src/ui/theme.dart';

/// What a scanned pairing link starts with.
///
/// Lower case, and it stays lower case: Android matches an intent filter's
/// scheme case-sensitively, so the QR carries it in lower case and only the
/// base32 body after it is upper case.
const String pairingScheme = 'tachyx://pair?c=';

/// Digits in the typed secret.
const int _typedDigits = 8;

/// Whether [text] is shaped like something the host would have shown.
///
/// A hint and nothing more. The host decides whether a code is right, and it
/// gets one attempt to decide it, so this exists only to stop an obvious typing
/// mistake from burning that attempt.
bool looksLikeCode(String text) {
  final String trimmed = text.trim();
  if (trimmed.toLowerCase().startsWith(pairingScheme)) {
    return trimmed.length > pairingScheme.length;
  }
  final int dash = trimmed.indexOf('-');
  if (dash <= 0) return false;
  final String nameplate = trimmed.substring(0, dash);
  final String digits = trimmed
      .substring(dash + 1)
      .replaceAll('-', '')
      .replaceAll(' ', '');
  return int.tryParse(nameplate) != null &&
      digits.length == _typedDigits &&
      int.tryParse(digits) != null;
}

/// Whether this platform has a camera a QR can be read with.
///
/// Android and iOS only. No Flutter package scans a QR from a live camera on
/// Linux, so the desktop client types the code or pastes the link, and it says
/// so rather than showing a viewfinder that will never fill.
bool get platformHasCamera =>
    defaultTargetPlatform == TargetPlatform.android ||
    defaultTargetPlatform == TargetPlatform.iOS;

/// Where the user hands over the code the host is showing.
///
/// Two ways in, because the host shows two codes and they are not the same
/// secret. The scanned one carries the host's own identifier, so it is the one
/// to prefer and it is what opens first.
class PairScreen extends StatefulWidget {
  /// A screen that reports a code through [onCode] and reflects [state].
  const PairScreen({
    required this.state,
    required this.canScan,
    required this.onCode,
    required this.onDone,
    this.seed,
    super.key,
  });

  /// How far the pairing has got.
  final Pairing state;

  /// Whether to offer the camera. [platformHasCamera] is what the application
  /// passes; it is a parameter rather than a global so that a screen can be
  /// driven without a camera and without reaching for a debug override.
  final bool canScan;

  /// Called with the code the user gave, typed or scanned.
  final ValueChanged<String> onCode;

  /// Called when the user has read the outcome and is finished here.
  final VoidCallback onDone;

  /// A code the screen opens with, from a `tachyx://pair` link the system
  /// handed over. It is filled in rather than used: a link that arrived while
  /// the phone was in a pocket should not spend the code by itself.
  final String? seed;

  @override
  State<PairScreen> createState() => _PairScreenState();
}

class _PairScreenState extends State<PairScreen> {
  late final TextEditingController _typed = TextEditingController(
    text: widget.seed ?? '',
  );

  /// Whether the viewfinder is showing. Open by itself where there is a camera,
  /// and never when a link already carried the code.
  late bool _scanning = widget.canScan && widget.seed == null;

  /// Set the moment a code leaves this screen, so a camera that reads the same
  /// QR sixty times a second hands it over once.
  bool _given = false;

  @override
  void dispose() {
    _typed.dispose();
    super.dispose();
  }

  void _give(String code) {
    if (_given) return;
    _given = true;
    setState(() => _scanning = false);
    widget.onCode(code.trim());
  }

  void _detected(BarcodeCapture capture) {
    for (final Barcode barcode in capture.barcodes) {
      final String? value = barcode.rawValue;
      if (value != null && looksLikeCode(value)) {
        _give(value);
        return;
      }
    }
  }

  @override
  Widget build(BuildContext context) => Material(
    color: Shade.ground,
    child: SafeArea(
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 22, vertical: 18),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: <Widget>[
            Row(
              children: <Widget>[
                IconButton(
                  onPressed: widget.onDone,
                  icon: const Icon(Icons.close, color: Shade.faint, size: 20),
                ),
                const SizedBox(width: 4),
                const Text('pair with a host', style: label),
              ],
            ),
            const SizedBox(height: 18),
            Expanded(child: _body()),
          ],
        ),
      ),
    ),
  );

  Widget _body() => switch (widget.state) {
    Exchanging() => const _Working(),
    Paired(:final String host, :final String device, :final String words) =>
      _Succeeded(host: host, device: device, words: words, onDone: widget.onDone),
    PairingRefused(:final String reason) => _Failed(
      reason: reason,
      onAgain: () => setState(() {
        _given = false;
        _scanning = widget.canScan;
        _typed.clear();
      }),
    ),
    Unpaired() => _entry(),
  };

  Widget _entry() => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: <Widget>[
      const Text(
        'run `tachyx-hostd pair` on the machine you want to drive.',
        style: TextStyle(color: Shade.faint, fontSize: 13, height: 1.5),
      ),
      const SizedBox(height: 16),
      if (_scanning) ...<Widget>[
        Expanded(child: _viewfinder()),
        const SizedBox(height: 16),
        TextButton(
          onPressed: () => setState(() => _scanning = false),
          child: const Text(
            'type the code instead',
            style: TextStyle(color: Shade.faint, fontSize: 13),
          ),
        ),
      ] else ...<Widget>[
        _typedField(),
        if (widget.canScan)
          TextButton(
            onPressed: () => setState(() => _scanning = true),
            child: const Text(
              'scan the code instead',
              style: TextStyle(color: Shade.faint, fontSize: 13),
            ),
          ),
        const Spacer(),
      ],
    ],
  );

  Widget _viewfinder() => ClipRRect(
    borderRadius: BorderRadius.circular(kCorner),
    child: DecoratedBox(
      decoration: BoxDecoration(border: Border.all(color: Shade.edge)),
      child: MobileScanner(
        onDetect: _detected,
        errorBuilder:
            (BuildContext context, MobileScannerException error) => Center(
              child: Padding(
                padding: const EdgeInsets.all(24),
                child: Text(
                  'the camera is not available. type the code instead.',
                  textAlign: TextAlign.center,
                  style: const TextStyle(color: Shade.dim, fontSize: 13),
                ),
              ),
            ),
      ),
    ),
  );

  Widget _typedField() => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: <Widget>[
      TextField(
        controller: _typed,
        autofocus: !widget.canScan,
        autocorrect: false,
        enableSuggestions: false,
        style: readout.copyWith(fontSize: 16),
        inputFormatters: <TextInputFormatter>[LengthLimitingTextInputFormatter(256)],
        decoration: const InputDecoration(
          hintText: '7-4921-8306',
          hintStyle: TextStyle(color: Shade.dim),
          filled: true,
          fillColor: Shade.raised,
          border: OutlineInputBorder(borderSide: BorderSide(color: Shade.edge)),
        ),
        onChanged: (String _) => setState(() {}),
        onSubmitted: (String code) {
          if (looksLikeCode(code)) _give(code);
        },
      ),
      const SizedBox(height: 6),
      const Text(
        'the code, or a pairing link pasted whole.',
        style: TextStyle(color: Shade.dim, fontSize: 12),
      ),
      const SizedBox(height: 14),
      FilledButton(
        onPressed: looksLikeCode(_typed.text) ? () => _give(_typed.text) : null,
        child: const Text('pair'),
      ),
    ],
  );
}

class _Working extends StatelessWidget {
  const _Working();

  @override
  Widget build(BuildContext context) => const Center(
    child: Column(
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        SizedBox(
          width: 18,
          height: 18,
          child: CircularProgressIndicator(strokeWidth: 1.5, color: Shade.faint),
        ),
        SizedBox(height: 16),
        Text('exchanging keys', style: TextStyle(color: Shade.faint, fontSize: 13)),
      ],
    ),
  );
}

class _Succeeded extends StatelessWidget {
  const _Succeeded({
    required this.host,
    required this.device,
    required this.words,
    required this.onDone,
  });

  final String host;
  final String device;
  final String words;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) => Center(
    child: Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.center,
      children: <Widget>[
        const Icon(Icons.check, color: Shade.good, size: 28),
        const SizedBox(height: 14),
        Text(
          'paired with $host',
          style: const TextStyle(color: Shade.text, fontSize: 16),
        ),
        const SizedBox(height: 6),
        Text(device, style: readout.copyWith(color: Shade.faint)),
        const SizedBox(height: 22),
        Text(words, style: const TextStyle(fontSize: 30)),
        const SizedBox(height: 8),
        const SizedBox(
          width: 260,
          child: Text(
            'the host shows these three as well. they will match unless '
            'somebody was in the middle.',
            textAlign: TextAlign.center,
            style: TextStyle(color: Shade.dim, fontSize: 12, height: 1.5),
          ),
        ),
        const SizedBox(height: 24),
        FilledButton(onPressed: onDone, child: const Text('done')),
      ],
    ),
  );
}

class _Failed extends StatelessWidget {
  const _Failed({required this.reason, required this.onAgain});

  final String reason;
  final VoidCallback onAgain;

  @override
  Widget build(BuildContext context) => Center(
    child: Column(
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        const Icon(Icons.close, color: Shade.bad, size: 26),
        const SizedBox(height: 14),
        SizedBox(
          width: 280,
          child: Text(
            reason,
            textAlign: TextAlign.center,
            style: const TextStyle(color: Shade.text, fontSize: 14, height: 1.5),
          ),
        ),
        const SizedBox(height: 8),
        const SizedBox(
          width: 280,
          child: Text(
            'a code is good for one attempt. run `tachyx-hostd pair` again '
            'for a new one.',
            textAlign: TextAlign.center,
            style: TextStyle(color: Shade.dim, fontSize: 12, height: 1.5),
          ),
        ),
        const SizedBox(height: 24),
        FilledButton(onPressed: onAgain, child: const Text('try again')),
      ],
    ),
  );
}
