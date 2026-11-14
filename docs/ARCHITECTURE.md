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

The displayed CPU quantity is total-minus-idle busy time: `providers/cpu.rs::busy_percent`
is the single derivation, forming per-logical-processor `idle`/`kernel`/`user` counter
deltas and reporting `(d_kernel + d_user - d_idle) / (d_kernel + d_user)` as a percent.
The CPU bar, the CPU load graph, history, the bottleneck peak input, and the Cache and
Frequency split all consume that one derivation; no surface derives a CPU percentage on
its own. Decision 2026-11-13 (EC-5): the shipped LCDSirReal app displays kernel+user
busy-time deltas from its own sampler, and this product intentionally displays
total-minus-idle. The Cache and Frequency split publishes `Metric::invalid()` when the
per-logical-processor sample is empty, so the split path shares the CPU bar's
invalid-on-no-data discipline: a failed Nt-query reads as unavailable there too, never as a
current 0% bar.

HWiNFO access is read-only shared memory with exact original-label pairs.
LibreHardwareMonitor is user-managed and restricted to loopback HTTP. Exact
SensorIds come from its live `data.json`, not diagnostics. Total power is never
created by adding components; CPU plus GPU power is a separate subtotal.

Network throughput uses Windows counters and sends no traffic. The optional
quality worker accepts one IP literal, one overall timeout, and no new I/O after
disable or safe mode. Policy changes discard prior probe history.

## Provider config re-arm checklist (EC-8)

One producer moves the config generation: the reload path in `src/app.rs`
(`src/app.rs:222` `telemetry_runtime.update_config(&cfg)`), and the published telemetry stays
generation-checked (`src/app.rs:191-195`) with explicit invalidation
(`src/app.rs:1202-1212` `invalidate_telemetry_publication`). Every provider loop reads config
and generation together through the one existing pattern, `current_request(&config, &generation)`
(first provider copy: `src/providers/network.rs:190`), and clears the in-flight policy state it
owns when the generation moves. `src/providers/mod.rs::tests::ec8_docs_checklist_names_every_provider`
pins every provider module declared in `src/providers/mod.rs` to one row of this table, so the
checklist cannot drift from the tree.

