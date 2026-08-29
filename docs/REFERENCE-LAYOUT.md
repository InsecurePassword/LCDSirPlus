# Fixed 160×43 layout reference

## Logical geometry

```text
x: 0                                                        159
   ┌──────────────────────────────────────────────────────────┐ y=0
   │ Windows-local date/day                  Windows-local time│
   ├─────────────────────────────┬────────────────────────────┤ y=7
   │ CPU: Cache CCD micro-bar     │ GPU load bar               │
   │      Frequency CCD micro-bar │                            │
   │ MEM load bar                 │ VRAM load bar              │
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
Frequency CCD 47%, MEM 63%, GPU 91%, VRAM 72%, headset 75% (raw level 3),
FPS 144, 1% low 118, GPU temp 74 °C; slots
`HEADSET_BATTERY FPS_CURRENT GPU_TEMP FPS_1LOW`.

| Variant | Logical pixel-buffer SHA-256 |
|---|---|
| Normal (dual-CCD) | `e44f9a1646d9e091fd1482dc5147f0cd9e639f0dd6b7fbb93edcf5d10da2757b` |
| Bars unavailable | `0561081d2f5ee1930177a40d7f4425ac32d84a53bde0aaf92e49f8e616033576` |
| Bars stale | `bd3e659e30ecb742c0a97aa331c74388608053a6a534496fd8cbfc635fb14cd0` |

These hashes are release invariants inherited from the Go 0.2.0 renderer. A
change to any of them is a fixed-layout change and requires explicit visual
review and an updated reference sample.

## Bar state grammar

- Valid value: solid fill from the left, `(interior_width × percent) / 100`.
- Unavailable: checkerboard placeholder `(x+y) % 2 == 0`.
- Stale: vertical stripe placeholder `x % 4 < 2`.
- Valid zero renders an empty bar — distinct from both placeholders.
