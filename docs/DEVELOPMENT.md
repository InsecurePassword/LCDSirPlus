# LCDSirPlus development

This source-only document covers building, testing, packaging, architecture,
and maintenance. End-user instructions belong in `README.md`, `modules.md`,
`docs/INSTRUCTION-MANUAL.md`, and `docs/CONFIGURATION.md`.

## Project overview

LCDSirPlus is a Windows 11 x64 Rust application. `src/main.rs` owns command-line
dispatch, `src/app.rs` wires configuration, telemetry, rendering, input, UI,
and backends, and `src/render/renderer.rs` produces the deterministic 160x43
frame with three selectable fixed built-in layouts.
Provider, backend, and package details are documented in
`docs/ARCHITECTURE.md`.

The build requires Rust 1.97.1
(`8bab26f4f68e0e26f0bb7960be334d5b520ea452`), host
`x86_64-pc-windows-msvc`, minimal profile, plus the corresponding Microsoft C++
build tools. This repository's `rust-toolchain.toml` selects the installed
exact version and does not use a mutable channel alias. Before running any of
the direct Cargo commands below, install this exact toolchain prerequisite:

```powershell
rustup toolchain install 1.97.1 --profile minimal `
  --component clippy --component rustfmt `
  --target x86_64-pc-windows-msvc
```

Build and test scripts select only that already-installed exact alias and verify
its version, commit, host, components, and target. An explicit external
`RUSTUP_TOOLCHAIN` override is accepted only when it passes the same provenance
checks; scripts never silently fall back to `stable` or ask rustup to install or
update a toolchain. Direct Cargo dependencies are Microsoft's `windows` crate
and the `winres` build dependency.

## Build and quality gates

Run commands from the repository root:

```powershell
cargo build
cargo build --release
cargo fmt --check
cargo clippy --all-targets
cargo test --release --locked
pwsh -NoProfile -File .\scripts\Test.ps1
```

`scripts/Test.ps1` parses the packaging scripts with supported PowerShell
versions, checks product identity and formatting, enforces zero Clippy warnings,
runs the deterministic documentation gate and release tests, builds the release
executable, and exercises public smoke commands plus the virtual hardware test.

`build.rs` copies the canonical root `lcdsirplus.txt` beside each Cargo debug or
release executable; local overlay include files are deliberately not copied.
Development and portable runs use that adjacent file unless `--config` is
explicitly supplied. Installed runs instead default to
`%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`; the installed template seeds
that file only when absent, and an explicit path always has precedence.

The release binary uses `IMAGE_SUBSYSTEM_WINDOWS_GUI`. One-shot command modes
attach to an existing parent console without allocating one, while tray/runtime
launches remain console-free even when options such as `--safe-mode` are used.
The quality gate inspects the PE header, captures command output and exits
through waited PowerShell pipelines, and launches exact-PID lifecycle/error-
dialog smokes with guaranteed cleanup.

## Packaging

Version 0.3.0 remains draft/unreleased. PresentMon is implemented,
software-tested, pinned, and enabled by default as a required feature, but live
game capture is not release-qualified and is deferred while this PC's memory is
occupied by the local LLM. Discord is implemented and software-tested; users
register their own applications and credentials remain current-user
DPAPI-protected, but live voice/OAuth qualification is pending unless explicitly
deferred. Guarded termination defaults off and remains unqualified until the
disposable-child physical-button gate passes. These open gates prevent a release
claim.

```powershell
pwsh -NoProfile -File .\scripts\Build.ps1
```

Release packaging requires a clean worktree and an unchanged exact Git HEAD.
The quality gate runs first. Source files are staged from `git archive HEAD`,
not from the working directory. Explicit allowlists produce deterministic
portable/source ZIPs and a versioned Inno Setup EXE with a sorted outer
hash/size manifest. `REPRODUCIBILITY.json` records logical source/tool labels,
tool versions/hashes, and the exact source commit without machine-specific
paths; it is covered by the outer manifest, which never hashes itself. The setup
is compiled twice and must be byte-identical. Portable, source, and setup
payloads carry the same ASCII `SOURCE-COMMIT.txt`; portable/source manifests
cover it, and setup installs it read-only. Final builds accept only the exact
40-lowercase-hex HEAD. Manual non-release setup compilation must explicitly use
`WORKTREE-` followed by a 64-lowercase-hex staged-manifest hash.
Static package tests do not execute the installer.

Real installer qualification is explicit and mutates Windows only when the
confirmation switch is present:

