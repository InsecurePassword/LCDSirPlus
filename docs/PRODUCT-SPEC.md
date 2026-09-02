# LCDSirPlus 0.3.0 Product Specification

Status: **Draft / Unreleased**. Existing b7 downloads are older. Final
downloadable package testing remains pending.

Audience: maintainers deciding whether source and package behavior satisfy the
0.3.0 contract. See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation
ownership, [REFERENCE-LAYOUT.md](REFERENCE-LAYOUT.md) for fixed geometry, and
[HARDWARE-ACCEPTANCE.md](HARDWARE-ACCEPTANCE.md) for unpublished release gates.
User procedures remain in [INSTRUCTION-MANUAL.md](INSTRUCTION-MANUAL.md).

## Product purpose

LCDSirPlus is a Windows 11 x64 dashboard for the Logitech G13 160x43 monochrome
LCD. It keeps useful system, game, device, network, and voice information visible
while another display is occupied. The preview provides the same dashboard
without a G13.

The product runs as a standard user. It installs no driver or service and opens
no incoming network listener.

## User-visible layout

Every layout has a compact Windows-local date/time header, a fixed main reading
area, and four button-aligned lower slots.

- Layout 1: CPU/RAM and GPU/VRAM halves.
- Layout 2: CPU/RAM, GPU/VRAM, and outgoing/incoming thirds.
- Layout 3: CPU/RAM and incoming/outgoing halves.

`main_display` accepts `1..3`, defaults to `1`, and applies after a valid settings
save. Network bars in layouts 2 and 3 show current throughput and use
`network_graph_ceiling_mbps`. One CPU domain uses one full-height CPU bar. Two
domains use Cache and Frequency bars. Manual CCD topology requires both
processor lists to be automatic or both to be explicit, disjoint `0..63` lists.

The button registry contains exactly 53 values in `src/config.rs` order. Their
descriptions appear in the canonical [modules.md](../modules.md) quick reference
and are mirrored exactly in the instruction manual/PDF. Graphs cover the trailing
30 seconds. There is no layout editor or plug-in system.

## Button behavior

- Physical button 1 through 4 cycles its matching slot forward.
- Preview left-click cycles forward; right-click cycles backward.
- A changed slot list keeps the selected value when it still exists.
- Button 4 acknowledges the highest active alert before ordinary cycling.
- Safe mode keeps harmless cycling and acknowledgement available.

The stored selection is not always the value rendered. Dynamic substitution in
the shared slot resolver temporarily shows the next eligible configured value
without changing slot state when:

- selected `PROC_HANG` has no target;
- selected `BOTTLENECK` reports current `NONE`;
- `presentmon_deferred 1` and the selected PresentMon value is unavailable.

The resolver skips inactive `PROC_HANG` and `BOTTLENECK`. PresentMon deferral
also skips other unavailable PresentMon values. No eligible value shows `CLEAR`.
With `presentmon_deferred 0`, the selected PresentMon pane remains visible with
its native inactive message.

## G13 operation

Connection modes are `auto`, `sdk`, `hid`, and `virtual`. `auto` selects the
trusted Logitech LCD SDK when validated exact `LCore.exe` is present; otherwise,
it attempts direct HID. Direct HID requires both exact `LCore.exe` and
`logi_lamparray_service.AMD64.exe` to be absent and fails closed if process
enumeration fails. Explicit physical modes do not switch to one another.
LCDSirPlus never stops Logitech software.

Hardware discovery scans at most 256 HID interfaces until the first exact G13
match. It reports that candidate plus preceding rejection reasons, omits device
paths, performs no writes, and does not inspect competing processes. Discovery
cannot establish ownership.

Device loss triggers reconnect with a configured capped delay. Unchanged frames
are not resubmitted. Only one normal or direct-HID runtime can run in a Windows
session.

## Reading sources

Windows supplies date/time, CPU load, memory, network throughput, audio, disk,
connections, system battery, and page-read pressure. XInput supplies controller
battery. Installed NVIDIA or AMD drivers supply GPU load, video memory,
temperature, and available power values.

HWiNFO and LibreHardwareMonitor are optional for readings without a suitable
Windows or driver source. Both remain user-managed. HWiNFO is read only through
exact label pairs. LibreHardwareMonitor is local-only and uses exact or limited
automatic sensor selection. LCDSirPlus does not read raw MSR, SMBus, EC, or
Super-I/O registers.

