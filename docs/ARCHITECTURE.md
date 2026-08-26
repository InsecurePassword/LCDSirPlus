# LCDForge 0.3.0 architecture

## Layout

```text
src\
├── main.rs             CLI parsing, command dispatch
├── app.rs              orchestration loop, hot reload, wiring
├── config.rs           v2 schema, defaults, validation
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
│   ├── memory.rs       GlobalMemoryStatusEx
│   └── ccd.rs          L3/NUMA topology + CPUID cache-size labeling
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

The backend thread is the only toucher of the device (mirrors the Go
`LockOSThread` discipline). Frames are submitted only when changed; button
edges are debounced in the backend thread and delivered as events.

## Data flow (one tick)

```text
clock + NtQuerySystemInformation + GlobalMemoryStatusEx
  → Snapshot { date/time, cpu_dual, cache/freq/total load, mem, readings }
  → Renderer::render(snapshot, overlay opts, view{slot modules})
  → Frame (160x43 bytes)
  ├── backend.submit → transform (orientation/invert) → pack_report
  │     → WriteFile 992 bytes (skipped if unchanged)
  └── ui.show_frame → BGRA → SetDIBitsToDevice
```

Backend button reports flow back: `parse_input` → `ButtonTracker.observe`
(debounce) → app cycles the matching slot.

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

## Security posture

- Standard user; no elevation, services, drivers, listeners, or injection.
- HID opens are read/write shared on the vendor collection only; no
  unrelated interfaces are written (enumeration rejects non-matching
  identities with recorded reasons).
- No secrets at rest in 0.3.0 (Discord DPAPI flow arrives in Phase 3).
- Diagnostics redaction expands in Phase 4; current logs contain no user
  data beyond window/metric values.
