# LCDSirPlus 0.3.0 architecture

## Layout

```text
src\
├── main.rs             CLI parsing, command dispatch
├── app.rs              orchestration loop, hot reload, wiring
├── config.rs           v2 schema, defaults, validation
├── diagnostics.rs      offline typed report + bounded store-only ZIP
├── parser.rs           tokenizer + include graph + line diagnostics
├── model.rs            canonical metric/reading/snapshot model
├── history.rs          ring buffer + frame statistics
├── render\
│   ├── mod.rs          160x43 Frame (set/get/rect/hash)
│   ├── font.rs         3x5 bitmap font (glyph-exact Go port)
│   └── renderer.rs     three built-in displays, slots, overlays, golden tests
├── backends\
│   ├── mod.rs          worker thread: selection, reconnect, suppression
│   ├── g13.rs          pure G13 report contract (pack/parse) + tests
│   └── hid.rs          Windows HID enumeration + overlapped I/O
├── providers\
│   ├── clock.rs        Win32 NLS date/time formatting
│   ├── cpu.rs          NtQuerySystemInformation per-LP deltas
│   ├── discord.rs      trust-checked desktop RPC, voice tracker, OAuth + DPAPI
│   ├── memory.rs       GlobalMemoryStatusEx
│   ├── ccd.rs          L3/NUMA topology + CPUID cache-size labeling
│   ├── gpu.rs          trusted NVAPI/NVML/ADLX + canonical GPU metrics
│   ├── hwinfo.rs       read-only exact-label HWiNFO shared-memory fallback
│   ├── hang.rs         bounded WM_NULL probes + exact process identity tracker
│   ├── lhm.rs          optional loopback temperature/exact-sensor fallback
│   ├── presentmon.rs   owned console capture + CSV frame statistics
│   ├── performance.rs  native PDH disk/page-read counters
│   ├── connections.rs  native established IPv4/IPv6 TCP count
│   ├── system_power.rs native AC/battery state
│   └── network.rs      bounded optional ICMP/TCP quality probe
├── runtime.rs          named-mutex ownership + owned HKCU startup value
├── alerts.rs           configured alert episodes + acknowledgement
├── telemetry.rs        provider workers, cadence, stale state, health
├── hardware_test.rs    STEP 01..10 sequence, transports, button capture
├── logging.rs          leveled log + size-capped rotation
├── ui.rs               preview window (GDI) + tray icon
├── slots.rs            four cycling slot windows
├── input.rs            button events + edge debounce
└── sha256.rs           dependency-free SHA-256 (NIST-vector tested)
```

## Threading model

| Thread | Owns | Communicates via |
|---|---|---|
| main/app | config, slots, providers, renderer, loop timing | channels |
| lcdsirplus-backend | physical arbitration and HID handles | latest frame in / state/buttons out |
| lcdsirplus-sdk-owner | trusted DLL lifetime and all Logitech SDK calls | bounded request/reply |
| lcdsirplus-ui | HWND, tray, GDI | frames in / `UiEvent` out |
| frame pump | latest-frame handoff to UI | `PostMessageW` wake |
| telemetry | fast native polls, stale state, provider health | snapshot channel |
| telemetry-gpu | read-only NVAPI/ADLX calls | event channel |
| telemetry-presentmon | target/capture lifecycle | update channel |
| telemetry-hang | visible-window probes and continuous-failure tracker | update channel |
| telemetry-network-quality | optional bounded IP-literal ICMP/TCP probe | update channel |
| lcdsirplus-discord | verified pipe, authentication, subscriptions, reconnect | snapshot channel |
| telemetry-lhm/hwinfo/headset | bounded blocking HTTP/shared-memory/HID | event channel |

The backend thread is the only toucher of the device (mirrors the Go
`LockOSThread` discipline). Frames are submitted only when changed; button
edges are debounced in the backend thread and delivered as events. The UI
producer atomically replaces one pending frame slot, so slow I/O and reconnect
retain only the newest frame while shutdown uses a separate reliable signal.
Before each physical open, a read-only ToolHelp snapshot arbitrates exact,
case-insensitive ownership. LCore present means SDK only; otherwise direct HID
also refuses Logitech LampArray when that service owns the G13. If a competing
owner appears during HID operation, writes stop and HID closes without a final
blank. SDK calls own their buffers, time out after three seconds, and
permanently open a process-wide physical circuit if a native owner does not
return.

