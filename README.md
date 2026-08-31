# LCDSirPlus 0.3.0

> **Draft / Unreleased:** LCDSirPlus 0.3.0 release acceptance is still pending.
> The installation instructions below apply only if 0.3.0 artifacts are published.

LCDSirPlus is an independent, open-source dashboard for the Logitech G13
160x43 monochrome LCD, inspired by the familiar LCDSirReal layout and
button-driven workflow. It keeps useful system, game, device, network, and
voice information visible while you play.

LCDSirPlus is not affiliated with or endorsed by the original LCDSirReal
developer.

- Three selectable built-in main displays with a shared compact header and four-button row.
- Four configurable button-aligned display slots with 53 available options.
- Automatic Logitech Gaming Software and direct-device operation.
- Native Windows CPU, memory, network, audio, controller, and GPU telemetry.
- Optional game performance, temperature, headset battery, and Discord voice
  information.
- Virtual preview, tray controls, alerts, safe mode, and offline diagnostics.
- Hot-reloaded text configuration and explicit unavailable/stale readings.
- Native portable application and standard current-user/all-users installer.

User guides: [Instruction Manual (PDF)](docs/LCDSirPlus-Instruction-Manual.pdf) |
[Instruction Manual (Markdown)](docs/INSTRUCTION-MANUAL.md) |
[Button Module Quick Reference](modules.md) |
[Configuration Reference](docs/CONFIGURATION.md)

## Install, update, portable use, and uninstall

The LCDSirPlus executable and package archives are not code-signed. Before
running them, compare each downloaded artifact with the matching entry in
`LCDSirPlus-0.3.0-SHA256SUMS.txt` by using
`Get-FileHash <file-name> -Algorithm SHA256`.

### Installer

If published, run `LCDSirPlus-0.3.0-win-x64-setup.exe`. The default current-user destination
is `%LOCALAPPDATA%\Programs\LCDSirPlus`; the install-mode dialog can instead
select an elevated all-users install under `%ProgramFiles%\LCDSirPlus`.
Start Menu and sign-in startup tasks are selected by default; the desktop
shortcut is optional. The startup task has a stable SID-specific name and runs
interactively with least privilege. Exit LCDSirPlus before installing.

### Update

For a published update, download and verify the new setup EXE, exit LCDSirPlus, and run it again in the
same install mode. Rerunning the exact setup also repairs installer-owned files.
Windows Modify, where offered, runs the exact setup copy cached under the install
directory with that same scope. The per-user configuration under
`%LOCALAPPDATA%\LCDSirPlus` is preserved. Uninstall an existing opposite-scope or
legacy PowerShell installation owned by the same Windows account before
switching scope. An all-users registration owned by another account may coexist
with this account's current-user installation; setup does not enumerate offline
user hives.

### Portable

If published, extract the portable ZIP anywhere on a local drive and run `LCDSirPlus.exe`.
Keep `lcdsirplus.txt` beside the executable. For a portable update, exit the
application, replace the packaged files, and retain your configured
`lcdsirplus.txt`.

### Uninstall

Exit LCDSirPlus, then uninstall it from Windows **Installed apps**. The standard
uninstaller removes installer-owned files, shortcuts, and the LCDSirPlus startup
task while preserving `%LOCALAPPDATA%\LCDSirPlus` configuration, logs, and
credentials. If the task cannot be authenticated or removed, uninstall stops
before deleting application files so it can be retried. Remove user data
manually only when it is no longer needed.

## Main Display

The framebuffer is always 160×43 pixels. Set the built-in main display with
exactly one integer value:

```text
main_display 1
```

Accepted values are `1`, `2`, and `3`; the default is `1`. Saving a valid
change hot-reloads the selected display. The compact date/time header and the
fixed four-button bottom row are shared by all three displays. These concise
examples show only the selectable metrics area:

