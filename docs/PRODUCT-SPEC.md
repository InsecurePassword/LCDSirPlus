# LCDSirPlus 0.3.0 product specification

Status: **Draft / Unreleased**. Live PresentMon and Discord gates described
below remain open; this specification is not a completed-release claim.

## Purpose

LCDSirPlus provides three selectable fixed built-in displays for the Logitech
G13 160×43 monochrome LCD when games occupy the user's normal displays. The 0.3.0 Rust
port preserves the 0.2.0 product contract while removing Process Lasso and
per-tick network polling for core telemetry. Direct HID requires no Logitech
runtime, while the supported SDK path interoperates with a trusted running
LCore installation.

## Normal screen

Every normal main display shows:

- Windows-local date/day/time;
- independent Cache-CCD and Frequency-CCD CPU load (dual-CCD CPUs), or one
  full-height CPU load bar (single-CCD CPUs);
- system memory load;
- four button-aligned configurable telemetry modules.

Layouts 1 and 2 also show GPU and VRAM load from available native vendor
providers, with explicit unavailable state when prerequisites are absent.
Layouts 2 and 3 show current network ingress and egress.

`main_display` selects layout 1 (original CPU/RAM and GPU/VRAM halves), layout 2
(CPU/RAM, GPU/VRAM, and OUT/IN thirds), or layout 3 (original CPU/RAM paired
with NET IN/NET OUT). It defaults to 1, accepts only 1..3, and applies on valid
hot reload. All three share the compact date/time header and fixed four-button
row. The network bars show current throughput using
`network_graph_ceiling_mbps`; 1000 Mbps is full scale for 1 Gbps.

The slot registry contains exactly 53 tokens. Graph variants show exactly the
trailing 30 seconds. Configured ceilings affect graph scale; additionally,
`network_graph_ceiling_mbps` scales the network bars in Main Displays 2
and 3.
Native system, disk, connection, battery, and vendor GPU sources require no
helper. Board, cooling, total-power, and CPU-temperature fallbacks fail closed
when their exact optional source is unavailable.

The three built-in layouts are intentionally fixed; there is no layout editor.
The selected layout and lower slot contents are the user-customizable surface.

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

Modes are `auto`, `sdk`, `hid`, and `virtual`. `auto` uses only the trusted
Logitech LCD SDK while LCore owns the display and otherwise attempts direct HID.
Direct HID also refuses the exact Logitech LampArray process, which can
exclusively claim the G13. The SDK is loaded only from the canonical signed
LCore installation and all calls are serialized on one bounded owner thread.
Explicit modes retry their selected transport and never cross-fallback. Exact
owner checks do not blanket-block unrelated G HUB processes.

## Required telemetry sources

- Windows local clock: date/day/time.
- Windows scheduler: per-logical-processor CPU load (CCD aggregation).
- GlobalMemoryStatusEx: memory load.
- NVIDIA NVAPI / AMD ADLX vendor DLLs provide GPU load, VRAM, and temperature;
  NVML can provide safely aligned NVIDIA power. HWiNFO shared memory and
  LibreHardwareMonitor loopback JSON are optional fallbacks only where no safe
  native source exists.
- Official signed bundled PresentMon v2.5.1 console: FPS/frame timing only;
  launched and owned only during active capture, with no service/MSI/API.
- Explicitly profiled SteelSeries Arctis/GameBuds wireless USB receiver
  families: optional battery, connection, and model-dependent charging state.
- Discord local RPC: implemented and software-tested active-speaker overlay with RPC OAuth and
  active-account-bound, per-Discord-user DPAPI credential storage; authorization
  uses no redirect URI or callback listener. Each user's application supplies a
  temporarily exposed client secret for the generic OAuth exchange; Social SDK
  Public Client authorization is not implemented.

PresentMon is implemented, software-tested, pinned, and remains enabled by
default because it is a required feature. Live capture against an actively
presenting game is not yet release-qualified and is deferred while this PC's
memory is occupied by the local LLM; release remains pending that gate. Discord
is also implemented and software-tested, but live voice/OAuth qualification is
pending unless explicitly deferred. Each user creates and registers their own
Discord application, and tokens remain DPAPI-protected local data.

## Operational requirements

- Standard user; medium integrity; no elevation.
- Local-first and offline except bounded Discord token exchange/refresh and
  explicitly enabled operator probes.
