# LCDSirPlus 0.3.0 (Rust)

LCDSirPlus is an independent, open-source modern replacement for LCDSirReal for
**Logitech LCD devices**. It preserves the familiar LCDSirReal-style display
layout and button-driven workflow while replacing legacy dependencies and
integrations with modern Windows-compatible implementations. This native
Windows 11 x64 Logitech G13 160×43 monochrome LCD implementation has
a smaller footprint and fewer dependencies:

LCDSirPlus is not affiliated with or endorsed by the original LCDSirReal developer.

- **Single native executable** with no bundled runtime or installer framework.
- **No Logitech runtime dependency**: direct-HID G13 backend (the proven path
  from the 0.2.0 repair, with no administrator rights). Logitech Gaming
  Software must be exited because its `LCore.exe` process also owns the LCD.
- **No Process Lasso**: Cache/Frequency CCD bars come from native topology
  detection (L3/NUMA domains + CPUID L3-size labeling of the 3D V-Cache die).
- **Native-first telemetry**: CPU load from scheduler accounting deltas,
  memory from `GlobalMemoryStatusEx`, GPU load/VRAM/temperature from the
  vendor-installed NVAPI or ADLX DLL, and PresentMon frame telemetry when its
  external console is installed. LHM is optional and temperatures-only.

## Fixed dashboard

The normal framebuffer is always 160×43 pixels:

```text
┌───────────────────────────────────────┐
│2026-08-08 Saturday          10:44:14 │
├───────────────────┬───────────────────┤
│CPU C████████      │GPU ███████████    │
│    F██████        │                   │
│MEM █████████      │VMEM ███████       │
├─────────┬─────────┬─────────┬─────────┤
│7P+ 75%  │FPS 144  │GPU 74°C │1% 118   │
└─────────┴─────────┴─────────┴─────────┘
```

Dual-CCD CPUs render stacked Cache (`C`) and Frequency (`F`) micro-bars;
single-CCD CPUs render one full-height CPU bar. The deterministic renderer is
hash-verified against the original Go implementation's goldens:

| Golden dashboard | SHA-256 |
|---|---|
| Normal | `3cee8a2dae57386c1d33b6039d28c79046e569f4c87f41692493dfae345e7525` |
| All-bars-unavailable | `79a153e1a0fba0904b8b9b75cf9daf6a2eaff15eb838fc178e5e68220928373a` |
| All-bars-stale | `c1a643090e938bd807a8e9b5c43791c6ae0efe20093e7c27762e1ee6d378ab6d` |

## G13 backend (proven contract)

Enumerated strictly via SetupAPI; exactly one candidate accepted:

- VID `046D`, PID `C21C`, usage page `FF00`, usage `0000`
- 8-byte input / 992-byte output reports
- Output: report ID `0x03`, 32-byte header + 960-byte payload,
  `report[32 + x + (y/8)*160] |= 1 << (y & 7)`
- Input: report ID `0x01`; LCD buttons in byte 6, bits `0x02 << 0..3`
- Overlapped I/O, 1 s op timeout, bounded reconnect backoff, blank-on-close
- Direct HID refuses to open while exact process name `LCore.exe` is running,
  preventing competing LCD writers. It does not blanket-block G HUB services.
- Static frames are submitted once; animated hardware-test frames are capped at
  10 updates per second while button polling continues independently.

## Command line

```text
LCDSirPlus.exe [--config PATH] [COMMAND]
  (none)             Run the dashboard application (tray + preview)
  --validate-config  Validate the configuration and exit
  --preview          Run with the virtual preview forced on
  --hardware-test    Run the deterministic 10-step G13 test sequence
  --hardware-discover  Passive read-only G13 HID enumeration
  --diagnostics      Write a bounded offline diagnostics ZIP and exit
  --discord-authorize  Authorize Discord local RPC for the current user
  --discord-clear-token  Remove the current-user Discord credential
  --backend hid|virtual  Backend for --hardware-test (default hid)
  --duration-secs N  Visible duration for --hardware-test (default 30)
  --safe-mode        Providers/destructive actions disabled
  --diagnostic-dir PATH  Log/diagnostic output directory
  --version
```

## Build

```powershell
cargo build --release          # target\release\LCDSirPlus.exe
cargo test                     # full suite incl. golden frames
cargo clippy                   # zero-warning policy
pwsh scripts/Build.ps1         # quality gates + provisional release packages
```

Toolchain: stable Rust (MSVC), only dependency is the official `windows`
crate. Copy `lcdsirplus.txt` next to the executable; live configuration and
logs live under `%LOCALAPPDATA%\LCDSirPlus\`.

`LCDSirPlus.exe --diagnostics` writes a local, store-only ZIP to that directory.
Use `--diagnostic-dir PATH` to select both the runtime log directory and the
diagnostics output directory. The bundle is capped at 1 MiB and contains only a
typed report, privacy notice, and SHA-256 manifest. It never includes raw logs,
configuration files, credentials, Discord data, window/process details, paths,
network targets, environment values, registry values, serials, or device paths;
collection does not start hardware, providers, probes, or actions. `--config`
is deliberately ignored in diagnostics mode, including UNC paths and includes.

## Install, update, and uninstall

Verify the downloaded ZIP against `LCDSirPlus-0.3.0-SHA256SUMS.txt`, extract the installer ZIP,
then run from its extracted root:

```powershell
Get-Content .\LCDSirPlus-0.3.0-SHA256SUMS.txt
Get-FileHash .\LCDSirPlus-0.3.0-*.zip -Algorithm SHA256
pwsh -NoProfile -File .\Install.ps1
pwsh -NoProfile -File .\Install.ps1 -EnableLogin  # optional HKCU Run ownership
& "$env:LOCALAPPDATA\Programs\LCDSirPlus\Uninstall.ps1"
& "$env:LOCALAPPDATA\Programs\LCDSirPlus\Uninstall.ps1" `
  -PurgeUserData -ConfirmPurge PURGE-LCDSIRPLUS-DATA
```