```text
1: classic halves (original metrics)
+-------------------+-------------------+
| CPU ========      | GPU ===========   |
|     ========      |                   |
| RAM =========     | VRAM =======      |
+-------------------+-------------------+

2: thirds
+------------+------------+------------+
| CPU =====  | GPU =====  | OUT =====  |
|     =====  |            |            |
| RAM =====  | VRAM ====  | IN ===     |
+------------+------------+------------+

3: system and network halves
+-------------------+-------------------+
| CPU ========      | NET IN ========   |
|     ========      |                   |
| RAM =========     | NET OUT ======    |
+-------------------+-------------------+
```

All three layouts retain the CPU label and render stacked Cache and Frequency
bars, in that order, without per-bar glyphs on dual-CCD CPUs. Single-CCD CPUs
use one full-height CPU bar. Layout 2 shows CPU/RAM, GPU/VRAM, and OUT/IN
thirds. Layout 3 keeps Layout 1's CPU/RAM half and
replaces the GPU/VRAM half with NET IN/NET OUT. Source-level renderer tests pin
the current implementation to these golden hashes:

| Golden frame | SHA-256 |
|---|---|
| Layout 1, normal | `a2f36db6c54c09adcd459756677a6fc348f181d01e0810a9c7e4d9c3c998c20c` |
| Layout 1, bars unavailable | `387f5269dded63f8c38cd965b55f02fbb1e2552e89c90e5353c34ec3e1058aa9` |
| Layout 1, bars stale | `f7bc46cc0f4763084dabf1445a2240e08ec892ed2c3c49991e9428296d69bdb8` |
| Layout 2 | `c6bf7ef1e919ef47dfc7ed13ef9a9f937253e47a449fe8558f00adf98a881701` |
| Layout 3 | `ef1dbde8de40632b213b7c149789a716165719b0ab4f40ab80aa508930b6ff8b` |

The OUT/IN bars in layouts 2 and 3 reuse `network_graph_ceiling_mbps`, the
same scale used by network graph modules. They show current throughput, not
history: with the default `1000` Mbps ceiling, 1 Gbps fills a bar.

## Configuration and buttons

Installed copies use `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`;
portable and development copies use `lcdsirplus.txt` beside `LCDSirPlus.exe`.
The installer places a default template, and the first installed launch seeds
the user file only when it does not already exist. Saving a valid change reloads
it automatically; an invalid change leaves the previous settings active. Each
`slot_0` through `slot_3` line is an ordered list for one of the four LCD
buttons. A short press cycles that button's slot forward. In the preview,
left-click a slot to cycle forward and right-click it to cycle backward.

The shipped slots are:

```text
slot_0  HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY
slot_1  FPS_CURRENT FPS_1LOW FRAME_TIME SESSION_TIME
slot_2  PROC_HANG GPU_TEMP PING JITTER AUDIO
slot_3  THERMALS PACKET_LOSS MIC_STATUS SESSION_SUMMARY PROVIDER_STATUS
```

Common provider settings include `gpu_provider`, `presentmon_target_mode`,
`lhm_mode`, `network_probe_enabled`, `network_probe_target`,
`discord_client_id`, and `preview_mode`.

Warning categories are independently hot-reloaded:

```text
temperature_warning_enabled 1
memory_warning_enabled      1
memory_warning              100
vmem_warning                100
warning                     1
```

`memory_warning` and `vmem_warning` default to `100` and trigger inclusively at
`>=`; `0` remains a compatibility disable value, but the category switch is
preferred. With temperature warnings enabled, compatibility key `warning 1`
uses pane flashing and suppresses temperature full-screen overlays, while
`warning 0` retains those overlays.
`critical_alert_linger_ms` applies only to severity-3 episodes and starts at the
first current valid recovery reading. Severity-2 warnings are removed
immediately on current recovery; unknown or stale readings retain an existing
episode, while disabling its category or the headset provider clears it.
See the [Configuration Reference](docs/CONFIGURATION.md) or
[Instruction Manual](docs/INSTRUCTION-MANUAL.md) for advanced settings and
button actions.

