# LCDSirPlus Development

Audience: maintainers building, testing, packaging, or updating documentation.
This source-only guide contains repository procedures; end-user procedures stay
in [INSTRUCTION-MANUAL.md](INSTRUCTION-MANUAL.md). Version 0.3.0 remains draft
and unreleased. For subsystem ownership use [ARCHITECTURE.md](ARCHITECTURE.md),
and for live release gates use [HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md).

## Prepare the toolchain

The repository pins Rust 1.97.1, commit
`8bab26f4f68e0e26f0bb7960be334d5b520ea452`, for
`x86_64-pc-windows-msvc`, plus Clippy and rustfmt. Install the matching Microsoft
C++ build tools and run:

```powershell
rustup toolchain install 1.97.1 --profile minimal `
  --component clippy --component rustfmt `
  --target x86_64-pc-windows-msvc
```

Scripts verify the selected version, commit, host, components, and target. They
do not install or update Rust automatically. Direct Rust dependencies are the
Microsoft `windows` crate and the vendored `winres` build dependency.

## Build and test

Run from the repository root:

```powershell
cargo build
cargo build --release
cargo fmt --check
cargo clippy --all-targets
cargo test --release --locked
pwsh -NoProfile -File .\scripts\Test.ps1
```

`scripts/Test.ps1` checks PowerShell syntax, dependency notices, product
identity, module documentation order, formatting, zero Clippy warnings, tests,
release build, public commands, virtual G13 output, and process lifecycle.
`-ReleaseOnly` stops after release tests and does not package.

Public scripts expose PowerShell help. From the repository root, inspect the
exact parameters and examples before using a less common mode:

```powershell
Get-Help .\scripts\Acquire-InnoSetup.ps1 -Full
Get-Help .\scripts\Acquire-PresentMon.ps1 -Full
Get-Help .\scripts\Test.ps1 -Full
Get-Help .\scripts\Test-ReproducibleBuild.ps1 -Full
Get-Help .\scripts\Package-Test.ps1 -Full
Get-Help .\scripts\Build.ps1 -Full
Get-Help .\scripts\Build-Manual.ps1 -Full
```

`build.rs` copies root `lcdsirplus.txt` beside Cargo executables. It does not
copy `lcdsirplus.local.txt`. Installed runs use the per-user configuration path
instead.

The binary uses the Windows GUI subsystem. Normal dashboard launch has no
console. One-shot commands attach to a parent console or redirected handles.

## Review architecture changes

- Start in [ARCHITECTURE.md](ARCHITECTURE.md) for ownership and trust boundaries.
- Use [PRODUCT-SPEC.md](PRODUCT-SPEC.md) for required behavior.
- Use [REFERENCE-LAYOUT.md](REFERENCE-LAYOUT.md) for pixel geometry and goldens.
- Use [HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md) for physical/live checks.
- Keep [CONFIGURATION.md](CONFIGURATION.md) complete with `src/config.rs` and
  `src/parser.rs`.

Renderer changes must update tests and receive visual review. Data-source and
device changes must preserve shutdown, stale-state, and privacy behavior.

## Prepare PresentMon

PresentMon is a release input, not a Cargo dependency:

```powershell
pwsh -NoProfile -File .\scripts\Acquire-PresentMon.ps1
pwsh -NoProfile -File .\scripts\Acquire-PresentMon.ps1 -VerifyOnly
```

`third_party/PresentMon/manifest.json` pins the official v2.5.1 x64 console,
956768-byte size, SHA-256
`9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191`,
Intel signature, upstream commit, required flags, and notice files. Acquisition
accepts no service, MSI, GUI, or API package.

Offline release builds must already contain the verified console and notices.
Packaging places one console beside the application. Raw Cargo builds need the
same approved adjacent executable for `presentmon_path auto` testing.

## Prepare Inno Setup

```powershell
pwsh -NoProfile -File .\scripts\Acquire-InnoSetup.ps1
pwsh -NoProfile -File .\scripts\Acquire-InnoSetup.ps1 -VerifyOnly
```

The script verifies official Inno Setup 6.7.3 against
`third_party/InnoSetup/manifest.json`. Release builds extract a fresh temporary
compiler, verify it, use it, and remove it. Compiler binaries are not included
in the source ZIP.

