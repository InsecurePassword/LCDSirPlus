# LCDSirPlus Architecture

Audience: maintainers reviewing source ownership, trust boundaries, and data
flow. This document describes the current source worktree; it does not claim
that final 0.3.0 packages have been built or qualified. Use
[DEVELOPMENT.md](DEVELOPMENT.md) for repository commands,
[PRODUCT-SPEC.md](PRODUCT-SPEC.md) for required behavior,
[REFERENCE-LAYOUT.md](REFERENCE-LAYOUT.md) for renderer invariants, and
[HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md) for live qualification.

## Source map

```text
src/main.rs                    command parsing and dispatch
src/app.rs                     main loop, configuration, slots, rendering
src/config.rs                  defaults and validation
src/parser.rs                  text parser and include graph
src/model.rs                   readings and snapshots
src/history.rs                 fixed histories and frame statistics
src/render/                    160x43 frame, font, layouts, golden tests
src/backends/                  G13 selection, SDK, HID, reconnect
src/providers/                 Windows, GPU, sensor, device, network, Discord
src/telemetry.rs               data-source workers and snapshot publication
src/alerts.rs                  alert episodes and acknowledgement
src/runtime.rs                 single instance, config path, startup ownership
src/ui.rs                      preview and tray
src/diagnostics.rs             offline redacted ZIP
src/hardware_test.rs           G13 test sequence
src/logging.rs                 rotating log
src/slots.rs                   four slot lists and temporary display choice
```

## Runtime owners

| Owner | Responsibility |
|---|---|
| Main thread | Active configuration, slot state, alerts, rendering, actions. |
| G13 thread | Connection selection, HID handles, latest frame, button edges. |
| SDK owner thread | Trusted Logitech DLL lifetime and all SDK calls. |
| UI thread | Preview window, tray icon, mouse events. |
| Reading workers | Windows/GPU/device/sensor/network samples. |
| PresentMon worker | One owned child, catalog read, selection, frame statistics. |
| Discord worker | Verified Discord Desktop connection, authorization, voice state. |
| Hung-window worker | Query-only window checks and confirmed target list. |

Only the G13 thread touches the device. It keeps only the newest pending frame.
Button edges are debounced before the main thread receives them.

Normal runtime and nonvirtual hardware tests use the per-session
`Local\LCDSirPlus.Runtime` mutex. Read-only commands and virtual hardware tests
do not. Installed configuration resolves to
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; portable and development
configuration resolves beside the executable. An explicit `--config` wins.

## G13 trust boundary

Direct HID accepts exact VID `046D`, PID `C21C`, usage page `FF00`, usage
`0000`, 8-byte input reports, and 992-byte output reports. The output report is
ID `0x03`, a 32-byte header, and a 960-byte packed image. Input report ID
`0x01` stores LCD button bits in byte 6.

`auto` selects the Logitech SDK when validated exact `LCore.exe` is present.
The SDK DLL is loaded from the signed canonical LCore installation after path,
file identity, AMD64 image, export, version, company, product, signer, and
certificate checks. The loaded module must still match the checked file.

Direct HID requires both exact `LCore.exe` and
`logi_lamparray_service.AMD64.exe` to be absent and fails closed if process
enumeration fails. Both names are checked before open, after open, and before
each write. `LCore.exe` is additionally checked while direct HID is idle;
LampArray is not polled while idle. LCDSirPlus never terminates competing
Logitech software.

`--hardware-discover` scans at most 256 HID interfaces until the first exact G13
match. It reports that candidate plus preceding rejection reasons, omits device
paths, performs no writes, and does not inspect competing processes.

## Data flow

```text
Windows + GPU driver + optional HWiNFO/LibreHardwareMonitor/devices/PresentMon
    -> Snapshot + 30-second histories
    -> selected main layout + effective slot values + overlays
    -> 160x43 Frame
    -> G13 transform/write and preview draw
```

The renderer receives canonical readings only. It does not poll external data.
All three main layouts share the header and four-button row. Golden hashes in
[REFERENCE-LAYOUT.md](REFERENCE-LAYOUT.md) pin the logical pixel output.

The effective slot resolver normally uses the stored selection. Dynamic
substitution temporarily renders another display value without mutating slot
state in three cases:

- `PROC_HANG` has no target;
- `BOTTLENECK` currently reports `NONE`;
- `presentmon_deferred 1` and the selected PresentMon value is unavailable.

The scan skips `PROC_HANG` and `BOTTLENECK`. PresentMon deferral also skips
other unavailable PresentMon values. No eligible value produces `CLEAR`.

Button 4 acknowledges the highest active alert before ordinary cycling. A
selected `PROC_HANG` owns its physical slot button and exact press-bound target.
Render priority is hung-action progress, unacknowledged alert, Discord speakers,
then the dashboard.

