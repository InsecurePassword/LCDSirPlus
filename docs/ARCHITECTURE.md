# LCDForge 0.3.0 architecture

## Layout

```text
src\
├── main.rs             CLI parsing, command dispatch
├── app.rs              orchestration loop, hot reload, wiring
├── config.rs           v2 schema, defaults, validation
├── diagnostics.rs      offline typed report + bounded store-only ZIP
├── parser.rs           tokenizer + include graph + line diagnostics
├── model.rs            canonical metric/reading/snapshot model
├── history.rs          ring buffer + frame statistics (Phase 2 consumers)
├── render\
│   ├── mod.rs          160x43 Frame (set/get/rect/hash/png)
│   ├── font.rs         3x5 bitmap font (glyph-exact Go port)
│   └── renderer.rs     dashboard, slots, overlays, golden tests
├── backends\
│   ├── mod.rs          worker thread: selection, reconnect, suppression
│   ├── g13.rs          pure G13 report contract (pack/parse) + tests
│   └── hid.rs          Windows HID enumeration + overlapped I/O
├── providers\
│   ├── clock.rs        Win32 NLS date/time formatting
│   ├── cpu.rs          NtQuerySystemInformation per-LP deltas
│   ├── discord.rs      verified desktop RPC, voice tracker, OAuth + DPAPI
│   ├── memory.rs       GlobalMemoryStatusEx
│   ├── ccd.rs          L3/NUMA topology + CPUID cache-size labeling
│   ├── gpu.rs          trusted NVAPI/ADLX loading + canonical GPU metrics
│   ├── hang.rs         bounded WM_NULL probes + exact process identity tracker
│   ├── lhm.rs          optional loopback temperatures-only fallback
│   ├── presentmon.rs   owned console capture + CSV frame statistics
│   └── network.rs      bounded optional ICMP/TCP quality probe
├── runtime.rs          named-mutex ownership + owned HKCU startup value
├── alerts.rs           configured alert episodes + acknowledgement
├── telemetry.rs        provider workers, cadence, stale state, health
├── hardware_test.rs    STEP 01..10 sequence, transports, button capture
├── logging.rs          leveled log + size-capped rotation
├── ui.rs               preview window (GDI) + tray icon
├── slots.rs            four cycling slot windows
├── input.rs            button events + edge debounce
├── sha256.rs           dependency-free SHA-256 (NIST-vector tested)
└── png.rs              deterministic stored-deflate PNG writer
```

## Threading model

| Thread | Owns | Communicates via |
|---|---|---|
| main/app | config, slots, providers, renderer, loop timing | channels |
| lcdforge-backend | HID handles, all device I/O | `Command` in / `Message` out |
| lcdforge-ui | HWND, tray, GDI | frames in / `UiEvent` out |
| frame pump | latest-frame handoff to UI | `PostMessageW` wake |
| telemetry | fast native polls, stale state, provider health | snapshot channel |
| telemetry-gpu | read-only NVAPI/ADLX calls | event channel |
| telemetry-presentmon | target/capture lifecycle | update channel |
| telemetry-hang | visible-window probes and continuous-failure tracker | update channel |
| telemetry-network-quality | optional bounded IP-literal ICMP/TCP probe | update channel |
| lcdforge-discord | verified pipe, authentication, subscriptions, reconnect | snapshot channel |
| telemetry-lhm/headset | bounded blocking HTTP/HID | event channel |

The backend thread is the only toucher of the device (mirrors the Go
`LockOSThread` discipline). Frames are submitted only when changed; button
edges are debounced in the backend thread and delivered as events.

Normal runtime and direct-HID hardware tests acquire the per-session
`Local\\LCDForge2.Runtime` mutex before opening a backend/device. Read-only CLI
commands and virtual tests do not acquire it. The RAII owner closes the handle
on return. Startup registration uses a transaction and mutates only an exact
owned current-user Run value.

## Data flow (one tick)

