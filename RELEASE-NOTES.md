# LCDSirPlus 0.3.0 Draft Release Notes (Unreleased)

LCDSirPlus 0.3.0 is an unreleased draft of the Rust port for Windows 11 x64.
There is no completed release claim. Its implemented software
scope includes deterministic rendering, direct G13 HID, native telemetry,
Discord active-speaker integration, alerts, guarded hung-window action,
single-instance/startup ownership, offline diagnostics, and transactional
per-user packaging.

## Highlights

- Added three selectable fixed main-display layouts with a compact shared
  header. `main_display` accepts `1` through `3`, defaults to `1`, and applies
  on valid hot reload; layouts 2 and 3 reuse `network_graph_ceiling_mbps` for
  their current network bars.
- Direct Logitech G13 HID transport without Logitech runtime or elevation.
- Native NVAPI/ADLX GPU metrics, pinned PresentMon frame-capture support, optional
  LHM, headset/XInput/audio/network providers, and deterministic
  stale/unavailable rendering.
- Expanded the slot registry to 53 modules, including trailing
  30-second load/temperature/disk/FPS graphs, board/cooling/power telemetry,
  native disk/connections/system-battery detail, network health, bottleneck,
  and selected-slot hung-window controls.
- Added native-first source arbitration: documented Win32/vendor APIs first,
  configured HWiNFO shared memory next, then exact/automatic LHM loopback only
  where no safe native source exists. No raw MSR/SMBus/EC/Super-I/O probing.
- Added independently hot-reloaded temperature and memory warning categories,
  both enabled by default.
  Temperature presentation remains compatible through `warning`: pane-local
  100 ms flashing when enabled, or full-screen temperature overlays when off.
  RAM/VRAM thresholds default to 100%, trigger inclusively, and retain explicit
  installed values during update. Severity-2 recovery removes immediately;
  severity-3 linger starts on the first current valid recovery. Unknown/stale
  readings retain existing episodes, while disabling the applicable category or
  headset provider clears them.
- Pinned and software-tested Intel's official signed PresentMon v2.5.1 console
  and its MIT/third-party notices for release packaging. Package-level Rust and
  Windows dependency notices are in
  `THIRD_PARTY_LICENSES.txt`. Default targetless capture autonomously selects a
  strong sustained local presenter from bounded PresentMon graphics/CPU workload
  with foreground/activity fallback, without a game-name list or vendor API.
  Targeted expert overrides remain available. LCDSirPlus owns PresentMon only during active frame capture;
  uninstall removes it. No PresentMon service, MSI, GUI, or API is installed.
- Implemented and software-tested Discord Desktop IPC with bounded OAuth,
  DPAPI-local credential storage, cancellation, and redacted errors. Each user
  creates and registers their own Discord application.
- Offline diagnostics ZIP with fixed entries, a 1 MiB cap, manifest hashes,
  no configuration read, and identity-checked no-overwrite publication.
- Standard current-user/all-users Inno Setup EXE with preserved LocalAppData,
  same-scope cached repair, scope/legacy collision refusal, and authenticated
  per-installing-user scheduled-task ownership.

## Requirements and Gaps

- Windows 11 x64 and a standard user account.
- Optional telemetry requires its corresponding vendor driver/hardware.
  HWiNFO and LibreHardwareMonitor remain user-managed and are not bundled;
  HWiNFO64 Free shared-memory monitoring has the vendor's 12-hour limit.
- Discord features require Discord Desktop, a developer application/tester
  setup, user consent, and network access for token exchange/refresh.
- The LCDSirPlus executable and package archives are not code-signed; verify
  supplied SHA-256 manifests. The bundled PresentMon executable has its own
  Intel signature.
- The Discord implementation is software-tested, but live voice/OAuth workflow
  acceptance is pending. Release remains pending this gate unless it is
  explicitly deferred; tokens remain current-user DPAPI-protected local data.
- PresentMon remains enabled by default because it is a required product
  feature. Autonomous mode optionally gates selection through the current
  user's bounded, read-only NVIDIA App local catalog using exact full-path and
  high-confidence flags; unavailable/invalid catalogs retain generic workload
  fallback. NVIDIA App is not required, and no catalog inventory is logged or
  written. Live capture against an actively presenting game is not yet
  release-qualified and is deferred because this PC's memory is occupied by the
  local LLM. Release remains pending that live gate.
- PresentMon panels now default to render-only deferred fallback
  (`presentmon_deferred 1`) without changing button selection; disabling it keeps
  existing `N/A`/`STALE`/`00:00`/`IDLE` text. Optional
  `presentmon_persist 1` allows generic presenters only in autonomous mode while
  retaining identity, exclusions, workload hysteresis, expiry, cleanup, and
  catalog privacy controls; it defaults to `0`.
- Guarded hung-window detection and termination are opt-in (`hang_enabled 0` by
  default). `PROC_HANG` remains selected in the shipped slot while its provider
  reports disabled/unavailable and its no-target pane falls through until
  enabled. The destructive action is unqualified until the
  disposable-child physical-button gate passes; automated tests do not
  establish that evidence.
- Current-session display and installer observations are not final-package
  release evidence. See `docs/HARDWARE-ACCEPTANCE.md` for the dated status and
  required rebinding/reruns.

## Upgrade and Removal

If release artifacts are published, run the versioned setup EXE again to update
or repair owned files. The standard
Windows registration points Modify to an exact cached setup copy with the same
current-user or all-users scope. Setup refuses the same account's opposite-scope
registration, a legacy PowerShell installation in the destination, or a foreign
collision on the stable SID-specific startup task. Another account's all-users
registration may coexist with a current-user install. Uninstall validates task
ownership during initialization, revalidates and removes it only when uninstall
commits, then removes application files. Cancellation leaves the task intact;
Task Scheduler failure preserves retry. Installed configuration at
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`, plus logs and credentials
elsewhere under `%LOCALAPPDATA%\LCDSirPlus`, remain untouched.
