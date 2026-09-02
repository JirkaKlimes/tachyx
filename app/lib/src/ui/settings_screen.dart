/// The user's choices: what a stream may cost, how the trackpad feels, and
/// which hosts are kept.
library;

import 'package:flutter/material.dart';

import 'package:tachyx/src/session/link.dart';
import 'package:tachyx/src/settings.dart';
import 'package:tachyx/src/ui/host_list.dart';
import 'package:tachyx/src/ui/theme.dart';

/// The settings screen.
///
/// Everything on it takes effect as it is moved and is stored as it changes.
/// There is no save button, because a setting with an unsaved state is two
/// settings.
class SettingsScreen extends StatelessWidget {
  /// A screen over [settings].
  const SettingsScreen({
    required this.settings,
    required this.onConnect,
    super.key,
  });

  /// What is being edited.
  final Settings settings;

  /// Called with an address when the user connects from here.
  final ValueChanged<String> onConnect;

  @override
  Widget build(BuildContext context) => Material(
    color: Shade.ground,
    child: SafeArea(
      child: AnimatedBuilder(
        animation: settings,
        builder: (BuildContext context, Widget? _) => SingleChildScrollView(
          padding: const EdgeInsets.fromLTRB(20, 8, 20, 28),
          child: Center(
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 440),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: <Widget>[
                  Row(
                    children: <Widget>[
                      IconButton(
                        onPressed: () => Navigator.of(context).pop(),
                        icon: const Icon(Icons.arrow_back, size: 18),
                        color: Shade.faint,
                        tooltip: 'back',
                      ),
                      const SizedBox(width: 4),
                      const Text(
                        'settings',
                        style: TextStyle(fontSize: 16, color: Shade.text),
                      ),
                    ],
                  ),
                  const SizedBox(height: 24),
                  const Text('stream', style: label),
                  const SizedBox(height: 4),
                  _SliderRow(
                    name: 'max bitrate',
                    value: _mbit(settings.streamBitrate),
                    min: _mbit(Settings.minBitrate),
                    max: _mbit(Settings.maxBitrate),
                    divisions:
                        _mbitOf(Settings.maxBitrate) -
                        _mbitOf(Settings.minBitrate),
                    display: '${_mbitOf(settings.streamBitrate)} Mbit/s',
                    onChanged: (double mbit) =>
                        settings.setStreamBitrate(mbit.round() * 1000000),
                  ),
                  const Text(
                    'a ceiling, not a target. the host spends less whenever '
                    'the picture allows.',
                    style: TextStyle(
                      color: Shade.dim,
                      fontSize: 12,
                      height: 1.5,
                    ),
                  ),
                  const SizedBox(height: 28),
                  const Text('trackpad', style: label),
                  const SizedBox(height: 4),
                  _SliderRow(
                    name: 'sensitivity',
                    value: settings.sensitivity,
                    min: Settings.minSensitivity,
                    max: Settings.maxSensitivity,
                    divisions:
                        ((Settings.maxSensitivity - Settings.minSensitivity) *
                                10)
                            .round(),
                    display: '${settings.sensitivity.toStringAsFixed(1)}×',
                    onChanged: settings.setSensitivity,
                  ),
                  _SliderRow(
                    name: 'acceleration',
                    value: settings.pointerAccel,
                    min: Settings.minPointerAccel,
                    max: Settings.maxPointerAccel,
                    divisions:
                        ((Settings.maxPointerAccel - Settings.minPointerAccel) *
                                10)
                            .round(),
                    display: settings.pointerAccel == 0
                        ? 'off'
                        : settings.pointerAccel.toStringAsFixed(1),
                    onChanged: settings.setPointerAccel,
                  ),
                  const Text(
                    'a flick crosses the remote screen, a slow drag stays as '
                    'precise as it was.',
                    style: TextStyle(
                      color: Shade.dim,
                      fontSize: 12,
                      height: 1.5,
                    ),
                  ),
                  const SizedBox(height: 28),
                  const Text('hosts', style: label),
                  const SizedBox(height: 10),
                  HostList(
                    // Settings is reachable only while nothing is being
                    // dialled, so the list here never has a state to report.
                    hosts: settings.hosts,
                    state: const Offline(),
                    onConnect: onConnect,
                    onForget: settings.forgetHost,
                  ),
                  const SizedBox(height: 14),
                  AddHostForm(onAdd: settings.keepHost),
                ],
              ),
            ),
          ),
        ),
      ),
    ),
  );

  static double _mbit(int bitsPerSecond) => bitsPerSecond / 1000000;

  static int _mbitOf(int bitsPerSecond) => (bitsPerSecond / 1000000).round();
}

/// A named slider with its value read out beside the name.
class _SliderRow extends StatelessWidget {
  const _SliderRow({
    required this.name,
    required this.value,
    required this.min,
    required this.max,
    required this.divisions,
    required this.display,
    required this.onChanged,
  });

  final String name;
  final double value;
  final double min;
  final double max;
  final int divisions;
  final String display;
  final ValueChanged<double> onChanged;

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: <Widget>[
      Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: <Widget>[
            Text(name, style: const TextStyle(color: Shade.text, fontSize: 13)),
            Text(display, style: readout),
          ],
        ),
      ),
      Slider(
        value: value.clamp(min, max),
        min: min,
        max: max,
        divisions: divisions,
        onChanged: onChanged,
      ),
    ],
  );
}
