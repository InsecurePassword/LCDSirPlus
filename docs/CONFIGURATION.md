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
`log_max_bytes`, and `log_backups` take effect after restart. The five advanced
compatibility-only settings are accepted but ignored.

## Write valid lines

Use one key followed by whitespace-separated values. Use single or double
quotes around values with spaces. `#` starts a comment when it appears between
values. Keys are case-insensitive. Button option names are also
case-insensitive.

Boolean values accept `1`, `true`, `yes`, or `on`, and `0`, `false`, `no`, or
`off`. Each string is limited to 4096 bytes and cannot contain control
characters. One line can contain at most 256 values. Other limits are 8 include
levels, 64 files, 64 include lines, 8 MiB total input, 16384 lines, and 16 KiB
per token. Duplicate keys with the same spelling in one file are rejected.

Six keys join their values into one string before use: `date_format`,
`time_format`, `logitech_friendly_name`, `presentmon_path`, `ccd_cache_processors`
and `ccd_frequency_processors`. The joined value is capped at 4096 bytes, counted
over every value plus the single spaces the join inserts, so the cap applies to the
total and not only to one token. An over-limit joined value is rejected with the
key's own name, such as `ccd_cache_processors exceeds limit of 4096 bytes`.

An `auto` interval uses the numeric default shown in its table. Other selectors
that accept `auto` are described separately.

