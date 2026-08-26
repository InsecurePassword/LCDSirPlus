# LCDForge 0.3.0 product specification

## Purpose

LCDForge provides a modern fixed dashboard for the Logitech G13 160×43
monochrome LCD when games occupy the user's normal displays. The 0.3.0 Rust
port preserves the 0.2.0 product contract while removing three dependencies:
the Logitech runtime, Process Lasso, and per-tick network polling for
core telemetry.

## Normal screen

The normal screen always shows:

- Windows-local date/day/time;
- independent Cache-CCD and Frequency-CCD CPU load (dual-CCD CPUs), or one
  full-height CPU load bar (single-CCD CPUs);
- system memory load;
- GPU load and VRAM load (Phase 2 providers; explicit unavailable state until
  then);
- four button-aligned configurable telemetry modules.

The fixed layout is intentionally static. The lower slot contents are the
user-customizable surface.

## Button behavior

- Physical button 1–4 short press: cycle matching slot forward.
- Preview left click: equivalent short press. Preview right click: backward.
- Selection wraps and persists across config reloads (module identity is
  preserved when a list changes).
- Unavailable modules display an explicit unavailable/stale state; they are
  never silently replaced by unrelated data.
- Safe mode disables destructive hung-target binding/action, not harmless slot
  cycling or alert acknowledgement.

## CCD bars (Process Lasso removed)

Detection is native and automatic:

1. `GetLogicalProcessorInformationEx(RelationCache)` groups logical
   processors by L3 cache domain; NUMA nodes are the fallback grouping.
2. Pinned-thread CPUID `Fn8000_001D` compares L3 sizes to label the 3D
   V-Cache die (larger L3 = cache CCD; e.g. 96 MB vs 32 MB on the 9950X3D).
3. Per-logical-processor busy time from
   `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` deltas
   is aggregated over each die's mask. No sensors, no drivers, no services.
4. Manual override: `ccd_cache_processors` / `ccd_frequency_processors`.
5. One detected domain renders the single-bar layout.

## G13 backend

Direct HID only. The Logitech SDK backend is retired (its runtime triggers
the G HUB conflict on the reference machine; the direct-HID path was
physically confirmed during the 0.2.0 repair). Modes: `auto` (= hid),
`hid`, `virtual`. Exact device contract and I/O discipline are documented in
README.md and enforced in `src/backends/g13.rs` + `src/backends/hid.rs`.

## Required telemetry sources

- Windows local clock: date/day/time.
- Windows scheduler: per-logical-processor CPU load (CCD aggregation).
- GlobalMemoryStatusEx: memory load.
- NVIDIA NVAPI / AMD ADLX vendor DLLs provide GPU load, VRAM, and temperature;
  LibreHardwareMonitor loopback JSON is an optional temperatures-only fallback
  (Phase 2).
- PresentMon: FPS/frame timing (Phase 2).
- Arctis 7P+ USB HID: headset battery (Phase 2).
- Discord local RPC: verified active-speaker overlay with RPC OAuth and
  current-user DPAPI credential storage (Phase 3).

## Operational requirements

- Standard user; medium integrity; no elevation.
- Local-first and offline (Discord token exchange in Phase 3 is the only
  network dependency; operator probes optional).
- No arbitrary scripts/plugins; no kernel drivers; no services; no listeners.
- Provider failures isolated; explicit stale/unavailable states.
- Config hot reload preserves last valid state.
- Reconnect after device loss with bounded, capped backoff.
- No unchanged-frame submission.
- Bounded histories/logs/protocol inputs.
- Single static native binary; no runtime installation.
- One normal/direct-HID runtime per Windows session; read-only and virtual test
  commands remain available alongside it.
- Optional network-quality probes are disabled by default, bounded to one
  configured IP-literal endpoint and one overall timeout, and always disabled
  in safe mode.
- Optional current-user startup registration never replaces or removes a
  foreign Run value and is never mutated in safe mode.

## Alerts and preview

Current CPU/GPU temperature, memory/VRAM load, and headset battery readings
produce deterministic configured alert episodes. Hung-target interaction has
highest display priority, followed by unacknowledged alerts, Discord speakers,
and the dashboard. Physical button 4 acknowledges the highest episode; an
episode rearms only after clearing and recurring.

Preview mode `auto` follows physical HID availability, `always` starts visible
unless minimized, and `never` starts hidden. Backend reconnect state changes
reuse the existing preview window rather than creating another UI owner and do
not override a manual tray toggle.

## Hung-window detector

The query-only detector considers visible, titled top-level windows outside the
Windows directory and configured ignore list. A target appears only after its
exact HWND, PID, creation time, and normalized image path fail consecutive
bounded `WM_NULL` probes for the configured minimum duration. Responsiveness,
absence, identity replacement, safe mode, or disabling the detector removes it
immediately. A detector publication is display input only and is never sufficient
authorization for the guarded action.

The guarded action binds the exact selected target only at physical button-down.
The same input source must remain continuously held and release after the
configured duration; full progress alone does nothing. Recovery, disappearance,
identity or selection change, device loss, provider failure, safe mode, disable,
or any hang-policy reload irreversibly cancels that press. The release boundary
independently repeats native eligibility, timeout, exclusion, and process
identity checks before using narrowly scoped terminate/query/synchronize rights.
It never elevates or retries with broader access.

## Non-goals

- G19 color LCD; direct Logitech SDK/LCore support.
- Steam/NVIDIA overlay hooking; kernel-driver sensor access.
- Email integration; remote telemetry/control; arbitrary third-party
  modules; layout editor; Process Lasso integration.
