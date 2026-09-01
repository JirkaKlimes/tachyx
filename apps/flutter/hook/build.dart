import 'package:flutter_rust_bridge_hooks/flutter_rust_bridge_hooks.dart';

// The crate is a member of the repository's cargo workspace rather than a copy
// under this package, so that it takes the workspace lints and is built and
// tested by the same `cargo` invocation as everything else.
void main(List<String> args) async {
  await build(args, (input, output) async {
    await const FlutterRustBridgeNativeAssetsBuilder(
      cratePath: '../../crates/tachyx-client-ffi',
    ).run(input: input, output: output);
  });
}