SDK discovery, trust validation, loading, and initialization share that same
three-second supervised owner operation. LCore, `LogitechLcd.dll`, and the DLL
parent chain are opened non-reparse with sharing that denies writes/deletes.
Both files must be regular, fixed-volume, single-linked objects. Validation
requires cached whole-chain Authenticode revocation success, exact signer
organization `Logitech Inc`, an identical SHA-256 signer certificate, exact
company `Logitech Inc.`, product `Logitech Gaming Framework`, full file-version
equality, AMD64 PE, and all six exports. After absolute `LoadLibraryExW`, the
loaded module is reopened and its volume serial, file index, size, and link
count must match the pinned DLL before `LogiLcdInit`; mismatch frees the module
and opens the circuit. Pins remain alive until SDK shutdown and `FreeLibrary`.

The final HID output handle is opened with no sharing, after an LCore precheck,
then LCore is checked again. Every write repeats the check, while idle polling
checks at `logitech_button_poll_ms` (bounded to 10-1000 ms). Windows handle
sharing prevents a later LCore handle from sharing the collection; process
observation itself is not claimed atomic, so the exclusive handle is the
contention barrier during the bounded observation interval.

Normal runtime and direct-HID hardware tests acquire the per-session
`Local\\LCDSirPlus.Runtime` mutex before opening a backend/device. Read-only CLI
commands and virtual tests do not acquire it. The RAII owner closes the handle
on return. Portable startup registration uses a transaction and mutates only an
exact owned current-user Run value; installed mode suppresses that path and uses
the installer's per-user scheduled task.

Configuration resolution gives an explicit `--config` path first precedence.
Without it, portable/development layouts use adjacent `lcdsirplus.txt`, while an
installed layout uses `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`. If that
installed user file is absent, the runtime validates the installer-owned
`lcdsirplus.default.txt` and atomically seeds the user file once without
overwriting a concurrently created or existing file.

## Packaging boundary

`scripts/Build.ps1` accepts only a clean exact Git HEAD, runs the full quality
gate, stages explicit member allowlists, and emits deterministic portable,
source ZIPs and a versioned Inno Setup EXE. Every ZIP has one root and a sorted
hash/size manifest; `LCDSirPlus-0.3.0-SHA256SUMS.txt` covers both ZIPs and the
setup EXE. The source tree is populated from `git archive HEAD`, not the working
directory.

The Inno installer uses auto Program Files constants for current-user/all-users
mode, standard uninstall registration, Restart Manager, and a per-original-user
interactive/limited ONLOGON task with a stable SID-specific name and
version-independent description. The same SID's opposite-scope registration and
legacy-install collisions are rejected before file copy; another SID's all-users
registration may coexist with a current-user install, without offline-HKU
enumeration claims. Standard registration supplies scope and install location;
task create/update/delete authenticates the protected installer owner file plus
exact source, description, action, working-directory, trigger, principal,
logon-type, and run-level fields. Create/update is atomic: the exact prior XML is
restored after registration or post-verification failure, and a failed first
install removes only its newly created verified task. The exact setup cache and
owner file are prepared transactionally before the task commit. Uninstall initialization
only validates; `usUninstall` immediately revalidates/deletes before files, so
cancel leaves the task intact and deletion failure aborts file removal. The exact
running setup is cached under the install root and registered as the same-scope
Modify path. Repair and uninstall leave `%LOCALAPPDATA%\LCDSirPlus` user data
untouched.

PresentMon support is implemented, software-tested, pinned, and enabled by
default as a required feature, but live game capture is not release-qualified.
That gate is deferred while this PC's memory is occupied by the local LLM, so
release remains pending. Discord RPC/OAuth is implemented and software-tested;
each user supplies their own registered application and current-user
DPAPI-protected tokens. Its live voice/OAuth gate is pending unless explicitly
deferred.

## Data flow (one tick)

```text
clock + CPU/memory/PDH/TCP/power + NVAPI/ADLX/NVML
  + optional HWiNFO/LHM + owned bundled PresentMon
  → Snapshot { date/time, cpu_dual, cache/freq/total load, mem, readings }
  → Renderer::render(snapshot, overlay opts{main display, scales}, view{slot modules})
  → Frame (160x43 bytes)
  ├── backend.submit → transform (orientation/invert)
  │     ├── HID pack_report → WriteFile 992 bytes
  │     └── SDK row-major 6880 bytes → set background + update
  └── ui.show_frame → BGRA → SetDIBitsToDevice
```

