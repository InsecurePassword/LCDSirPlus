//! Canonical metric model shared by providers and the renderer.
//!
//! Ported from the original Go implementation (`internal/model`) with the
//! same display semantics: explicit availability/freshness states, canonical
//! byte values that never round-trip through float64, and legacy percentage
//! projections for the fixed bars.
#![allow(dead_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

/// Logical framebuffer geometry. Fixed by product contract.
pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 43;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ValueKind {
    #[default]
    Percent,
    Bytes,
    ByteRate,
    Celsius,
    Watts,
    Rpm,
    Count,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Freshness {
    #[default]
    Never,
    Current,
    Stale,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Availability {
    #[default]
    Unavailable,
    Available,
}

/// Canonical metric identities. Renderer selection is by identity, never by
/// provider name, so providers can be replaced without touching display code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum MetricKey {
    #[default]
    CPUUtilization,
    RAMUtilization,
    RAMUsed,
    RAMTotal,
    GPUUtilization,
    VRAMUtilization,
    VRAMUsed,
    VRAMTotal,
    CPUCCDTemp,
    CPUPeakUtilization,
    VRMTemp,
    ChipsetTemp,
    MotherboardTemp,
    CPUFanControl,
    CPUFanRpm,
    CPUFanPercent,
    PumpControl,
    PumpRpm,
    PumpPercent,
    TotalPower,
    CPUPower,
    GPUPower,
    CPUGPUPower,
    DiskReadBytesPerSec,
    DiskWriteBytesPerSec,
    PageReadsPerSec,
    EstablishedConnections,
}

/// One canonical reading published by a provider.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub key: MetricKey,
    pub kind: ValueKind,
    pub number: f64,
    pub bytes: u64,
    pub has_value: bool,
    pub availability: Availability,
    pub freshness: Freshness,
    pub hardware_id: String,
    pub sampled_at: Option<SystemTime>,
}

impl Reading {
    pub fn current_percent(key: MetricKey, value: f64, hardware_id: &str, at: SystemTime) -> Self {
        Reading {
            key,
            kind: ValueKind::Percent,
            number: value,
            has_value: true,
            availability: Availability::Available,
            freshness: Freshness::Current,
            hardware_id: hardware_id.to_string(),
            sampled_at: Some(at),
            ..Reading::default()
        }
    }

    pub fn current_bytes(key: MetricKey, bytes: u64, hardware_id: &str, at: SystemTime) -> Self {
        Reading {
            key,
            kind: ValueKind::Bytes,
            bytes,
            has_value: true,
            availability: Availability::Available,
            freshness: Freshness::Current,
            hardware_id: hardware_id.to_string(),
            sampled_at: Some(at),
            ..Reading::default()
        }
    }

    pub fn current_celsius(key: MetricKey, value: f64, hardware_id: &str, at: SystemTime) -> Self {
        Self::current_number(key, ValueKind::Celsius, value, hardware_id, at)
    }

    pub fn current_number(
        key: MetricKey,
        kind: ValueKind,
        value: f64,
        hardware_id: &str,
        at: SystemTime,
    ) -> Self {
        Reading {
            key,
            kind,
            number: value,
            has_value: true,
            availability: Availability::Available,
            freshness: Freshness::Current,
            hardware_id: hardware_id.to_string(),
            sampled_at: Some(at),
            ..Reading::default()
        }
    }

    /// Renderer-safe byte access without float conversion.
    pub fn byte_value(&self) -> Option<u64> {
        if self.has_value && self.kind == ValueKind::Bytes {
            Some(self.bytes)
        } else {
            None
        }
    }

