# LCDSirPlus 0.3.0 Configuration Reference

This is the complete advanced settings reference. Version 0.3.0 is still draft
and unreleased; final downloadable package testing remains pending. For first
setup, use the [instruction manual](INSTRUCTION-MANUAL.md) instead.

## Find the active file

- Installed: `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`
- Portable or development: `lcdsirplus.txt` beside `LCDSirPlus.exe`
- Override: `LCDSirPlus.exe --config PATH`

The installer keeps `lcdsirplus.default.txt` in the installed program folder,
not the documentation folder. On first installed start, LCDSirPlus copies it to
the user location only if that file is missing. Update, repair, and uninstall do
not replace or remove the user file.

Validate a file with:

```powershell
.\LCDSirPlus.exe --validate-config
.\LCDSirPlus.exe --validate-config --config C:\path\lcdsirplus.txt
```

Run `.\LCDSirPlus.exe` commands from the folder containing the executable. From
any PowerShell window, an installed copy can use:

```powershell
& "$env:LOCALAPPDATA\Programs\LCDSirPlus\LCDSirPlus.exe" --validate-config
& "$env:ProgramFiles\LCDSirPlus\LCDSirPlus.exe" --validate-config
```

Most valid changes apply while the app runs. Invalid changes are rejected and
the last valid settings stay active. `preview_scale`, `log_level`,
`log_max_bytes`, and `log_backups` take effect after restart. Compatibility-only
settings are accepted but ignored.

## Write valid lines

Use one key followed by whitespace-separated values. Use single or double
quotes around values with spaces. `#` starts a comment when it appears between
values. Keys are case-insensitive. Button option names are also
case-insensitive.

Boolean values accept `1`, `true`, `yes`, or `on`, and `0`, `false`, `no`, or
`off`. Each string is limited to 4096 bytes and cannot contain control
characters. One line can contain at most 256 values. Other limits are 8 include
levels, 64 files, 64 include lines, 8 MiB total input, 16384 lines, and 16 KiB
per token. Duplicate keys in one file are rejected.

An `auto` interval uses the numeric default shown in its table. Other selectors
that accept `auto` are described separately.

