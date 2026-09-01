//! LCDSirPlus v2 configuration: schema, defaults, validation.
//!
//! LCDSirReal-style text format: `#` comments, whitespace-separated
//! key/value lines, ordered `slot_0`..`slot_3` module lists, `include`
//! override files, hot reload with last-valid-state preservation.
//!
//! Selectors generally default to `auto`; `network_probe_method` accepts
//! `auto` but intentionally defaults to `icmp`. The v2 schema removes the
//! retired Process Lasso and Logitech SDK keys.

use std::time::Duration;

pub const MAX_INCLUDES: usize = 8;
pub const MAX_CONFIG_AGGREGATE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONFIG_LINES: usize = 16384;
pub const MAX_CONFIG_FILES: usize = 64;
pub const MAX_CONFIG_INCLUDE_DIRECTIVES: usize = 64;
pub const MAX_CONFIG_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_CONFIG_STRING_BYTES: usize = 4 * 1024;
pub const MAX_CONFIG_LIST_ITEMS: usize = 256;

/// The four button-aligned slot defaults, byte-identical to the shipped
/// LCDSirPlus portable configuration.
pub const DEFAULT_SLOTS: [&[&str]; 4] = [
    &["HEADSET_BATTERY", "CPU_TEMP", "CONTROLLER_BATTERY"],
    &["FPS_CURRENT", "FPS_1LOW", "FRAME_TIME", "SESSION_TIME"],
    &["PROC_HANG", "GPU_TEMP", "PING", "JITTER", "AUDIO"],
    &[
        "THERMALS",
        "PACKET_LOSS",
        "MIC_STATUS",
        "SESSION_SUMMARY",
        "PROVIDER_STATUS",
    ],
];

pub const MODULES: &[&str] = &[
    "HEADSET_BATTERY",
    "CONTROLLER_BATTERY",
    "FPS_CURRENT",
    "FPS_1LOW",
    "FPS_01LOW",
    "FRAME_TIME",
    "CPU_TEMP",
    "GPU_TEMP",
    "NET_IN",
    "NET_OUT",
    "NET_BOTH",
    "NET_IN_GRAPH",
    "NET_OUT_GRAPH",
    "NET_GRAPH",
    "PING",
    "JITTER",
    "PACKET_LOSS",
    "MIC_STATUS",
    "AUDIO",
    "SESSION_TIME",
    "SESSION_SUMMARY",
    "CLOCK",
    "GAME_NAME",
    "ALERTS",
    "PROVIDER_STATUS",
    "CPU_LOAD",
    "RAM_USAGE",
    "GPU_LOAD",
    "VRAM_USAGE",
    "CPU_CACHE_TEMP",
    "CPU_FREQ_TEMP",
    "CPU_LOAD_GRAPH",
    "GPU_LOAD_GRAPH",
    "CPU_TEMP_GRAPH",
    "GPU_TEMP_GRAPH",
    "VRM_TEMP",
    "CPU_FAN",
    "PUMP_RPM",
    "POWER_LIMIT",
    "CPU_GPU_POWER",
    "CHIPSET_TEMP",
    "MOTHERBOARD_TEMP",
    "DISK_IO",
    "DISK_IO_GRAPH",
    "RAM_DETAIL",
    "FPS_GRAPH",
    "THERMALS",
    "CONNECTIONS",
    "NET_HEALTH",
    "SYSTEM_BATTERY",
    "HARD_FAULTS",
    "BOTTLENECK",
    "PROC_HANG",
];