    /// Percentage or Celsius projection into the legacy renderer metric.
    pub fn legacy_metric(&self) -> Metric {
        let mut metric = Metric {
            stale: self.freshness == Freshness::Stale,
            updated: self.sampled_at,
            ..Metric::default()
        };
        if self.has_value
            && self.availability == Availability::Available
            && self.kind != ValueKind::Bytes
        {
            metric.value = self.number;
            metric.valid = true;
        }
        metric
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(clippy::upper_case_acronyms)]
pub enum HardwareKind {
    #[default]
    Other,
    CPUPackage,
    CPUDomain,
    GPU,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HardwareRole {
    #[default]
    Unclassified,
    Cache,
    Frequency,
}

#[derive(Clone, Debug, Default)]
pub struct HardwareDescriptor {
    pub hardware_id: String,
    pub kind: HardwareKind,
    pub role: HardwareRole,
}

/// Immutable canonical snapshot published by the readings arbitrator.
#[derive(Clone, Debug, Default)]
pub struct ReadingsSnapshot {
    pub hardware: Vec<HardwareDescriptor>,
    pub metrics: Vec<Reading>,
}

impl ReadingsSnapshot {
    /// Selected reading for key + hardwareID. Empty hardwareID selects the
    /// first entry in canonical snapshot order.
    pub fn lookup(&self, key: MetricKey, hardware_id: &str) -> Option<&Reading> {
        self.metrics
            .iter()
            .find(|r| r.key == key && (hardware_id.is_empty() || r.hardware_id == hardware_id))
    }
}

/// Legacy renderer metric: a value plus explicit valid/stale display state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Metric {
    pub value: f64,
    pub valid: bool,
    pub stale: bool,
    pub updated: Option<SystemTime>,
}

impl Metric {
    pub fn valid(value: f64, at: SystemTime) -> Self {
        Metric {
            value,
            valid: true,
            stale: false,
            updated: Some(at),
        }
    }

