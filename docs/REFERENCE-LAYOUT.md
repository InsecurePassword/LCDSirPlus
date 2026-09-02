# Fixed 160x43 Layout Reference

Audience: renderer maintainers reviewing fixed coordinates and golden frames.
This reference describes current source-renderer geometry. Its hashes are
implementation checks, not final package or physical-release evidence. Use
[PRODUCT-SPEC.md](PRODUCT-SPEC.md) for behavior and
[HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md) for physical review.

## Shared geometry

```text
x: 0                                                        159
   Windows-local date/day                  Windows-local time y=0..4
                                                               y=5 blank
   ---------------------------------------------------------- y=6
   selectable main-display metrics                           y=7..24
   ---------------------------------------------------------- y=25
   slot/button 1 | slot/button 2 | slot/button 3 | slot/button 4
                                                               y=26..42
```

- Date/day starts at x=1 and time is right-aligned to x=158; their glyphs
  occupy rows 0..4. Row 5 is blank.
- The top separator spans x=0..159 at y=6.
- Selectable main-display metrics occupy rows 7..24. Standard bar borders use
  y=8..14 and y=17..23. Row 24 is blank except for vertical dividers.
- The bottom separator spans x=0..159 at y=25. Four button-aligned slots occupy
  rows 26..42, with dividers at x=39, 79, and 119.
- All rendering uses the embedded 3x5 bitmap font and integer coordinates.
- The physical framebuffer is one byte per pixel at the Logitech SDK boundary:
  zero off, 255 on. The G13 HID transport packs it into the 992-byte report.

## Layout 1: classic halves

- Main divider: x=79, y=7..24.
- Dual-CCD CPU: label x=1, y=10; unlabeled Cache bar x=14..77, y=8..10;
  unlabeled Frequency bar x=14..77, y=12..14.
- Single-CCD CPU: label x=1, y=9; bar x=14..75, y=8..14.
- RAM: label x=1, y=18; bar x=14..75, y=17..23.
- GPU: visible label x=81, y=9; bar x=98..157, y=8..14.
- VRAM: label x=81, y=18; bar x=98..157, y=17..23.

On CPUs with one CCD, the left column renders one full-height CPU bar instead
of the stacked Cache and Frequency bars. Dual-CCD bars retain one shared CPU
label and display no per-bar glyphs. Detected topology selects the mode.

## Layout 2: thirds

- Dividers: x=52 and x=105, y=7..24.
- Dual-CCD CPU: label x=1, y=10; unlabeled Cache bar x=14..51, y=8..10;
  unlabeled Frequency bar x=14..51, y=12..14.
- Single-CCD CPU: label x=1, y=9; bar x=14..51, y=8..14.
- RAM: label x=1, y=18; bar x=14..51, y=17..23.
- GPU: visible label x=54, y=9; bar x=71..104, y=8..14.
- VRAM: label x=54, y=18; bar x=71..104, y=17..23.
- OUT: label x=107, y=9; bar x=120..157, y=8..14.
- IN: label x=107, y=18; bar x=120..157, y=17..23.

## Layout 3: system and network halves

- Main divider: x=79, y=7..24.
- The left CPU/RAM geometry is identical to Layout 1.
- NET IN: label x=81, y=9; bar x=110..157, y=8..14.
- NET OUT: label x=81, y=18; bar x=110..157, y=17..23.
- Layout 3 does not render GPU or VRAM in the main metrics region.

## Network scale

Layouts 2 and 3 network bars show current direction throughput. Inbound/IN uses
`network_graph_ceiling_download_mbps`; outbound/OUT uses
`network_graph_ceiling_upload_mbps`. The same mapping applies to
`NET_IN_GRAPH`/`NET_OUT_GRAPH` and the top/bottom halves of `NET_GRAPH`. Each
direction scales independently. Both defaults are 1000 Mbps, so 125,000,000
bytes/s (1 Gbps) is full width or height; higher values clip at full scale. A
1000-down/40-up plan sets the download ceiling to `1000` and upload ceiling to
`40`; these plan rates are not auto-detected.

## Golden frames

The renderer's canonical sample and state fixtures are pinned to these hashes:

| Variant | Logical pixel-buffer SHA-256 |
|---|---|
| Layout 1, normal dual-CCD | `a2f36db6c54c09adcd459756677a6fc348f181d01e0810a9c7e4d9c3c998c20c` |
| Layout 1, bars unavailable | `387f5269dded63f8c38cd965b55f02fbb1e2552e89c90e5353c34ec3e1058aa9` |
| Layout 1, bars stale | `f7bc46cc0f4763084dabf1445a2240e08ec892ed2c3c49991e9428296d69bdb8` |
| Layout 2 | `c6bf7ef1e919ef47dfc7ed13ef9a9f937253e47a449fe8558f00adf98a881701` |
| Layout 3 | `ef1dbde8de40632b213b7c149789a716165719b0ab4f40ab80aa508930b6ff8b` |

Tests in `src/render/renderer.rs` enforce these hashes. A changed hash is a
built-in-layout change and requires visual review plus an updated reference.

## Bar state grammar

- Valid value: solid fill from the left, `(interior_width x percent) / 100`.
- Unavailable: checkerboard placeholder `(x+y) % 2 == 0`.
- Stale: vertical stripe placeholder `x % 4 < 2`.
- Valid zero renders an empty bar, distinct from both placeholders.