Backend button reports flow back: `parse_input` → `ButtonTracker.observe`
(debounce) → app handles the matching action. A selected `PROC_HANG` token owns
its slot's matching button and exact bound target; inactive `PROC_HANG` and
current `BOTTLENECK NONE` dynamically render the next active token without
changing selection. Button 4 acknowledges the highest active alert before
cycling its slot. Render priority is hung hold, remaining unacknowledged alert
overlays, Discord speaker overlay, then dashboard. Hot-reloaded
`temperature_warning_enabled` and `memory_warning_enabled`, both default true,
gate CPU/GPU and RAM/VRAM episodes independently before alert state is updated.
Stale or unavailable readings cannot create either category and retain any
existing episode until a current valid recovery arrives. That first recovery
removes severity-2 warnings immediately and starts configured linger only for
severity-3 episodes. Disabling a warning category clears its episodes, and
disabling the headset provider clears its episode. With the
temperature category enabled, compatibility `warning` presentation suppresses
full-screen temperature overlays and inverts only the selected qualifying pane
on alternating 100 ms phases when true; when false, temperature overlays remain.
Acknowledgement remains shared alert-state behavior.

## Determinism

- The renderer is a pure function of (Snapshot, OverlayOptions, View) plus
  exact trailing-30-second graph histories. `main_display` selects one of three
  fixed built-in metric regions while all retain the shared header and button
  row; golden hashes pin all three byte-for-byte.
- SHA-256 is dependency-free and deterministic.

## Error handling

- Config: token/parse diagnostics identify the source line; final range and
  cross-field errors may use line 0 when no exact line is retained; last valid
  config stays active; include cycles, traversal, and oversize inputs rejected.
- Backend: every failure carries an exact reason string into
  `BackendState::Disconnected`; reconnect backoff doubles from
  `logitech_reconnect_ms` to `logitech_reconnect_max_ms`; device loss emits
  canceled button releases so no press is ever stuck.
- Providers: absent data renders explicit `N/A`/`STALE` states; built-in display
  bars read only canonical readings, never legacy projections.
- Telemetry source arbitration is native-first: documented Win32/vendor APIs,
  then configured HWiNFO shared memory, then exact/automatic LHM loopback only
  for values with no safe native source. No raw MSR, SMBus, EC, or Super-I/O
  probing exists. Total power is never synthesized; CPU+GPU power is a separate
  complete-current-components subtotal. LibreHardwareMonitor remains
  user-managed and unbundled; absence is an acceptable unavailable state, and
  exact SensorIds come from its loopback `data.json` response rather than
  diagnostics.
- Network quality: disabled and safe-mode policies clear metrics and perform no
  I/O. One configured IP endpoint is probed per bounded interval; one absolute
  deadline covers ICMP, fallback, and TCP connect. Shutdown or policy changes
  prevent fallback/new I/O, and the worker joins within one configured timeout
  plus its 50 ms scheduling tick. Timeout and loss remain distinct from
  provider/API failure, and reload discards old policy history before publish.
- Hung detector: query-only visible top-level-window enumeration captures
  HWND/PID/creation-time/image identity, applies configured and Windows-directory
  exclusions, and counts only documented `WM_NULL` timeouts after revalidating
  the exact identity. Other message errors are indeterminate. Recovery, absence,
  identity or probe-policy change, disable, and safe mode clear targets
  immediately; in-flight results are compared with current policy before publish.
  The normal detector worker has no process-termination capability.
  The hidden bounded smoke mode copies the same executable under a disposable
  non-ignored basename, creates one visible window that intentionally stops
  pumping messages, and detects only that child PID. An isolated cleanup owner
  waits for normal exit or terminates only that disposable child on exceptional
  return, then removes only its temporary tree.
