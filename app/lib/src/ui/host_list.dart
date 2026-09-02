/// The hosts the user keeps, as one widget both screens share.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/ui/theme.dart';

/// The kept hosts: tap one to connect, cross one off to forget it.
///
/// The list itself carries the connection state inline. A row being dialled
/// says so under its own address, and a refusal is read where it happened
/// rather than in a status bar somewhere else on the screen.
class HostList extends StatelessWidget {
  /// A list over [hosts], dialling through [onConnect].
  const HostList({
    required this.hosts,
    required this.state,
    required this.onConnect,
    required this.onForget,
    super.key,
  });

  /// The kept addresses, most recently used first.
  final List<String> hosts;

  /// How far the client has got, so the row being dialled can say so.
  final Link state;

  /// Called with the address of a tapped row.
  final ValueChanged<String> onConnect;

  /// Called with the address of a crossed-off row.
  final ValueChanged<String> onForget;

  bool get _busy => state is Dialling;

  @override
  Widget build(BuildContext context) {
    if (hosts.isEmpty) {
      return const Padding(
        padding: EdgeInsets.symmetric(vertical: 18),
        child: Text(
          'no hosts yet. add one to begin.',
          style: TextStyle(color: Shade.dim, fontSize: 13),
        ),
      );
    }
    return DecoratedBox(
      decoration: BoxDecoration(
        border: Border.all(color: Shade.edge),
        borderRadius: BorderRadius.circular(kCorner),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          for (final (int index, String address) in hosts.indexed) ...<Widget>[
            if (index > 0) const Divider(height: 1, color: Shade.edge),
            _HostRow(
              address: address,
              state: state,
              enabled: !_busy,
              onConnect: () => onConnect(address),
              onForget: () => onForget(address),
            ),
          ],
        ],
      ),
    );
  }
}

class _HostRow extends StatelessWidget {
  const _HostRow({
    required this.address,
    required this.state,
    required this.enabled,
    required this.onConnect,
    required this.onForget,
  });

  final String address;
  final Link state;
  final bool enabled;
  final VoidCallback onConnect;
  final VoidCallback onForget;

  /// What this row has to say about itself, or null for a quiet row.
  Widget? _status() => switch (state) {
    Dialling(:final String address) when address == this.address => Text(
      'dialling…',
      style: readout.copyWith(color: Shade.faint, fontSize: 11),
    ),
    Refused(:final String address, :final String reason)
        when address == this.address =>
      Text(
        reason,
        style: readout.copyWith(color: Shade.bad, fontSize: 11),
      ),
    _ => null,
  };

  @override
  Widget build(BuildContext context) {
    final Widget? status = _status();
    return InkWell(
      onTap: enabled ? onConnect : null,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(14, 12, 4, 12),
        child: Row(
          children: <Widget>[
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: <Widget>[
                  Text(
                    address,
                    style: readout.copyWith(fontSize: 13),
                    overflow: TextOverflow.ellipsis,
                  ),
                  if (status != null) ...<Widget>[
                    const SizedBox(height: 4),
                    status,
                  ],
                ],
              ),
            ),
            IconButton(
              onPressed: enabled ? onForget : null,
              icon: const Icon(Icons.close, size: 14),
              color: Shade.dim,
              tooltip: 'forget',
            ),
          ],
        ),
      ),
    );
  }
}

/// Two fields and a verb, for putting a new host on the list.
class AddHostForm extends StatefulWidget {
  /// A form that reports a full `host:port` address through [onAdd].
  const AddHostForm({required this.onAdd, super.key});

  /// Called with the combined address once the user commits it.
  final ValueChanged<String> onAdd;

  @override
  State<AddHostForm> createState() => _AddHostFormState();
}

class _AddHostFormState extends State<AddHostForm> {
  final TextEditingController _host = TextEditingController();
  final TextEditingController _port = TextEditingController(
    text: '${HostLink.defaultPort}',
  );

  @override
  void dispose() {
    _host.dispose();
    _port.dispose();
    super.dispose();
  }

  void _add() {
    final String host = _host.text.trim();
    if (host.isEmpty) return;
    final int port = int.tryParse(_port.text) ?? HostLink.defaultPort;
    widget.onAdd('$host:$port');
    _host.clear();
    _port.text = '${HostLink.defaultPort}';
    FocusScope.of(context).unfocus();
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: <Widget>[
      Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: <Widget>[
          Expanded(
            flex: 3,
            child: HairlineField(
              controller: _host,
              label: 'host',
              hint: '192.168.1.20',
              keyboard: TextInputType.url,
              onSubmit: _add,
            ),
          ),
          const SizedBox(width: 10),
          Expanded(
            child: HairlineField(
              controller: _port,
              label: 'port',
              hint: '${HostLink.defaultPort}',
              keyboard: TextInputType.number,
              digitsOnly: true,
              onSubmit: _add,
            ),
          ),
        ],
      ),
      const SizedBox(height: 12),
      OutlinedButton(
        onPressed: _add,
        style: OutlinedButton.styleFrom(
          minimumSize: const Size.fromHeight(44),
          foregroundColor: Shade.text,
          side: const BorderSide(color: Shade.edgeLit),
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(kCorner),
          ),
        ),
        child: const Text('add host'),
      ),
    ],
  );
}

/// A text field drawn in the client's own hairline style.
class HairlineField extends StatelessWidget {
  /// A field over [controller].
  const HairlineField({
    required this.controller,
    required this.label,
    required this.hint,
    required this.keyboard,
    required this.onSubmit,
    this.digitsOnly = false,
    super.key,
  });

  /// The text being edited.
  final TextEditingController controller;

  /// What the field is for, shown above the text.
  final String label;

  /// An example value, shown while the field is empty.
  final String hint;

  /// Which on-screen keyboard to raise.
  final TextInputType keyboard;

  /// Called when the user submits from the keyboard.
  final VoidCallback onSubmit;

  /// Whether anything but digits is refused as it is typed.
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
    cursorColor: Shade.text,
    decoration: InputDecoration(
      labelText: label,
      hintText: hint,
      isDense: true,
      filled: true,
      fillColor: Shade.raised,
      labelStyle: readout.copyWith(color: Shade.faint),
      hintStyle: readout.copyWith(color: Shade.dim),
      enabledBorder: OutlineInputBorder(
        borderSide: const BorderSide(color: Shade.edge),
        borderRadius: BorderRadius.circular(kCorner),
      ),
      focusedBorder: OutlineInputBorder(
        borderSide: const BorderSide(color: Shade.edgeLit),
        borderRadius: BorderRadius.circular(kCorner),
      ),
      border: OutlineInputBorder(
        borderRadius: BorderRadius.circular(kCorner),
      ),
    ),
  );
}
