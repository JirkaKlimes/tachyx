/// The relays this client asks, as one widget the settings screen shows.
library;

import 'package:flutter/material.dart';

import 'package:tachyx/src/ui/host_list.dart' show HairlineField;
import 'package:tachyx/src/ui/theme.dart';

/// The configured relays: one row each, crossed off to remove.
///
/// A relay is written `address:port#KEY`. The key is what stops somebody who
/// can intercept the address from becoming the relay and learning who this
/// client reaches and when. It buys that and nothing above it: a session is end
/// to end between two pinned device keys, so even a hostile relay cannot read
/// one.
class RelayList extends StatelessWidget {
  /// A list over [relays], removing through [onForget].
  const RelayList({required this.relays, required this.onForget, super.key});

  /// The configured relays, in the order they are tried.
  final List<String> relays;

  /// Called with the relay of a crossed-off row.
  final ValueChanged<String> onForget;

  @override
  Widget build(BuildContext context) {
    if (relays.isEmpty) {
      return const Padding(
        padding: EdgeInsets.symmetric(vertical: 18),
        child: Text(
          'no relay. this client can reach only hosts on this network, and '
          'can pair with none.',
          style: TextStyle(color: Shade.dim, fontSize: 13, height: 1.5),
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
          for (final (int index, String relay) in relays.indexed) ...<Widget>[
            if (index > 0) const Divider(height: 1, color: Shade.edge),
            _RelayRow(relay: relay, onForget: () => onForget(relay)),
          ],
        ],
      ),
    );
  }
}

class _RelayRow extends StatelessWidget {
  const _RelayRow({required this.relay, required this.onForget});

  final String relay;
  final VoidCallback onForget;

  @override
  Widget build(BuildContext context) {
    // The address is what a person recognises; the key after it is 52
    // characters nobody reads. Both are shown, and the key is dimmed.
    final int hash = relay.indexOf('#');
    final String address = hash < 0 ? relay : relay.substring(0, hash);
    final String key = hash < 0 ? '' : relay.substring(hash);
    return Padding(
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
                if (key.isNotEmpty) ...<Widget>[
                  const SizedBox(height: 2),
                  Text(
                    key,
                    style: readout.copyWith(color: Shade.dim, fontSize: 10),
                    overflow: TextOverflow.ellipsis,
                  ),
                ],
              ],
            ),
          ),
          IconButton(
            onPressed: onForget,
            icon: const Icon(Icons.close, size: 14),
            color: Shade.dim,
            tooltip: 'remove',
          ),
        ],
      ),
    );
  }
}

/// One field and a verb, for putting a relay on the list.
class AddRelayForm extends StatefulWidget {
  /// A form that reports a whole `address:port#KEY` through [onAdd].
  const AddRelayForm({required this.onAdd, super.key});

  /// Called with the relay once the user commits it.
  final ValueChanged<String> onAdd;

  @override
  State<AddRelayForm> createState() => _AddRelayFormState();
}

class _AddRelayFormState extends State<AddRelayForm> {
  final TextEditingController _relay = TextEditingController();

  @override
  void dispose() {
    _relay.dispose();
    super.dispose();
  }

  void _add() {
    final String written = _relay.text.trim();
    // The client core refuses one that is not written this way, and says so.
    // This only stops an empty one reaching it.
    if (written.isEmpty) return;
    widget.onAdd(written);
    _relay.clear();
    FocusScope.of(context).unfocus();
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: <Widget>[
      HairlineField(
        controller: _relay,
        label: 'relay',
        hint: '100.64.0.2:47990#KEY',
        keyboard: TextInputType.url,
        onSubmit: _add,
      ),
      const SizedBox(height: 10),
      Align(
        alignment: Alignment.centerLeft,
        child: TextButton(
          onPressed: _add,
          style: TextButton.styleFrom(foregroundColor: Shade.faint),
          child: const Text('+ add relay'),
        ),
      ),
    ],
  );
}