```powershell
$setup = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-setup.exe).Path
$setupSha = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
$portable = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-portable.zip).Path
$sourceIdentity = (& tar.exe -xOf $portable `
  LCDSirPlus-0.3.0-win-x64-portable/SOURCE-COMMIT.txt).Trim()
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode CurrentUserLifecycle `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
# Run from elevated PowerShell on a disposable VM:
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode AllUsersQualification `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
```

These modes require no existing product registration, task, or shortcuts.
Pre-existing `%LOCALAPPDATA%\LCDSirPlus` is supported: the harness records its
exact file/directory metadata and verifies that the lifecycle preserves it.
They exercise legacy/foreign-task refusal, default task selection,
both same-user opposite-scope directions where elevation permits, install,
tamper refusal, cached Modify repair, SID-specific task XML ownership, uninstall,
user-data preservation, and best-effort cleanup. External qualification logs are
retained for diagnosis. Standard-user over-the-shoulder UAC/original-user,
second-account all-users coexistence, and interactive uninstall cancellation
remain explicit disposable-VM gates when those sessions cannot be automated;
the tests do not claim offline-HKU coverage. `Build.ps1` never invokes either
mutation mode. Both lifecycle modes must use the exact same final setup file:
the SHA-256 passed to each command and reported in both transcripts and retained
`qualification-evidence.txt` files must match.

Portable and installer payloads contain public user documentation, the signed
pinned `PresentMon.exe`, and its `LICENSE.txt`/`THIRD_PARTY.txt` notices. The
installer's exact document inventory is `README.md`, `modules.md`, `LICENSE`,
`THIRD_PARTY_LICENSES.txt`, `docs/CONFIGURATION.md`,
`docs/INSTRUCTION-MANUAL.md`, and the PDF manual; the portable ZIP additionally
ships release, security, and hardware-acceptance documents.
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

### Pinned Inno Setup acquisition

```powershell
pwsh -NoProfile -File .\scripts\Acquire-InnoSetup.ps1
pwsh -NoProfile -File .\scripts\Acquire-InnoSetup.ps1 -VerifyOnly
```

This caches only the official tag `is-6_7_3` installer under the ignored
`third_party/InnoSetup/cache/6.7.3` directory. It verifies the exact installer
URL, byte size, SHA-256, and Authenticode certificate identity against
`third_party/InnoSetup/manifest.json`. Release builds verify that cached
installer, freshly extract the complete portable toolchain into a unique
build-owned temporary directory, verify `ISCC.exe` SHA-256/banner and the
installed license, and remove the extraction on success or failure. They never
execute an extracted long-lived cache or acquire from the network. The source
ZIP includes the `.iss`, manifest, license, and acquisition script, but no
compiler binaries.

PresentMon is MIT-licensed by Intel. HWiNFO is separately licensed under its
vendor terms; LibreHardwareMonitor is MPL-2.0. ADLX and NVAPI/NVML are
vendor-installed APIs used under vendor terms. None is a Rust dependency, and
no sensor/GPU SDK is bundled. These projects and vendors are not affiliated
with or endorsers of LCDSirPlus.

`THIRD_PARTY_LICENSES.txt` inventories the selected Windows x64 MSVC Cargo
dependency closure, its Cargo.lock checksums, the observed Rust standard-library
provenance, and build-only dependencies. PresentMon retains its separately
shipped upstream notices under `licenses/PresentMon/`.

## G13 backend proven contract

- SetupAPI accepts exactly VID `046D`, PID `C21C`, usage page `FF00`, usage
  `0000`, with 8-byte input and 992-byte output reports.
- Output uses report ID `0x03`, a 32-byte header, and a 960-byte payload:
  `report[32 + x + (y/8)*160] |= 1 << (y & 7)`.
- Input uses report ID `0x01`; LCD buttons are byte 6 bits
  `0x02 << 0..3`.
- Direct HID uses overlapped I/O, a one-second operation timeout, bounded
  reconnect backoff, and blank-on-close. It refuses exact owners `LCore.exe` and
  `logi_lamparray_service.AMD64.exe` without blanket-blocking other G HUB
  services.
- `auto` uses the trusted Logitech LCD SDK while LCore is present and direct HID
  while it is absent. SDK loading is absolute-path-only from the running signed
  LCore installation after pinned-object Program Files, reparse/ACL/hard-link,
  AMD64 PE/export, cached-revocation, exact signer-certificate, product, and
  full-version validation. The loaded module identity must match the pinned DLL
  before any SDK call, and all SDK calls run on one bounded owner thread.
- Direct HID takes a non-shared output handle, rechecks competing owners after
  opening and before every write, and polls ownership while idle. Owner
  detection closes HID without a final blank.
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
- `docs/REFERENCE-LAYOUT.md`: shared and per-layout geometry plus renderer golden hashes.
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

The renderer's three built-in displays are pinned by implementation tests to
the hashes in `docs/REFERENCE-LAYOUT.md`. These are not release-build or
physical-acceptance evidence. Intentional shared or per-layout changes require
updated goldens and visual review.

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