pub fn valid_module(s: &str) -> bool {
    let upper = s.to_uppercase();
    MODULES.contains(&upper.as_str())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub path: String,

    pub config_refresh: Duration,
    pub telemetry_interval: Duration,
    pub render_interval: Duration,
    pub main_display: i32,
    pub preview_mode: String,
    pub preview_scale: i32,
    pub start_minimized: bool,
    pub start_at_login: bool,
    pub safe_mode: bool,

    pub date_format: String,
    pub time_format: String,

    pub slots: [Vec<String>; 4],

    // Cache/Frequency CCD mapping (native topology; Process Lasso removed).
    pub ccd_source: String,
    pub ccd_cache_processors: Option<Vec<u32>>,
    pub ccd_frequency_processors: Option<Vec<u32>>,

    // Logitech G13 backend. Auto arbitrates between LCore's SDK and direct HID.
    pub logitech_backend: String,
    pub logitech_reconnect: Duration,
    pub logitech_reconnect_max: Duration,
    pub logitech_button_poll: Duration,
    pub logitech_button_debounce: Duration,
    pub logitech_friendly_name: String,
    pub logitech_orientation: String,
    pub logitech_invert: bool,

    // LibreHardwareMonitor: demoted to an optional temps-only fallback.
    pub lhm_mode: String,
    pub lhm_url: String,
    pub lhm_interval: Duration,
    pub lhm_stale_after: Duration,
    pub lhm_sensors: std::collections::BTreeMap<String, String>,
    pub cpu_fan_max_rpm: u32,
    pub pump_max_rpm: u32,

    pub hwinfo_cpu_temp_sensor: String,
    pub hwinfo_cpu_temp_reading: String,
    pub hwinfo_total_power_sensor: String,
    pub hwinfo_total_power_reading: String,
    pub hwinfo_cpu_power_sensor: String,
    pub hwinfo_cpu_power_reading: String,
    pub hwinfo_gpu_power_sensor: String,
    pub hwinfo_gpu_power_reading: String,
    pub hwinfo_stale_after: Duration,

    // GPU provider selection: native NVAPI -> ADLX in auto.
    pub gpu_provider: String,

    pub presentmon_enabled: bool,
    pub presentmon_path: String,
    pub presentmon_interval: Duration,
    pub presentmon_window: Duration,
    pub presentmon_target_mode: String,
    pub presentmon_deferred: bool,
    pub presentmon_persist: bool,
    pub presentmon_process_name: String,
    pub presentmon_exclude: Vec<String>,
    pub stutter_threshold_ms: f64,

    pub headset_enabled: bool,
    pub headset_poll: Duration,
    pub headset_query_timeout: Duration,
    pub headset_stale_after: Duration,
    pub headset_warn_percent: i32,
    pub headset_critical_percent: i32,
    pub headset_estimate_hours: bool,

    pub controller_enabled: bool,
    pub controller_index: i32,
    pub controller_poll: Duration,

    pub network_probe_enabled: bool,
    pub network_probe_method: String,
    pub network_probe_target: String,
    pub network_probe_interval: Duration,
    pub network_probe_timeout: Duration,
    pub network_probe_window: i32,
    pub network_graph_ceiling_mbps: f64,
    pub disk_graph_ceiling_mbps: f64,
    pub fps_graph_ceiling: f64,

    pub warning: bool,
    pub memory_warning_enabled: bool,
    pub temperature_warning_enabled: bool,
    pub cpu_temp_max_c: f64,
    pub gpu_temp_max_c: f64,

    pub bottleneck_cpu_percent: f64,
    pub bottleneck_gpu_percent: f64,
    pub bottleneck_memory_percent: f64,
    pub bottleneck_disk_mbps: f64,
    pub bottleneck_sustain: Duration,

    pub audio_enabled: bool,
    pub audio_poll: Duration,

    pub discord_enabled: bool,
    pub discord_client_id: String,
    // Legacy configuration key accepted for compatibility; Discord RPC OAuth ignores it.
    pub discord_redirect_uri: String,
    pub discord_linger: Duration,
    pub discord_max_speakers: i32,
    pub discord_show_self: bool,
    pub discord_show_channel: bool,

    pub hang_enabled: bool,
    // Legacy persisted binding; PROC_HANG slot/button behavior is resolved separately.
    pub hang_button: i32,
    pub hang_hold: Duration,
    pub hang_probe_interval: Duration,
    pub hang_probe_timeout: Duration,
    pub hang_failures: i32,
    pub hang_minimum: Duration,
    pub hang_ignore: Vec<String>,

    pub cpu_temp_warning: f64,
    pub cpu_temp_critical: f64,
    pub gpu_temp_warning: f64,
    pub gpu_temp_critical: f64,
    pub memory_warning: f64,
    pub vmem_warning: f64,
    pub critical_alert_linger: Duration,

    pub log_level: String,
    pub log_max_bytes: u64,
    pub log_backups: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            path: String::new(),
            config_refresh: Duration::from_secs(1),
            telemetry_interval: Duration::from_millis(300),
            render_interval: Duration::from_millis(100),
            main_display: 1,
            preview_mode: "auto".into(),
            preview_scale: 4,
            start_minimized: false,
            start_at_login: false,
            safe_mode: false,
            date_format: "yyyy-MM-dd dddd".into(),
            time_format: "HH:mm:ss".into(),
            slots: [
                DEFAULT_SLOTS[0].iter().map(|s| s.to_string()).collect(),
                DEFAULT_SLOTS[1].iter().map(|s| s.to_string()).collect(),
                DEFAULT_SLOTS[2].iter().map(|s| s.to_string()).collect(),
                DEFAULT_SLOTS[3].iter().map(|s| s.to_string()).collect(),
            ],
            ccd_source: "auto".into(),
            ccd_cache_processors: None,
            ccd_frequency_processors: None,
            logitech_backend: "auto".into(),
            logitech_reconnect: Duration::from_secs(5),
            logitech_reconnect_max: Duration::from_secs(60),
            logitech_button_poll: Duration::from_millis(50),
            logitech_button_debounce: Duration::from_millis(40),
            logitech_friendly_name: "LCDSirPlus".into(),
            logitech_orientation: "normal".into(),
            logitech_invert: false,
            lhm_mode: "auto".into(),
            lhm_url: "auto".into(),
            lhm_interval: Duration::from_millis(300),
            lhm_stale_after: Duration::from_secs(3),
            lhm_sensors: Default::default(),
            cpu_fan_max_rpm: 0,
            pump_max_rpm: 0,
            hwinfo_cpu_temp_sensor: String::new(),
            hwinfo_cpu_temp_reading: String::new(),
            hwinfo_total_power_sensor: String::new(),
            hwinfo_total_power_reading: String::new(),
            hwinfo_cpu_power_sensor: String::new(),
            hwinfo_cpu_power_reading: String::new(),
            hwinfo_gpu_power_sensor: String::new(),
            hwinfo_gpu_power_reading: String::new(),
            hwinfo_stale_after: Duration::from_secs(5),
            gpu_provider: "auto".into(),
            presentmon_enabled: true,
            presentmon_path: "auto".into(),
            presentmon_interval: Duration::from_secs(1),
            presentmon_window: Duration::from_secs(60),
            presentmon_target_mode: "presenting".into(),
            presentmon_deferred: true,
            presentmon_persist: false,
            presentmon_process_name: String::new(),
            presentmon_exclude: [
                "dwm.exe",
                "explorer.exe",
                "applicationframehost.exe",
                "textinputhost.exe",
                "searchhost.exe",
                "lcdsirplus.exe",
                "lcdsirplus.console.exe",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            stutter_threshold_ms: 33.34,
            headset_enabled: true,
            headset_poll: Duration::from_secs(15),
            headset_query_timeout: Duration::from_millis(1200),
            headset_stale_after: Duration::from_secs(45),
            headset_warn_percent: 25,
            headset_critical_percent: 0,
            headset_estimate_hours: true,
            controller_enabled: true,
            controller_index: -1,
            controller_poll: Duration::from_secs(10),
            network_probe_enabled: false,
            network_probe_method: "icmp".into(),
            network_probe_target: "1.1.1.1".into(),
            network_probe_interval: Duration::from_secs(1),
            network_probe_timeout: Duration::from_millis(1500),
            network_probe_window: 30,
            network_graph_ceiling_mbps: 1000.0,
            disk_graph_ceiling_mbps: 1000.0,
            fps_graph_ceiling: 240.0,
            warning: true,
            memory_warning_enabled: true,
            temperature_warning_enabled: true,
            cpu_temp_max_c: 90.0,
            gpu_temp_max_c: 90.0,
            bottleneck_cpu_percent: 90.0,
            bottleneck_gpu_percent: 95.0,
            bottleneck_memory_percent: 90.0,
            bottleneck_disk_mbps: 500.0,
            bottleneck_sustain: Duration::from_secs(2),
            audio_enabled: true,
            audio_poll: Duration::from_secs(1),
            discord_enabled: true,
            discord_client_id: String::new(),
            discord_redirect_uri: "http://127.0.0.1".into(),
            discord_linger: Duration::from_millis(700),
            discord_max_speakers: 2,
            discord_show_self: false,
            discord_show_channel: false,
            hang_enabled: false,
            hang_button: 3,
            hang_hold: Duration::from_secs(2),
            hang_probe_interval: Duration::from_secs(2),
            hang_probe_timeout: Duration::from_millis(350),
            hang_failures: 3,
            hang_minimum: Duration::from_secs(6),
            hang_ignore: [
                "lcdsirplus.exe",
                "explorer.exe",
                "dwm.exe",
                "winlogon.exe",
                "csrss.exe",
                "services.exe",
                "lsass.exe",
                "smss.exe",
                "fontdrvhost.exe",
                "sihost.exe",
                "taskhostw.exe",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            cpu_temp_warning: 85.0,
            cpu_temp_critical: 95.0,
            gpu_temp_warning: 83.0,
            gpu_temp_critical: 90.0,
            memory_warning: 100.0,
            vmem_warning: 100.0,
            critical_alert_linger: Duration::from_secs(3),
            log_level: "info".into(),
            log_max_bytes: 2 * 1024 * 1024,
            log_backups: 3,
        }
    }
}

