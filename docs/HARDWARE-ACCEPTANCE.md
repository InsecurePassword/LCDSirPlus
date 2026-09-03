# Hardware and Release Acceptance

Audience: maintainers qualifying final package bytes on disposable Windows 11
x64 systems and physical hardware. This source-only ledger is not included in
the installer or portable package. Build instructions are in
[DEVELOPMENT.md](DEVELOPMENT.md); trust boundaries are in
[ARCHITECTURE.md](ARCHITECTURE.md). Record the source commit, checksums, Windows
build, hardware/driver versions, date, and operator. Test the final downloads
again if any included file changes.

## Current status: 2026-09-01

| Area | Current evidence | Final package status |
|---|---|---|
| Release | 0.3.0 remains draft. Existing b7 downloads are older. | Pending. |
| Main layouts 1-3 | Observed during local development. | Test the final download. |
| Installer lifecycle | Earlier current-user/all-users runs used older downloads. | Test both modes with one final setup file. |
| PresentMon | Basic start, game detection, and stop behavior were observed during local development; this is not a final-package claim. | Test one final portable build. |
| Discord | Implementation and software tests exist. | Live voice/authorization check pending. |
| Hung action | Disabled by default. | Disposable-child physical-button check pending. |
| PDF manual | The Markdown manual changed; the tracked PDF is not durable final evidence. | After the final Markdown change, regenerate and validate the PDF, then package that exact file. Pending. |

Observed during local development; final downloadable package testing remains pending.
Do not publish a final release claim until the checks below use the
unchanged files selected for publication.

## Record package identity

- Verify setup and portable ZIP with `LCDSirPlus-0.3.0-SHA256SUMS.txt`.
- Verify portable `PACKAGE-MANIFEST.txt` and `SOURCE-COMMIT.txt`.
- Record the extracted `LCDSirPlus.exe` checksum.
- Verify adjacent `PresentMon.exe` is 956768 bytes, SHA-256
  `9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191`,
  and signed by `Intel Corporation`.
- Use the same unchanged setup file for current-user and all-users lifecycle
  checks.

```powershell
$setup = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-setup.exe).Path
$portable = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-portable.zip).Path
$setupSha = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
$sourceIdentity = (& tar.exe -xOf $portable `
  LCDSirPlus-0.3.0-win-x64-portable/SOURCE-COMMIT.txt).Trim()
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode CurrentUserLifecycle `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
```

From elevated PowerShell at the repository root on a disposable VM, run:

```powershell
$setup = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-setup.exe).Path
$portable = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-portable.zip).Path
$setupSha = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
$sourceIdentity = (& tar.exe -xOf $portable `
  LCDSirPlus-0.3.0-win-x64-portable/SOURCE-COMMIT.txt).Trim()
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode AllUsersQualification `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
```

Keep both transcripts and confirm their setup checksum and source identity
match.

## Prepare the final portable executable

From a standard unelevated PowerShell session at the repository root, extract
the unchanged final portable ZIP to a new private directory and bind every
executable check below to that copy:

```powershell
$portable = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-portable.zip).Path
$acceptanceRoot = Join-Path $env:TEMP "LCDSirPlus-0.3.0-acceptance-$PID"
Expand-Archive -LiteralPath $portable -DestinationPath $acceptanceRoot
$packageRoot = Join-Path $acceptanceRoot 'LCDSirPlus-0.3.0-win-x64-portable'
$exe = (Resolve-Path -LiteralPath (Join-Path $packageRoot 'LCDSirPlus.exe')).Path
$pm = (Resolve-Path -LiteralPath (Join-Path $packageRoot 'PresentMon.exe')).Path
Get-FileHash -LiteralPath $portable,$exe,$pm -Algorithm SHA256
Get-Item -LiteralPath $pm | Select-Object FullName,Length
Get-AuthenticodeSignature -FilePath $pm |
  Select-Object Status,@{n='Signer';e={$_.SignerCertificate.GetNameInfo('SimpleName',$false)}}
```

Retain `$exe` for the remaining commands. A rebuilt or re-extracted candidate
requires a new evidence record; do not substitute a Cargo or installed binary.

## Check the Logitech G13

Use a standard Windows 11 x64 account.

1. Exit Logitech Gaming Software. If and only if exact
   `logi_lamparray_service.AMD64.exe` is running, record the prior status and
   startup type of **Logitech LampArray Service**, stop that exact service for
   steps 2 through 6, and do not change its startup type. Do not stop unrelated
   G HUB services.
2. Run `& $exe --hardware-discover`. Confirm it scans at most 256 HID interfaces
   until the first exact G13 match, reports that candidate plus preceding
   rejection reasons, omits device paths, performs no writes, and does not
   inspect competing processes. Discovery cannot establish ownership.
3. Run `& $exe --hardware-test --backend hid --duration-secs 60`.
   Confirm direct HID requires both exact `LCore.exe` and
   `logi_lamparray_service.AMD64.exe` to be absent and fails closed if process
   enumeration fails.
4. Confirm steps 1 through 10, geometry, no tearing/flicker, all four button
   down/release events, and blank-on-close.
5. Start signed Logitech Gaming Software normally, confirm validated exact
   `LCore.exe` is present, and run the same test with
   `& $exe --hardware-test --backend sdk --duration-secs 60`.
6. In normal `auto` mode, confirm it selects SDK when validated exact
   `LCore.exe` is present, plus one clean HID-to-SDK and SDK-to-HID change.
7. If step 1 stopped **Logitech LampArray Service**, restore its recorded prior
   state and confirm its startup type is unchanged.
