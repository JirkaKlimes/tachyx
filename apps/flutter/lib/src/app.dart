/// The application, and the one decision it makes: connect screen or session.
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'dart:ui' show FlutterView;

import 'package:flutter/material.dart';

import 'package:tachyx/src/rust/api/session.dart' as rust;
import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/session/viewport.dart';
import 'package:tachyx/src/ui/connect_screen.dart';
import 'package:tachyx/src/ui/session_screen.dart';
import 'package:tachyx/src/ui/theme.dart';

/// The tachyx client.
class TachyxApp extends StatefulWidget {
  /// A client that starts on the connect screen.
  const TachyxApp({super.key});

  @override
  State<TachyxApp> createState() => _TachyxAppState();
}

class _TachyxAppState extends State<TachyxApp> {
  final HostLink _link = HostLink();
  late final int _protocolVersion = rust.protocolVersion();

  @override
  void initState() {
    super.initState();
    unawaited(_link.restore());
    // Naming a host in the environment dials it as soon as there is a view to
    // measure. Without this a session can only begin by hand, which makes an
    // end-to-end run something nobody can repeat.
    final String preset = Platform.environment['TACHYX_HOST'] ?? '';
    if (preset.isNotEmpty) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _dialAddress(preset));
    }
  }

  /// Dials `host:port`, or the default port when the address names none.
  void _dialAddress(String address) {
    final int colon = address.lastIndexOf(':');
    if (colon < 0) {
      _dial(address, HostLink.defaultPort);
      return;
    }
    final int? port = int.tryParse(address.substring(colon + 1));
    _dial(address.substring(0, colon), port ?? HostLink.defaultPort);
  }

  @override
  void dispose() {
    _link.dispose();
    super.dispose();
  }

  void _dial(String host, int port) {
    final FlutterView view = View.of(context);
    unawaited(
      _link.dial(
        '$host:$port',
        viewport: viewportOf(view),
        refreshHz: refreshHz(view),
      ),
    );
  }

  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'tachyx',
    debugShowCheckedModeBanner: false,
    theme: tachyxTheme(),
    // Flutter's own shortcut set moves focus on tab and the arrows and acts on
    // space and enter. Every one of those keys belongs to the remote desktop.
    shortcuts: const <ShortcutActivator, Intent>{},
    home: AnimatedBuilder(
      animation: _link,
      builder: (BuildContext context, Widget? _) => switch (_link.state) {
        Live(:final String address, :final rust.Session session) =>
          SessionScreen(
            key: ValueKey<String>('live:$address'),
            address: address,
            session: session,
            protocolVersion: _protocolVersion,
            onLeave: _link.hangUp,
          ),
        Detached() => SessionScreen(
          key: const ValueKey<String>('detached'),
          protocolVersion: _protocolVersion,
          onLeave: _link.hangUp,
        ),
        final Link state => ConnectScreen(
          state: state,
          protocolVersion: _protocolVersion,
          remembered: _link.remembered,
          onDial: _dial,
          onDetach: _link.detach,
        ),
      },
    ),
  );
}
