# tachyx

Low-latency remote desktop. Rust core, Flutter clients.

The design document is [`spec/architecture.md`](spec/architecture.md).

## Current milestone

One Wayland host on a LAN, one client. Nothing else exists yet.

* `hostd` runs headless on a wlroots compositor (developed against Hyprland).
  It captures a monitor, encodes H.264 on the GPU, and streams it over QUIC.
* The Flutter client connects over localhost or the LAN, decodes with hardware
  acceleration, and sends pointer and keyboard input back.
* Input on a touch screen behaves like a trackpad: one tap is a left click,
  two taps a right click, three taps toggle the on-screen keyboard, a one-finger
  drag moves the pointer, a two-finger drag scrolls.

Pairing, authentication, relays and the coordination server are **not built**.
The transport accepts any client that can reach the port. Run it on a network
you trust, and only there.

## Repository layout

```
crates/            Rust workspace (virtual manifest)
apps/flutter/      Flutter client: Android and Linux
spec/              Protocol and architecture documents
```

## Development

The repository ships a Nix flake with the full toolchain: Rust with the Android
targets, Flutter, the Android SDK and NDK, GStreamer with the hardware encoders,
and the Wayland and VA-API development libraries.

```sh
nix develop
```
