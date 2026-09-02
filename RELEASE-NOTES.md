# LCDSirPlus 0.3.0 Draft Release Notes

## Draft status

LCDSirPlus 0.3.0 is unpublished. The package described below is planned, and
final package testing remains pending. Older b7 builds do not contain every
delta below and are not final 0.3.0 builds.

## Deltas

- Adds three fixed main layouts, four button slots, and 53 exact display values.
  [modules.md](modules.md) is the sole option table.
- Adds trusted Logitech Gaming Software access, direct Logitech G13 HID access,
  and a virtual preview that does not require a G13 or take focus from the active
  app.
- Adds native Windows CPU, memory, audio, disk, connection, battery, and network
  readings; NVIDIA/AMD GPU readings; XInput and supported SteelSeries receiver
  state; and HWiNFO and LibreHardwareMonitor support.
- Adds 30-second graphs, alerts, safe mode, bounded offline diagnostics, and
  Discord Desktop active-speaker display.
- Adds PresentMon automatic selection with
  `presentmon_target_mode presenting`. The planned package is intended to place
  one Intel-signed PresentMon v2.5.1 console beside `LCDSirPlus.exe`.
- Adds `presentmon_deferred 1` by default. An unavailable PresentMon option
  temporarily shows the next eligible option without changing the stored
  selection. `PROC_HANG` and `BOTTLENECK` are not temporary replacements; an
  all-unavailable list shows `CLEAR`.
- Adds `presentmon_persist 0` as the normal game preference. A valid NVIDIA App
  catalog rejects ordinary desktop apps; if it is unavailable, fallback may
  select another app. Setting it to `1` permits desktop apps even with a valid
  catalog. A filename and FPS may appear, and old readings are not preserved.
- Inactive `PROC_HANG` and `BOTTLENECK` selections now temporarily show the next
  eligible option without changing the stored selection.
- Adds independent `temperature_warning_enabled` and
  `memory_warning_enabled` switches. Memory warning thresholds default to `100`
  and trigger at or above the configured value.
- Keeps network probing and the destructive hung-window action off by default.

## Migration

- Existing installed settings are preserved during update or repair, including
  existing memory warning thresholds.
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

When 0.3.0 is published, exit LCDSirPlus, verify the planned setup checksum, and
run setup in the same current-user or all-users scope. Running the same setup
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
- Final planned-package, installer, physical G13, and live Discord acceptance
  remain pending.
- The planned LCDSirPlus packages are unsigned. Planned PresentMon content has
  its own Intel signature.
- PresentMon capture can require **Performance Log Users** membership followed
  by sign-out and sign-in. Elevation is for diagnosis only.
- HWiNFO and LibreHardwareMonitor are not planned package contents. HWiNFO64
  Free shared-memory monitoring has a 12-hour limit; LibreHardwareMonitor must
  be started and configured locally by the user.
- Only documented SteelSeries Arctis/GameBuds wireless USB receivers are
  supported; Bluetooth-only and wired models are not.
- The destructive hung-window action can lose unsaved work. Its physical
  behavior has not completed release testing, and no end-user test is supported.
