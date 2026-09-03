# Security

## Supported versions

LCDSirPlus 0.3.0 is a draft, unpublished release. The planned package has not
completed final package testing. Older b7 builds are not final 0.3.0 builds.

## Report a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/InsecurePassword/LCDSirPlus/security/advisories/new).
Include the affected version, reproduction steps, impact, and whether Discord
credentials or the hung-window action are involved. Never submit a live secret,
token, private log, or unreviewed diagnostic bundle.

If private reporting is unavailable, open a
[public issue](https://github.com/InsecurePassword/LCDSirPlus/issues/new) with no
sensitive details and request a private contact method. Allow time for a fix
before disclosure.

## Protect secrets and tokens

The numeric `discord_client_id` is public. Discord client secrets, access
tokens, and refresh tokens are not. Never place them in source, settings,
arguments, logs, screenshots, issues, chat, or diagnostics.

Use only the masked PowerShell 5/7 workflow in the
[instruction manual](docs/INSTRUCTION-MANUAL.md#connect-discord). It removes the
temporary `LCDSIRPLUS_DISCORD_CLIENT_SECRET` environment value in a `finally`
block. Authorization requests exactly the `rpc`, `identify`, and
`rpc.voice.read` scopes. Windows protects stored records for the current Windows
account. To disconnect, run the explicit `--discord-clear-token` command and
separately revoke the app under Discord **User Settings > Authorized Apps**.

## Limit data exposure

- PresentMon runs only while frame capture is enabled, and LCDSirPlus stops only
  the child it starts. With `presentmon_persist 0`, a valid NVIDIA App game
  catalog rejects ordinary desktop apps; if that catalog is unavailable,
  automatic fallback may still select another app. Setting it to `1` permits
  desktop apps even with a valid catalog. A selected executable filename and
  FPS may appear, and old readings are not preserved. Set
  `presentmon_enabled 0` if that exposure is unacceptable.
- NVIDIA catalog parsing is local and read only; catalog JSON, names, paths, and
  record details are not logged. The fingerprint field remains required and
  boolean-typed, but a stale false value is tolerated only for an otherwise-safe
  eligible exact path matching the running executable. All other qualification,
  identity, exclusion, workload, staleness, and cleanup gates remain in force.
- If Windows denies PresentMon capture, add the user to **Performance Log
  Users**, sign out, and sign in. Elevation is for diagnosis, not normal use.
- HWiNFO access is read only. LibreHardwareMonitor is restricted to the local
  computer; do not expose its web server to other computers.
- The optional network quality probe is off by default. Enabling it sends ICMP
  or TCP traffic to the configured IP address. Discord authorization and refresh
  use outgoing HTTPS.

## Use diagnostics and hardware tests safely

Diagnostics is a bounded offline summary and does not collect active provider
state. It ignores `--config`, starts no HID, provider, Discord, network, or
hung-action work, and excludes raw logs, settings, credentials, identifiers,
content, process/window names, paths, addresses, targets, environment values,
command lines, registry values, serial numbers, directory listings, and device
paths. Review the ZIP before sharing it; review raw logs privately and
separately.

`--hardware-discover` reports HID compatibility without raw device paths or
writes and does not inspect running processes. A direct-HID hardware test writes
to the G13 and refuses to run if another LCDSirPlus process is running, either
competing Logitech process is running, or running-process enumeration fails.
Prefer Logitech Gaming Software and follow the bounded LampArray procedure in the
[instruction manual](docs/INSTRUCTION-MANUAL.md#check-a-logitech-g13).

## Avoid destructive actions

Run LCDSirPlus as a standard Windows user. Keep `hang_enabled 0`; the
hung-window action can terminate a program and lose unsaved work, its physical
behavior has not completed release testing, and there is no supported end-user
test.

Safe mode limits the dashboard to the clock, built-in CPU load, memory, preview,
slot cycling, and alert acknowledgement. It disables optional dashboard data,
Discord dashboard access, probes, startup changes, and the destructive action.
The explicit Discord authorize and clear commands remain available.

The planned LCDSirPlus packages are unsigned. Verify their supplied SHA-256
checksums before use; planned PresentMon content has its own Intel signature.
Uninstall preserves `%LOCALAPPDATA%\LCDSirPlus`, including settings, logs, and
Discord credentials. Remove that folder only when its data is no longer needed.
