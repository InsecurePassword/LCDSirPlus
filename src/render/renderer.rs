//! Fixed 160x43 dashboard renderer. Layout 1 preserves legacy Go behavior;
//! additional layouts and helpers are Rust-specific. Golden framebuffer hashes
//! are release invariants requiring explicit review when changed.
#![allow(clippy::too_many_arguments)] // Direct drawing signatures cover legacy and Rust layouts.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use super::font::{fit_text, text_width};
use super::Frame;
use crate::history::Series;
use crate::model::{
    session_duration, AcState, Availability, Freshness, HardwareKind, HardwareRole, HeadsetBattery,
    HungTarget, Metric, MetricKey, ReadingsSnapshot, Snapshot, Speaker, ValueKind, WIDTH,
};

/// Overlay knobs the renderer reads from configuration.
#[derive(Clone, Copy, Debug)]
pub struct OverlayOptions {
    pub main_display: i32,
    pub discord_linger: Duration,
    pub discord_max_speakers: usize,
    pub discord_show_self: bool,
    pub discord_show_channel: bool,
    pub network_graph_ceiling_download_mbps: f64,
    pub network_graph_ceiling_upload_mbps: f64,
    pub disk_graph_ceiling_mbps: f64,
    pub fps_graph_ceiling: f64,
    pub cpu_temp_max_c: f64,
    pub gpu_temp_max_c: f64,
    pub warning: bool,
    pub warning_phase: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        OverlayOptions {
            main_display: 1,
            discord_linger: Duration::from_millis(700),
            discord_max_speakers: 2,
            discord_show_self: false,
            discord_show_channel: false,
            network_graph_ceiling_download_mbps: 1000.0,
            network_graph_ceiling_upload_mbps: 1000.0,
            disk_graph_ceiling_mbps: 1000.0,
            fps_graph_ceiling: 240.0,
            cpu_temp_max_c: 90.0,
            gpu_temp_max_c: 90.0,
            warning: false,
            warning_phase: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct View {
    pub slot_modules: [String; 4],
    pub hung_index: usize,
    pub hung_detail: bool,
    pub hung_hold: f64,
    pub alert_acknowledged: HashSet<String>,
}

#[derive(Default)]
pub struct Renderer {
    hist: HashMap<&'static str, Series>,
    last: HashMap<&'static str, SystemTime>,
    now: Option<SystemTime>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DisplayState {
    Unavailable,
    Stale,
    Current,
}

impl Renderer {
    pub fn new() -> Self {
        Renderer::default()
    }

    pub fn render(&mut self, s: &Snapshot, opts: OverlayOptions, v: &View) -> Frame {
        self.now = s.now;
        self.capture(s);
        let mut f = Frame::new();
        if !s.hung.is_empty()
            && v.slot_modules
                .iter()
                .any(|module| module.eq_ignore_ascii_case("PROC_HANG"))
        {
            self.dashboard(&mut f, s, v, &opts);
            return f;
        }
        if let Some(alert) = highest_critical(&s.alerts, &v.alert_acknowledged, opts.warning) {
            alert_overlay(&mut f, alert);
            return f;
        }
        let speakers = visible_speakers(
            &s.discord,
            s.now,
            opts.discord_linger,
            opts.discord_show_self,
        );
        if !speakers.is_empty() {
            discord_overlay(&mut f, &s.discord, &speakers, &opts);
            return f;
        }
        self.dashboard(&mut f, s, v, &opts);
        f
    }

    fn capture(&mut self, s: &Snapshot) {
        let items: [(&'static str, Metric); 13] = [
            ("frame", s.game.frame_time_ms),
            ("fps", s.game.fps),
            ("netin", s.network_in),
            ("netout", s.network_out),
            ("ping", s.ping_ms),
            ("cpu", s.cpu_temp),
            ("gpu", s.gpu_temp),
            (
                "cpuload",
                canonical_percent_metric(&s.readings, MetricKey::CPUUtilization, ""),
            ),
            (
                "gpuload",
                canonical_percent_metric(&s.readings, MetricKey::GPUUtilization, ""),
            ),
            (
                "diskread",
                canonical_number_metric(
                    &s.readings,
                    MetricKey::DiskReadBytesPerSec,
                    ValueKind::ByteRate,
                ),
            ),
            (
                "diskwrite",
                canonical_number_metric(
                    &s.readings,
                    MetricKey::DiskWriteBytesPerSec,
                    ValueKind::ByteRate,
                ),
            ),
            (
                "cachetemp",
                canonical_cpu_domain_temperature(&s.readings, HardwareRole::Cache),
            ),
            (
                "freqtemp",
                canonical_cpu_domain_temperature(&s.readings, HardwareRole::Frequency),
            ),
        ];
        for (key, metric) in items {
            if !metric.valid || !metric.value.is_finite() {
                continue;
            }
            let Some(updated) = metric.updated else {
                continue;
            };
            if self.last.get(key) == Some(&updated) {
                continue;
            }
            let series = self.hist.entry(key).or_insert_with(|| Series::new(320));
            series.add(updated, metric.value);
            self.last.insert(key, updated);
        }
    }

    fn dashboard(&mut self, f: &mut Frame, s: &Snapshot, v: &View, opts: &OverlayOptions) {
        let mut date = s.date_text.clone();
        if text_width(&date, 1) + text_width(&s.time_text, 1) + 3 > WIDTH as i32 {
            date = s.date_short_text.clone();
        }
        let date = fit_text(&date, WIDTH as i32 - text_width(&s.time_text, 1) - 3, 1);
        f.text(1, 0, &date, true);
        f.text_right(158, 0, &s.time_text, true);
        f.h_line(0, 159, 6, true);
        f.h_line(0, 159, 25, true);
        for x in [39, 79, 119] {
            f.v_line(x, 26, 42, true);
        }
        match opts.main_display {
            2 => self.main_display_2(
                f,
                s,
                opts.network_graph_ceiling_download_mbps,
                opts.network_graph_ceiling_upload_mbps,
            ),
            3 => self.main_display_3(
                f,
                s,
                opts.network_graph_ceiling_download_mbps,
                opts.network_graph_ceiling_upload_mbps,
            ),
            _ => {
                f.v_line(79, 7, 24, true);
                if s.cpu_dual {
                    self.cpu_split(f, 1, 8, s.cpu_cache_load, s.cpu_freq_load, 77);
                } else {
                    // Single-CCD: one standard-height CPU bar.
                    self.metric_bar(f, 1, 8, "CPU", &s.cpu_cache_load, 75, 7);
                }
                self.metric_bar(
                    f,
                    1,
                    17,
                    "RAM",
                    &canonical_percent_metric(&s.readings, MetricKey::RAMUtilization, ""),
                    75,
                    7,
                );
                self.metric_bar(
                    f,
                    81,
                    8,
                    "GPU ",
                    &canonical_percent_metric(&s.readings, MetricKey::GPUUtilization, ""),
                    77,
                    7,
                );
                self.metric_bar(
                    f,
                    81,
                    17,
                    "VRAM",
                    &canonical_percent_metric(&s.readings, MetricKey::VRAMUtilization, ""),
                    77,
                    7,
                );
            }
        }
        for i in 0..4usize {
            self.slot(f, i, &v.slot_modules[i], s, opts, v);
            if opts.warning
                && opts.warning_phase
                && slot_temperature_warning(&v.slot_modules[i], s, opts)
            {
                let (x, w) = slot_bounds(i);
                f.invert_rect(x, 26, w, 17);
            }
        }
    }

    fn main_display_2(
        &mut self,
        f: &mut Frame,
        s: &Snapshot,
        download_ceiling_mbps: f64,
        upload_ceiling_mbps: f64,
    ) {
        f.v_line(52, 7, 24, true);
        f.v_line(105, 7, 24, true);
        if s.cpu_dual {
            self.cpu_split(f, 1, 8, s.cpu_cache_load, s.cpu_freq_load, 51);
        } else {
            self.metric_bar(f, 1, 8, "CPU", &s.cpu_cache_load, 51, 7);
        }
        self.metric_bar(
            f,
            1,
            17,
            "RAM",
            &canonical_percent_metric(&s.readings, MetricKey::RAMUtilization, ""),
            51,
            7,
        );
        self.metric_bar(
            f,
            54,
            8,
            "GPU ",
            &canonical_percent_metric(&s.readings, MetricKey::GPUUtilization, ""),
            51,
            7,
        );
        self.metric_bar(
            f,
            54,
            17,
            "VRAM",
            &canonical_percent_metric(&s.readings, MetricKey::VRAMUtilization, ""),
            51,
            7,
        );
        self.network_bar(
            f,
            107,
            9,
            8,
            "OUT",
            s.network_out,
            157,
            7,
            upload_ceiling_mbps,
        );
        self.network_bar(
            f,
            107,
            18,
            17,
            "IN ",
            s.network_in,
            157,
            7,
            download_ceiling_mbps,
        );
    }

    fn main_display_3(
        &mut self,
        f: &mut Frame,
        s: &Snapshot,
        download_ceiling_mbps: f64,
        upload_ceiling_mbps: f64,
    ) {
        f.v_line(79, 7, 24, true);
        if s.cpu_dual {
            self.cpu_split(f, 1, 8, s.cpu_cache_load, s.cpu_freq_load, 77);
        } else {
            self.metric_bar(f, 1, 8, "CPU", &s.cpu_cache_load, 75, 7);
        }
        self.metric_bar(
            f,
            1,
            17,
            "RAM",
            &canonical_percent_metric(&s.readings, MetricKey::RAMUtilization, ""),
            75,
            7,
        );
        self.network_bar(
            f,
            81,
            9,
            8,
            "NET IN ",
            s.network_in,
            157,
            7,
            download_ceiling_mbps,
        );
        self.network_bar(
            f,
            81,
            18,
            17,
            "NET OUT",
            s.network_out,
            157,
            7,
            upload_ceiling_mbps,
        );
    }

    fn cpu_split(
        &mut self,
        f: &mut Frame,
        x: i32,
        y: i32,
        cache: Metric,
        freq: Metric,
        right: i32,
    ) {
        f.text(x, y + 2, "CPU", true);
        let bx = x + text_width("CPU", 1) + 2;
        self.small_bar(f, bx, y, cache, right - bx + 1);
        self.small_bar(f, bx, y + 4, freq, right - bx + 1);
    }

    fn small_bar(&mut self, f: &mut Frame, x: i32, y: i32, m: Metric, w: i32) {
        f.rect(x, y, w, 3, true);
        let p = metric_percent(&m);
        if p < 0 {
            self.metric_placeholder(f, x + 1, y + 1, w - 2, 1, m.valid && m.stale);
            return;
        }
        let fill = (w - 2) * p / 100;
        f.fill_rect(x + 1, y + 1, fill, 1, true);
    }

    fn metric_bar(
        &mut self,
        f: &mut Frame,
        x: i32,
        y: i32,
        label: &str,
        m: &Metric,
        total_w: i32,
        h: i32,
    ) {
        f.text(x, y + 1, label, true);
        let label_width = text_width(label, 1);
        let bx = x + label_width + 2;
        let bw = total_w - label_width - 2;
        f.rect(bx, y, bw, h, true);
        let p = metric_percent(m);
        if p < 0 {
            self.metric_placeholder(f, bx + 1, y + 1, bw - 2, h - 2, m.valid && m.stale);
            return;
        }
        let fill = (bw - 2) * p / 100;
        f.fill_rect(bx + 1, y + 1, fill, h - 2, true);
    }

    fn network_bar(
        &mut self,
        f: &mut Frame,
        x: i32,
        label_y: i32,
        bar_y: i32,
        label: &str,
        m: Metric,
        right: i32,
        height: i32,
        ceiling_mbps: f64,
    ) {
        f.text(x, label_y, label, true);
        let bx = x + text_width(label, 1) + 2;
        let bw = right - bx + 1;
        f.rect(bx, bar_y, bw, height, true);
        let state = metric_state(m);
        if state != DisplayState::Current {
            self.metric_placeholder(
                f,
                bx + 1,
                bar_y + 1,
                bw - 2,
                height - 2,
                state == DisplayState::Stale,
            );
        } else {
            let fill = network_graph_height(m.value, ceiling_mbps, bw - 2);
            f.fill_rect(bx + 1, bar_y + 1, fill, height - 2, true);
        }
    }

    fn metric_placeholder(
        &mut self,
        f: &mut Frame,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stale: bool,
    ) {
        for yy in 0..height {
            for xx in 0..width {
                let mut on = (xx + yy) % 2 == 0;
                if stale {
                    on = xx % 4 < 2;
                }
                if on {
                    f.set(x + xx, y + yy, true);
                }
            }
        }
    }

    fn slot(
        &mut self,
        f: &mut Frame,
        i: usize,
        module: &str,
        s: &Snapshot,
        opts: &OverlayOptions,
        v: &View,
    ) {
        let (x, w) = slot_bounds(i);
        let left = x + 1;
        let width = w - 2;
        let module = module.to_uppercase();
        match module.as_str() {
            "HEADSET_BATTERY" => headset_slot(
                f,
                left,
                width,
                &s.headset,
                s.providers.get("safe-mode") == Some(&true),
            ),
            "CONTROLLER_BATTERY" => controller_slot(f, left, width, &s.controller),
            "FPS_CURRENT" => self.numeric_slot(f, left, width, "FPS", s.game.fps, 0, ""),
            "FPS_1LOW" => self.numeric_slot(f, left, width, "1% LOW", s.game.one_percent, 0, ""),
            "FPS_01LOW" => {
                self.numeric_slot(f, left, width, ".1% LOW", s.game.point_one_low, 0, "")
            }
            "FRAME_TIME" => {
                self.graph_slot(f, left, width, "FT", s.game.frame_time_ms, "frame", "MS")
            }
            "CPU_TEMP" => self.numeric_slot(f, left, width, "CPU TEMP", s.cpu_temp, 0, "\u{00B0}"),
            "GPU_TEMP" => self.numeric_slot(f, left, width, "GPU TEMP", s.gpu_temp, 0, "\u{00B0}"),
            "CPU_LOAD" => self.numeric_slot(
                f,
                left,
                width,
                "CPU LOAD",
                canonical_percent_metric(&s.readings, MetricKey::CPUUtilization, ""),
                0,
                "%",
            ),
            "RAM_USAGE" => self.usage_slot(
                f,
                left,
                width,
                "RAM",
                &s.readings,
                MetricKey::RAMUtilization,
                MetricKey::RAMUsed,
                MetricKey::RAMTotal,
            ),
            "GPU_LOAD" => self.numeric_slot(
                f,
                left,
                width,
                "GPU LOAD",
                canonical_percent_metric(&s.readings, MetricKey::GPUUtilization, ""),
                0,
                "%",
            ),
            "VRAM_USAGE" => self.usage_slot(
                f,
                left,
                width,
                "VRAM",
                &s.readings,
                MetricKey::VRAMUtilization,
                MetricKey::VRAMUsed,
                MetricKey::VRAMTotal,
            ),
            "CPU_CACHE_TEMP" => self.numeric_slot(
                f,
                left,
                width,
                "CACHE T",
                canonical_cpu_domain_temperature(&s.readings, HardwareRole::Cache),
                0,
                "\u{00B0}",
            ),
            "CPU_FREQ_TEMP" => self.numeric_slot(
                f,
                left,
                width,
                "FREQ T",
                canonical_cpu_domain_temperature(&s.readings, HardwareRole::Frequency),
                0,
                "\u{00B0}",
            ),
            "NET_IN" => self.network_numeric_slot(f, left, width, "NET IN", s.network_in),
            "NET_OUT" => self.network_numeric_slot(f, left, width, "NET OUT", s.network_out),
            "NET_BOTH" => self.network_numeric_slot(
                f,
                left,
                width,
                "NET BOTH",
                combined_network_metric(s.network_in, s.network_out),
            ),
            "NET_IN_GRAPH" => self.network_graph_slot(
                f,
                left,
                width,
                "I",
                s.network_in,
                "netin",
                opts.network_graph_ceiling_download_mbps,
            ),
            "NET_OUT_GRAPH" => self.network_graph_slot(
                f,
                left,
                width,
                "O",
                s.network_out,
                "netout",
                opts.network_graph_ceiling_upload_mbps,
            ),
            "NET_GRAPH" => self.network_dual_graph_slot(
                f,
                left,
                width,
                s.network_in,
                s.network_out,
                opts.network_graph_ceiling_download_mbps,
                opts.network_graph_ceiling_upload_mbps,
            ),
            "PING" => self.numeric_slot(f, left, width, "PING", s.ping_ms, 0, "MS"),
            "JITTER" => self.numeric_slot(f, left, width, "JITTER", s.jitter_ms, 0, "MS"),
            "PACKET_LOSS" => self.numeric_slot(f, left, width, "LOSS", s.packet_loss, 1, "%"),
            "MIC_STATUS" => {
                let text = if !s.microphone_known {
                    "N/A"
                } else if s.microphone_muted {
                    "MUTED"
                } else {
                    "LIVE"
                };
                self.text_slot(f, left, width, "MIC", text);
            }
            "AUDIO" => self.numeric_slot(f, left, width, "VOLUME", s.audio_volume, 0, "%"),
            "SESSION_TIME" => self.text_slot(
                f,
                left,
                width,
                "SESSION",
                &crate::model::format_duration(session_duration(s)),
            ),
            "SESSION_SUMMARY" => self.session_slot(f, left, width, s),
            "CLOCK" => self.text_slot(f, left, width, "TIME", &s.time_text),
            "GAME_NAME" => self.text_slot(
                f,
                left,
                width,
                "GAME",
                &first_non_empty(&[&s.game.game_name, &s.game.process_name], "N/A"),
            ),
            "ALERTS" => alert_slot(f, left, width, &s.alerts),
            "PROVIDER_STATUS" => provider_slot(f, left, width, &s.providers),
            "CPU_LOAD_GRAPH" => self.fixed_graph_slot(
                f,
                left,
                width,
                "CPU",
                canonical_percent_metric(&s.readings, MetricKey::CPUUtilization, ""),
                "cpuload",
                100.0,
                "%",
            ),
            "GPU_LOAD_GRAPH" => self.fixed_graph_slot(
                f,
                left,
                width,
                "GPU",
                canonical_percent_metric(&s.readings, MetricKey::GPUUtilization, ""),
                "gpuload",
                100.0,
                "%",
            ),
            "CPU_TEMP_GRAPH" => self.fixed_graph_slot(
                f,
                left,
                width,
                "CPU",
                s.cpu_temp,
                "cpu",
                opts.cpu_temp_max_c,
                "C",
            ),
            "GPU_TEMP_GRAPH" => self.fixed_graph_slot(
                f,
                left,
                width,
                "GPU",
                s.gpu_temp,
                "gpu",
                opts.gpu_temp_max_c,
                "C",
            ),
            "VRM_TEMP" => self.reading_numeric_slot(
                f,
                left,
                width,
                "VRM TEMP",
                &s.readings,
                MetricKey::VRMTemp,
                ValueKind::Celsius,
                0,
                "C",
            ),
            "CHIPSET_TEMP" => self.reading_numeric_slot(
                f,
                left,
                width,
                "CHIPSET",
                &s.readings,
                MetricKey::ChipsetTemp,
                ValueKind::Celsius,
                0,
                "C",
            ),
            "MOTHERBOARD_TEMP" => self.reading_numeric_slot(
                f,
                left,
                width,
                "BOARD",
                &s.readings,
                MetricKey::MotherboardTemp,
                ValueKind::Celsius,
                0,
                "C",
            ),
            "CPU_FAN" => self.reading_numeric_slot(
                f,
                left,
                width,
                "CPU FAN",
                &s.readings,
                MetricKey::CPUFanPercent,
                ValueKind::Percent,
                0,
                "%",
            ),
            "PUMP_RPM" => self.reading_numeric_slot(
                f,
                left,
                width,
                "PUMP",
                &s.readings,
                MetricKey::PumpPercent,
                ValueKind::Percent,
                0,
                "%",
            ),
            "POWER_LIMIT" => self.reading_numeric_slot(
                f,
                left,
                width,
                "POWER",
                &s.readings,
                MetricKey::TotalPower,
                ValueKind::Watts,
                0,
                "W",
            ),
            "CPU_GPU_POWER" => self.reading_numeric_slot(
                f,
                left,
                width,
                "CPU+GPU",
                &s.readings,
                MetricKey::CPUGPUPower,
                ValueKind::Watts,
                0,
                "W",
            ),
            "DISK_IO" => self.disk_io_slot(f, left, width, s),
            "DISK_IO_GRAPH" => {
                self.disk_graph_slot(f, left, width, s, opts.disk_graph_ceiling_mbps)
            }
            "RAM_DETAIL" => self.ram_detail_slot(f, left, width, s),
            "FPS_GRAPH" => self.fixed_graph_slot(
                f,
                left,
                width,
                "FPS",
                s.game.fps,
                "fps",
                opts.fps_graph_ceiling,
                "",
            ),
            "THERMALS" => thermals_slot(f, left, width, s.cpu_temp, s.gpu_temp),
            "CONNECTIONS" => self.reading_numeric_slot(
                f,
                left,
                width,
                "TCP EST",
                &s.readings,
                MetricKey::EstablishedConnections,
                ValueKind::Count,
                0,
                "",
            ),
            "NET_HEALTH" => net_health_slot(f, left, width, s.ping_ms, s.jitter_ms, s.packet_loss),
            "SYSTEM_BATTERY" => self.text_slot(
                f,
                left,
                width,
                "POWER",
                &system_battery_text(s.system_battery),
            ),
            "HARD_FAULTS" => hard_faults_slot(f, left, width, &s.readings),
            "BOTTLENECK" => self.text_slot(f, left, width, "BOTTLENECK", bottleneck_text(s)),
            "PROC_HANG" => hang_slot(f, i, &s.hung, v),
            "CLEAR" => self.text_slot(f, left, width, "STATUS", "CLEAR"),
            other => {
                debug_assert!(
                    !crate::config::valid_module(other),
                    "registered module {other} reached the unknown-module fallback"
                );
                self.text_slot(f, left, width, other, "N/A")
            }
        }
    }

    fn label(&mut self, f: &mut Frame, x: i32, w: i32, label: &str) {
        f.text_centered(x, w, 27, &fit_text(label, w, 1), 1, true);
    }

    fn numeric_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        label: &str,
        m: Metric,
        decimals: i32,
        suffix: &str,
    ) {
        self.label(f, x, w, label);
        if metric_state(m) != DisplayState::Current {
            f.text_centered(x, w, 35, state_text(metric_state(m)), 1, true);
            return;
        }
        f.text_centered(
            x,
            w,
            35,
            &format_bounded_number("", m.value, decimals as usize, suffix, w),
            1,
            true,
        );
    }

    fn network_numeric_slot(&mut self, f: &mut Frame, x: i32, w: i32, label: &str, m: Metric) {
        self.label(f, x, w, label);
        let text = metric_network_rate(m);
        f.text_centered(x, w, 35, &text, 1, true);
    }

    fn network_graph_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        prefix: &str,
        m: Metric,
        key: &'static str,
        ceiling_mbps: f64,
    ) {
        let reading = metric_network_rate(m);
        if reading == "N/A" || reading == "STALE" {
            self.label(f, x, w, prefix);
            f.text_centered(x, w, 35, &reading, 1, true);
            return;
        }
        self.label(f, x, w, &format_network_graph_rate(prefix, m.value, w));
        let Some(samples) = self.hist.get(key) else {
            return;
        };
        let values =
            samples.second_bins(self.now.or(m.updated).unwrap_or(SystemTime::UNIX_EPOCH), 30);
        let count = 30.min(w as usize);
        let start_x = x + (w - count as i32) / 2;
        for (offset, sample) in values[values.len() - count..].iter().enumerate() {
            let Some(sample) = sample else { continue };
            let height = network_graph_height(*sample, ceiling_mbps, 10);
            if height > 0 {
                f.v_line(start_x + offset as i32, 43 - height, 42, true);
            }
        }
    }

    fn network_dual_graph_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        network_in: Metric,
        network_out: Metric,
        download_ceiling_mbps: f64,
        upload_ceiling_mbps: f64,
    ) {
        self.label(f, x, w, "I/O");
        let state = if !network_in.valid
            || !network_out.valid
            || !network_in.value.is_finite()
            || !network_out.value.is_finite()
        {
            Some("N/A")
        } else if network_in.stale || network_out.stale {
            Some("STALE")
        } else {
            None
        };
        if let Some(state) = state {
            f.text_centered(x, w, 35, state, 1, true);
            return;
        }
        let (Some(in_samples), Some(out_samples)) =
            (self.hist.get("netin"), self.hist.get("netout"))
        else {
            return;
        };
        let end = self
            .now
            .or(network_in.updated.max(network_out.updated))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let in_values = in_samples.second_bins(end, 30);
        let out_values = out_samples.second_bins(end, 30);
        let count = 30.min(w as usize);
        let start_x = x + (w - count as i32) / 2;
        let in_start = in_values.len() - count;
        let out_start = out_values.len() - count;
        let baseline = 37;
        f.h_line(start_x, start_x + count as i32 - 1, baseline, true);
        for offset in 0..count {
            let in_height = in_values[in_start + offset]
                .map(|value| network_graph_height(value, download_ceiling_mbps, 4))
                .unwrap_or(0);
            let out_height = out_values[out_start + offset]
                .map(|value| network_graph_height(value, upload_ceiling_mbps, 5))
                .unwrap_or(0);
            if in_height > 0 {
                f.v_line(
                    start_x + offset as i32,
                    baseline - in_height,
                    baseline - 1,
                    true,
                );
            }
            if out_height > 0 {
                f.v_line(
                    start_x + offset as i32,
                    baseline + 1,
                    baseline + out_height,
                    true,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn usage_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        label: &str,
        readings: &ReadingsSnapshot,
        percent_key: MetricKey,
        used_key: MetricKey,
        total_key: MetricKey,
    ) {
        let percent = readings.lookup(percent_key, "");
        let hardware_id = percent.map(|p| p.hardware_id.clone()).unwrap_or_default();
        let used = readings.lookup(used_key, &hardware_id);
        let total = readings.lookup(total_key, &hardware_id);
        let states = [
            percent
                .map(display_state_reading)
                .unwrap_or(DisplayState::Unavailable),
            used.map(display_state_reading)
                .unwrap_or(DisplayState::Unavailable),
            total
                .map(display_state_reading)
                .unwrap_or(DisplayState::Unavailable),
        ];
        let mut state = DisplayState::Current;
        for candidate in states {
            if candidate == DisplayState::Unavailable {
                state = DisplayState::Unavailable;
                break;
            }
            if candidate == DisplayState::Stale {
                state = DisplayState::Stale;
            }
        }
        let percent_ok = percent
            .map(|p| p.has_value && p.kind == ValueKind::Percent)
            .unwrap_or(false);
        if state != DisplayState::Current
            || !percent_ok
            || !percent.unwrap().number.is_finite()
            || !(0.0..=100.0).contains(&percent.unwrap().number)
        {
            self.label(f, x, w, &format!("{} USE", label));
            let text = if state == DisplayState::Stale {
                "STALE"
            } else {
                "N/A"
            };
            f.text_centered(x, w, 35, text, 1, true);
            return;
        }
        let (used_bytes, total_bytes) = match (
            used.and_then(|u| u.byte_value()),
            total.and_then(|t| t.byte_value()),
        ) {
            (Some(u), Some(t)) if t != 0 && u <= t => (u, t),
            _ => {
                self.label(f, x, w, &format!("{} USE", label));
                f.text_centered(x, w, 35, "N/A", 1, true);
                return;
            }
        };
        self.label(
            f,
            x,
            w,
            &format!("{} {:.0}%", label, percent.unwrap().number),
        );
        f.text_centered(
            x,
            w,
            35,
            &format_iec_usage(used_bytes, total_bytes, w),
            1,
            true,
        );
    }

    fn text_slot(&mut self, f: &mut Frame, x: i32, w: i32, label: &str, value: &str) {
        self.label(f, x, w, label);
        f.text_centered(x, w, 35, &fit_text(value, w, 1), 1, true);
    }

    fn graph_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        label: &str,
        m: Metric,
        key: &'static str,
        suffix: &str,
    ) {
        if !m.valid || m.stale {
            self.label(f, x, w, label);
            let text = if m.valid && m.stale { "STALE" } else { "N/A" };
            f.text_centered(x, w, 35, text, 1, true);
            return;
        }
        self.label(f, x, w, &format_labeled_number(label, m.value, suffix, w));
        let Some(samples) = self.hist.get(key) else {
            return;
        };
        let vals =
            samples.second_bins(self.now.or(m.updated).unwrap_or(SystemTime::UNIX_EPOCH), 30);
        let min = vals
            .iter()
            .flatten()
            .copied()
            .reduce(f64::min)
            .unwrap_or(0.0);
        let mut max = vals
            .iter()
            .flatten()
            .copied()
            .reduce(f64::max)
            .unwrap_or(1.0);
        if max - min < 1.0 {
            max = min + 1.0;
        }
        let start_x = x + (w - 30) / 2;
        for (i, value) in vals.iter().enumerate() {
            let Some(value) = value else { continue };
            let height = (((value - min) / (max - min) * 8.0).ceil() as i32).clamp(1, 9);
            f.v_line(start_x + i as i32, 43 - height, 42, true);
        }
    }

    fn fixed_graph_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        label: &str,
        m: Metric,
        key: &'static str,
        ceiling: f64,
        suffix: &str,
    ) {
        if metric_state(m) != DisplayState::Current {
            self.label(f, x, w, label);
            f.text_centered(x, w, 35, state_text(metric_state(m)), 1, true);
            return;
        }
        self.label(f, x, w, &format_labeled_number(label, m.value, suffix, w));
        let Some(end) = self.now.or(m.updated) else {
            return;
        };
        let Some(series) = self.hist.get(key) else {
            return;
        };
        let bins = series.second_bins(end, 30);
        let start_x = x + (w - 30) / 2;
        for (column, value) in bins.into_iter().enumerate() {
            let Some(value) = value else { continue };
            let height = graph_height(value, ceiling, 9);
            if height > 0 {
                f.v_line(start_x + column as i32, 43 - height, 42, true);
            }
        }
    }

    fn reading_numeric_slot(
        &mut self,
        f: &mut Frame,
        x: i32,
        w: i32,
        label: &str,
        readings: &ReadingsSnapshot,
        key: MetricKey,
        kind: ValueKind,
        decimals: i32,
        suffix: &str,
    ) {
        self.numeric_slot(
            f,
            x,
            w,
            label,
            canonical_number_metric(readings, key, kind),
            decimals,
            suffix,
        );
    }

    fn disk_io_slot(&mut self, f: &mut Frame, x: i32, w: i32, s: &Snapshot) {
        let read = canonical_number_metric(
            &s.readings,
            MetricKey::DiskReadBytesPerSec,
            ValueKind::ByteRate,
        );
        let write = canonical_number_metric(
            &s.readings,
            MetricKey::DiskWriteBytesPerSec,
            ValueKind::ByteRate,
        );
        let state = combined_state(&[read, write]);
        if state != DisplayState::Current {
            self.label(f, x, w, "DISK I/O");
            f.text_centered(x, w, 35, state_text(state), 1, true);
            return;
        }
        f.text_centered(x, w, 28, &format_disk_rate('R', read.value, w), 1, true);
        f.text_centered(x, w, 36, &format_disk_rate('W', write.value, w), 1, true);
    }

    fn disk_graph_slot(&mut self, f: &mut Frame, x: i32, w: i32, s: &Snapshot, ceiling_mbps: f64) {
        let read = canonical_number_metric(
            &s.readings,
            MetricKey::DiskReadBytesPerSec,
            ValueKind::ByteRate,
        );
        let write = canonical_number_metric(
            &s.readings,
            MetricKey::DiskWriteBytesPerSec,
            ValueKind::ByteRate,
        );
        let label = if combined_state(&[read, write]) == DisplayState::Current {
            format_disk_graph_rates(read.value, write.value, w)
        } else {
            "DISK R/W".into()
        };
        self.label(f, x, w, &label);
        let state = combined_state(&[read, write]);
        if state != DisplayState::Current {
            f.text_centered(x, w, 35, state_text(state), 1, true);
            return;
        }
        let (Some(reads), Some(writes), Some(end)) = (
            self.hist.get("diskread"),
            self.hist.get("diskwrite"),
            self.now.or(read.updated.max(write.updated)),
        ) else {
            return;
        };
        let reads = reads.second_bins(end, 30);
        let writes = writes.second_bins(end, 30);
        let start_x = x + (w - 30) / 2;
        let baseline = 37;
        f.h_line(start_x, start_x + 29, baseline, true);
        for column in 0..30 {
            if let Some(value) = reads[column] {
                let h = graph_height(value, ceiling_mbps * 1_000_000.0, 4);
                if h > 0 {
                    f.v_line(start_x + column as i32, baseline - h, baseline - 1, true);
                }
            }
            if let Some(value) = writes[column] {
                let h = graph_height(value, ceiling_mbps * 1_000_000.0, 5);
                if h > 0 {
                    f.v_line(start_x + column as i32, baseline + 1, baseline + h, true);
                }
            }
        }
    }

    fn ram_detail_slot(&mut self, f: &mut Frame, x: i32, w: i32, s: &Snapshot) {
        self.label(f, x, w, "RAM");
        let used = s.readings.lookup(MetricKey::RAMUsed, "");
        let total = s.readings.lookup(MetricKey::RAMTotal, "");
        let state = combined_reading_state(&[used, total], ValueKind::Bytes);
        if state != DisplayState::Current {
            f.text_centered(x, w, 35, state_text(state), 1, true);
            return;
        }
        let (Some(used), Some(total)) = (
            used.and_then(|r| r.byte_value()),
            total.and_then(|r| r.byte_value()),
        ) else {
            return;
        };
        if total == 0 || used > total {
            f.text_centered(x, w, 35, "N/A", 1, true);
            return;
        }
        f.text_centered(x, w, 35, &format_iec_usage(used, total, w), 1, true);
    }

    fn session_slot(&mut self, f: &mut Frame, x: i32, w: i32, s: &Snapshot) {
        self.label(f, x, w, "SESSION");
        if !s.game.active {
            f.text_centered(x, w, 35, "IDLE", 1, true);
            return;
        }
        f.text_centered(
            x,
            w,
            35,
            &format_session_summary(session_duration(s), s.game.stutters, w),
            1,
            true,
        );
    }
}

fn metric_network_rate(m: Metric) -> String {
    if !m.valid || !m.value.is_finite() {
        "N/A".to_string()
    } else if m.stale {
        "STALE".to_string()
    } else {
        format_network_rate(m.value)
    }
}

fn format_labeled_number(label: &str, value: f64, suffix: &str, max_width: i32) -> String {
    format_bounded_number(&format!("{label} "), value, 0, suffix, max_width)
}

fn format_bounded_number(
    prefix: &str,
    value: f64,
    decimals: usize,
    suffix: &str,
    max_width: i32,
) -> String {
    if !value.is_finite() {
        return "N/A".into();
    }
    for precision in (0..=decimals).rev() {
        let text = format!("{prefix}{value:.precision$}{suffix}");
        if text_width(&text, 1) <= max_width {
            return text;
        }
    }
    for digits in (1..=15).rev() {
        let bound = "9".repeat(digits);
        let threshold = 10f64.powi(digits as i32) - 1.0;
        if value.abs() <= threshold {
            continue;
        }
        let text = if value.is_sign_negative() {
            format!("{prefix}<-{bound}{suffix}")
        } else {
            format!("{prefix}>{bound}{suffix}")
        };
        if text_width(&text, 1) <= max_width {
            return text;
        }
    }
    unreachable!("numeric prefix and suffix must leave room for a truthful bound")
}

fn format_session_summary(duration: Duration, stutters: i32, max_width: i32) -> String {
    let duration_text = crate::model::format_duration(duration);
    let stutter_text = format!("{}S", stutters.max(0));
    let hours = duration.as_secs() / 3600;
    let compact_duration = if hours > 999 {
        ">9H".to_string()
    } else if hours > 0 {
        format!("{hours}H")
    } else {
        duration_text.clone()
    };
    let compact_stutters = if stutters > 999 {
        ">9S".to_string()
    } else {
        stutter_text.clone()
    };
    for (duration, stutters) in [
        (duration_text.as_str(), stutter_text.as_str()),
        (duration_text.as_str(), compact_stutters.as_str()),
        (compact_duration.as_str(), stutter_text.as_str()),
        (compact_duration.as_str(), compact_stutters.as_str()),
    ] {
        let text = format!("{duration} {stutters}");
        if text_width(&text, 1) <= max_width {
            return text;
        }
    }
    ">9H >9S".into()
}

fn metric_state(m: Metric) -> DisplayState {
    if !m.valid || !m.value.is_finite() {
        DisplayState::Unavailable
    } else if m.stale {
        DisplayState::Stale
    } else {
        DisplayState::Current
    }
}

fn state_text(state: DisplayState) -> &'static str {
    match state {
        DisplayState::Unavailable => "N/A",
        DisplayState::Stale => "STALE",
        DisplayState::Current => "",
    }
}

fn combined_state(metrics: &[Metric]) -> DisplayState {
    if metrics
        .iter()
        .any(|metric| metric_state(*metric) == DisplayState::Unavailable)
    {
        DisplayState::Unavailable
    } else if metrics
        .iter()
        .any(|metric| metric_state(*metric) == DisplayState::Stale)
    {
        DisplayState::Stale
    } else {
        DisplayState::Current
    }
}

fn combined_reading_state(
    readings: &[Option<&crate::model::Reading>],
    kind: ValueKind,
) -> DisplayState {
    if readings.iter().any(|reading| {
        reading
            .map(|r| display_state_kind(r, kind))
            .unwrap_or(DisplayState::Unavailable)
            == DisplayState::Unavailable
    }) {
        DisplayState::Unavailable
    } else if readings
        .iter()
        .any(|reading| reading.is_some_and(|r| display_state_kind(r, kind) == DisplayState::Stale))
    {
        DisplayState::Stale
    } else {
        DisplayState::Current
    }
}

fn canonical_number_metric(readings: &ReadingsSnapshot, key: MetricKey, kind: ValueKind) -> Metric {
    let Some(reading) = readings.lookup(key, "") else {
        return Metric::invalid();
    };
    let state = display_state_kind(reading, kind);
    if state == DisplayState::Unavailable
        || !reading.number.is_finite()
        || (kind == ValueKind::Percent && !(0.0..=100.0).contains(&reading.number))
    {
        return Metric::invalid();
    }
    Metric {
        value: reading.number,
        valid: true,
        stale: state == DisplayState::Stale,
        updated: reading.sampled_at,
    }
}

fn graph_height(value: f64, ceiling: f64, max_height: i32) -> i32 {
    if !value.is_finite() || value <= 0.0 || !ceiling.is_finite() || ceiling <= 0.0 {
        return 0;
    }
    ((value.clamp(0.0, ceiling) / ceiling * max_height as f64).ceil() as i32).clamp(1, max_height)
}

fn format_byte_rate(value: f64) -> String {
    let value = value.max(0.0);
    for (scale, suffix) in [(1e9, "GB/s"), (1e6, "MB/s"), (1e3, "KB/s")] {
        if value >= scale {
            let scaled = value / scale;
            return if scaled < 10.0 {
                format!("{scaled:.1}{suffix}")
            } else {
                format!("{scaled:.0}{suffix}")
            };
        }
    }
    format!("{value:.0}B/s")
}

fn format_disk_rate(prefix: char, value: f64, max_width: i32) -> String {
    let text = format!("{prefix} {}", format_byte_rate(value));
    if text_width(&text, 1) <= max_width {
        return text;
    }
    let units = [(1.0, "B/s"), (1e3, "KB/s"), (1e6, "MB/s"), (1e9, "GB/s")];
    let mut unit = units
        .iter()
        .rposition(|(scale, _)| value >= *scale)
        .unwrap_or(0);
    let mut rounded = (value.max(0.0) / units[unit].0).round();
    if rounded >= 1000.0 && unit < units.len() - 1 {
        unit += 1;
        rounded = (value.max(0.0) / units[unit].0).round();
    }
    let compact = if unit == units.len() - 1 && rounded >= 1000.0 {
        format!("{prefix} >99GB/s")
    } else {
        format!("{prefix} {rounded:.0}{}", units[unit].1)
    };
    debug_assert!(text_width(&compact, 1) <= max_width);
    compact
}

fn format_short_rate(value: f64) -> String {
    let value = value.max(0.0);
    let units = [(1.0, "B"), (1e3, "K"), (1e6, "M"), (1e9, "G")];
    let mut unit = units
        .iter()
        .rposition(|(scale, _)| value >= *scale)
        .unwrap_or(0);
    let mut rounded = (value / units[unit].0).round();
    if rounded >= 1000.0 && unit < units.len() - 1 {
        unit += 1;
        rounded = (value / units[unit].0).round();
    }
    if unit == units.len() - 1 && rounded >= 1000.0 {
        ">999G".into()
    } else {
        format!("{rounded:.0}{}", units[unit].1)
    }
}

fn format_disk_graph_rates(read: f64, write: f64, max_width: i32) -> String {
    let label = format!(
        "R {}/W {}",
        format_two_char_rate(read),
        format_two_char_rate(write)
    );
    debug_assert!(text_width(&label, 1) <= max_width);
    label
}

fn format_two_char_rate(value: f64) -> String {
    let exact = format_short_rate(value);
    if exact.chars().count() <= 2 {
        return exact;
    }
    let value = value.max(0.0);
    let units = [(1.0, "B"), (1e3, "K"), (1e6, "M"), (1e9, "G")];
    let unit = units
        .iter()
        .rposition(|(scale, _)| value >= *scale)
        .unwrap_or(0);
    if unit == units.len() - 1 {
        return ">G".into();
    }
    let next = value / units[unit + 1].0;
    if next >= 0.5 {
        format!("1{}", units[unit + 1].1)
    } else {
        format!("<{}", units[unit + 1].1)
    }
}

fn thermals_slot(f: &mut Frame, x: i32, w: i32, cpu: Metric, gpu: Metric) {
    let state = combined_state(&[cpu, gpu]);
    if state != DisplayState::Current {
        f.text_centered(x, w, 27, "THERMALS", 1, true);
        f.text_centered(x, w, 35, state_text(state), 1, true);
    } else {
        f.text_centered(
            x,
            w,
            28,
            &format_labeled_number("CPU", cpu.value, "C", w),
            1,
            true,
        );
        f.text_centered(
            x,
            w,
            36,
            &format_labeled_number("GPU", gpu.value, "C", w),
            1,
            true,
        );
    }
}

fn net_health_slot(f: &mut Frame, x: i32, w: i32, ping: Metric, jitter: Metric, loss: Metric) {
    let state = combined_state(&[ping, jitter, loss]);
    if state != DisplayState::Current {
        f.text_centered(x, w, 27, "NET HEALTH", 1, true);
        f.text_centered(x, w, 35, state_text(state), 1, true);
    } else {
        f.text_centered(
            x,
            w,
            26,
            &format_labeled_number("P", ping.value, "MS", w),
            1,
            true,
        );
        f.text_centered(
            x,
            w,
            32,
            &format_labeled_number("J", jitter.value, "MS", w),
            1,
            true,
        );
        f.text_centered(
            x,
            w,
            38,
            &format_labeled_number("L", loss.value, "%", w),
            1,
            true,
        );
    }
}

fn system_battery_text(battery: crate::model::SystemBattery) -> String {
    if battery.freshness == Freshness::Stale {
        return "STALE".into();
    }
    if battery.freshness != Freshness::Current {
        return "UNKNOWN".into();
    }
    match (
        battery.ac,
        battery.battery_present,
        battery.percent,
        battery.charging,
    ) {
        (AcState::Online, Some(false), _, _) => "AC ONLY".into(),
        (AcState::Offline, Some(false), _, _) => "NO BAT".into(),
        (_, Some(true), Some(percent), true) => format!("CHG {percent}%"),
        (AcState::Online, Some(true), Some(percent), false) => format!("AC {percent}%"),
        (AcState::Offline, Some(true), Some(percent), false) => format!("BAT {percent}%"),
        _ => "UNKNOWN".into(),
    }
}

fn hard_faults_slot(f: &mut Frame, x: i32, w: i32, readings: &ReadingsSnapshot) {
    f.text_centered(x, w, 27, "FAULTS", 1, true);
    let metric = canonical_number_metric(readings, MetricKey::PageReadsPerSec, ValueKind::Count);
    if metric_state(metric) != DisplayState::Current {
        f.text_centered(x, w, 35, state_text(metric_state(metric)), 1, true);
    } else {
        let text = format!("{:.0}/s", metric.value);
        f.text_centered(
            x,
            w,
            35,
            if text_width(&text, 1) <= w {
                &text
            } else {
                ">9999/s"
            },
            1,
            true,
        );
    }
}

fn bottleneck_text(s: &Snapshot) -> &'static str {
    if s.bottleneck.freshness == Freshness::Stale {
        "STALE"
    } else {
        s.bottleneck.state.token().unwrap_or("N/A")
    }
}

