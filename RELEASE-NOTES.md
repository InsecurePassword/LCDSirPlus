# LCDSirPlus 0.3.0 Release Notes

LCDSirPlus 0.3.0 is the completed Rust port for Windows 11 x64. P1-P4 software
scope is implemented: fixed rendering, direct G13 HID, native telemetry,
Discord active-speaker integration, alerts, guarded hung-window action,
single-instance/startup ownership, offline diagnostics, and transactional
per-user packaging.

## Highlights

- Direct Logitech G13 HID transport without Logitech runtime or elevation.
- Native NVAPI/ADLX GPU metrics, bundled PresentMon frame capture, optional
  LHM, headset/XInput/audio/network providers, and deterministic
  stale/unavailable rendering.
- Expanded the slot registry to 53 modules, including trailing
  30-second load/temperature/disk/FPS graphs, board/cooling/power telemetry,
  native disk/connections/system-battery detail, network health, bottleneck,
  and selected-slot hung-window controls.
- Added native-first source arbitration: documented Win32/vendor APIs first,
  configured HWiNFO shared memory next, then exact/automatic LHM loopback only
  where no safe native source exists. No raw MSR/SMBus/EC/Super-I/O probing.
- Added pane-local 100 ms temperature warnings using shared graph thresholds;
  non-temperature alert behavior remains.
- Bundled Intel's official signed PresentMon v2.5.1 console and exact MIT/
  third-party notices. LCDSirPlus owns it only during active frame capture;
  uninstall removes it. No PresentMon service, MSI, GUI, or API is installed.
- Verified Discord Desktop IPC with bounded OAuth, DPAPI credential storage,
  cancellation, and redacted errors.
- Offline diagnostics ZIP with fixed entries, a 1 MiB cap, manifest hashes,
  no configuration read, and identity-checked no-overwrite publication.
- Per-user install/update/uninstall with package verification, preserved
  `lcdsirplus.txt`, rollback, exact shortcut/Run ownership, and optional explicit
  user-data purge.

## Requirements and Gaps

- Windows 11 x64 and a standard user account.
- Optional telemetry requires its corresponding vendor driver/hardware.
  HWiNFO and LibreHardwareMonitor remain user-managed and are not bundled;
  HWiNFO64 Free shared-memory monitoring has the vendor's 12-hour limit.
- Discord features require Discord Desktop, a developer application/tester
  setup, user consent, and network access for token exchange/refresh.
- Packages are not code-signed; verify supplied SHA-256 manifests.
- Final physical G13 display/buttons/reconnect/endurance and live Discord voice
  acceptance remain operator gates. See `docs/HARDWARE-ACCEPTANCE.md`.

## Upgrade and Removal

Run `Install.ps1` from the extracted installer ZIP. Updates preserve the live
configuration byte-for-byte. `Uninstall.ps1` preserves configuration and
`%LOCALAPPDATA%\LCDSirPlus`; explicit purge requires the documented confirmation
token. Both operations refuse a running installed executable.
