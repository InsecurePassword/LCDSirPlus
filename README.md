# LCDSirPlus 0.3.0

LCDSirPlus is an independent, open-source dashboard for the Logitech G13
160x43 monochrome LCD, inspired by the familiar LCDSirReal layout and
button-driven workflow. It keeps useful system, game, device, network, and
voice information visible while you play.

LCDSirPlus is not affiliated with or endorsed by the original LCDSirReal
developer.

- Fixed, readable dashboard with date, time, CPU, memory, GPU, and VRAM data.
- Four configurable button-aligned display slots with 53 available options.
- Automatic Logitech Gaming Software and direct-device operation.
- Native Windows CPU, memory, network, audio, controller, and GPU telemetry.
- Optional game performance, temperature, headset battery, and Discord voice
  information.
- Virtual preview, tray controls, alerts, safe mode, and offline diagnostics.
- Hot-reloaded text configuration and explicit unavailable/stale readings.
- Native portable application and per-user installer; no elevation required.

User guides: [Instruction Manual (PDF)](docs/LCDSirPlus-Instruction-Manual.pdf) |
[Instruction Manual (Markdown)](docs/INSTRUCTION-MANUAL.md) |
[Button Module Quick Reference](modules.md) |
[Configuration Reference](docs/CONFIGURATION.md)

## Install, update, portable use, and uninstall

Packages are not code-signed. Before running them, compare the downloaded ZIP
with the matching entry in `LCDSirPlus-0.3.0-SHA256SUMS.txt` by using
`Get-FileHash <zip-name> -Algorithm SHA256`.

### Installer

Extract the installer ZIP, open PowerShell in its root, and run:

```powershell
powershell.exe -NoProfile -File .\Install.ps1
```

This installs LCDSirPlus for the current user and creates a Start Menu shortcut.
Exit LCDSirPlus before installing. To start it at sign-in, set
`start_at_login 1` in `lcdsirplus.txt`; `-EnableLogin` can create the initial Run
value, but the application removes that owned value if the setting remains `0`.

### Update

Download and verify the new installer ZIP, extract it, exit LCDSirPlus, and run
`Install.ps1` again. Your installed `lcdsirplus.txt` is preserved. An update
refuses an undeclared file or a modified installer-owned file under the install
root rather than silently replacing it.

### Portable

Extract the portable ZIP anywhere on a local drive and run `LCDSirPlus.exe`.
Keep `lcdsirplus.txt` beside the executable. For a portable update, exit the
application, replace the packaged files, and retain your configured
`lcdsirplus.txt`.

### Uninstall

Exit LCDSirPlus, then run:

```powershell
& "$env:LOCALAPPDATA\Programs\LCDSirPlus\Uninstall.ps1"
```

Uninstall refuses a modified installer-owned file and retains configuration,
unknown files, and `%LOCALAPPDATA%\LCDSirPlus` user data. To remove the canonical
user-data directory too, use:

```powershell
& "$env:LOCALAPPDATA\Programs\LCDSirPlus\Uninstall.ps1" `
  -PurgeUserData -ConfirmPurge PURGE-LCDSIRPLUS-DATA
```

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
│SS BAT   │FPS 144  │GPU 74°C │1% 118   │
└─────────┴─────────┴─────────┴─────────┘
```

Dual-CCD CPUs render stacked Cache (`C`) and Frequency (`F`) micro-bars;
single-CCD CPUs render one full-height CPU bar. The deterministic renderer is
hash-verified against the original Go implementation's goldens:

| Golden dashboard | SHA-256 |
|---|---|
| Normal | `e44f9a1646d9e091fd1482dc5147f0cd9e639f0dd6b7fbb93edcf5d10da2757b` |
| All-bars-unavailable | `0561081d2f5ee1930177a40d7f4425ac32d84a53bde0aaf92e49f8e616033576` |
| All-bars-stale | `bd3e659e30ecb742c0a97aa331c74388608053a6a534496fd8cbfc635fb14cd0` |

## Configuration and buttons

Edit `lcdsirplus.txt` beside `LCDSirPlus.exe`. Saving a valid change reloads it
automatically; an invalid change leaves the previous settings active. Each
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
See the [Configuration Reference](docs/CONFIGURATION.md) or
[Instruction Manual](docs/INSTRUCTION-MANUAL.md) for advanced settings and
button actions.

Unavailable or stale data is shown explicitly rather than replaced by another
metric. The signed official PresentMon v2.5.1 console is bundled for frame/FPS
timing; other optional programs and hardware are not bundled.

## Display options

The 53 button-slot options cover device and audio status, PresentMon game/frame
telemetry, CPU/GPU/memory data, network throughput and quality, exact
temperature/cooling/power sensors, native disk/battery/connection statistics,
30-second graphs, alerts, provider health, bottleneck diagnosis, and the guarded
hung-window action. See the complete simple [button module quick
reference](modules.md); the standalone manual includes prerequisites and
fallback behavior for every token.

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
Without it, those modes use `lcdsirplus.txt` beside the executable. Diagnostics
intentionally ignores it. Logs normally go to
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
  `licenses/PresentMon/LICENSE.txt` and `THIRD_PARTY.txt` contain its notices.
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

[MIT](LICENSE)