For display message meanings and setup tasks, see the
[instruction manual](INSTRUCTION-MANUAL.md#read-display-messages).

## Set the app and preview

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `config_refresh_ms` | `1000` (`auto`) | `100..60000` | Settings file check interval. |
| `telemetry_interval_ms` | `300` (`auto`) | `100..10000` | Fast reading interval. |
| `render_interval_ms` | `100` (`auto`) | `25..5000`; must be `<=100` when `warning 1` and `temperature_warning_enabled 1` | Screen update interval. |
| `main_display` | `1` | integer `1`, `2`, or `3` | Main layout. |
| `preview_mode` | `auto` | `auto`, `always`, `never` | Initial preview visibility. |
| `preview_scale` | `4` | integer `1..10`; restart required | Preview size. |
| `start_minimized` | `0` | boolean | Do not automatically show preview at startup. |
| `start_at_login` | `0` | boolean | Portable current-user startup entry; ignored in safe mode and installed mode. |
| `safe_mode` | `0` | boolean | Restrict the normal dashboard to local CPU, RAM, and clock data; harmless slot cycling and alert acknowledgement remain available. |

`preview_mode auto` shows the preview when neither physical G13 connection is
active. A tray toggle overrides later automatic visibility changes.

Layouts are fixed:

- `1`: CPU/RAM and GPU/VRAM halves.
- `2`: CPU/RAM, GPU/VRAM, and outgoing/incoming thirds.
- `3`: CPU/RAM and incoming/outgoing halves.

Layouts 2 and 3 scale their network bars with
`network_graph_ceiling_mbps`.

## Set button slots

| Key | Default | Constraint |
|---|---|---|
| `slot_0` | `HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY` | One or more of the 53 button values. |
| `slot_1` | `FPS_CURRENT FPS_1LOW FRAME_TIME SESSION_TIME` | One or more button values. |
| `slot_2` | `PROC_HANG GPU_TEMP PING JITTER AUDIO` | One or more button values. |
| `slot_3` | `THERMALS PACKET_LOSS MIC_STATUS SESSION_SUMMARY PROVIDER_STATUS` | One or more button values. |

The lists map to physical and preview buttons 1 through 4. A short press moves
through one list. `PROC_HANG` may appear only once across all four lists. See
the canonical [53-value button option reference](../modules.md) and the manual's
[slot setup steps](INSTRUCTION-MANUAL.md#choose-layouts-and-button-options).

## Set CPU bars

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `ccd_source` | `auto` | `auto`, `manual` | Automatic CPU-domain detection or manual lists. |
| `ccd_cache_processors` | `auto` | `auto` or logical processor list such as `0,1,2` or `0-15`; each `0..63` | Cache-domain list in manual mode. |
| `ccd_frequency_processors` | `auto` | Same list syntax and range; cannot overlap cache list | Frequency-domain list in manual mode. |

The cache and frequency lists must both be `auto` or both be explicit. Explicit
lists take effect only with `ccd_source manual`; otherwise automatic detection
groups CPU cores by L3 cache or NUMA layout and uses CPUID L3 size to identify
the larger V-Cache domain. One detected domain uses one full-height CPU bar.

## Choose the G13 connection

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `logitech_backend` | `auto` | `auto`, `sdk`, `hid`, `virtual` | G13 or preview connection mode. |
| `logitech_reconnect_ms` | `5000` (`auto`) | `250..300000` | First reconnect delay. |
| `logitech_reconnect_max_ms` | `60000` (`auto`) | at least reconnect delay; at most `600000` | Maximum reconnect delay. |
| `logitech_button_poll_ms` | `50` (`auto`) | `10..1000` | Physical button check interval. |
| `logitech_button_debounce_ms` | `40` (`auto`) | `10..500` | Stable button edge time. |
| `logitech_friendly_name` | `LCDSirPlus` | non-empty; no NUL | Name shown to Logitech software. |
| `logitech_orientation` | `normal` | `normal`, `flip_x`, `flip_y`, `rotate_180` | Screen orientation. |
| `logitech_invert` | `0` | boolean | Reverse black and white pixels. |

`auto` selects the trusted Logitech LCD SDK when validated exact `LCore.exe` is
present; otherwise, it tries direct G13 access. Direct HID requires both exact
`LCore.exe` and `logi_lamparray_service.AMD64.exe` to be absent and fails closed
if process enumeration fails. Explicit `sdk` and `hid` do not switch to each
other.

`--hardware-discover` scans at most 256 HID interfaces until the first exact G13
match. It reports that candidate plus preceding rejection reasons, omits device
paths, performs no writes, and does not inspect competing processes. Discovery
cannot establish ownership.

## Use LibreHardwareMonitor

LibreHardwareMonitor is optional and not bundled. Start it yourself and safely
enable its web server. LCDSirPlus accepts local HTTP only and never starts or
configures LibreHardwareMonitor.

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `lhm_mode` | `auto` | `auto`, `on`, `off` | Enable, disable, or try local LibreHardwareMonitor. |
| `lhm_url` | `auto` | `auto` or local `http://` URL with no credentials, query, or fragment | JSON address; `auto` is `http://127.0.0.1:8085/data.json`. |
| `lhm_interval_ms` | `300` (`auto`) | `100..60000` | Read interval. |
| `lhm_stale_ms` | `3000` (`auto`) | at least interval; at most `300000` | Age limit. |
| `lhm_cpu_temp_sensor` | omitted | exact optional SensorId | CPU temperature override. |
| `lhm_gpu_temp_sensor` | omitted | exact optional SensorId | GPU temperature override. |
| `lhm_vrm_temp_sensor` | omitted | exact temperature SensorId | VRM temperature. |
| `lhm_chipset_temp_sensor` | omitted | exact temperature SensorId | Chipset temperature. |
| `lhm_motherboard_temp_sensor` | omitted | exact temperature SensorId | Motherboard temperature. |
| `lhm_cpu_fan_control_sensor` | omitted | exact `0..100%` control SensorId | CPU fan duty. |
| `lhm_cpu_fan_rpm_sensor` | omitted | exact fan RPM SensorId | CPU fan estimate when duty is absent. |
| `lhm_pump_control_sensor` | omitted | exact `0..100%` control SensorId | Pump duty. |
| `lhm_pump_rpm_sensor` | omitted | exact fan RPM SensorId | Pump estimate when duty is absent. |
| `lhm_total_power_sensor` | omitted | exact total-power SensorId | `POWER_LIMIT` fallback. |
| `lhm_cpu_power_sensor` | omitted | exact CPU-package-power SensorId | CPU part of `CPU_GPU_POWER`. |
| `lhm_gpu_power_sensor` | omitted | exact GPU-board-power SensorId | GPU power fallback. |
| `cpu_fan_max_rpm` | `0` | `0..30000`; `0` disables RPM estimate | Fan RPM calibration. |
| `pump_max_rpm` | `0` | `0..30000`; `0` disables RPM estimate | Pump RPM calibration. |

Copy exact SensorIds from your local `data.json` page. Diagnostics does not
contain them. Missing, duplicate, wrong-type, or out-of-range exact sensors are
not used. LibreHardwareMonitor supplies temperature and exact configured
cooling/power readings only. It does not supply LCDSirPlus CPU/GPU load, memory,
disk, or network readings.

## Use HWiNFO

HWiNFO is optional and not bundled. Start Sensors-only mode, enable **Shared
Memory Support**, and run `LCDSirPlus.exe --list-hwinfo-sensors`. Labels are
exact and case-sensitive. Each pair must be fully blank or fully set.

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `hwinfo_cpu_temp_sensor` | empty | exact sensor original label; pair required | CPU temperature sensor. |
| `hwinfo_cpu_temp_reading` | empty | exact temperature original label; pair required | CPU temperature reading. |
| `hwinfo_total_power_sensor` | empty | exact sensor original label; pair required | Total-power sensor. |
| `hwinfo_total_power_reading` | empty | exact power original label; pair required | Total-power reading. |
| `hwinfo_cpu_power_sensor` | empty | exact sensor original label; pair required | CPU-package-power sensor. |
| `hwinfo_cpu_power_reading` | empty | exact power original label; pair required | CPU-package-power reading. |
| `hwinfo_gpu_power_sensor` | empty | exact sensor original label; pair required | GPU-board-power sensor. |
| `hwinfo_gpu_power_reading` | empty | exact power original label; pair required | GPU-board-power reading. |
| `hwinfo_stale_ms` | `5000` | `1000..60000` | Maximum HWiNFO reading age. |

HWiNFO64 Free shared-memory monitoring has a 12-hour limit. Use HWiNFO32 or an
appropriately licensed HWiNFO Pro for unattended operation. LCDSirPlus reads
HWiNFO only and does not start, configure, restart, or license it.

## Choose GPU data

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `gpu_provider` | `auto` | `auto`, `nvapi`, `adlx`, `off` | NVIDIA/AMD GPU data source. |

`auto` tries NVIDIA then AMD. Installed vendor drivers can supply GPU load,
video memory, temperature, and some power readings. LibreHardwareMonitor can
replace only missing temperature or exact configured power readings.

## Use PresentMon

The planned 0.3.0 packages are designed to contain one signed PresentMon v2.5.1
console as `PresentMon.exe`. Raw Cargo builds do not contain it. PresentMon
supplies frame and session values only, not hardware temperature or load values.

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `presentmon_enabled` | `1` | boolean | Frame capture; safe mode disables it. |
| `presentmon_path` | `auto` | `auto` or approved console path on a fixed local drive | PresentMon executable. |
| `presentmon_interval_ms` | `1000` (`auto`) | `100..10000` | PresentMon output interval. |
| `presentmon_window_ms` | `60000` (`auto`) | `5000..600000` | 1% and 0.1% low window. |
| `presentmon_target_mode` | `presenting` | `presenting`, `foreground`, `process_name`, `disabled` | Automatic, targeted, or disabled capture. |
| `presentmon_deferred` | `1` | boolean | Temporarily show another eligible slot option when PresentMon data is unavailable. |
| `presentmon_persist` | `0` | boolean | Normal game preference; `1` allows ordinary desktop apps even with a valid game list. |
| `presentmon_process_name` | empty | one process name; required for `process_name` | Explicit target. |
| `presentmon_exclude` | `dwm.exe explorer.exe applicationframehost.exe textinputhost.exe searchhost.exe lcdsirplus.exe lcdsirplus.console.exe` | zero to 256 names | Names rejected from selection. |
| `stutter_threshold_ms` | `33.34` | `1..1000` | Frame time counted as a stutter. |

Default `presenting` starts one LCDSirPlus-owned PresentMon child and selects an
active game automatically. It optionally reads only:

```text
%LOCALAPPDATA%\NVIDIA Corporation\NVIDIA App\NvBackend\ApplicationStorage.json
```

The file is treated as untrusted, read only, and limited to 1 MiB, 4096
application records, and 64 detected paths per record. A usable record must
provide all expected typed fields. A path qualifies only when the record is not
creative, supports OPS, is fingerprint-detected, and is not manually added.
The running executable's normalized full fixed-drive path must match a qualified
catalog path; matching is case-insensitive and is not a physical-file identity
check. LCDSirPlus never writes or lists the catalog, starts NVIDIA software,
uses DRS, reads Xbox catalogs, or contacts a catalog service.

With `presentmon_persist 0`, a usable catalog rejects non-game paths. If the
catalog is unavailable, unsafe, changing, too large, malformed, or has an
unsupported shape, automatic workload fallback may still select another app.
There is no user-maintained game list.

With `presentmon_deferred 1`, unavailable PresentMon button options are skipped
only for display. The saved selection does not change. `PROC_HANG` and
`BOTTLENECK` are never selected as the temporary result. An all-unavailable list
shows `CLEAR`. Set it to `0` to keep the selected inactive panel.

With `presentmon_persist 1`, automatic mode allows generic workload selection
even when a valid catalog exists. The selected executable filename and FPS may
appear. Old readings are not preserved. This setting does not bypass the
exclusion list, pin a program, guarantee DWM readings, or affect `foreground`
and `process_name`. Set `presentmon_enabled 0` if no filename or FPS exposure is
acceptable.

Frame values become stale after five seconds without frames and expire after
another five seconds. A changed selected process resets the session. LCDSirPlus
stops only the PresentMon child it owns. If Windows denies capture, add the user
to **Performance Log Users**, sign out, and sign in. Elevation is for diagnosis,
not normal use.

## Use a supported SteelSeries receiver

`HEADSET_BATTERY` supports only SteelSeries VID `1038` with the exact receiver
PID, USB interface, usage, and report profile below. This is not support for all
SteelSeries, Bluetooth, or wired products. PID `2212` is preferred when more
than one supported receiver is connected.

### Supported SteelSeries receivers

| Family | Exact PIDs | Interface and usage | Battery behavior |
|---|---|---|---|
| Arctis 1/7X/7P Wireless | `12B3`, `12B6`, `12D7`, `12D5` | interface 3, `FF43:0202` | direct percentage; no charge flag |
| Arctis 9 | `12C2` | interface 0, `FFC0:0001` | scaled percentage; charge flag |
| Arctis Pro Wireless | `1290` | interface 0, `FF00:0001` | 25% bands; no charge flag |
| Arctis 7+/7P+ variants | `220E`, `2212`, `2216`, `2236` | interface 3, `FFC0:0001` | direct or 25% bands; charge flag |
| Nova 7/7X/7P coarse variants | `2202`, `2206`, `220A`, `223A`, `227A`, `22A4`, `22AB` | interface 3, `FFC0:0001` | 25% bands; charge flag |
| Nova 7/7X/7P direct variants | `22A1`, `227E`, `2258`, `229E`, `22A9`, `22A5`, `22A7`, `2298`, `22AD` | interface 3, `FFC0:0001` | direct percentage; charge flag |
| Nova 5/5X and Nova 3P/3X Wireless variants | `2232`, `2253`, `2264`, `2269`, `226D` | interface 3, `FFC0:0001` | direct percentage; charge flag |
| Arctis GameBuds | `230A` | interface 3, `FFC0:0001` | lower active-earbud percentage; no charge flag |

The table has 32 unique PIDs across eight profiles. A listed family or PID is
not enough without the required control collection. Unsupported known PIDs are
`1260`, `12AD`, `12E0`, `12E5`, `225D`, `1252`, `1280`, `12EC`, `220C`, `2200`,
`2204`, `2208`, `2267`, `230C`, `231A`, `1292`, and `1297`. Every other unlisted
PID is also unsupported. SteelSeries GG may coexist but is not read or required.

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `headset_enabled` | `1` | boolean | Receiver battery reading; safe mode disables it. |
| `headset_poll_ms` | `15000` (`auto`) | `1000..600000` | Read interval. |
| `headset_query_timeout_ms` | `1200` (`auto`) | `100..10000` | Complete query time limit. |
| `headset_stale_ms` | `45000` (`auto`) | at least poll interval; at most `3600000` | Reading age limit. |
| `headset_warn_percent` | `25` | `0..100` | Battery warning threshold. |
| `headset_critical_percent` | `0` | `0..100`; not above warning | Battery critical threshold. |

## Set controller, audio, and network options

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `controller_enabled` | `1` | boolean | XInput battery reading; safe mode disables it. |
| `controller_index` | `-1` | `-1..3`; `-1` chooses first connected | XInput controller. |
| `controller_poll_ms` | `10000` (`auto`) | `1000..600000` | Controller check interval. |
| `audio_enabled` | `1` | boolean | Default output and microphone readings; safe mode disables them. |
| `audio_poll_ms` | `1000` (`auto`) | `100..60000` | Sound device check interval. |
| `network_probe_enabled` | `0` | boolean | Send network quality probes; safe mode disables them. |
| `network_probe_method` | `icmp` | `auto`, `icmp`, `tcp` | Probe method. |
| `network_probe_target` | `1.1.1.1` | when enabled, one IP literal, optionally TCP port `1..65535`; explicit ICMP needs IPv4 without port | Probe destination. |
| `network_probe_interval_ms` | `1000` (`auto`) | `250..60000` | Delay between probes. |
| `network_probe_timeout_ms` | `1500` (`auto`) | `50..30000` | Total timeout for one attempt and fallback. |
| `network_probe_window` | `30` | `5..600` while enabled | Attempts kept for jitter and loss. |

The network probe is off by default and sends no traffic while off or in safe
mode. Host names are rejected. TCP defaults to port 443 when no port is given.
`auto` tries IPv4 ICMP and then TCP within one timeout; IPv6 uses TCP.

## Set graph, pane warning, and bottleneck limits

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `network_graph_ceiling_mbps` | `1000` | `1..100000` | Network graphs and layout 2/3 current bars. |
| `disk_graph_ceiling_mbps` | `1000` | `1..100000` | Disk graph. |
| `fps_graph_ceiling` | `240` | `1..1000` | FPS graph. |
| `warning` | `1` | boolean | `1` flashes selected temperature panes; `0` uses full-screen temperature alerts. |
| `cpu_temp_max_c` | `90` | `1..150` | CPU graph ceiling and pane flash threshold. |
| `gpu_temp_max_c` | `90` | `1..150` | GPU graph ceiling and pane flash threshold. |
| `bottleneck_cpu_percent` | `90` | `1..100` | CPU bottleneck threshold. |
| `bottleneck_gpu_percent` | `95` | `1..100` | GPU bottleneck threshold. |
| `bottleneck_memory_percent` | `90` | `1..100` | RAM/VRAM bottleneck threshold. |
| `bottleneck_disk_mbps` | `500` | `1..100000` | Combined disk read/write threshold. |
| `bottleneck_sustain_ms` | `2000` | `0..60000` | Time a bottleneck must continue. |

All graphs cover the trailing 30 seconds. CPU/GPU load graphs use a fixed
0..100% scale. `FRAME_TIME` scales to its observed 30-second range. Ceilings do
not clip numeric text.

## Set Discord

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `discord_enabled` | `1` | boolean | Discord voice display; safe mode disables Discord access by the normal dashboard. |
| `discord_client_id` | empty | digits only | Public application ID. Empty means not configured. |
| `discord_linger_ms` | `700` | `0..10000` | Keep a speaker visible after speaking stops. |
| `discord_max_speakers` | `2` | `1..4` | Visible speaker count before `+N`. |
| `discord_show_self` | `0` | boolean | Include the current user. |
| `discord_show_channel` | `0` | boolean | Show voice channel name as title. |

The client ID is not secret. Each user creates a Discord application and
authorizes each Discord account. The authorization command requires the client
secret temporarily in `LCDSIRPLUS_DISCORD_CLIENT_SECRET`; use the manual's
[masked workflow](INSTRUCTION-MANUAL.md#connect-discord). Never put the secret
or tokens in this file, arguments, logs, screenshots, issues, or chat.

Windows protects credentials, including the secret needed for refresh, for the
current Windows account under `%LOCALAPPDATA%\LCDSirPlus`. Follow the manual's
[disconnect instructions](INSTRUCTION-MANUAL.md#disconnect-discord) to remove
local records and revoke remote access. LCDSirPlus opens no callback listener
and does not use `discord_redirect_uri`. Safe mode restricts the normal
dashboard; explicit `--discord-authorize` and `--discord-clear-token` commands
remain available.

## Set the hung-window action

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `hang_enabled` | `0` | boolean | Opt in to detection and destructive termination. |
| `hang_hold_ms` | `2000` | `1000..10000` | Continuous hold before automatic action. |
| `hang_probe_interval_ms` | `2000` | `250..60000` | Window check interval. |
| `hang_probe_timeout_ms` | `350` | `10..5000`; less than probe interval | One window response timeout. |
| `hang_failures_required` | `3` | `2..10` | Consecutive timeouts before showing a target. |
| `hang_minimum_ms` | `6000` | `1000..60000` | Minimum continuous failure time. |
| `hang_ignore` | `lcdsirplus.exe explorer.exe dwm.exe winlogon.exe csrss.exe services.exe lsass.exe smss.exe fontdrvhost.exe sihost.exe taskhostw.exe` | zero to 256 names | Programs never offered for termination. |

Do not enable this action in the draft 0.3.0 build. Its physical behavior is not
release-tested, and there is no supported end-user test. If enabled despite
that warning, the selected `PROC_HANG` slot's physical button controls the
action. A short
release changes detail or target. A continuous hold requests termination at the
threshold after checking the same target again. Recovery, selection or device
change, a settings change, or safe mode cancels the hold. If the app responds
too late, the hold is refused rather than fired late.

Termination can lose unsaved work. The default keeps detection and action off.

## Advanced compatibility-only settings

These keys keep older configuration files valid. LCDSirPlus parses and validates
them but ignores their values. New files should omit them.

| Key | Default | Accepted value or constraint | Compatibility behavior |
|---|---:|---|---|
| `date_format` | `yyyy-MM-dd dddd` | non-empty reserved string; not validated as a Windows format | Header date remains fixed. |
| `time_format` | `HH:mm:ss` | non-empty reserved string; not validated as a Windows format | Header time remains fixed. |
| `headset_estimate_hours` | `1` | boolean | No battery-hours estimate is shown. |
| `discord_redirect_uri` | `http://127.0.0.1` | exactly one reserved string | No redirect is sent, registered, or opened. |
| `hang_button` | `3` | integer `1..4` | The slot containing selected `PROC_HANG` owns the actual button. |

## Set alerts

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `temperature_warning_enabled` | `1` | boolean | CPU/GPU temperature alerts and pane flashing. |
| `memory_warning_enabled` | `1` | boolean | RAM/VRAM alerts. |
| `cpu_temp_warning` | `85` | `0..150`; not above critical | CPU warning. |
| `cpu_temp_critical` | `95` | `0..150`; not below warning | CPU critical alert. |
| `gpu_temp_warning` | `83` | `0..150`; not above critical | GPU warning. |
| `gpu_temp_critical` | `90` | `0..150`; not below warning | GPU critical alert. |
| `memory_warning` | `100` | `0..100`; `0` is compatibility disable | Physical memory warning at or above value. |
| `vmem_warning` | `100` | `0..100`; `0` is compatibility disable | Video memory warning at or above value. |
| `critical_alert_linger_ms` | `3000` | `0..60000` | Keep a recovered critical alert for this long. |

Use the category switches to turn alerts off. Existing installed threshold
values are preserved during update. Only current readings create alerts. A
warning clears on the first current recovered reading. A critical alert starts
its linger time on that reading. Unknown or stale data does not prove recovery.
Button 4 acknowledges the highest active alert before its normal slot action.

## Set logging

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `log_level` | `info` | `debug`, `info`, `warn`, `error`; restart required | Minimum log level. |
| `log_max_bytes` | `2097152` | `65536..104857600`; restart required | Size of each log file. |
| `log_backups` | `3` | `1..20`; restart required | Rotated log count. |

The default log is `%LOCALAPPDATA%\LCDSirPlus\lcdsirplus.log`. An explicit
`--diagnostic-dir PATH` chooses another directory for that run.

## Include another file

| Directive | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `include` | none | exactly one relative path; up to 64 include lines and 8 levels | Load an override file. |

Example:

```text
include lcdsirplus.local.txt
```

The path is relative to the file containing the line. Absolute paths, `..`, and
environment-variable syntax are rejected. Include loops are rejected. A file
whose case-insensitive path spelling is already completed is not loaded again;
this suppression is not a physical-file identity check. Later included values
replace earlier files. Installed include files belong under
`%LOCALAPPDATA%\LCDSirPlus\Config`, which update and uninstall preserve.
