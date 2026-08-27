# Final Hardware Acceptance Checklist

Record the tested source commit, release executable SHA-256, package SHA-256,
Windows build, device/driver versions, date, operator, and evidence location.
Any executable or package byte change invalidates all evidence below and
requires a complete rerun.

## Preconditions

- Verify the installer/portable ZIP against `LCDSirPlus-0.3.0-SHA256SUMS.txt` and its internal
  `PACKAGE-MANIFEST.txt`; record both hashes.
- Use a standard unelevated Windows 11 x64 account.
- Back up `lcdsirplus.txt`; ensure no unsaved work is used for hang-action tests.
- Record optional NVAPI/ADLX, PresentMon, LHM, headset, controller, and Discord
  prerequisites actually present. Absence is acceptable only when the UI shows
  explicit unavailable state.

## Logitech G13

- Exit Logitech Gaming Software normally and verify `LCore.exe` is no longer
  running before direct-HID testing. Do not stop unrelated G HUB services.
- Run `LCDSirPlus.exe --hardware-discover`; record the accepted VID/PID, usage,
  input/output report lengths, and rejection summary. Device paths are private
  and must not be published.
- Run `LCDSirPlus.exe --hardware-test --backend hid --duration-secs 60` and
  visually confirm STEP 01-10, correct geometry, no tearing or full-screen
  disappear/return flicker, and blank-on-close.
- Press each physical LCD button during the test and record down/release events.
- Start Logitech Gaming Software normally, verify signed `LCore.exe` is running,
  then run `LCDSirPlus.exe --hardware-test --backend sdk --duration-secs 60`.
  Confirm STEP 01-10, all four buttons, and blank-on-close. Do not copy or load
  any SDK DLL outside its canonical LCore installation.
- In normal `auto` mode, start and exit LGS normally. Confirm one clean
  HID-to-SDK and SDK-to-HID transition with no overlap or flicker.
- Start normal mode and verify all four slot buttons, alert acknowledgement,
  preview fallback, tray controls, orientation, and inversion.
- Unplug/replug during normal operation; confirm bounded reconnect, preview
  continuity, no duplicate owner, and recovery without restart.
- Run for at least two hours, including sleep/wake if supported; record memory,
  CPU, log sizes/rotation, frame continuity, and any stale/unavailable states.

## Discord

- Use a dedicated developer application/test account and register the configured
  redirect URI. Never record the client secret or DPAPI token bytes.
- Authorize with Discord Desktop running; confirm no listener/browser is opened
  and clear the temporary secret environment variable.
- Join a voice channel with another participant. Confirm channel label,
  self/other speaking order, mute/deafen transitions, linger, leave/rejoin,
  Discord restart, LCDSirPlus restart, and token refresh.
- Revoke authorization and run `--discord-clear-token`; confirm the overlay
  becomes unavailable without leaking remote error content.

## Guarded Hung Action

- Use only `LCDSirPlus.exe --hang-test-harness`; never use an application with
  unsaved data.
- Confirm short press only cycles, early release does not terminate, target/
  identity/recovery/config/safe-mode/device changes cancel, and only a completed
  continuous physical hold followed by release terminates the disposable child.
- Record positive and negative harness results. Do not treat software unit tests
  as physical-button authorization evidence.

## Acceptance

- Attach logs only after private review; diagnostics ZIP is preferred and raw
  logs/configuration must not be shared.
- Mark each item pass/fail/not-present with evidence. Release acceptance requires
  all present-hardware checks and both G13 and Discord gates applicable to the
  release claim. Sign and date the record.
