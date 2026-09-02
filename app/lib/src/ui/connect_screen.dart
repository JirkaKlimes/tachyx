/// Choosing a host.
library;

import 'package:flutter/material.dart';

import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/settings.dart';
import 'package:tachyx/src/ui/host_list.dart';
import 'package:tachyx/src/ui/settings_screen.dart';
import 'package:tachyx/src/ui/theme.dart';

/// Where the user says which machine to drive.
///
/// The screen is the list of hosts the user keeps. Tapping one connects,
/// crossing one off forgets it, and a new one is added in place. What a
/// stream may cost and how the trackpad feels live one screen deeper.
class ConnectScreen extends StatefulWidget {
  /// A screen that dials through [onDial], over the hosts [settings] keeps.
  const ConnectScreen({
    required this.state,
    required this.protocolVersion,
    required this.settings,
    required this.onDial,
    required this.onDetach,
    super.key,
  });

  /// How far the client has got.
  final Link state;

  /// The protocol version this build speaks.
  final int protocolVersion;

  /// The kept hosts and the user's other choices.
  final Settings settings;

  /// Called with a `host:port` address to dial.
  final ValueChanged<String> onDial;

  /// Called when the user wants the input surface with no host behind it.
  final VoidCallback onDetach;

  @override
  State<ConnectScreen> createState() => _ConnectScreenState();
}

class _ConnectScreenState extends State<ConnectScreen> {
  /// Whether the add form is open. Open by itself while the list is empty,
  /// because an empty list leaves nothing else on the screen to act on.
  bool _adding = false;

  void _add(String address) {
    widget.settings.keepHost(address);
    setState(() => _adding = false);
  }

  void _openSettings() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (BuildContext context) => SettingsScreen(
          settings: widget.settings,
          onConnect: (String address) {
            Navigator.of(context).pop();
            widget.onDial(address);
          },
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) => Material(
    color: Shade.ground,
    child: SafeArea(
      child: AnimatedBuilder(
        animation: widget.settings,
        builder: (BuildContext context, Widget? _) {
          final List<String> hosts = widget.settings.hosts;
          final bool adding = _adding || hosts.isEmpty;
          return Stack(
            children: <Widget>[
              Center(
                child: SingleChildScrollView(
                  padding: const EdgeInsets.all(28),
                  child: ConstrainedBox(
                    constraints: const BoxConstraints(maxWidth: 380),
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: <Widget>[
                        const Text('tachyx', style: wordmark),
                        const SizedBox(height: 6),
                        Text(
                          'protocol v${widget.protocolVersion}',
                          style: readout.copyWith(color: Shade.dim),
                        ),
                        const SizedBox(height: 36),
                        const Text('hosts', style: label),
                        const SizedBox(height: 10),
                        HostList(
                          hosts: hosts,
                          state: widget.state,
                          onConnect: widget.onDial,
                          onForget: widget.settings.forgetHost,
                        ),
                        const SizedBox(height: 14),
                        if (adding)
                          AddHostForm(onAdd: _add)
                        else
                          TextButton(
                            onPressed: () => setState(() => _adding = true),
                            style: TextButton.styleFrom(
                              foregroundColor: Shade.faint,
                              alignment: Alignment.centerLeft,
                            ),
                            child: const Text('+ add host'),
                          ),
                      ],
                    ),
                  ),
                ),
              ),
              Positioned(
                left: 16,
                right: 16,
                bottom: 8,
                child: Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: <Widget>[
                    TextButton(
                      onPressed: _openSettings,
                      style: TextButton.styleFrom(foregroundColor: Shade.faint),
                      child: const Text('settings'),
                    ),
                    TextButton(
                      onPressed: widget.onDetach,
                      style: TextButton.styleFrom(foregroundColor: Shade.dim),
                      child: const Text('input surface, no host'),
                    ),
                  ],
                ),
              ),
            ],
          );
        },
      ),
    ),
  );
}
