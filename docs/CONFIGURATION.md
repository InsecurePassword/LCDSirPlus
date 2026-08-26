# LCDForge 0.3.0 configuration reference

Format: LCDSirReal-style. `#` comments, whitespace-separated key/value
lines, quoted values for embedded spaces, `include` override files. The
parser enforces the same limits as the Go implementation: 8 include depth,
64 files, 8 MiB aggregate, 16384 lines, 256 list items, 16 KiB tokens,
duplicate keys rejected per file.

Hot reload: saving a valid file applies immediately; invalid changes are
rejected with `file:line: message` diagnostics and the last valid
configuration stays active.

Every selector that supports one defaults to `auto`.

## Core

| Key | Default | Range / values |
|---|---|---|
| `config_refresh_ms` | 1000 | 100..60000 |
| `telemetry_interval_ms` | 300 (`auto`) | 100..10000 |
| `render_interval_ms` | 100 (`auto`) | 25..5000 |
| `preview_mode` | `auto` | `auto` \| `always` \| `never` |
| `preview_scale` | 4 | 1..10 |
| `start_minimized` | 0 | bool |
| `start_at_login` | 0 | bool |
| `safe_mode` | 0 | bool |
| `date_format` | `yyyy-MM-dd dddd` | Windows GetDateFormat tokens |
| `time_format` | `HH:mm:ss` | Windows GetTimeFormat tokens |

`preview_mode auto` shows the preview only when the physical backend is
unavailable (LCDSirReal `testwindow` semantics). A tray toggle takes precedence
over later `auto` backend changes, and `start_minimized` suppresses automatic
preview display. `start_at_login` owns only the current user's
`LCDForge` Run value and refuses to replace or remove a foreign value. Safe mode
never changes startup registration.

## Slots (fixed four-button contract)

```text
slot_0  HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY
slot_1  FPS_CURRENT FPS_1LOW FRAME_TIME SESSION_TIME
slot_2  GPU_TEMP PING JITTER AUDIO
slot_3  PACKET_LOSS MIC_STATUS SESSION_SUMMARY PROVIDER_STATUS
```

Valid modules: `HEADSET_BATTERY CONTROLLER_BATTERY FPS_CURRENT FPS_1LOW
FPS_01LOW FRAME_TIME CPU_TEMP GPU_TEMP NET_IN NET_OUT PING JITTER
PACKET_LOSS MIC_STATUS AUDIO SESSION_TIME SESSION_SUMMARY CLOCK GAME_NAME
ALERTS PROVIDER_STATUS CPU_LOAD RAM_USAGE GPU_LOAD VRAM_USAGE
CPU_CACHE_TEMP CPU_FREQ_TEMP`. A short press cycles only the matching slot.

## CPU / CCD (Process Lasso removed)

| Key | Default | Notes |
|---|---|---|
| `ccd_source` | `auto` | `auto` \| `manual` |
| `ccd_cache_processors` | `auto` | LP list: `0,1,2` / `0-15` |
| `ccd_frequency_processors` | `auto` | must not overlap cache list |

Auto detection: L3/NUMA topology grouping + CPUID L3-size labeling of the
V-Cache die. Manual lists override. Single detected domain switches the
dashboard to the one-bar CPU layout automatically.

## Logitech G13 backend

| Key | Default | Range / values |
|---|---|---|
| `logitech_backend` | `auto` | `auto` \| `hid` \| `virtual` (SDK retired) |
| `logitech_reconnect_ms` | 5000 (`auto`) | 250..300000 |
| `logitech_reconnect_max_ms` | 60000 (`auto`) | ≥ reconnect, ≤ 600000 |
| `logitech_button_poll_ms` | 50 (`auto`) | 10..1000 |
| `logitech_button_debounce_ms` | 40 (`auto`) | 10..500 |
| `logitech_friendly_name` | LCDForge | non-empty |
| `logitech_orientation` | `normal` | `normal` \| `flip_x` \| `flip_y` \| `rotate_180` |
| `logitech_invert` | 0 | bool |

## LibreHardwareMonitor (optional temps fallback, Phase 2)