pub fn validate_config_string(name: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_CONFIG_STRING_BYTES {
        return Err(format!(
            "{} exceeds limit of {} bytes",
            name, MAX_CONFIG_STRING_BYTES
        ));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(format!("{} contains a control character", name));
    }
    Ok(())
}

fn check_range_u64(name: &str, v: u64, lo: u64, hi: u64) -> Result<(), String> {
    if v < lo || v > hi {
        Err(format!("{} must be {}..{}", name, lo, hi))
    } else {
        Ok(())
    }
}

fn check_range_f64(name: &str, v: f64, lo: f64, hi: f64) -> Result<(), String> {
    if !v.is_finite() || v < lo || v > hi {
        Err(format!("{} must be {}..{}", name, lo, hi))
    } else {
        Ok(())
    }
}

pub fn validate(c: &Config) -> Result<(), String> {
    check_range_u64(
        "config_refresh_ms",
        c.config_refresh.as_millis() as u64,
        100,
        60000,
    )?;
    check_range_u64(
        "telemetry_interval_ms",
        c.telemetry_interval.as_millis() as u64,
        100,
        10000,
    )?;
    check_range_u64(
        "render_interval_ms",
        c.render_interval.as_millis() as u64,
        25,
        5000,
    )?;
    if c.warning && c.temperature_warning_enabled && c.render_interval > Duration::from_millis(100)
    {
        return Err(
            "render_interval_ms must be <=100 when warning=1 and temperature_warning_enabled=1"
                .into(),
        );
    }
    check_range_i32("main_display", c.main_display, 1, 3)?;
    if !matches!(c.preview_mode.as_str(), "auto" | "always" | "never") {
        return Err("preview_mode must be auto, always, or never".into());
    }
    if !(1..=10).contains(&c.preview_scale) {
        return Err("preview_scale must be 1..10".into());
    }
    if c.date_format.is_empty() || c.time_format.is_empty() {
        return Err("date_format and time_format cannot be empty".into());
    }
    if !matches!(c.ccd_source.as_str(), "auto" | "manual") {
        return Err("ccd_source must be auto or manual".into());
    }
    if let (Some(cache), Some(freq)) = (&c.ccd_cache_processors, &ccd_frequency(c)) {
        if cache.iter().any(|p| freq.contains(p)) {
            return Err("ccd_cache_processors and ccd_frequency_processors overlap".into());
        }
    }
    if !matches!(
        c.logitech_backend.as_str(),
        "auto" | "sdk" | "hid" | "virtual"
    ) {
        return Err("logitech_backend must be auto, sdk, hid, or virtual".into());
    }
    check_range_u64(
        "logitech_reconnect_ms",
        c.logitech_reconnect.as_millis() as u64,
        250,
        300000,
    )?;
    if c.logitech_reconnect_max < c.logitech_reconnect
        || c.logitech_reconnect_max > Duration::from_secs(600)
    {
        return Err(
            "logitech_reconnect_max_ms must be >= logitech_reconnect_ms and <= 600000".into(),
        );
    }
    check_range_u64(
        "logitech_button_poll_ms",
        c.logitech_button_poll.as_millis() as u64,
        10,
        1000,
    )?;
    check_range_u64(
        "logitech_button_debounce_ms",
        c.logitech_button_debounce.as_millis() as u64,
        10,
        500,
    )?;
    if c.logitech_friendly_name.trim().is_empty() || c.logitech_friendly_name.contains('\0') {
        return Err("logitech_friendly_name cannot be empty or contain NUL".into());
    }
    if !matches!(
        c.logitech_orientation.as_str(),
        "normal" | "flip_x" | "flip_y" | "rotate_180"
    ) {
        return Err("logitech_orientation must be normal, flip_x, flip_y, or rotate_180".into());
    }
    if !matches!(c.lhm_mode.as_str(), "auto" | "on" | "off") {
        return Err("lhm_mode must be auto, on, or off".into());
    }
    if c.lhm_url != "auto" {
        validate_lhm_url(&c.lhm_url)?;
    }
    check_range_u64(
        "lhm_interval_ms",
        c.lhm_interval.as_millis() as u64,
        100,
        60000,
    )?;
    if c.lhm_stale_after < c.lhm_interval || c.lhm_stale_after > Duration::from_secs(300) {
        return Err("lhm_stale_ms must be >= lhm_interval_ms and <= 300000".into());
    }
    if c.cpu_fan_max_rpm > 30000 || c.pump_max_rpm > 30000 {
        return Err("cpu_fan_max_rpm and pump_max_rpm must be 0..30000".into());
    }
    for (sensor, reading, name) in [
        (
            &c.hwinfo_cpu_temp_sensor,
            &c.hwinfo_cpu_temp_reading,
            "cpu_temp",
        ),
        (
            &c.hwinfo_total_power_sensor,
            &c.hwinfo_total_power_reading,
            "total_power",
        ),
        (
            &c.hwinfo_cpu_power_sensor,
            &c.hwinfo_cpu_power_reading,
            "cpu_power",
        ),
        (
            &c.hwinfo_gpu_power_sensor,
            &c.hwinfo_gpu_power_reading,
            "gpu_power",
        ),
    ] {
        if sensor.is_empty() != reading.is_empty() {
            return Err(format!(
                "hwinfo_{name}_sensor and hwinfo_{name}_reading must both be blank or both nonblank"
            ));
        }
    }
    check_range_u64(
        "hwinfo_stale_ms",
        c.hwinfo_stale_after.as_millis() as u64,
        1000,
        60000,
    )?;
    if !matches!(c.gpu_provider.as_str(), "auto" | "nvapi" | "adlx" | "off") {
        return Err("gpu_provider must be auto, nvapi, adlx, or off".into());
    }
    if !matches!(
        c.presentmon_target_mode.as_str(),
        "presenting" | "foreground" | "process_name" | "disabled"
    ) {
        return Err(
            "presentmon_target_mode must be presenting, foreground, process_name, or disabled"
                .into(),
        );
    }
    if c.presentmon_target_mode == "process_name" && c.presentmon_process_name.is_empty() {
        return Err("presentmon_process_name is required in process_name mode".into());
    }
    check_range_u64(
        "presentmon_interval_ms",
        c.presentmon_interval.as_millis() as u64,
        100,
        10000,
    )?;
    check_range_u64(
        "presentmon_window_ms",
        c.presentmon_window.as_millis() as u64,
        5000,
        600000,
    )?;
    check_range_f64("stutter_threshold_ms", c.stutter_threshold_ms, 1.0, 1000.0)?;
    check_range_i32("headset_warn_percent", c.headset_warn_percent, 0, 100)?;
    check_range_i32(
        "headset_critical_percent",
        c.headset_critical_percent,
        0,
        100,
    )?;
    if c.headset_critical_percent > c.headset_warn_percent {
        return Err("headset_critical_percent must be <= headset_warn_percent".into());
    }
    check_range_u64(
        "headset_poll_ms",
        c.headset_poll.as_millis() as u64,
        1000,
        600000,
    )?;
    check_range_u64(
        "headset_query_timeout_ms",
        c.headset_query_timeout.as_millis() as u64,
        100,
        10000,
    )?;
    if c.headset_stale_after < c.headset_poll || c.headset_stale_after > Duration::from_secs(3600) {
        return Err("headset_stale_ms must be >= headset_poll_ms and <= 3600000".into());
    }
    check_range_i32("controller_index", c.controller_index, -1, 3)?;
    check_range_u64(
        "controller_poll_ms",
        c.controller_poll.as_millis() as u64,
        1000,
        600000,
    )?;
    check_range_u64("audio_poll_ms", c.audio_poll.as_millis() as u64, 100, 60000)?;
    if !matches!(c.network_probe_method.as_str(), "auto" | "icmp" | "tcp") {
        return Err("network_probe_method must be auto, icmp, or tcp".into());
    }
    if c.network_probe_enabled {
        if c.network_probe_target.trim().is_empty() {
            return Err("network_probe_target cannot be empty".into());
        }
        let (address, port) = parse_network_target(&c.network_probe_target)?;
        if c.network_probe_method == "icmp" && (!address.is_ipv4() || port.is_some()) {
            return Err(
                "network_probe_target must be an IPv4 literal without a port for icmp".into(),
            );
        }
        check_range_i32("network_probe_window", c.network_probe_window, 5, 600)?;
    }
    check_range_u64(
        "network_probe_interval_ms",
        c.network_probe_interval.as_millis() as u64,
        250,
        60000,
    )?;
    check_range_u64(
        "network_probe_timeout_ms",
        c.network_probe_timeout.as_millis() as u64,
        50,
        30000,
    )?;
    check_range_f64(
        "network_graph_ceiling_mbps",
        c.network_graph_ceiling_mbps,
        1.0,
        100000.0,
    )?;
    check_range_f64(
        "disk_graph_ceiling_mbps",
        c.disk_graph_ceiling_mbps,
        1.0,
        100000.0,
    )?;
    check_range_f64("fps_graph_ceiling", c.fps_graph_ceiling, 1.0, 1000.0)?;
    check_range_f64("cpu_temp_max_c", c.cpu_temp_max_c, 1.0, 150.0)?;
    check_range_f64("gpu_temp_max_c", c.gpu_temp_max_c, 1.0, 150.0)?;
    check_range_f64(
        "bottleneck_cpu_percent",
        c.bottleneck_cpu_percent,
        1.0,
        100.0,
    )?;
    check_range_f64(
        "bottleneck_gpu_percent",
        c.bottleneck_gpu_percent,
        1.0,
        100.0,
    )?;
    check_range_f64(
        "bottleneck_memory_percent",
        c.bottleneck_memory_percent,
        1.0,
        100.0,
    )?;
    check_range_f64(
        "bottleneck_disk_mbps",
        c.bottleneck_disk_mbps,
        1.0,
        100000.0,
    )?;
    check_range_u64(
        "bottleneck_sustain_ms",
        c.bottleneck_sustain.as_millis() as u64,
        0,
        60000,
    )?;
    for (name, v) in [
        ("cpu_temp_warning", c.cpu_temp_warning),
        ("cpu_temp_critical", c.cpu_temp_critical),
        ("gpu_temp_warning", c.gpu_temp_warning),
        ("gpu_temp_critical", c.gpu_temp_critical),
    ] {
        check_range_f64(name, v, 0.0, 150.0)?;
    }
    for (name, v) in [
        ("memory_warning", c.memory_warning),
        ("vmem_warning", c.vmem_warning),
    ] {
        check_range_f64(name, v, 0.0, 100.0)?;
    }
    if c.cpu_temp_critical < c.cpu_temp_warning || c.gpu_temp_critical < c.gpu_temp_warning {
        return Err("critical temperature must be >= warning temperature".into());
    }
    if c.critical_alert_linger > Duration::from_secs(60) {
        return Err("critical_alert_linger_ms must be 0..60000".into());
    }
    if c.discord_max_speakers < 1 || c.discord_max_speakers > 4 {
        return Err("discord_max_speakers must be 1..4".into());
    }
    if c.discord_linger > Duration::from_secs(10) {
        return Err("discord_linger_ms must be 0..10000".into());
    }
    if !c.discord_client_id.is_empty() && !c.discord_client_id.chars().all(|r| r.is_ascii_digit()) {
        return Err("discord_client_id must contain digits only".into());
    }
    check_range_i32("hang_button", c.hang_button, 1, 4)?;
    check_range_u64("hang_hold_ms", c.hang_hold.as_millis() as u64, 1000, 10000)?;
    check_range_u64(
        "hang_probe_interval_ms",
        c.hang_probe_interval.as_millis() as u64,
        250,
        60000,
    )?;
    check_range_u64(
        "hang_probe_timeout_ms",
        c.hang_probe_timeout.as_millis() as u64,
        10,
        5000,
    )?;
    if c.hang_probe_timeout >= c.hang_probe_interval {
        return Err("hang_probe_timeout_ms must be less than hang_probe_interval_ms".into());
    }
    if c.hang_failures < 2 || c.hang_failures > 10 {
        return Err("hang_failures_required must be 2..10".into());
    }
    check_range_u64(
        "hang_minimum_ms",
        c.hang_minimum.as_millis() as u64,
        1000,
        60000,
    )?;
    for i in 0..4 {
        if c.slots[i].is_empty() {
            return Err(format!("slot_{} must contain at least one module", i));
        }
        for module in &c.slots[i] {
            if !valid_module(module) {
                return Err(format!("slot_{} contains unknown module {:?}", i, module));
            }
        }
    }
    let proc_hang_count = c
        .slots
        .iter()
        .flatten()
        .filter(|module| module.eq_ignore_ascii_case("PROC_HANG"))
        .count();
    if proc_hang_count > 1 {
        return Err("PROC_HANG may occur only once across all slots".into());
    }
    if !matches!(c.log_level.as_str(), "debug" | "info" | "warn" | "error") {
        return Err("log_level must be debug, info, warn, or error".into());
    }
    check_range_u64("log_max_bytes", c.log_max_bytes, 65536, 104857600)?;
    if c.log_backups < 1 || c.log_backups > 20 {
        return Err("log_backups must be 1..20".into());
    }
    Ok(())
}

