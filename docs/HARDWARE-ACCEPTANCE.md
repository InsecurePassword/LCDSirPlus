# Final Hardware Acceptance Checklist

Record the tested source commit, release executable SHA-256, package SHA-256,
Windows build, device/driver versions, date, operator, and evidence location.
Any executable or package byte change invalidates all evidence below and
requires a complete rerun.

## Status as of 2026-08-31

- **No release:** LCDSirPlus 0.3.0 remains Draft/Unreleased. This block records
  current work-order status only and is not signed acceptance evidence.
- **Displays 1-3:** physically accepted in the current development session.
  Those observations are not bound to a final release executable/package and
  must be repeated against the final package bytes.
- **Inno lifecycle:** current-user and all-users lifecycle runs passed against
  setup SHA-256 `9ff678...` (the operator-provided abbreviated value; no full
  digest is asserted here). Later package-byte changes require both lifecycle
  modes to be rerun against the final setup.
- **PresentMon:** implemented, software-tested, pinned, and enabled by default,
  but live game capture is not release-qualified. The live run is deferred
  because this PC's memory is occupied by the local LLM; release remains pending
  this gate.
- **Discord:** implemented and software-tested, but live voice/OAuth workflow is
  pending. Each user creates and registers their own Discord application, and
  tokens remain current-user DPAPI-protected local data. Release remains pending
  unless this live gate is explicitly deferred.
- **Guarded termination:** disabled by default and unqualified until the
  disposable-child physical-button gate below passes.

## Preconditions

- Verify the setup EXE and portable ZIP against `LCDSirPlus-0.3.0-SHA256SUMS.txt`.
  Also verify the portable ZIP's internal `PACKAGE-MANIFEST.txt` covers
  `SOURCE-COMMIT.txt`; record its exact 40-lowercase-hex identity and the hashes.
- Use a standard unelevated Windows 11 x64 account.
- Back up the active configuration: installed
  `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`, or adjacent
  `lcdsirplus.txt` for portable/development use. Ensure no unsaved work is used
  for hang-action tests.
- Record optional NVAPI/ADLX, PresentMon, LHM, headset, controller, and Discord
  prerequisites actually present. Absence is acceptable only when the UI shows
  explicit unavailable state.

Run both lifecycle qualifications against one unchanged final setup artifact:

```powershell
$setup = (Resolve-Path .\LCDSirPlus-0.3.0-win-x64-setup.exe).Path
$setupSha = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
$sourceIdentity = (& tar.exe -xOf .\LCDSirPlus-0.3.0-win-x64-portable.zip `
  LCDSirPlus-0.3.0-win-x64-portable/SOURCE-COMMIT.txt).Trim()
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode CurrentUserLifecycle `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
# Repeat from elevated PowerShell on the all-users disposable VM.
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode AllUsersQualification `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
```

Archive both transcripts and retained `qualification-evidence.txt` files. Their
reported setup SHA-256 and source identity must be identical; copying or
rebuilding setup between runs invalidates both results.

## Logitech G13

- Exit Logitech Gaming Software normally and verify `LCore.exe` is no longer
  running before direct-HID testing. If present, stop only Logitech LampArray
  service, which can exclusively own the G13; do not stop unrelated G HUB
  services.
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
- Confirm STEP 07 names the selected `HID` or `SDK` transport. A failure during
  the STEP 10 remainder or final blank/shutdown must produce a failed verdict.
- Start normal mode and verify all four slot buttons, alert acknowledgement,
  preview fallback, tray controls, orientation, and inversion.
- Unplug/replug during normal operation; confirm bounded reconnect, preview
  continuity, no duplicate owner, and recovery without restart.
- Run for at least two hours, including sleep/wake if supported; record memory,
  CPU, log sizes/rotation, frame continuity, and any stale/unavailable states.

## PresentMon executable acceptance

This final-package gate is currently **PENDING/DEFERRED** because the local LLM
occupies the memory needed for a representative game run. The deferral is not a
pass: release acceptance remains blocked until every item below passes against
one unchanged final package.

- Bind the run to the final portable ZIP SHA-256 from
  `LCDSirPlus-0.3.0-SHA256SUMS.txt`, its `SOURCE-COMMIT.txt`, and the extracted
  `LCDSirPlus.exe` SHA-256. Verify colocated `PresentMon.exe` is exactly 956768
  bytes, SHA-256
  `9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191`,
  and has a valid `Intel Corporation` Authenticode signer:

```powershell
$package = (Resolve-Path .\LCDSirPlus-0.3.0-win-x64-portable.zip).Path
$root = (Resolve-Path .\LCDSirPlus-0.3.0-win-x64-portable).Path
$pm = Join-Path $root 'PresentMon.exe'
Get-FileHash -LiteralPath $package,$pm -Algorithm SHA256
Get-Item -LiteralPath $pm | Select-Object FullName,Length
Get-AuthenticodeSignature -FilePath $pm |
  Select-Object Status,@{n='Signer';e={$_.SignerCertificate.GetNameInfo('SimpleName',$false)}}
