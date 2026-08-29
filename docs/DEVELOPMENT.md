# LCDSirPlus development

This source-only document covers building, testing, packaging, architecture,
and maintenance. End-user instructions belong in `README.md`, `modules.md`,
`docs/INSTRUCTION-MANUAL.md`, and `docs/CONFIGURATION.md`.

## Project overview

LCDSirPlus is a Windows 11 x64 Rust application. `src/main.rs` owns command-line
dispatch, `src/app.rs` wires configuration, telemetry, rendering, input, UI,
and backends, and `src/render/renderer.rs` produces the fixed 160x43 frame.
Provider, backend, and package details are documented in
`docs/ARCHITECTURE.md`.

The build requires stable Rust with the MSVC target and the corresponding
Microsoft C++ build tools. Direct Cargo dependencies are Microsoft's `windows`
crate and the `winres` build dependency.

## Build and quality gates

Run commands from the repository root:

```powershell
cargo build
cargo build --release
cargo fmt --check
cargo clippy --all-targets
cargo test --release
pwsh -NoProfile -File .\scripts\Test.ps1
```

`scripts/Test.ps1` parses the packaging scripts with supported PowerShell
versions, checks product identity and formatting, enforces zero Clippy warnings,
runs the deterministic documentation gate and release tests, builds the release
executable, and exercises public smoke commands plus the virtual hardware test.

`build.rs` copies the canonical root `lcdsirplus.txt` beside each Cargo debug or
release executable; local overlay include files are deliberately not copied.

The release binary uses `IMAGE_SUBSYSTEM_WINDOWS_GUI`. One-shot command modes
attach to an existing parent console without allocating one, while tray/runtime
launches remain console-free even when options such as `--safe-mode` are used.
The quality gate inspects the PE header, captures command output and exits
through waited PowerShell pipelines, and launches exact-PID lifecycle/error-
dialog smokes with guaranteed cleanup.

## Packaging

```powershell
pwsh -NoProfile -File .\scripts\Build.ps1
```

Release packaging requires a clean worktree and an unchanged exact Git HEAD.
The quality gate runs first. Source files are staged from `git archive HEAD`,
not from the working directory. Explicit allowlists produce deterministic
portable, installer, and source ZIPs with sorted hash/size manifests and an
archive checksum file. Package lifecycle tests use clean extraction and verify
install, update, rollback, ownership, and uninstall behavior.

Portable and installer packages contain only public user documentation:
`README.md`, `modules.md`, `LICENSE`, `RELEASE-NOTES.md`, `SECURITY.md`,
`docs/CONFIGURATION.md`, `docs/HARDWARE-ACCEPTANCE.md`,
`docs/INSTRUCTION-MANUAL.md`, and
`docs/LCDSirPlus-Instruction-Manual.pdf`, plus the signed pinned
`PresentMon.exe` and `licenses/PresentMon/LICENSE.txt`/`THIRD_PARTY.txt`.
Architecture, product-specification, reference-layout, and development documents
remain source-archive-only.

### Pinned PresentMon acquisition

PresentMon is a release input, not a Cargo dependency. Run:

```powershell
pwsh -NoProfile -File .\scripts\Acquire-PresentMon.ps1
pwsh -NoProfile -File .\scripts\Acquire-PresentMon.ps1 -VerifyOnly
```

The acquisition script accepts only the official PresentMon v2.5.1 x64 console
artifact pinned in `third_party/PresentMon/manifest.json`: exact URL, 956768-byte
size, SHA-256 `9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191`,
valid Intel Corporation Authenticode signer, and required console flags. It also
pins the upstream commit and exact hashes/sizes of `LICENSE.txt` and
`THIRD_PARTY.txt`, verifies downloads before atomic publication, and never
acquires a service, MSI, GUI, or API package.

Offline release builds must pre-populate `third_party/PresentMon/PresentMon.exe`,
`LICENSE.txt`, and `THIRD_PARTY.txt` with those exact pinned files. Run
`Acquire-PresentMon.ps1 -VerifyOnly` before disconnecting; `Build.ps1` performs
the same verification and does not download. Packaging copies the console and
notices into portable/installer payloads, and package lifecycle tests verify
identity, signature, installation, and removal.

PresentMon is MIT-licensed by Intel. HWiNFO is separately licensed under its
vendor terms; LibreHardwareMonitor is MPL-2.0. ADLX and NVAPI/NVML are
vendor-installed APIs used under vendor terms. None is a Rust dependency, and
no sensor/GPU SDK is bundled. These projects and vendors are not affiliated
with or endorsers of LCDSirPlus.

## G13 backend proven contract

- SetupAPI accepts exactly VID `046D`, PID `C21C`, usage page `FF00`, usage
  `0000`, with 8-byte input and 992-byte output reports.
- Output uses report ID `0x03`, a 32-byte header, and a 960-byte payload:
  `report[32 + x + (y/8)*160] |= 1 << (y & 7)`.
- Input uses report ID `0x01`; LCD buttons are byte 6 bits
  `0x02 << 0..3`.
- Direct HID uses overlapped I/O, a one-second operation timeout, bounded
  reconnect backoff, and blank-on-close. It refuses to open while exact process
  name `LCore.exe` is running and does not blanket-block G HUB services.
- `auto` uses the trusted Logitech LCD SDK while LCore is present and direct HID
  while it is absent. SDK loading is absolute-path-only from the running signed
  LCore installation after pinned-object Program Files, reparse/ACL/hard-link,
  AMD64 PE/export, cached-revocation, exact signer-certificate, product, and
  full-version validation. The loaded module identity must match the pinned DLL
  before any SDK call, and all SDK calls run on one bounded owner thread.
