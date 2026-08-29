# LCDSirPlus 0.3.0 configuration reference

Format: LCDSirReal-style. `#` comments, whitespace-separated key/value
lines, quoted values for embedded spaces, `include` override files. The
parser enforces the same limits as the Go implementation: 8 include depth,
64 files, 8 MiB aggregate, 16384 lines, 256 list items, 16 KiB tokens,
duplicate keys rejected per file.

Hot reload: saving a valid file applies immediately; invalid changes are
rejected and the last valid configuration stays active. Tokenization and parse
errors identify the source file and line. Final range and cross-field validation
errors may report line `0` because the parser does not retain one exact source
line for those checks.

Selectors generally default to `auto`; `network_probe_method` accepts `auto`
but intentionally defaults to `icmp`.

Boolean settings accept `1`, `true`, `yes`, or `on`, and `0`, `false`, `no`,
or `off`, case-insensitively. Unless a row says otherwise, one value is
required; strings are limited to 4096 bytes and cannot contain control
characters. An `auto` interval resolves to the numeric default shown.

The default primary file is `lcdsirplus.txt` beside `LCDSirPlus.exe`. Normal
runtime, validation, hardware tests, and Discord authorization/credential
removal accept `--config PATH`; diagnostics intentionally ignores it. Cargo
builds copy the canonical root file beside the debug or release executable.

## Core

| Key | Default | Range / values | Purpose / dependency |
|---|---|---|---|
| `config_refresh_ms` | 1000 (`auto`) | 100..60000 | Check the primary/include graph for hot-reload changes. |
| `telemetry_interval_ms` | 300 (`auto`) | 100..10000 | Poll fast native telemetry and build snapshots. |
| `render_interval_ms` | 100 (`auto`) | 25..5000 | Render cadence; must be <=100 when `warning=1`. |
| `preview_mode` | `auto` | `auto` \| `always` \| `never` | Control preview visibility; `auto` treats connected HID and SDK transports as physical backends. |
| `preview_scale` | 4 | 1..10 | Integer preview-window magnification. |
| `start_minimized` | 0 | boolean | Suppress automatic preview display when the application starts automatically. |
| `start_at_login` | 0 | boolean | Own the current-user Run value; ignored in safe mode. |
| `safe_mode` | 0 | boolean | Disable optional providers, network/action paths, and startup mutation. |
| `date_format` | `yyyy-MM-dd dddd` | non-empty Windows GetDateFormat token string | Accepted/reserved fixed-header date format. |
| `time_format` | `HH:mm:ss` | non-empty Windows GetTimeFormat token string | Accepted/reserved fixed-header time format. |

`preview_mode auto` shows the preview only when neither direct HID nor the
Logitech SDK is connected (LCDSirReal `testwindow` semantics). A tray toggle
takes precedence over later `auto` backend changes, and `start_minimized`
suppresses automatic preview display. `start_at_login` owns only the current
user's `LCDSirPlus` Run value and refuses to replace or remove a foreign value.
Safe mode never changes startup registration.

`start_at_login 1` is required for persistent automatic startup. Installer
`-EnableLogin` creates the same owned Run value immediately, but the application
synchronizes that value to configuration and removes it if `start_at_login`
remains `0`. It never replaces or removes a foreign value with the same name.

The clock and fixed header currently use the default date and time formats shown
above. `date_format` and `time_format` are accepted and reserved until runtime
wiring is completed; changing them does not currently alter displayed text.
`preview_scale`, `log_level`, `log_max_bytes`, and `log_backups` are also read
successfully during hot reload but take effect only after restart. Other valid
changes apply live unless their row says otherwise.

## Slots (fixed four-button contract)

| Key | Default | Range / values | Purpose / dependency |
|---|---|---|---|
| `slot_0` | `HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY` | one or more of the 53 module tokens below | Ordered module cycle for physical/preview button 1. |
| `slot_1` | `FPS_CURRENT FPS_1LOW FRAME_TIME SESSION_TIME` | one or more module tokens | Ordered module cycle for button 2. |
| `slot_2` | `PROC_HANG GPU_TEMP PING JITTER AUDIO` | one or more module tokens | Ordered module cycle for button 3. |
| `slot_3` | `THERMALS PACKET_LOSS MIC_STATUS SESSION_SUMMARY PROVIDER_STATUS` | one or more module tokens | Ordered module cycle for button 4; alert acknowledgement takes priority. |

A short press cycles only the matching slot. Module names are case-insensitive;
the reference below uses their canonical tokens. `PROC_HANG` may occur at most
once across all four lists.

## Display options

All 53 accepted options are grouped here for navigation. For a one-line list
that can be kept beside the application, see [`modules.md`](../modules.md).

