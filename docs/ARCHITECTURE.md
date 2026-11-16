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
(`src/app.rs:232` `telemetry_runtime.update_config(&cfg)`), and the published telemetry stays
generation-checked (`src/app.rs:201-203`) with explicit invalidation
(`src/app.rs:1443-1452` `invalidate_telemetry_publication`). Every provider loop reads config
and generation together through the one existing pattern, `current_request(&config, &generation)`
(first provider copy: `src/providers/network.rs:190`), and clears the in-flight policy state it
owns when the generation moves. `src/providers/mod.rs::tests::ec8_docs_checklist_names_every_provider`
pins every provider module declared in `src/providers/mod.rs` to one row of this table, so the
checklist cannot drift from the tree.

| Provider module | Loop + generation observation (current lines) | In-flight state cleared on a bump | Mid-flight coverage |
|---|---|---|---|
| `src/providers/network.rs` | own worker: `worker` L200, loop L212, `current_request` L213, post-probe re-check L264, in-probe policy cancellation closure L292-296 | probe history, policy, cadence, disabled-publish gate: L216-219, L232-234, L268-270 | `network_config_change_midflight_clears_baseline_and_republishes_the_new_generation`; post-probe re-check arms: `generation_bump_midprobe_discards_the_stale_outcome_and_republishes_the_new_generation`, `policy_content_change_midprobe_discards_the_stale_outcome_and_republishes_a_cleared_baseline`, `policy_disabled_midprobe_discards_the_stale_outcome_and_republishes_the_disabled_state`; probe cancellation: `canceled_tcp_probe_starts_no_connection`, `shutdown_before_auto_fallback_starts_no_tcp_io` |
| `src/providers/hang.rs` | own worker: `worker` L859, loop L880, loop-head cached read L881 (`cached_request` L845; `current_request` L824 stays for the post-poll re-check L949) | tracker evidence, cadence, policy, published availability/targets, disabled gate: L883-889, L953-957 (`reset_on_policy_change` L742) | `hang_config_change_midflight_clears_tracker_and_republishes_the_new_generation`; state reset: `disabled_policy_clears_tracker_and_cadence_reloads`, `changed_or_disabled_policy_discards_poll_and_resets_evidence`; WO-TEL-D3-B1 (L5): `hung_worker_loop_head_reads_a_cached_arc_config` |
| `src/providers/presentmon.rs` | own worker: `run` L1420, loop L1433, `cached_request` L1434 (fn L1262); pre-spawn gate `confirm_start_request` L1276 with `current_request` L1285; WO-TEL-D2 (U-3) zero-demand park: the `demanded(.., "presentmon")` gate L1455-1463 (mechanism note L1443-1454) runs the shipped cleanup arm once, publishes one deduped `unavailable("no display consumer")` and parks on the existing 50 ms sleep, so `reconcile` (L1464 - target selection, executable resolution, spawn retry ladder) is never reached | capture identity, child/ETW-session ownership, statistics, publication-dedupe state: L1435-1442 via `stop_and_report_cleanup` L2155; start refused on a moved generation L1292-1299; a demand release runs that same cleanup once (L1456) | `presentmon_config_change_midflight_clears_capture_and_republishes_the_disabled_reason`; `start_refuses_a_changed_final_request_before_spawn`, `presenting_flow_exit_overflow_and_config_cleanup_clear_identity_state`; zero-demand park: `presentmon_run_gate_stops_target_poll_and_child_spawn_at_zero_demand` |
| `src/providers/discord.rs` | WO-TEL-D3-C split: this module is the HOST side of an optional dynamic component. The declared provider set is 18 modules in `src/providers/mod.rs` - 17 providers that live wholly in the app, plus this one, whose RPC/OAuth engine now lives in 1 out-of-exe component (18 declared = 17 in-app + 1 split pair). The module-boundary machinery: `component_path` L496, `validate_table` L511, `load_table_from` L530, `load_production_table` L590 (the v1 handshake: the single export `lcsd_get_table`, one size-checked 10-entry function table, POD payloads, status-code returns). Consumer rule L2201 (`provider_enabled`: `discord_enabled` AND a configured `discord_client_id`); `spawn` L1142 delegates to `spawn_with` L1149, which creates the worker thread ONLY under that gate L1164 and, when it does not, publishes the parked `disabled` state once L1237 and starts no thread, loads no module and touches no credential. The worker `spawn_worker` L1182 is the only thread that touches the module and drives it synchronously: cached loop-head read L1221 (`cached_request` L2241), `update_config` L1091 (called from `src/app.rs:236`) writes then bumps the worker-visible generation L1095 (the WO-TEL-D2/D3-B1 L3 pairing, byte-identical), content re-arm through `session_config_changed` L2183, the mid-session cancellation reproduces the shipped closure (worker shutdown OR a content change), the component step quantum L1321 (`poll_io`), backoff ladder `next_backoff` L2215. Unload order stop L930 -> join L944 -> release -> free, in `unload_module` L921; a component reporting work in flight REFUSES the unload: the image is retained (leaked, so no free can race live code) and REPORTED L1233, never claimed as a full unload. The disable edge ENDS the worker thread L1239. Credentials, DPAPI and the IPC/Authenticode verifier stay in the executable: `host_provide_token` L702, `host_store_credential` L753, `credential_path` L1474, `save_credential` L1717, `load_credential_blob` L1787, `clear_token` L1814 (the CLI clear is DLL-free: it names no loader, module handle, table or ABI entry), `verify_authenticode` L2020 with its bounded wrapper L2112 and `verify_pipe` L2135; the one-shot CLI `authorize` L1430 is the documented temporary load. The RPC framing, payload codec, OAuth classification, speaker tracker, token-record codec, WinHTTP transport and session driver MOVED into the sibling workspace cdylib `components/lcdsirplus_discord`, whose ABI mirror is byte-identical to this module's block and which creates no thread, opens no file, logs nothing and names no app type | host `Runtime` L1017 is the only owner: a bump re-arms the cached loop-head read; a content change ends the component session (`LCSD_E_CANCELED` arm L1331 via `end_session`) and publishes `unavailable("reconfiguring")` L1335 with the backoff reset to 1 s L1337; the disable edge publishes `disabled` once L1237 then joins the thread; `unload_module` L921 clears the handle and the module, refusing and reporting when the component is in flight; `impl Drop for Runtime` L1132 runs the same stop -> join -> release order at teardown. Rows are published from the host-side projection `state_from_pod` L828 through `publish_component_state` L1384, only when the component's own revision moves, so a stale session is never published as fresh | shipped: `lifecycle_fingerprint_safe_mode_and_backoff_are_bounded` (change predicate + bounded backoff), `provider_enabled_requires_a_configured_client_id` (the gate predicate), `safe_mode_runtime_publishes_disabled_without_starting_a_session`, `per_account_credentials_are_independent_and_path_bound`, `clear_removes_only_exact_owned_credentials_and_legacy`, `credential_boundary_rejects_hard_link_and_reparse_targets`, `dpapi_round_trip_is_current_user_and_bounded`, `endpoint_policy_requires_every_identity_property`; the live loop path still needs a real Discord IPC pipe, so no unit test drives it. WO-TEL-D3-B1: `discord_park_loop_head_reads_a_cached_arc_config` (restated for the split: zero per-tick `Config` clones, one cached loop-head read, and the L4 publish-once park, now structural), `discord_park_publishes_its_disabled_state_once`, `discord_source_pin_of_the_session_config_read_field_set`. WO-TEL-D3-C new: `discord_disabled_never_loads_never_spawns_a_worker_never_touches_credentials`, `discord_enabled_demanded_loads_and_starts_exactly_once`, `discord_handshake_refusals_never_activate_the_provider`, `discord_missing_dll_is_non_fatal_with_one_bounded_diagnostic_and_no_retry`, `discord_missing_module_reports_once_and_never_retries`, `discord_hot_reload_set_clear_set_loads_unloads_and_never_publishes_stale_as_fresh`, `discord_toggle_fifty_cycles_leaves_no_thread_handle_or_module_drift`, `discord_module_is_absent_across_toggle_cycles_when_not_installed`, `discord_refuses_unload_while_the_component_reports_work_in_flight_and_retains_the_module`, `discord_cli_clear_token_is_dll_free`, `discord_authorize_is_a_documented_temporary_load_and_clear_needs_none`, `discord_state_projection_keeps_the_renderer_consumers_supplied`, `discord_abi_blocks_stay_byte_identical_across_the_boundary`, `discord_component_keeps_host_types_and_logging_out_of_its_production_text`; the component's own boundary proofs run in that package (`handshake_refuses_bad_magic_version_or_size`, `engine_lifecycle_is_balanced_and_quiesces_after_stop`, `a_step_in_flight_makes_stop_join_and_release_refuse`, `settings_mirror_reads_exactly_the_pinned_seven_fields`, `component_source_carries_no_app_types_and_no_secret_logging`, `abi_block_is_byte_identical_across_the_boundary`) |
| `src/providers/headset.rs` | telemetry sampling loop `spawn_headset` `src/telemetry.rs:1373`, sampler seam param L1379 (production `spawn` passes the real `headset::query` at L1848), loop L1387, `cached_request` L1388, generation stamped on the result L1413 | coordinator success L683 and stale-generation event drop L716 | `delayed_old_generation_sensor_results_are_discarded`, `staleness_and_enablement_are_deterministic` (in `src/telemetry.rs`); cadence re-arm driven on the real loop with the sampler injected: `telemetry_headset_loop_behavioral_generation_bump_rearms_sampling`; zero-demand park + resume: `telemetry_headset_loop_zero_demand_parks_and_resume_samples_within_bound` |
| `src/providers/lhm.rs` | `spawn_lhm` `src/telemetry.rs:1434`, sampler seam param L1440 with its `LhmSampler` fn-pointer alias L1427-1432 (production `spawn` passes the real `lhm::sample_extended` at L1856), loop L1458, `cached_request` L1459, sampler call L1488. No config reference in the module (0 `Config`/`cfg.` hits); URL, overrides, selectors and timeout arrive as arguments | success L684, readings L704, diagnostic L708, stale drop L719 | `lhm_values_transition_from_current_to_stale`, `config_update_replaces_shared_runtime_config`; cadence re-arm driven on the real loop with the sampler injected: `telemetry_lhm_loop_behavioral_generation_bump_rearms_sampling`; provider-outcome backoff replacing the shipped inversion: `telemetry_lhm_loop_provider_outcome_backoff_is_bounded_by_keep_alive` |
| `src/providers/hwinfo.rs` | `spawn_hwinfo` `src/telemetry.rs:1511`, sampler seam param L1517 (production `spawn` passes the real `hwinfo::sample` at L1872), loop L1525, `cached_request` L1526, selector set and generation captured together L1544-1546. No config reference in the module (0 hits); selectors and max age arrive as arguments | readings L705, success L689, CPU temperature L698, stale drop L725-728 | `hwinfo_selector_generation_change_invalidates_temperature`, `hwinfo_diagnostic_survives_generation_and_changes_once_per_selector`, `safe_mode_clears_expansion_and_rebaselines`; cadence re-arm driven on the real loop with the sampler injected: `telemetry_hwinfo_loop_behavioral_generation_bump_rearms_sampling`; zero-demand park + resume: `telemetry_hwinfo_loop_zero_demand_parks_and_resume_samples_within_bound` |
| `src/providers/gpu.rs` | `spawn_gpu` `src/telemetry.rs:1751`, sampler seam param L1757 with the one-line adapter `gpu_sample` L1747-1749 (its body is the shipped method call; production `spawn` passes the adapter at L1864), loop L1779, `cached_request` L1780. No config reference in the module (0 hits); `cfg.gpu_provider` is passed to `sample` L1796 | provider backend cache is dropped when the selector moves (`src/providers/gpu.rs:58-61`) and on disable L1807; coordinator success L685, backend name L702, GPU temperature/power L700-703; WO-TEL-D3-B1 core tier: the four CORE rows go explicit-unavailable via `publish_core_gpu_unavailable` L859, called from the `apply_gpu` failure arm L898 and the freshness disabled arm L1217, with the park loop sending exactly one core event (flag reset L1788, publish L1814-1823); the shipped DG-4 clear arm (success L685, backend L702, temperature/power L700-703) stays as accepted | `native_gpu_temperature_wins_until_stale`, `delayed_provider_updates_keep_their_generation_and_are_discarded`; cadence re-arm driven on the real loop with the sampler injected: `telemetry_gpu_loop_behavioral_generation_bump_rearms_sampling`; provider-outcome backoff bounding the vendor re-open chain: `telemetry_gpu_loop_provider_outcome_backoff_is_bounded_by_keep_alive`; zero-demand park + resume: `telemetry_gpu_loop_zero_demand_parks_and_resume_samples_within_bound`; WO-TEL-D3-B1 core tier: `gpu_disabled_park_publishes_explicit_core_unavailable_rows_once`, `gpu_loop_park_publishes_one_core_unavailable_event` |
| `src/providers/netif.rs` | coordinator `Worker::tick` `src/telemetry.rs:595`, `cached_request` L599, poll L1014-1033. No config reference in the module (0 hits) | throughput baseline provider L681 and attempt stamp L693 | `failure_clears_baseline_and_recovery_requires_a_new_interval` (`src/providers/netif.rs:138-193`, the EC-8 template), `safe_mode_runtime_owns_and_joins_all_threads` |
| `src/providers/performance.rs` | coordinator `Worker::tick` `src/telemetry.rs:595`, `cached_request` L599, poll L1046 with `performance.sample()` L1063. No config reference in the module (0 hits) | provider L682, attempt L694, readings L706, success L686 | `formatted_values_require_valid_status_and_finite_data` (module), `gated_provider_sampling_uses_keep_alive_bound_not_full_cadence`, `consumer_set_publication_is_visible_to_the_scheduler` |
| `src/providers/connections.rs` | coordinator `Worker::tick` `src/telemetry.rs:595`, `cached_request` L599, poll L1104 with `connections::sample()` L1116. No config reference in the module (0 hits) | attempt L1114/L695, reading L707, success L1125/L687 | `decoder_is_bounded_and_counts_only_established` (module), `consumer_rejoin_restores_full_cadence_on_the_next_scheduled_tick`, `attempts_not_successes_control_cadence` |
| `src/providers/system_power.rs` | coordinator `Worker::tick` `src/telemetry.rs:595`, `cached_request` L599, poll L1135 with `system_power::sample()` L1147. No config reference in the module (0 hits) | battery attempt L1145/L696, success L1161/L688 | `decoder_distinguishes_unknown_zero_and_no_battery` (module), `staleness_and_enablement_are_deterministic` |
| `src/providers/audio.rs` | coordinator `Worker::tick` `src/telemetry.rs:595`, `cached_request` L599, poll L982 with `audio::read()` L999. No config reference in the module (0 hits) | audio attempt stamp L692 | none in the module (stateless `read()`, no in-flight policy state to reset); `staleness_and_enablement_are_deterministic` covers the coordinator state |
| `src/providers/xinput.rs` | coordinator `Worker::tick` `src/telemetry.rs:595`, `cached_request` L599, poll L939-956. No config reference in the module (0 hits); `cfg.controller_index` arrives as an argument | controller attempt stamp L691 | `staleness_and_enablement_are_deterministic`, `query_does_not_panic_without_controller` |
| `src/providers/cpu.rs` | app loop, synchronous: sampled in `build_snapshot` (`src/app.rs:1226`) at `src/app.rs:1237`. The app loop owns `cfg` by value and replaces it wholesale at `src/app.rs:223` before the bump at `src/app.rs:232`, so no stale config read exists. No config reference in the module (0 hits) | per-logical-processor counter baseline inside `cpu::CpuLoadProvider`, plus the WO-TEL-D3-B1 (L7) resident sample buffer: `RECORDS` L23, the `buffer` field L116 and the guarded single allocation at L126-130 (`src/providers/cpu.rs:109`); the provider is app-loop-owned (`src/app.rs:152`) and stateless across a reload | `ec5_synthetic_records_yield_hand_derived_percents`, `ec5_all_cpu_surfaces_share_this_one_derivation`, `cpu_provider_reuses_one_resident_sample_buffer` |
| `src/providers/clock.rs` | app loop, synchronous: sampled at `src/app.rs:1236` as `clock::read(&cfg.date_format)`. The WO-TEL-D3-B1 (L6) seam takes the display format as a parameter (`src/providers/clock.rs:28-31`), so the sampling path builds no configuration value and performs no config read of its own; the app loop passes the live `cfg.date_format`, the same value the renderer slots use | none: `read()` is stateless, so there is no in-flight policy state to reset | `fixed_date_format_renders`, `explicit_short_time_patterns_render_12h_and_24h`, `read_uses_the_supplied_date_format_and_builds_no_config` |
| `src/providers/memory.rs` | app loop, synchronous: sampled at `src/app.rs:1250` with `memory::sample()`. No config reference in the module (0 hits) | none: `sample()` is stateless, so there is no in-flight policy state to reset | none in the module; the app loop samples it synchronously from the `cfg` value it replaced wholesale at `src/app.rs:223` |
| `src/providers/ccd.rs` | app loop, reload-driven: `src/app.rs:229` re-resolves the topology through `resolve_ccd_topology` (`src/app.rs:808-810`) inside the same reload block that bumps the generation. No config reference in the module (0 hits); explicit lists arrive as arguments | the app-loop `topology` value is replaced by the reloaded one at `src/app.rs:229` | `from_lists_builds_masks`, `from_lists_single_when_one_side_missing` |

Two bounded cadence details the checklist records without changing: the four `telemetry.rs`
sampling loops clear their loop-local `last_attempt` stamp on the loop head when the generation
moves (the same re-arm the network worker does), so a re-tuned poll interval takes effect on the
immediately-next sample and no stamp taken under the old policy suppresses it; under an unchanged
generation the configured cadence is untouched. That re-arm is verified behaviorally on the shipped
loop bodies through the per-loop sampler seams (each real loop thread is driven, only the sampler
is injected, and the assertions read the events the loop publishes):
`telemetry_headset_loop_behavioral_generation_bump_rearms_sampling`,
`telemetry_lhm_loop_behavioral_generation_bump_rearms_sampling`,
`telemetry_hwinfo_loop_behavioral_generation_bump_rearms_sampling`,
`telemetry_gpu_loop_behavioral_generation_bump_rearms_sampling`; the same shape is pinned
structurally by `telemetry_sampling_loops_clear_cadence_only_on_generation_change`. The HWiNFO/LHM
diagnostic dedupe deliberately survives a bump, as pinned by
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
