/// Choosing a host.
library;

import 'package:flutter/material.dart';

import 'package:tachyx/src/rust/api/pairing.dart' show KnownHostView;
import 'package:tachyx/src/session/hosts.dart';
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
    required this.book,
    required this.onDial,
    required this.onPair,
    required this.onDetach,
    super.key,
  });

  /// How far the client has got.
  final Link state;

  /// The protocol version this build speaks.
  final int protocolVersion;

  /// What a stream may cost and how the trackpad feels.
  final Settings settings;

  /// The hosts this client has paired with.
  final HostBook book;

  /// Called with the host to dial.
  final ValueChanged<KnownHostView> onDial;

  /// Called when the user wants to pair with a new host.
  final VoidCallback onPair;

  /// Called when the user wants the input surface with no host behind it.
  final VoidCallback onDetach;

  @override
  State<ConnectScreen> createState() => _ConnectScreenState();
}

class _ConnectScreenState extends State<ConnectScreen> {
  void _openSettings() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (BuildContext context) =>
            SettingsScreen(settings: widget.settings),
      ),
    );
  }

  @override
  Widget build(BuildContext context) => Material(
    color: Shade.ground,
    child: SafeArea(
      child: AnimatedBuilder(
        animation: Listenable.merge(<Listenable>[widget.settings, widget.book]),
        builder: (BuildContext context, Widget? _) {
          final List<KnownHostView> hosts = widget.book.hosts;
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
                          onForget: widget.book.forget,
                        ),
                        if (widget.book.fault != null) ...<Widget>[
                          const SizedBox(height: 10),
                          Text(
                            widget.book.fault!,
                            style: readout.copyWith(
                              color: Shade.bad,
                              fontSize: 11,
                            ),
                          ),
                        ],
                        const SizedBox(height: 14),
                        TextButton(
                          onPressed: widget.onPair,
                          style: TextButton.styleFrom(
                            foregroundColor: Shade.faint,
                            alignment: Alignment.centerLeft,
                          ),
                          child: const Text('+ pair with a host'),
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