## Build release packages

```powershell
pwsh -NoProfile -File .\scripts\Build.ps1
```

Release packaging requires a clean unchanged Git HEAD. It runs the quality
checks, creates the source ZIP from `git archive HEAD`, and produces
deterministic portable/source ZIPs and a versioned setup EXE. The source ZIP is
the committed HEAD, never an overlay of uncommitted worktree files. Each ZIP has
one root, `SOURCE-COMMIT.txt`, and a sorted `PACKAGE-MANIFEST.txt`. The outer
checksum file covers the ZIPs and setup EXE.

`REPRODUCIBILITY.json` records source and tool identities without local paths.
The setup is compiled twice and must match byte-for-byte. Static package tests
do not install it.

The current document inventories are:

| Package | Documents |
|---|---|
| Installer | `README.md`, `modules.md`, `RELEASE-NOTES.md`, `SECURITY.md`, `LICENSE`, `THIRD_PARTY_LICENSES.txt`, `docs/CONFIGURATION.md`, `docs/INSTRUCTION-MANUAL.md`, `docs/LCDSirPlus-Instruction-Manual.pdf` |
| Portable ZIP | The same nine documents as the installer. |
| Source ZIP | The same nine documents plus `docs/ARCHITECTURE.md`, `docs/DEVELOPMENT.md`, `docs/HARDWARE-ACCEPTANCE.md`, `docs/PRODUCT-SPEC.md`, and `docs/REFERENCE-LAYOUT.md`, all from committed HEAD. |

The installer and portable ZIP also carry PresentMon's `LICENSE.txt` and
`THIRD_PARTY.txt` under `licenses/PresentMon`; the source ZIP carries those
notices under `third_party/PresentMon`.

The installer Start Menu group links to LCDSirPlus, the PDF **Instruction
Manual**, `SECURITY.md` as **Security**, and the uninstaller. The five maintainer
documents, including the acceptance ledger, remain source-only.

## Qualify installer lifecycle

System-changing checks require an explicit switch and a disposable environment:

```powershell
$setup = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-setup.exe).Path
$setupSha = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
$portable = (Resolve-Path .\artifacts\release\LCDSirPlus-0.3.0-win-x64-portable.zip).Path
$sourceIdentity = (& tar.exe -xOf $portable `
  LCDSirPlus-0.3.0-win-x64-portable/SOURCE-COMMIT.txt).Trim()
pwsh -NoProfile -File .\scripts\Package-Test.ps1 -Mode CurrentUserLifecycle `
  -SetupPath $setup -ExpectedSetupSha256 $setupSha `
  -ExpectedSourceIdentity $sourceIdentity -ConfirmSystemMutation
```

Run `AllUsersQualification` from elevated PowerShell on a disposable VM with
the same setup checksum and source identity. Keep both transcripts. The checks
cover install scope, startup-task ownership, repair, collision refusal,
uninstall, and LocalAppData preservation. Interactive cancellation,
over-the-shoulder approval, and second-account coexistence still need explicit
manual checks where automation cannot reproduce them.

## Update documentation

`modules.md` is the canonical quick reference for all 53 values. Its table must
contain exactly the `src/config.rs` values in code order and be mirrored exactly
in the instruction manual/PDF. The parser/document inventory has 123 accepted
entries, the default template has 96 active keys, and exactly five advanced
compatibility-only keys are ignored. `scripts/Test.ps1` checks these inventories,
source order, row parity, and required links.

After any Markdown manual change, regenerate the PDF before packaging:

```powershell
pwsh -NoProfile -File .\scripts\Build-Manual.ps1
```

PowerShell 7 and installed Chrome or Edge are required. The script normalizes
PDF timestamps and replaces the tracked PDF only after validation.

## Keep notices current

PresentMon retains its own shipped MIT and third-party notices. HWiNFO is used
under vendor terms. LibreHardwareMonitor is MPL-2.0. NVIDIA and AMD interfaces
come from vendor-installed drivers. None of those projects endorses
LCDSirPlus. Update `THIRD_PARTY_LICENSES.txt` only through the separate legal
and dependency review process.
