//! Fixed 160x43 dashboard renderer, ported pixel-for-pixel from the original
//! Go implementation. The golden framebuffer hashes below are release invariants;
//! any change to them is a fixed-layout change requiring explicit review.
#![allow(clippy::too_many_arguments)] // drawing signatures mirror the Go renderer 1:1

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use super::font::{fit_text, text_width};
use super::Frame;
use crate::history::Series;
use crate::model::{
    session_duration, Availability, Freshness, HardwareKind, HardwareRole, HeadsetBattery,
    HungTarget, Metric, MetricKey, ReadingsSnapshot, Snapshot, Speaker, ValueKind, WIDTH,
};

/// Overlay knobs the renderer reads from configuration.
#[derive(Clone, Copy, Debug)]
pub struct OverlayOptions {
    pub discord_linger: Duration,
    pub discord_max_speakers: usize,
    pub discord_show_self: bool,
    pub discord_show_channel: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        OverlayOptions {
            discord_linger: Duration::from_millis(700),
            discord_max_speakers: 2,
            discord_show_self: false,
            discord_show_channel: false,
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
        self.capture(s);
        let mut f = Frame::new();
        if !s.hung.is_empty() {
            self.dashboard(&mut f, s, v, true);
            return f;
        }
        if let Some(alert) = highest_critical(&s.alerts, &v.alert_acknowledged) {
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
        self.dashboard(&mut f, s, v, false);
        f
    }

    fn capture(&mut self, s: &Snapshot) {
        let items: [(&'static str, Metric); 6] = [
            ("frame", s.game.frame_time_ms),
            ("netin", s.network_in),
            ("netout", s.network_out),
            ("ping", s.ping_ms),
            ("cpu", s.cpu_temp),
            ("gpu", s.gpu_temp),
        ];
        for (key, metric) in items {
            let Some(updated) = metric.updated else {
                continue;
            };
            if self.last.get(key) == Some(&updated) {
                continue;
            }
            let series = self.hist.entry(key).or_insert_with(|| Series::new(80));
            series.add(updated, metric.value);
            self.last.insert(key, updated);
        }
    }

    fn dashboard(&mut self, f: &mut Frame, s: &Snapshot, v: &View, hang: bool) {
        let mut date = s.date_text.clone();
        if text_width(&date, 1) + text_width(&s.time_text, 1) + 3 > WIDTH as i32 {
            date = s.date_short_text.clone();
        }
        let date = fit_text(&date, WIDTH as i32 - text_width(&s.time_text, 1) - 3, 1);
        f.text(1, 1, &date, true);
        f.text_right(158, 1, &s.time_text, true);
        f.h_line(0, 159, 7, true);
        f.v_line(79, 8, 24, true);
        f.h_line(0, 159, 25, true);
        for x in [39, 79, 119] {
            f.v_line(x, 26, 42, true);
        }
        if s.cpu_dual {
            self.cpu_split(f, 1, 9, s.cpu_cache_load, s.cpu_freq_load);
        } else {
            // Single-CCD: one full-height bar, MEM-style.
            self.metric_bar(f, 1, 9, "CPU", &s.cpu_cache_load, 75, 7);
        }
        self.metric_bar(
            f,
            1,
            18,
            "MEM",
            &canonical_percent_metric(&s.readings, MetricKey::RAMUtilization, ""),
            75,
            7,
        );
        self.metric_bar(
            f,
            81,
            9,
            "GPU",
            &canonical_percent_metric(&s.readings, MetricKey::GPUUtilization, ""),
            77,
            7,
        );
        self.metric_bar(
            f,
            81,
            18,
            "VRAM",
            &canonical_percent_metric(&s.readings, MetricKey::VRAMUtilization, ""),
            77,
            7,
        );
        for i in 0..4usize {
            if hang && i == 2 {
                hang_slot(f, &s.hung, v);
                continue;
            }
            self.slot(f, i, &v.slot_modules[i], s);
        }
    }

    fn cpu_split(&mut self, f: &mut Frame, x: i32, y: i32, cache: Metric, freq: Metric) {
        f.text(x, y + 2, "CPU", true);
        self.small_bar(f, x + 15, y, "C", cache, 62);
        self.small_bar(f, x + 15, y + 4, "F", freq, 62);
    }

    fn small_bar(&mut self, f: &mut Frame, x: i32, y: i32, label: &str, m: Metric, w: i32) {
        f.text(x, y, label, true);
        let bx = x + 4;
        let bw = w - 4;
        f.rect(bx, y, bw, 3, true);
        let p = metric_percent(&m);
        if p < 0 {
            self.metric_placeholder(f, bx + 1, y + 1, bw - 2, 1, m.valid && m.stale);
            return;
        }
        let fill = (bw - 2) * p / 100;
        f.fill_rect(bx + 1, y + 1, fill, 1, true);
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

    fn slot(&mut self, f: &mut Frame, i: usize, module: &str, s: &Snapshot) {
        let (x, w) = slot_bounds(i);
        let left = x + 1;
        let width = w - 2;
        let module = module.to_uppercase();
        match module.as_str() {
            "HEADSET_BATTERY" => headset_slot(f, left, width, &s.headset),
            "CONTROLLER_BATTERY" => controller_slot(f, left, width, &s.controller),
            "FPS_CURRENT" => self.numeric_slot(f, left, width, "FPS", s.game.fps, 0, ""),
            "FPS_1LOW" => self.numeric_slot(f, left, width, "1% LOW", s.game.one_percent, 0, ""),
            "FPS_01LOW" => {
                self.numeric_slot(f, left, width, ".1% LOW", s.game.point_one_low, 0, "")
            }
            "FRAME_TIME" => {
                self.graph_slot(f, left, width, "FRAME", s.game.frame_time_ms, "frame", "MS")
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
            "NET_IN" => self.graph_slot(f, left, width, "NET IN", s.network_in, "netin", ""),
            "NET_OUT" => self.graph_slot(f, left, width, "NET OUT", s.network_out, "netout", ""),
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
            other => self.text_slot(f, left, width, other, "N/A"),
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
        if !m.valid || m.stale {
            let text = if m.valid && m.stale { "STALE" } else { "N/A" };
            f.text_centered(x, w, 35, text, 1, true);
            return;
        }
        let text = format!("{:.*}{}", decimals as usize, m.value, suffix);
        if text_width(&text, 1) <= w {
            f.text_centered(x, w, 35, &text, 1, true);
        } else {
            f.text_centered(x, w, 34, &fit_text(&text, w, 1), 1, true);
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
        self.label(f, x, w, label);
        if !m.valid || m.stale {
            let text = if m.valid && m.stale { "STALE" } else { "N/A" };
            f.text_centered(x, w, 35, text, 1, true);
            return;
        }
        let val = format!("{:.0}{}", m.value, suffix);
        f.text(x, 35, &fit_text(&val, w / 2, 1), true);
        let Some(samples) = self.hist.get(key) else {
            return;
        };
        let vals = samples.snapshot();
        if vals.len() < 2 {
            return;
        }
        let gx = x + w / 2;
        let gw = w - (gx - x);
        let mut min = vals[0].value;
        let mut max = vals[0].value;
        for v in &vals {
            if v.value < min {
                min = v.value;
            }
            if v.value > max {
                max = v.value;
            }
        }
        if max - min < 1.0 {
            max = min + 1.0;
        }
        let start = vals.len().saturating_sub(gw as usize);
        for (i, v) in vals[start..].iter().enumerate() {
            let mut yy = 42 - ((v.value - min) / (max - min) * 6.0) as i32;
            if yy < 35 {
                yy = 35;
            }
            f.v_line(gx + i as i32, yy, 42, true);
        }
    }

    fn session_slot(&mut self, f: &mut Frame, x: i32, w: i32, s: &Snapshot) {
        self.label(f, x, w, "SESSION");
        if !s.game.active {
            f.text_centered(x, w, 35, "IDLE", 1, true);
            return;
        }
        let text = format!(
            "{} {}S",
            crate::model::format_duration(session_duration(s)),
            s.game.stutters
        );
        f.text_centered(x, w, 35, &fit_text(&text, w, 1), 1, true);
    }
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
    if reading_matches != 1 {
        return Metric::invalid();
    }

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

fn hang_slot(f: &mut Frame, hung: &[HungTarget], v: &View) {
    if hung.is_empty() {
        return;
    }
    let (x, w) = slot_bounds(2);
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
    if v.hung_hold > 0.0 {
        let fill = (width as f64 * v.hung_hold.min(1.0)) as i32;
        f.h_line(left, left + fill - 1, 42, true);
    }
}

#[allow(clippy::needless_range_loop)]
fn headset_slot(f: &mut Frame, x: i32, w: i32, b: &HeadsetBattery) {
    f.text_centered(x, w, 27, "7P+", 1, true);
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
    let mut text = format!("{}%", b.percent);
    if b.charging {
        text = format!("C{}", text);
    }
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
        f.text_centered(x, w, 35, &format!("{} L{}", n, max), 1, true);
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
) -> Option<&'a crate::model::Alert> {
    let mut best: Option<&crate::model::Alert> = None;
    for a in alerts {
        if a.severity < 2 || a.acknowledged || ack.contains(&a.id) {
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

    const GOLDEN_NORMAL: &str = "16eaeeb02f8ed4b89f721cad9557b749ad5ca2c24a7a9ff41b9682d678cec9a4";
    const GOLDEN_UNAVAILABLE: &str =
        "0ba19f9f03f908986009a86c3766755b5570e699fb97409dbabaa55a941a6940";
    const GOLDEN_STALE: &str = "2905e74cd8844a8b168a7575b663b889232c04aa8b0dedac7b57f58d42208854";

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

    #[test]
    fn render_is_deterministic_and_matches_golden() {
        let s = sample_snapshot();
        let a = render_dashboard(&s);
        let b = render_dashboard(&s);
        assert!(a.equal(&b), "frames differ");
        assert_eq!(a.hash_hex(), GOLDEN_NORMAL, "fixed dashboard changed");
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

        assert_eq!(
            Renderer::new()
                .render(&snapshot, OverlayOptions::default(), &view)
                .hash_hex(),
            "c93f879db203dbe8a974f797bec8f8f222c61191006b074207cf80f8a19f3cb2"
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
        let label_width = text_width("MEM", 1);
        for (name, metric) in cases {
            // Full bar
            let mut r = Renderer::new();
            let mut f = Frame::new();
            r.metric_bar(&mut f, 1, 18, "MEM", &metric, 75, 7);
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
            r.small_bar(&mut f, 16, 9, "C", metric, 62);
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
        previous.fill_rect(81, 19, text_width("VRAM", 1), 5, false);
        previous.text(81, 19, "VMEM", true);

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
                .all(|&(x, y)| (86..=91).contains(&x) && (19..=23).contains(&y)),
            "label diff escaped audited bounds: {changed:?}"
        );
    }

    #[test]
    fn fixed_bars_ignore_noncanonical_sources() {
        // The fixed MEM/GPU/VRAM bars read only the canonical readings; a
        // change to unrelated snapshot fields (temps feed slots, not bars)
        // must leave the bar regions pixel-identical.
        let s = sample_snapshot();
        let base = render_dashboard(&s);
        let mut changed = s.clone();
        changed.cpu_temp = Metric::valid(99.0, s.now.unwrap());
        changed.gpu_temp = Metric::valid(99.0, s.now.unwrap());
        let after = render_dashboard(&changed);
        // MEM bar region: x=14..75, y=18..24. GPU bar region: x=95..157, y=9..15.
        for y in 18..25 {
            for x in 14..76 {
                assert_eq!(
                    base.get(x, y),
                    after.get(x, y),
                    "MEM bar pixel ({},{})",
                    x,
                    y
                );
            }
        }
        for y in 9..16 {
            for x in 95..158 {
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
        r.slot(&mut online, 0, "HEADSET_BATTERY", &s);
        s.headset.stale = true;
        let mut stale = Frame::new();
        r.slot(&mut stale, 0, "HEADSET_BATTERY", &s);
        s.headset = crate::model::HeadsetBattery {
            raw_level: -1,
            ..Default::default()
        };
        let mut missing = Frame::new();
        r.slot(&mut missing, 0, "HEADSET_BATTERY", &s);
        assert!(!online.equal(&stale));
        assert!(!stale.equal(&missing));
        assert!(!online.equal(&missing));
    }

    #[test]
    fn single_ccd_mode_renders_one_full_bar() {
        let mut s = sample_snapshot();
        s.cpu_dual = false;
        s.cpu_cache_load = Metric::valid(63.0, s.now.unwrap());
        let dual = render_dashboard(&sample_snapshot());
        let single = render_dashboard(&s);
        assert!(!single.equal(&dual));
        // Single mode draws the "CPU" label at y=10 (dual draws it at y=11):
        // the C glyph's row-1 pixel lands at (1,11) only in single mode.
        assert!(single.get(1, 11), "CPU label at single-mode position");
        assert!(!dual.get(1, 11), "dual mode has different label geometry");
        // Single mode: full-height bar border at x=15, rows 9..15.
        assert!(single.get(15, 12), "full-height CPU bar left border");
        assert!(!dual.get(15, 12), "dual micro-bars start at x=20");
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
        hold_view.hung_hold = 0.5;
        let with_priority = renderer.render(&normal, OverlayOptions::default(), &hold_view);

        let mut only_hung = normal.clone();
        only_hung.alerts.clear();
        only_hung.discord = Default::default();
        let expected = Renderer::new().render(&only_hung, OverlayOptions::default(), &hold_view);
        assert!(with_priority.equal(&expected));
        assert!((81..100).all(|x| with_priority.get(x, 42)));

        normal.hung.clear();
        normal.alerts.clear();
        normal.discord = Default::default();
        let restored = renderer.render(&normal, OverlayOptions::default(), &view);
        assert!(restored.equal(&dashboard));
    }
}