| Group | Options |
|---|---|
| Devices and audio | [`HEADSET_BATTERY`](#headset_battery), [`CONTROLLER_BATTERY`](#controller_battery), [`AUDIO`](#audio), [`MIC_STATUS`](#mic_status) |
| Game performance | [`FPS_CURRENT`](#fps_current), [`FPS_1LOW`](#fps_1low), [`FPS_01LOW`](#fps_01low), [`FRAME_TIME`](#frame_time), [`SESSION_TIME`](#session_time), [`SESSION_SUMMARY`](#session_summary), [`GAME_NAME`](#game_name) |
| Temperatures and utilization | [`CPU_TEMP`](#cpu_temp), [`GPU_TEMP`](#gpu_temp), [`CPU_LOAD`](#cpu_load), [`RAM_USAGE`](#ram_usage), [`GPU_LOAD`](#gpu_load), [`VRAM_USAGE`](#vram_usage), [`CPU_CACHE_TEMP`](#cpu_cache_temp), [`CPU_FREQ_TEMP`](#cpu_freq_temp) |
| Thirty-second graphs | [`CPU_LOAD_GRAPH`](#cpu_load_graph), [`GPU_LOAD_GRAPH`](#gpu_load_graph), [`CPU_TEMP_GRAPH`](#cpu_temp_graph), [`GPU_TEMP_GRAPH`](#gpu_temp_graph), [`DISK_IO_GRAPH`](#disk_io_graph), [`FPS_GRAPH`](#fps_graph) |
| Cooling, board, and power | [`VRM_TEMP`](#vrm_temp), [`CHIPSET_TEMP`](#chipset_temp), [`MOTHERBOARD_TEMP`](#motherboard_temp), [`CPU_FAN`](#cpu_fan), [`PUMP_RPM`](#pump_rpm), [`POWER_LIMIT`](#power_limit), [`CPU_GPU_POWER`](#cpu_gpu_power), [`THERMALS`](#thermals) |
| System detail | [`DISK_IO`](#disk_io), [`RAM_DETAIL`](#ram_detail), [`CONNECTIONS`](#connections), [`SYSTEM_BATTERY`](#system_battery), [`HARD_FAULTS`](#hard_faults) |
| Network throughput | [`NET_IN`](#net_in), [`NET_OUT`](#net_out), [`NET_BOTH`](#net_both), [`NET_IN_GRAPH`](#net_in_graph), [`NET_OUT_GRAPH`](#net_out_graph), [`NET_GRAPH`](#net_graph) |
| Network quality | [`PING`](#ping), [`JITTER`](#jitter), [`PACKET_LOSS`](#packet_loss), [`NET_HEALTH`](#net_health) |
| General status and action | [`CLOCK`](#clock), [`ALERTS`](#alerts), [`PROVIDER_STATUS`](#provider_status), [`BOTTLENECK`](#bottleneck), [`PROC_HANG`](#proc_hang) |

Unless an option says otherwise, `N/A` means no usable reading has been
received. `STALE` means a previously valid reading is retained but its source
has not refreshed it in time. Safe mode disables optional providers.

### `HEADSET_BATTERY`

Shows battery state from an explicitly supported SteelSeries wireless USB
receiver family: Arctis 1/7X/7P, Arctis 9, Arctis Pro Wireless, Arctis 7+ and
7P+, Arctis Nova 3/5/7 Wireless variants, or Arctis GameBuds. This is not
generic support for every SteelSeries, Bluetooth, wired, or USB audio device.
The receiver must be connected through the supported USB HID control interface;
Bluetooth alone is insufficient. SteelSeries GG may run at the same time, but
LCDSirPlus reads the receiver directly and neither requires nor reads GG.

Battery meaning depends on the receiver profile. Some report a direct or scaled
percentage, some report only 0/25/50/75/100% bands, and 7+/7P+ receivers may
report either form. GameBuds displays the lower percentage of the currently
active earbuds; docked earbuds and case charge are not readings. Charging is
shown only where the profile reports it. While charging, the title row reads
`CHARGING` and the value remains a complete percentage such as `75%`. The
normal `SS BAT` title does not prove that a profile is not charging.

The provider admits only VID `1038` and the exact PID, USB interface, HID usage,
and report-size profile below. PID `2212` (the known-working Arctis 7P+ baseline)
is tried first when multiple supported receivers are present.

| Family/profile | Exact PIDs (VID `1038`) | USB control collection | Battery / charging |
|---|---|---|---|
| Arctis 1/7X/7P Wireless | `12B3`, `12B6`, `12D7`, `12D5` | interface 3, usage `FF43:0202` | direct; no charging flag |
| Arctis 9 | `12C2` | interface 0, usage `FFC0:0001` | scaled percentage; charging available |
| Arctis Pro Wireless | `1290` | interface 0, usage `FF00:0001` | 25% bands; no charging flag |
| Arctis 7+/7P+ receiver variants | `220E`, `2212`, `2216`, `2236` | interface 3, usage `FFC0:0001` | direct or 25% bands; charging available |
| Arctis Nova 7/7X/7P variants, coarse profile | `2202`, `2206`, `220A`, `223A`, `227A`, `22A4`, `22AB` | interface 3, usage `FFC0:0001` | 25% bands; charging available |
| Arctis Nova 7/7X/7P variants, direct profile | `22A1`, `227E`, `2258`, `229E`, `22A9`, `22A5`, `22A7`, `2298`, `22AD` | interface 3, usage `FFC0:0001` | direct; charging available |
| Arctis Nova 5/5X and Nova 3P/3X Wireless receiver variants | `2232`, `2253`, `2264`, `2269`, `226D` | interface 3, usage `FFC0:0001` | direct; charging available |
| Arctis GameBuds | `230A` | interface 3, usage `FFC0:0001` | minimum active earbud; no charging flag |

The table contains 32 unique PIDs across eight protocol profiles. A family name
or PID alone is not enough if the required control collection is absent. The
legacy Arctis 7/7 (2019) PIDs `1260` and `12AD` and Nova Pro Wireless PIDs
`12E0`, `12E5`, and `225D` are unsupported because their exact command-bearing
usage was not evidenced. Other PIDs deliberately not admitted are `1252`,
`1280`, `12EC`, `220C`, `2200`, `2204`, `2208`, `2267`, `230C`, `231A`, `1292`,
and `1297`; every other unknown PID also fails closed. Wired, Bluetooth-only,
receiverless, and unrelated SteelSeries devices are unsupported.

`NO DONGLE` means no exact compatible receiver/control collection was read, the
provider is off, or querying failed before a valid result. `OFFLINE` means a
supported receiver answered that the headset or active earbuds were disconnected
or off. `STALE` means the last valid result exceeded `headset_stale_ms`; safe mode
shows `DISABLED`.

For diagnostics, find the receiver PID in Device Manager under **Properties >
Details > Hardware Ids**, or list present SteelSeries Plug and Play entries:

```powershell
Get-PnpDevice -PresentOnly | Where-Object InstanceId -Match 'VID_1038&PID_' |
  Select-Object FriendlyName, InstanceId
```

Match the four hexadecimal PID digits to the table. If the PID is listed but the
slot remains `NO DONGLE`, check the LCDSirPlus log for an open, timeout, interface,
usage, or report-length error; reconnect the USB receiver directly and retry.

| Setting | Default | Range / constraint / purpose |
|---|---|---|
| `headset_enabled` | `1` | boolean; enable receiver battery polling (safe mode disables it) |
| `headset_poll_ms` | `15000` (`auto`) | 1000..600000; receiver query cadence when enabled |
| `headset_query_timeout_ms` | `1200` (`auto`) | 100..10000; bound one complete HID query |
| `headset_stale_ms` | `45000` (`auto`) | >= `headset_poll_ms`; <= 3600000; expire retained readings |
| `headset_warn_percent` | `25` | 0..100; warning episode threshold |
| `headset_critical_percent` | `0` | 0..100; <= `headset_warn_percent`; critical threshold |
| `headset_estimate_hours` | `1` | boolean; accepted/reserved estimate toggle |

Related settings: `headset_enabled`, `headset_poll_ms`,
`headset_query_timeout_ms`, and `headset_stale_ms`. `headset_warn_percent` and
`headset_critical_percent` control headset alerts, not the displayed percentage.
`headset_estimate_hours` is accepted but the current display does not estimate
hours remaining.

### `CONTROLLER_BATTERY`

Shows the selected Windows XInput gamepad as player `P1` through `P4`. A
wireless battery uses XInput's coarse Empty/Low/Medium/Full levels, displayed as
0/33/66/100%; a wired controller shows `P# WIRED` because XInput supplies no
battery percentage for it. `controller_index -1` selects the first connected
index from 0 through 3; an explicit 0..3 selects only that index. `OFFLINE`
means the selected controller is not connected, and provider failure also
leaves no battery reading.

Related settings: `controller_enabled`, `controller_index`, and
`controller_poll_ms`. Requires an XInput-compatible controller and Windows
XInput support.

### `AUDIO`

Shows the Windows default multimedia playback/output endpoint's master volume
as 0..100%. This is the system output volume, not one application's audio level
and not microphone volume. `N/A` means the default output endpoint or its Core
Audio volume control could not be read. There is no retained `STALE` audio
value.

Related settings: `audio_enabled` and `audio_poll_ms`. No external software is
required beyond a working Windows playback device.

### `MIC_STATUS`

Shows `LIVE` or `MUTED` from the mute state of the Windows default multimedia
recording endpoint. It reports the endpoint's master mute, not whether an
application is currently recording. `N/A` means there is no readable default
recording device. There is no retained `STALE` microphone state.

Related settings: `audio_enabled` and `audio_poll_ms`. Requires a Windows
recording endpoint; it shares the Core Audio poll used by `AUDIO`.

### `FPS_CURRENT`

Shows the current FPS of the PresentMon target. LCDSirPlus averages recent frame
times from approximately the last second and converts that time to frames per
second, so the value is a stable current rate rather than one instantaneous
frame. Higher is better. `N/A` means there is no active captured frame stream;
`STALE` appears after five seconds without a new frame, and the reading expires
after a further five seconds.

Related settings: `presentmon_enabled`, `presentmon_path`,
`presentmon_interval_ms`, `presentmon_target_mode`, `presentmon_process_name`,
and `presentmon_exclude`. Release packages include the official signed
PresentMon v2.5.1 console as `PresentMon.exe`; a selected process must be
actively presenting.

### `FPS_1LOW`

Shows 1% low FPS for the selected PresentMon target over
`presentmon_window_ms`. In plain language, LCDSirPlus takes the slowest 1% of
captured frame times in that window, averages them, and converts the result to
FPS. It describes brief slowdowns that an average FPS can hide; higher is
better. `N/A`, `STALE`, and expiry follow `FPS_CURRENT`.

Related settings and requirements are the same as `FPS_CURRENT`, plus
`presentmon_window_ms` (default 60000 ms, range 5000..600000).

### `FPS_01LOW`

Shows 0.1% low FPS over `presentmon_window_ms`. It averages the slowest one
tenth of one percent of captured frame times and converts them to FPS, exposing
rarer and more severe hitches than the 1% low. With a small sample set, at least
one slow frame is used. Higher is better. `N/A`, `STALE`, and expiry follow
`FPS_CURRENT`.

Related settings and requirements are the same as `FPS_1LOW`.

### `FRAME_TIME`

Shows how many milliseconds are required to render the current frame-time
reading; lower is better. The displayed current value is the recent
approximately one-second mean used for current FPS, and the two measurements
are inverses: about 16.7 ms is 60 FPS and about 6.9 ms is 144 FPS. A miniature
graph plots exactly the trailing 30 one-second bins and scales to the observed
minimum and maximum in that window. `N/A`, `STALE`, and expiry follow
`FPS_CURRENT`, and unavailable/stale values do not draw a graph.

Related settings and requirements are the same as `FPS_CURRENT`.

### `SESSION_TIME`

Shows elapsed time for the active PresentMon game/capture session (`mm:ss`, or
`h:mm` after one hour). A session starts when capture starts for a selected
target and frame accumulation begins with the first accepted frame. It resets
when the target process or capture identity changes, PresentMon restarts, or a
capture-affecting setting such as path, interval, window, or stutter threshold
starts a new capture. With no active session it shows `00:00`; it does not use
`N/A` or `STALE` text.

Related settings and requirements are the PresentMon settings listed for
`FPS_CURRENT`, including `presentmon_window_ms` and `stutter_threshold_ms`.

### `SESSION_SUMMARY`

Shows the active PresentMon session's elapsed time followed by its accumulated
stutter count, for example `12:34 8S`. A stutter is each captured frame whose
frame time is greater than or equal to `stutter_threshold_ms` (default 33.34
ms, range 1..1000). The count covers the whole current session, not only
`presentmon_window_ms`, and resets with the session. Large values are compacted
without dropping either field. `IDLE` means there is no active frame stream;
the summary does not use `N/A` or `STALE` text.

Related settings and requirements are the same as `SESSION_TIME`, especially
`stutter_threshold_ms`.

### `GAME_NAME`

Shows the selected PresentMon target executable's filename stem. If that value
is unavailable, it falls back to the target process name; no friendly-name
source is used. Long names are shortened to fit the LCD. `N/A` means PresentMon
has no selected target. During the short stale-data period the last target name
can remain visible until the capture expires.

Related settings and requirements are the PresentMon target settings listed
for `FPS_CURRENT`.

### `CPU_TEMP`

Shows CPU temperature in degrees Celsius. Current Windows has no generic API
for CPU package temperature, and LCDSirPlus never probes raw MSRs, SMBus, EC,
or Super-I/O registers. Source priority is a configured exact HWiNFO original-
label pair, then LibreHardwareMonitor loopback JSON. LHM automatic selection
prefers CPU Package, then Tctl/Tdie, CPU, and finally a core; an exact
`lhm_cpu_temp_sensor` ID overrides that choice.

`N/A` can mean no pair/ID was configured, HWiNFO shared memory or the LHM web
server is not running, the pair/ID is missing or ambiguous, the unit/value is
invalid, the source is stale, or the source ABI is unsupported. For the
preferred fallback setup, start user-managed HWiNFO in Sensors-only mode,
enable **Shared Memory Support**, and run:

```powershell
.\LCDSirPlus.exe --list-hwinfo-sensors
```

Copy the exact case-sensitive `sensor=` and `reading=` original labels from one
temperature line into `hwinfo_cpu_temp_sensor` and
`hwinfo_cpu_temp_reading`. Both keys must be present together. HWiNFO64 Free
limits shared-memory monitoring to 12 hours; for unattended use, use HWiNFO32
or an appropriately licensed HWiNFO Pro edition according to HWiNFO's vendor
terms. LCDSirPlus does not bundle, start, configure, restart, or license HWiNFO.

LHM remains optional: the user must explicitly start it and safely enable its
web server. LCDSirPlus neither enables nor manages that server and permits only
loopback HTTP. Related settings are the HWiNFO CPU pair and stale limit,
`lhm_mode`, `lhm_url`, `lhm_interval_ms`, `lhm_stale_ms`,
`lhm_cpu_temp_sensor`, `cpu_temp_max_c`, and the separate alert thresholds.

### `GPU_TEMP`

Shows the first selected GPU's core temperature in degrees Celsius. The primary
source is the vendor driver API selected by `gpu_provider`: `auto` tries NVIDIA
NVAPI and then AMD ADLX. If the native temperature is missing or stale,
LibreHardwareMonitor supplies the fallback when enabled; its automatic order is
NVIDIA, AMD, then Intel and prefers a sensor whose name contains Core.
`lhm_gpu_temp_sensor` can pin an exact LHM SensorId. `N/A` means neither source
has a valid temperature, and `STALE` means only an expired retained reading is
available.

Related settings: `gpu_provider`, `telemetry_interval_ms`, `lhm_mode`,
`lhm_url`, `lhm_interval_ms`, `lhm_stale_ms`, `lhm_gpu_temp_sensor`,
`gpu_temp_warning`, and `gpu_temp_critical`. Native readings require a supported
vendor GPU and installed driver; fallback requires LibreHardwareMonitor's local
web server.

### `CPU_LOAD`

Shows whole-system CPU utilization from Windows scheduler accounting as
0..100%, averaged across all logical processors over the latest telemetry
sample interval. It is total CPU busy time, not the Cache/Frequency split used
by the fixed dashboard. An empty or unusable Windows logical-processor sample
is currently published and displayed as a valid `0%`; the reading is not
retained as `STALE`.

Related setting: `telemetry_interval_ms`. No external software is required.

### `RAM_USAGE`

Shows physical RAM utilization percentage plus used/total capacity in IEC
units. All values come from native Windows `GlobalMemoryStatusEx`; no external
software or option-specific setting is required.

### `GPU_LOAD`

Shows utilization of the first selected GPU as 0..100%, using NVIDIA NVAPI or
AMD ADLX driver telemetry. `N/A` means the selected native provider is disabled,
unsupported, unhealthy, or did not return utilization. `STALE` means a prior
reading has not refreshed for at least three telemetry intervals or three
seconds, whichever is longer. LibreHardwareMonitor is not a load fallback.

Related settings: `gpu_provider` and `telemetry_interval_ms`. Requires a
supported vendor GPU and installed driver.

### `VRAM_USAGE`

Shows dedicated video-memory utilization percentage and used/total capacity in
IEC units for the first selected GPU. Both byte values and a valid percentage
must come from NVIDIA NVAPI or AMD ADLX; otherwise the complete slot shows
`N/A`. `STALE` means retained native GPU readings exceeded the same freshness
limit as `GPU_LOAD`. LibreHardwareMonitor is not a VRAM fallback.

Related settings and requirements: `gpu_provider` and `telemetry_interval_ms`,
plus a supported vendor GPU and installed driver. `vmem_warning` controls VRAM
alerts but does not change this display.

### `CPU_CACHE_TEMP`

Reserved for the temperature, in degrees Celsius, of the CPU domain identified
as the Cache/V-Cache domain. Display requires exactly one Cache domain and one
matching domain-temperature reading. No current provider publishes those
per-domain temperatures, including LibreHardwareMonitor, so this option
currently shows `N/A`; it cannot currently become `STALE`.

`ccd_source` and `ccd_cache_processors` identify CPU load domains but do not
create temperature readings. There is currently no setting or external program
that enables this slot.

### `CPU_FREQ_TEMP`

Reserved for the temperature, in degrees Celsius, of the CPU domain identified
as the Frequency domain. It has the same exact-one-domain requirement as
`CPU_CACHE_TEMP`. No current provider publishes this reading, so the option
currently shows `N/A` and cannot currently become `STALE`.

`ccd_source` and `ccd_frequency_processors` identify CPU load domains but do not
create temperature readings. There is currently no setting or external program
that enables this slot.

### `CPU_LOAD_GRAPH`

Shows current whole-system CPU load and exactly the trailing 30 seconds as 30
one-second columns. The fixed scale is 0..100%; values are never auto-scaled.
Missing seconds remain blank, and an unavailable or stale current reading
replaces the graph with `N/A` or `STALE`. Source: native Windows scheduler
accounting. Related setting: `telemetry_interval_ms`.

### `GPU_LOAD_GRAPH`

Shows current first-selected-GPU load and exactly the trailing 30 seconds on a
fixed 0..100% scale. It uses native NVIDIA NVAPI or AMD ADLX telemetry; LHM is
not a load fallback. Missing, unavailable, and stale behavior matches
`CPU_LOAD_GRAPH`. Related settings: `gpu_provider` and
`telemetry_interval_ms`.

### `CPU_TEMP_GRAPH`

Shows current CPU temperature and exactly the trailing 30 seconds. Graph height
uses `cpu_temp_max_c` as a fixed ceiling; samples at or above it reach full
height, but the numeric reading is not clipped. This same setting is the
pane-warning threshold. Source priority and setup are identical to `CPU_TEMP`.

### `GPU_TEMP_GRAPH`

Shows current GPU temperature and exactly the trailing 30 seconds. The fixed
ceiling and pane-warning threshold are both `gpu_temp_max_c`; the numeric
reading is not clipped. Source priority and setup are identical to `GPU_TEMP`.

### `VRM_TEMP`

Shows a configured voltage-regulator temperature in Celsius. There is no safe
generic Win32 or vendor API for this board-specific sensor, so it is populated
only by the exact LHM SensorId in `lhm_vrm_temp_sensor`. Automatic name guessing
is not used. The user must run LHM and explicitly enable its loopback web
server; otherwise the slot is `N/A` or `STALE`.

### `CPU_FAN`

Displays CPU-fan percentage, not raw RPM. An exact
`lhm_cpu_fan_control_sensor` supplies true reported control duty and takes
priority. If duty is absent, an exact `lhm_cpu_fan_rpm_sensor` can be divided by
`cpu_fan_max_rpm` and clamped to 0..100%. The default max RPM is `0`, which
disables that estimate. There is no low-level Super-I/O/EC probing.

### `PUMP_RPM`

Despite the compatibility token name, this slot displays pump percentage, not
raw RPM. It prefers true duty from exact `lhm_pump_control_sensor`; otherwise it
uses exact `lhm_pump_rpm_sensor / pump_max_rpm`, clamped to 0..100%. The default
max RPM `0` disables the estimate. LCDSirPlus performs no direct motherboard
controller probing.

### `POWER_LIMIT`

Shows only an authoritative configured whole-system/total-power reading in
watts. HWiNFO's exact `hwinfo_total_power_sensor` plus
`hwinfo_total_power_reading` pair is preferred; exact
`lhm_total_power_sensor` is the fallback. LCDSirPlus never invents this value by
adding component power. Without one of those exact total sensors it shows
`N/A`.

### `CPU_GPU_POWER`

Shows the complete current CPU-package plus GPU-board power subtotal in watts.
CPU power comes from an exact HWiNFO or LHM CPU-power selection. GPU power
prefers native vendor telemetry (NVML with the NVAPI path where safely aligned),
then exact HWiNFO or LHM selection. Both components must be current and valid;
otherwise the subtotal is `N/A`. It is deliberately separate from
`POWER_LIMIT` and is not whole-system power.

### `CHIPSET_TEMP`

Shows Celsius from exact `lhm_chipset_temp_sensor`. No generic safe native
source or automatic name match is used; LHM loopback must be user-enabled.

### `MOTHERBOARD_TEMP`

Shows Celsius from exact `lhm_motherboard_temp_sensor`. No generic safe native
source or automatic name match is used; LHM loopback must be user-enabled.

### `DISK_IO`

Shows separate `R` and `W` byte rates from Windows PDH
`PhysicalDisk(_Total)` counters. This is the system aggregate across physical
disks, not one selected disk or process. Display units are decimal B/s, KB/s,
MB/s, or GB/s. Both directions must be current.

### `DISK_IO_GRAPH`

Shows current aggregate read/write labels and exactly the trailing 30 seconds,
with reads above and writes below the center line. Both use the fixed
`disk_graph_ceiling_mbps` scale (default 1000, range 1..100000 MB/s). Its compact
direction labels retain spaces, for example `R 1M/W 1M`; the separate `DISK_IO`
pane retains fuller rates. Source and completeness rules match `DISK_IO`.

### `RAM_DETAIL`

Shows native physical memory as a fitted IEC `used/total` value, preferring GiB
(for example `31/32GiB`) and reducing precision or units only when necessary to
keep both values visible. Both byte readings come from `GlobalMemoryStatusEx`;
no external provider or setting is required.

### `FPS_GRAPH`

Shows current FPS and exactly the trailing 30 seconds. Graph height uses the
fixed `fps_graph_ceiling` (default 240, range 1..1000); the numeric FPS is not
clipped. It uses the bundled PresentMon capture and follows `FPS_CURRENT`
availability/staleness.

### `THERMALS`

Shows complete `CPU nC` and `GPU nC` rows. Both readings must be current; if
either is unavailable or stale, the combined slot is `N/A` or `STALE`. Each
side retains the native-first/fallback policy of its individual temperature
module. Either side can trigger pane inversion using its respective
`*_temp_max_c` setting.

### `CONNECTIONS`

Shows the total number of established TCP connections from native Windows IPv4
and IPv6 TCP tables. It counts only the established state, not listening,
closing, UDP, or per-process ownership. No external program is required.

### `NET_HEALTH`

Combines latest ping, jitter, and packet loss for the one configured
`network_probe_target` as three complete rows: `P nMS`, `J nMS`, and `L n%`.
Blank scanlines separate the rows. All three readings must be current. It uses
`network_probe_enabled`, method, target, interval, timeout, and window settings
described under `PING`; it does not discover a game server.

### `SYSTEM_BATTERY`

Uses native `GetSystemPowerStatus`. Display states are `CHG n%` (charging),
`AC n%` (battery present on AC), `BAT n%` (on battery), `AC ONLY` (AC with no
battery), `NO BAT` (offline with no battery), `UNKNOWN`, and `STALE`. No
external utility is required.

### `HARD_FAULTS`

Shows `n/s` under the `FAULTS` title from the native Windows PDH
`Memory\\Page Reads/sec` counter. Page reads are only an approximation of
hard-fault pressure and can include reads not caused by page faults; the display
does not add an unsupported approximation glyph. It has no external dependency.

### `BOTTLENECK`

Shows a configurable heuristic: `CPU`, `GPU`, `MEM`, `DISK I/O`, or `NONE`.
CPU uses peak logical-processor load, GPU uses native GPU load, MEM uses the
higher available RAM/VRAM percentage, and disk uses aggregate read+write MB/s.
Candidates must meet their configured threshold continuously for
`bottleneck_sustain_ms`; if several qualify, the largest threshold ratio wins
with stable CPU/GPU/MEM/disk ordering on ties. CPU, RAM, and both disk readings
are required for the heuristic; missing required input yields `N/A`.

When the result is current `NONE`, the pane dynamically renders the next token
in that slot, skipping inactive `BOTTLENECK` and `PROC_HANG`, or `CLEAR` if
nothing remains. This fallback does not change the user's selected token or
cycle index. Related settings are all five `bottleneck_*` keys.

### `PROC_HANG`

Shows the selected repeatedly unresponsive visible top-level window. When no
target exists, the pane dynamically renders the next active token or `CLEAR`
without changing selection. `PROC_HANG` may occur only once across all four
configured slot lists.

The token owns its runtime action binding: only while `PROC_HANG` is actually
selected does that slot's matching physical button bind the exact displayed
target on button-down. A continuous `hang_hold_ms` hold is required and full
progress alone never acts; termination is considered only on matching release
after identity, timeout, visibility, exclusion, and policy revalidation.
Recovery, selection change, device loss, provider failure, reload, early
release, or safe mode cancels. The legacy `hang_button` key remains accepted
for persisted configuration but does not choose the runtime button. Termination
can lose unsaved work.

All six throughput options use Windows counters from all operational,
non-loopback network interfaces, sampled about once per second. Rates are
decimal SI bit rates: 1 Kbps = 1,000 bits/s, 1 Mbps = 1,000,000 bits/s, and
1 Gbps = 1,000,000,000 bits/s. A real idle interval displays zero; `N/A` means
no rate sample is available. Although the display understands `STALE`, the
current Windows counter provider clears a failed sample instead of retaining
it, so throughput failures normally show `N/A`. No external software or
network-quality probe is required.

### `NET_IN`

Shows the current aggregate incoming/received rate across the included network
interfaces. It has no option-specific setting; `network_graph_ceiling_mbps`
does not affect this numeric reading.

### `NET_OUT`

Shows the current aggregate outgoing/transmitted rate across the included
network interfaces. It has no option-specific setting; the graph ceiling does
not affect this numeric reading.

### `NET_BOTH`

Shows incoming plus outgoing rates as one aggregate current bit rate. It is a
sum, not a full-duplex maximum. If either direction is unavailable the result is
`N/A`; if either is stale the sum is `STALE`. The graph ceiling does not affect
the numeric reading.

### `NET_IN_GRAPH`

Shows the current incoming SI bit rate and exactly the trailing 30 seconds in
one-second bins. Graph height is clipped to the
shared `network_graph_ceiling_mbps` setting (default 1000 Mbps, range
1..100000); changing the ceiling does not change the numeric rate. `N/A` or
`STALE` replaces the graph when the current reading is unusable.

### `NET_OUT_GRAPH`

Shows the current outgoing SI bit rate and exactly the trailing 30 seconds in
one-second bins. It uses the same
`network_graph_ceiling_mbps` ceiling as every network graph. `N/A` or `STALE`
replaces the graph when the current reading is unusable.

### `NET_GRAPH`

Shows incoming and outgoing history together around a center line: incoming
extends above it and outgoing below it. It uses 30 shared one-second bins
covering exactly the trailing 30 seconds, and both directions use the same
`network_graph_ceiling_mbps` ceiling. It intentionally omits numeric rates. If
either direction is unavailable the slot shows `N/A`; if either is stale it
shows `STALE`.

The three network-quality options all probe the single
`network_probe_target`; they do not measure game-server latency unless that
server address is the configured target. Probing is off by default. ICMP sends
an IPv4 echo request; TCP times a connection attempt (default port 443).
`auto` tries IPv4 ICMP and falls back to TCP within the same timeout, while IPv6
uses TCP only. The target must be an IP literal, not a hostname. The history is
the most recent `network_probe_window` attempts (default 30, range 5..600), not
a fixed time period; its duration is approximately window x interval.

### `PING`

Shows the latest successful round-trip latency to `network_probe_target` in
milliseconds. Lower is better. A timeout/lost probe makes the current ping
`N/A` while still contributing to packet loss. A provider error retains the
last valid ping as `STALE`; disabling probing clears it to `N/A`.

Related settings: `network_probe_enabled`, `network_probe_method`,
`network_probe_target`, `network_probe_interval_ms`, and
`network_probe_timeout_ms`. Requires network access to a target that answers the
selected ICMP or TCP probe; ICMP may be blocked by firewall or network policy.

### `JITTER`

Shows latency variation in milliseconds: the mean absolute difference between
consecutive successful probe latencies within `network_probe_window`. Lower is
better. At least two successful probes are required, so it initially shows
`N/A`; losses affect `PACKET_LOSS` but are omitted from the jitter calculation.
A provider error or aged retained value shows `STALE`, and disabling probing
clears it to `N/A`.

Related settings and requirements are the same as `PING`, plus
`network_probe_window`.

### `PACKET_LOSS`

Shows the percentage of probe attempts in `network_probe_window` that timed out
or otherwise counted as loss. For example, 3 losses among 30 retained attempts
is 10.0%. The window fills gradually after startup or a probe-setting change.
A provider error or aged retained value shows `STALE`; disabling probing shows
`N/A`.

Related settings and requirements are the same as `JITTER`.

### `CLOCK`

Shows the current Windows local time using the default `HH:mm:ss` format. It
does not require a provider or external service and does not use `N/A` or
`STALE`. The fixed dashboard header uses the default `yyyy-MM-dd dddd` date,
falling back to `yyyy-MM-dd ddd` when needed to fit. The accepted `time_format`
and `date_format` keys do not currently customize either display.

### `ALERTS`

Shows `CLEAR` when there are no unacknowledged warning/critical episodes;
otherwise it shows the unacknowledged count and highest severity as `n L 2`
(warning) or `n L 3` (critical). Episodes can come from CPU temperature, GPU
temperature, RAM percentage, VRAM percentage, and headset battery. Only current
valid readings create alerts, so unavailable or stale telemetry does not create
one. Physical button 4 acknowledges the highest active episode.

Related settings: `cpu_temp_warning`, `cpu_temp_critical`,
`gpu_temp_warning`, `gpu_temp_critical`, `memory_warning`, `vmem_warning`,
`headset_warn_percent`, `headset_critical_percent`, and
`critical_alert_linger_ms`. `CLEAR` is the normal idle state; this option does
not show `N/A` or `STALE`.

### `PROVIDER_STATUS`

Shows how many configured/tracked data sources are currently unavailable or
unhealthy. `OK` means every tracked source presently reports healthy; `N DOWN`
means that many sources do not. Typical tracked sources include enabled
headset, controller, audio, GPU, LHM, PresentMon, network probe, network
interface, hung-window, and Discord services. Most disabled optional providers
are removed rather than counted. The current status set always includes Discord
and the hung-window detector, however, so Discord is `DOWN` unless connected
and authenticated and either entry can count as `DOWN` while its feature is
disabled. This is provider health, not a count of individual `N/A` metrics, and
it does not use `STALE` text. Safe mode is the exception: it publishes only the
healthy `safe-mode` provider state, so `PROVIDER_STATUS` renders `OK` rather than
counting Discord, the hung-window detector, or other disabled providers.

The exact related settings are each provider's enable/selector settings:
`headset_enabled`, `controller_enabled`, `audio_enabled`, `gpu_provider`,
`lhm_mode`, `presentmon_enabled`, `presentmon_target_mode`,
`network_probe_enabled`, `hang_enabled`, `discord_enabled`, and `safe_mode`.
External requirements are those of the enabled providers described above.

## CPU / CCD (Process Lasso removed)

| Key | Default | Range / constraint / purpose |
|---|---|---|
| `ccd_source` | `auto` | `auto` \| `manual`; select native detection or manual domain mapping |
| `ccd_cache_processors` | `auto` | LP list using `0,1,2` / `0-15`, each 0..1023; Cache-domain override used with `ccd_source=manual` |
| `ccd_frequency_processors` | `auto` | same LP-list syntax; must not overlap cache list; Frequency-domain override used with `ccd_source=manual` |

Auto detection: L3/NUMA topology grouping + CPUID L3-size labeling of the
V-Cache die. Manual lists override. Single detected domain switches the
dashboard to the one-bar CPU layout automatically.

## Logitech G13 backend

| Key | Default | Range / values / purpose |
|---|---|---|
| `logitech_backend` | `auto` | `auto` \| `sdk` \| `hid` \| `virtual`; choose display transport |
| `logitech_reconnect_ms` | 5000 (`auto`) | 250..300000; initial reconnect delay |
| `logitech_reconnect_max_ms` | 60000 (`auto`) | >= reconnect, <= 600000; backoff cap |
| `logitech_button_poll_ms` | 50 (`auto`) | 10..1000; physical input/ownership poll cadence |
| `logitech_button_debounce_ms` | 40 (`auto`) | 10..500; stable-edge debounce period |
| `logitech_friendly_name` | `LCDSirPlus` | non-empty, no NUL; SDK application name |
| `logitech_orientation` | `normal` | `normal` \| `flip_x` \| `flip_y` \| `rotate_180`; output transform |
| `logitech_invert` | 0 | boolean; invert output pixels |

`auto` selects the trusted SDK while exact process name `LCore.exe` is present
and direct HID while it is absent. Explicit `sdk` never falls back to HID;
explicit `hid` waits while LCore owns the display. Ownership changes close the
old transport before reevaluation. It never terminates Logitech software or
blocks unrelated G HUB background components.

## LibreHardwareMonitor (optional loopback fallback)

LCDSirPlus is native-first: Win32 and installed vendor APIs are used wherever
they provide a safe documented source. HWiNFO shared memory, then exact or
automatic LHM loopback selection, is used only where those native sources do
not exist. LCDSirPlus never probes raw MSRs, SMBus, EC, or Super-I/O registers.

| Key | Default | Range / constraint / purpose |
|---|---|---|
| `lhm_mode` | `auto` | `auto` \| `on` \| `off`; optional fallback, never starts LHM |
| `lhm_url` | `auto` → `http://127.0.0.1:8085/data.json` | loopback HTTP only, no query/fragment/credentials; select the LHM JSON endpoint |
| `lhm_interval_ms` | 300 (`auto`) | 100..60000; LHM poll cadence when enabled |
| `lhm_stale_ms` | 3000 (`auto`) | >= interval, <= 300000; expire retained LHM readings |
| `lhm_cpu_temp_sensor`, `lhm_gpu_temp_sensor` | omitted | optional exact stable SensorId overrides for otherwise automatic temperature selection; require enabled/reachable LHM |
| `lhm_vrm_temp_sensor` | omitted | exact temperature SensorId |
| `lhm_chipset_temp_sensor` | omitted | exact temperature SensorId |
| `lhm_motherboard_temp_sensor` | omitted | exact temperature SensorId |
| `lhm_cpu_fan_control_sensor` | omitted | exact 0..100% control SensorId |
| `lhm_cpu_fan_rpm_sensor` | omitted | exact fan-RPM SensorId; used only with max RPM when control is absent |
| `lhm_pump_control_sensor` | omitted | exact 0..100% control SensorId |
| `lhm_pump_rpm_sensor` | omitted | exact fan-RPM SensorId; used only with max RPM when control is absent |
| `lhm_total_power_sensor` | omitted | exact total-power SensorId; authoritative `POWER_LIMIT` fallback |
| `lhm_cpu_power_sensor` | omitted | exact CPU-package-power SensorId |
| `lhm_gpu_power_sensor` | omitted | exact GPU-board-power SensorId |
| `cpu_fan_max_rpm` | 0 | 0..30000; 0 disables RPM-derived percentage |
| `pump_max_rpm` | 0 | 0..30000; 0 disables RPM-derived percentage |

The user must explicitly run LibreHardwareMonitor and enable its web server.
LCDSirPlus does not enable or manage it. Only loopback HTTP is accepted. Exact
extended SensorIds fail closed when missing, ambiguous, wrong-type, or outside
their expected ranges; no LHM load, VRAM, RAM, disk, or network values are
published.

## HWiNFO shared memory (optional exact fallback)

| Key | Default | Range / constraint / purpose |
|---|---|---|
| `hwinfo_cpu_temp_sensor` / `hwinfo_cpu_temp_reading` | omitted | exact case-sensitive original-label pair; both blank or both nonblank |
| `hwinfo_total_power_sensor` / `hwinfo_total_power_reading` | omitted | exact total-power pair; both blank or both nonblank |
| `hwinfo_cpu_power_sensor` / `hwinfo_cpu_power_reading` | omitted | exact CPU-package-power pair; both blank or both nonblank |
| `hwinfo_gpu_power_sensor` / `hwinfo_gpu_power_reading` | omitted | exact GPU-board-power pair; both blank or both nonblank |
| `hwinfo_stale_ms` | 5000 | 1000..60000; reject aged shared-memory readings |

Start user-managed HWiNFO in Sensors-only mode, enable **Shared Memory
Support**, run `LCDSirPlus.exe --list-hwinfo-sensors`, then copy the exact
quoted labels from its output. A configured pair must resolve to exactly one
reading with the expected temperature/power type and unit. HWiNFO64 Free's
shared-memory monitoring is limited to 12 hours; use HWiNFO32 or appropriately
licensed HWiNFO Pro for unattended operation according to the
[vendor's terms](https://www.hwinfo.com/). LCDSirPlus only reads the mapping and
does not bundle, manage, restart, configure, or license HWiNFO.

## GPU

| Key | Default | Range / values / purpose |
|---|---|---|
| `gpu_provider` | `auto` | `auto` \| `nvapi` \| `adlx` \| `off`; select or disable native GPU telemetry |

Trusted vendor-installed NVAPI/NVML or ADLX libraries provide GPU load, VRAM,
temperature, and available power; `auto` tries NVIDIA then AMD. LHM can fall
back only for temperature and exact configured power sensors. The vendor APIs
and drivers are external platform components, not Rust dependencies.

## PresentMon

| Key | Default | Range / values / purpose |
|---|---|---|
| `presentmon_enabled` | 1 | boolean; enable owned frame capture (safe mode disables it) |
| `presentmon_path` | `auto` | bundled colocated `PresentMon.exe`, or explicit fixed-local-drive console path; select console |
| `presentmon_interval_ms` | 1000 (`auto`) | 100..10000; capture/report interval |
| `presentmon_window_ms` | 60000 (`auto`) | 5000..600000; rolling low-FPS window |
| `presentmon_target_mode` | `foreground` | `foreground` \| `process_name` \| `disabled`; target policy |
| `presentmon_process_name` | empty | one process name; required in `process_name` mode |
| `presentmon_exclude` | `dwm.exe explorer.exe applicationframehost.exe textinputhost.exe searchhost.exe lcdsirplus.exe lcdsirplus.console.exe` | zero to 256 foreground names excluded from capture |
| `stutter_threshold_ms` | 33.34 | 1..1000; frame-time threshold for session stutter count |

Release packages bundle the official signed PresentMon v2.5.1 console as
`PresentMon.exe`. LCDSirPlus launches and owns it only while an enabled target
frame capture is active, passes arguments without a shell, and stops only its
own child on target/config change or shutdown. PresentMon supplies frame/FPS
timing only; it is not a source of CPU/GPU hardware telemetry. No PresentMon
service, MSI, GUI, or API/SDK is bundled or installed. Uninstall removes the
owned console.

Frame metrics become stale after five seconds without output and expire after
another five. Low-FPS metrics use `presentmon_window_ms`; changing target,
path, interval, window, or stutter threshold starts a new capture/session. If
capture cannot start, add the user to Windows' **Performance Log Users** group
and sign out/in, or test an elevated launch if local ETW policy requires it.
Elevation is troubleshooting, not a normal LCDSirPlus requirement.

PresentMon is MIT-licensed by Intel and is not affiliated with LCDSirPlus. Its
exact notices ship as `licenses/PresentMon/LICENSE.txt` and
`licenses/PresentMon/THIRD_PARTY.txt`; upstream is
[GameTechDev/PresentMon](https://github.com/GameTechDev/PresentMon).

## Headset / Controller / Network / Audio

The detailed module sections above define `headset_*`; warn must be greater
than or equal to critical, and `headset_query_timeout_ms` bounds the complete
HID write/read sequence after enumeration.

| Key | Default | Range / values | Purpose / dependency |
|---|---|---|---|
| `controller_enabled` | 1 | boolean | Enable XInput battery polling; safe mode disables it. |
| `controller_index` | -1 | -1..3 | Select XInput index 0..3; -1 chooses the first connected controller. |
| `controller_poll_ms` | 10000 (`auto`) | 1000..600000 | Controller poll cadence when enabled. |
| `network_probe_enabled` | 0 | boolean | Enable bounded network-quality I/O; safe mode always disables it. |
| `network_probe_method` | `icmp` | `auto` \| `icmp` \| `tcp` | Select probe transport; `auto` tries IPv4 ICMP then TCP and uses TCP for IPv6. |
| `network_probe_target` | `1.1.1.1` | when enabled, one IP literal optionally with TCP port 1..65535; otherwise one string | Probe endpoint; explicit ICMP requires IPv4 without a port. |
| `network_probe_interval_ms` | 1000 (`auto`) | 250..60000 | Delay between probe attempts. |
| `network_probe_timeout_ms` | 1500 (`auto`) | 50..30000 | One absolute attempt/fallback deadline. |
| `network_probe_window` | 30 | 5..600 while probing is enabled | Retained attempt count for jitter and packet loss. |
| `audio_enabled` | 1 | boolean | Enable the shared Core Audio poll for `AUDIO` and `MIC_STATUS`; safe mode disables it. |
| `audio_poll_ms` | 1000 (`auto`) | 100..60000 | Core Audio endpoint poll cadence when enabled. |

Network quality probing is off by default and performs no network I/O while
disabled or in safe mode. The target must be one IP literal, optionally with a
TCP port (`127.0.0.1:443` or `[::1]:443`); omitted TCP ports default to 443.
Explicit `icmp` accepts IPv4 without a port only. `auto` tries IPv4 ICMP before
TCP, while an IPv6 target skips unsupported ICMP and uses TCP only. One
absolute deadline bounds the complete probe including fallback. Interval,
timeout, and history-window settings are hot-reloaded. Probe timeout/loss
contributes to packet loss, while native provider failures retain prior values
as stale and mark the provider unavailable. Hostnames are rejected because
standard-library DNS resolution cannot be canceled with the required shutdown bound.

`NET_IN`, `NET_OUT`, and `NET_BOTH` show current decimal SI bit rates. The graph
variants show exactly the trailing 30 seconds in one-second bins.
`network_graph_ceiling_mbps`
defaults to 1000 and accepts 1..100000 Mbps; it clips graph height only and is
hot-reloaded without changing numeric readings.

## Graph, warning, and bottleneck settings

| Key | Default | Range / behavior |
|---|---|---|
| `network_graph_ceiling_mbps` | 1000 | 1..100000; network graph scale only |
| `disk_graph_ceiling_mbps` | 1000 | 1..100000; disk graph scale only |
| `fps_graph_ceiling` | 240 | 1..1000; FPS graph scale only |
| `warning` | 1 | boolean; enable selected temperature-pane inversion |
| `cpu_temp_max_c` | 90 | 1..150; CPU graph ceiling and pane-warning threshold |
| `gpu_temp_max_c` | 90 | 1..150; GPU graph ceiling and pane-warning threshold |
| `bottleneck_cpu_percent` | 90 | 1..100; CPU heuristic threshold |
| `bottleneck_gpu_percent` | 95 | 1..100; GPU heuristic threshold |
| `bottleneck_memory_percent` | 90 | 1..100; RAM/VRAM heuristic |
| `bottleneck_disk_mbps` | 500 | 1..100000; aggregate read+write decimal MB/s |
| `bottleneck_sustain_ms` | 2000 | 0..60000; continuous qualification duration |

All graph modules render exactly the trailing 30 seconds in one-second bins.
Load graphs use 100%; temperature, disk, network, and FPS graphs use the fixed
ceilings above. `FRAME_TIME` alone scales to its observed min/max within the
same 30-second window. Ceilings never clip numeric text.

With `warning 1`, a currently selected `CPU_TEMP`, `CPU_TEMP_GRAPH`,
`CPU_CACHE_TEMP`, `CPU_FREQ_TEMP`, `GPU_TEMP`, `GPU_TEMP_GRAPH`, or `THERMALS`
pane inverts on alternating 100 ms phases at or above its shared graph/warning
threshold. This requires `render_interval_ms <= 100`; validation rejects a
slower interval. Only that slot pane inverts. This replaces the old full-screen
CPU/GPU temperature overlays. The separate `cpu_temp_warning/critical`,
`gpu_temp_warning/critical`, memory/VRAM, headset, `ALERTS`, acknowledgement,
and non-temperature full-screen alert behavior remain available.

## Discord

| Key | Default | Notes |
|---|---|---|
| `discord_enabled` | 1 | disabled, and always off in safe mode, means no IPC/token/network access |
| `discord_client_id` | empty | numeric Discord developer application client ID |
| `discord_redirect_uri` | `http://127.0.0.1` | legacy accepted/no-op key; not sent by the RPC OAuth flow |
| `discord_linger_ms` | 700 | 0..10000 after speaking stops |
| `discord_max_speakers` | 2 | 1..4; additional visible speakers render as `+N` |
| `discord_show_self` | 0 | include the current user in the speaking overlay |
| `discord_show_channel` | 0 | use the selected voice-channel name as the title |

`discord_client_id` is the public numeric Application ID/OAuth client ID from
the Discord Developer Portal; it is not a client secret. Leaving it empty is
an unconfigured state and no IPC connection is attempted. Running Discord alone
is insufficient: Discord Desktop must run as the same Windows user and session,
the configured application must be authorized for the active account, and its
fixed-local IPC server must pass process, session, executable, Authenticode, and
Discord publisher checks. LCDSirPlus displays voice state only; it does not
publish Rich Presence and needs no image assets.

No token or client secret belongs in this file. Use `--discord-authorize` once
while each Discord Account Switcher account is active. Its v2 access/refresh
record is stored under the immutable Discord user ID and current-Windows-user
DPAPI protected in `%LOCALAPPDATA%\LCDSirPlus`. A confidential-client secret,
when required, is accepted only from `LCDSIRPLUS_DISCORD_CLIENT_SECRET` during
authorization and retained inside that encrypted record for refresh; remove the
environment variable afterward. Use `--discord-clear-token` to remove all exact
LCDSirPlus Discord credential records, including the old global record.
RPC authorization opens no listener, uses no redirect URI, and requires no
portal redirect registration.

## Hung-process guard

| Key | Default | Range / behavior |
|---|---|---|
| `hang_enabled` | 1 | boolean; enable query-only hung-window detection |
| `hang_button` | 3 | accepted legacy 1..4 value; runtime binding follows selected `PROC_HANG` slot |
| `hang_hold_ms` | 2000 | 1000..10000; continuous selected-slot hold required before release can act |
| `hang_probe_interval_ms` | 2000 | 250..60000; detector probe cadence |
| `hang_probe_timeout_ms` | 350 | 10..5000 and less than probe interval; bound one `WM_NULL` probe |
| `hang_failures_required` | 3 | 2..10; consecutive timeouts required for a target |
| `hang_minimum_ms` | 6000 | 1000..60000; minimum continuous failure duration |
| `hang_ignore` | `lcdsirplus.exe explorer.exe dwm.exe winlogon.exe csrss.exe services.exe lsass.exe smss.exe fontdrvhost.exe sihost.exe taskhostw.exe` | zero to 256 process names excluded from detection/action |

`PROC_HANG` may occur only once across all slots. The selected token's physical
slot button owns display/action binding; `hang_button` is compatibility-only.
A short release while a target is bound toggles target detail, then advances to
the next target when detail is already shown; it does not terminate. A release
after at least `hang_hold_ms` terminates only after target and policy
revalidation. A release held beyond the bounded maximum of twice the configured
hold, clamped to 3..15 seconds, is refused as stale. Safe mode prevents target
binding and termination but preserves ordinary short-release slot cycling and
button 4 alert acknowledgement.

## Alerts

| Key | Default | Range / values | Purpose / dependency |
|---|---|---|---|
| `cpu_temp_warning` | 85 | 0..150 | CPU warning-episode threshold; must be <= `cpu_temp_critical`. |
| `cpu_temp_critical` | 95 | 0..150 | CPU critical-episode threshold; must be >= `cpu_temp_warning`. |
| `gpu_temp_warning` | 83 | 0..150 | GPU warning-episode threshold; must be <= `gpu_temp_critical`. |
| `gpu_temp_critical` | 90 | 0..150 | GPU critical-episode threshold; must be >= `gpu_temp_warning`. |
| `memory_warning` | 90 | 0..100 | Physical-memory warning-episode percentage. |
| `vmem_warning` | 95 | 0..100 | VRAM warning-episode percentage. |
| `critical_alert_linger_ms` | 3000 | 0..60000 | Retain a cleared critical overlay for this duration. |

These alert-episode thresholds are separate from `cpu_temp_max_c` and
`gpu_temp_max_c` pane warnings. Only current valid readings create episodes.
Alerts are ordered by severity, first observation, then stable identity;
physical button 4 or preview slot 4 acknowledges the highest unacknowledged
episode before its normal slot action. A cleared episode rearms if it later recurs. With
`warning 1`, CPU/GPU temperature episodes remain visible in `ALERTS` but their
full-screen overlays are suppressed in favor of pane inversion; other alert
overlays remain.

## Logging

| Key | Default | Range / values | Purpose / dependency |
|---|---|---|---|
| `log_level` | `info` | `debug` \| `info` \| `warn` \| `error` | Minimum emitted log severity. |
| `log_max_bytes` | 2097152 | 65536..104857600 | Per-file byte cap that triggers bounded rotation. |
| `log_backups` | 3 | 1..20 | Number of rotated ring slots retained. |

Log file:
`%LOCALAPPDATA%\LCDSirPlus\lcdsirplus.log` (or `--diagnostic-dir`).
Startup fails explicitly if the selected log directory or file cannot be
opened. A later write/rotation failure leaves stdout logging active, drops the
failed line, and retries the same bounded file sink on the next log call.
Initialization atomically trims every retained file to the configured cap and
removes slots above `log_backups`. Backups are atomic ring slots
`lcdsirplus.log.1` through `.N`; modification time, not suffix, gives newest to
oldest order. Rotation fully prepares and flushes a bounded replacement before
replacing the next slot, then truncates the current log.

## Diagnostics

`LCDSirPlus.exe --diagnostics [--config PATH] [--diagnostic-dir PATH]` ignores
`--config` without resolving, opening, or parsing it, then writes a uniquely
named atomic ZIP without starting HID, providers, Discord, network probes, or
hung actions. The archive is store-only, has fixed entries (`privacy.txt`,
`report.txt`, `manifest.txt`), per-entry caps, and a 1 MiB total cap. The
selected directory must be a regular non-reparse directory on a fixed local
volume; native no-replace publication refuses an existing destination and
verifies the final file's pinned identity and size.

The report uses a closed allowlist of version/build, CLI safe-mode state,
compile-time schema/G13 facts, closed provider states, and size totals for known
rotated log names. Raw logs/configuration and user, credential,
Discord, title, path, address/target, environment, command-line, registry,
serial, arbitrary-listing, and device-path data are excluded.

## Includes

| Directive | Default | Range / values | Purpose / dependency |
|---|---|---|---|
| `include` | none | exactly one relative path; at most 64 directives and depth 8 | Parse an override file relative to the including file. |

Example: `include lcdsirplus.local.txt`.

Relative to the including file; no `..`, no absolute paths, no environment
syntax; cycles rejected; later files override earlier values.

Includes are practical for source and portable layouts. Installer update allows
only the declared installed inventory plus `lcdsirplus.txt`, so an undeclared
include file under the install root causes update refusal. Installed users
should keep one primary `lcdsirplus.txt`.