For display message meanings and setup tasks, see the
[instruction manual](INSTRUCTION-MANUAL.md#read-display-messages).

## Set the app and preview

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `config_refresh_ms` | `1000` (`auto`) | `100..60000` | Settings file check interval. |
| `telemetry_interval_ms` | `300` (`auto`) | `100..10000` | Fast reading interval. |
| `render_interval_ms` | `100` (`auto`) | `25..5000` | Screen update interval. |
| `main_display` | `1` | integer `1`, `2`, or `3` | Main layout. |
| `preview_mode` | `auto` | `auto`, `always`, `never` | Initial preview visibility. |
| `preview_scale` | `4` | integer `1..10`; restart required | Preview size. |
| `start_minimized` | `0` | boolean | Do not automatically show preview at startup. |
| `start_at_login` | `0` | boolean | Portable current-user startup entry; ignored in safe mode and installed mode. |
| `safe_mode` | `0` | boolean | Restrict the normal dashboard to local CPU, RAM, and clock data; harmless slot cycling remains available. |

`preview_mode auto` shows the preview when neither physical G13 connection is
active. A tray toggle overrides later automatic visibility changes.

Layouts are fixed:

- `1`: CPU/RAM and GPU/VRAM halves.
- `2`: CPU/RAM, GPU/VRAM, and outgoing/incoming thirds.
- `3`: CPU/RAM and incoming/outgoing halves.

Layouts 2 and 3 scale inbound/IN bars with
`network_graph_ceiling_download_mbps` and outbound/OUT bars with
`network_graph_ceiling_upload_mbps`.

## Set button slots

| Key | Default | Constraint |
|---|---|---|
| `slot_0` | `HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY` | One or more of the 50 button values. |
| `slot_1` | `FPS_CURRENT NET_BOTH NET_GRAPH FRAME_TIME SESSION_TIME` | One or more button values. |
| `slot_2` | `PROC_HANG FPS_1LOW DISK_IO DISK_IO_GRAPH GPU_TEMP PING JITTER AUDIO` | One or more button values. |
| `slot_3` | `THERMALS POWER_LIMIT` | One or more button values. |

The lists map to physical and preview buttons 1 through 4. A short press moves
through one list. `PROC_HANG` may appear only once across all four lists. See
the canonical [50-value button option reference](../modules.md) and the manual's
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
Each joined list is capped at 4096 bytes in total (see "Write valid lines"), and an
over-limit list is reported under the key's own name, not as a generic
processor-list message.

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

The file is parsed locally as untrusted, read-only input and limited to 1 MiB,
4096 application records, and 64 detected paths per record. A usable record must
provide all expected typed fields, including a boolean `IsFingerprintDetected`.
Because that value can be stale, either boolean value is accepted; `false` is
tolerated only for an otherwise-eligible exact path. The record must still be
noncreative, OPS-supported, and nonmanual, and the path must be a safe normalized
fixed-drive path. The running executable's normalized full path must match it
exactly, case-insensitively; this is not a physical-file identity check.
LCDSirPlus never writes or lists the catalog, logs its raw JSON, names, paths, or
record details, starts NVIDIA software, uses DRS, reads Xbox catalogs, or
contacts a catalog service.

With `presentmon_persist 0`, a usable catalog rejects non-game paths. If the
catalog is unavailable, unsafe, changing, too large, malformed, or has an
unsupported shape, automatic workload fallback may still select another app.
There is no user-maintained game list.

With `presentmon_deferred 1`, unavailable PresentMon button options are skipped
only for display. The saved selection does not change. `PROC_HANG` is never
selected as the temporary result. An all-unavailable list
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

## Set graph ceilings

| Key | Default | Accepted value or constraint | What it controls |
|---|---:|---|---|
| `network_graph_ceiling_download_mbps` | `1000` | finite `1..100000` | Inbound/IN network graphs and layout 2/3 current bars. |
| `network_graph_ceiling_upload_mbps` | `1000` | finite `1..100000` | Outbound/OUT network graphs and layout 2/3 current bars. |
| `network_graph_ceiling_mbps` | none | finite `1..100000` | Active legacy alias that sets both directional network ceilings at that point in parse order. |
| `disk_graph_ceiling_mbps` | `1000` | `1..100000` | Disk graph. |
| `fps_graph_ceiling` | `240` | `1..1000` | FPS graph. |
| `cpu_temp_max_c` | `90` | `1..150` | CPU temperature graph and history scale only. |
| `gpu_temp_max_c` | `90` | `1..150` | GPU temperature graph and history scale only. |

`cpu_temp_max_c` and `gpu_temp_max_c` are graph-scale settings. They no longer
flash a pane, invert a panel, or feed any alarm: the whole non-hang warning,
alert, flashing, and bottleneck subsystem was removed, so no ordinary reading
requests attention. A temperature above its ceiling simply draws a full-height
graph; the numeric reading stays exact and unlabelled.

All graphs cover the trailing 30 seconds. CPU/GPU load graphs use a fixed
0..100% scale. `FRAME_TIME` scales to its observed 30-second range. Ceilings do
not clip numeric text.

Set the directional ceilings to the downstream and upstream rates in your
internet plan; LCDSirPlus does not auto-detect them. For a 1000 Mbps downstream,
40 Mbps upstream plan:

```text
network_graph_ceiling_download_mbps 1000
network_graph_ceiling_upload_mbps 40
```

Each direction independently reaches full graph height or bar width at its own
ceiling. Download means inbound/IN: `NET_IN_GRAPH`, the top half of `NET_GRAPH`,
the layout 2 IN bar, and the layout 3 NET IN bar. Upload means outbound/OUT:
`NET_OUT_GRAPH`, the bottom half of `NET_GRAPH`, the layout 2 OUT bar, and the
layout 3 NET OUT bar. Numeric `NET_IN`, `NET_OUT`, and `NET_BOTH` values are not
scaled by these settings.

Old files may keep `network_graph_ceiling_mbps`; it remains an active alias, not
an ignored compatibility key. Assignments execute sequentially across includes:
the legacy spelling sets both directions when encountered, and a later
directional assignment replaces only that direction. A later legacy assignment
replaces both. Distinct legacy and directional spellings may coexist in one
file, but repeating the same spelling in one file is an error.

## Set Discord

| Key | Default | Accepted value or constraint | What it controls |
|---|---|---|---|
| `discord_enabled` | `1` | boolean | Discord voice-state integration; safe mode disables Discord access by the normal dashboard. |
| `discord_client_id` | empty | digits only | Public application ID. Empty means not configured. |
| `discord_linger_ms` | `700` | `0..10000` | How long the provider keeps a speaker in the published voice state after speaking stops. |
| `discord_max_speakers` | `2` | `1..4` | Speaker entries the provider tracks before reporting `+N`. |
| `discord_show_self` | `0` | boolean | Include the current user in the published voice state. |
| `discord_show_channel` | `0` | boolean | Voice channel name carried in the published voice state. |

The panel no longer shows a speaker list. The automatic full-screen
active-speaker takeover was removed, because only the hung-window feature may
replace your selected display without your request; `PROVIDER_STATUS` keeps
reporting the `Discord` connection row, and the settings above continue to shape
the provider's published voice state. No replacement speaker pane exists, and
retiring these four settings is a separate product decision.

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

This is the product's only attention path. No ordinary reading produces an alert,
episode, flash, full-screen warning, sticky banner, bottleneck verdict, or
acknowledgement step, so nothing can intercept this button, steal the frame, or
queue ahead of a hung-window panel. A requested termination is reported as
requested: `TerminateProcess` can return before the process exits, and pending
I/O can delay it, so no outcome is presented as a confirmed recovery.

## Advanced compatibility-only settings

These keys keep older configuration files valid. LCDSirPlus parses and validates
them but ignores their values. New files should omit them.

| Key | Default | Accepted value or constraint | Compatibility behavior |
|---|---:|---|---|
| `date_format` | `yyyy-MM-dd dddd` | non-empty reserved string; not validated as a Windows format | Header date remains fixed. |
| `time_format` | `HH:mm:ss` | non-empty reserved string; not validated as a Windows format | Ignored; Windows Short time controls output. |
| `discord_redirect_uri` | `http://127.0.0.1` | exactly one reserved string | No redirect is sent, registered, or opened. |
| `hang_button` | `3` | integer `1..4` | The slot containing selected `PROC_HANG` owns the actual button. |

## Retired: alerts, warnings, and bottleneck

LCDSirPlus is a monitoring dashboard: you choose which readings to observe, and
the program no longer classifies ordinary resource conditions into
attention-demanding warnings. The complete non-hang warning/alert subsystem and
the complete bottleneck classifier were deleted, not disabled: no evaluator,
threshold, retained episode, severity ranking, linger timer, queue,
acknowledgement state, full-screen warning, pane flash, `ALERTS` option,
`BOTTLENECK` option, or alarm-only polling remains, and there is no hidden
toggle, no compatibility shim, and no replacement alarm classifier.

The hung-process `PROC_HANG` feature is the only remaining attention path. Its
detection, target presentation, short-press and long-press handling,
cancellation, safe-mode refusal, guarded termination, audit lines, and outcomes
are unchanged, and no ordinary alert can intercept its input or cover its panel.

These settings are retired. They are ordinary unknown keys now: a file that
still sets one is rejected with the normal `unknown key` diagnostic, and the
last valid configuration stays active. Remove the lines; nothing is ignored,
and no recurring obsolete-key warning is emitted.

```text
bottleneck_cpu_percent   bottleneck_gpu_percent   bottleneck_memory_percent
bottleneck_disk_mbps     bottleneck_sustain_ms    warning
temperature_warning_enabled   memory_warning_enabled
cpu_temp_warning         cpu_temp_critical        gpu_temp_warning
gpu_temp_critical        memory_warning           vmem_warning
critical_alert_linger_ms headset_warn_percent     headset_critical_percent
```

`headset_enabled` and the battery polling controls stay, and keep reporting
headset battery percentage, charging, online, and AC states: only the
low/critical battery alarms are gone. `cpu_temp_max_c` and `gpu_temp_max_c`
stay as graph scales (see "Set graph ceilings"). No warning setting may force a
faster render cadence any more, so `render_interval_ms` keeps only its own
`25..5000` bound and your chosen interval is honored.

The shipped template and a migrated copy of an installed file are provided with
the release notes; the migration removes only the retired assignments and the
retired `ALERTS`/`BOTTLENECK` button values, preserves every other line, order,
quote, and include, and replaces a button list that became empty with `CLOCK`.
Default slot 4 is now `THERMALS POWER_LIMIT`.

`headset_estimate_hours` retired the same way, and its compatibility-only entry
was removed with it. It was parsed, validated and ignored, so it kept old files
valid; that is no longer true. A file that still sets it is rejected as an
unknown key, a running instance keeps its last valid configuration, and
`--validate-config` exits 2. Remove the line.

`CPU_CACHE_TEMP` and `CPU_FREQ_TEMP` are retired button values: no data source
ever published them, so both showed `N/A`. A slot line naming either one is now
rejected with `unknown button option`, and the last valid configuration stays
active. Replace the name with `CPU_TEMP`.

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