Headset battery support is limited to exact documented SteelSeries
Arctis/GameBuds wireless USB receiver profiles. Bluetooth-only, wired, unknown,
or name-only matches are unsupported.

Network quality probing is off by default. When enabled, it sends ICMP or TCP to
one configured IP literal. Safe mode sends no probe traffic.

`PROVIDER_STATUS` reports tracked normal-dashboard sources that may be disabled,
unconfigured, disconnected, or otherwise not ready as `DOWN`; `DOWN` is not
necessarily a failure. A draft default setup can therefore show Discord and the
disabled hung action as down. In safe mode it summarizes only the safe-mode
dashboard source set and reports that healthy restricted state as `OK`.

## PresentMon behavior

PresentMon is enabled by default and is included in release packages. It supplies
frame and game-session values while capture is active. Game selection is
automatic and requires no user-maintained game list.

`presentmon_deferred` defaults on, so an unavailable FPS screen can temporarily
show another configured option without changing the saved selection.
`presentmon_persist` defaults off as the normal game preference. When a valid
NVIDIA App game list is available, ordinary desktop apps are rejected. If the
list is unavailable, automatic fallback may still select another app. Enabling
persist allows desktop apps even when the game list is available. The selected
filename and FPS may appear, and old readings are not preserved. Disabling
PresentMon prevents filename and FPS exposure.

Values become stale after five seconds without frames and expire after another
five seconds. Advanced selection and privacy rules are documented in
[CONFIGURATION.md](CONFIGURATION.md), [SECURITY.md](../SECURITY.md), and
[ARCHITECTURE.md](ARCHITECTURE.md).

## Discord behavior

Discord Desktop voice state is optional. Each user supplies a developer
application and authorizes each Discord account. The numeric client ID is public.
The generic OAuth exchange requires a client secret supplied only through the
temporary masked environment workflow.

Credentials are separated by immutable Discord user ID and protected for the
current Windows account. The local Discord process, user, session,
executable, signature, and publisher are checked before use. Authorization opens
no browser or callback listener. LCDSirPlus reads voice state only and publishes
no Rich Presence.

## Alerts and safe mode

Current CPU/GPU temperature, RAM/VRAM use, and headset battery can create alert
episodes. Temperature and memory categories are independent and enabled by
default. Stale or unavailable readings do not create an alert. Warning recovery
is immediate; critical recovery uses `critical_alert_linger_ms`.

`warning 1` flashes selected temperature panes and suppresses full-screen
temperature alerts. `warning 0` keeps those full-screen alerts. Memory and
headset alerts are unchanged.

Safe mode restricts the dashboard snapshot to Windows-local date/time, native
CPU and RAM, the fixed layout shell, and the safe-mode health marker. It starts
no optional data-source, Discord, probe, startup-change, hung-target, or
termination work. Preview, slot cycling, and acknowledgement remain available.

## Hung-window behavior

Detection and destructive action default off. When enabled, only repeatedly
unresponsive visible top-level windows outside fixed/configured exclusions can
be offered. The detector cannot terminate a process.

The selected `PROC_HANG` slot owns the physical button. Button-down binds the
exact displayed target. A short release changes detail or target. A continuous
hold requests termination at `hang_hold_ms` after all checks repeat. Recovery,
selection or identity change, device loss, settings change, disable, or safe mode
cancels the press. The action never elevates and can lose unsaved work.

Physical-button testing with a disposable child remains pending.

## Configuration, diagnostics, and install

Valid settings saves replace runtime settings. Invalid saves leave the last
valid version active. Installed configuration is under
`%LOCALAPPDATA%\LCDSirPlus\Config`; portable/development configuration is beside
the executable. Explicit `--config` wins.

Diagnostics is offline, capped at 1 MiB, and built from a fixed allowlist. It
reads no configuration and starts no hardware, optional source, network, Discord,
or action work. Private content and identifiers are excluded.

Setup supports current-user and all-users scope. Installed startup uses an owned
per-user sign-in task. Portable startup uses only its exact current-user Run
value. Update and repair preserve LocalAppData. Uninstall removes installed
files, shortcuts, and the owned task while preserving configuration, logs, and
credentials.

## Non-goals

- G19 color LCD support.
- Display hooking or kernel sensor drivers.
- Remote monitoring or control.
- Arbitrary scripts, plug-ins, or third-party button modules.
- A visual layout editor.
- A user-maintained game list.