| Key | Default | Notes |
|---|---|---|
| `lhm_mode` | `auto` | `auto` (CPU temp and missing native GPU temp only) \| `on` \| `off` |
| `lhm_url` | `auto` → `http://127.0.0.1:8085/data.json` | loopback HTTP only, no query/fragment/credentials |
| `lhm_interval_ms` | 300 (`auto`) | 100..60000 |
| `lhm_stale_ms` | 3000 (`auto`) | ≥ interval, ≤ 300000 |
| `lhm_cpu_temp_sensor`, `lhm_gpu_temp_sensor` | — | stable SensorId overrides; no LHM load/VRAM/RAM/network publication |

## GPU / PresentMon / Headset / Controller / Network / Audio (Phase 2)

`gpu_provider` (`auto`\|`nvapi`\|`adlx`\|`off`) uses trusted System32 vendor
DLLs; `auto` tries NVAPI then ADLX. `presentmon_*`
(`presentmon_target_mode`: `foreground`\|`process_name`\|`disabled`;
`stutter_threshold_ms` 1..1000), `headset_*` (warn ≥ critical),
`controller_index` (-1..3), `network_probe_*` (`auto`\|`icmp`\|`tcp`),
`audio_poll_ms`. PresentMon resolves an explicitly configured path or, for
`auto`, a colocated `PresentMon.exe` beside LCDForge. Frame metrics become stale
after five seconds without output; 1% and 0.1% lows use the configured history
window, and changing target/capture settings starts a new session. PresentMon
is a separate runtime prerequisite and is not redistributed.
`headset_query_timeout_ms` bounds the complete HID write/read sequence after
bounded device enumeration.

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

## Discord (Phase 3)

| Key | Default | Notes |
|---|---|---|
| `discord_enabled` | 1 | disabled, and always off in safe mode, means no IPC/token/network access |
| `discord_client_id` | empty | numeric Discord developer application client ID |
| `discord_redirect_uri` | `http://127.0.0.1` | must exactly match the developer application; LCDForge does not listen on it |
| `discord_linger_ms` | 700 | 0..10000 after speaking stops |
| `discord_max_speakers` | 2 | 1..4; additional visible speakers render as `+N` |
| `discord_show_self` | 0 | include the current user in the speaking overlay |
| `discord_show_channel` | 0 | use the selected voice-channel name as the title |

No token or client secret belongs in this file. Use `--discord-authorize`; the
resulting access/refresh record is current-user DPAPI protected under
`%LOCALAPPDATA%\LCDForge2`. A confidential-client secret, when required, is
accepted only from the temporary `LCDFORGE_DISCORD_CLIENT_SECRET` environment
variable. Use `--discord-clear-token` to remove the local record.

## Hung-process guard (Phase 4)

`hang_enabled`, `hang_button` (fixed 3), `hang_hold_ms` (1000..10000),
`hang_probe_interval_ms` (250..60000), `hang_probe_timeout_ms` (10..5000,
< interval), `hang_failures_required` (2..10), `hang_minimum_ms`
(1000..60000), `hang_ignore` (process list). Safe mode prevents target binding
and termination but preserves ordinary short-press slot cycling and button 4
alert acknowledgement.

## Alerts (Phase 2/4)

`cpu_temp_warning/critical` (0..150, critical ≥ warning), `gpu_temp_*`,
`memory_warning`/`vmem_warning` (0..100), `critical_alert_linger_ms`
(0..60000). Only current, valid readings create episodes. Alerts are ordered by
severity, first observation, then stable identity; physical button 4
acknowledges the highest unacknowledged warning/critical episode before its
normal slot action. A cleared episode rearms if it later recurs.

## Logging

`log_level` (`debug`\|`info`\|`warn`\|`error`), `log_max_bytes`
(65536..104857600), `log_backups` (1..20). Log file:
`%LOCALAPPDATA%\LCDForge2\lcdforge.log` (or `--diagnostic-dir`).
Startup fails explicitly if the selected log directory or file cannot be
opened. A later write/rotation failure leaves stdout logging active, drops the
failed line, and disables file output rather than risking an unbounded file.
Initialization atomically trims every retained file to the configured cap and
removes slots above `log_backups`. Backups are atomic ring slots
`lcdforge.log.1` through `.N`; modification time, not suffix, gives newest to
oldest order. Rotation fully prepares and flushes a bounded replacement before
replacing the next slot, then truncates the current log.

## Diagnostics

`lcdforge.exe --diagnostics [--config PATH] [--diagnostic-dir PATH]` ignores
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

```text
include lcdforge.local.txt
```

Relative to the including file; no `..`, no absolute paths, no environment
syntax; cycles rejected; later files override earlier values.
