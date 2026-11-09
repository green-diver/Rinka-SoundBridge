# Rinka SoundBridge LAN

This fork adds two-PC LAN audio routing to Rinka SoundBridge. PC 1 captures the microphone and plays the processed return into a virtual audio cable. PC 2 runs the VST chains and Vonovox through either built-in ASIO bridge.

- [`source/`](source/) contains the Rust application, tests, assets, and build scripts.
- `app/` is the local runnable build and runtime data; it is excluded from Git.
- [LAN setup and diagnostics](source/LAN_SETUP.md) explains both PC roles, device selection, and network settings.
- [Original project documentation](source/README.md) covers the existing audio and VST features.

## Build

Install stable Rust for `x86_64-pc-windows-msvc` and Visual Studio C++ Build Tools with a Windows SDK, then run from the repository root:

```powershell
.\source\build-lan.ps1
```

The build creates `source/dist/` and `source/RinkaSoundBridge-LAN.zip`. It also supports the workspace-local Rust toolchain under `source/.tools/`.

Extract the package on both PCs. Run the executable and select **LAN Client** on PC 1 and **LAN Server** on PC 2, or use the included launchers. All VST and overlay processing stays on PC 2. Start the server, then start Vonovox's matching ASIO stream at 48 kHz, and connect the client.

## Test

From `source/`:

```powershell
cargo test --locked --all-targets
```

The UDP tests need localhost network access. Close other SoundBridge/Vonovox audio sessions before running the tests that exercise the shared ASIO buses. See the [setup guide](source/LAN_SETUP.md) for the two-PC hardware acceptance checks.
