# LAN microphone processing on PC 2

Run the same Windows x64 build on both PCs. All VST and overlay processing stays on PC 2.

```
PC 1 microphone -> UDP -> PC 2 Line A -> virtual ASIO -> Vonovox
PC 1 virtual cable <- UDP <- PC 2 Line B <- virtual ASIO <- Vonovox
```

## PC 2: server and Vonovox

1. Extract the package to a permanent folder. Keep `Modules/rinka_soundbridge_core.dll` with it.
2. Run **Start LAN Server.cmd**, or `RinkaSoundBridge.exe --lan-server`.
3. Select **Rinka SoundBridge ASIO** (default) or **RSB ASIO**. Open **ASIO bridge settings** while stopped to configure its existing buffer/cushion controls. LAN operation uses 48,000 Hz.
4. Click **Start Server**. Driver setup/registration happens here when required. It is also available with `RinkaSoundBridge.exe --register-asio`. PC 1 needs no SoundBridge virtual ASIO registration.
5. In Vonovox, select ASIO, the matching driver name, and **48,000 Hz**. Start/restart Vonovox's audio stream **after the LAN server is running**. Restart Vonovox after replacing the core DLL or changing bridge/buffer settings.
6. Configure microphone pre-processing on **Line A**, and post-processing on **Line B**. The server opens no physical microphone or speaker. VST controls, master levels, line mute, overlays, and GPU cushion policies retain their existing behavior.
7. Give PC 1 the server's Ethernet IPv4 address and port (default UDP **40100**). The address display uses the default network route; use `ipconfig` to find the correct Ethernet adapter if a VPN or another NIC is selected.

## PC 1: capture and cable return

1. Run **Start LAN Client.cmd**, or `RinkaSoundBridge.exe --lan-client`.
2. Enter PC 2's IPv4 address and UDP port.
3. Select **WASAPI Shared**, **WASAPI Exclusive**, or **ASIO Native**, then choose the microphone/ASIO driver and microphone channel. Channels are numbered from 1 in the UI.
4. Select the virtual cable's **playback** endpoint as **Return playback device**. For example, VB-CABLE uses `CABLE Input`; Virtual Audio Cable commonly uses `Line 1`. Discord/OBS/other consumers select the cable's corresponding **recording** endpoint.
5. Start with the **5 ms receive buffer** on each PC. The device buffer/latency section exposes capture buffer size and WASAPI latency hints; ASIO uses its own driver's buffer rules. Shared-mode fallback remains available if a device rejects WASAPI Exclusive.
6. Click **Connect**. The mic and return level/mute controls are live. Other device/connection changes require Disconnect first.

Client mode skips VST scanning, virtual ASIO setup, and GPU configuration. It does not run processing chains. LAN connections start manually, including after restarting the app. Switching roles stops the previous routes; Local-mode device routing and saved VST states are preserved.

## Firewall and network

- Use wired Ethernet on a trusted LAN. V1 sends unencrypted, uncompressed float32 PCM: mono upstream and stereo downstream, at 48 kHz.
- Allow `RinkaSoundBridge.exe` on the Windows **Private** network profile on both PCs. PC 2 must accept incoming UDP on the configured server port. The client binds an ephemeral port and receives the server's return on that same socket.
- With a custom firewall policy, add an inbound Private-profile UDP rule for the server's configured port, and allow the client's return traffic. Do not forward this port through your router.
- One client connects to a server. A second client sees **busy** until the first disconnects or its three-second timeout expires. Interrupted connections retry automatically until Disconnect is selected.
- Missing audio fades to silence. Disconnects flush network buffers; reconnects use a fresh session. A stalled capture or playback endpoint reports an error requiring device repair and reconnect.
- **Network RTT** measures control-packet RTT. **Server Processing Delay** measures/estimates the server's internal path, excluding the LAN and PC 1's devices. The receive target is additional reserve; actual fill also accounts for capture/render callback bursts. Large device blocks increase latency. These buffers are separate from ASIO cushions and Vonovox's processing latency.

## Build and test

Install stable Rust (`x86_64-pc-windows-msvc`) and Visual Studio C++ Build Tools with a Windows SDK. Run the following from the repository's `source/` directory:

```powershell
.\build-lan.ps1
```

This builds both executables and the core DLL, creates `source/dist`, and packages `source/RinkaSoundBridge-LAN.zip`. The separate root `app/` directory holds your local runnable copy and runtime data and is excluded from Git. The build does not install/register ASIO by default; `-RegisterAsio` explicitly runs registration on the current PC. A workspace-local Rust toolchain in `source/.tools` is supported without changing your global PATH. `Cargo.lock` pins dependencies.

```powershell
cargo test --locked --all-targets
```

Tests require localhost UDP access. Close other SoundBridge/Vonovox audio sessions before running tests: the headless server test exercises the actual named ASIO buses with a simulated voice-changer callback. Tests cover protocol validation, callback packetization, session isolation/restart, timeout, jitter/loss, both server bridge adapters, a one-hour clock-controller simulation at ±300 ppm, and real resampling under jitter/drift.

## Transport latency check

The bundled console utility tests transport without microphones, virtual drivers, VSTs, or Vonovox. Stop the normal LAN server first, or use another port.

On PC 2:

```powershell
.\lan_echo.exe server 40100 120
```

On PC 1:

```powershell
.\lan_echo.exe probe 192.168.1.100 40100 30
```

Allow `lan_echo.exe` through the Private-profile firewall for this test. Results include both receive buffers, packetization, resampling, and scheduling. The approximate transport target is 10–20 ms round trip on wired Ethernet; this is separate from total microphone-to-Vonovox-to-cable delay.

## Hardware acceptance checklist

- Test each capture method with your actual microphone/interface at 48 kHz and the selected mono channel.
- Test each virtual ASIO bridge with actual Vonovox; confirm stereo return and Line A/B processing.
- Run the full two-PC route for an hour, checking loss/late/underrun counters and stable latency.
- Unplug/reconnect Ethernet, stop/restart the server, and restart Vonovox. Confirm recovery and no stale audio.
- Check virtual cable output in the consuming app, and confirm Local-mode settings survive role changes.

Automated/headless tests do not substitute for these two-PC hardware checks.