```

- Use a standard unelevated Windows 11 x64 account. Record membership in
  **Performance Log Users**, sign out/in after changing membership, and record
  whether a separate elevated troubleshooting run was required. Do not install
  a PresentMon service, MSI, driver, GUI, or SDK. Before each run, record
  existing `PresentMon` PIDs and do not stop unrelated instances. Use a copied
  acceptance configuration with `presentmon_enabled 1`, `presentmon_path auto`,
  `log_level debug`, and a new evidence directory selected with
  `--diagnostic-dir`.
- Test `process_name` first. Set `presentmon_target_mode process_name` and
  `presentmon_process_name` to the exact executable filename of a known
  presenting game, start that game, then run the final packaged
  `LCDSirPlus.exe --config <acceptance-config> --diagnostic-dir <evidence-dir>`.
  Record the LCD/preview and the single child `PresentMon.exe` PID. Cycle through
  `FPS_CURRENT`, `FPS_1LOW`, `FPS_01LOW`, `FRAME_TIME`, `SESSION_TIME`,
  `SESSION_SUMMARY`, and `GAME_NAME`; confirm plausible FPS/lows/frame time,
  advancing session time, accumulated stutter count, and the exact selected game.
- Then set `presentmon_target_mode foreground`, clear
  `presentmon_process_name`, and repeat after focusing the game. Alt-Tab between
  two presenting processes and an excluded shell process. Confirm target/game
  changes start a new session, excluded/no-target state does not capture the
  shell, and no more than one LCDSirPlus-owned PresentMon child exists.
- Stop rendering without exiting for at least five seconds and confirm metrics
  become `STALE`; continue beyond ten seconds and confirm they expire. Resume
  and confirm recovery. Change target and PresentMon settings while capturing,
  then close the target and exit LCDSirPlus from the tray. Each old child PID
  must exit promptly, no stale session data may cross the target boundary, all
  LCDSirPlus-owned children must be gone after shutdown, and every unrelated
  baseline PresentMon PID must remain untouched.
- Exercise and retain executable debug-log evidence for all four failure
  diagnostics in a controlled account/policy environment: `PresentMon produced
  no CSV header in 45 seconds`; `PresentMon produced a CSV header but no matching
  frames ... in 45 seconds` (a selected non-presenting process is suitable);
  `PresentMon exited (...) while capturing ...`; and an early-exit/timeout line
  with bounded sanitized `; stderr:` detail. The UI must show unavailable rather
  than stale valid data, retry must remain bounded, and no child may survive the
  failure. Do not replace the pinned binary or weaken release-tree permissions
  to inject these faults.
- Mark every case pass/fail with timestamped screenshots/video, private debug
  logs, package/executable hashes, source identity, account/elevation state,
  target names, child PID timeline, and failure-diagnostic excerpts. Review for
  private paths before sharing. Include this signed record in the final gate;
  any missing case, unexpected child, hash change, or unexplained metric is a
  release failure.

## Discord

- Use a dedicated developer application/test account. No portal redirect is
  required. Never record the client secret or DPAPI token bytes.
- Authorize with Discord Desktop running; confirm no listener/browser is opened
  and clear the client-secret environment variable. Switch to a second dedicated
  test account, authorize it once, and confirm switching either direction uses
  only that account's voice session without another authorization.
- Join a voice channel with another participant. Confirm channel label,
  self/other speaking order, mute/deafen transitions, linger, leave/rejoin,
  Discord restart, LCDSirPlus restart, and token refresh.
- Revoke authorization and run `--discord-clear-token`; confirm all LCDSirPlus
  Discord account records are removed and the overlay becomes unavailable
  without leaking remote error content.

## Guarded Hung Action

- Explicitly set `hang_enabled 1` only for this disposable test. The shipped
  default is `0`; `PROC_HANG` otherwise remains selected in its slot, its
  provider reports disabled/unavailable, and the no-target pane falls through.
- Use only `LCDSirPlus.exe --hang-test-harness`; never use an application with
  unsaved data.
- Confirm a bound short release navigates target detail/selection without
  terminating; with no bound target it performs ordinary slot cycling. Confirm
  target/identity/recovery/provider/config/safe-mode/slot/device changes cancel,
  a qualified continuous physical hold terminates the disposable child
  automatically at `hang_hold_ms`, and release afterward only resets without a
  second action. Confirm an app-loop resume after the maximum press duration is
  refused as stale rather than firing a delayed action.
- Record positive and negative harness results. Do not treat software unit tests
  as physical-button action evidence.

## Acceptance

- Attach logs only after private review; diagnostics ZIP is preferred and raw
  logs/configuration must not be shared.
- Mark each item pass/fail/not-present with evidence. Release acceptance requires
  all present-hardware checks and both G13 and Discord gates applicable to the
  release claim. Sign and date the record.
