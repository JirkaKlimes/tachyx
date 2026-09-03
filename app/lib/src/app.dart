/// The application, and the one decision it makes: connect screen or session.
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'dart:ui' show FlutterView;

import 'package:flutter/material.dart';

import 'package:app_links/app_links.dart';

import 'package:tachyx/src/rust/api/pairing.dart' show KnownHostView;
import 'package:tachyx/src/rust/api/session.dart' as rust;
import 'package:tachyx/src/session/hosts.dart';
import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/session/viewport.dart';
import 'package:tachyx/src/settings.dart';
import 'package:tachyx/src/ui/connect_screen.dart';
import 'package:tachyx/src/ui/pair_screen.dart';
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
  final HostBook _book = HostBook();
  final AppLinks _links = AppLinks();
  StreamSubscription<Uri>? _linked;
  late final int _protocolVersion = rust.protocolVersion();

  /// The one navigator key, so a deep link can open the pairing screen from
  /// outside any widget's context.
  final GlobalKey<NavigatorState> _navigator = GlobalKey<NavigatorState>();

  @override
  void initState() {
    super.initState();
    unawaited(_settings.restore());
    unawaited(_book.load());
    unawaited(_watchLinks());
    // Naming a paired host in the environment dials it as soon as there is a
    // view to measure. Without this a session can only begin by hand, which
    // makes an end-to-end run something nobody can repeat. It names a device
    // identifier rather than an address, because an address alone cannot
    // connect: the host has to hold this client's key.
    final String preset = Platform.environment['TACHYX_HOST'] ?? '';
    if (preset.isNotEmpty) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _dialPreset(preset));
    }
  }

  @override
  void dispose() {
    unawaited(_linked?.cancel());
    _link.dispose();
    _settings.dispose();
    _book.dispose();
    super.dispose();
  }

  /// Opens the pairing screen when a `tachyx://pair` link arrives.
  ///
  /// Both halves are needed. The stream does not replay the link that started
  /// the application, so a cold start is read once from `getInitialLink` and
  /// everything after it comes from the stream.
  Future<void> _watchLinks() async {
    _linked = _links.uriLinkStream.listen(_openPairing);
    final Uri? started = await _links.getInitialLink();
    if (started != null) _openPairing(started);
  }

  void _openPairing([Uri? link]) {
    final NavigatorState? navigator = _navigator.currentState;
    if (navigator == null) return;
    _book.forgetPairing();
    unawaited(
      navigator.push(
        MaterialPageRoute<void>(
          builder: (BuildContext context) => AnimatedBuilder(
            animation: _book,
            builder: (BuildContext context, Widget? _) => PairScreen(
              state: _book.pairing,
              canScan: platformHasCamera,
              seed: link?.toString(),
              onCode: (String code) =>
                  unawaited(_book.pair(code, _settings.relays)),
              onDone: () => navigator.maybePop(),
            ),
          ),
        ),
      ),
    );
  }

  /// Dials the host the environment named, once the paired list has been read.
  ///
  /// Matched on the identifier or on a prefix of it, so a run script does not
  /// have to carry all 52 characters.
  Future<void> _dialPreset(String wanted) async {
    await _book.load();
    final KnownHostView? host = _book.hosts
        .where((KnownHostView paired) => paired.device.startsWith(wanted))
        .firstOrNull;
    if (host != null && mounted) _dial(host);
  }

  /// Opens a session with a paired host.
  void _dial(KnownHostView host) {
    final FlutterView view = View.of(context);
    unawaited(
      _link.dial(
        host.device,
        name: host.name,
        viewport: viewportOf(view),
        refreshHz: refreshHz(view),
        bitrateCeiling: _settings.streamBitrate,
        relays: _settings.relays,
      ),
    );
  }

  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'tachyx',
    navigatorKey: _navigator,
    debugShowCheckedModeBanner: false,
    theme: tachyxTheme(),
    // Flutter's own shortcut set moves focus on tab and the arrows and acts on
    // space and enter. Every one of those keys belongs to the remote desktop.
    shortcuts: const <ShortcutActivator, Intent>{},
    home: AnimatedBuilder(
      animation: _link,
      builder: (BuildContext context, Widget? _) => switch (_link.state) {
        Live(:final String host, :final String name, :final rust.Session session) =>
          SessionScreen(
            key: ValueKey<String>('live:$host'),
            address: name,
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
          book: _book,
          onDial: _dial,
          onPair: _openPairing,
          onDetach: _link.detach,
        ),
      },
    ),
  );
}