- No arbitrary scripts/plugins; no kernel drivers; no services; no listeners.
- Provider failures isolated; explicit stale/unavailable states.
- Config hot reload preserves last valid state.
- Installed configuration defaults to
  `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; portable/development
  configuration is adjacent to the executable. Explicit `--config` takes
  precedence. A validated installer template seeds the installed user file only
  when it is first missing and never replaces it on update.
- Reconnect after device loss with bounded, capped backoff.
- No unchanged-frame submission.
- Bounded histories/logs/protocol inputs.
- A local-only diagnostics command produces an atomic, redacted ZIP capped at
  1 MiB from typed offline facts; it excludes raw logs/configuration and private
  identifiers/content and starts no hardware, provider, network, or action path.
- One native LCDSirPlus application executable plus the bundled PresentMon
  console helper; no framework/runtime installation.
- No raw MSR, SMBus, EC, or Super-I/O probing. Optional HWiNFO/LHM processes
  remain user-managed and are never bundled, started, or configured.
  Missing LHM is an acceptable unavailable state; exact LHM SensorIds are copied
  from its loopback `data.json`, not from diagnostics.
- One normal/direct-HID runtime per Windows session; read-only and virtual test
  commands remain available alongside it.
- Optional network-quality probes are disabled by default, bounded to one
  configured IP-literal endpoint and one overall timeout, and always disabled
  in safe mode.
- Portable startup registration never replaces or removes a foreign current-user
  Run value and is never mutated in safe mode. Installed startup instead uses a
  stable SID-specific, per-installing-user ONLOGON task with interactive-token
  and least-privilege settings; every mutation authenticates the protected
  installer owner file and all ownership-critical definition fields.
  Create/update restores the exact prior definition on registration or
  verification failure.
- Standard Inno setup supports current-user and elevated all-users scopes and
  refuses the same SID's opposite-scope or legacy PowerShell installs. Another
  SID's all-users registration may coexist with a current-user install; setup
  does not enumerate offline user hives. Rerun/Modify uses the cached exact setup
  to restore owned files while preserving LocalAppData. Uninstall validates the
  task without mutation during initialization, revalidates/deletes it when
  uninstall commits immediately before files, removes shortcuts and setup cache,
  and preserves configuration, logs, and credentials. Cancellation leaves the
  task intact.

## Alerts and preview

Current CPU/GPU temperature, memory/VRAM load, and headset battery readings
produce deterministic configured alert episodes. Temperature and memory
categories are independently hot-reloaded and enabled by default. Disabling a
category prevents its episodes; temperature disable also prevents pane
flashing. It does not affect memory or headset alerts, and memory disable does
not affect temperature or headset alerts. Only current readings trigger, with
RAM/VRAM thresholds inclusive at `>=`; stale and unavailable readings do not.
Memory thresholds default to 100%, while `0` remains a compatibility disable
value. Existing explicit installed thresholds are preserved on update.
Existing episodes survive unknown or stale readings. A first current valid
recovery removes severity-2 warnings immediately and starts
`critical_alert_linger_ms` only for severity-3 episodes. Disabling a warning
category clears its episodes; disabling the headset provider clears its episode.

With temperature warnings enabled, compatibility key `warning=1` makes
selected temperature panes at their shared graph/warning threshold invert on
alternating 100 ms phases and suppresses CPU/GPU full-screen overlays;
`warning=0` retains those overlays. Hung-target interaction has highest display
priority, followed by remaining unacknowledged overlays, Discord speakers, and
the dashboard. Physical button 4 acknowledges the highest episode; an episode
rearms only after clearing and recurring, and critical linger behavior is
as described above.

Preview mode `auto` hides the preview when either HID or SDK is connected as a
physical backend, `always` starts visible unless minimized, and `never` starts
hidden. Backend reconnect state changes reuse the existing preview window
rather than creating another UI owner and do not override a manual tray toggle.

## Hung-window detector

The query-only detector considers visible, titled top-level windows outside the
Windows directory and configured ignore list. A target appears only after its
exact HWND, PID, creation time, and normalized image path fail consecutive
bounded `WM_NULL` probes for the configured minimum duration. Responsiveness,
absence, identity replacement, safe mode, or disabling the detector removes it
immediately. A detector publication is display input only and is never sufficient
authorization for the guarded action.

The guarded action binds the exact selected target only at physical button-down
on the slot whose currently selected token is `PROC_HANG`; the legacy
`hang_button` value does not choose the runtime button. `PROC_HANG` is globally
unique, and no-target display fallback does not change its selection.
The same input source must remain continuously held. Reaching the configured
duration requests termination automatically, and release afterward only resets
the hold. Recovery, disappearance, identity or selection change, device loss,
provider failure, safe mode, disable, or any hang-policy reload irreversibly
cancels that press. The action boundary independently repeats native eligibility,
timeout, exclusion, and process identity checks before using narrowly scoped
terminate/query/synchronize rights. A loop delayed beyond the maximum press
duration refuses the hold as stale instead of firing late.
It never elevates or retries with broader access.

`hang_enabled` defaults to `0`. `PROC_HANG` remains in its shipped slot and its
button assignment is unchanged until the user explicitly opts in. The provider
reports disabled/unavailable while the no-target pane falls through without
changing selection. The destructive action remains unqualified until the
disposable-child physical-button acceptance gate passes; software tests alone
do not establish release acceptance.

## Non-goals

- G19 color LCD.
- Steam/NVIDIA overlay hooking; kernel-driver sensor access.
- Email integration; remote telemetry/control; arbitrary third-party
  modules; layout editor; Process Lasso integration.