fn current_at_or_above(metric: Metric, threshold: f64) -> bool {
    metric_state(metric) == DisplayState::Current && metric.value >= threshold
}

fn slot_temperature_warning(module: &str, s: &Snapshot, opts: &OverlayOptions) -> bool {
    match module.to_ascii_uppercase().as_str() {
        "CPU_TEMP" | "CPU_TEMP_GRAPH" => current_at_or_above(s.cpu_temp, opts.cpu_temp_max_c),
        "CPU_CACHE_TEMP" => current_at_or_above(
            canonical_cpu_domain_temperature(&s.readings, HardwareRole::Cache),
            opts.cpu_temp_max_c,
        ),
        "CPU_FREQ_TEMP" => current_at_or_above(
            canonical_cpu_domain_temperature(&s.readings, HardwareRole::Frequency),
            opts.cpu_temp_max_c,
        ),
        "GPU_TEMP" | "GPU_TEMP_GRAPH" => current_at_or_above(s.gpu_temp, opts.gpu_temp_max_c),
        "THERMALS" => {
            current_at_or_above(s.cpu_temp, opts.cpu_temp_max_c)
                || current_at_or_above(s.gpu_temp, opts.gpu_temp_max_c)
        }
        _ => false,
    }
}

fn format_network_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() {
        return "N/A".into();
    }
    let bytes_per_second = bytes_per_second.max(0.0);
    if bytes_per_second < 125_000.0 {
        let kbps = bytes_per_second / 125.0;
        if kbps.round() < 1_000.0 {
            format!("{}Kbps", kbps.round() as u64)
        } else {
            format!("{:.1}Kbps", (kbps * 10.0).floor() / 10.0)
        }
    } else if bytes_per_second < 125_000_000.0 {
        let mbps = bytes_per_second / 125_000.0;
        if mbps < 10.0 {
            format!("{mbps:.1}Mbps")
        } else if mbps.round() < 1_000.0 {
            format!("{:.0}Mbps", mbps)
        } else {
            format!("{:.1}Mbps", (mbps * 10.0).floor() / 10.0)
        }
    } else {
        let gbps = bytes_per_second / 125_000_000.0;
        if gbps < 10.0 {
            format!("{gbps:.1}Gbps")
        } else if gbps.round() < 1_000.0 {
            format!("{:.0}Gbps", gbps)
        } else {
            ">999Gbps".into()
        }
    }
}