## Data-source rules

Windows and installed GPU driver APIs are preferred. HWiNFO and
LibreHardwareMonitor fill only readings without a usable preferred source.
LCDSirPlus does not read raw MSR, SMBus, EC, or Super-I/O registers.

CPU topology uses native L3-cache domains with NUMA fallback. A manual topology
requires both `ccd_cache_processors` and `ccd_frequency_processors` to be
`auto`, or both to be explicit, disjoint lists of logical processors `0..63`.
One resulting domain uses the single CPU bar; two use Cache and Frequency bars.

HWiNFO access is read-only shared memory with exact original-label pairs.
LibreHardwareMonitor is user-managed and restricted to loopback HTTP. Exact
SensorIds come from its live `data.json`, not diagnostics. Total power is never
created by adding components; CPU plus GPU power is a separate subtotal.

Network throughput uses Windows counters and sends no traffic. The optional
quality worker accepts one IP literal, one overall timeout, and no new I/O after
disable or safe mode. Policy changes discard prior probe history.

## PresentMon lifecycle

Release packaging pins the signed PresentMon v2.5.1 console by size, SHA-256,
signer, and required flags. Cargo does not copy it. Automatic discovery accepts
only the approved adjacent executable. Arguments are passed without a shell.

Default `presenting` starts one child across presenter changes. Rows are grouped
by process identity. Invalid, excluded, LCDSirPlus, ended, or changed identities
are rejected. Only the selected identity owns full statistics. A switch resets
the session without restarting the child.

Automatic mode can read the fixed current-user NVIDIA App
`ApplicationStorage.json`. Input size, record count, path count, file identity,
and schema are limited. A valid catalog keeps normalized exact paths only when
all required game flags qualify. Raw JSON, names, paths, and record details are
not logged or retained. No write, NVIDIA process call, DRS use, Xbox source, or
catalog network access occurs.

For catalog matching, exact path spelling means an absolute drive path
normalized to lowercase backslashes. Relative paths, namespaces, parent
components, alternate streams, and trailing-dot/space components are rejected;
process creation time and executable identity still bind the selected process.

A valid catalog admits only qualified exact paths. Generic graphics/CPU
workload ranking is available only when the catalog is unavailable.
`presentmon_persist 1` supplies that generic path in automatic mode even when a
catalog is valid. It does not bypass identity checks, exclusions, limits,
switching rules, expiry, or cleanup.

Frame readings become stale after five seconds and expire after ten. Shutdown
stops and joins only the LCDSirPlus-owned child and trace session.

## Discord boundary

Discord Desktop candidates are limited to local pipe names 0 through 9. The
server must match the current user/session, a fixed local Discord executable,
valid signature, and Discord publisher before client or credential data is
sent. Frames and HTTPS responses have fixed size limits. Remote error bodies are
not logged.

Authorization uses local Discord Desktop plus outgoing HTTPS. It opens no
browser or callback listener. The generic OAuth flow requires a client secret.
Credentials are keyed by immutable Discord user ID and protected for the current
Windows account. Safe mode starts no Discord discovery, IPC, credential, or
HTTPS work.

## Hung-action boundary

The detector enumerates visible titled top-level windows, applies fixed and
configured exclusions, and sends timed `WM_NULL` queries. It publishes a target
only after the configured repeated timeouts and minimum duration. The detector
cannot terminate a process.

Button-down binds the exact displayed identity. Hold state cancels on release,
recovery, identity or selection change, device loss, source failure, settings
change, disable, or safe mode. The action boundary repeats visibility, timeout,
path, exclusion, and process-identity checks before requesting termination with
only query, synchronize, and terminate rights. Late holds are refused.

`hang_enabled` defaults off. Physical-button qualification with a disposable
child remains pending.

## Diagnostics boundary

Diagnostics is a separate offline command. It reads no configuration and starts
no G13, data source, probe, Discord, or action work. Its store-only ZIP is capped
at 1 MiB total and 128 KiB per entry and contains exactly `privacy.txt`,
`report.txt`, and `manifest.txt`. The report uses a fixed allowlist. Raw logs,
configuration, credentials, identities, content, titles, paths, addresses,
targets, environment, command lines, registry values, serials, directory
listings, and device paths are excluded.

## Packaging boundary

`scripts/Build.ps1` requires a clean exact Git HEAD and stages from
`git archive HEAD`, not the worktree. It runs tests before producing portable
and source ZIPs plus the Inno Setup EXE. ZIP members use explicit allowlists and
sorted checksum/size manifests. The setup is built twice and must match.

The installer supports current-user and all-users scope. It owns a stable
per-installing-user sign-in task and verifies its complete definition before
change or removal. Update and repair preserve LocalAppData. Uninstall removes
installed files and the owned task but preserves user settings, logs, and
credentials.
