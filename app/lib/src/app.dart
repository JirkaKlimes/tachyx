/// The application, and the one decision it makes: connect screen or session.
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'dart:ui' show FlutterView;

import 'package:flutter/material.dart';

import 'package:tachyx/src/rust/api/session.dart' as rust;
import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/session/viewport.dart';
import 'package:tachyx/src/settings.dart';
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
  final Settings _settings = Settings();
  late final int _protocolVersion = rust.protocolVersion();

  @override
  void initState() {
    super.initState();
    unawaited(_settings.restore());
    // Naming a host in the environment dials it as soon as there is a view to
    // measure. Without this a session can only begin by hand, which makes an
    // end-to-end run something nobody can repeat.
    final String preset = Platform.environment['TACHYX_HOST'] ?? '';
    if (preset.isNotEmpty) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _dial(preset));
    }
  }

  @override
  void dispose() {
    _link.dispose();
    _settings.dispose();
    super.dispose();
  }

  /// Dials `host:port`, or the default port when the address names none.
  void _dial(String address) {
    final String dialled = address.contains(':')
        ? address
        : '$address:${HostLink.defaultPort}';
    final FlutterView view = View.of(context);
    unawaited(() async {
      await _link.dial(
        dialled,
        viewport: viewportOf(view),
        refreshHz: refreshHz(view),
        bitrateCeiling: _settings.streamBitrate,
      );
      // Only an address that answered is worth keeping, and keeping it again
      // moves it to the top: the list stays in most-recently-reached order.
      if (_link.state is Live) await _settings.keepHost(dialled);
    }());
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
            settings: _settings,
            onLeave: _link.hangUp,
          ),
        Detached() => SessionScreen(
          key: const ValueKey<String>('detached'),
          protocolVersion: _protocolVersion,
          settings: _settings,
          onLeave: _link.hangUp,
        ),
        final Link state => ConnectScreen(
          state: state,
          protocolVersion: _protocolVersion,
          settings: _settings,
          onDial: _dial,
          onDetach: _link.detach,
        ),
      },
    ),
  );
}