pub(crate) fn parse_network_target(
    target: &str,
) -> Result<(std::net::IpAddr, Option<u16>), String> {
    if target != target.trim() {
        return Err("network_probe_target must be one IP literal with no whitespace".into());
    }
    if let Ok(address) = target.parse::<std::net::SocketAddr>() {
        if address.port() == 0 {
            return Err("network_probe_target port must be 1..65535".into());
        }
        return Ok((address.ip(), Some(address.port())));
    }
    target
        .parse::<std::net::IpAddr>()
        .map(|address| (address, None))
        .map_err(|_| {
            "network_probe_target must be an IP literal, optionally with a TCP port".into()
        })
}

fn ccd_frequency(c: &Config) -> &Option<Vec<u32>> {
    &c.ccd_frequency_processors
}

fn check_range_i32(name: &str, v: i32, lo: i32, hi: i32) -> Result<(), String> {
    if v < lo || v > hi {
        Err(format!("{} must be {}..{}", name, lo, hi))
    } else {
        Ok(())
    }
}

fn validate_lhm_url(url: &str) -> Result<(), String> {
    // The runtime client supports loopback HTTP only.
    let rest = url.strip_prefix("http://").ok_or("lhm_url must use http")?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.contains('@') {
        return Err("lhm_url must not contain credentials".into());
    }
    if url.contains('?') {
        return Err("lhm_url must not contain a query".into());
    }
    if url.contains('#') {
        return Err("lhm_url must not contain a fragment".into());
    }
    let parsed = crate::http::parse_url(url)?;
    if !crate::http::is_loopback_host(&parsed.host) {
        return Err("lhm_url must be loopback".into());
    }
    Ok(())
}

