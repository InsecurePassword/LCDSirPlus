# LCDSirPlus 0.3.0 Instruction Manual

> **Draft / Unpublished:** LCDSirPlus 0.3.0 is not published. The package
> described here is planned, and final package testing remains pending.

LCDSirPlus runs on Windows 11 x64. A Logitech G13 is needed only for the
physical LCD and buttons. The virtual preview works without one.

Start here: [verify a download](#verify-a-download),
[install or extract it](#install-or-run-portable), then follow
[first start](#start-lcdsirplus-the-first-time).

## Contents

- [Read display messages](#read-display-messages)
- [Verify a download](#verify-a-download)
- [Install or run portable](#install-or-run-portable)
- [Start LCDSirPlus the first time](#start-lcdsirplus-the-first-time)
- [Open and edit settings](#open-and-edit-settings)
- [Button option guide](#button-option-guide)
- [Show game FPS](#show-game-fps)
- [Set up CPU temperature](#set-up-cpu-temperature)
- [Connect devices and network data](#connect-devices-and-network-data)
- [Understand PROVIDER_STATUS](#understand-provider_status)
- [Connect Discord](#connect-discord)
- [Use alerts and safe mode](#use-alerts-and-safe-mode)
- [Run command-line tools](#run-command-line-tools)
- [Check a Logitech G13](#check-a-logitech-g13)
- [Fix common problems](#fix-common-problems)
- [Update or uninstall](#update-or-uninstall)

## Read display messages

- `N/A`: the required device or software is not configured, connected, or
  returning a usable reading. Check the requirement in
  [the button option guide](../modules.md).
- `STALE`: the displayed value is old because updates stopped. Do not trust it;
  check the required device or software.
- `OFFLINE`: a supported device is disconnected or turned off.
- `DISABLED`: the feature or safe mode turned the data source off.
- `IDLE`: no active PresentMon game session.
- `CLEAR`: no alert or no available temporary button option.
- `DOWN`: one or more tracked sources are disabled, unconfigured,
  disconnected, or unavailable. Optional sources you do not use can count as
  down, so this is not necessarily a failure.
- `NO DONGLE`: no supported SteelSeries wireless receiver was found.
- `UNKNOWN`: Windows did not provide a usable system battery state.
- `AC ONLY`: AC power is connected and Windows reports no battery.
- `NO BAT`: AC power is disconnected and Windows reports no battery.
- `CHG`: the system battery is charging; a percentage follows.
- `AC`: the system is on AC power; a battery percentage follows.
- `BAT`: the system is on battery power; a percentage follows.
- `OK`: all sources included in the current status summary are ready. In safe
  mode, this means the restricted safe-mode dashboard is healthy.
- `WIRED`: an XInput controller is connected by cable, so no battery percentage
  is available.
- `MUTED`: the default Windows microphone is muted.
- `LIVE`: the default Windows microphone is not muted.

In safe mode, optional panels can show inactive states such as `DISABLED`,
`OFFLINE`, `N/A`, or `CLEAR`. These states describe the restricted dashboard;
they do not mean safe mode itself failed.

## Verify a download

The planned LCDSirPlus packages are unsigned. When they are published, open
Terminal in the folder containing the package and
`LCDSirPlus-0.3.0-SHA256SUMS.txt`, then run the matching command:

```powershell
Get-FileHash .\LCDSirPlus-0.3.0-win-x64-setup.exe -Algorithm SHA256
Get-FileHash .\LCDSirPlus-0.3.0-win-x64-portable.zip -Algorithm SHA256
```

The displayed hash must exactly match the line for the same filename in the
checksum file. Do not run or extract the package if the filename is missing or
the hash differs. The planned ZIP also has `PACKAGE-MANIFEST.txt`, which lists
every file's checksum and size. PresentMon has its own Intel signature.

## Install or run portable

### Install for the current user or all users

Exit LCDSirPlus, then run `LCDSirPlus-0.3.0-win-x64-setup.exe`.

- Current-user program files go to
  `%LOCALAPPDATA%\Programs\LCDSirPlus`.
- All-users program files go to `%ProgramFiles%\LCDSirPlus` after approval.
- Start Menu shortcuts for LCDSirPlus, this manual, and the security guide are
  selected by default, as is **Start LCDSirPlus when I sign in**.
- The desktop shortcut is optional.

The planned install also includes `SECURITY.md`, `RELEASE-NOTES.md`, and
`modules.md` in the program folder.

An all-users install makes program files available to every user. The selected
sign-in startup task belongs only to the Windows account that ran setup. Setup
does not automatically add a startup task for other users.

The first installed start creates this user-owned file only if it is missing:

```text
%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt
```

Updates and repairs do not replace it.

### Run the portable package

1. Extract the ZIP to a folder on a local drive.
2. Open the folder in File Explorer.
3. Right-click empty space and select **Open in Terminal**.
4. Run `.\LCDSirPlus.exe --preview`.

Keep `LCDSirPlus.exe`, `PresentMon.exe`, `lcdsirplus.txt`, `SECURITY.md`,
`RELEASE-NOTES.md`, `modules.md`, and the `docs` folder together.
Portable use creates no installer-owned shortcuts.

## Run commands from the correct folder

Commands beginning with `.\LCDSirPlus.exe` must run in the folder containing
that executable. Use **Open in Terminal** there first.

For either an installed or portable copy, open Terminal in the folder that
contains `LCDSirPlus.exe`. All command examples below are package-relative and
use `.\LCDSirPlus.exe`.

## Start LCDSirPlus the first time

### Installed copy

Open **Start**, find **LCDSirPlus**, and select it.

### Portable copy

Double-click `LCDSirPlus.exe`, or run it from the open Terminal.

Expected result:

- The LCDSirPlus tray icon appears.
- Without a G13, the preview appears automatically.
- With a working physical G13 connection, the preview normally starts hidden.
- Left-click the tray icon to show or hide the preview.
- Right-click it for **Toggle Preview** and **Exit**.
- Closing the preview window exits LCDSirPlus.
- Showing or clicking the preview does not take keyboard focus from the active
  game or app.

Only one normal LCDSirPlus process can run in one Windows session.

## Open and edit settings

### Installed copy

1. Press **Win+R**.
2. Enter `%LOCALAPPDATA%\LCDSirPlus\Config` and press Enter.
3. Right-click `lcdsirplus.txt`.
4. Select **Open with**, then **Notepad**.

### Portable copy

Open `lcdsirplus.txt` beside `LCDSirPlus.exe` in Notepad.

Save after editing. Valid changes apply automatically. An invalid change is
rejected, and the last valid settings remain active. Validate with:

```powershell
.\LCDSirPlus.exe --validate-config
```

An explicit file can be checked with:

```powershell
.\LCDSirPlus.exe --validate-config --config .\lcdsirplus.txt
```

The format uses one setting followed by values. `#` starts a comment. Quote a
value that contains spaces. See [CONFIGURATION.md](CONFIGURATION.md) for every
key, default, range, and include-file rule.

## Choose layouts and button options

Set one main layout:

```text
main_display 1
```

- `1`: CPU/RAM and GPU/VRAM halves.
- `2`: CPU/RAM, GPU/VRAM, and outgoing/incoming thirds.
- `3`: CPU/RAM and incoming/outgoing halves.

All three keep the date/time header and four lower button slots. Layouts 2 and
3 scale network bars with `network_graph_ceiling_mbps`.

Each slot is an ordered list for one physical button and preview area:

```text
slot_0  HEADSET_BATTERY CPU_TEMP CONTROLLER_BATTERY
slot_1  FPS_CURRENT FPS_1LOW FRAME_TIME SESSION_TIME
slot_2  PROC_HANG GPU_TEMP PING JITTER AUDIO
slot_3  THERMALS PACKET_LOSS MIC_STATUS SESSION_SUMMARY PROVIDER_STATUS
```

A short physical press moves forward. Preview left-click moves forward and
right-click moves backward. Button 4 first acknowledges the highest active
alert. `PROC_HANG` can appear only once across all slots.

### Button option guide

The quick reference in [modules.md](../modules.md) mirrors this table exactly.
In an installed or portable package, open `modules.md` beside `LCDSirPlus.exe`.

| Option | Description |
|---|---|
| `HEADSET_BATTERY` | Shows battery and connection. What you need: A supported SteelSeries Arctis/GameBuds wireless USB receiver. |
| `CONTROLLER_BATTERY` | Shows controller battery or WIRED. What you need: A controller that Windows recognizes as an Xbox (XInput) controller. |
| `FPS_CURRENT` | Shows current FPS. What you need: PresentMon, planned for the 0.3.0 package. |
| `FPS_1LOW` | Shows common FPS slowdowns. What you need: PresentMon, planned for the 0.3.0 package. |
| `FPS_01LOW` | Shows rarer severe FPS slowdowns. What you need: PresentMon, planned for the 0.3.0 package. |
| `FRAME_TIME` | Shows frame time and a 30-second graph. What you need: PresentMon, planned for the 0.3.0 package. |
| `CPU_TEMP` | Shows CPU temperature. What you need: HWiNFO or LibreHardwareMonitor, installed and set up separately. |
| `GPU_TEMP` | Shows GPU temperature. What you need: An NVIDIA/AMD graphics driver, or LibreHardwareMonitor set up separately. |
| `NET_IN` | Shows the current incoming rate. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `NET_OUT` | Shows the current outgoing rate. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `NET_BOTH` | Shows incoming plus outgoing. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `NET_IN_GRAPH` | Shows incoming traffic for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `NET_OUT_GRAPH` | Shows outgoing traffic for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `NET_GRAPH` | Shows both network directions for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `PING` | Shows the latest network delay. What you need: The optional network probe, enabled and configured separately. |
| `JITTER` | Shows network delay variation. What you need: The optional network probe, enabled and configured separately. |
| `PACKET_LOSS` | Shows the percentage of failed checks. What you need: The optional network probe, enabled and configured separately. |
| `MIC_STATUS` | Shows LIVE or MUTED. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `AUDIO` | Shows default output volume. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `SESSION_TIME` | Shows active game-session time. What you need: PresentMon, planned for the 0.3.0 package. |
| `SESSION_SUMMARY` | Shows session time and stutters. What you need: PresentMon, planned for the 0.3.0 package. |
| `CLOCK` | Shows local time. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `GAME_NAME` | Shows the selected game or app filename. What you need: PresentMon, planned for the 0.3.0 package. A filename may appear under either persist setting; set `presentmon_enabled 0` to prevent this exposure. |
| `ALERTS` | Shows unacknowledged active alerts or CLEAR. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `PROVIDER_STATUS` | Shows unavailable tracked data sources or OK. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `CPU_LOAD` | Shows total CPU use. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `RAM_USAGE` | Shows memory use and capacity. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `GPU_LOAD` | Shows GPU use. What you need: An NVIDIA or AMD graphics driver. |
| `VRAM_USAGE` | Shows video memory use and capacity. What you need: An NVIDIA or AMD graphics driver. |
| `CPU_CACHE_TEMP` | Shows N/A. What you need: Not available in the draft 0.3.0 build. |
| `CPU_FREQ_TEMP` | Shows N/A. What you need: Not available in the draft 0.3.0 build. |
| `CPU_LOAD_GRAPH` | Shows CPU use for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `GPU_LOAD_GRAPH` | Shows GPU use for 30 seconds. What you need: An NVIDIA or AMD graphics driver. |
| `CPU_TEMP_GRAPH` | Shows CPU temperature for 30 seconds. What you need: HWiNFO or LibreHardwareMonitor, installed and set up separately. |
| `GPU_TEMP_GRAPH` | Shows GPU temperature for 30 seconds. What you need: An NVIDIA/AMD graphics driver, or LibreHardwareMonitor set up separately. |
| `VRM_TEMP` | Shows motherboard power-circuit temperature. What you need: LibreHardwareMonitor with the correct VRM temperature selected. |
| `CPU_FAN` | Shows CPU fan percentage. What you need: LibreHardwareMonitor with the correct CPU fan selected. |
| `PUMP_RPM` | Shows pump percentage. What you need: LibreHardwareMonitor, set up with the matching pump reading selected. |
| `POWER_LIMIT` | Shows total system power. What you need: One total-power reading selected in HWiNFO or LibreHardwareMonitor. |
| `CPU_GPU_POWER` | Shows CPU plus GPU power. What you need: CPU power from HWiNFO or LibreHardwareMonitor, plus GPU power from the graphics driver, HWiNFO, or LibreHardwareMonitor. |
| `CHIPSET_TEMP` | Shows chipset temperature. What you need: LibreHardwareMonitor, set up with the matching sensor selected. |
| `MOTHERBOARD_TEMP` | Shows motherboard temperature. What you need: LibreHardwareMonitor, set up with the matching sensor selected. |
| `DISK_IO` | Shows total disk read and write rates. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `DISK_IO_GRAPH` | Shows disk activity for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `RAM_DETAIL` | Shows used and total memory. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `FPS_GRAPH` | Shows FPS for 30 seconds. What you need: PresentMon, planned for the 0.3.0 package. |
| `THERMALS` | Shows CPU and GPU temperatures. What you need: Both CPU and GPU temperature sources set up and current. |
| `CONNECTIONS` | Shows established IPv4 and IPv6 TCP connections only. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `NET_HEALTH` | Shows ping, jitter, and packet loss. What you need: The optional network probe, enabled and configured separately. |
| `SYSTEM_BATTERY` | Shows AC, battery, and charging. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `HARD_FAULTS` | Shows memory-to-disk pressure. What you need: Built into Windows and LCDSirPlus; no extra software. |
| `BOTTLENECK` | Shows CPU, GPU, RAM, or disk. With current NONE, the next eligible option appears temporarily or the slot shows CLEAR. What you need: Built-in system readings; GPU results also need a supported graphics driver. |
| `PROC_HANG` | Shows a hung-window target. With no target, including while disabled, the next eligible option appears temporarily or the slot shows CLEAR. What you need: Not supported for end users in this draft release; leave `hang_enabled 0`. |

## Show game FPS

The planned packages include PresentMon, and FPS is enabled by default.

1. Keep `FPS_CURRENT` in a slot. The default `slot_1` already has it.
2. Start LCDSirPlus.
3. Start the game.
4. Press the second LCD button, or click the second preview slot, until current
   FPS appears.

Game selection is automatic. You do not maintain a game list.

The useful defaults are:

```text
presentmon_enabled      1
presentmon_target_mode  presenting
presentmon_deferred     1
presentmon_persist      0
```

`presentmon_deferred 1` temporarily skips any unavailable PresentMon option for
display without changing the stored choice. For example, with
`slot_2 FPS_CURRENT GPU_TEMP`, a closed game can make the slot show GPU
temperature; FPS returns when game data becomes available. A temporary result
is never `PROC_HANG` or `BOTTLENECK`, and an all-unavailable list shows `CLEAR`.
Set `presentmon_deferred 0` to keep the selected inactive panel visible instead.

`presentmon_persist 0` is the normal game preference. When a valid NVIDIA App
game list is available, ordinary desktop apps are rejected. If that list is
unavailable, automatic fallback may still select another app. Set it to `1` to
allow desktop apps even when the game list is available. Under either persist
value, the selected executable filename and FPS may appear; old readings are
not preserved. If that exposure is not acceptable, set `presentmon_enabled 0`.
Advanced details are in
[CONFIGURATION.md](CONFIGURATION.md#use-presentmon).

If Windows denies capture, add your Windows user to **Performance Log Users**,
then sign out and sign in. Run elevated only to diagnose access; elevation is
not the normal operating mode.

## Set up CPU temperature

CPU temperature needs either
[HWiNFO](https://www.hwinfo.com/) or
[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor),
installed and set up separately. LCDSirPlus has no fixed minimum supported
version for either tool; use a current official release and validate your setup.

### Use HWiNFO

1. Download and install HWiNFO from its official site.
2. Start HWiNFO, select **Sensors-only**, and click **Start**.
3. In the Sensors window, click the settings gear.
4. On the **General** tab, enable **Shared Memory Support**, then click **OK**.
5. In Terminal opened in the LCDSirPlus executable folder, run:

   ```powershell
   .\LCDSirPlus.exe --list-hwinfo-sensors
   ```

6. Find the CPU package/control temperature you want. Output resembles:

   ```text
   sensor="CPU [#0]" reading="CPU (Tctl/Tdie)" type=Temperature value=60 unit="C"
   ```

7. Copy your exact, case-sensitive labels into the installed or portable
   settings file:

   ```text
   hwinfo_cpu_temp_sensor  "CPU [#0]"
   hwinfo_cpu_temp_reading "CPU (Tctl/Tdie)"
   ```

8. Save and validate.

Both settings must be present together. HWiNFO64 Free shared-memory monitoring
has a 12-hour limit. For unattended use, use HWiNFO32 or an appropriately
licensed HWiNFO Pro. LCDSirPlus does not start or configure HWiNFO.

### Use LibreHardwareMonitor

1. Download a current official LibreHardwareMonitor release.
2. Extract it and start `LibreHardwareMonitor.exe`.
3. In its menu, select **Options > Remote Web Server > Run**.
4. Open `http://127.0.0.1:8085/data.json` in a browser on the same computer.
5. Keep these LCDSirPlus defaults:

   ```text
   lhm_mode auto
   lhm_url auto
   ```

6. Save and validate. Automatic CPU temperature selection may be enough.
7. If it selects the wrong temperature, copy the exact sensor path shown by
   `data.json`, for example:

   ```text
   lhm_cpu_temp_sensor /amdcpu/0/temperature/2
   ```

LCDSirPlus accepts only a local LibreHardwareMonitor address. It does not enable
the web server or request a firewall exception. Do not expose this server to
other computers.

`CPU_CACHE_TEMP` and `CPU_FREQ_TEMP` are not available in this release.

## Connect devices and network data

`HEADSET_BATTERY` supports only documented SteelSeries Arctis/GameBuds wireless
USB receivers. Bluetooth-only and wired models are unsupported. SteelSeries GG
is not required. GameBuds shows the lower active-earbud battery, not the case.
See the exact receiver list in
[CONFIGURATION.md](CONFIGURATION.md#supported-steelseries-receivers).

`CONTROLLER_BATTERY` uses an XInput-compatible controller.
`controller_index -1` chooses the first connected controller. A wired
controller shows its wired state because XInput supplies no percentage.

`AUDIO` and `MIC_STATUS` use the current default Windows sound devices.

Incoming and outgoing rates are built into Windows and send no probe traffic.
Ping, jitter, and packet loss use a separate optional probe. It is off by
default because it sends traffic. To enable it:

```text
network_probe_enabled     1
network_probe_method      auto
network_probe_target      1.1.1.1
network_probe_interval_ms 1000
network_probe_timeout_ms  1500
network_probe_window      30
```

The target must be an IP address, not a hostname. TCP can include a port.
Explicit ICMP accepts IPv4 without a port. Safe mode sends no probe traffic.

## Understand PROVIDER_STATUS

`PROVIDER_STATUS` summarizes tracked data sources. A new setup can show `DOWN`
because Discord has no application ID and the hung-window action is disabled.
This is expected and is not necessarily a failure.

Setting `discord_enabled 0` or keeping `hang_enabled 0` does not remove those
two entries from the summary. Other intentionally unused sources can be turned
off, for example:

```text
headset_enabled       0
controller_enabled    0
audio_enabled         0
gpu_provider          off
lhm_mode              off
presentmon_enabled    0
network_probe_enabled 0
```

Safe mode reports only its healthy safe-mode state, so the button option shows
`OK`. To hide the summary during normal use, remove `PROVIDER_STATUS` from
`slot_3`.

An inactive `PROC_HANG` or `BOTTLENECK` selection temporarily shows the next
eligible option without changing the stored selection. Neither option is used
as the other's temporary replacement. If no eligible option is available, the
slot shows `CLEAR`.

## Connect Discord

**Never put the Discord client secret in settings, command arguments, or a text
file. Copy it only when needed and paste it only into the masked prompt below.
Never include it in logs, screenshots, issues, or chat.**

Discord display needs Discord Desktop and your own application:

1. Open the official
   [Discord Developer Portal](https://discord.com/developers/applications).
2. Select **New Application**, enter a name, and create it.
3. Open **General Information** and copy the numeric **Application ID**.
4. Put that public number in your settings:

   ```text
   discord_enabled   1
   discord_client_id 123456789012345678
   ```

5. Add the Discord account as an application tester if Discord requires tester
   access for the application.
6. In the Developer Portal, open **OAuth2 > General**. Under **Client Secret**,
   select **Reset Secret** if Discord requires it, then copy the displayed secret
   only for the masked prompt below. Do not save it in a text file.
7. Start Discord Desktop under the same Windows account and select the account
   you want to authorize.
8. Open Terminal in the folder containing `LCDSirPlus.exe`. The following
   masked workflow works in Windows PowerShell 5 and PowerShell 7:

   ```powershell
   $exe = ".\LCDSirPlus.exe"
   try {
       $secret = Read-Host 'Discord client secret' -AsSecureString
       $env:LCDSIRPLUS_DISCORD_CLIENT_SECRET = [System.Net.NetworkCredential]::new('', $secret).Password
       & $exe --discord-authorize
   } finally {
       Remove-Item Env:\LCDSIRPLUS_DISCORD_CLIENT_SECRET -ErrorAction SilentlyContinue
       $secret = $null
   }
   ```

9. Approve the prompt in Discord and join a voice channel. When someone speaks,
   their name should appear automatically on the LCD or preview.

Discord's **Public Client** switch does not remove the secret requirement for
this workflow. Windows protects the secret and access records for the current
Windows account. Authorize each Discord account separately.

### Disconnect Discord

1. Open Terminal in the folder containing `LCDSirPlus.exe`.
2. Set `$exe = ".\LCDSirPlus.exe"` as in step 8.
3. Remove the local credentials:

```powershell
& $exe --discord-clear-token
```

4. Revoke the application under Discord **User Settings > Authorized Apps**.

Local deletion and remote revocation are separate; do both after suspected
exposure.

## Use alerts and safe mode

Temperature and memory alerts are independent. Only current readings create an
alert. `memory_warning` and `vmem_warning` default to `100` and trigger at or
above that value. Button 4 acknowledges the highest active alert.

`warning 1` flashes the selected temperature button area and suppresses its
full-screen temperature alert. `warning 0` keeps the full-screen alert.

Start safe mode with:

```powershell
.\LCDSirPlus.exe --safe-mode
```

Safe mode limits the dashboard to the clock, built-in CPU load, memory, the
preview, slot cycling, and alert acknowledgement. It disables every optional
dashboard data source, Discord dashboard access, network probes, startup
changes, and the destructive hung-window action. The explicit
`--discord-authorize` and `--discord-clear-token` commands remain available;
safe mode does not silently authorize or remove credentials.

Do not enable the hung-window action in this release. Its physical behavior has
not been release-tested, and there is no supported end-user test. It can
terminate a program and lose unsaved work. Keep `hang_enabled 0`.

## Create diagnostics safely

Run from the executable folder:

```powershell
.\LCDSirPlus.exe --diagnostics
.\LCDSirPlus.exe --diagnostics --diagnostic-dir .\diagnostics
```

Diagnostics is a bounded offline summary, not a report of active provider
state. It ignores `--config` and does not start HID, optional providers,
Discord, network activity, or actions. The ZIP contains only `privacy.txt`,
`report.txt`, and `manifest.txt` and is capped at 1 MiB total and 128 KiB per
entry. It excludes raw logs, settings, credentials, Discord content and IDs,
process/window names, paths, network targets and addresses, environment values,
command lines, registry values, serial numbers, arbitrary directory listings,
and device paths. It records only the sizes and counts of known rotated logs,
not their contents.

Review the ZIP before sharing. Use the task-specific checks in this manual,
such as `--validate-config`, `--list-hwinfo-sensors`, or
`--hardware-discover`, to investigate a particular failure. Review the normal
log privately and separately; do not attach it without checking every line.

The normal log is `%LOCALAPPDATA%\LCDSirPlus\lcdsirplus.log`.

## Run command-line tools

```text
LCDSirPlus.exe [--config PATH] [COMMAND]
  (none)             Run the dashboard application
  --validate-config  Validate the configuration and exit
  --list-hwinfo-sensors  List usable HWiNFO sensor/reading labels and exit
  --preview          Run with the virtual preview forced on
  --hardware-test    Run the deterministic 10-step G13 test sequence
  --hardware-discover  Inspect up to 256 HID interfaces for the first exact G13 match
  --diagnostics      Write a bounded offline diagnostics ZIP and exit
  --discord-authorize  Authorize the active Discord Desktop account
  --discord-clear-token  Remove all LCDSirPlus Discord credentials
  --backend auto|sdk|hid|virtual  Hardware test: sdk=Logitech software, hid=direct USB
  --duration-secs N  Duration for timed test commands (1..3600 seconds)
  --safe-mode        Disable optional dashboard data sources and destructive actions
  --diagnostic-dir PATH  Log/diagnostic output directory
  --help, -h         Print command help
  --version, -v      Print version
```

Exit codes are `0` success, `1` operating failure, `2` invalid arguments or
settings, `3` hardware discovery, selection, or open refusal including
process-gate failures, `4` hardware-test step or shutdown failure, and `5`
another LCDSirPlus instance or single-instance check failure.

## Check a Logitech G13

The preview verifies rendering without physical hardware. For a G13, first
check HID compatibility without writing:

```powershell
.\LCDSirPlus.exe --hardware-discover
```

Discovery checks at most 256 HID interfaces until the first exact G13 match. It
reports that candidate plus earlier rejection reasons, omits device paths,
performs no writes, and does not inspect competing processes.
Direct HID checks both exact `LCore.exe` and
`logi_lamparray_service.AMD64.exe` before open, after open, and before each
write. It additionally checks `LCore.exe` while idle; LampArray is not polled
while idle. Both processes must be absent, and direct HID fails closed if
process enumeration fails.

Prefer Logitech Gaming Software (LGS). Start it normally, leave
`logitech_backend auto`, and test its SDK connection with:

```powershell
.\LCDSirPlus.exe --hardware-test --backend sdk --duration-secs 60
```

Use direct access only if LGS is not suitable and `--hardware-discover` reports
a compatible interface. Exit LCDSirPlus and LGS normally. A direct test is
refused while `LCore.exe` or `logi_lamparray_service.AMD64.exe` is running, and
it is also refused if another LCDSirPlus process is already running.

If exact `logi_lamparray_service.AMD64.exe` is running, press **Win+R**, run
`services.msc`, and find only **Logitech LampArray Service**. Record its current
status and startup type, stop that exact service for the test, and do not change
its startup type. Do not stop the service when that exact process is not
running, and do not stop any other Logitech or G HUB service. Then run the timed
direct test:

```powershell
.\LCDSirPlus.exe --hardware-test --backend hid --duration-secs 60
```

After the test, restore the recorded prior service state and confirm its startup
type is unchanged.

Watch all ten steps and press all four LCD buttons. Command success means writes
completed; still inspect the physical image. Normal `logitech_backend auto`
selects the SDK when validated exact `LCore.exe` is present and otherwise tries
direct access. Explicit `sdk` and `hid` do not switch to one another.

## Fix common problems

### Settings do not load

Run `.\LCDSirPlus.exe --validate-config`. Fix the named key and line. A final
range or setting combination error can report line `0`. An invalid save leaves
the last valid settings active.

### Preview does not appear

Left-click the tray icon. Run `.\LCDSirPlus.exe --preview`, or set
`preview_mode always` and `start_minimized 0`. In `auto`, a working G13
normally hides the preview.

### PresentMon shows no game data

The planned packages place `PresentMon.exe` beside `LCDSirPlus.exe`. Confirm
that layout, start LCDSirPlus before the game, and select an FPS button option.
Set `presentmon_deferred 0` temporarily to keep the unavailable FPS panel
visible. If capture is denied, add your user to **Performance Log Users**, sign
out, and sign in; use elevation only to diagnose the access problem.

### CPU temperature is N/A or STALE

Repeat the HWiNFO or LibreHardwareMonitor setup above. Use your own exact HWiNFO
labels or LibreHardwareMonitor sensor path. Remember the HWiNFO64 Free 12-hour
limit. Do not trust a stale temperature.

### Discord speakers do not appear

Confirm the Application ID, tester access, active-account authorization, voice
channel, and `discord_show_self` choice. Discord Desktop must run under the same
Windows account. Never share credential files.

## Update or uninstall

When an update is published, exit LCDSirPlus, verify the new setup checksum,
and run setup in the same current-user or all-users mode. Running the same setup
repairs installed files. Remove an opposite-scope installation owned by the
same account before changing scope.

Uninstall from Windows **Installed apps**. The uninstaller removes program
files, shortcuts, and its owned sign-in task. It preserves
`%LOCALAPPDATA%\LCDSirPlus`, including settings, logs, and Discord credentials.
Delete that folder only when its data is no longer needed. Revoke Discord access
separately.

For portable removal, set `start_at_login 0`, run that exact portable copy once,
exit it, then remove its folder.
