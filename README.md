# LCDForge 0.3.0 (Rust)

LCDForge is a native Windows 11 x64 dashboard for the **Logitech G13 160×43
monochrome LCD** — a faithful port of the LCDForge 0.2.0 Go application with
a smaller footprint and fewer dependencies:

- **~370 KB single static executable** (Go build: multi-MB with runtime).
- **No Logitech runtime**: direct-HID G13 backend (the proven path from the
  0.2.0 repair — no LCore, no G HUB conflict, no administrator rights).
- **No Process Lasso**: Cache/Frequency CCD bars come from native topology
  detection (L3/NUMA domains + CPUID L3-size labeling of the 3D V-Cache die).
- **Native-first telemetry**: CPU load from scheduler accounting deltas,
  memory from `GlobalMemoryStatusEx` — memory reads, not network polling.

## Fixed dashboard

The normal framebuffer is always 160×43 pixels:

```text
┌───────────────────────────────────────┐
│2026-08-08 Saturday          10:44:14 │
├───────────────────┬───────────────────┤
│CPU C████████      │GPU ███████████    │
│    F██████        │                   │
│MEM █████████      │VMEM ███████       │
├─────────┬─────────┬─────────┬─────────┤
│7P+ 75%  │FPS 144  │GPU 74°C │1% 118   │
└─────────┴─────────┴─────────┴─────────┘
```

Dual-CCD CPUs render stacked Cache (`C`) and Frequency (`F`) micro-bars;
single-CCD CPUs render one full-height CPU bar. The deterministic renderer is
hash-verified against the Go 0.2.0 goldens:

| Golden dashboard | SHA-256 |
|---|---|
| Normal | `3cee8a2dae57386c1d33b6039d28c79046e569f4c87f41692493dfae345e7525` |
| All-bars-unavailable | `79a153e1a0fba0904b8b9b75cf9daf6a2eaff15eb838fc178e5e68220928373a` |
| All-bars-stale | `c1a643090e938bd807a8e9b5c43791c6ae0efe20093e7c27762e1ee6d378ab6d` |

## G13 backend (proven contract)

Enumerated strictly via SetupAPI; exactly one candidate accepted:

- VID `046D`, PID `C21C`, usage page `FF00`, usage `0000`
- 8-byte input / 992-byte output reports
- Output: report ID `0x03`, 32-byte header + 960-byte payload,
  `report[32 + x + (y/8)*160] |= 1 << (y & 7)`
- Input: report ID `0x01`; LCD buttons in byte 6, bits `0x02 << 0..3`
- Overlapped I/O, 1 s op timeout, bounded reconnect backoff, blank-on-close

## Command line

```text
lcdforge.exe [--config PATH] [COMMAND]
  (none)             Run the dashboard application (tray + preview)
  --validate-config  Validate the configuration and exit
  --preview          Run with the virtual preview forced on
  --hardware-test    Run the deterministic 10-step G13 test sequence
  --hardware-discover  Passive read-only G13 HID enumeration
  --backend hid|virtual  Backend for --hardware-test (default hid)
  --duration-secs N  Visible duration for --hardware-test (default 30)
  --safe-mode        Providers/destructive actions disabled
  --diagnostic-dir PATH  Log/diagnostic output directory
  --version
```

## Build

```powershell
cargo build --release          # target\release\lcdforge.exe
cargo test                     # full suite incl. golden frames
cargo clippy                   # zero-warning policy
```

Toolchain: stable Rust (MSVC), only dependency is the official `windows`
crate. Copy `lcdforge.txt` next to the executable; live configuration and
logs live under `%LOCALAPPDATA%\LCDForge2\`.

## Verification status

- Automated: 68 tests green (golden frames, parser, tokenizer, include
  graph, slots, debounce, G13 report packing/parsing, IEC formatting,
  hardware-test sequencing, SHA-256/PNG codecs, CCD list handling).
- Live machine: G13 vendor collection enumerated (`046d:c21c`, 8/992) at
  medium integrity; configuration validated; virtual hardware-test sequence
  passes end to end.
- **Pending human acceptance**: physical display of the STEP 01–10 sequence
  on the G13, physical button presses, unplug/replug recovery, and the
  sustained run. Run `--hardware-test --backend hid --duration-secs 60` and
  observe. Software transport results are never physical confirmation.

## Phase roadmap

- **P1 (this release)**: renderer + golden frames, direct-HID G13 backend,
  virtual preview + tray, clock/CPU/CCD/memory providers, config v2 hot
  reload, hardware test.
- **P2**: GPU vendor DLLs (nvapi/ADLX) → LHM temps fallback, network,
  Arctis 7P+ battery, XInput, Core Audio, PresentMon.
- **P3**: Discord active-speaker overlay (IPC + OAuth + DPAPI).
- **P4**: guarded hung-process termination, alerts, diagnostics bundle,
  installer/packaging.

## License

MIT. Logitech, SteelSeries, LibreHardwareMonitor, PresentMon, and Discord
are trademarks of their owners; no third-party binaries are redistributed.