| Provider module | Loop + generation observation (current lines) | In-flight state cleared on a bump | Mid-flight coverage |
|---|---|---|---|
| `src/providers/network.rs` | own worker: `worker` L200, loop L212, `current_request` L213, post-probe re-check L264, in-probe policy cancellation closure L292-296 | probe history, policy, cadence, disabled-publish gate: L216-219, L232-234, L268-270 | `network_config_change_midflight_clears_baseline_and_republishes_the_new_generation`; post-probe re-check arms: `generation_bump_midprobe_discards_the_stale_outcome_and_republishes_the_new_generation`, `policy_content_change_midprobe_discards_the_stale_outcome_and_republishes_a_cleared_baseline`, `policy_disabled_midprobe_discards_the_stale_outcome_and_republishes_the_disabled_state`; probe cancellation: `canceled_tcp_probe_starts_no_connection`, `shutdown_before_auto_fallback_starts_no_tcp_io` |
| `src/providers/hang.rs` | own worker: `worker` L834, loop L852, `current_request` L853, post-poll re-check L921 | tracker evidence, cadence, policy, published availability/targets, disabled gate: L856-861, L926-929 (`reset_on_policy_change` L742) | `hang_config_change_midflight_clears_tracker_and_republishes_the_new_generation`; state reset: `disabled_policy_clears_tracker_and_cadence_reloads`, `changed_or_disabled_policy_discards_poll_and_resets_evidence` |
| `src/providers/presentmon.rs` | own worker: `run` L1400, loop L1408, `current_request` L1409; pre-spawn gate `confirm_start_request` L1256 with `current_request` L1265 | capture identity, child/ETW-session ownership, statistics, publication-dedupe state: L1411-1416 via `stop_and_report_cleanup` L2109; start refused on a moved generation L1272-1279 | `presentmon_config_change_midflight_clears_capture_and_republishes_the_disabled_reason`; `start_refuses_a_changed_final_request_before_spawn`, `presenting_flow_exit_overflow_and_config_cleanup_clear_identity_state` |
| `src/providers/discord.rs` | own worker closure in `spawn` L2161, loop L2174, config re-read every iteration L2175-2178, mid-session checks L1872 and L1885, backoff-wait interrupt L2219-2224. It owns a private `RwLock<Config>` refreshed by `Runtime::update_config` L2147 (called from `src/app.rs:226`) and holds no `Arc<AtomicU64>`, so it re-arms on config content through `session_config_changed` L1527 | speaking/RPC state published as `unavailable("reconfiguring")` and reconnect backoff reset to 1 s: L2198-2207; disabled re-arm resets state and backoff L2179-2185 | `lifecycle_fingerprint_safe_mode_and_backoff_are_bounded` pins the change predicate and the bounded backoff. The loop path needs a live Discord IPC pipe, so no unit test drives it |
| `src/providers/headset.rs` | telemetry sampling loop `spawn_headset` `src/telemetry.rs:1123`, loop L1134, `current_request` L1135, generation stamped on the result L1151 | coordinator success L525 and stale-generation event drop L556 | `delayed_old_generation_sensor_results_are_discarded`, `staleness_and_enablement_are_deterministic` (in `src/telemetry.rs`) |
| `src/providers/lhm.rs` | `spawn_lhm` `src/telemetry.rs:1165`, loop L1177, `current_request` L1178. No config reference in the module (0 `Config`/`cfg.` hits); URL, overrides, selectors and timeout arrive as arguments | success L526, readings L545, diagnostic L549, stale drop L559 | `lhm_values_transition_from_current_to_stale`, `config_update_replaces_shared_runtime_config` |
| `src/providers/hwinfo.rs` | `spawn_hwinfo` `src/telemetry.rs:1211`, loop L1222, `current_request` L1223, selector set and generation captured together L1234-1236. No config reference in the module (0 hits); selectors and max age arrive as arguments | readings L546, success L531, CPU temperature L539, stale drop L565-568 | `hwinfo_selector_generation_change_invalidates_temperature`, `hwinfo_diagnostic_survives_generation_and_changes_once_per_selector`, `safe_mode_clears_expansion_and_rebaselines` |
| `src/providers/gpu.rs` | `spawn_gpu` `src/telemetry.rs:1435`, loop L1447, `current_request` L1448. No config reference in the module (0 hits); `cfg.gpu_provider` is passed to `sample` L1461 | provider backend cache is dropped when the selector moves (`src/providers/gpu.rs:58-61`) and on disable L1468; coordinator success L527, backend name L543, GPU temperature/power L541-544 | `native_gpu_temperature_wins_until_stale`, `delayed_provider_updates_keep_their_generation_and_are_discarded` |
| `src/providers/netif.rs` | coordinator `Worker::tick` `src/telemetry.rs:470`, `current_request` L471, poll L805-816. No config reference in the module (0 hits) | throughput baseline provider L523 and attempt stamp L535 | `failure_clears_baseline_and_recovery_requires_a_new_interval` (`src/providers/netif.rs:138-193`, the EC-8 template), `safe_mode_runtime_owns_and_joins_all_threads` |
| `src/providers/performance.rs` | coordinator `Worker::tick` `src/telemetry.rs:470`, `current_request` L471, poll L829 with `performance.sample()` L835. No config reference in the module (0 hits) | provider L524, attempt L536, readings L547, success L528 | `formatted_values_require_valid_status_and_finite_data` (module), `gated_provider_sampling_uses_keep_alive_bound_not_full_cadence`, `consumer_set_publication_is_visible_to_the_scheduler` |
| `src/providers/connections.rs` | coordinator `Worker::tick` `src/telemetry.rs:470`, `current_request` L471, poll L871 with `connections::sample()` L877. No config reference in the module (0 hits) | attempt L876/L537, reading L548, success L886/L529 | `decoder_is_bounded_and_counts_only_established` (module), `consumer_rejoin_restores_full_cadence_on_the_next_scheduled_tick`, `attempts_not_successes_control_cadence` |
| `src/providers/system_power.rs` | coordinator `Worker::tick` `src/telemetry.rs:470`, `current_request` L471, poll L896 with `system_power::sample()` L902. No config reference in the module (0 hits) | battery attempt L901/L538, success L916/L530 | `decoder_distinguishes_unknown_zero_and_no_battery` (module), `staleness_and_enablement_are_deterministic` |
| `src/providers/audio.rs` | coordinator `Worker::tick` `src/telemetry.rs:470`, `current_request` L471, poll L779 with `audio::read()` L790. No config reference in the module (0 hits) | audio attempt stamp L534 | none in the module (stateless `read()`, no in-flight policy state to reset); `staleness_and_enablement_are_deterministic` covers the coordinator state |
| `src/providers/xinput.rs` | coordinator `Worker::tick` `src/telemetry.rs:470`, `current_request` L471, poll L742-753. No config reference in the module (0 hits); `cfg.controller_index` arrives as an argument | controller attempt stamp L533 | `staleness_and_enablement_are_deterministic`, `query_does_not_panic_without_controller` |
| `src/providers/cpu.rs` | app loop, synchronous: sampled in `build_snapshot` (`src/app.rs:985`) at `src/app.rs:996`. The app loop owns `cfg` by value and replaces it wholesale at `src/app.rs:213` before the bump at `src/app.rs:222`, so no stale config read exists. No config reference in the module (0 hits) | per-logical-processor counter baseline inside `cpu::CpuLoadProvider`; the provider is app-loop-owned (`src/app.rs:151`) and stateless across a reload | `ec5_synthetic_records_yield_hand_derived_percents`, `ec5_all_cpu_surfaces_share_this_one_derivation` |
| `src/providers/clock.rs` | app loop, synchronous: sampled at `src/app.rs:995`. Its only config reference is `Config::default()` at `src/providers/clock.rs:22`, so no live config read exists | none: `read()` is stateless | `fixed_date_format_renders`, `explicit_short_time_patterns_render_12h_and_24h` |
| `src/providers/memory.rs` | app loop, synchronous: sampled at `src/app.rs:1009` with `memory::sample()`. No config reference in the module (0 hits) | none: `sample()` is stateless, so there is no in-flight policy state to reset | none in the module; the app loop samples it synchronously from the `cfg` value it replaced wholesale at `src/app.rs:213` |
| `src/providers/ccd.rs` | app loop, reload-driven: `src/app.rs:219` re-resolves the topology through `resolve_ccd_topology` (`src/app.rs:651-657`) inside the same reload block that bumps the generation. No config reference in the module (0 hits); explicit lists arrive as arguments | the app-loop `topology` value is replaced by the reloaded one at `src/app.rs:219` | `from_lists_builds_masks`, `from_lists_single_when_one_side_missing` |

Two bounded cadence details the checklist records without changing: the four `telemetry.rs`
sampling loops keep their loop-local `last_attempt` stamp across a bump, so a re-tuned poll
interval takes effect on the next due sample (the config value itself is re-read every
iteration), and the HWiNFO/LHM diagnostic dedupe deliberately survives a bump, as pinned by
`hwinfo_diagnostic_survives_generation_and_changes_once_per_selector`. Neither masks an
invalidation: every stale-generation result is refused, and `invalidate_telemetry_publication`
stays explicit.

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
and schema are limited. Records still require a boolean
`IsFingerprintDetected`, but its value is not a qualification predicate because
`false` can be stale. A stale `false` admits only an otherwise-eligible
noncreative, OPS-supported, nonmanual, safe normalized fixed-drive path. Raw
JSON, names, paths, and record details are not logged or retained. Parsing is
local and read only; no write, NVIDIA process call, DRS use, Xbox source, or
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
change, disable, or safe mode. A bind also requires a fresh publication: the
bound target's publish age must not exceed the probe interval in force at bind
time plus one bounded publish cycle, and the action boundary requires the
target in the current publication generation; stale publications cancel as
continuity refusals. The action boundary repeats visibility, timeout,
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