- Hung action: the physical button matching the selected `PROC_HANG` slot binds
  the exact currently selected confirmed target at button-down. A single-source
  monotonic state machine cancels on release loss, recovery,
  selection/identity/provider/policy change, safe mode, disable, or reload.
  Reaching full progress while still held enters the native boundary once;
  release afterward only resets state. A loop delayed beyond the bounded maximum
  refuses the hold as stale. The native boundary repeats visible titled
  top-level HWND/PID/creation/path/exclusion and `ERROR_TIMEOUT` checks, opens
  only `PROCESS_TERMINATE`, `PROCESS_SYNCHRONIZE`, and
  `PROCESS_QUERY_LIMITED_INFORMATION`, rechecks identity through that handle,
  terminates once, and waits at most five seconds. Audits contain only PID,
  sanitized basename, and a fixed outcome label.
  Safe mode creates only unbound button presses, so releases retain normal slot
  behavior; the app and native action boundaries independently refuse any
  terminate command while safe mode is active. `hang_enabled` defaults false;
  the shipped slot/button remains present, the provider reports
  disabled/unavailable, and the no-target pane falls through until explicit
  opt-in. Termination remains unqualified until disposable-child
  physical-button acceptance passes.
- Vendor DLLs are loaded by name only from System32. GPU APIs are read-only,
  versioned, and bounded to vendor maximums. Automatically discovered
  bundled signed PresentMon v2.5.1 is canonically contained beside LCDSirPlus;
  default mode uses one targetless child and bounded one-second PresentMon
  graphics/CPU workload buckets with foreground/recent-activity fallback and
  switch hysteresis. Autonomous selection first applies an optional bounded,
  read-only NVIDIA App private local catalog gate: a valid catalog retains only
  normalized exact full paths carrying all required high-confidence flags;
  malformed or ambiguous records are skipped, at least one fully typed record is
  required, and unavailable/unsupported input retains generic inference. The
  cache holds no raw JSON or descriptive inventory,
  refreshes no faster than every five seconds, retries one metadata race, logs
  only fixed-code/count state transitions, and clears with provider state. No NVIDIA process,
  API/DRS, Xbox source, network, or vendor write is involved. Full statistics
  belong only to the selected PID, creation time, and executable image.
  `presentmon_persist` defaults off; when enabled only in autonomous mode, the
  same admission, purge, and ranking pipeline receives an unavailable catalog
  view and therefore reuses generic R4 inference without bypassing identity,
  exclusion, bounds, expiry, or lifecycle controls. At render time the shared
  slot resolver can defer unavailable PresentMon panels cyclically without
  mutating the slot manager's stored selection. Presenter changes reset
  statistics without restarting the child. Arguments are passed without a shell,
  and only the child/session started for active frame capture is terminated. Its
  owner thread is joined during shutdown.

## Security posture

- Standard user; no services, drivers, listeners, or injection. PresentMon ETW
  access can require Performance Log Users membership or elevation under local
  policy, but LCDSirPlus installs neither and normally runs unelevated.
- HID discovery/detail handles are read/write shared while attributes are
  inspected. The final validated output handle uses `FILE_SHARE_MODE(0)` as an
  exclusive LCore contention barrier; no unrelated interfaces are written
  (enumeration rejects non-matching identities with recorded reasons).
- Discord scans only `discord-ipc-0` through `-9` and accepts a server only
  after same-session/current-user checks plus canonical local executable,
  recognized Discord image, valid Authenticode, and `Discord Inc.` publisher
  verification. Verification is bounded and fails closed before client ID or
  token disclosure.
- Discord RPC frames are bounded to 4 MiB. RPC errors include only the numeric
  code and, for OAuth failures, a fixed local classification; remote messages,
  raw payloads, and OAuth response bodies are never included. Token HTTPS
  responses are bounded to 1 MiB and credentials to 64 KiB.
- Discord credentials are v2, keyed by immutable Discord user ID, and
  current-Windows-user DPAPI protected at rest. The generic OAuth exchange
  requires a client secret before any authorization IPC; it enters only through
  the temporary authorization environment variable and remains encrypted with
  the account record for refresh. Social SDK Public Client authorization is not
  implemented.
- Authorization is local RPC plus outbound WinHTTP token exchange. Both the RPC
  request and authorization-code exchange omit `redirect_uri`; there is no
  callback listener, browser launch, bot/Gateway connection, or user token.
- Diagnostics are a separate offline privacy boundary: a closed typed allowlist
  is written atomically as a store-only ZIP capped at 1 MiB. Raw logs/config,
  credentials, Discord content/identifiers, titles, paths, addresses/targets,
  environment/command-line/registry values, serials, listings, and device paths
  are never archive inputs. Collection starts no HID, providers, probes, RPC,
  actions, or configuration reads. Output rejects reparse directories and
  linked files; native no-replace publication verifies the final volume, file
  identity, link count, and size before releasing the owned handle.
