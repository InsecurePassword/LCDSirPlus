# LCDSirPlus 0.3.0 Release Notes

## Release status

These notes describe the LCDSirPlus 0.3.0 release. Older b7 builds do not
contain every delta below and are not final 0.3.0 builds.

## Deltas

- Adds one-shot recovery for a lost, previously working physical G13
  connection while LCDSirPlus remains responsive: one LCDSirPlus relaunch,
  followed if needed by one trusted Logitech Gaming Software restart.
  A persistent recovery log prevents repeated attempts across launches and
  records failures; initial connection failures and application crashes do
  not trigger recovery. Safe mode disables these restart actions.

- Adds three fixed main layouts, four button slots, and 51 exact display values.
  [modules.md](modules.md) is the sole option table.
- Adds trusted Logitech Gaming Software access, direct Logitech G13 HID access,
  and a virtual preview that does not require a G13 or take focus from the active
  app.
- Adds native Windows CPU, memory, audio, disk, connection, battery, and network
  readings; NVIDIA/AMD GPU readings; XInput and supported SteelSeries receiver
  state; and HWiNFO and LibreHardwareMonitor support.
- Adds 30-second graphs, safe mode, bounded offline diagnostics, and Discord
  Desktop connection status.
- Adds PresentMon automatic selection with
  `presentmon_target_mode presenting`. The release package places
  one Intel-signed PresentMon v2.5.1 console beside `LCDSirPlus.exe`.
- Adds `presentmon_deferred 1` by default. An unavailable PresentMon option
  temporarily shows the next eligible option without changing the stored
  selection. `PROC_HANG` is not a temporary replacement; an
  all-unavailable list shows `CLEAR`.
- Adds `presentmon_persist 0` as the normal game preference. A valid NVIDIA App
  catalog rejects ordinary desktop apps; if it is unavailable, fallback may
  select another app. Setting it to `1` permits desktop apps even with a valid
  catalog. A filename and FPS may appear, and old readings are not preserved.
- PresentMon now tolerates a stale false NVIDIA fingerprint value only for an
  otherwise-eligible exact live executable path. Every other catalog, path,
  process identity, exclusion, workload, staleness, and cleanup gate is
  unchanged. There is no configuration migration or default change. A
  previously suppressed app may now show its filename and FPS; disable
  PresentMon if that exposure is unacceptable.
- Inactive `PROC_HANG` selection now temporarily shows the next
  eligible option without changing the stored selection.
- Removes the non-hang warning/alert subsystem and the bottleneck classifier
  completely: the evaluators, thresholds, retained episodes, severity ranking,
  linger and re-arm timers, queues, acknowledgement, pane flashing and
  inversion, full-screen warning takeover, the `ALERTS` and `BOTTLENECK`
  options, their 17 settings, and the collector demand that existed only to
  feed them are deleted, with no disabled implementation, hidden toggle,
  empty manager, always-CLEAR shim, or replacement alarm classifier. Nothing
  ordinary can request attention, flash, latch after recovery, or consume a
  button press any more, and no provider is polled for a deleted alarm.
- Keeps every reading: core CPU/GPU/GPU-memory/memory monitoring with bars,
  numbers and histories, temperatures, THERMALS, battery and charging states,
  `POWER_LIMIT`, disk, network, FPS, power, audio, status, and stutter counts.
  `cpu_temp_max_c` and `gpu_temp_max_c` remain as graph scales only. Keeps all
  log levels, including `warn` and `error`, the offline diagnostics bundle, the
  connection-recovery journal, the resource soak records, and the hung-action
  audit log: a log severity is a diagnostic, not a user-facing alarm.
- Keeps the hung-window `PROC_HANG` feature exactly as before: detection,
  identity and self checks, slot ownership, short-press and long-press
  semantics, cancellation, safe-mode refusal, guarded termination, audit, and
  outcomes. It is the only remaining attention path. Its limits stay honest: a
  `TerminateProcess` request can return before the process exits, so a
  requested termination is never reported as a confirmed one.
- Removes the automatic full-screen Discord speaker takeover. The Discord
  provider, its dynamic component, authorization, credentials, and lazy
  lifecycle are unchanged, and `PROVIDER_STATUS` keeps reporting the Discord
  connection state; no speaker list is drawn on the panel, and no replacement
  pane was added.
- Keeps network probing and the destructive hung-window action off by default.
- Adds independent download/inbound and upload/outbound network graph ceilings,
  both defaulting to `1000` Mbps.

## Migration

- Existing installed settings are preserved during update or repair, except the
  retired alert/bottleneck settings. The current parser rejects each of the 17
  retired keys and both retired button options as ordinary unknown input, with
  the normal diagnostic, and the last valid configuration stays active. The
  supplied migration removes only those lines and the retired `ALERTS` and
  `BOTTLENECK` button values, preserves every other line, order, quoting, and
  include, and replaces a button list that became empty with `CLOCK`. Default
  slot 4 becomes `THERMALS POWER_LIMIT`. Reapplying the migration changes
  nothing. Nothing is silently ignored and no obsolete-key warning repeats.
- Existing `network_graph_ceiling_mbps` lines remain active and set both new
  directional ceilings at their parse position. Later directional assignments
  replace one direction; a later legacy assignment replaces both.
- `ccd_source` accepts `auto` or `manual`. Manual
  `ccd_cache_processors` and `ccd_frequency_processors` indexes are `0..63`;
  both lists must be explicit together and must not overlap.
- Review `presentmon_deferred` and `presentmon_persist` after migration. Set
  `presentmon_enabled 0` if executable-name or FPS exposure is unacceptable.
- Keep `hang_enabled 0`. Do not migrate an experimental enabled value into
  normal use.

See [CONFIGURATION.md](docs/CONFIGURATION.md) for configuration syntax and
accepted values.

## Update or remove

To update to 0.3.0, exit LCDSirPlus, verify the setup checksum, and run setup in
the same current-user or all-users scope. Running the same setup
repairs installed files. Remove an opposite-scope installation owned by the
same Windows account before changing scope.

Uninstall from Windows **Installed apps**. It removes installed files,
shortcuts, and its owned sign-in task but preserves configuration, logs, and
Discord credentials under `%LOCALAPPDATA%\LCDSirPlus`. Remove that folder only
when its data is no longer needed, and revoke Discord access separately.

For a portable copy, set `start_at_login 0`, run that exact copy once, exit it,
and then remove its folder.

## Known limitations

- Windows 11 x64 is required. A G13 is required only for physical display and
  button use.
- The LCDSirPlus packages are unsigned. Included PresentMon content has
  its own Intel signature.
- PresentMon capture can require **Performance Log Users** membership followed
  by sign-out and sign-in. Elevation is for diagnosis only.
- HWiNFO and LibreHardwareMonitor are not package contents. HWiNFO64
  Free shared-memory monitoring has a 12-hour limit; LibreHardwareMonitor must
  be started and configured locally by the user.
- Only documented SteelSeries Arctis/GameBuds wireless USB receivers are
  supported; Bluetooth-only and wired models are not.
- The destructive hung-window action can lose unsaved work. Its physical
  behavior has not completed release testing, and no end-user test is supported.
- The packaged `docs/LCDSirPlus-Instruction-Manual.pdf` predates the
  alert/warning and bottleneck removal and describes obsolete behavior,
  including the retired options, the 17 retired settings, alert
  acknowledgement, and the automatic speaker takeover. The Markdown manual is
  authoritative; regenerating the PDF is the separate packaging lane, and no
  distribution-ready claim applies to the current PDF.