```text
clock + CPU/memory + NVAPI/ADLX + optional LHM + PresentMon
  → Snapshot { date/time, cpu_dual, cache/freq/total load, mem, readings }
  → Renderer::render(snapshot, overlay opts, view{slot modules})
  → Frame (160x43 bytes)
  ├── backend.submit → transform (orientation/invert) → pack_report
  │     → WriteFile 992 bytes (skipped if unchanged)
  └── ui.show_frame → BGRA → SetDIBitsToDevice
```

Backend button reports flow back: `parse_input` → `ButtonTracker.observe`
(debounce) → app handles the matching action. Button 3 remains exclusive to a
bound hung-target hold; button 4 acknowledges the highest active alert before
cycling its slot. Render priority is hung hold, unacknowledged critical alert,
Discord speaker overlay, then dashboard.

## Determinism

- The renderer is a pure function of (Snapshot, OverlayOptions, View) plus
  graph history for FRAME_TIME/NET slots; golden hashes pin the fixed
  dashboard byte-for-byte against the Go 0.2.0 renderer.
- SHA-256 and PNG encoders are dependency-free and deterministic; evidence
  artifacts are byte-stable across machines.

## Error handling

- Config: file/line diagnostics; last valid config stays active; include
  cycles, traversal, and oversize inputs rejected.
- Backend: every failure carries an exact reason string into
  `BackendState::Disconnected`; reconnect backoff doubles from
  `logitech_reconnect_ms` to `logitech_reconnect_max_ms`; device loss emits
  canceled button releases so no press is ever stuck.
- Providers: absent data renders explicit `N/A`/`STALE` states — the fixed
  bars read only canonical readings, never legacy projections.
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
- Hung action: physical button 3 binds the exact currently selected confirmed
  target at button-down. A single-source monotonic state machine cancels on
  release loss, recovery, selection/identity/provider/policy change, safe mode,
  disable, or reload. Reaching full progress never acts; only the matching
  release can enter the native boundary. That boundary repeats visible titled
  top-level HWND/PID/creation/path/exclusion and `ERROR_TIMEOUT` checks, opens
  only `PROCESS_TERMINATE`, `PROCESS_SYNCHRONIZE`, and
  `PROCESS_QUERY_LIMITED_INFORMATION`, rechecks identity through that handle,
  terminates once, and waits at most five seconds. Audits contain only PID,
  sanitized basename, and a fixed outcome label.
  Safe mode creates only unbound button presses, so releases retain normal slot
  behavior; the app and native action boundaries independently refuse any
  terminate command while safe mode is active.
- Vendor DLLs are loaded by name only from System32. GPU APIs are read-only,
  versioned, and bounded to vendor maximums. Automatically discovered
  PresentMon is canonically contained beside LCDForge; arguments are passed
  without a shell, and only the child started by LCDForge is terminated. Its
  owner thread is joined during shutdown.

## Security posture

- Standard user; no elevation, services, drivers, listeners, or injection.
- HID opens are read/write shared on the vendor collection only; no
  unrelated interfaces are written (enumeration rejects non-matching
  identities with recorded reasons).
- Discord scans only `discord-ipc-0` through `-9` and accepts a server only
  after same-session/current-user checks plus canonical local executable,
  recognized Discord image, valid Authenticode, and `Discord Inc.` publisher
  verification. Verification is bounded and fails closed before client ID or
  token disclosure.
- Discord RPC frames are bounded to 4 MiB. Remote messages and OAuth response
  bodies are never included in errors. Token HTTPS responses are bounded to
  1 MiB and credentials to 64 KiB.
- Discord credentials are versioned and current-user DPAPI protected at rest.
  Optional client secrets cross only the temporary environment boundary.
- Authorization is local RPC plus outbound WinHTTP token exchange. There is no
  callback listener, browser launch, bot/Gateway connection, or user token.
- Diagnostics are a separate offline privacy boundary: a closed typed allowlist
  is written atomically as a store-only ZIP capped at 1 MiB. Raw logs/config,
  credentials, Discord content/identifiers, titles, paths, addresses/targets,
  environment/command-line/registry values, serials, listings, and device paths
  are never archive inputs. Collection starts no HID, providers, probes, RPC,
  actions, or configuration reads. Output rejects reparse directories and
  linked files; native no-replace publication verifies the final volume, file
  identity, link count, and size before releasing the owned handle.
