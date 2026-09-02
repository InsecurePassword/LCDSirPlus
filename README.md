# LCDSirPlus 0.3.0

> **Draft / unreleased:** LCDSirPlus 0.3.0 is not published. There is no
> supported public download yet, and final package testing is still pending.

LCDSirPlus is a Windows dashboard for the Logitech G13. It shows system, game,
device, network, and Discord voice information on the G13 LCD or an on-screen
preview. The preview works without a G13.

## Requirements

- Windows 11 x64
- A Logitech G13 for the physical LCD and buttons; optional for preview use
- Logitech Gaming Software for the recommended G13 connection
- PresentMon for FPS; planned packages include the approved executable
- HWiNFO or LibreHardwareMonitor for CPU temperature; neither is bundled
- Discord Desktop and your own Discord application for voice names

## Start LCDSirPlus

When a release is published, verify its SHA-256 checksum before running it. The
[instruction manual](docs/INSTRUCTION-MANUAL.md#verify-a-download) contains the
complete verification and installation steps.

For an installed copy, open **LCDSirPlus** from the Start Menu. The planned
installer also adds shortcuts to the instruction manual and security guide.

For a portable copy, extract the ZIP, open its folder in Terminal, and run:

```powershell
.\LCDSirPlus.exe --preview
```

The tray icon controls the preview. Clicking the preview does not take keyboard
focus from a game or other active app.

## Configure It

Installed settings are stored in:

```text
%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt
```

Portable settings are in `lcdsirplus.txt` beside `LCDSirPlus.exe`. Valid saved
changes apply automatically; invalid changes are rejected without replacing the
last valid settings.

Use the [instruction manual](docs/INSTRUCTION-MANUAL.md) for setup and
troubleshooting, the [configuration reference](docs/CONFIGURATION.md) for every
setting, and the [button option reference](modules.md) for all 53 LCD choices.

## Safety And Privacy

- Keep `hang_enabled 0`. The hung-window action can terminate a program and
  lose unsaved work, and its physical behavior is not release-qualified.
- Never put a Discord client secret in settings, command arguments, logs,
  screenshots, issues, or chat. Paste it only into the manual's masked prompt.
- PresentMon can display a selected program filename and FPS. Set
  `presentmon_enabled 0` if that exposure is unacceptable.

Safe mode disables optional dashboard data sources, network probes, startup
changes, and destructive actions while keeping basic local readings and the
preview available:

```powershell
.\LCDSirPlus.exe --safe-mode
```

Explicit Discord authorization and credential-removal commands remain available
in safe mode. See the [security guide](SECURITY.md) before sharing diagnostics or
changing hardware-related services.

## Update Or Remove

Exit LCDSirPlus before updating. Run a newer installer with the same scope, or
replace the files in a portable folder. Settings under `%LOCALAPPDATA%` are kept
when an installed copy is removed; the manual explains how to remove them.

## Documentation

- [Instruction manual](docs/INSTRUCTION-MANUAL.md)
- [PDF manual](docs/LCDSirPlus-Instruction-Manual.pdf)
- [Configuration reference](docs/CONFIGURATION.md)
- [Button option reference](modules.md)
- [Security guide](SECURITY.md)
- [Draft release notes](RELEASE-NOTES.md)
- [MIT license](LICENSE)
- [Third-party notices](THIRD_PARTY_LICENSES.txt)
