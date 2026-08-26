# Fixed 160×43 layout reference

## Logical geometry

```text
x: 0                                                        159
   ┌──────────────────────────────────────────────────────────┐ y=0
   │ Windows-local date/day                  Windows-local time│
   ├─────────────────────────────┬────────────────────────────┤ y=7
   │ CPU: Cache CCD micro-bar     │ GPU load bar               │
   │      Frequency CCD micro-bar │                            │
   │ MEM load bar                 │ VMEM load bar              │
   ├──────────────┬──────────────┬──────────────┬──────────────┤ y=25
   │ slot/button 1│ slot/button 2│ slot/button 3│ slot/button 4│
   └──────────────┴──────────────┴──────────────┴──────────────┘ y=42
```

- Header: rows 0–7.
- Fixed metrics: rows 8–24.
- Lower slots: rows 26–42.
- Center divider: x=79, rows 8–24.
- Slot dividers: x=39, 79, 119, rows 26–42.
- All rendering uses the embedded 3×5 bitmap font and integer coordinates.
- The physical framebuffer is one byte per pixel at the Logitech SDK
  boundary: zero off, 255 on. The G13 HID transport packs it into the
  992-byte report (see README).

### Single-CCD variant

On CPUs with one CCD the left column renders one full-height CPU bar
(identical geometry to the MEM bar) at rows 9–15 instead of the stacked
`C`/`F` micro-bars. The mode is chosen by detected topology, not config.

## Golden frames

Rendered with: date `2026-08-08 Saturday`, time `10:44:14`, Cache CCD 22%,
Frequency CCD 47%, MEM 63%, GPU 91%, VMEM 72%, headset 75% (raw level 3),
FPS 144, 1% low 118, GPU temp 74 °C; slots
`HEADSET_BATTERY FPS_CURRENT GPU_TEMP FPS_1LOW`.

| Variant | Logical pixel-buffer SHA-256 |
|---|---|
| Normal (dual-CCD) | `3cee8a2dae57386c1d33b6039d28c79046e569f4c87f41692493dfae345e7525` |
| Bars unavailable | `79a153e1a0fba0904b8b9b75cf9daf6a2eaff15eb838fc178e5e68220928373a` |
| Bars stale | `c1a643090e938bd807a8e9b5c43791c6ae0efe20093e7c27762e1ee6d378ab6d` |

These hashes are release invariants inherited from the Go 0.2.0 renderer. A
change to any of them is a fixed-layout change and requires explicit visual
review and an updated reference sample.

## Bar state grammar

- Valid value: solid fill from the left, `(interior_width × percent) / 100`.
- Unavailable: checkerboard placeholder `(x+y) % 2 == 0`.
- Stale: vertical stripe placeholder `x % 4 < 2`.
- Valid zero renders an empty bar — distinct from both placeholders.
