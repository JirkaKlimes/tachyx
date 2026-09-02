import 'dart:io';

import 'package:flutter_rust_bridge_hooks/flutter_rust_bridge_hooks.dart';

// The crate is a member of the repository's cargo workspace rather than a copy
// under this package, so that it takes the workspace lints and is built and
// tested by the same `cargo` invocation as everything else.
void main(List<String> args) async {
  await build(args, (input, output) async {
    await FlutterRustBridgeNativeAssetsBuilder(
      cratePath: '../crates/tachyx-client-ffi',
      extraCargoEnvironmentVariables: _pkgConfig(),
    ).run(input: input, output: output);
  });
}

// A build hook runs in a semi-hermetic environment. It keeps PATH, HOME,
// LIBCLANG_PATH, ANDROID_*, CCACHE_* and anything named NIX_*, and strips
// everything else, so that a hook's result depends on its input rather than on
// whoever's shell started it. PKG_CONFIG_PATH is one of the stripped ones, and
// without it the crates that wrap GStreamer cannot find a single library: the
// same cargo command succeeds from a terminal and fails from `flutter build`.
//
// The dev shell copies the value into NIX_PKG_CONFIG_PATH, which the hermetic
// filter lets through, and this puts it back under the name pkg-config reads.
Map<String, String> _pkgConfig() {
  final String? path = Platform.environment['NIX_PKG_CONFIG_PATH'];
  return path == null || path.isEmpty
      ? const <String, String>{}
      : <String, String>{'PKG_CONFIG_PATH': path};
}