fn format_network_graph_rate(prefix: &str, bytes_per_second: f64, max_width: i32) -> String {
    let bytes = bytes_per_second.max(0.0);
    let units = [
        (125.0, "Kbps"),
        (125_000.0, "Mbps"),
        (125_000_000.0, "Gbps"),
    ];
    let mut unit = units
        .iter()
        .rposition(|(scale, _)| bytes >= *scale)
        .unwrap_or(0);
    let mut rounded = (bytes / units[unit].0).round();
    if rounded >= 1000.0 && unit < units.len() - 1 {
        unit += 1;
        rounded = (bytes / units[unit].0).round();
    }
    let reading = if unit == units.len() - 1 && rounded >= 1000.0 {
        ">9Gbps".to_string()
    } else {
        format!("{rounded:.0}{}", units[unit].1)
    };
    let text = format!("{prefix} {reading}");
    debug_assert!(text_width(&text, 1) <= max_width);
    text
}

fn combined_network_metric(network_in: Metric, network_out: Metric) -> Metric {
    if !network_in.valid
        || !network_out.valid
        || !network_in.value.is_finite()
        || !network_out.value.is_finite()
    {
        return Metric::invalid();
    }
    let value = network_in.value + network_out.value;
    if !value.is_finite() {
        return Metric::invalid();
    }
    Metric {
        value,
        valid: true,
        stale: network_in.stale || network_out.stale,
        updated: match (network_in.updated, network_out.updated) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
    }
}

fn network_graph_height(bytes_per_second: f64, ceiling_mbps: f64, max_height: i32) -> i32 {
    if !bytes_per_second.is_finite()
        || bytes_per_second <= 0.0
        || !ceiling_mbps.is_finite()
        || ceiling_mbps <= 0.0
        || max_height <= 0
    {
        return 0;
    }
    ((bytes_per_second / (ceiling_mbps * 125_000.0) * max_height as f64).ceil() as i32)
        .clamp(1, max_height)
}

