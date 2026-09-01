{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    # The daemon captures through Wayland and encodes through VA-API, so the host
    # side is Linux only. The client also builds for Android, from here.
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-linux"
      ]
      (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
            config = {
              allowUnfree = true;
              android_sdk.accept_license = true;
            };
          };

          inherit (pkgs) lib;

          # Flutter hard codes this version. On a mismatch Gradle calls
          # sdkmanager, which cannot write into the read-only store.
          ndkVersion = "28.2.13676358";

          # rust-toolchain.toml is the single source of truth, so cargo, rustup
          # and this flake all agree on the channel, components and targets.
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          androidSdk =
            (pkgs.androidenv.composeAndroidPackages {
              buildToolsVersions = [ "36.0.0" ];
              platformToolsVersion = "37.0.1";
              platformVersions = [ "36" ];
              abiVersions = [
                "arm64-v8a"
                "x86_64"
              ];
              includeNDK = true;
              ndkVersions = [ ndkVersion ];
              includeEmulator = false;
              includeSources = false;
              extraLicenses = [
                "android-sdk-preview-license"
                "google-gdk-license"
              ];
            }).androidsdk;

          androidSdkRoot = "${androidSdk}/libexec/android-sdk";

          # GStreamer finds plugins at run time, so every entry here must also
          # reach GST_PLUGIN_SYSTEM_PATH_1_0.
          gstPackages = with pkgs.gst_all_1; [
            gstreamer
            gst-plugins-base
            gst-plugins-good
            gst-plugins-bad
            gst-plugins-ugly
            gst-libav
          ];

          systemLibs =
            gstPackages
            ++ (with pkgs; [
              wayland
              wayland-protocols
              libxkbcommon
              libdrm
              libgbm
              libva
              gtk3
            ]);

          # The flutter wrapper only appends its own clang to PATH, so the host
          # clang wins and the Linux desktop build cannot find the C runtime
          # objects. Shadow clang and clang++ alone: cc and c++ must stay on gcc,
          # which is what cargo links with.
          flutterClang = pkgs.runCommand "flutter-clang" { } ''
            mkdir -p $out/bin
            ln -s ${pkgs.clang}/bin/clang ${pkgs.clang}/bin/clang++ $out/bin/
          '';

          rustTools = [
            rustToolchain
            # Both of flutter_rust_bridge's build backends shell out to rustup,
            # even the one that never downloads anything.
            pkgs.rustup
            pkgs.cargo-ndk
            pkgs.cargo-watch
            pkgs.cargo-deny
            # flutter_rust_bridge_codegen runs `cargo expand` on every run.
            pkgs.cargo-expand
          ];

          flutterTools = [
            flutterClang
          ]
          ++ (with pkgs; [
            flutter
            flutter_rust_bridge_codegen
            jdk17
            androidSdk
          ]);

          buildTools = with pkgs; [
            pkg-config
            cmake
            ninja
            just
            libva-utils
          ];
        in
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = rustTools ++ flutterTools ++ buildTools;
            buildInputs = systemLibs;

            ANDROID_HOME = androidSdkRoot;
            ANDROID_SDK_ROOT = androidSdkRoot;
            ANDROID_NDK_ROOT = "${androidSdkRoot}/ndk/${ndkVersion}";
            JAVA_HOME = "${pkgs.jdk17}/lib/openjdk";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            GST_PLUGIN_SYSTEM_PATH_1_0 = lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPackages;

            shellHook = ''
              # The GPU drivers live outside the store and must come first: the
              # VA-API and OpenGL drivers have to match the running kernel.
              export LD_LIBRARY_PATH="/run/opengl-driver/lib:${lib.makeLibraryPath systemLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              export LIBVA_DRIVERS_PATH="/run/opengl-driver/lib/dri"

              # Publish the store toolchain under a name of its own so that the
              # Flutter build reuses it instead of downloading a second one.
              rustup toolchain link nix ${rustToolchain} 2>/dev/null || true

              # Gradle and pub write to their caches, and the store is read only.
              export GRADLE_USER_HOME="$HOME/.gradle"
              export PUB_CACHE="$HOME/.pub-cache"
              export PATH="$PUB_CACHE/bin:$PATH"

              # Take over interactive shells only, so that `nix develop --command`
              # still runs the command it was given.
              case $- in
                *i*) exec ${pkgs.zsh}/bin/zsh ;;
              esac
            '';
          };
        }
      );
}
