//! Canonical metric model shared by providers and the renderer.
//!
//! Ported from the original Go implementation (`internal/model`) with the
//! same display semantics: explicit availability/freshness states, canonical
//! byte values that never round-trip through float64, and legacy percentage
//! projections for the fixed bars.

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

    // Only test caller left (src/telemetry.rs:2261, a #[cfg(test)] module outside this scope);
    // delete this wrapper together with that caller.
    #[allow(dead_code)]
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

// Kept: the only reader was the retired CPU-cache/frequency temperature surface, and the field is
// named by a struct literal outside this unit's file scope (`src/app.rs:3803` keeps
// `..Default::default()`). Follow-up: delete this field, these three types and that row together.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(clippy::upper_case_acronyms)]
pub enum HardwareKind {
    #[default]
    Other,
    CPUPackage,
    CPUDomain,
    GPU,
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HardwareRole {
    #[default]
    Unclassified,
    Cache,
    Frequency,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct HardwareDescriptor {
    pub hardware_id: String,
    pub kind: HardwareKind,
    pub role: HardwareRole,
}

/// Immutable canonical snapshot published by the readings arbitrator.
#[derive(Clone, Debug, Default)]
pub struct ReadingsSnapshot {
    #[allow(dead_code)]
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

/// RC-5 published device/recovery state for the human-facing surface (the
/// `PROVIDER_STATUS`-family panel module and the log). Each arm has exactly
/// one fixed label; the cause-coded arms carry an RC-1 cause code
/// (`LossCause::short()`, code included where the cause has one) and never
/// the transport `reason` free text, so a failure is named by code rather
/// than by concatenated prose. All four non-healthy arms keep the existing
/// preview-retained fallback: the panel is never blanked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Connected: the module keeps the existing provider summary.
    #[default]
    Healthy,
    /// (a) Device absent (RC-6 ladder terminal: no candidates).
    Absent,
    /// (b) Device present, connection failed: named RC-1 cause code.
    LinkFailed { code: String },
    /// (c) Recovery allowance consumed: no further automatic action.
    AllowanceConsumed,
    /// (d) Recovery action failed: named RC-1 cause code.
    RecoveryFailed { code: String },
}

impl DeviceStatus {
    pub const LABEL_HEALTHY: &'static str = "OK";
    pub const LABEL_ABSENT: &'static str = "ABSENT";
    pub const LABEL_ALLOWANCE: &'static str = "NO RETRY";
    pub const PREFIX_LINK: &'static str = "LINK ";
    pub const PREFIX_RECOVERY: &'static str = "RECOVER ";

    /// Panel/log label: enum arm template plus the cause code, nothing else.
    pub fn label(&self) -> String {
        match self {
            Self::Healthy => Self::LABEL_HEALTHY.to_string(),
            Self::Absent => Self::LABEL_ABSENT.to_string(),
            Self::AllowanceConsumed => Self::LABEL_ALLOWANCE.to_string(),
            Self::LinkFailed { code } => format!("{}{code}", Self::PREFIX_LINK),
            Self::RecoveryFailed { code } => format!("{}{code}", Self::PREFIX_RECOVERY),
        }
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
    // Carried by the host projection `state_from_pod` (src/providers/discord.rs:907) from the
    // byte-identical component ABI block; the renderer consumers were retired, so the compiler
    // names these dead. Kept: removing them requires editing the ABI projection outside this scope.
    #[allow(dead_code)]
    pub channel_id: String,
    pub channel_name: String,
    pub self_mute: bool,
    #[allow(dead_code)]
    pub self_deaf: bool,
    #[allow(dead_code)]
    pub voice_ping_ms: f64,
    #[allow(dead_code)]
    pub connection_state: String,
    pub speakers: Vec<Speaker>,
    #[allow(dead_code)]
    pub error: String,
    #[allow(dead_code)]
    pub updated: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct HungTarget {
    pub hwnd: usize,
    pub pid: u32,
    pub creation_time: u64,
    pub image_path: String,
    pub process_name: String,
    pub title: String,
    /// TC-1 freshness stamp: the monotonic publish generation of the detector
    /// publication that carried this target; `0` = unstamped, never actionable.
    pub publish_generation: u64,
    /// Monotonic publish time of that publication; `None` = unstamped.
    pub publish_time: Option<Instant>,
}

impl PartialEq for HungTarget {
    /// Identity/display equality only: the publish stamp is freshness metadata, so
    /// the press-binding continuity checks and the action-boundary revalidation
    /// keep their pre-TC-1 identity meaning across republishes.
    fn eq(&self, other: &Self) -> bool {
        self.hwnd == other.hwnd
            && self.pid == other.pid
            && self.creation_time == other.creation_time
            && self.image_path == other.image_path
            && self.process_name == other.process_name
            && self.title == other.title
    }
}

impl Eq for HungTarget {}

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
    /// RC-5: the device/recovery state published on the human-facing surface.
    /// `Healthy` reproduces the pre-RC-5 provider summary exactly.
    pub device_status: DeviceStatus,
    pub discord: DiscordState,
    pub hung: Vec<HungTarget>,
    pub system_battery: SystemBattery,
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
    fn format_duration_matches_go_semantics() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00");
        assert_eq!(format_duration(Duration::from_secs(59)), "00:59");
        assert_eq!(format_duration(Duration::from_secs(60)), "01:00");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1:01");
    }

    #[test]
    fn device_status_labels_are_four_distinct_arm_templates() {
        let four = [
            DeviceStatus::Absent,
            DeviceStatus::LinkFailed {
                code: "open-failed(0x80070002)".to_string(),
            },
            DeviceStatus::AllowanceConsumed,
            DeviceStatus::RecoveryFailed {
                code: "worker-stalled".to_string(),
            },
        ];
        assert_eq!(four[0].label(), DeviceStatus::LABEL_ABSENT);
        assert_eq!(
            four[1].label(),
            format!("{}open-failed(0x80070002)", DeviceStatus::PREFIX_LINK)
        );
        assert_eq!(four[2].label(), DeviceStatus::LABEL_ALLOWANCE);
        assert_eq!(
            four[3].label(),
            format!("{}worker-stalled", DeviceStatus::PREFIX_RECOVERY)
        );
        for (index, left) in four.iter().enumerate() {
            for right in four[index + 1..].iter() {
                assert_ne!(left.label(), right.label(), "published labels must differ");
            }
        }
        assert_eq!(DeviceStatus::default(), DeviceStatus::Healthy);
        assert_eq!(DeviceStatus::Healthy.label(), DeviceStatus::LABEL_HEALTHY);
    }
}