    pub fn invalid() -> Self {
        Metric::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct HeadsetBattery {
    pub present: bool,
    pub online: bool,
    pub stale: bool,
    pub charging: bool,
    pub percent: i32,
    pub raw_level: i32,
}

#[derive(Clone, Debug, Default)]
pub struct ControllerBattery {
    pub connected: bool,
    pub index: i32,
    pub type_name: String,
    pub percent: i32,
}

#[derive(Clone, Debug, Default)]
pub struct GameStats {
    pub active: bool,
    pub process_name: String,
    pub game_name: String,
    pub fps: Metric,
    pub one_percent: Metric,
    pub point_one_low: Metric,
    pub frame_time_ms: Metric,
    pub stutters: i32,
    pub session_start: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct Speaker {
    pub user_id: String,
    pub name: String,
    pub speaking: bool,
    pub is_self: bool,
    pub started_at: Option<SystemTime>,
    pub stopped_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct DiscordState {
    pub connected: bool,
    pub authenticated: bool,
    pub channel_id: String,
    pub channel_name: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub voice_ping_ms: f64,
    pub connection_state: String,
    pub speakers: Vec<Speaker>,
    pub error: String,
    pub updated: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct Alert {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub severity: i32,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HungTarget {
    pub hwnd: usize,
    pub pid: u32,
    pub creation_time: u64,
    pub image_path: String,
    pub process_name: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AcState {
    Offline,
    Online,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBattery {
    pub ac: AcState,
    pub battery_present: Option<bool>,
    pub charging: bool,
    pub percent: Option<u8>,
    pub freshness: Freshness,
    pub sampled_at: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BottleneckState {
    #[default]
    Unavailable,
    None,
    Cpu,
    Gpu,
    Mem,
    DiskIo,
}

impl BottleneckState {
    pub fn token(self) -> Option<&'static str> {
        match self {
            Self::Unavailable => None,
            Self::None => Some("NONE"),
            Self::Cpu => Some("CPU"),
            Self::Gpu => Some("GPU"),
            Self::Mem => Some("RAM"),
            Self::DiskIo => Some("DISK I/O"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BottleneckReading {
    pub state: BottleneckState,
    pub freshness: Freshness,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BottleneckThresholds {
    pub cpu_percent: f64,
    pub gpu_percent: f64,
    pub memory_percent: f64,
    pub disk_mbps: f64,
    pub sustain: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BottleneckInputs {
    pub cpu_peak_percent: Option<f64>,
    pub gpu_percent: Option<f64>,
    pub ram_percent: Option<f64>,
    pub vram_percent: Option<f64>,
    pub disk_mbps: Option<f64>,
}

#[derive(Debug, Default)]
pub struct BottleneckDetector {
    thresholds: Option<BottleneckThresholds>,
    candidates: [Option<Instant>; 4],
}

impl BottleneckDetector {
    pub fn update(
        &mut self,
        thresholds: BottleneckThresholds,
        input: BottleneckInputs,
        now: Instant,
    ) -> BottleneckReading {
        if self.thresholds != Some(thresholds) {
            self.thresholds = Some(thresholds);
            self.candidates = [None; 4];
        }
        let memory = match (input.ram_percent, input.vram_percent) {
            (Some(ram), Some(vram)) => Some(ram.max(vram)),
            (ram, None) => ram,
            (None, vram) => vram,
        };
        let values = [
            (
                BottleneckState::Cpu,
                input.cpu_peak_percent,
                thresholds.cpu_percent,
            ),
            (
                BottleneckState::Gpu,
                input.gpu_percent,
                thresholds.gpu_percent,
            ),
            (BottleneckState::Mem, memory, thresholds.memory_percent),
            (
                BottleneckState::DiskIo,
                input.disk_mbps,
                thresholds.disk_mbps,
            ),
        ];
        let mut best = None;
        for (index, (state, value, threshold)) in values.into_iter().enumerate() {
            let ratio = value
                .filter(|value| value.is_finite() && *value >= threshold)
                .map(|value| value / threshold);
            let Some(ratio) = ratio else {
                self.candidates[index] = None;
                continue;
            };
            let since = *self.candidates[index].get_or_insert(now);
            if now.saturating_duration_since(since) >= thresholds.sustain
                && best.is_none_or(|(_, best_ratio)| ratio > best_ratio)
            {
                best = Some((state, ratio));
            }
        }
        if !input
            .cpu_peak_percent
            .is_some_and(|value| value.is_finite())
            || !input.ram_percent.is_some_and(|value| value.is_finite())
            || !input.disk_mbps.is_some_and(|value| value.is_finite())
        {
            return BottleneckReading::default();
        }
        BottleneckReading {
            state: best
                .map(|(state, _)| state)
                .unwrap_or(BottleneckState::None),
            freshness: Freshness::Current,
        }
    }
}

/// Full display input for one render tick.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub now: Option<SystemTime>,
    pub date_text: String,
    pub date_short_text: String,
    pub time_text: String,
    /// True on dual-CCD CPUs: stacked Cache/Frequency micro-bars. False on
    /// single-CCD CPUs: one full-height CPU bar (MEM-style).
    pub cpu_dual: bool,
    pub cpu_cache_load: Metric,
    pub cpu_freq_load: Metric,
    pub cpu_temp: Metric,
    pub gpu_temp: Metric,
    pub network_in: Metric,
    pub network_out: Metric,
    pub ping_ms: Metric,
    pub jitter_ms: Metric,
    pub packet_loss: Metric,
    pub audio_volume: Metric,
    pub microphone_known: bool,
    pub microphone_muted: bool,
    pub game: GameStats,
    pub headset: HeadsetBattery,
    pub controller: ControllerBattery,
    pub readings: ReadingsSnapshot,
    pub providers: HashMap<String, bool>,
    pub alerts: Vec<Alert>,
    pub discord: DiscordState,
    pub hung: Vec<HungTarget>,
    pub system_battery: SystemBattery,
    pub bottleneck: BottleneckReading,
}

/// Session duration helper shared by slot modules.
pub fn session_duration(s: &Snapshot) -> Duration {
    match (s.game.session_start, s.now) {
        (Some(start), Some(now)) => now.duration_since(start).unwrap_or(Duration::ZERO),
        _ => Duration::ZERO,
    }
}

/// `h:mm` above one hour, otherwise `mm:ss`.
pub fn format_duration(d: Duration) -> String {
    let h = d.as_secs() / 3600;
    let m = (d.as_secs() % 3600) / 60;
    let sec = d.as_secs() % 60;
    if h > 0 {
        format!("{}:{:02}", h, m)
    } else {
        format!("{:02}:{:02}", m, sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottleneck_state_tokens_are_stable_display_text() {
        for (state, token) in [
            (BottleneckState::Unavailable, None),
            (BottleneckState::None, Some("NONE")),
            (BottleneckState::Cpu, Some("CPU")),
            (BottleneckState::Gpu, Some("GPU")),
            (BottleneckState::Mem, Some("RAM")),
            (BottleneckState::DiskIo, Some("DISK I/O")),
        ] {
            assert_eq!(state.token(), token);
        }
    }

    #[test]
    fn lookup_empty_hardware_selects_first_in_order() {
        let at = SystemTime::UNIX_EPOCH;
        let readings = ReadingsSnapshot {
            metrics: vec![
                Reading::current_percent(MetricKey::GPUUtilization, 10.0, "gpu0", at),
                Reading::current_percent(MetricKey::GPUUtilization, 20.0, "gpu1", at),
            ],
            ..Default::default()
        };
        assert_eq!(
            readings
                .lookup(MetricKey::GPUUtilization, "")
                .unwrap()
                .number,
            10.0
        );
        assert_eq!(
            readings
                .lookup(MetricKey::GPUUtilization, "gpu1")
                .unwrap()
                .number,
            20.0
        );
        assert!(readings.lookup(MetricKey::GPUUtilization, "gpu2").is_none());
    }

    #[test]
    fn legacy_metric_rejects_bytes() {
        let at = SystemTime::UNIX_EPOCH;
        let r = Reading::current_bytes(MetricKey::RAMUsed, 5, "mem", at);
        assert!(!r.legacy_metric().valid);
        assert_eq!(r.byte_value(), Some(5));
    }

    #[test]
    fn legacy_metric_preserves_source_timestamp() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(9);
        let metric =
            Reading::current_number(MetricKey::CPUPower, ValueKind::Watts, 65.0, "cpu", at)
                .legacy_metric();
        assert_eq!(metric.updated, Some(at));
    }

    #[test]
    fn bottleneck_sustains_normalizes_and_resets() {
        let thresholds = BottleneckThresholds {
            cpu_percent: 90.0,
            gpu_percent: 95.0,
            memory_percent: 90.0,
            disk_mbps: 500.0,
            sustain: Duration::from_secs(2),
        };
        let at = Instant::now();
        let mut detector = BottleneckDetector::default();
        let both = BottleneckInputs {
            cpu_peak_percent: Some(99.0),
            gpu_percent: Some(100.0),
            ram_percent: Some(20.0),
            disk_mbps: Some(0.0),
            ..Default::default()
        };
        assert_eq!(
            detector.update(thresholds, both, at).state,
            BottleneckState::None
        );
        assert_eq!(
            detector
                .update(thresholds, both, at + Duration::from_secs(2))
                .state,
            BottleneckState::Cpu
        );
        let clear = BottleneckInputs {
            cpu_peak_percent: Some(1.0),
            ram_percent: Some(1.0),
            disk_mbps: Some(1.0),
            ..Default::default()
        };
        assert_eq!(
            detector
                .update(thresholds, clear, at + Duration::from_secs(3))
                .state,
            BottleneckState::None
        );
        assert_eq!(
            detector
                .update(thresholds, BottleneckInputs::default(), at)
                .state,
            BottleneckState::Unavailable
        );
        assert_eq!(
            detector
                .update(
                    thresholds,
                    BottleneckInputs {
                        cpu_peak_percent: Some(f64::NAN),
                        ram_percent: Some(1.0),
                        disk_mbps: Some(1.0),
                        ..Default::default()
                    },
                    at,
                )
                .state,
            BottleneckState::Unavailable
        );
        let changed = BottleneckThresholds {
            cpu_percent: 99.0,
            ..thresholds
        };
        assert_eq!(
            detector
                .update(changed, both, at + Duration::from_secs(4))
                .state,
            BottleneckState::None
        );

        let exact_tie = BottleneckInputs {
            cpu_peak_percent: Some(90.0),
            gpu_percent: Some(95.0),
            ram_percent: Some(1.0),
            disk_mbps: Some(1.0),
            ..Default::default()
        };
        let immediate = BottleneckThresholds {
            sustain: Duration::ZERO,
            ..thresholds
        };
        assert_eq!(
            BottleneckDetector::default()
                .update(immediate, exact_tie, at)
                .state,
            BottleneckState::Cpu,
            "normalized ties use stable CPU-first classification"
        );
    }

    #[test]
    fn bottleneck_classifies_gpu_memory_and_disk() {
        let thresholds = BottleneckThresholds {
            cpu_percent: 90.0,
            gpu_percent: 95.0,
            memory_percent: 90.0,
            disk_mbps: 500.0,
            sustain: Duration::ZERO,
        };
        let at = Instant::now();
        for (expected, input) in [
            (
                BottleneckState::Gpu,
                BottleneckInputs {
                    cpu_peak_percent: Some(1.0),
                    gpu_percent: Some(96.0),
                    ram_percent: Some(1.0),
                    disk_mbps: Some(1.0),
                    ..Default::default()
                },
            ),
            (
                BottleneckState::Mem,
                BottleneckInputs {
                    cpu_peak_percent: Some(1.0),
                    ram_percent: Some(1.0),
                    vram_percent: Some(91.0),
                    disk_mbps: Some(1.0),
                    ..Default::default()
                },
            ),
            (
                BottleneckState::DiskIo,
                BottleneckInputs {
                    cpu_peak_percent: Some(1.0),
                    ram_percent: Some(1.0),
                    disk_mbps: Some(501.0),
                    ..Default::default()
                },
            ),
        ] {
            assert_eq!(
                BottleneckDetector::default()
                    .update(thresholds, input, at)
                    .state,
                expected
            );
        }
    }

    #[test]
    fn bottleneck_candidates_sustain_independently_across_winner_changes() {
        let thresholds = BottleneckThresholds {
            cpu_percent: 90.0,
            gpu_percent: 90.0,
            memory_percent: 90.0,
            disk_mbps: 100.0,
            sustain: Duration::from_secs(2),
        };
        let start = Instant::now();
        let mut detector = BottleneckDetector::default();
        let input = |cpu, gpu| BottleneckInputs {
            cpu_peak_percent: Some(cpu),
            gpu_percent: Some(gpu),
            ram_percent: Some(1.0),
            disk_mbps: Some(0.0),
            ..Default::default()
        };
        assert_eq!(
            detector.update(thresholds, input(95.0, 1.0), start).state,
            BottleneckState::None
        );
        assert_eq!(
            detector
                .update(
                    thresholds,
                    input(95.0, 100.0),
                    start + Duration::from_secs(1),
                )
                .state,
            BottleneckState::None
        );
        assert_eq!(
            detector
                .update(
                    thresholds,
                    input(95.0, 100.0),
                    start + Duration::from_secs(2),
                )
                .state,
            BottleneckState::Cpu,
            "brief ratio winner changes do not reset CPU sustain"
        );
        assert_eq!(
            detector
                .update(
                    thresholds,
                    input(95.0, 100.0),
                    start + Duration::from_secs(3),
                )
                .state,
            BottleneckState::Gpu
        );
        assert_eq!(
            detector
                .update(thresholds, input(95.0, 1.0), start + Duration::from_secs(4),)
                .state,
            BottleneckState::Cpu,
            "only the stopped GPU candidate is cleared"
        );
    }

    #[test]
    fn format_duration_matches_go_semantics() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00");
        assert_eq!(format_duration(Duration::from_secs(59)), "00:59");
        assert_eq!(format_duration(Duration::from_secs(60)), "01:00");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1:01");
    }
}
