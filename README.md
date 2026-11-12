# tachyx

Low-latency remote desktop for Wayland. The host and the relay are Rust. The
clients are Flutter, on Linux and Android. A session is one QUIC connection.

**Status: work in progress.** A desktop streams to a phone end to end, with
audio, pairing and relays. It runs on one tested setup (see [Tested on](#tested-on)).
The wire format is not frozen.

## What works

- **Host** (`tachyx-hostd`). Captures a wlroots output with
  `ext-image-copy-capture-v1` into DMA-BUF and encodes H.264 or AV1 on the GPU
  through VA-API. Injects pointer and keyboard input through the wlr virtual
  pointer and virtual keyboard protocols. Streams the host's sound as Opus, with
  remote volume control.
- **Transport.** One QUIC connection (quinn). Video travels as unreliable
  datagrams. Control and input travel on one reliable stream, so a lost video
  packet never delays an input event.
- **Rate control.** A delay-based congestion controller reads the client's
  receiver report. A resolution ladder spends bitrate before it spends
  resolution. Reed-Solomon parity follows measured loss and drops to zero on a
  clean link.
- **Codec negotiation.** Each end probes its hardware at startup. The host picks
  a codec that both ends can run, and refuses to start without an encoder.
- **Pairing.** Each device has one Ed25519 key, and the public key is the device
  identifier. The host shows a QR code and an 8-digit code. The client pairs with
  CPace over Ristretto255. After that, both ends pin the exact key of the other
  end in mutual TLS. There is no trust on first use: the host refuses an unpaired
  device inside the handshake.
- **Relay** (`relayd`). Presence, rendezvous, reflexive addresses, UDP hole
  punching, and a splice fallback when the punch fails. Session TLS runs end to
  end through the relay. A hostile relay can drop or delay traffic. It cannot
  read or inject.
- **Clients.** Hardware decode with no CPU copy of a frame: VA-API to a GL
  texture on Linux, MediaCodec to a Flutter surface producer on Android. A touch
  screen works as a trackpad. The client draws the cursor and moves it on local
  input at once, then corrects it from the host's reports.
- **Host CLI.** `tachyx-hostd id | status | pair | pairings | sessions | relay |
  doctor`. Each command takes `--json`.

## Not built

- Coordination server, accounts and billing. These are design only, in
  [`spec/architecture.md`](spec/architecture.md).
- Move of a relayed session to a direct path. The route swap works in tests.
  The congestion controller cannot yet handle the change of path.
- Enforcement of per-device permissions. `tachyx-hostd pair --view-only` and
  `--attended` are stored with the pairing, but a session does not read them
  yet: every paired device can send input.
- Clipboard, file transfer, more than one client at a time.
- iOS, macOS, Windows and browser clients. Hosts on compositors that are not
  wlroots.
- A sandboxed process for capture and encode.

## Tested on

NixOS, Hyprland 0.56, AMD Radeon 780M (Mesa VA-API), a Galaxy S25 on Android 16,
and the Linux client. Other GPUs, compositors and phones are not tested.
[`spec/assumptions.md`](spec/assumptions.md) lists what is expected to break on a
different machine.

## Engineering rules

- `unsafe` is forbidden in every crate except `tachyx-unsafe` (raw platform
  handles) and `tachyx-client-ffi` (the Dart boundary). A test in `tachyx-proto`
  checks that every crate takes the workspace lint table. Clippy `pedantic` and
  `missing_docs` are errors.
- The protocol and the client session do no I/O and own no clock
  (`tachyx-proto`, `tachyx-client`). Tests drive them as pure state machines.
  `tachyx-driver` is the one client loop that owns a socket and a clock.
- Numbers come from runs on real hardware. [`spec/findings.md`](spec/findings.md)
  marks each claim as measured, read or inferred, and records where a measurement
  overturned the design. Two examples: Hyprland reports full-frame damage on
  every frame, so damage regions save no bitrate on this compositor. quinn's
  bandwidth estimate read 0.00 Mbit/s on a healthy 100 Mbit/s path, so the
  congestion controller is this project's own.

## Repository layout

| Path | Contents |
|---|---|
| `crates/tachyx-proto` | Wire protocol: control messages (postcard), the 16-byte video header, packetiser, reassembler, parity |
| `crates/tachyx-transport` | QUIC endpoints, datagram pacing and queueing, device identity checks in TLS |
| `crates/tachyx-congestion` | Delay-based congestion control |
| `crates/tachyx-crypto` | Device keys, pairing codes, the CPace exchange, pairing records |
| `crates/tachyx-relay` | Relay protocol, relay client, the routing socket |
| `crates/tachyx-relayd` | Relay daemon |
| `crates/tachyx-media` | Wayland capture, VA-API encode, audio capture |
| `crates/tachyx-input` | Input injection into a wlroots compositor |
| `crates/tachyx-sched` | Realtime thread priority through RealtimeKit |
| `crates/tachyx-hostd` | Host daemon and its CLI |
| `crates/tachyx-client` | Client session state machine, no I/O |
| `crates/tachyx-driver` | Client I/O loop |
| `crates/tachyx-video` | Client decode and present |
| `crates/tachyx-audio` | Client Opus decode, jitter buffer, playback |
| `crates/tachyx-client-ffi` | `flutter_rust_bridge` boundary |
| `crates/tachyx-unsafe` | Raw platform handles |
| `app/` | Flutter client |
| `spec/` | Design documents |

## Build

The Nix flake supplies the full toolchain: Rust with the Android targets,
Flutter, the Android SDK and NDK, GStreamer with the VA-API plugins, and the
Wayland development libraries.

```sh
nix develop
cargo build --release
cargo test --workspace
```

The client:

```sh
cd app
flutter run -d linux
flutter build apk
```

## Run

The host needs a wlroots compositor and a VA-API encoder. Pairing goes through a
relay mailbox, so start a relay first. It can run on the host machine.

```sh
relayd                                    # prints the relay line: address:port#KEY
tachyx-hostd relay add 'ADDRESS:47990#KEY'
tachyx-hostd                              # the daemon
tachyx-hostd doctor                       # checks what a session needs
tachyx-hostd pair                         # shows a QR code and an 8-digit code
```

The binaries are in `target/release`. The built-in relay lists of the host and
the client hold a development relay. Remove it with `tachyx-hostd relay remove`,
and add the same relay line to the client's relay settings.

Configuration is optional and lives in `~/.config/tachyx/hostd.toml`. The device
key lives in `~/.local/state/tachyx`. The host listens on UDP 47989 and the relay
on UDP 47990.

## Documents

| Document | Contents |
|---|---|
| [`architecture.md`](spec/architecture.md) | Overall design, with built and planned parts marked |
| [`protocol.md`](spec/protocol.md) | Wire protocol |
| [`pairing.md`](spec/pairing.md) | Device identity and pairing |
| [`relay.md`](spec/relay.md) | Relay protocol, hole punching, splicing |
| [`congestion.md`](spec/congestion.md) | Congestion control |
| [`findings.md`](spec/findings.md) | Measurements on real hardware |
| [`assumptions.md`](spec/assumptions.md) | What breaks on a different machine |
| [`roadmap.md`](spec/roadmap.md) | What comes next |
| [`acceptance.md`](spec/acceptance.md) | What the current milestone must do |

## License

[AGPL-3.0-or-later](LICENSE).
