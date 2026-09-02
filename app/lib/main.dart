import 'dart:io';

import 'package:flutter/widgets.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;

import 'package:tachyx/src/app.dart';
import 'package:tachyx/src/rust/frb_generated.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init(externalLibrary: _bundled());
  runApp(const TachyxApp());
}

/// The client's own library, named by its path rather than by its name.
///
/// A Linux bundle puts it in `lib/` beside the runner, and the runner's runpath
/// names that directory, but that is not enough: the call that loads it comes
/// from Flutter's engine library and not from the runner, and a runpath does not
/// carry across that boundary. The default loader falls through to a bare
/// `dlopen`, the dynamic linker has no reason to look inside the bundle, and the
/// application dies before its first frame.
///
/// Null everywhere the default loader already works: on Android the library is
/// unpacked where the linker does look, and under `flutter test` there is no
/// bundle to find.
ExternalLibrary? _bundled() {
  if (!Platform.isLinux) return null;
  final String beside =
      '${File(Platform.resolvedExecutable).parent.path}/lib'
      '/libtachyx_client_ffi.so';
  return File(beside).existsSync() ? ExternalLibrary.open(beside) : null;
}