- Direct HID takes a non-shared output handle, rechecks LCore after opening and
  before every write, and polls ownership while idle. LCore detection closes HID
  without a final blank.
- Static frames are submitted only when changed. Hardware-test animation is
  capped at 10 updates per second while button polling remains independent.

See `src/backends/g13.rs`, `src/backends/hid.rs`, `src/backends/sdk.rs`, and
`docs/ARCHITECTURE.md` for the implementation and security boundaries.

## SteelSeries battery profile contract

`src/providers/headset.rs` admits SteelSeries VID `1038` only by exact PID,
interface number, HID usage selector, and minimum input/output report length.
Unknown and name-only matches fail closed. PID `2212`, the known-working Arctis
7P+ baseline, receives the only positive candidate priority; ties are stable by
lowercase device path. The table contains 32 unique PIDs across eight protocols:

| Internal profile | Exact PIDs | Interface and usage | Result semantics |
|---|---|---|---|
| `Legacy12` | `12B3`, `12B6`, `12D7`, `12D5` | 3, exact `FF43:0202` | direct percentage, offline, no charge flag |
| `Arctis9` | `12C2` | 0, exact `FFC0:0001` | `64..9A` scaled to percentage, charge flag |
| `ProWireless` | `1290` | 0, exact `FF00:0001` | two-step query, 25% bands, offline, no charge flag |
| `SevenPlus` | `220E`, `2212`, `2216`, `2236` | 3, exact `FFC0:0001` | direct or 25% bands, offline, charge flag |
| `NovaDiscrete` | `2202`, `2206`, `220A`, `223A`, `227A`, `22A4`, `22AB` | 3, exact `FFC0:0001` | 25% bands, offline, charge flag |
| `NovaDirect` | `22A1`, `227E`, `2258`, `229E`, `22A9`, `22A5`, `22A7`, `2298`, `22AD` | 3, exact `FFC0:0001` | direct percentage, offline, charge flag |
| `Nova5` | `2232`, `2253`, `2264`, `2269`, `226D` | 3, exact `FFC0:0001` | direct percentage at Nova 5 offset, offline, charge flag |
| `GameBuds` | `230A` | 3, exact `FFC0:0001` | minimum active earbud percentage, no charge flag |

The model-family evidence supports Arctis 1/7X/7P, 9, Pro Wireless, 7+/7P+,
Nova 7/7X/7P variants, Nova 5/5X, Nova 3P/3X Wireless, and GameBuds. PIDs
without a proven one-to-one retail alias remain documented as receiver/firmware
variants. Legacy Arctis 7/7 (2019) PIDs `1260` and `12AD` and Nova Pro Wireless
PIDs `12E0`, `12E5`, and `225D` are rejected because their exact command-bearing
usage was not evidenced. Other known rejected PIDs are `1252`, `1280`, `12EC`,
`220C`, `2200`, `2204`, `2208`, `2267`, `230C`, `231A`, `1292`, and `1297`;
every other unlisted PID is also rejected.

GG is not queried and is not required. Handles use shared read/write access so
GG may coexist, subject to normal USB/HID driver behavior. A single absolute
`headset_query_timeout_ms` deadline covers ranked candidates and each profile's
bounded write/read sequence.

### Clean-room protocol-fact research

Device identifiers, control-collection facts, request/response layouts, and
model-family aliases were independently cross-checked against publicly
documented behavior in:

- [HeadsetControl](https://github.com/Sapd/HeadsetControl) (GPL-3.0);
- [Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager)
  (GPL-3.0);
- [Arctis Nova 3X Battery Tray](https://github.com/pokjump/arctisnova3xbatterytray)
  (MIT).

These are research credits, not dependencies. LCDSirPlus contains an independent
Rust implementation; no source code from those projects is included or
redistributed, and their licenses do not change LCDSirPlus's MIT license.

## Documentation map

- `docs/ARCHITECTURE.md`: subsystem layout, threading, data flow, boundaries.
- `docs/PRODUCT-SPEC.md`: product behavior and operational requirements.
- `docs/REFERENCE-LAYOUT.md`: fixed geometry and renderer golden hashes.
- `docs/HARDWARE-ACCEPTANCE.md`: physical G13 and live integration checks.
- `docs/CONFIGURATION.md`: complete configuration schema and ranges.
- `docs/INSTRUCTION-MANUAL.md`: detailed user procedures and troubleshooting.
- `modules.md`: package-ready button-slot quick reference in canonical registry order.
- `SECURITY.md`: supported security and disclosure expectations.

For customization, start with `src/config.rs`, `src/parser.rs`, and the
configuration reference. For display changes, use the reference layout and
renderer goldens. For provider, backend, hardware, Discord, install, or runtime
problems, use the instruction manual first, then the architecture and acceptance
documents for subsystem-specific checks.

## Deterministic artifacts

The renderer's fixed dashboard is pinned to the hashes in
`docs/REFERENCE-LAYOUT.md`. Intentional layout changes require updated goldens
and visual review.

The documentation gate verifies that `modules.md` contains exactly the 53
canonical `src/config.rs` tokens in order and that the README and standalone
manual reference it. The instruction-manual PDF is generated manually rather than during every
build. PowerShell 7 and installed Google Chrome or Microsoft Edge are required:

```powershell
pwsh -NoProfile -File .\scripts\Build-Manual.ps1
```

The script renders `docs/INSTRUCTION-MANUAL.md`, normalizes PDF timestamps,
validates the result, and atomically replaces the tracked PDF. Do not regenerate
it for unrelated documentation changes.

## Icon integration

The tracked root `LCDSirPlus.ico` is required for Windows builds. `winres`
embeds it as executable resource ID 1, and the UI uses that resource for both
the window and tray icons, with the stock Windows icon as a runtime fallback.
Release validation should visually verify the packaged result in Explorer.
