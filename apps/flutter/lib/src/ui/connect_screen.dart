/// Choosing a host.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/ui/theme.dart';

/// Where the user says which machine to drive.
class ConnectScreen extends StatefulWidget {
  /// A screen that dials through [onDial], starting from [remembered].
  const ConnectScreen({
    required this.state,
    required this.protocolVersion,
    required this.remembered,
    required this.onDial,
    required this.onDetach,
    super.key,
  });

  /// How far the client has got.
  final Link state;

  /// The protocol version this build speaks.
  final int protocolVersion;

  /// The address the last run dialled, or empty.
  final String remembered;

  /// Called with a host and a port.
  final void Function(String host, int port) onDial;

  /// Called when the user wants the input surface with no host behind it.
  final VoidCallback onDetach;

  @override
  State<ConnectScreen> createState() => _ConnectScreenState();
}

class _ConnectScreenState extends State<ConnectScreen> {
  late final TextEditingController _host;
  late final TextEditingController _port;

  @override
  void initState() {
    super.initState();
    final (String host, int port) = _split(widget.remembered);
    _host = TextEditingController(text: host);
    _port = TextEditingController(text: '$port');
  }

  @override
  void didUpdateWidget(ConnectScreen old) {
    super.didUpdateWidget(old);
    // The remembered address arrives from storage after the first frame.
    if (widget.remembered != old.remembered && _host.text.isEmpty) {
      final (String host, int port) = _split(widget.remembered);
      _host.text = host;
      _port.text = '$port';
    }
  }

  @override
  void dispose() {
    _host.dispose();
    _port.dispose();
    super.dispose();
  }

  /// An address as it is stored, split back into the two fields it is edited
  /// in. An address with no port takes the default one.
  static (String, int) _split(String address) {
    final int colon = address.lastIndexOf(':');
    if (colon < 0) {
      return (address, HostLink.defaultPort);
    }
    return (
      address.substring(0, colon),
      int.tryParse(address.substring(colon + 1)) ?? HostLink.defaultPort,
    );
  }

  bool get _dialling => widget.state is Dialling;

  void _dial() {
    final String host = _host.text.trim();
    if (host.isEmpty) return;
    widget.onDial(host, int.tryParse(_port.text) ?? HostLink.defaultPort);
  }

  @override
  Widget build(BuildContext context) => Material(
    color: Shade.ground,
    child: SafeArea(
      child: Center(
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(28),
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 360),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: <Widget>[
                const Text(
                  'tachyx',
                  style: TextStyle(
                    fontSize: 30,
                    letterSpacing: 6,
                    color: Shade.text,
                  ),
                ),
                const SizedBox(height: 4),
                Text(
                  'protocol v${widget.protocolVersion}',
                  style: readout.copyWith(color: Shade.faint),
                ),
                const SizedBox(height: 28),
                Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: <Widget>[
                    Expanded(
                      flex: 3,
                      child: _Field(
                        controller: _host,
                        label: 'host',
                        hint: '192.168.1.20',
                        keyboard: TextInputType.url,
                        onSubmit: _dial,
                      ),
                    ),
                    const SizedBox(width: 10),
                    Expanded(
                      child: _Field(
                        controller: _port,
                        label: 'port',
                        hint: '${HostLink.defaultPort}',
                        keyboard: TextInputType.number,
                        digitsOnly: true,
                        onSubmit: _dial,
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 18),
                FilledButton(
                  onPressed: _dialling ? null : _dial,
                  style: FilledButton.styleFrom(
                    minimumSize: const Size.fromHeight(46),
                    backgroundColor: Shade.text,
                    foregroundColor: Shade.ground,
                  ),
                  child: Text(_dialling ? 'connecting…' : 'connect'),
                ),
                const SizedBox(height: 14),
                _Status(state: widget.state),
                const SizedBox(height: 22),
                TextButton(
                  onPressed: widget.onDetach,
                  style: TextButton.styleFrom(foregroundColor: Shade.faint),
                  child: const Text('open the input surface with no host'),
                ),
              ],
            ),
          ),
        ),
      ),
    ),
  );
}

/// What happened last time the user pressed connect.
class _Status extends StatelessWidget {
  const _Status({required this.state});

  final Link state;

  @override
  Widget build(BuildContext context) => switch (state) {
    Offline() || Detached() => const SizedBox(height: 4),
    Dialling(:final String address) => Text(
      'dialling $address',
      style: readout.copyWith(color: Shade.faint),
    ),
    Live(:final String address) => Text(
      'connected to $address',
      style: readout.copyWith(color: Shade.good),
    ),
    Refused(:final String address, :final String reason) => DecoratedBox(
      decoration: BoxDecoration(
        border: Border.all(color: Shade.bad.withValues(alpha: 0.4)),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Padding(
        padding: const EdgeInsets.all(10),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: <Widget>[
            Text('$address refused', style: readout.copyWith(color: Shade.bad)),
            const SizedBox(height: 4),
            Text(reason, style: readout.copyWith(color: Shade.faint)),
          ],
        ),
      ),
    ),
  };
}

class _Field extends StatelessWidget {
  const _Field({
    required this.controller,
    required this.label,
    required this.hint,
    required this.keyboard,
    required this.onSubmit,
    this.digitsOnly = false,
  });

  final TextEditingController controller;
  final String label;
  final String hint;
  final TextInputType keyboard;
  final VoidCallback onSubmit;
  final bool digitsOnly;

  @override
  Widget build(BuildContext context) => TextField(
    controller: controller,
    keyboardType: keyboard,
    autocorrect: false,
    enableSuggestions: false,
    textInputAction: TextInputAction.go,
    onSubmitted: (_) => onSubmit(),
    inputFormatters: digitsOnly
        ? <TextInputFormatter>[FilteringTextInputFormatter.digitsOnly]
        : null,
    style: readout.copyWith(fontSize: 14),
    decoration: InputDecoration(
      labelText: label,
      hintText: hint,
      labelStyle: readout.copyWith(color: Shade.faint),
      hintStyle: readout.copyWith(color: Shade.faint.withValues(alpha: 0.5)),
      enabledBorder: const OutlineInputBorder(
        borderSide: BorderSide(color: Shade.edge),
      ),
      focusedBorder: const OutlineInputBorder(
        borderSide: BorderSide(color: Shade.faint),
      ),
      border: const OutlineInputBorder(),
    ),
  );
}