8. Check all four slot buttons, button 4 alert acknowledgement, tray and preview,
   orientation, inversion, unplug/replug recovery, and no duplicate display
   output.
9. Run for two hours, including sleep/wake when supported. Record CPU, memory,
   log rotation, frame continuity, and unavailable/stale states.

Each hardware-test step normally lasts two seconds. A 60-second test therefore
runs all ten steps and leaves the remainder on step 10's live dashboard. The
two-hour check is a separate normal-dashboard soak, not a longer hardware test.

## Check PresentMon

Use one copied settings file and a private evidence directory. Do not install a
PresentMon service, MSI, GUI, or SDK. Record unrelated PresentMon processes and
do not stop them.

1. Start with `presentmon_enabled 1`, `presentmon_path auto`,
   `presentmon_target_mode presenting`, `presentmon_deferred 1`,
   `presentmon_persist 0`, and `log_level debug`.
2. Use a valid synthetic NVIDIA App catalog case whose noncreative,
   OPS-supported, nonmanual exact safe path has `IsFingerprintDetected` set to
   `false`, plus a higher-workload ordinary desktop presenter. Confirm the exact
   catalog path is selected and the desktop presenter is rejected. Repeat with
   the fingerprint value set to `true`.
3. Test at least two representative games independently from closed state to
   active capture and back to closed state. Record one LCDSirPlus-owned
   PresentMon child, FPS,
   low FPS, frame time, session time, stutters, game name, and cleanup.
4. Use exact test line `slot_2 PROC_HANG FPS_CURRENT GPU_TEMP`. Confirm stopped
   game display moves from unavailable FPS to GPU temperature without changing
   the stored `FPS_CURRENT` selection.
5. Test every PresentMon value with `presentmon_deferred 1`. Confirm the next
   eligible option or `CLEAR`. Repeat with `0` and confirm native inactive text.
6. Make the NVIDIA catalog unavailable without editing it. Confirm automatic
   workload selection still works. Restore a valid catalog and confirm ordinary
   desktop presenters are rejected.
7. Set `presentmon_persist 1`. Confirm a sustained ordinary presenter can be
   selected and its executable name/FPS can appear. Confirm exclusions,
   identity changes, switching, stale expiry, settings reset, and one-child
   ownership still hold. Restore `0`.
8. Test `process_name` and `foreground` separately. Confirm each targeted change
   starts a fresh session and never leaves more than one owned child.
9. Stop frames for five seconds and confirm stale display. Continue past ten
   seconds and confirm expiry. Resume and confirm recovery.
10. Exit LCDSirPlus. Confirm every owned child is gone and unrelated PresentMon
    processes remain.
11. Capture expected startup/no-frame/early-exit errors in controlled tests.
    Confirm unavailable display, delayed retry, sanitized detail, and no child
    leak.

If capture access is denied, record **Performance Log Users** membership and
whether sign-out/sign-in fixed it. Elevation is a diagnosis only. Keep names,
paths, and debug logs private.

## Check Discord

Use a dedicated application and test accounts. Never record the client secret or
credential bytes.

1. Authorize through the manual's masked environment workflow. Confirm a blank
   secret fails before Discord use and the environment value is removed.
2. Authorize two test accounts separately. Switch accounts and confirm only the
   active account's voice state appears.
3. Check channel title, self/other speakers, speaking order, mute/deafen,
   linger, leave/rejoin, Discord restart, LCDSirPlus restart, and refresh.
4. Revoke access and follow the manual's
   [disconnect instructions](INSTRUCTION-MANUAL.md#disconnect-discord). Confirm
   local credentials are removed and no private remote error text appears.

## Check the guarded hung action

Use no valuable or unsaved work.

1. Set `hang_enabled 1` only in a copied test file.
2. Start only `& $exe --hang-test-harness --duration-secs 120` as the target.
3. Confirm short release changes detail/target without termination.
4. Confirm target, identity, recovery, data-source, settings, safe-mode, slot,
   and device changes cancel a held action.
5. Confirm a continuous physical hold terminates only the disposable child at
   `hang_hold_ms` and release cannot trigger a second action.
6. Confirm a delayed app-loop resume refuses the old hold.
7. Restore `hang_enabled 0`.

Software tests do not replace this physical-button check.

## Run internal smoke modes

These hidden maintainer modes belong only in this source-only ledger. Run them
from the final portable package in a disposable session and retain exit codes
and output after the harness check above; they supplement rather than replace
the live checks:

```powershell
& $exe --hang-detector-smoke
& $exe --hang-action-smoke
& $exe --hang-action-negative-smoke
& $exe --instance-smoke
```

`--instance-smoke` invokes the internal `--instance-smoke-child`; do not invoke
the child directly or record it as an independent gate. The hang modes create
only disposable targets, but still require no valuable or unsaved work.

## Check installer and removal

- Confirm current-user and all-users destinations and registration.
- Confirm default Start Menu/sign-in choices and optional desktop shortcut.
- Confirm opposite-scope, legacy install, and foreign startup-task collisions
  are refused without changing existing data.
- Confirm same-scope update and cached Modify repair restore installed files.
- Confirm cancellation leaves the startup task and files intact.
- Confirm uninstall removes installed files, shortcuts, and the owned task.
- Confirm `%LOCALAPPDATA%\LCDSirPlus` settings, logs, and credentials are
  preserved.

## Sign off

Review diagnostics and logs for private data before sharing. Mark each item
pass, fail, or not present with evidence. Final acceptance requires all claimed
hardware/integration checks against one unchanged final package set. Publication
also requires the packaged PDF to be the validated tracked PDF generated after
the last Markdown manual change; any later manual or PDF byte change reopens
that gate.