/// Parse a logical-processor list: `0,1,2` / `0-15` / `auto` (empty = auto).
pub fn parse_processor_list(v: &str) -> Result<Option<Vec<u32>>, String> {
    validate_config_string("processor list", v)?;
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let v = v.replace(',', ";");
    let mut out: Vec<u32> = Vec::new();
    for part in v.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.contains('-') {
            let bits: Vec<&str> = part.splitn(2, '-').collect();
            let lo: u32 = bits[0]
                .trim()
                .parse()
                .map_err(|_| format!("invalid processor range {:?}", part))?;
            let hi: u32 = bits[1]
                .trim()
                .parse()
                .map_err(|_| format!("invalid processor range {:?}", part))?;
            if hi < lo || hi > 1023 {
                return Err(format!("invalid processor range {:?}", part));
            }
            for i in lo..=hi {
                if !out.contains(&i) {
                    if out.len() >= MAX_CONFIG_LIST_ITEMS {
                        return Err(format!(
                            "processor list exceeds limit of {} values",
                            MAX_CONFIG_LIST_ITEMS
                        ));
                    }
                    out.push(i);
                }
            }
        } else {
            let n: u32 = part
                .parse()
                .map_err(|_| format!("invalid processor {:?}", part))?;
            if n > 1023 {
                return Err(format!("invalid processor {:?}", part));
            }
            if !out.contains(&n) {
                if out.len() >= MAX_CONFIG_LIST_ITEMS {
                    return Err(format!(
                        "processor list exceeds limit of {} values",
                        MAX_CONFIG_LIST_ITEMS
                    ));
                }
                out.push(n);
            }
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.main_display, 1);
        assert!(cfg.memory_warning_enabled);
        assert!(cfg.temperature_warning_enabled);
        assert!(!cfg.hang_enabled);
        assert_eq!(cfg.presentmon_target_mode, "presenting");
        assert!(cfg.presentmon_deferred);
        assert!(!cfg.presentmon_persist);
        assert_eq!(cfg.memory_warning, 100.0);
        assert_eq!(cfg.vmem_warning, 100.0);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn presentmon_modes_preserve_targeted_overrides() {
        for mode in ["presenting", "foreground", "disabled"] {
            assert!(validate(&Config {
                presentmon_target_mode: mode.into(),
                ..Config::default()
            })
            .is_ok());
        }
        assert!(validate(&Config {
            presentmon_target_mode: "process_name".into(),
            presentmon_process_name: "game.exe".into(),
            ..Config::default()
        })
        .is_ok());
        assert!(validate(&Config {
            presentmon_target_mode: "process_name".into(),
            ..Config::default()
        })
        .is_err());
    }

    #[test]
    fn network_probe_targets_are_single_and_method_specific() {
        let mut cfg = Config {
            network_probe_enabled: true,
            network_probe_method: "auto".into(),
            network_probe_target: "127.0.0.1".into(),
            ..Config::default()
        };
        assert!(validate(&cfg).is_ok());
        cfg.network_probe_target = "127.0.0.1:443".into();
        assert!(validate(&cfg).is_ok());
        cfg.network_probe_method = "tcp".into();
        cfg.network_probe_target = "::1".into();
        assert!(validate(&cfg).is_ok(), "TCP permits IPv6 with default port");
        cfg.network_probe_target = "127.0.0.1:0".into();
        assert!(validate(&cfg).is_err());
        cfg.network_probe_target = "example.test:443".into();
        assert!(validate(&cfg).is_err());
        cfg.network_probe_method = "icmp".into();
        cfg.network_probe_target = "::1".into();
        assert!(validate(&cfg).is_err());
        cfg.network_probe_target = "127.0.0.1:443".into();
        assert!(validate(&cfg).is_err());
        cfg.network_probe_method = "auto".into();
        cfg.network_probe_target = "::1".into();
        assert!(validate(&cfg).is_ok(), "auto IPv6 uses TCP only");
    }

    #[test]
    fn network_graph_ceiling_is_positive_finite_and_bounded() {
        for module in [
            "NET_IN",
            "NET_OUT",
            "NET_BOTH",
            "NET_IN_GRAPH",
            "NET_OUT_GRAPH",
            "NET_GRAPH",
        ] {
            assert!(valid_module(module));
        }
        for value in [0.0, f64::NAN, f64::INFINITY, 100001.0] {
            let cfg = Config {
                network_graph_ceiling_mbps: value,
                ..Config::default()
            };
            assert!(validate(&cfg).is_err(), "accepted {value}");
        }
        let cfg = Config {
            network_graph_ceiling_mbps: 100000.0,
            ..Config::default()
        };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn defaults_match_shipped_portable_config() {
        let shipped = crate::parser::parse_standalone(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/lcdsirplus.txt"
        )))
        .expect("canonical shipped configuration must parse");
        let defaults = Config::default();
        assert_eq!(defaults.slots, shipped.slots);
        assert_eq!(
            (
                &defaults.presentmon_target_mode,
                defaults.presentmon_deferred,
                defaults.presentmon_persist,
            ),
            (
                &shipped.presentmon_target_mode,
                shipped.presentmon_deferred,
                shipped.presentmon_persist,
            )
        );
        assert_eq!(
            (
                defaults.memory_warning_enabled,
                defaults.temperature_warning_enabled,
                defaults.hang_enabled,
                defaults.memory_warning,
                defaults.vmem_warning,
            ),
            (
                shipped.memory_warning_enabled,
                shipped.temperature_warning_enabled,
                shipped.hang_enabled,
                shipped.memory_warning,
                shipped.vmem_warning,
            )
        );
    }

    #[test]
    fn module_registry_is_exact_unique_and_case_insensitive() {
        assert_eq!(MODULES.len(), 53);
        let unique: std::collections::HashSet<_> = MODULES.iter().collect();
        assert_eq!(unique.len(), MODULES.len());
        assert!(MODULES
            .iter()
            .all(|module| valid_module(&module.to_lowercase())));
    }

    #[test]
    fn module_expansion_ranges_and_hwinfo_pairs_validate() {
        let defaults = Config::default();
        assert!(defaults.hwinfo_cpu_temp_sensor.is_empty());
        assert!(defaults.hwinfo_cpu_temp_reading.is_empty());
        let mut cfg = Config {
            hwinfo_cpu_temp_sensor: "CPU [#0]".into(),
            ..Config::default()
        };
        assert!(validate(&cfg).is_err());
        cfg.hwinfo_cpu_temp_reading = "CPU (Tctl/Tdie)".into();
        assert!(validate(&cfg).is_ok());
        cfg.hwinfo_total_power_sensor = "System".into();
        assert!(validate(&cfg).is_err());
        cfg.hwinfo_total_power_reading = "Total Power".into();
        assert!(validate(&cfg).is_ok());
        cfg.cpu_fan_max_rpm = 30001;
        assert!(validate(&cfg).is_err());
        cfg.cpu_fan_max_rpm = 0;
        cfg.cpu_temp_max_c = f64::NAN;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn proc_hang_is_globally_unique_and_legacy_button_is_accepted() {
        let mut cfg = Config {
            hang_enabled: true,
            hang_button: 1,
            ..Config::default()
        };
        assert!(validate(&cfg).is_ok());
        cfg.slots[0].push("PROC_HANG".into());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn warning_requires_rendering_each_hundred_millisecond_transition() {
        assert!(validate(&Config {
            warning: true,
            render_interval: Duration::from_millis(100),
            ..Config::default()
        })
        .is_ok());
        assert!(validate(&Config {
            warning: true,
            render_interval: Duration::from_millis(101),
            ..Config::default()
        })
        .is_err());
        assert!(validate(&Config {
            warning: false,
            render_interval: Duration::from_secs(5),
            ..Config::default()
        })
        .is_ok());
        assert!(validate(&Config {
            warning: true,
            temperature_warning_enabled: false,
            render_interval: Duration::from_secs(5),
            ..Config::default()
        })
        .is_ok());
        assert!(validate(&Config {
            memory_warning: 0.0,
            vmem_warning: 0.0,
            ..Config::default()
        })
        .is_ok());
    }

    #[test]
    fn accepts_sdk_and_rejects_retired_ccd_source() {
        let c = Config {
            logitech_backend: "sdk".into(),
            ..Config::default()
        };
        assert!(validate(&c).is_ok(), "SDK backend is supported");
        let c = Config {
            ccd_source: "lasso".into(),
            ..Config::default()
        };
        assert!(validate(&c).is_err(), "Process Lasso must be retired");
    }

    #[test]
    fn lhm_url_requires_loopback() {
        let c = Config {
            lhm_url: "http://192.168.1.5:8085/data.json".into(),
            ..Config::default()
        };
        assert!(validate(&c).is_err());
        let c = Config {
            lhm_url: "http://localhost:8085/data.json".into(),
            ..Config::default()
        };
        assert!(validate(&c).is_ok());
        let c = Config {
            lhm_url: "http://[::1]:8085/data.json".into(),
            ..Config::default()
        };
        assert!(validate(&c).is_ok());
        for url in [
            "https://127.0.0.1:8085/data.json",
            "http://example.com@127.0.0.1:8085/data.json",
            "http://::1/data.json",
            "http://127.0.0.1:/data.json",
            "http://127.0.0.1:80:90/data.json",
            "http://[::1]:/data.json",
            "http://127.0.0.1:8085/data json",
            "http://127.0.0.1:8085/data.json\r\nX-Injected: yes",
            "http://127.0.0.1:8085/data.json#fragment",
        ] {
            let c = Config {
                lhm_url: url.into(),
                ..Config::default()
            };
            assert!(validate(&c).is_err());
        }
    }

    #[test]
    fn processor_lists_parse_ranges_and_dedupe() {
        assert_eq!(parse_processor_list("auto").unwrap(), None);
        assert_eq!(parse_processor_list("").unwrap(), None);
        assert_eq!(parse_processor_list("0,1,2").unwrap(), Some(vec![0, 1, 2]));
        assert_eq!(
            parse_processor_list("0-3;2").unwrap(),
            Some(vec![0, 1, 2, 3])
        );
        assert!(parse_processor_list("5-3").is_err());
        assert!(parse_processor_list("9999").is_err());
    }
}
