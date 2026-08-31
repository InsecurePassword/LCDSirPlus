# LCDSirPlus 0.3.0 Instruction Manual

**Draft / Unreleased:** 0.3.0 release acceptance is pending. Installer and
portable-package procedures apply only if those artifacts are published.

LCDSirPlus is a Windows 11 x64 dashboard for the Logitech G13 160x43
monochrome LCD. It runs as a standard user, provides a virtual preview, and
uses explicit `N/A`, stale, offline, or disabled states when optional telemetry
is unavailable.

## Contents

- [Requirements and dependencies](#requirements-and-dependencies)
- [Install or run portable](#install-or-run-portable)
- [First start](#first-start)
- [Slots and buttons](#slots-and-buttons)
- [Main displays](#main-displays)
- [Configuration and preview](#configuration-and-preview)
- [Providers](#providers)
- [Network rates and quality](#network-rates-and-quality)
- [Discord setup](#discord-setup)
- [Safe mode and hung-window action](#safe-mode-and-hung-window-action)
- [Command line and exit codes](#command-line-and-exit-codes)
- [Troubleshooting](#troubleshooting)
- [Logs, diagnostics, and privacy](#logs-diagnostics-and-privacy)
- [Update and uninstall](#update-and-uninstall)
- [Slot option reference](#slot-option-reference)

## Requirements and dependencies

Required:

- Windows 11 x64 and a standard, unelevated user account.
- A Logitech G13 only for the physical LCD and buttons. The built-in virtual
  preview works without one.
- A writable configuration location: installed copies use
  `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; portable/development copies
  use `lcdsirplus.txt` beside `LCDSirPlus.exe`.

The main application is one native executable. Published release packages are designed to include
the official signed PresentMon v2.5.1 console for frame timing; LCDSirPlus does
not install a runtime, service, driver, listener, browser extension, PresentMon
MSI/service, or elevated component. LCDSirPlus packages are not code signed, so
verify their SHA-256 checksums before use. Package-level dependency notices are
in `THIRD_PARTY_LICENSES.txt`; PresentMon's notices remain under
`licenses/PresentMon/`.

Optional features have separate dependencies:

| Feature | Dependency |
|---|---|
| G13 through Logitech software | Installed, running Logitech Gaming Software (`LCore.exe`) with its trusted LCD SDK. `auto` otherwise attempts direct HID, which also requires Logitech LampArray service to release the G13. |
| GPU load, VRAM, temperature | NVIDIA or AMD display driver exposing NVAPI or ADLX. |
| CPU temperature | Preferred: user-managed HWiNFO Sensors with Shared Memory Support and an exact configured label pair. Optional fallback: LibreHardwareMonitor with its loopback web server explicitly enabled by the user. |
| FPS, frame time, game name, and sessions | Bundled official signed PresentMon v2.5.1 console, `PresentMon.exe`; plain Cargo output does not bundle it. |
| Headset battery | Supported SteelSeries Arctis 1, 7X/7P, 9, Pro Wireless, 7+/7P+, Nova 3/5/7, or GameBuds wireless USB receiver family. |
| Controller battery | Connected XInput controller. |
| Discord speaker overlay | Discord Desktop, a Discord developer application, tester access when required, and one authorization per Discord account. |
| Ping, jitter, and packet loss | An explicitly enabled ICMP/TCP probe to one configured IP literal. |

## Install or run portable

### Verify a release

Place the checksum file beside the downloaded artifacts and compare the
published values with the local files:

```powershell
Get-Content .\LCDSirPlus-0.3.0-SHA256SUMS.txt
Get-FileHash .\LCDSirPlus-0.3.0-* -Algorithm SHA256
```

Each ZIP also contains a sorted `PACKAGE-MANIFEST.txt` listing every member's
SHA-256 and byte size. The setup EXE is covered by the outer checksum file.

### Installer package

If published, run `LCDSirPlus-0.3.0-win-x64-setup.exe`. Current-user mode installs to
`%LOCALAPPDATA%\Programs\LCDSirPlus`. The mode dialog can select an elevated
all-users install to `%ProgramFiles%\LCDSirPlus`. Start Menu and per-user sign-in
startup tasks are checked by default; the desktop shortcut is unchecked. The
startup integration is an interactive, limited scheduled task, not an HKCU Run
value or service. Its stable name includes the owning Windows account SID, so
different accounts do not share one task identity.
Setup installs a default configuration template. On first installed launch, the
application validates and atomically seeds
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt` only if that user file is
absent; update and repair do not replace it.

### Portable package

Extract the portable ZIP to a local directory and run `LCDSirPlus.exe` there.
Keep `lcdsirplus.txt` beside it. Portable use makes no installer-owned shortcut.
The package includes [`modules.md`](../modules.md) as a simple slot-value guide.

## First start

1. Validate the shipped configuration:

   ```powershell
   .\LCDSirPlus.exe --validate-config
   ```

2. Start with a forced preview so the dashboard can be checked without taking
   the G13 display:

   ```powershell
   .\LCDSirPlus.exe --preview
   ```

   Preview is a long-running normal application mode and owns the per-session
   LCDSirPlus mutex. Close it before a nonvirtual hardware test. The tray icon
   remains available whenever the application runs; the preview title-bar X
   exits the application rather than merely hiding the window.

3. If using a physical G13, inspect it without writing to the LCD:

   ```powershell
   .\LCDSirPlus.exe --hardware-discover
   ```

4. Exit Logitech Gaming Software normally before a direct-HID test, then run:

   ```powershell
   .\LCDSirPlus.exe --hardware-test --backend hid --duration-secs 60
   ```

5. For the SDK path, start Logitech Gaming Software normally and run:

   ```powershell
   .\LCDSirPlus.exe --hardware-test --backend sdk --duration-secs 60
   ```

Watch the physical LCD and press all four LCD buttons during hardware tests.
Command success proves transport submission, not physical display quality.

Normal `logitech_backend auto` selects the trusted Logitech SDK while exact
process `LCore.exe` owns the G13 and otherwise attempts direct HID. Direct HID
also refuses Logitech LampArray when that service owns the device. Explicit
`sdk` and `hid` modes do not cross-fallback. LCDSirPlus never stops Logitech
software for you.

## Slots and buttons

The lower row has four slots aligned with the four physical LCD buttons. Each
`slot_N` configuration line is an ordered list. A short press cycles only its
matching slot and wraps at the end:

```text
slot_0  HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY
slot_1  FPS_CURRENT FPS_1LOW FRAME_TIME SESSION_TIME
slot_2  PROC_HANG GPU_TEMP PING JITTER AUDIO
slot_3  THERMALS PACKET_LOSS MIC_STATUS SESSION_SUMMARY PROVIDER_STATUS
```

In the preview, left-click a slot to cycle forward and right-click it to cycle
backward. The tray icon always exists while the application runs: left-click it
to toggle the preview, or right-click it for Toggle Preview and Exit. Closing
the preview with its title-bar X exits LCDSirPlus.

Physical button 4 or preview slot 4 first acknowledges the highest active alert
before performing its normal slot action. When the guarded hung-window UI is
active, its selected slot's physical button has special hold/release behavior
described below.

Every graph shows exactly the trailing 30 seconds. Temperature and memory
warnings are enabled independently. With temperature warnings enabled and
`warning 1`, only the selected CPU/GPU temperature pane inverts in alternating
100 ms phases at its configured maximum; CPU/GPU temperature alert episodes do
not use full-screen overlays. With `warning 0`, those temperature overlays
remain. Memory and headset alerts are unaffected by this presentation setting.

## Main displays

The compact date/time header and fixed four-button bottom row stay the same.
Select one of three fixed built-in metric layouts:

- `main_display 1` (default): original CPU/RAM and GPU/VRAM halves.
- `main_display 2`: CPU/RAM, GPU/VRAM, and OUT/IN thirds.
- `main_display 3`: original CPU/RAM on the left and NET IN/NET OUT on the
  right.

The value is an integer and only `1`, `2`, or `3` is valid. Saving a valid
change applies it on hot reload. The OUT/IN bars show current throughput using
`network_graph_ceiling_mbps`; with its default `1000` Mbps ceiling, 1 Gbps
fills a bar. Layout 1 has no network bars.

## Configuration and preview

The text format uses whitespace-separated keys and values, `#` comments, and
quoted values for embedded spaces. The executable watches the primary file and
hot-reloads valid changes. Tokenization and parse errors identify their source
file and line. Final range and cross-field validation errors may report line `0`
because no exact source line is retained for those checks. The last valid
configuration remains active after any invalid edit.

Installed copies default to
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; portable/development copies
use the adjacent file. Cargo copies the canonical root configuration beside
debug and release executables. An explicit `--config PATH` has precedence for
normal runtime, validation, hardware tests, and Discord
authorization/credential removal, as detailed under Command line.

Validate before or after editing:

```powershell
.\LCDSirPlus.exe --validate-config
.\LCDSirPlus.exe --validate-config --config C:\path\lcdsirplus.txt
```

The complete key, default, and range reference is in
[CONFIGURATION.md](CONFIGURATION.md). Important initial settings are:

| Key | Purpose |
|---|---|
| `main_display 1` | Select fixed built-in layout 1, 2, or 3; applies on valid hot reload. |
| `temperature_warning_enabled 1` | Enable CPU/GPU temperature episodes and their selected presentation. |
| `memory_warning_enabled 1` | Enable RAM/VRAM capacity episodes independently. |
| `memory_warning 100` | RAM warning percentage, inclusive at `>=`; `0` is the compatibility disable value. |
| `vmem_warning 100` | VRAM warning percentage, inclusive at `>=`; `0` is the compatibility disable value. |
| `warning 1` | With temperature warnings enabled, flash selected panes and suppress temperature full-screen overlays; `0` retains overlays. |
| `preview_mode auto` | Show preview when a physical HID/SDK backend is unavailable. `always` starts it visible; `never` starts it hidden. |
| `preview_scale 4` | Preview scale, accepted range 1 through 10. |
| `start_minimized 0` | Suppress automatic preview display when set to 1. |
| `start_at_login 0` | In portable mode, own the current user's exact LCDSirPlus Run value when set to 1. Installed startup is selected in setup instead. Safe mode never changes it. |
| `logitech_backend auto` | Select trusted SDK with LCore, otherwise direct HID. `virtual` is preview-only. |
| `hang_enabled 0` | Keep hung-window detection and destructive termination disabled until explicit opt-in and physical disposable-child qualification. |
| `safe_mode 0` | Disable providers, Discord, probes, startup mutation, and the destructive hung action. |

`preview_scale`, `log_level`, `log_max_bytes`, and `log_backups` are validated
during hot reload but take effect only after restart. `date_format` and
`time_format` are accepted but currently do not change the shared header.
`start_minimized` describes automatic-start preview behavior.

The two warning category switches apply on valid hot reload. Disabling
temperature warnings prevents CPU/GPU episodes and pane flashing without
affecting memory or headset alerts. Disabling memory warnings prevents RAM/VRAM
episodes without affecting temperature or headset alerts. Stale and unavailable
readings never trigger. Use the category switch instead of zero thresholds for
new configurations; installer updates preserve existing explicit thresholds
and do not rewrite them. Existing episodes remain through unknown or stale
readings. A first current valid recovery removes severity-2 warnings immediately
and starts `critical_alert_linger_ms` only for severity-3 episodes. Disabling a
warning category clears its episodes; disabling the headset provider clears its
episode.

An include file can hold machine-local overrides in source or portable use:

```text
include lcdsirplus.local.txt
```

Relative includes cannot use `..`, absolute paths, or environment expansion.
Later included values override earlier files; duplicate keys in one file are
rejected. Installed relative includes belong beside the user configuration
under `%LOCALAPPDATA%\LCDSirPlus\Config`, which setup preserves.

## Providers

### Built-in Windows sources

- Local date/time uses the shared fixed Windows date/time header layout.
- CPU load uses native scheduler accounting and CCD topology detection.
- RAM percentage uses `GlobalMemoryStatusEx`.
- Network throughput uses native interface counters, excluding loopback and
  interfaces that are down.
- Audio volume and microphone mute use the default Windows Core Audio endpoints.
- Controller battery uses XInput.
- Aggregate disk I/O and approximate page-read pressure use Windows PDH.
- Established connections use native IPv4 and IPv6 TCP tables.
- AC/battery state uses `GetSystemPowerStatus`.

These sources require no downloaded helper. Native Win32 and installed vendor
APIs always take priority. HWiNFO or exact/automatic LibreHardwareMonitor
selection is used only where no safe native source exists. LCDSirPlus never
probes raw MSRs, SMBus, EC, or Super-I/O registers.

### GPU

`gpu_provider auto` tries the vendor-installed NVIDIA NVAPI provider and then
AMD ADLX. `nvapi` or `adlx` selects one explicitly; `off` disables both. Native
GPU telemetry can provide load, VRAM used/total, and temperature. LHM can only
fall back for temperature; it does not provide LCDSirPlus GPU load or VRAM.

### CPU temperature: HWiNFO and LibreHardwareMonitor

Current Windows has no generic CPU package-temperature API. `CPU_TEMP` can be
`N/A` because no source is configured, HWiNFO shared memory/LHM is stopped, the
configured label pair or SensorId is missing or ambiguous, a value has the
wrong type/unit, the data is stale, or the shared-memory ABI is unsupported.

The preferred fallback is user-managed HWiNFO:

1. Start HWiNFO in **Sensors-only** mode.
2. Enable **Shared Memory Support** in HWiNFO settings.
3. Run `LCDSirPlus.exe --list-hwinfo-sensors`.
4. Find the intended CPU package/control temperature line.
5. Copy its exact case-sensitive `sensor=` and `reading=` original labels into:

   ```text
   hwinfo_cpu_temp_sensor "<exact sensor original label>"
   hwinfo_cpu_temp_reading "<exact temperature original label>"
   ```

6. Validate configuration and start LCDSirPlus.

Both keys must be configured together. HWiNFO64 Free limits shared-memory
monitoring to 12 hours. For unattended use, use HWiNFO32 or an appropriately
licensed HWiNFO Pro edition according to HWiNFO's vendor terms. LCDSirPlus does
not bundle, start, configure, restart, or license HWiNFO. Access is read-only,
and configured original labels are exact and case-sensitive.

LibreHardwareMonitor is an optional fallback restricted to loopback HTTP. The
user must explicitly run it and safely enable its web server; LCDSirPlus does
not enable or manage that server and does not request a firewall exception. At
the default address, use:

```text
lhm_mode auto
lhm_url auto
```

`auto` resolves to `http://127.0.0.1:8085/data.json`. If automatic temperature
selection is wrong, copy stable SensorIds from that live loopback `data.json`
response, not from an LCDSirPlus diagnostics bundle, and set the overrides
documented in `CONFIGURATION.md`. LHM is user-managed and not bundled; its
absence is an acceptable unavailable state. The accepted `CPU_CACHE_TEMP` and
`CPU_FREQ_TEMP` options are also currently unavailable because no runtime
provider publishes the required per-domain temperatures.

### PresentMon

Published release packages are designed to bundle Intel's official signed PresentMon v2.5.1 console as
`PresentMon.exe`. Default `presentmon_path auto` uses that colocated file;
plain Cargo output does not bundle it. Advanced users can select a valid console
on a trusted fixed local drive explicitly. Default foreground
targeting follows the current foreground process. Process-name mode requires
`presentmon_process_name`.

LCDSirPlus launches and owns PresentMon only while an enabled target frame
capture is active and stops its own child on target/config change or shutdown.
It provides FPS, low FPS, frame time, game name, session duration, and stutter
count only; it does not provide CPU/GPU hardware telemetry. No PresentMon
service, MSI, GUI, or API/SDK is bundled or installed. Data becomes stale after
five seconds without frames and expires after another five.

If capture cannot start, add the user to Windows' **Performance Log Users**
group and sign out/in, or test elevation if local ETW policy requires it.
Elevation is troubleshooting, not a normal requirement. PresentMon is MIT
licensed; its exact notices are in `licenses/PresentMon/LICENSE.txt` and
`licenses/PresentMon/THIRD_PARTY.txt`, and upstream is
[GameTechDev/PresentMon](https://github.com/GameTechDev/PresentMon).

The provider is implemented, software-tested, pinned, and remains enabled by
default because it is a required feature. Live capture against an actively
presenting game is not yet release-qualified and is deferred while this PC's
memory is occupied by the local LLM. Release remains pending that live gate.

### Headset, controller, and audio

The headset provider supports an explicit set of SteelSeries Arctis 1, 7X/7P,
9, Pro Wireless, 7+/7P+, Nova 3/5/7 Wireless, and GameBuds USB receiver
families. It does not generically support every SteelSeries, Bluetooth, or wired
device. Connect the
wireless receiver through USB; Bluetooth alone does not expose the supported
control interface. SteelSeries GG may coexist, but LCDSirPlus reads the receiver
directly and does not require GG.

Battery values follow the hardware: direct or scaled percentages on some models
and 0/25/50/75/100% bands on others. GameBuds shows the lower battery percentage
of the active earbuds, not docked earbuds or the case. The title changes to
`CHARGING` only when that profile supplies a charging state; the value remains a
complete percentage. Exact receiver PIDs and profile details are in
[CONFIGURATION.md](CONFIGURATION.md#headset_battery).

XInput battery levels are the platform's coarse values. Core Audio reads the
current default multimedia endpoints. Disable an unneeded provider with its
`*_enabled 0` setting.

`PROVIDER_STATUS` summarizes providers currently tracked by the runtime. `OK`
means none of those tracked providers is marked failed; it does not mean every
optional provider is installed or enabled. Safe mode publishes only its healthy
`safe-mode` state, so the slot renders `OK` rather than counting disabled
Discord, hung-window, or other providers.

## Network rates and quality

Throughput and quality probing are independent:

- `NET_IN`, `NET_OUT`, and `NET_BOTH` use local interface counters and do not
  send network traffic. Numeric values are instantaneous decimal SI bit rates.
- `NET_IN_GRAPH` and `NET_OUT_GRAPH` show exactly the trailing 30 seconds in
  one-second bins.
- `NET_GRAPH` shows ingress above and egress below its center line for the same
  30-second history.
- `PING`, `JITTER`, and `PACKET_LOSS` require the optional active probe, which
  is off by default.

All network graph variants and the current OUT/IN bars in main displays 2 and 3
share `network_graph_ceiling_mbps`. Its default is `1000` and accepted range is
`1..100000` Mbps. At the default, 1 Gbps fills a bar. The ceiling does not
change numeric readings. Example for a 2.5 Gbps link:

```text
network_graph_ceiling_mbps 2500
```

To enable a bounded quality probe:

```text
network_probe_enabled 1
network_probe_method auto
network_probe_target 1.1.1.1
network_probe_interval_ms 1000
network_probe_timeout_ms 1500
network_probe_window 30
```

The target must be one IP literal, optionally with a TCP port. Hostnames are
rejected. `icmp` accepts IPv4 without a port. `tcp` accepts IPv4 or IPv6 and
defaults to port 443 when omitted. `auto` tries IPv4 ICMP then TCP within one
overall timeout; IPv6 goes directly to TCP. Safe mode and a disabled probe send
no probe traffic.

## Discord setup

Discord integration uses the trust-checked local Discord Desktop named pipe for
voice state and bounded HTTPS only for token exchange/refresh. It opens no
browser, callback listener, or portal redirect.

Running Discord alone is insufficient. `discord_client_id` must be the public
numeric Application ID/OAuth client ID from a Discord Developer Portal
application; it is not a secret. If the ID is missing, Discord is unconfigured
and no IPC connection is attempted, so this is not a connection failure.
LCDSirPlus reads voice state only: it publishes no Rich Presence and requires no
image assets.

The integration is implemented and software-tested, but its live voice/OAuth
workflow is not yet release-qualified. Each user creates and registers their own
Discord application, and tokens remain current-user DPAPI-protected local data.
Release remains pending this gate unless it is explicitly deferred.

1. Create a Discord developer application and add the account as an application
   tester if Discord requires it.
2. Put the numeric application ID in configuration:

   ```text
   discord_enabled 1
   discord_client_id 123456789012345678
   ```

3. Start Discord Desktop and activate the account to authorize.
4. Run authorization:

   ```powershell
   .\LCDSirPlus.exe --discord-authorize
   ```

5. If the application is a confidential client, expose its secret only for the
   authorization command, then remove it:

   ```powershell
   $env:LCDSIRPLUS_DISCORD_CLIENT_SECRET = '<secret>'
   .\LCDSirPlus.exe --discord-authorize
   Remove-Item Env:\LCDSIRPLUS_DISCORD_CLIENT_SECRET -ErrorAction SilentlyContinue
   ```

Never place a secret or token in configuration or command arguments. Each
Discord Account Switcher account must be active and authorized once. Credentials
are encrypted for the current Windows user with DPAPI and stored under that
Discord user ID in `%LOCALAPPDATA%\LCDSirPlus`.

Discord Desktop must continue running as the same Windows user and in the same
interactive session. LCDSirPlus accepts only a named-pipe server whose process,
session, fixed-local executable, Authenticode signature, and Discord publisher
identity pass its checks.

Optional overlay controls include `discord_linger_ms`,
`discord_max_speakers`, `discord_show_self`, and `discord_show_channel`. To
remove all LCDSirPlus Discord credential files:

```powershell
.\LCDSirPlus.exe --discord-clear-token
```

Revoke the application separately under Discord **User Settings > Authorized
Apps** when authorization should also be invalidated remotely. Local credential
removal and remote revocation are separate operations; do both after suspected
credential exposure.

## Safe mode and hung-window action

Start safe mode temporarily:

```powershell
.\LCDSirPlus.exe --safe-mode
```

or set `safe_mode 1`. Safe mode disables optional telemetry providers,
Discord access, network probes, hung-target binding/termination, and startup
registration changes. Clock, native CPU load, native RAM percentage, preview,
slot cycling, and alert acknowledgement remain harmless local functions.

The hung-window detector and destructive action are disabled by default. Set
`hang_enabled 1` only to opt in. Until then, `PROC_HANG` remains in the shipped
slot with the same button position and cannot bind or terminate a target. The
provider reports disabled/unavailable while the existing no-target pane shows
the next configured token without changing selection.

When enabled, the detector can expose repeatedly unresponsive windows.
`PROC_HANG` may occur only once across all slot lists. The token's selected slot
owns the matching physical button at runtime; legacy `hang_button` remains
accepted but does not choose that binding. With no target, the next active token
or `CLEAR` renders without changing selection. Button-down binds one exact
displayed target. A short release toggles details, then advances to the next
target when details are already shown; it does not terminate. A continuous hold
automatically requests termination at `hang_hold_ms` after revalidation; release
afterward only resets and cannot act again. If the app loop resumes only after
twice the configured hold, clamped to 3..15 seconds, the hold is refused as
stale. Recovery, target or selection change, device loss, provider failure,
reload, or safe mode cancels the action.
Safe mode leaves ordinary short-release navigation available because no target
is bound. Termination can lose unsaved work. Test only with the disposable
harness in [HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md), never valuable work.
The action remains unqualified until that disposable-child physical-button gate
passes; software tests do not qualify termination for release.

`BOTTLENECK` uses sustained configurable CPU, GPU, memory, and disk heuristics.
When its current result is `NONE`, it dynamically renders the next active token,
as does an inactive `PROC_HANG`; neither fall-through changes the selected token
or cycle index.

## Command line and exit codes

```text
LCDSirPlus.exe [--config PATH] [COMMAND]
  (none)                 Run the GUI/tray dashboard
  --validate-config      Validate configuration and exit
  --list-hwinfo-sensors  List exact usable HWiNFO labels and exit
  --preview              Force preview for the normal long-running runtime
  --hardware-test        Run the deterministic G13 test sequence
  --hardware-discover    List compatible G13 HID discovery results without writes
  --diagnostics          Write a bounded offline diagnostics ZIP and exit
  --discord-authorize    Authorize the active Discord Desktop account
  --discord-clear-token  Remove local LCDSirPlus Discord credentials
  --backend MODE         auto, sdk, hid, or virtual; hardware test only
  --duration-secs N      1..3600; hardware test only
  --safe-mode            Disable providers and destructive actions in normal runtime
  --diagnostic-dir PATH  Select runtime/test logs or diagnostics output
  --version, -v          Print version
  --help, -h             Print help
```

`--config` applies to normal runtime, validation, hardware tests, Discord
authorization, and Discord credential removal. Diagnostics deliberately ignores
it; hardware discovery, HWiNFO listing, help, and version do not read it.
An explicit path takes precedence. Without one, installed copies use
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`, while portable/development
copies use adjacent `lcdsirplus.txt`; first installed launch seeds a missing user
file from the installer template.
`--diagnostic-dir` applies to normal runtime, hardware-test logs, and diagnostics
output. `--preview` affects normal runtime. Diagnostics records the CLI safe-mode
state but still starts no providers; other one-shot commands do not need safe
mode. Only one command may be selected, and `--backend`/`--duration-secs` require
`--hardware-test`.

| Exit code | Meaning |
|---|---|
| `0` | Success. |
| `1` | Operational failure such as logging, diagnostics, HWiNFO, or Discord failure. |
| `2` | Invalid arguments or configuration. |
| `3` | No compatible G13 candidate, required owner, or selected hardware transport. |
| `4` | One or more hardware-test submission steps failed. |
| `5` | The single-instance mutex is already owned or could not be acquired. |

## Troubleshooting

### Configuration will not start or reload

Run `--validate-config --config PATH`. For tokenization or parse errors, correct
the reported source file and line. Final range and cross-field validation errors
may report line `0`; inspect the named settings because no exact source line was
retained. Unknown slot tokens, duplicate keys in one file, invalid ranges,
include cycles, and unsafe include paths are rejected. A failed hot reload
leaves the previous valid configuration active.

### G13 is not detected

Run `--hardware-discover`. For direct HID, exit Logitech Gaming Software,
confirm `LCore.exe` is absent, and stop Logitech LampArray service if it owns the
G13. For SDK mode, start the installed Logitech Gaming Software normally. Do not
copy SDK DLLs or kill unrelated G HUB services. Use `--preview` or
`logitech_backend virtual` to isolate rendering from hardware.

### Preview does not appear

Use `--preview`, set `preview_mode always`, and ensure `start_minimized 0`.
Left-click the tray icon to toggle it. In `auto`, the preview normally hides
while a physical HID/SDK backend is connected.

### A metric says N/A, STALE, OFFLINE, or DOWN

Check the slot table below for its source. Confirm the matching provider is
enabled and its hardware/program exists. Review the log for provider detail.
`N/A` means no valid current value, `STALE` means a prior value stopped updating,
and offline/disabled messages identify expected provider states.

For `HEADSET_BATTERY`, `NO DONGLE` means the enabled provider could not read an
exact supported receiver/control interface; it can also follow a USB open or
query failure. `OFFLINE` means a supported receiver answered but the headset or
active earbuds are off/disconnected. `STALE` preserves the last valid battery
after updates stop, and safe mode shows `DISABLED`. Find `VID_1038&PID_XXXX` in
Device Manager's Hardware Ids and compare `XXXX` with the 32-PID table in
[CONFIGURATION.md](CONFIGURATION.md#headset_battery). If listed, reconnect the
USB receiver and inspect the log for interface, usage, report-length, open, or
timeout detail. A matching marketing name without a matching PID and control
collection is not sufficient.

Known current limitations:

- `CPU_CACHE_TEMP` and `CPU_FREQ_TEMP` are accepted but have no runtime
  temperature producer and render `N/A`.

### CPU temperature remains N/A

Run `--list-hwinfo-sensors`. If it fails, confirm HWiNFO Sensors is running and
**Shared Memory Support** is enabled. If it succeeds, copy one exact original
sensor/reading label pair; do not shorten or translate labels. Check the log for
inactive mapping, stale data, unsupported ABI, incomplete pair, ambiguity, or
invalid unit/value. HWiNFO64 Free shared-memory monitoring stops after 12 hours;
restart it for interactive use or use HWiNFO32/appropriately licensed Pro for
unattended use under vendor terms.

For LHM fallback, confirm the user explicitly started LHM, enabled its web
server, and kept `lhm_url` on loopback. LCDSirPlus cannot turn that server on.

### PresentMon capture remains N/A

Confirm `PresentMon.exe` remains beside `LCDSirPlus.exe`,
`presentmon_enabled 1`, targeting is not disabled, and the foreground/explicit
process is actively presenting frames. Do not install the PresentMon service or
MSI for LCDSirPlus. If logs show ETW/capture access failure, add the user to
**Performance Log Users** and sign out/in; elevation can be tested only to
diagnose restrictive local policy. PresentMon supplies no hardware telemetry,
so it cannot fix CPU/GPU temperature or load-source failures.

### Ping/jitter/loss remain unavailable

The probe is disabled by default. Set `network_probe_enabled 1`, use a valid IP
literal and method, validate the file, and check firewall/network policy. Jitter
needs at least two successful samples. Safe mode always disables probing.

### Discord overlay does not appear

An empty `discord_client_id` is an unconfigured state, not a failed connection;
LCDSirPlus does not attempt IPC until a numeric ID is supplied. Otherwise,
confirm Discord Desktop is running in the same user/session, the numeric ID is
correct, the current account was authorized, required tester access exists, the
account is in a voice channel, and someone allowed by `discord_show_self` is
speaking. Reauthorize after revocation or client-ID changes. Never share token
files.

### Application says it is already running

Only one normal/direct-HID runtime may own the current Windows session. Exit the
existing tray application normally before starting another or a direct hardware
test.

## Logs, diagnostics, and privacy

The default log is `%LOCALAPPDATA%\LCDSirPlus\lcdsirplus.log`. Rotation is
bounded by `log_max_bytes` and `log_backups`. `--diagnostic-dir PATH` selects a
different log/diagnostic directory for that run. Normal tray launch has no
console window. A configuration or logging failure during desktop startup is
shown in a user-dismissible native error dialog; one-shot command modes report
failures through standard error instead.

Create an offline diagnostics bundle with:

```powershell
.\LCDSirPlus.exe --diagnostics
.\LCDSirPlus.exe --diagnostics --diagnostic-dir C:\safe\local\directory
```

Diagnostics mode ignores `--config`, starts no hardware/providers/probes/
Discord/actions, and writes a store-only ZIP capped at 1 MiB. It contains only
`privacy.txt`, `report.txt`, and `manifest.txt`. It excludes raw logs and
configuration, credentials, Discord content and identifiers, process/window
details, user paths, network targets, environment and registry values, serials,
and device paths. Review any bundle before sharing. Raw logs and configuration
may still contain operational detail and should not be posted without private
review.

LCDSirPlus is local-first. Normal optional outbound traffic is limited to a
configured network probe and Discord token exchange/refresh. LHM is restricted
to loopback. HWiNFO access is read-only shared memory, and PresentMon uses local
ETW/captured frame output. No LCDSirPlus feature opens an inbound listener.

Normal launch is a GUI/tray process with no console. One-shot commands attach
output to a parent console or redirected handles. Interactive PowerShell does
not always wait for a GUI-subsystem executable before returning the prompt; use
a pipeline such as `& .\LCDSirPlus.exe --help 2>&1 | Out-String` when output and
synchronous completion matter, or use `Start-Process -Wait -PassThru` when only
waiting/exit status is needed. A normal desktop startup failure such as invalid
configuration or an unavailable log path is also shown in a dismissible
**LCDSirPlus startup error** MessageBox; one-shot command failures stay on
standard error.

## Update and uninstall

For a published update or repair, close LCDSirPlus, verify the new setup EXE, and run it in
the same install mode. Rerunning setup restores installer-owned files. Windows
Modify, where offered, runs the exact setup cached at
`installer\LCDSirPlus-Setup.exe` with the registered `/CURRENTUSER` or
`/ALLUSERS` scope. Setup refuses an opposite-scope registration, an old
PowerShell install in the selected directory, or a foreign startup-task
collision when owned by the same account; remove that account's old installation
first. An all-users registration owned by another account may coexist with this
account's current-user install. Setup checks the active account and does not
enumerate offline user hives. Installed configuration remains at
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; logs and credentials remain
elsewhere under `%LOCALAPPDATA%\LCDSirPlus`.

Uninstall from Windows **Installed apps**. The uninstaller removes application
files, shortcuts, and the product-unique startup task. It preserves
`%LOCALAPPDATA%\LCDSirPlus`. Task ownership and removal are checked before any
application files are deleted; initialization only validates, while confirmed
uninstall revalidates and deletes the task immediately before file removal. A
cancel leaves the task intact, and a Task Scheduler failure leaves the
installation available for retry. Delete user data manually only when its logs,
configuration, and Discord credentials are no longer needed. Revoke Discord
authorization separately if needed.

For portable removal, first set `start_at_login 0`, start that exact portable
executable once, and exit it so its owned Run value is removed. Then delete the
portable directory. This cleanup never removes a foreign Run value.

## Slot option reference

The following table is the complete set of 53 tokens accepted in `slot_0`
through `slot_3`. The packaged [`modules.md`](../modules.md) is the shorter
button-slot quick reference; this table remains standalone and includes each
source, prerequisite, and fallback.

| Exact token | Display | Source/config dependency | External dependency and fallback |
|---|---|---|---|
| `HEADSET_BATTERY` | SteelSeries wireless battery/charge/online state | Exact supported USB receiver HID profile; `headset_enabled`, `headset_*` | Supported Arctis/GameBuds receiver family; otherwise `DISABLED`, `NO DONGLE`, `OFFLINE`, or `STALE`. |
| `CONTROLLER_BATTERY` | Controller number, battery, or wired | XInput; `controller_enabled`, `controller_index`, poll setting | Connected XInput controller; otherwise `OFFLINE`. |
| `FPS_CURRENT` | Current FPS | PresentMon capture and `presentmon_*` | Bundled PresentMon and selected game; otherwise `N/A`/`STALE`. |
| `FPS_1LOW` | 1% low FPS | PresentMon and `presentmon_window_ms` | Same PresentMon fallback. |
| `FPS_01LOW` | 0.1% low FPS | PresentMon and `presentmon_window_ms` | Same PresentMon fallback. |
| `FRAME_TIME` | Frame time in ms and trailing 30-second graph | PresentMon | Same PresentMon fallback. |
| `CPU_TEMP` | CPU package temperature | Exact HWiNFO pair, then LHM loopback | No generic Windows source; `N/A`/`STALE` without a usable fallback. |
| `GPU_TEMP` | GPU temperature | NVAPI/ADLX; LHM temperature fallback | Vendor driver or LHM; otherwise `N/A`/`STALE`. |
| `NET_IN` | Instantaneous ingress SI bit rate | Native interface counters, one-second sample | No external program; `N/A` in safe mode/provider failure. |
| `NET_OUT` | Instantaneous egress SI bit rate | Native interface counters | Same as `NET_IN`. |
| `NET_BOTH` | Ingress plus egress SI bit rate | Both native direction readings | `N/A`/`STALE` unless both are current. |
| `NET_IN_GRAPH` | Ingress over exactly the trailing 30 seconds | Native counters; graph ceiling setting | Empty/`N/A` without throughput. |
| `NET_OUT_GRAPH` | Egress over exactly the trailing 30 seconds | Native counters; graph ceiling setting | Empty/`N/A` without throughput. |
| `NET_GRAPH` | 30-second graph, ingress above/egress below center | Both native directions; graph ceiling setting | `N/A` if either direction is unavailable or nonfinite; `STALE` if either valid direction is stale. |
| `PING` | Latest latency in ms | Enabled network probe and target/method/timing settings | Sends configured ICMP/TCP; `N/A` when disabled/lost, stale on provider error. |
| `JITTER` | Latency variation in ms | Probe window | Requires at least two successful probes. |
| `PACKET_LOSS` | Probe loss percentage | Probe window | `N/A` without enabled/history data; stale on provider error. |
| `MIC_STATUS` | `LIVE`/`MUTED` | Core Audio; `audio_enabled`, poll setting | Readable default microphone; otherwise `N/A`. |
| `AUDIO` | Output volume percent | Core Audio; `audio_enabled`, poll setting | Readable default output; otherwise `N/A`. |
| `SESSION_TIME` | Active capture elapsed time | PresentMon session | PresentMon/active game; `00:00` when inactive. |
| `SESSION_SUMMARY` | Fitted duration and stutter count, or `IDLE` | PresentMon and stutter threshold | PresentMon/active game; otherwise `IDLE`. |
| `CLOCK` | Local time | Windows clock; accepted `time_format` is currently a no-op | None. |
| `GAME_NAME` | Captured game/process name | PresentMon target | `N/A` without a selected name. |
| `ALERTS` | Alert count and spaced level (`n L 2`/`n L 3`), or `CLEAR` | Alert thresholds and corresponding current telemetry | Missing telemetry creates no false alert; physical/preview slot 4 acknowledges globally. |
| `PROVIDER_STATUS` | Tracked providers down or `OK` | Runtime provider health | Safe mode publishes only healthy `safe-mode` and therefore displays `OK`. |
| `CPU_LOAD` | Whole-system CPU percent | Native scheduler accounting | None. |
| `RAM_USAGE` | Percent and used/total RAM | Native Windows memory status | No external program. |
| `GPU_LOAD` | GPU utilization percent | NVAPI/ADLX and `gpu_provider` | Vendor driver; `N/A`/`STALE` if unavailable. No LHM fallback. |
| `VRAM_USAGE` | VRAM percent and used/total | NVAPI/ADLX and `gpu_provider` | Vendor memory telemetry; `N/A`/`STALE` if incomplete. No LHM fallback. |
| `CPU_CACHE_TEMP` | Intended Cache-domain CPU temperature | Requires one Cache domain descriptor and temperature | Currently no runtime producer, including LHM; always `N/A`. |
| `CPU_FREQ_TEMP` | Intended Frequency-domain CPU temperature | Requires one Frequency domain descriptor and temperature | Currently no runtime producer, including LHM; always `N/A`. |
| `CPU_LOAD_GRAPH` | CPU load and trailing 30-second graph | Native scheduler accounting; fixed 0..100% scale | No external program. |
| `GPU_LOAD_GRAPH` | GPU load and trailing 30-second graph | NVAPI/ADLX; fixed 0..100% scale | Vendor driver; no LHM load fallback. |
| `CPU_TEMP_GRAPH` | CPU temperature and trailing 30-second graph | CPU source; `cpu_temp_max_c` ceiling/warning threshold | Same fallback as `CPU_TEMP`. |
| `GPU_TEMP_GRAPH` | GPU temperature and trailing 30-second graph | GPU source; `gpu_temp_max_c` ceiling/warning threshold | Same fallback as `GPU_TEMP`. |
| `VRM_TEMP` | VRM temperature | Exact `lhm_vrm_temp_sensor` | User-enabled LHM loopback only. |
| `CPU_FAN` | CPU-fan percentage | Exact LHM duty, else RPM / configured max | `cpu_fan_max_rpm 0` disables RPM estimate. |
| `PUMP_RPM` | Pump percentage despite compatibility name | Exact LHM duty, else RPM / configured max | `pump_max_rpm 0` disables RPM estimate. |
| `POWER_LIMIT` | Authoritative total power | Exact HWiNFO total pair, then exact LHM total sensor | Never synthesized; `N/A` without exact total. |
| `CPU_GPU_POWER` | Complete CPU+GPU subtotal | Current CPU and GPU component power | `N/A` if either component is absent/stale; not total system power. |
| `CHIPSET_TEMP` | Chipset temperature | Exact LHM SensorId | User-enabled LHM loopback only. |
| `MOTHERBOARD_TEMP` | Motherboard temperature | Exact LHM SensorId | User-enabled LHM loopback only. |
| `DISK_IO` | Aggregate system read/write rates | Native PDH `PhysicalDisk(_Total)` | No external program. |
| `DISK_IO_GRAPH` | Compact spaced read/write labels and trailing 30-second graph | Native PDH; `disk_graph_ceiling_mbps` | Scale clips graph height; `DISK_IO` retains fuller rates. |
| `RAM_DETAIL` | Fitted used/total physical RAM in IEC units | Native Windows memory status | No external program. |
| `FPS_GRAPH` | FPS and trailing 30-second graph | Bundled PresentMon; `fps_graph_ceiling` | Active capture required. |
| `THERMALS` | Complete `CPU nC` and `GPU nC` rows | Both current temperature readings | Either threshold can invert this pane. |
| `CONNECTIONS` | Established TCP count | Native IPv4+IPv6 TCP tables | No external program. |
| `NET_HEALTH` | Three rows: `P nMS`, `J nMS`, and `L n%` | Enabled probe and target/settings | All three readings required; blank scanlines separate rows. |
| `SYSTEM_BATTERY` | AC/battery/charging state | Native Windows system power status | Shows AC/BAT/CHG/no-battery/unknown/stale states. |
| `HARD_FAULTS` | Approximate page-read pressure as `n/s` | Native PDH `Page Reads/sec` | Titled `FAULTS`; not an exact hard-fault count. |
| `BOTTLENECK` | CPU/GPU/RAM/DISK I/O/NONE heuristic | Native metrics, thresholds, sustain | `NONE` dynamically displays next token without changing selection. |
| `PROC_HANG` | Selected hung window and guarded emergency hold action | Native detector and selected-slot physical button; `hang_enabled 0` by default | Unique across slots; provider disabled/unavailable and no-target fall-through until opt-in, then short release navigates and the hold threshold acts automatically; physical disposable-child acceptance remains required. |

For exhaustive configuration key defaults and validation ranges, see
[CONFIGURATION.md](CONFIGURATION.md). For physical action acceptance, see
[HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md).