fn metric_percent(m: &Metric) -> i32 {
    if !m.valid || m.stale {
        return -1;
    }
    let v = m.value.round() as i64;
    v.clamp(0, 100) as i32
}

fn display_state_reading(reading: &crate::model::Reading) -> DisplayState {
    if !reading.has_value {
        return DisplayState::Unavailable;
    }
    if reading.freshness == Freshness::Stale {
        return DisplayState::Stale;
    }
    if reading.availability == Availability::Available && reading.freshness == Freshness::Current {
        return DisplayState::Current;
    }
    DisplayState::Unavailable
}

fn display_state_kind(reading: &crate::model::Reading, kind: ValueKind) -> DisplayState {
    if !reading.has_value || reading.kind != kind {
        return DisplayState::Unavailable;
    }
    display_state_reading(reading)
}

fn canonical_percent_metric(
    readings: &ReadingsSnapshot,
    key: MetricKey,
    hardware_id: &str,
) -> Metric {
    let Some(reading) = readings.lookup(key, hardware_id) else {
        return Metric::invalid();
    };
    let state = display_state_kind(reading, ValueKind::Percent);
    if state == DisplayState::Unavailable
        || !reading.number.is_finite()
        || !(0.0..=100.0).contains(&reading.number)
    {
        return Metric::invalid();
    }
    let mut metric = reading.legacy_metric();
    metric.valid = true;
    metric.stale = state == DisplayState::Stale;
    metric
}

fn canonical_cpu_domain_temperature(readings: &ReadingsSnapshot, role: HardwareRole) -> Metric {
    if role != HardwareRole::Cache && role != HardwareRole::Frequency {
        return Metric::invalid();
    }
    let mut hardware_id = String::new();
    let mut matches = 0;
    for descriptor in &readings.hardware {
        if descriptor.kind != HardwareKind::CPUDomain || descriptor.role != role {
            continue;
        }
        matches += 1;
        if matches > 1 || descriptor.hardware_id.is_empty() {
            return Metric::invalid();
        }
        hardware_id = descriptor.hardware_id.clone();
    }
    if matches != 1 {
        return Metric::invalid();
    }

    let mut reading_matches = 0;
    let mut reading = None;
    for candidate in &readings.metrics {
        if candidate.key != MetricKey::CPUCCDTemp || candidate.hardware_id != hardware_id {
            continue;
        }
        reading_matches += 1;
        if reading_matches > 1 {
            return Metric::invalid();
        }
        reading = Some(candidate);
    }
    let Some(reading) = reading else {
        return Metric::invalid();
    };
    let state = display_state_kind(reading, ValueKind::Celsius);
    if state == DisplayState::Unavailable
        || !reading.number.is_finite()
        || !(-50.0..=200.0).contains(&reading.number)
    {
        return Metric::invalid();
    }
    let mut metric = reading.legacy_metric();
    metric.valid = true;
    metric.stale = state == DisplayState::Stale;
    metric
}

fn slot_bounds(i: usize) -> (i32, i32) {
    let x = (i * 40) as i32;
    let w = if i == 3 { 41 } else { 40 };
    (x, w)
}

fn hang_slot(f: &mut Frame, slot: usize, hung: &[HungTarget], v: &View) {
    if hung.is_empty() {
        let (x, w) = slot_bounds(slot);
        f.text_centered(x + 1, w - 2, 27, "PROC HANG", 1, true);
        f.text_centered(x + 1, w - 2, 35, "CLEAR", 1, true);
        return;
    }
    let (x, w) = slot_bounds(slot);
    let left = x + 1;
    let width = w - 2;
    let idx = v.hung_index % hung.len();
    let h = &hung[idx];
    f.text_centered(
        left,
        width,
        27,
        &fit_text(&format!("HANG {}/{}", idx + 1, hung.len()), width, 1),
        1,
        true,
    );
    let text = if v.hung_detail {
        &h.title
    } else {
        &h.process_name
    };
    f.text_centered(left, width, 35, &fit_text(text, width, 1), 1, true);
    let fill = (width as f64 * v.hung_hold.clamp(0.0, 1.0)) as i32;
    if fill > 0 {
        f.h_line(left, left + fill - 1, 26, true);
    }
}

#[allow(clippy::needless_range_loop)]
fn headset_slot(f: &mut Frame, x: i32, w: i32, b: &HeadsetBattery, disabled: bool) {
    let charging = !disabled && !b.stale && b.present && b.online && b.charging;
    f.text_centered(
        x,
        w,
        27,
        if charging { "CHARGING" } else { "SS BAT" },
        1,
        true,
    );
    if disabled {
        f.text_centered(x, w, 35, "DISABLED", 1, true);
        return;
    }
    if b.stale {
        f.text_centered(x, w, 35, "STALE", 1, true);
        return;
    }
    if !b.present {
        f.text_centered(x, w, 35, "NO DONGLE", 1, true);
        return;
    }
    if !b.online {
        f.text_centered(x, w, 35, "OFFLINE", 1, true);
        return;
    }
    let mut segments = b.raw_level;
    if segments < 0 {
        segments = (b.percent + 24) / 25;
    }
    let segments = segments.clamp(0, 4);
    for i in 0..4 {
        f.rect(x + i * 5, 34, 4, 7, true);
        if i < segments {
            f.fill_rect(x + i * 5 + 1, 35, 2, 5, true);
        }
    }
    let text = format!("{}%", b.percent);
    f.text_right(x + w - 1, 35, &fit_text(&text, w - 21, 1), true);
}

fn controller_slot(f: &mut Frame, x: i32, w: i32, b: &crate::model::ControllerBattery) {
    f.text_centered(x, w, 27, "PAD BAT", 1, true);
    if !b.connected {
        f.text_centered(x, w, 35, "OFFLINE", 1, true);
        return;
    }
    if b.type_name.eq_ignore_ascii_case("WIRED") {
        f.text_centered(x, w, 35, &format!("P{} WIRED", b.index + 1), 1, true);
        return;
    }
    f.text_centered(
        x,
        w,
        35,
        &format!("P{} {}%", b.index + 1, b.percent),
        1,
        true,
    );
}

fn alert_slot(f: &mut Frame, x: i32, w: i32, alerts: &[crate::model::Alert]) {
    f.text_centered(x, w, 27, "ALERTS", 1, true);
    let mut n = 0;
    let mut max = 0;
    for a in alerts {
        if !a.acknowledged {
            n += 1;
            if a.severity > max {
                max = a.severity;
            }
        }
    }
    if n == 0 {
        f.text_centered(x, w, 35, "CLEAR", 1, true);
    } else {
        f.text_centered(x, w, 35, &format_alert_summary(n, max, w), 1, true);
    }
}

fn format_alert_summary(count: i32, severity: i32, max_width: i32) -> String {
    let text = format!("{count} L {severity}");
    if text_width(&text, 1) <= max_width {
        text
    } else {
        format!(">9 L {}", severity.clamp(0, 9))
    }
}

fn provider_slot(f: &mut Frame, x: i32, w: i32, providers: &HashMap<String, bool>) {
    f.text_centered(x, w, 27, "STATUS", 1, true);
    let bad = providers.values().filter(|&&available| !available).count();
    if bad == 0 {
        f.text_centered(x, w, 35, "OK", 1, true);
    } else {
        f.text_centered(x, w, 35, &format!("{} DOWN", bad), 1, true);
    }
}

fn first_non_empty(values: &[&str], fallback: &str) -> String {
    for v in values {
        if !v.trim().is_empty() {
            return (*v).to_string();
        }
    }
    fallback.to_string()
}

const IEC_UNITS: [(u64, &str); 7] = [
    (1, "B"),
    (1 << 10, "KiB"),
    (1 << 20, "MiB"),
    (1 << 30, "GiB"),
    (1 << 40, "TiB"),
    (1 << 50, "PiB"),
    (1 << 60, "EiB"),
];

#[allow(clippy::needless_range_loop)]
fn format_iec_usage(used: u64, total: u64, max_width: i32) -> String {
    if total == 0 || used > total || max_width <= 0 {
        return "N/A".to_string();
    }
    let mut start = 0;
    for index in (1..IEC_UNITS.len()).rev() {
        if total >= IEC_UNITS[index].0 {
            start = index;
            break;
        }
    }
    for index in start..IEC_UNITS.len() {
        let (unit, name) = IEC_UNITS[index];
        let left = format_iec_number(used, unit);
        let right = format_iec_number(total, unit);
        let text = format!("{}/{}{}", left, right, name);
        if text_width(&text, 1) <= max_width {
            return text;
        }
        let compact = format!(
            "{}/{}{}",
            compact_iec_number(&left),
            compact_iec_number(&right),
            name
        );
        if text_width(&compact, 1) <= max_width {
            return compact;
        }
        let rounded = format!(
            "{}/{}{}",
            format_iec_whole(used, unit),
            format_iec_whole(total, unit),
            name
        );
        if text_width(&rounded, 1) <= max_width {
            return rounded;
        }
    }
    "N/A".to_string()
}

fn format_iec_whole(value: u64, unit: u64) -> u64 {
    let mut whole = value / unit;
    if value % unit >= unit.div_ceil(2) {
        whole += 1;
    }
    whole
}

fn format_iec_number(value: u64, unit: u64) -> String {
    let mut whole = value / unit;
    let remainder = value % unit;
    if remainder == 0 {
        return whole.to_string();
    }
    let mut tenths = (remainder * 10 + unit / 2) / unit;
    if tenths == 10 {
        whole += 1;
        tenths = 0;
    }
    if tenths == 0 {
        whole.to_string()
    } else {
        format!("{}.{}", whole, tenths)
    }
}

fn compact_iec_number(value: &str) -> String {
    if let Some(stripped) = value.strip_prefix("0.") {
        format!(".{}", stripped)
    } else {
        value.to_string()
    }
}

fn visible_speakers(
    d: &crate::model::DiscordState,
    now: Option<SystemTime>,
    linger: Duration,
    show_self: bool,
) -> Vec<Speaker> {
    if !d.connected || !d.authenticated {
        return Vec::new();
    }
    let Some(now) = now else {
        return Vec::new();
    };
    let mut out: Vec<Speaker> = d
        .speakers
        .iter()
        .filter(|s| {
            if s.is_self && !show_self {
                return false;
            }
            s.speaking
                || s.stopped_at
                    .map(|stopped| now.duration_since(stopped).unwrap_or(Duration::ZERO) <= linger)
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        b.speaking
            .cmp(&a.speaking)
            .then_with(|| b.started_at.cmp(&a.started_at))
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    out
}

fn discord_overlay(
    f: &mut Frame,
    d: &crate::model::DiscordState,
    speakers: &[Speaker],
    opts: &OverlayOptions,
) {
    f.rect(0, 0, 160, 43, true);
    let mut title = "DISCORD".to_string();
    if opts.discord_show_channel && !d.channel_name.is_empty() {
        title = fit_text(&d.channel_name, 154, 1);
    }
    f.text_centered(3, 154, 2, &title, 1, true);
    let max = opts.discord_max_speakers.min(speakers.len());
    if max == 1 {
        let name = fit_text(&speakers[0].name, 152, 2);
        f.text_centered(4, 152, 16, &name, 2, true);
    } else {
        let mut y = 11;
        for speaker in speakers.iter().take(max) {
            let name = fit_text(&speaker.name, 152, 1);
            f.text_centered(4, 152, y, &name, 1, true);
            y += 8;
        }
    }
    if speakers.len() > max {
        f.text_right(156, 36, &format!("+{}", speakers.len() - max), true);
    }
}

fn highest_critical<'a>(
    alerts: &'a [crate::model::Alert],
    ack: &HashSet<String>,
    suppress_temperature: bool,
) -> Option<&'a crate::model::Alert> {
    let mut best: Option<&crate::model::Alert> = None;
    for a in alerts {
        if a.severity < 2
            || a.acknowledged
            || ack.contains(&a.id)
            || (suppress_temperature && matches!(a.id.as_str(), "cpu-temp" | "gpu-temp"))
        {
            continue;
        }
        match best {
            None => best = Some(a),
            Some(b) if a.severity > b.severity => best = Some(a),
            _ => {}
        }
    }
    best
}