Running `Install.ps1` again performs an update. It verifies every declared
member, stages on the same fixed local volume, preserves the installed
`lcdsirplus.txt` byte-for-byte, and rolls back a failed publication/post-check.
Install/update/uninstall refuse while the exact installed executable is
running. Uninstall removes only manifest-owned files and exact owned shortcut/
Run entries; it preserves configuration, unknown install files, and
`%LOCALAPPDATA%\LCDSirPlus` unless purge is explicitly confirmed. No operation
requests elevation, kills a process, or replaces foreign integration state.

Release output contains three deterministic archives plus `LCDSirPlus-0.3.0-SHA256SUMS.txt`:

- `LCDSirPlus-0.3.0-win-x64-portable.zip`
- `LCDSirPlus-0.3.0-win-x64-installer.zip`
- `LCDSirPlus-0.3.0-source.zip`

Each archive has one root directory and a sorted `PACKAGE-MANIFEST.txt` with
SHA-256 and byte size for every other member. Packages are **not code-signed**;
checksum verification is mandatory. PresentMon, LibreHardwareMonitor, vendor
drivers, Discord Desktop, and Discord developer/tester setup are optional
external prerequisites and are not redistributed.

## Verification status

- Automated: the full Rust suite covers golden rendering, config, device
  protocols, native GPU arbitration/projection, LHM restriction, PresentMon,
  Discord RPC/tracker/reload/redaction, DPAPI, and endpoint identity policy.
- Live machine: the running Discord Desktop named pipe passed the bounded
  same-session/current-user/fixed-drive/recognized-image/Authenticode publisher
  verification smoke without sending a client ID or token.
- Live machine: G13 vendor collection enumerated (`046d:c21c`, 8/992) at
  medium integrity; configuration validated; virtual hardware-test sequence
  passes end to end.
- **Pending human acceptance**: physical display of the STEP 01–10 sequence
  on the G13, physical button presses, unplug/replug recovery, and the
  sustained run. Run `--hardware-test --backend hid --duration-secs 60` and
  observe. Software transport results are never physical confirmation.
- **Pending Discord acceptance**: developer-application authorization and live
  voice-channel speaker/channel/reconnect/refresh behavior require the user's
  Discord application, tester account, consent, and another voice participant.

## Completion status

- **P1 complete**: renderer + golden frames, direct-HID G13 backend,
  virtual preview + tray, clock/CPU/CCD/memory providers, config v2 hot
  reload, hardware test.
- **P2 complete**: native NVAPI/ADLX GPU telemetry, optional LHM
  temperatures fallback, interface network throughput, Arctis 7P+ battery,
  XInput, Core Audio, and PresentMon. Vendor hardware, LHM, and the PresentMon
  console remain optional runtime prerequisites; absence renders unavailable.
- **P3 complete**: verified Discord desktop IPC active-speaker overlay,
  RPC OAuth authorization, refresh, and current-user DPAPI credential storage.
- **P4 complete**: guarded hung-process termination, alerts,
  single-instance ownership, startup registration, preview fallback, and
  optional network-quality probes, offline diagnostics, and transactional
  install/update/uninstall packaging.

## Discord authorization

Discord access requires a developer application client ID and may require the
account to be added as an application tester. Register the redirect URI used in
`lcdsirplus.txt` (default `http://127.0.0.1`), set `discord_client_id`, run Discord
Desktop, then authorize:

```powershell
$env:LCDSIRPLUS_DISCORD_CLIENT_SECRET = '<temporary secret only if required>'
.\LCDSirPlus.exe --discord-authorize
Remove-Item Env:\LCDSIRPLUS_DISCORD_CLIENT_SECRET -ErrorAction SilentlyContinue
```

The secret is never accepted in configuration or process arguments. The
access/refresh credential is stored at
`%LOCALAPPDATA%\LCDSirPlus\discord.token`, encrypted for the current Windows
user with DPAPI. `--discord-clear-token` removes only that local copy; revoke
the application in Discord separately when needed. Credential access rejects
reparse points and hard links and verifies the pinned local path boundary.

Authorization uses Discord's verified local named-pipe `AUTHORIZE` flow with
scopes `identify`, `rpc`, and `rpc.voice.read`. LCDSirPlus opens no callback
listener and launches no browser. The only Discord network request is the
HTTPS token exchange/refresh, which has a finite 20-second deadline. Discord
pipe operations are cancelable during shutdown and configuration reload.

The application otherwise remains local-first. Optional network quality uses
only the configured IP literal with a bounded ICMP/TCP deadline and is disabled
in safe mode. Discord authorization/refresh uses bounded outbound HTTPS; LHM is
restricted to loopback. No feature opens an inbound listener.

The hung-window action can terminate a process. It requires repeated bounded
failure evidence, exact identity revalidation, a continuous physical hold, and
release; safe mode disables it. Test only with `--hang-test-harness` and never
with unsaved work. Final physical and Discord acceptance is tracked in
`docs/HARDWARE-ACCEPTANCE.md`.

## License

MIT. Logitech, SteelSeries, LibreHardwareMonitor, PresentMon, and Discord
are trademarks of their owners; no third-party binaries are redistributed.