Unavailable or stale data is shown explicitly rather than replaced by another
metric. Release packages are designed to bundle the signed official PresentMon
v2.5.1 console for frame/FPS timing; other optional programs and hardware,
including user-managed LibreHardwareMonitor, are not bundled. LHM being absent
is an acceptable unavailable state, and exact SensorIds come from its loopback
`data.json` response, not diagnostics.

PresentMon support is implemented, software-tested, and pinned, and remains
enabled by default as a required feature. Live capture against an actively
presenting game is not yet release-qualified and is deferred while this PC's
memory is occupied by the local LLM; release remains pending that gate.

## Display options

The 53 button-slot options cover device and audio status, PresentMon game/frame
telemetry, CPU/GPU/memory data, network throughput and quality, exact
temperature/cooling/power sensors, native disk/battery/connection statistics,
30-second graphs, alerts, provider health, bottleneck diagnosis, and the guarded
hung-window action. See the complete simple [button module quick
reference](modules.md); the standalone manual includes prerequisites and
fallback behavior for every token.

`hang_enabled` defaults to `0`. `PROC_HANG` remains in the shipped slot with the
same button until the user explicitly opts in. Its provider reports
disabled/unavailable while the existing no-target pane fallback shows
the next configured token without changing selection. Termination can lose
unsaved work and remains unqualified until the documented
disposable-child physical-button gate passes.

All graphs cover exactly the trailing 30 seconds. Network rates use decimal
`Kbps`, `Mbps`, or `Gbps`, and `PUMP_RPM` displays a calibrated percentage rather
than raw RPM. `CPU_CACHE_TEMP` and `CPU_FREQ_TEMP` are reserved and currently
display `N/A`.

## Discord setup

