# LCDForge 0.3.0 Release Notes

LCDForge 0.3.0 is the completed Rust port for Windows 11 x64. P1-P4 software
scope is implemented: fixed rendering, direct G13 HID, native telemetry,
Discord active-speaker integration, alerts, guarded hung-window action,
single-instance/startup ownership, offline diagnostics, and transactional
per-user packaging.

## Highlights

- Direct Logitech G13 HID transport without Logitech runtime or elevation.
- Native NVAPI/ADLX GPU metrics, optional PresentMon and LHM, headset/XInput/
  audio/network providers, and deterministic stale/unavailable rendering.
- Verified Discord Desktop IPC with bounded OAuth, DPAPI credential storage,
  cancellation, and redacted errors.
- Offline diagnostics ZIP with fixed entries, a 1 MiB cap, manifest hashes,
  no configuration read, and identity-checked no-overwrite publication.
- Per-user install/update/uninstall with package verification, preserved
  `lcdforge.txt`, rollback, exact shortcut/Run ownership, and optional explicit
  user-data purge.

## Requirements and Gaps

- Windows 11 x64 and a standard user account.
- Optional telemetry requires its corresponding vendor driver/hardware;
  PresentMon and LibreHardwareMonitor are not redistributed.
- Discord features require Discord Desktop, a developer application/tester
  setup, user consent, and network access for token exchange/refresh.
- Packages are not code-signed; verify supplied SHA-256 manifests.
- Final physical G13 display/buttons/reconnect/endurance and live Discord voice
  acceptance remain operator gates. See `docs/HARDWARE-ACCEPTANCE.md`.

## Upgrade and Removal

Run `Install.ps1` from the extracted installer ZIP. Updates preserve the live
configuration byte-for-byte. `Uninstall.ps1` preserves configuration and
`%LOCALAPPDATA%\LCDForge2`; explicit purge requires the documented confirmation
token. Both operations refuse a running installed executable.
