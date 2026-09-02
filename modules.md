# LCDSirPlus Button Options

This is the canonical and sole description reference for all 53 button options,
in source order. For setup and editing steps, see the
[instruction manual](docs/INSTRUCTION-MANUAL.md#choose-layouts-and-button-options).
Put these values in `slot_0` through `slot_3`.

HEADSET_BATTERY - Shows battery and connection. What you need: A supported SteelSeries Arctis/GameBuds wireless USB receiver.
CONTROLLER_BATTERY - Shows controller battery or WIRED. What you need: A controller that Windows recognizes as an Xbox (XInput) controller.
FPS_CURRENT - Shows current FPS. What you need: PresentMon, planned for the 0.3.0 package.
FPS_1LOW - Shows common FPS slowdowns. What you need: PresentMon, planned for the 0.3.0 package.
FPS_01LOW - Shows rarer severe FPS slowdowns. What you need: PresentMon, planned for the 0.3.0 package.
FRAME_TIME - Shows frame time and a 30-second graph. What you need: PresentMon, planned for the 0.3.0 package.
CPU_TEMP - Shows CPU temperature. What you need: HWiNFO or LibreHardwareMonitor, installed and set up separately.
GPU_TEMP - Shows GPU temperature. What you need: An NVIDIA/AMD graphics driver, or LibreHardwareMonitor set up separately.
NET_IN - Shows the current incoming rate. What you need: Built into Windows and LCDSirPlus; no extra software.
NET_OUT - Shows the current outgoing rate. What you need: Built into Windows and LCDSirPlus; no extra software.
NET_BOTH - Shows incoming plus outgoing. What you need: Built into Windows and LCDSirPlus; no extra software.
NET_IN_GRAPH - Shows incoming traffic for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software.
NET_OUT_GRAPH - Shows outgoing traffic for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software.
NET_GRAPH - Shows both network directions for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software.
PING - Shows the latest network delay. What you need: The optional network probe, enabled and configured separately.
JITTER - Shows network delay variation. What you need: The optional network probe, enabled and configured separately.
PACKET_LOSS - Shows the percentage of failed checks. What you need: The optional network probe, enabled and configured separately.
MIC_STATUS - Shows LIVE or MUTED. What you need: Built into Windows and LCDSirPlus; no extra software.
AUDIO - Shows default output volume. What you need: Built into Windows and LCDSirPlus; no extra software.
SESSION_TIME - Shows active game-session time. What you need: PresentMon, planned for the 0.3.0 package.
SESSION_SUMMARY - Shows session time and stutters. What you need: PresentMon, planned for the 0.3.0 package.
CLOCK - Shows local time. What you need: Built into Windows and LCDSirPlus; no extra software.
GAME_NAME - Shows the selected game or app filename. What you need: PresentMon, planned for the 0.3.0 package. A filename may appear under either persist setting; set `presentmon_enabled 0` to prevent this exposure.
ALERTS - Shows unacknowledged active alerts or CLEAR. What you need: Built into Windows and LCDSirPlus; no extra software.
PROVIDER_STATUS - Shows unavailable tracked data sources or OK. What you need: Built into Windows and LCDSirPlus; no extra software.
CPU_LOAD - Shows total CPU use. What you need: Built into Windows and LCDSirPlus; no extra software.
RAM_USAGE - Shows memory use and capacity. What you need: Built into Windows and LCDSirPlus; no extra software.
GPU_LOAD - Shows GPU use. What you need: An NVIDIA or AMD graphics driver.
VRAM_USAGE - Shows video memory use and capacity. What you need: An NVIDIA or AMD graphics driver.
CPU_CACHE_TEMP - Shows N/A. What you need: Not available in the draft 0.3.0 build.
CPU_FREQ_TEMP - Shows N/A. What you need: Not available in the draft 0.3.0 build.
CPU_LOAD_GRAPH - Shows CPU use for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software.
GPU_LOAD_GRAPH - Shows GPU use for 30 seconds. What you need: An NVIDIA or AMD graphics driver.
CPU_TEMP_GRAPH - Shows CPU temperature for 30 seconds. What you need: HWiNFO or LibreHardwareMonitor, installed and set up separately.
GPU_TEMP_GRAPH - Shows GPU temperature for 30 seconds. What you need: An NVIDIA/AMD graphics driver, or LibreHardwareMonitor set up separately.
VRM_TEMP - Shows motherboard power-circuit temperature. What you need: LibreHardwareMonitor with the correct VRM temperature selected.
CPU_FAN - Shows CPU fan percentage. What you need: LibreHardwareMonitor with the correct CPU fan selected.
PUMP_RPM - Shows pump percentage. What you need: LibreHardwareMonitor, set up with the matching pump reading selected.
POWER_LIMIT - Shows total system power. What you need: One total-power reading selected in HWiNFO or LibreHardwareMonitor.
CPU_GPU_POWER - Shows CPU plus GPU power. What you need: CPU power from HWiNFO or LibreHardwareMonitor, plus GPU power from the graphics driver, HWiNFO, or LibreHardwareMonitor.
CHIPSET_TEMP - Shows chipset temperature. What you need: LibreHardwareMonitor, set up with the matching sensor selected.
MOTHERBOARD_TEMP - Shows motherboard temperature. What you need: LibreHardwareMonitor, set up with the matching sensor selected.
DISK_IO - Shows total disk read and write rates. What you need: Built into Windows and LCDSirPlus; no extra software.
DISK_IO_GRAPH - Shows disk activity for 30 seconds. What you need: Built into Windows and LCDSirPlus; no extra software.
RAM_DETAIL - Shows used and total memory. What you need: Built into Windows and LCDSirPlus; no extra software.
FPS_GRAPH - Shows FPS for 30 seconds. What you need: PresentMon, planned for the 0.3.0 package.
THERMALS - Shows CPU and GPU temperatures. What you need: Both CPU and GPU temperature sources set up and current.
CONNECTIONS - Shows established IPv4 and IPv6 TCP connections only. What you need: Built into Windows and LCDSirPlus; no extra software.
NET_HEALTH - Shows ping, jitter, and packet loss. What you need: The optional network probe, enabled and configured separately.
SYSTEM_BATTERY - Shows AC, battery, and charging. What you need: Built into Windows and LCDSirPlus; no extra software.
HARD_FAULTS - Shows memory-to-disk pressure. What you need: Built into Windows and LCDSirPlus; no extra software.
BOTTLENECK - Shows CPU, GPU, RAM, or disk. With current NONE, the next eligible option appears temporarily or the slot shows CLEAR. What you need: Built-in system readings; GPU results also need a supported graphics driver.
PROC_HANG - Shows a hung-window target. With no target, including while disabled, the next eligible option appears temporarily or the slot shows CLEAR. What you need: Not supported for end users in this draft release; leave `hang_enabled 0`.