LCDSirPlus can show active speakers from the current Discord Desktop voice
channel. Discord requires a developer application and may require your account
to be listed as an application tester. See Discord's official
[Developer Portal](https://discord.com/developers/applications),
[Desktop download](https://discord.com/download), and
[RPC authorization documentation](https://discord.com/developers/docs/topics/rpc#authorize).

The integration is implemented and software-tested, but the live voice/OAuth
workflow is not yet release-qualified. The user creates and registers their own
Discord application; tokens remain DPAPI-protected local data. Release remains
pending this live gate unless it is explicitly deferred.

The configured `discord_client_id` is the application's public numeric
Application ID/OAuth client ID, not a secret. Leaving it empty is unconfigured
and causes no IPC attempt. LCDSirPlus reads voice state only; it publishes no
Rich Presence and needs no image assets. Discord Desktop must run under the same
Windows user/session and pass the signed fixed-local IPC checks.

1. Create an application in the Developer Portal and add your Discord account
   as a tester if required.
2. Copy the application's client ID into `discord_client_id` in
   `lcdsirplus.txt`.
3. Start Discord Desktop and sign in to the account you want to authorize.
4. Run `LCDSirPlus.exe --discord-authorize` and approve the prompt in Discord.

If Discord requires the application's client secret for authorization, expose
it only for that command and remove it immediately afterward:

```powershell
$env:LCDSIRPLUS_DISCORD_CLIENT_SECRET = '<secret>'
.\LCDSirPlus.exe --discord-authorize
Remove-Item Env:\LCDSIRPLUS_DISCORD_CLIENT_SECRET
```

Never place the secret in `lcdsirplus.txt` or a command argument. Repeat the
authorization while each Discord Account Switcher account is active. When a
secret is required, LCDSirPlus retains it only inside the protected local
credential so token refresh can continue; the environment variable remains
temporary and should still be removed immediately.

To remove LCDSirPlus's local Discord credentials, run
`LCDSirPlus.exe --discord-clear-token`. To end access remotely, also revoke the
application from Discord's **User Settings > Authorized Apps**. Do both if a
secret or token may have been exposed.

## Command line

```text
LCDSirPlus.exe [--config PATH] [COMMAND]
  (none)                 Run the dashboard application
  --validate-config      Validate the configuration and exit
  --list-hwinfo-sensors  List usable HWiNFO sensor/reading labels and exit
  --preview              Run with the virtual preview forced on
  --hardware-test        Run the G13 display/button test sequence
  --hardware-discover    List compatible G13 HID discovery results
  --diagnostics          Write an offline diagnostics ZIP and exit
  --discord-authorize    Authorize the active Discord Desktop account
  --discord-clear-token  Remove local LCDSirPlus Discord credentials
  --backend auto|sdk|hid|virtual
                         Backend for --hardware-test (default: hid)
  --duration-secs N      Hardware-test duration, 1..3600 (default: 30)
  --safe-mode            Disable providers and destructive actions
  --diagnostic-dir PATH  Select the log and diagnostics directory
  --version, -v          Print the version
  --help, -h             Print command help
```

`--config PATH` selects another configuration file for normal runtime,
validation, hardware tests, and Discord authorization/credential removal.
An explicit path takes precedence over every default. Without it, installed
copies use `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; portable and
development copies use the adjacent `lcdsirplus.txt`. On the first installed
launch only, the installer-provided template seeds a missing user file.
Diagnostics intentionally ignores `--config`. Logs normally go to
`%LOCALAPPDATA%\LCDSirPlus\lcdsirplus.log`.

LCDSirPlus is a Windows GUI executable, so normal launch does not create or
retain a terminal. One-shot command output remains available to parent consoles
and redirected pipes. Windows shells do not wait for GUI executables in every
interactive context; use a PowerShell pipeline (for example,
`& .\LCDSirPlus.exe --help 2>&1 | Out-String`) or
`start /wait "" LCDSirPlus.exe --help` from `cmd.exe` when synchronous waiting
is required.

## Credits and dependencies

- LCDSirPlus is inspired by [SirReal's LCDSirReal multipurpose Logitech LCD
  plugin](https://hydrogenaudio.org/index.php/topic,46373.0.html). It is an
  independent project and is not endorsed by the original developer.
- Required platform: [Windows 11 x64](https://www.microsoft.com/windows/windows-11)
  as a standard user. A [Logitech G13](https://support.logi.com/) is required
  only for the physical LCD and buttons; the preview works without one.
- Optional Logitech runtime: [Logitech Gaming Software](https://support.logi.com/hc/en-us/articles/360025298053-Logitech-Gaming-Software).
- The official signed [PresentMon](https://github.com/GameTechDev/PresentMon)
  v2.5.1 console is bundled under its MIT license for frame/FPS timing only;
  `licenses/PresentMon/LICENSE.txt` and `licenses/PresentMon/THIRD_PARTY.txt`
  contain its notices.
- Optional telemetry and integration tools are user-managed
  [HWiNFO](https://www.hwinfo.com/),
  [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor),
  [Discord Desktop](https://discord.com/download), and compatible SteelSeries
  headset or XInput controller hardware. HWiNFO is separately licensed under
  its vendor terms; LibreHardwareMonitor is MPL-2.0 licensed.
- Published SteelSeries USB protocol facts were independently checked against
  [HeadsetControl](https://github.com/Sapd/HeadsetControl),
  [Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager), and
  [Arctis Nova 3X Battery Tray](https://github.com/pokjump/arctisnova3xbatterytray).
  LCDSirPlus does not include or redistribute their code.
- GPU telemetry uses vendor-installed [NVIDIA NVAPI/NVML](https://developer.nvidia.com/management-library-nvml)
  or [AMD ADLX](https://gpuopen.com/adlx/) APIs under their vendor terms; their
  SDKs are not bundled.
- Direct Rust dependencies are Microsoft's [`windows` crate](https://github.com/microsoft/windows-rs)
  and the [`winres` build dependency](https://crates.io/crates/winres).

LCDSirPlus is not affiliated with or endorsed by Logitech, SteelSeries, Intel,
NVIDIA, AMD, HWiNFO, LibreHardwareMonitor, PresentMon, Discord, or Microsoft.
Names and trademarks belong to their respective owners.

## License

LCDSirPlus source is [MIT-licensed](LICENSE). Redistributed Rust and Windows
dependency notices are in [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt);
the bundled PresentMon notices are shipped separately at the paths above.