fn alert_overlay(f: &mut Frame, a: &crate::model::Alert) {
    f.rect(0, 0, 160, 43, true);
    f.text_centered(3, 154, 3, &fit_text(&a.title, 154, 2), 2, true);
    f.text_centered(3, 154, 30, &fit_text(&a.detail, 154, 1), 1, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Alert, ControllerBattery, GameStats, Reading};
    use std::time::{Duration, SystemTime};

    const GOLDEN_NORMAL: &str = "a2f36db6c54c09adcd459756677a6fc348f181d01e0810a9c7e4d9c3c998c20c";
    const GOLDEN_UNAVAILABLE: &str =
        "387f5269dded63f8c38cd965b55f02fbb1e2552e89c90e5353c34ec3e1058aa9";
    const GOLDEN_STALE: &str = "f7bc46cc0f4763084dabf1445a2240e08ec892ed2c3c49991e9428296d69bdb8";
    const GOLDEN_LAYOUT_2: &str =
        "c6bf7ef1e919ef47dfc7ed13ef9a9f937253e47a449fe8558f00adf98a881701";
    const GOLDEN_LAYOUT_3: &str =
        "ef1dbde8de40632b213b7c149789a716165719b0ab4f40ab80aa508930b6ff8b";

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn sample_snapshot() -> Snapshot {
        let now = at(1_800_000_000);
        let gpu_hw = "luid_0x00000000_0x00016343_phys_0";
        let mut s = Snapshot {
            now: Some(now),
            cpu_dual: true,
            date_text: "2026-08-08 Saturday".into(),
            date_short_text: "2026-08-08 Sat".into(),
            time_text: "10:44:14".into(),
            cpu_cache_load: Metric::valid(22.0, now),
            cpu_freq_load: Metric::valid(47.0, now),
            cpu_temp: Metric::valid(68.0, now),
            gpu_temp: Metric::valid(74.0, now),
            game: GameStats {
                fps: Metric::valid(144.0, now),
                one_percent: Metric::valid(118.0, now),
                ..Default::default()
            },
            headset: crate::model::HeadsetBattery {
                present: true,
                online: true,
                percent: 75,
                raw_level: 3,
                ..Default::default()
            },
            readings: ReadingsSnapshot {
                metrics: vec![
                    Reading::current_percent(MetricKey::CPUUtilization, 54.0, "cpu-system", now),
                    Reading::current_bytes(MetricKey::RAMTotal, 32 << 30, "physical-memory", now),
                    Reading::current_bytes(MetricKey::RAMUsed, 20 << 30, "physical-memory", now),
                    Reading::current_percent(
                        MetricKey::RAMUtilization,
                        63.0,
                        "physical-memory",
                        now,
                    ),
                    Reading::current_percent(MetricKey::GPUUtilization, 91.0, gpu_hw, now),
                    Reading::current_bytes(MetricKey::VRAMTotal, 16 << 30, gpu_hw, now),
                    Reading::current_bytes(
                        MetricKey::VRAMUsed,
                        (16u64 << 30) * 72 / 100,
                        gpu_hw,
                        now,
                    ),
                    Reading::current_percent(MetricKey::VRAMUtilization, 72.0, gpu_hw, now),
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        s.controller = ControllerBattery::default();
        s
    }

    fn audit_snapshot() -> Snapshot {
        let mut snapshot = sample_snapshot();
        let now = snapshot.now.unwrap();
        snapshot.network_in = Metric::valid(1_250_000.0, now);
        snapshot.network_out = Metric::valid(842_000.0, now);
        snapshot.ping_ms = Metric::valid(24.0, now);
        snapshot.jitter_ms = Metric::valid(4.0, now);
        snapshot.packet_loss = Metric::valid(1.0, now);
        snapshot.audio_volume = Metric::valid(73.0, now);
        snapshot.microphone_known = true;
        snapshot.game.active = true;
        snapshot.game.game_name = "GAME".into();
        snapshot.game.process_name = "game.exe".into();
        snapshot.game.point_one_low = Metric::valid(96.0, now);
        snapshot.game.frame_time_ms = Metric::valid(17.0, now);
        snapshot.game.session_start = Some(now - Duration::from_secs(12 * 60 + 34));
        snapshot.game.stutters = 8;
        snapshot.headset.charging = true;
        snapshot.headset.percent = 100;
        snapshot.headset.raw_level = 4;
        snapshot.controller = ControllerBattery {
            connected: true,
            index: 0,
            type_name: "BATTERY".into(),
            percent: 100,
        };
        snapshot.providers.insert("discord".into(), false);
        snapshot.alerts = vec![
            Alert {
                severity: 3,
                ..Default::default()
            },
            Alert {
                severity: 2,
                ..Default::default()
            },
        ];
        snapshot.system_battery = crate::model::SystemBattery {
            ac: AcState::Online,
            battery_present: Some(true),
            percent: Some(73),
            freshness: Freshness::Current,
            ..Default::default()
        };
        snapshot.bottleneck = crate::model::BottleneckReading {
            state: crate::model::BottleneckState::Gpu,
            freshness: Freshness::Current,
        };
        snapshot.readings.hardware = vec![
            crate::model::HardwareDescriptor {
                hardware_id: "domain-cache".into(),
                kind: HardwareKind::CPUDomain,
                role: HardwareRole::Cache,
            },
            crate::model::HardwareDescriptor {
                hardware_id: "domain-frequency".into(),
                kind: HardwareKind::CPUDomain,
                role: HardwareRole::Frequency,
            },
        ];
        snapshot.readings.metrics.extend([
            Reading::current_celsius(MetricKey::CPUCCDTemp, 69.0, "domain-cache", now),
            Reading::current_celsius(MetricKey::CPUCCDTemp, 65.0, "domain-frequency", now),
            Reading::current_number(MetricKey::VRMTemp, ValueKind::Celsius, 58.0, "vrm", now),
            Reading::current_number(
                MetricKey::ChipsetTemp,
                ValueKind::Celsius,
                61.0,
                "chipset",
                now,
            ),
            Reading::current_number(
                MetricKey::MotherboardTemp,
                ValueKind::Celsius,
                42.0,
                "board",
                now,
            ),
            Reading::current_number(
                MetricKey::CPUFanPercent,
                ValueKind::Percent,
                72.0,
                "fan",
                now,
            ),
            Reading::current_number(
                MetricKey::PumpPercent,
                ValueKind::Percent,
                83.0,
                "pump",
                now,
            ),
            Reading::current_number(MetricKey::TotalPower, ValueKind::Watts, 496.0, "power", now),
            Reading::current_number(
                MetricKey::CPUGPUPower,
                ValueKind::Watts,
                412.0,
                "subtotal",
                now,
            ),
            Reading::current_number(
                MetricKey::DiskReadBytesPerSec,
                ValueKind::ByteRate,
                1_250_000.0,
                "disk-total",
                now,
            ),
            Reading::current_number(
                MetricKey::DiskWriteBytesPerSec,
                ValueKind::ByteRate,
                842_000.0,
                "disk-total",
                now,
            ),
            Reading::current_number(
                MetricKey::EstablishedConnections,
                ValueKind::Count,
                123.0,
                "tcp",
                now,
            ),
            Reading::current_number(
                MetricKey::PageReadsPerSec,
                ValueKind::Count,
                17.0,
                "memory",
                now,
            ),
        ]);
        snapshot
    }

    fn edge_snapshot() -> Snapshot {
        let mut snapshot = audit_snapshot();
        let now = snapshot.now.unwrap();
        snapshot.date_text = "WEDNESDAY 9999-12-31".into();
        snapshot.date_short_text = "9999-12-31 WED".into();
        snapshot.time_text = "23:59:59".into();
        snapshot.cpu_cache_load = Metric::valid(100.0, now);
        snapshot.cpu_freq_load = Metric::valid(100.0, now);
        snapshot.cpu_temp = Metric::valid(f64::MAX, now);
        snapshot.gpu_temp = Metric::valid(f64::MAX, now);
        snapshot.network_in = Metric::valid(f64::MAX, now);
        snapshot.network_out = Metric::valid(f64::MAX, now);
        snapshot.ping_ms = Metric::valid(f64::MAX, now);
        snapshot.jitter_ms = Metric::valid(f64::MAX, now);
        snapshot.packet_loss = Metric::valid(f64::MAX, now);
        snapshot.audio_volume = Metric::valid(f64::MAX, now);
        snapshot.game.fps = Metric::valid(f64::MAX, now);
        snapshot.game.point_one_low = Metric::valid(f64::MAX, now);
        snapshot.game.one_percent = Metric::valid(f64::MAX, now);
        snapshot.game.frame_time_ms = Metric::valid(f64::MAX, now);
        snapshot.game.session_start = Some(SystemTime::UNIX_EPOCH);
        snapshot.game.stutters = i32::MAX;
        snapshot.game.game_name = "EXTREMELY-LONG-GAME-NAME-FOR-FITTING".into();
        snapshot.game.process_name = "EXTREMELY-LONG-PROCESS-NAME.EXE".into();
        snapshot.headset.percent = 100;
        snapshot.controller.percent = 100;
        snapshot.system_battery.percent = Some(100);
        snapshot.providers = (0..100)
            .map(|index| (format!("provider-{index}"), false))
            .collect();
        snapshot.alerts = (0..100)
            .map(|_| Alert {
                severity: 3,
                ..Default::default()
            })
            .collect();
        for reading in &mut snapshot.readings.metrics {
            match reading.kind {
                ValueKind::Percent => reading.number = 100.0,
                ValueKind::Bytes => reading.bytes = u64::MAX,
                _ => reading.number = f64::MAX,
            }
        }
        snapshot
    }

    fn status_snapshot() -> Snapshot {
        let mut snapshot = audit_snapshot();
        for metric in [
            &mut snapshot.cpu_cache_load,
            &mut snapshot.cpu_freq_load,
            &mut snapshot.cpu_temp,
            &mut snapshot.gpu_temp,
            &mut snapshot.network_in,
            &mut snapshot.network_out,
            &mut snapshot.ping_ms,
            &mut snapshot.jitter_ms,
            &mut snapshot.packet_loss,
            &mut snapshot.audio_volume,
            &mut snapshot.game.fps,
            &mut snapshot.game.one_percent,
            &mut snapshot.game.point_one_low,
            &mut snapshot.game.frame_time_ms,
        ] {
            metric.stale = true;
        }
        for reading in &mut snapshot.readings.metrics {
            reading.freshness = Freshness::Stale;
        }
        snapshot.microphone_known = false;
        snapshot.game.active = false;
        snapshot.game.game_name.clear();
        snapshot.game.process_name.clear();
        snapshot.headset.stale = true;
        snapshot.controller = ControllerBattery::default();
        snapshot.alerts.clear();
        snapshot.system_battery.freshness = Freshness::Stale;
        snapshot.bottleneck.freshness = Freshness::Stale;
        snapshot
    }

    fn golden_view() -> View {
        View {
            slot_modules: [
                "HEADSET_BATTERY".into(),
                "FPS_CURRENT".into(),
                "GPU_TEMP".into(),
                "FPS_1LOW".into(),
            ],
            ..Default::default()
        }
    }

    fn render_dashboard(s: &Snapshot) -> Frame {
        Renderer::new().render(s, OverlayOptions::default(), &golden_view())
    }

    fn render_main_display(s: &Snapshot, main_display: i32, ceiling_mbps: f64) -> Frame {
        Renderer::new().render(
            s,
            OverlayOptions {
                main_display,
                network_graph_ceiling_download_mbps: ceiling_mbps,
                network_graph_ceiling_upload_mbps: ceiling_mbps,
                ..Default::default()
            },
            &golden_view(),
        )
    }

    #[test]
    fn render_is_deterministic_and_matches_golden() {
        let s = sample_snapshot();
        let a = render_dashboard(&s);
        let b = render_dashboard(&s);
        assert!(a.equal(&b), "frames differ");
        assert_eq!(a.hash_hex(), GOLDEN_NORMAL, "layout 1 dashboard changed");
    }

    #[test]
    fn golden_unavailable_and_stale_dashboards() {
        let mut unavailable = sample_snapshot();
        unavailable.readings.metrics.clear();
        assert_eq!(
            render_dashboard(&unavailable).hash_hex(),
            GOLDEN_UNAVAILABLE
        );

        let mut stale = sample_snapshot();
        for reading in &mut stale.readings.metrics {
            reading.freshness = Freshness::Stale;
        }
        assert_eq!(render_dashboard(&stale).hash_hex(), GOLDEN_STALE);

        let zero = {
            let mut z = sample_snapshot();
            for reading in &mut z.readings.metrics {
                if matches!(
                    reading.key,
                    MetricKey::RAMUtilization
                        | MetricKey::GPUUtilization
                        | MetricKey::VRAMUtilization
                ) {
                    reading.number = 0.0;
                }
            }
            z
        };
        let zero_frame = render_dashboard(&zero);
        assert!(!zero_frame.equal(&render_dashboard(&unavailable)));
        assert!(!zero_frame.equal(&render_dashboard(&stale)));
    }

    #[test]
    fn selectable_layouts_are_deterministic_and_keep_shared_regions_exact() {
        let mut snapshot = sample_snapshot();
        let now = snapshot.now.unwrap();
        snapshot.network_in = Metric::valid(62_500_000.0, now);
        snapshot.network_out = Metric::valid(125_000_000.0, now);

        let classic = render_dashboard(&snapshot);
        let layout_2 = render_main_display(&snapshot, 2, 1000.0);
        let layout_3 = render_main_display(&snapshot, 3, 1000.0);
        assert_eq!(layout_2.hash_hex(), GOLDEN_LAYOUT_2);
        assert_eq!(layout_3.hash_hex(), GOLDEN_LAYOUT_3);
        assert!(layout_2.equal(&render_main_display(&snapshot, 2, 1000.0)));
        assert!(layout_3.equal(&render_main_display(&snapshot, 3, 1000.0)));

        for frame in [&layout_2, &layout_3] {
            for y in (0..=6).chain(25..=42) {
                for x in 0..160 {
                    assert_eq!(frame.get(x, y), classic.get(x, y), "shared pixel ({x},{y})");
                }
            }
        }
        for (frame, dividers) in [
            (&classic, &[79][..]),
            (&layout_2, &[52, 105][..]),
            (&layout_3, &[79][..]),
        ] {
            assert!(frame.get(1, 0), "header text must start at y=0");
            assert!((0..160).all(|x| !frame.get(x, 5)), "row 5 must be blank");
            assert!(
                (0..160).all(|x| frame.get(x, 6)),
                "header separator must be y=6"
            );
            assert!(
                (0..160).all(|x| frame.get(x, 25)),
                "bottom separator must be solid"
            );
            assert!(dividers.iter().all(|&x| frame.get(x, 7)));
        }
        assert!((7..=24).all(|y| layout_2.get(52, y) && layout_2.get(105, y)));
        assert!((7..=24).all(|y| layout_3.get(79, y)));
        for (frame, left, right) in [(&classic, 98, 157), (&layout_2, 71, 104)] {
            for y in [8, 14, 17, 23] {
                assert!((left..=right).all(|x| frame.get(x, y)));
            }
            assert!((left..=right).all(|x| !frame.get(x, 16)));
        }
        for (frame, right) in [(&classic, 77), (&layout_2, 51), (&layout_3, 77)] {
            assert!((14..=right).all(|x| frame.get(x, 8) && frame.get(x, 10)));
            assert!((14..=right).all(|x| frame.get(x, 12) && frame.get(x, 14)));
            assert!((8..=10).all(|y| frame.get(14, y) && frame.get(right, y)));
            assert!((12..=14).all(|y| frame.get(14, y) && frame.get(right, y)));
            assert!(!frame.get(16, 11), "former C glyph cell must be blank");
            assert!(
                !frame.get(16, 15) && !frame.get(16, 16),
                "former F glyph cells must be blank"
            );
        }

        snapshot.cpu_dual = false;
        let single_2 = render_main_display(&snapshot, 2, 1000.0);
        assert!((14..=51).all(|x| single_2.get(x, 8)));
        assert!((8..=14).all(|y| single_2.get(51, y) && !single_2.get(53, y)));
        assert!(!single_2.equal(&layout_2));

        let single_3 = render_main_display(&snapshot, 3, 1000.0);
        let layout_1_single = render_main_display(&snapshot, 1, 1000.0);
        for y in 7..=24 {
            for x in 0..=79 {
                assert_eq!(
                    single_3.get(x, y),
                    layout_1_single.get(x, y),
                    "left pane ({x},{y})"
                );
            }
        }
        assert!(!single_3.equal(&layout_3));
        assert!(
            render_main_display(&snapshot, 4, 1000.0).equal(&layout_1_single),
            "unknown layouts fall back to layout 1"
        );
    }

    #[test]
    fn standard_lower_fills_and_placeholders_stop_before_row_24() {
        let current = sample_snapshot();
        let mut unavailable = current.clone();
        unavailable.readings.metrics.clear();
        let mut stale = current.clone();
        for reading in &mut stale.readings.metrics {
            reading.freshness = Freshness::Stale;
        }
        stale.network_in = Metric::valid(1.0, stale.now.unwrap());
        stale.network_in.stale = true;
        stale.network_out = stale.network_in;

        for snapshot in [&current, &unavailable, &stale] {
            for (main_display, dividers) in [(1, &[79][..]), (2, &[52, 105][..]), (3, &[79][..])] {
                let frame = render_main_display(snapshot, main_display, 1000.0);
                for x in 0..160 {
                    assert_eq!(
                        frame.get(x, 24),
                        dividers.contains(&x),
                        "layout {main_display} row 24 x={x}"
                    );
                    assert!(frame.get(x, 25), "layout {main_display} row 25 x={x}");
                }
            }
        }
    }

    #[test]
    fn all_layouts_use_ram_label_and_explicit_state_patterns() {
        let mut expected_label = Frame::new();
        expected_label.text(1, 18, "RAM", true);
        for main_display in 1..=3 {
            let current = render_main_display(&sample_snapshot(), main_display, 1000.0);
            for y in 18..=22 {
                for x in 1..=11 {
                    assert_eq!(
                        current.get(x, y),
                        expected_label.get(x, y),
                        "layout {main_display} RAM pixel ({x},{y})"
                    );
                }
            }
        }
        let current = render_main_display(&sample_snapshot(), 2, 1000.0);
        for y in [8, 14, 17, 23] {
            assert!((120..=157).all(|x| current.get(x, y)));
        }

        let mut states = sample_snapshot();
        states
            .readings
            .metrics
            .iter_mut()
            .find(|reading| reading.key == MetricKey::RAMUtilization)
            .unwrap()
            .freshness = Freshness::Stale;
        states.network_out = Metric::valid(1.0, states.now.unwrap());
        states.network_out.stale = true;
        states.network_in = Metric::invalid();
        let frame = render_main_display(&states, 2, 1000.0);

        for (x, expected) in [(15, true), (16, true), (17, false), (18, false)] {
            assert_eq!(frame.get(x, 18), expected, "RAM stale stripe x={x}");
        }
        assert!(frame.get(121, 9) && frame.get(122, 9) && !frame.get(123, 9));
        assert!(frame.get(121, 18) && !frame.get(122, 18));
        assert!(!frame.get(121, 19) && frame.get(122, 19));
    }

    #[test]
    fn network_bars_follow_configured_ceiling_in_both_network_layouts() {
        let now = at(50);
        let snapshot = Snapshot {
            network_in: Metric::valid(6_250_000.0, now),
            network_out: Metric::valid(12_500_000.0, now),
            ..Default::default()
        };

        let thirds = render_main_display(&snapshot, 2, 100.0);
        assert!((120..=157).all(|x| thirds.get(x, 8) && thirds.get(x, 14)));
        assert!((120..=157).all(|x| thirds.get(x, 17) && thirds.get(x, 23)));
        assert!((121..=156).all(|x| (9..=13).all(|y| thirds.get(x, y))));
        assert!((121..=138).all(|x| (18..=22).all(|y| thirds.get(x, y))));
        assert!((139..=156).all(|x| (18..=22).all(|y| !thirds.get(x, y))));

        let network = render_main_display(&snapshot, 3, 100.0);
        assert!((110..=157).all(|x| network.get(x, 8) && network.get(x, 14)));
        assert!((110..=157).all(|x| network.get(x, 17) && network.get(x, 23)));
        assert!((111..=133).all(|x| (9..=13).all(|y| network.get(x, y))));
        assert!((134..=156).all(|x| (9..=13).all(|y| !network.get(x, y))));
        assert!((111..=156).all(|x| (18..=22).all(|y| network.get(x, y))));
    }

    #[test]
    fn final_layout_3_matches_layout_1_left_and_ignores_gpu_readings() {
        let snapshot = sample_snapshot();
        let classic = render_main_display(&snapshot, 1, 1000.0);
        let current = render_main_display(&snapshot, 3, 1000.0);
        assert!((7..=24).all(|y| current.get(79, y)));
        for y in 7..=24 {
            for x in 0..=79 {
                assert_eq!(current.get(x, y), classic.get(x, y), "left pane ({x},{y})");
            }
        }
        for y in [8, 14, 17, 23] {
            assert!((110..=157).all(|x| current.get(x, y)));
        }

        let mut gpu_changed = snapshot.clone();
        gpu_changed.readings.metrics.retain(|reading| {
            !matches!(
                reading.key,
                MetricKey::GPUUtilization
                    | MetricKey::VRAMUtilization
                    | MetricKey::VRAMUsed
                    | MetricKey::VRAMTotal
            )
        });
        assert!(current.equal(&render_main_display(&gpu_changed, 3, 1000.0)));

        let mut states = snapshot;
        states.network_in = Metric::valid(1.0, states.now.unwrap());
        states.network_in.stale = true;
        states.network_out = Metric::invalid();
        let frame = render_main_display(&states, 3, 1000.0);
        assert!(frame.get(111, 9) && frame.get(112, 9) && !frame.get(113, 9));
        assert!(frame.get(111, 18) && !frame.get(112, 18));
        assert!(!frame.get(111, 19) && frame.get(112, 19));
    }

    #[test]
    fn attached_target_snapshot_matches() {
        let snapshot = Snapshot {
            cpu_dual: true,
            date_text: "2026-08-26 WEDNESDAY".into(),
            date_short_text: "2026-08-26 WED".into(),
            time_text: "16:42:13".into(),
            ..Default::default()
        };
        let view = View {
            slot_modules: [
                "HEADSET_BATTERY".into(),
                "FPS_CURRENT".into(),
                "GPU_TEMP".into(),
                "PACKET_LOSS".into(),
            ],
            ..Default::default()
        };

        let hash = Renderer::new()
            .render(&snapshot, OverlayOptions::default(), &view)
            .hash_hex();
        assert_eq!(
            hash,
            "375a955ea619691e07c49c69e111e3c0831ba3fb338da116c0007c360684e76d"
        );
    }

    #[test]
    fn utilization_bars_use_solid_and_explicit_state_patterns() {
        let now = at(42);
        let cases = vec![
            ("zero", Metric::valid(0.0, now)),
            ("partial", Metric::valid(50.0, now)),
            ("full", Metric::valid(100.0, now)),
            ("below-range", Metric::valid(-10.0, now)),
            ("above-range", Metric::valid(125.0, now)),
            ("unavailable", Metric::invalid()),
            (
                "stale",
                Metric {
                    value: 0.0,
                    valid: true,
                    stale: true,
                    updated: None,
                },
            ),
        ];
        let label_width = text_width("RAM", 1);
        for (name, metric) in cases {
            // Full bar
            let mut r = Renderer::new();
            let mut f = Frame::new();
            r.metric_bar(&mut f, 1, 18, "RAM", &metric, 75, 7);
            let bx = 1 + label_width + 2;
            let p = metric_percent(&metric);
            let want_filled = if p >= 0 {
                (75 - label_width - 4) * p / 100
            } else {
                0
            };
            match name {
                "unavailable" => {
                    assert_bar_interior_pattern(&f, bx + 1, 19, 75 - label_width - 4, 5, |x, y| {
                        (x + y) % 2 == 0
                    })
                }
                "stale" => {
                    assert_bar_interior_pattern(&f, bx + 1, 19, 75 - label_width - 4, 5, |x, _| {
                        x % 4 < 2
                    })
                }
                _ => assert_bar_interior(&f, bx + 1, 19, 75 - label_width - 4, 5, want_filled),
            }

            // Small bar
            let mut r = Renderer::new();
            let mut f = Frame::new();
            r.small_bar(&mut f, 20, 9, metric, 58);
            let want_filled = if p >= 0 { 56 * p / 100 } else { 0 };
            match name {
                "unavailable" => {
                    assert_bar_interior_pattern(&f, 21, 10, 56, 1, |x, y| (x + y) % 2 == 0)
                }
                "stale" => assert_bar_interior_pattern(&f, 21, 10, 56, 1, |x, _| x % 4 < 2),
                _ => assert_bar_interior(&f, 21, 10, 56, 1, want_filled),
            }
        }
    }

    fn assert_bar_interior(f: &Frame, x: i32, y: i32, width: i32, height: i32, filled: i32) {
        for yy in y..y + height {
            for offset in 0..width {
                assert_eq!(
                    f.get(x + offset, yy),
                    offset < filled,
                    "interior pixel ({},{}) filled={}",
                    x + offset,
                    yy,
                    filled
                );
            }
        }
    }

    fn assert_bar_interior_pattern(
        f: &Frame,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        want: impl Fn(i32, i32) -> bool,
    ) {
        for yy in 0..height {
            for xx in 0..width {
                assert_eq!(
                    f.get(x + xx, y + yy),
                    want(xx, yy),
                    "interior pixel ({},{})",
                    x + xx,
                    y + yy
                );
            }
        }
    }

    #[test]
    fn fixed_dashboard_uses_vram_label_only() {
        let current = render_dashboard(&sample_snapshot());
        let mut previous = current.clone();
        previous.fill_rect(81, 18, text_width("VRAM", 1), 5, false);
        previous.text(81, 18, "VMEM", true);

        let changed: Vec<_> = current
            .pixels
            .iter()
            .zip(&previous.pixels)
            .enumerate()
            .filter(|(_, (current, previous))| current != previous)
            .map(|(i, _)| ((i % WIDTH) as i32, (i / WIDTH) as i32))
            .collect();
        assert_eq!(changed.len(), 10);
        assert!(
            changed
                .iter()
                .all(|&(x, y)| (86..=91).contains(&x) && (18..=22).contains(&y)),
            "label diff escaped audited bounds: {changed:?}"
        );
    }

    #[test]
    fn layout_one_bars_ignore_noncanonical_sources() {
        // Layout 1 RAM/GPU/VRAM bars read only the canonical readings; a
        // change to unrelated snapshot fields (temps feed slots, not bars)
        // must leave the bar regions pixel-identical.
        let s = sample_snapshot();
        let base = render_dashboard(&s);
        let mut changed = s.clone();
        changed.cpu_temp = Metric::valid(99.0, s.now.unwrap());
        changed.gpu_temp = Metric::valid(99.0, s.now.unwrap());
        let after = render_dashboard(&changed);
        // RAM bar region: x=14..75, y=17..23. GPU bar region: x=98..157, y=8..14.
        for y in 17..24 {
            for x in 14..76 {
                assert_eq!(
                    base.get(x, y),
                    after.get(x, y),
                    "RAM bar pixel ({},{})",
                    x,
                    y
                );
            }
        }
        for y in 8..15 {
            for x in 98..158 {
                assert_eq!(
                    base.get(x, y),
                    after.get(x, y),
                    "GPU bar pixel ({},{})",
                    x,
                    y
                );
            }
        }
        assert!(
            !base.equal(&after),
            "temps must still change the slot modules"
        );
    }

    #[test]
    fn discord_overlay_draws_border() {
        let mut s = sample_snapshot();
        s.discord.connected = true;
        s.discord.authenticated = true;
        s.discord.speakers.push(Speaker {
            user_id: "1".into(),
            name: "Nezumi".into(),
            speaking: true,
            started_at: s.now,
            ..Default::default()
        });
        let f = Renderer::new().render(&s, OverlayOptions::default(), &View::default());
        assert!(f.get(0, 0) && f.get(159, 42), "missing overlay border");
    }

    #[test]
    fn discord_visibility_order_linger_self_and_restoration() {
        let mut s = sample_snapshot();
        let dashboard = render_dashboard(&s);
        let now = s.now.unwrap();
        s.discord.connected = true;
        s.discord.authenticated = true;
        s.discord.channel_name = "General".into();
        s.discord.speakers = vec![
            Speaker {
                user_id: "2".into(),
                name: "Older".into(),
                speaking: true,
                started_at: Some(now - Duration::from_secs(2)),
                ..Default::default()
            },
            Speaker {
                user_id: "1".into(),
                name: "Newer".into(),
                speaking: true,
                started_at: Some(now - Duration::from_secs(1)),
                ..Default::default()
            },
            Speaker {
                user_id: "self".into(),
                name: "Self".into(),
                speaking: true,
                is_self: true,
                started_at: Some(now),
                ..Default::default()
            },
            Speaker {
                user_id: "linger".into(),
                name: "Linger".into(),
                stopped_at: Some(now - Duration::from_millis(700)),
                ..Default::default()
            },
        ];
        let visible = visible_speakers(&s.discord, s.now, Duration::from_millis(700), false);
        assert_eq!(
            visible
                .iter()
                .map(|speaker| speaker.user_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "linger"]
        );
        assert_eq!(
            visible_speakers(&s.discord, s.now, Duration::from_millis(699), false).len(),
            2
        );
        assert_eq!(
            visible_speakers(&s.discord, s.now, Duration::from_millis(700), true).len(),
            4
        );

        let one = Renderer::new().render(
            &s,
            OverlayOptions {
                discord_max_speakers: 1,
                discord_show_channel: true,
                ..Default::default()
            },
            &golden_view(),
        );
        let many = Renderer::new().render(
            &s,
            OverlayOptions {
                discord_max_speakers: 2,
                discord_show_channel: true,
                ..Default::default()
            },
            &golden_view(),
        );
        assert!(
            !one.equal(&many),
            "one/two speaker and +N layouts must differ"
        );

        for speaker in &mut s.discord.speakers {
            speaker.speaking = false;
            speaker.stopped_at = Some(now - Duration::from_millis(701));
        }
        assert!(
            Renderer::new()
                .render(&s, OverlayOptions::default(), &golden_view())
                .equal(&dashboard),
            "expired overlay must restore the exact dashboard"
        );
    }

    #[test]
    fn iec_formatting_matches_go() {
        let cases = [
            (20u64 << 30, 32u64 << 30, 38, "20/32GiB"),
            (512 << 20, 1 << 30, 38, "0.5/1GiB"),
            ((16u64 << 30) * 72 / 100, 16 << 30, 38, "12/16GiB"),
            (128 << 30, 256 << 30, 38, ".1/.3TiB"),
            (512, 1024, 38, "0.5/1KiB"),
            (0, 16 << 30, 38, "0/16GiB"),
            (0, 0, 38, "N/A"),
            (2, 1, 38, "N/A"),
        ];
        for (used, total, width, want) in cases {
            assert_eq!(
                format_iec_usage(used, total, width),
                want,
                "used={} total={}",
                used,
                total
            );
        }
    }

    #[test]
    fn network_rates_use_compact_decimal_si_units_without_lower_unit_rollover() {
        let cases = [
            (-1.0, "0Kbps"),
            (0.0, "0Kbps"),
            (124.0, "1Kbps"),
            (999_499.0 / 8.0, "999Kbps"),
            (999_500.0 / 8.0, "999.5Kbps"),
            (999_999.0 / 8.0, "999.9Kbps"),
            (1_000_000.0 / 8.0, "1.0Mbps"),
            (12_400_000.0 / 8.0, "12Mbps"),
            (999_499_999.0 / 8.0, "999Mbps"),
            (999_999_999.0 / 8.0, "999.9Mbps"),
            (1_000_000_000.0 / 8.0, "1.0Gbps"),
            (10_000_000_000.0 / 8.0, "10Gbps"),
            (999_499_999_999.0 / 8.0, "999Gbps"),
            (999_500_000_000.0 / 8.0, ">999Gbps"),
            (f64::MAX, ">999Gbps"),
        ];
        for (bytes_per_second, expected) in cases {
            assert_eq!(format_network_rate(bytes_per_second), expected);
            assert!(text_width(expected, 1) <= 38, "{expected} does not fit");
            assert!(!expected.contains("INF"));
        }
        assert_eq!(format_network_rate(f64::NAN), "N/A");
        assert_eq!(format_network_rate(f64::INFINITY), "N/A");
    }

    #[test]
    fn net_both_sums_before_formatting_and_propagates_state() {
        let at = at(10);
        let combined = combined_network_metric(
            Metric::valid(62_500.0, at),
            Metric::valid(62_500.0, at + Duration::from_secs(1)),
        );
        assert_eq!(combined.value, 125_000.0);
        assert_eq!(metric_network_rate(combined), "1.0Mbps");
        assert_eq!(combined.updated, Some(at + Duration::from_secs(1)));

        let mut stale = Metric::valid(1.0, at);
        stale.stale = true;
        assert!(combined_network_metric(stale, Metric::valid(1.0, at)).stale);
        assert!(!combined_network_metric(Metric::invalid(), stale).valid);
        assert!(
            !combined_network_metric(Metric::valid(f64::MAX, at), Metric::valid(f64::MAX, at))
                .valid
        );
    }

    #[test]
    fn network_slots_render_stale_and_invalid_states_without_history() {
        let at = at(20);
        let mut stale = Metric::valid(1.0, at);
        stale.stale = true;
        let invalids = [Metric::invalid(), Metric::valid(f64::NAN, at)];

        let mut renderer = Renderer::new();
        renderer.capture(&Snapshot {
            network_in: Metric::valid(125_000_000.0, at),
            network_out: Metric::valid(125_000_000.0, at),
            ..Default::default()
        });
        for metric in invalids {
            assert_eq!(metric_network_rate(metric), "N/A");
            let mut numeric = Frame::new();
            renderer.network_numeric_slot(&mut numeric, 1, 38, "NET IN", metric);
            let mut expected = Frame::new();
            renderer.label(&mut expected, 1, 38, "NET IN");
            expected.text_centered(1, 38, 35, "N/A", 1, true);
            assert!(numeric.equal(&expected));

            let mut single = Frame::new();
            renderer.network_graph_slot(&mut single, 1, 38, "I", metric, "netin", 1000.0);
            let mut expected = Frame::new();
            renderer.label(&mut expected, 1, 38, "I");
            expected.text_centered(1, 38, 35, "N/A", 1, true);
            assert!(single.equal(&expected));

            let mut dual = Frame::new();
            renderer.network_dual_graph_slot(
                &mut dual,
                1,
                38,
                metric,
                Metric::valid(1.0, at),
                1000.0,
                1000.0,
            );
            let mut expected = Frame::new();
            renderer.label(&mut expected, 1, 38, "I/O");
            expected.text_centered(1, 38, 35, "N/A", 1, true);
            assert!(dual.equal(&expected));
        }

        assert_eq!(metric_network_rate(stale), "STALE");
        let mut numeric = Frame::new();
        renderer.network_numeric_slot(&mut numeric, 1, 38, "NET IN", stale);
        let mut expected = Frame::new();
        renderer.label(&mut expected, 1, 38, "NET IN");
        expected.text_centered(1, 38, 35, "STALE", 1, true);
        assert!(numeric.equal(&expected));

        let mut single = Frame::new();
        renderer.network_graph_slot(&mut single, 1, 38, "I", stale, "netin", 1000.0);
        let mut expected = Frame::new();
        renderer.label(&mut expected, 1, 38, "I");
        expected.text_centered(1, 38, 35, "STALE", 1, true);
        assert!(single.equal(&expected));

        let mut dual = Frame::new();
        renderer.network_dual_graph_slot(
            &mut dual,
            1,
            38,
            stale,
            Metric::valid(1.0, at),
            1000.0,
            1000.0,
        );
        let mut expected = Frame::new();
        renderer.label(&mut expected, 1, 38, "I/O");
        expected.text_centered(1, 38, 35, "STALE", 1, true);
        assert!(dual.equal(&expected));
    }

    #[test]
    fn network_history_keeps_30_unique_sampler_timestamps() {
        let mut renderer = Renderer::new();
        for second in 0..85 {
            let sampled_at = at(second);
            let snapshot = Snapshot {
                network_in: Metric::valid(second as f64, sampled_at),
                network_out: Metric::valid((second * 2) as f64, sampled_at),
                game: GameStats {
                    frame_time_ms: Metric::valid(second as f64, sampled_at),
                    ..Default::default()
                },
                ..Default::default()
            };
            renderer.capture(&snapshot);
            renderer.capture(&snapshot);
        }
        let values = renderer.hist["netin"].snapshot();
        assert_eq!(values.len(), 85);
        assert_eq!(values.first().unwrap().value, 0.0);
        assert_eq!(values.last().unwrap().value, 84.0);
        let frame_values = renderer.hist["frame"].snapshot();
        assert_eq!(frame_values.len(), 85);
        assert_eq!(frame_values.first().unwrap().value, 0.0);
    }

    #[test]
    fn network_scale_clips_and_dual_graph_uses_opposite_baseline_sides() {
        assert_eq!(network_graph_height(0.0, 1000.0, 10), 0);
        assert_eq!(network_graph_height(1.0, 1000.0, 10), 1);
        assert_eq!(network_graph_height(125_000_000.0, 1000.0, 10), 10);
        assert_eq!(network_graph_height(250_000_000.0, 1000.0, 10), 10);

        let graph = |in_rate: f64, out_rate: f64| {
            let mut renderer = Renderer::new();
            for second in 1..=2 {
                renderer.capture(&Snapshot {
                    network_in: Metric::valid(in_rate, at(second)),
                    network_out: Metric::valid(out_rate, at(second)),
                    ..Default::default()
                });
            }
            let mut frame = Frame::new();
            renderer.network_dual_graph_slot(
                &mut frame,
                1,
                38,
                Metric::valid(in_rate, at(2)),
                Metric::valid(out_rate, at(2)),
                1000.0,
                1000.0,
            );
            frame
        };
        let ingress = graph(125_000_000.0, 0.0);
        let egress = graph(0.0, 125_000_000.0);
        assert!((1..39).any(|x| ingress.get(x, 33)));
        assert!(!(1..39).any(|x| ingress.get(x, 42)));
        assert!((1..39).any(|x| egress.get(x, 42)));
        assert!(!(1..39).any(|x| egress.get(x, 33)));
    }

    #[test]
    fn asymmetric_network_ceilings_map_to_every_directional_graph() {
        let sampled_at = at(30);
        let snapshot = Snapshot {
            now: Some(sampled_at),
            network_in: Metric::valid(625_000.0, sampled_at),
            network_out: Metric::valid(625_000.0, sampled_at),
            ..Default::default()
        };
        let options = |main_display| OverlayOptions {
            main_display,
            network_graph_ceiling_download_mbps: 10.0,
            network_graph_ceiling_upload_mbps: 100.0,
            ..Default::default()
        };

        let clear_view = View {
            slot_modules: std::array::from_fn(|_| "CLEAR".into()),
            ..Default::default()
        };
        let layout_2 = Renderer::new().render(&snapshot, options(2), &clear_view);
        assert!(layout_2.get(130, 20), "layout 2 IN must use download");
        assert!(!layout_2.get(130, 11), "layout 2 OUT must use upload");
        let layout_3 = Renderer::new().render(&snapshot, options(3), &clear_view);
        assert!(layout_3.get(120, 11), "layout 3 NET IN must use download");
        assert!(!layout_3.get(120, 20), "layout 3 NET OUT must use upload");

        let graph = |module: &str| {
            Renderer::new().render(
                &snapshot,
                options(1),
                &View {
                    slot_modules: [
                        module.into(),
                        "CLEAR".into(),
                        "CLEAR".into(),
                        "CLEAR".into(),
                    ],
                    ..Default::default()
                },
            )
        };
        let inbound = graph("NET_IN_GRAPH");
        let outbound = graph("NET_OUT_GRAPH");
        assert!(inbound.get(34, 38), "NET_IN_GRAPH must use download");
        assert!(!outbound.get(34, 38), "NET_OUT_GRAPH must use upload");

        let both = graph("NET_GRAPH");
        assert!(both.get(34, 35), "NET_GRAPH top/inbound must use download");
        assert!(both.get(34, 38), "NET_GRAPH bottom/outbound must render");
        assert!(
            !both.get(34, 39),
            "NET_GRAPH bottom/outbound must use upload"
        );
    }

    #[test]
    fn ccd_temperature_modules_resolve_roles_and_fail_closed() {
        let at = at(1_800_100_000);
        let readings = ReadingsSnapshot {
            hardware: vec![
                crate::model::HardwareDescriptor {
                    hardware_id: "domain-frequency".into(),
                    kind: HardwareKind::CPUDomain,
                    role: HardwareRole::Frequency,
                },
                crate::model::HardwareDescriptor {
                    hardware_id: "domain-cache".into(),
                    kind: HardwareKind::CPUDomain,
                    role: HardwareRole::Cache,
                },
            ],
            metrics: vec![
                Reading::current_celsius(MetricKey::CPUCCDTemp, 71.0, "domain-cache", at),
                Reading::current_celsius(MetricKey::CPUCCDTemp, 63.0, "domain-frequency", at),
            ],
        };
        let cache = canonical_cpu_domain_temperature(&readings, HardwareRole::Cache);
        let frequency = canonical_cpu_domain_temperature(&readings, HardwareRole::Frequency);
        assert!(cache.valid && !cache.stale && cache.value == 71.0);
        assert!(frequency.valid && !frequency.stale && frequency.value == 63.0);

        // Ambiguity, wrong kind, invalid values all fail closed.
        let mut ambiguous = readings.clone();
        ambiguous.hardware.push(crate::model::HardwareDescriptor {
            hardware_id: "domain-cache-2".into(),
            kind: HardwareKind::CPUDomain,
            role: HardwareRole::Cache,
        });
        assert!(!canonical_cpu_domain_temperature(&ambiguous, HardwareRole::Cache).valid);

        let mut wrong_kind = readings.clone();
        wrong_kind.metrics[0].kind = ValueKind::Percent;
        assert!(!canonical_cpu_domain_temperature(&wrong_kind, HardwareRole::Cache).valid);

        let mut nan = readings.clone();
        nan.metrics[0].number = f64::NAN;
        assert!(!canonical_cpu_domain_temperature(&nan, HardwareRole::Cache).valid);
    }

    #[test]
    fn headset_states_render_distinctly() {
        let mut r = Renderer::new();
        let mut s = sample_snapshot();
        let mut online = Frame::new();
        r.slot(
            &mut online,
            0,
            "HEADSET_BATTERY",
            &s,
            &OverlayOptions::default(),
            &View::default(),
        );
        s.headset.stale = true;
        let mut stale = Frame::new();
        r.slot(
            &mut stale,
            0,
            "HEADSET_BATTERY",
            &s,
            &OverlayOptions::default(),
            &View::default(),
        );
        s.headset = crate::model::HeadsetBattery {
            raw_level: -1,
            ..Default::default()
        };
        let mut missing = Frame::new();
        r.slot(
            &mut missing,
            0,
            "HEADSET_BATTERY",
            &s,
            &OverlayOptions::default(),
            &View::default(),
        );
        s.providers.insert("safe-mode".into(), true);
        let mut disabled = Frame::new();
        r.slot(
            &mut disabled,
            0,
            "HEADSET_BATTERY",
            &s,
            &OverlayOptions::default(),
            &View::default(),
        );
        assert!(!online.equal(&stale));
        assert!(!stale.equal(&missing));
        assert!(!online.equal(&missing));
        assert!(!missing.equal(&disabled));
    }

    #[test]
    fn single_ccd_mode_renders_one_full_bar() {
        let mut s = sample_snapshot();
        s.cpu_dual = false;
        s.cpu_cache_load = Metric::valid(63.0, s.now.unwrap());
        let dual = render_dashboard(&sample_snapshot());
        let single = render_dashboard(&s);
        assert!(!single.equal(&dual));
        // Single mode draws the "CPU" label at y=9 (dual draws it at y=10):
        // the C glyph's row-1 pixel lands at (1,10) only in single mode.
        assert!(single.get(1, 10), "CPU label at single-mode position");
        assert!(!dual.get(1, 10), "dual mode has different label geometry");
        // Single mode remains continuous where dual mode has its center gap.
        assert!(single.get(15, 11), "full-height CPU bar left border");
        assert!(
            !dual.get(15, 11),
            "dual compact bars retain their center gap"
        );
        // Deterministic.
        assert_eq!(single.hash_hex(), render_dashboard(&s).hash_hex());
    }

    #[test]
    fn critical_alert_overrides_discord_and_dashboard() {
        let mut s = sample_snapshot();
        s.discord.connected = true;
        s.discord.authenticated = true;
        s.discord.speakers.push(Speaker {
            user_id: "1".into(),
            name: "Nezumi".into(),
            speaking: true,
            started_at: s.now,
            ..Default::default()
        });
        let base = render_dashboard(&s);
        s.alerts.push(Alert {
            id: "cpu".into(),
            title: "CPU HOT".into(),
            detail: "95C".into(),
            severity: 2,
            acknowledged: false,
        });
        let alert_frame = render_dashboard(&s);
        assert!(!alert_frame.equal(&base));
        assert!(alert_frame.get(0, 0) && alert_frame.get(159, 42));
    }

    #[test]
    fn hung_hold_overrides_alert_and_discord_then_restores_exact_dashboard() {
        let mut normal = sample_snapshot();
        let view = golden_view();
        let mut renderer = Renderer::new();
        let dashboard = renderer.render(&normal, OverlayOptions::default(), &view);

        normal.hung.push(HungTarget {
            hwnd: 1,
            pid: 2,
            creation_time: 3,
            image_path: r"c:\games\game.exe".into(),
            process_name: "game.exe".into(),
            title: "Game".into(),
        });
        normal.alerts.push(Alert {
            severity: 2,
            title: "CRITICAL".into(),
            ..Default::default()
        });
        normal.discord.connected = true;
        normal.discord.authenticated = true;
        normal.discord.speakers.push(Speaker {
            user_id: "1".into(),
            name: "speaker".into(),
            speaking: true,
            ..Default::default()
        });
        let mut hold_view = view.clone();
        hold_view.slot_modules[2] = "PROC_HANG".into();
        hold_view.hung_hold = 0.5;
        let with_priority = renderer.render(&normal, OverlayOptions::default(), &hold_view);

        let mut only_hung = normal.clone();
        only_hung.alerts.clear();
        only_hung.discord = Default::default();
        let expected = Renderer::new().render(&only_hung, OverlayOptions::default(), &hold_view);
        assert!(with_priority.equal(&expected));
        assert!((81..100).all(|x| with_priority.get(x, 26)));

        normal.hung.clear();
        normal.alerts.clear();
        normal.discord = Default::default();
        let restored = renderer.render(&normal, OverlayOptions::default(), &view);
        assert!(restored.equal(&dashboard));
    }

    #[test]
    fn hung_hold_draws_only_whole_positive_progress_widths() {
        let hung = [HungTarget {
            process_name: "game.exe".into(),
            ..Default::default()
        }];
        let row = |hold| {
            let mut frame = Frame::new();
            hang_slot(
                &mut frame,
                2,
                &hung,
                &View {
                    hung_hold: hold,
                    ..Default::default()
                },
            );
            (80..120).map(|x| frame.get(x, 26)).collect::<Vec<_>>()
        };
        assert!(row(0.0).iter().all(|pixel| !pixel));
        assert!(row(0.5 / 38.0).iter().all(|pixel| !pixel));
        let first = row(1.5 / 38.0);
        assert!(first[1]);
        assert!(first[2..].iter().all(|pixel| !pixel));
        let full = row(1.0);
        assert!(full[1..39].iter().all(|pixel| *pixel));
        assert!(!full[0] && !full[39]);
    }

    #[test]
    fn canonical_registry_dispatches_all_modules_with_bounded_frames() {
        assert_eq!(crate::config::MODULES.len(), 53);
        let normal = audit_snapshot();
        let edge = edge_snapshot();
        let status = status_snapshot();
        for module in crate::config::MODULES {
            for (variant, snapshot) in [("normal", &normal), ("edge", &edge), ("status", &status)] {
                let mut renderer = Renderer::new();
                renderer.capture(snapshot);
                let mut frame = Frame::new();
                renderer.slot(
                    &mut frame,
                    0,
                    module,
                    snapshot,
                    &OverlayOptions::default(),
                    &View::default(),
                );
                assert!(
                    frame.pixels.iter().any(|&pixel| pixel != 0),
                    "{module} {variant} blank"
                );
                assert!(
                    frame.pixels.iter().enumerate().all(|(index, pixel)| {
                        *pixel == 0 || (index % WIDTH < 40 && index / WIDTH >= 26)
                    }),
                    "{module} {variant} escaped slot bounds"
                );
            }
        }
    }

    #[test]
    fn audited_label_value_rows_are_spaced_complete_and_fitted() {
        let assert_row = |frame: &Frame, y: i32, text: &str| {
            let mut expected = Frame::new();
            expected.text_centered(1, 38, y, text, 1, true);
            for yy in y..y + 5 {
                for x in 1..39 {
                    assert_eq!(
                        frame.get(x, yy),
                        expected.get(x, yy),
                        "{text} pixel ({x},{yy})"
                    );
                }
            }
        };
        let assert_space = |frame: &Frame, y: i32, text: &str, index: i32| {
            let left = 1 + (38 - text_width(text, 1)) / 2;
            assert!(
                (left + index * 4..left + index * 4 + 3)
                    .all(|x| (y..y + 5).all(|yy| !frame.get(x, yy))),
                "space after semantic label is not blank: {text}"
            );
        };
        let now = at(30);

        let mut renderer = Renderer::new();
        let mut frame = Frame::new();
        renderer.graph_slot(
            &mut frame,
            1,
            38,
            "FT",
            Metric::valid(17.0, now),
            "frame",
            "MS",
        );
        assert_row(&frame, 27, "FT 17MS");
        assert_space(&frame, 27, "FT 17MS", 2);

        for (label, value, suffix, expected) in [
            ("CPU", 54.0, "%", "CPU 54%"),
            ("GPU", 91.0, "%", "GPU 91%"),
            ("CPU", 68.0, "C", "CPU 68C"),
            ("GPU", 74.0, "C", "GPU 74C"),
            ("FPS", 144.0, "", "FPS 144"),
        ] {
            let mut frame = Frame::new();
            Renderer::new().fixed_graph_slot(
                &mut frame,
                1,
                38,
                label,
                Metric::valid(value, now),
                "unused",
                100.0,
                suffix,
            );
            assert_row(&frame, 27, expected);
            assert_space(&frame, 27, expected, 3);
        }

        let alerts = vec![
            Alert {
                severity: 3,
                ..Default::default()
            },
            Alert {
                severity: 2,
                ..Default::default()
            },
        ];
        let mut frame = Frame::new();
        alert_slot(&mut frame, 1, 38, &alerts);
        assert_row(&frame, 35, "2 L 3");
        assert_space(&frame, 35, "2 L 3", 3);

        let mut frame = Frame::new();
        thermals_slot(
            &mut frame,
            1,
            38,
            Metric::valid(68.0, now),
            Metric::valid(74.0, now),
        );
        assert_row(&frame, 28, "CPU 68C");
        assert_row(&frame, 36, "GPU 74C");
        assert_space(&frame, 28, "CPU 68C", 3);
        assert_space(&frame, 36, "GPU 74C", 3);
        assert!((33..36).all(|y| (1..39).all(|x| !frame.get(x, y))));

        let mut frame = Frame::new();
        net_health_slot(
            &mut frame,
            1,
            38,
            Metric::valid(24.0, now),
            Metric::valid(4.0, now),
            Metric::valid(1.0, now),
        );
        assert_row(&frame, 26, "P 24MS");
        assert_row(&frame, 32, "J 4MS");
        assert_row(&frame, 38, "L 1%");
        assert_space(&frame, 26, "P 24MS", 1);
        assert_space(&frame, 32, "J 4MS", 1);
        assert_space(&frame, 38, "L 1%", 1);
        assert!((1..39).all(|x| !frame.get(x, 31) && !frame.get(x, 37)));

        for (prefix, value, expected) in
            [("I", 1_250_000.0, "I 10Mbps"), ("O", 842_000.0, "O 7Mbps")]
        {
            let mut frame = Frame::new();
            Renderer::new().network_graph_slot(
                &mut frame,
                1,
                38,
                prefix,
                Metric::valid(value, now),
                "unused",
                1000.0,
            );
            assert_row(&frame, 27, expected);
            assert_space(&frame, 27, expected, 1);
        }

        let mut frame = Frame::new();
        headset_slot(
            &mut frame,
            1,
            38,
            &HeadsetBattery {
                present: true,
                online: true,
                charging: true,
                percent: 100,
                raw_level: 4,
                ..Default::default()
            },
            false,
        );
        assert_row(&frame, 27, "CHARGING");
        let mut percent = Frame::new();
        percent.text_right(38, 35, "100%", true);
        assert!((24..39).all(|x| (35..40).all(|y| frame.get(x, y) == percent.get(x, y))));

        let readings = ReadingsSnapshot {
            metrics: vec![
                Reading::current_bytes(MetricKey::RAMUsed, 31u64 << 30, "memory", now),
                Reading::current_bytes(MetricKey::RAMTotal, 32u64 << 30, "memory", now),
                Reading::current_number(
                    MetricKey::PageReadsPerSec,
                    ValueKind::Count,
                    17.0,
                    "memory",
                    now,
                ),
            ],
            ..Default::default()
        };
        let snapshot = Snapshot {
            readings: readings.clone(),
            ..Default::default()
        };
        let mut frame = Frame::new();
        Renderer::new().ram_detail_slot(&mut frame, 1, 38, &snapshot);
        assert_row(&frame, 35, "31/32GiB");
        let mut frame = Frame::new();
        hard_faults_slot(&mut frame, 1, 38, &readings);
        assert_row(&frame, 27, "FAULTS");
        assert_row(&frame, 35, "17/s");

        let summary = format_session_summary(Duration::from_secs(u32::MAX as u64), i32::MAX, 38);
        assert_eq!(summary, ">9H >9S");
        assert!(text_width(&summary, 1) <= 38 && summary.ends_with('S'));
        let mut frame = Frame::new();
        let snapshot = Snapshot {
            now: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(u32::MAX as u64)),
            game: GameStats {
                active: true,
                session_start: Some(SystemTime::UNIX_EPOCH),
                stutters: i32::MAX,
                ..Default::default()
            },
            ..Default::default()
        };
        Renderer::new().slot(
            &mut frame,
            0,
            "SESSION_SUMMARY",
            &snapshot,
            &OverlayOptions::default(),
            &View::default(),
        );
        assert_row(&frame, 35, ">9H >9S");

        for text in [
            format_labeled_number("FT", f64::MAX, "MS", 38),
            format_labeled_number("FPS", f64::MAX, "", 38),
            format_labeled_number("CPU", 100.0, "%", 38),
            format_network_graph_rate("I", f64::MAX, 38),
            format_network_graph_rate("O", 0.0, 38),
            format_disk_graph_rates(1_250_000.0, 842_000.0, 38),
            format_alert_summary(i32::MAX, 3, 38),
        ] {
            assert!(text_width(&text, 1) <= 38, "formatter overflow: {text}");
        }
        assert_eq!(format_labeled_number("FT", f64::MAX, "MS", 38), "FT >999MS");
        assert_eq!(format_labeled_number("FPS", f64::MAX, "", 38), "FPS >9999");
        assert_eq!(format_network_graph_rate("I", f64::MAX, 38), "I >9Gbps");
        assert_eq!(
            format_disk_graph_rates(1_250_000.0, 842_000.0, 38),
            "R 1M/W 1M"
        );
        assert_eq!(format_alert_summary(i32::MAX, 3, 38), ">9 L 3");
    }

    #[test]
    fn bounded_numeric_formatter_preserves_precision_bounds_and_suffixes() {
        let cases = [
            ("", 17.0, 0, "MS", "17MS"),
            ("", 1.25, 1, "%", "1.2%"),
            ("", 999_999_999.0, 0, "", "999999999"),
            ("", f64::MAX, 0, "", ">99999999"),
            ("", f64::MAX, 0, "W", ">9999999W"),
            ("", f64::MAX, 0, "MS", ">999999MS"),
            ("", f64::MAX, 0, "%", ">9999999%"),
            ("", f64::MAX, 0, "C", ">9999999C"),
            ("", f64::MAX, 0, "\u{00B0}", ">9999999\u{00B0}"),
            ("", -f64::MAX, 0, "W", "<-999999W"),
            ("CPU ", f64::MAX, 0, "%", "CPU >999%"),
            ("P ", f64::MAX, 0, "MS", "P >9999MS"),
        ];
        for (prefix, value, decimals, suffix, expected) in cases {
            let text = format_bounded_number(prefix, value, decimals, suffix, 38);
            assert_eq!(text, expected);
            assert!(text_width(&text, 1) <= 38);
            assert!(text.ends_with(suffix));
            assert!(!text.contains("..."));
        }
        assert_eq!(format_bounded_number("", f64::NAN, 0, "W", 38), "N/A");
        assert_eq!(format_bounded_number("", f64::INFINITY, 0, "MS", 38), "N/A");

        let now = at(40);
        for (suffix, expected) in [
            ("", ">99999999"),
            ("W", ">9999999W"),
            ("MS", ">999999MS"),
            ("%", ">9999999%"),
            ("C", ">9999999C"),
            ("\u{00B0}", ">9999999\u{00B0}"),
        ] {
            let mut frame = Frame::new();
            Renderer::new().numeric_slot(
                &mut frame,
                1,
                38,
                "VALUE",
                Metric::valid(f64::MAX, now),
                0,
                suffix,
            );
            let mut expected_frame = Frame::new();
            expected_frame.text_centered(1, 38, 35, expected, 1, true);
            assert!(
                (1..39).all(|x| (35..40).all(|y| { frame.get(x, y) == expected_frame.get(x, y) }))
            );
        }
        let mut nonfinite = Frame::new();
        Renderer::new().numeric_slot(
            &mut nonfinite,
            1,
            38,
            "VALUE",
            Metric::valid(f64::INFINITY, now),
            0,
            "W",
        );
        let mut expected = Frame::new();
        expected.text_centered(1, 38, 27, "VALUE", 1, true);
        expected.text_centered(1, 38, 35, "N/A", 1, true);
        assert!(nonfinite.equal(&expected));
    }

    #[test]
    fn warning_is_inclusive_current_and_inverts_only_its_slot_pane() {
        let mut snapshot = sample_snapshot();
        snapshot.cpu_temp.value = 90.0;
        let view = View {
            slot_modules: [
                "CPU_TEMP".into(),
                "CLOCK".into(),
                "CLOCK".into(),
                "CLOCK".into(),
            ],
            ..Default::default()
        };
        let base = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: true,
                cpu_temp_max_c: 90.0,
                ..Default::default()
            },
            &view,
        );
        let flashed = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: true,
                warning_phase: true,
                cpu_temp_max_c: 90.0,
                ..Default::default()
            },
            &view,
        );
        for y in 0..43 {
            for x in 0..160 {
                assert_eq!(
                    flashed.get(x, y),
                    if x < 40 && y >= 26 {
                        !base.get(x, y)
                    } else {
                        base.get(x, y)
                    }
                );
            }
        }
        snapshot.cpu_temp.stale = true;
        let stale = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: true,
                warning_phase: true,
                cpu_temp_max_c: 90.0,
                ..Default::default()
            },
            &view,
        );
        let stale_base = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: true,
                cpu_temp_max_c: 90.0,
                ..Default::default()
            },
            &view,
        );
        assert!(stale.equal(&stale_base));

        snapshot.cpu_temp.stale = false;
        let category_disabled = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: false,
                warning_phase: true,
                cpu_temp_max_c: 90.0,
                ..Default::default()
            },
            &view,
        );
        let disabled_base = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: false,
                cpu_temp_max_c: 90.0,
                ..Default::default()
            },
            &view,
        );
        assert!(category_disabled.equal(&disabled_base));
    }

    #[test]
    fn warning_suppresses_only_temperature_full_screen_alerts() {
        let mut snapshot = sample_snapshot();
        let dashboard = render_dashboard(&snapshot);
        snapshot.alerts.push(Alert {
            id: "cpu-temp".into(),
            severity: 3,
            title: "CPU HOT".into(),
            ..Default::default()
        });
        let suppressed = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: true,
                ..Default::default()
            },
            &golden_view(),
        );
        assert!(suppressed.equal(&dashboard));
        snapshot.alerts.push(Alert {
            id: "memory".into(),
            severity: 2,
            title: "MEMORY HIGH".into(),
            ..Default::default()
        });
        let other = Renderer::new().render(
            &snapshot,
            OverlayOptions {
                warning: true,
                ..Default::default()
            },
            &golden_view(),
        );
        assert!(!other.equal(&dashboard));
        assert!(other.get(0, 0) && other.get(159, 42));
    }

    #[test]
    fn expansion_state_formatters_are_truthful_and_compact() {
        assert_eq!(format_byte_rate(1_250_000.0), "1.2MB/s");
        assert_eq!(format_byte_rate(0.0), "0B/s");
        assert_eq!(
            system_battery_text(crate::model::SystemBattery {
                ac: AcState::Online,
                battery_present: Some(true),
                percent: Some(73),
                freshness: Freshness::Current,
                ..Default::default()
            }),
            "AC 73%"
        );
        assert_eq!(
            system_battery_text(crate::model::SystemBattery {
                ac: AcState::Online,
                battery_present: Some(false),
                freshness: Freshness::Current,
                ..Default::default()
            }),
            "AC ONLY"
        );
        assert_eq!(
            system_battery_text(crate::model::SystemBattery {
                freshness: Freshness::Stale,
                ..Default::default()
            }),
            "STALE"
        );
        assert_eq!(graph_height(200.0, 100.0, 7), 7);
        assert_eq!(graph_height(-1.0, 100.0, 7), 0);
    }

    #[test]
    fn bottleneck_text_maps_every_state_and_stale_precedence() {
        let mut snapshot = Snapshot::default();
        snapshot.bottleneck.freshness = Freshness::Current;
        for (state, expected) in [
            (crate::model::BottleneckState::Unavailable, "N/A"),
            (crate::model::BottleneckState::None, "NONE"),
            (crate::model::BottleneckState::Cpu, "CPU"),
            (crate::model::BottleneckState::Gpu, "GPU"),
            (crate::model::BottleneckState::Mem, "RAM"),
            (crate::model::BottleneckState::DiskIo, "DISK I/O"),
        ] {
            snapshot.bottleneck.state = state;
            assert_eq!(bottleneck_text(&snapshot), expected);
        }
        snapshot.bottleneck.freshness = Freshness::Stale;
        assert_eq!(bottleneck_text(&snapshot), "STALE");
    }

    #[test]
    fn disk_io_rows_and_graph_labels_remain_distinct_and_in_bounds() {
        let snapshot = |read, write, sampled_at| Snapshot {
            now: Some(sampled_at),
            readings: ReadingsSnapshot {
                metrics: vec![
                    Reading::current_number(
                        MetricKey::DiskReadBytesPerSec,
                        ValueKind::ByteRate,
                        read,
                        "disk-total",
                        sampled_at,
                    ),
                    Reading::current_number(
                        MetricKey::DiskWriteBytesPerSec,
                        ValueKind::ByteRate,
                        write,
                        "disk-total",
                        sampled_at,
                    ),
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let cases = [
            (0.0, 999.0),
            (999.0, 1_000.0),
            (999_999.0, 1_000_000.0),
            (999_999_999.0, 1_000_000_000.0),
            (f64::MAX, 10_000_000_000_000.0),
        ];
        for (read, write) in cases {
            let snapshot = snapshot(read, write, at(30));
            let read_text = format_disk_rate('R', read, 38);
            let write_text = format_disk_rate('W', write, 38);
            assert_ne!(read_text, write_text);
            assert!(read_text.starts_with("R "));
            assert!(write_text.starts_with("W "));
            assert!(text_width(&read_text, 1) <= 38);
            assert!(text_width(&write_text, 1) <= 38);

            let mut frame = Frame::new();
            Renderer::new().disk_io_slot(&mut frame, 1, 38, &snapshot);
            assert!((28..33).any(|y| (1..39).any(|x| frame.get(x, y))));
            assert!((36..41).any(|y| (1..39).any(|x| frame.get(x, y))));
            assert!((33..36).all(|y| (1..39).all(|x| !frame.get(x, y))));
            for (text, y) in [(&read_text, 28), (&write_text, 36)] {
                let left = 1 + (38 - text_width(text, 1)) / 2;
                assert!((left + 3..left + 8).all(|x| (y..y + 5).all(|y| !frame.get(x, y))));
            }
            assert!(frame.pixels.iter().enumerate().all(|(index, pixel)| {
                if *pixel == 0 {
                    return true;
                }
                let (x, y) = (index % WIDTH, index / WIDTH);
                (1..39).contains(&x) && (28..41).contains(&y)
            }));
        }

        let fixture = snapshot(1_250_000.0, 842_000.0, at(30));
        let mut old = Frame::new();
        old.text_centered(1, 38, 27, "DISK I/O", 1, true);
        old.text_centered(1, 38, 34, "R1.2MB/s", 1, true);
        old.text_centered(1, 38, 39, "W842KB/s", 1, true);
        let mut current = Frame::new();
        Renderer::new().disk_io_slot(&mut current, 1, 38, &fixture);
        assert!((33..36).any(|y| (1..39).any(|x| old.get(x, y))));
        assert!((33..36).all(|y| (1..39).all(|x| !current.get(x, y))));
        let old_write_ink = (39..43)
            .map(|y| (1..39).filter(|&x| old.get(x, y)).count())
            .sum::<usize>();
        let mut complete_write = Frame::new();
        complete_write.text_centered(1, 38, 36, "W842KB/s", 1, true);
        assert!(
            old_write_ink
                < complete_write
                    .pixels
                    .iter()
                    .filter(|&&pixel| pixel != 0)
                    .count()
        );

        assert_eq!(format_short_rate(0.0), "0B");
        assert_eq!(format_short_rate(999.4), "999B");
        assert_eq!(format_short_rate(999.5), "1K");
        assert_eq!(format_short_rate(999_499.0), "999K");
        assert_eq!(format_short_rate(999_500.0), "1M");
        assert_eq!(format_short_rate(999_499_999.0), "999M");
        assert_eq!(format_short_rate(999_500_000.0), "1G");
        assert_eq!(format_short_rate(999_499_999_999.0), "999G");
        assert_eq!(format_short_rate(999_500_000_000.0), ">999G");
        for (read, write, expected) in [
            (0.0, 0.0, "R 0B/W 0B"),
            (12.0, 999.0, "R <K/W 1K"),
            (1_000.0, 1_000_000.0, "R 1K/W 1M"),
            (1_250_000.0, 842_000.0, "R 1M/W 1M"),
            (1_000_000.0, 1_000_000_000.0, "R 1M/W 1G"),
            (1e12, 1e12, "R >G/W >G"),
        ] {
            let label = format_disk_graph_rates(read, write, 38);
            assert_eq!(label, expected);
            assert!(text_width(&label, 1) <= 38);
            let (read_label, write_label) = label.split_once("/W").unwrap();
            assert!(read_label.starts_with("R ") && read_label.len() > 2);
            assert!(write_label.starts_with(' ') && write_label.len() > 1);
            assert!(!label.contains('.'));
        }

        let mut renderer = Renderer::new();
        for second in 1..=30 {
            renderer.capture(&snapshot(1_000_000_000.0, 1_000_000_000.0, at(second)));
        }
        let latest = snapshot(1_000_000_000.0, 1_000_000_000.0, at(30));
        let mut graph = Frame::new();
        renderer.disk_graph_slot(&mut graph, 1, 38, &latest, 1000.0);
        let mut expected_label = Frame::new();
        let graph_label = "R 1G/W 1G";
        expected_label.text_centered(1, 38, 27, graph_label, 1, true);
        for y in 27..32 {
            for x in 1..39 {
                assert_eq!(graph.get(x, y), expected_label.get(x, y));
            }
        }
        assert!((1..39).all(|x| !graph.get(x, 32)));
        assert!((33..37).any(|y| (1..39).any(|x| graph.get(x, y))));
        assert!((38..43).any(|y| (1..39).any(|x| graph.get(x, y))));
        assert!((5..35).any(|x| graph.get(x, 33)));
        assert!((5..35).all(|x| graph.get(x, 37)));
        assert!((5..35).any(|x| graph.get(x, 42)));
        assert!(graph.pixels.iter().enumerate().all(|(index, pixel)| {
            *pixel == 0
                || ((1..39).contains(&(index % WIDTH)) && (27..43).contains(&(index / WIDTH)))
        }));

        let unavailable = Snapshot::default();
        let mut stale = snapshot(1_000.0, 2_000.0, at(30));
        stale.readings.metrics[0].freshness = Freshness::Stale;
        for (snapshot, status) in [(&unavailable, "N/A"), (&stale, "STALE")] {
            let mut frame = Frame::new();
            Renderer::new().disk_io_slot(&mut frame, 1, 38, snapshot);
            let mut expected = Frame::new();
            expected.text_centered(1, 38, 27, "DISK I/O", 1, true);
            expected.text_centered(1, 38, 35, status, 1, true);
            assert!(frame.equal(&expected));
            assert!(frame.pixels.iter().enumerate().all(|(index, pixel)| {
                *pixel == 0
                    || ((1..39).contains(&(index % WIDTH)) && (27..40).contains(&(index / WIDTH)))
            }));
        }

        assert_eq!(format_disk_rate('R', 1_250_000.0, 38), "R 1.2MB/s");
        assert_eq!(format_disk_rate('W', 842_000.0, 38), "W 842KB/s");
        assert_eq!(format_disk_rate('R', 999_999.0, 38), "R 1MB/s");
        assert_eq!(format_disk_rate('R', f64::MAX, 38), "R >99GB/s");
    }
}
