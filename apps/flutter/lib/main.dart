import 'package:flutter/widgets.dart';

import 'package:tachyx/src/app.dart';
import 'package:tachyx/src/rust/frb_generated.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  runApp(const TachyxApp());
}
