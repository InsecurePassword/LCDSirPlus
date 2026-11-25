//! Phase 2 telemetry runtime. Blocking headset HID and LHM HTTP calls have
//! dedicated persistent workers; the coordinator handles fast native polls,
//! retention, staleness, and publication to the render loop.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::model::{
    AcState, Availability, ControllerBattery, Freshness, GameStats, HeadsetBattery, HungTarget,
    Metric, MetricKey, Reading, SystemBattery, ValueKind,
};
use crate::providers::{
    audio, connections, gpu, hang, headset, hwinfo, lhm, netif, network, performance, presentmon,
    system_power, xinput,
};

const DEFAULT_LHM_URL: &str = "http://127.0.0.1:8085/data.json";
/// WO-OPT02B (finding O1, slice 2): the 50 ms tick is RETIRED from production -
/// the five loops it paced now park to their own deadlines (`sampling_loop_park`,
/// `Worker::park_bound`). The behavioral tests keep it as the cadence ruler
/// (`BEHAVIORAL_SUPPRESS_WINDOW / TICK`, the `cadence - 3 * TICK` floors), so it
/// is now a test-only constant, which keeps `clippy -D warnings` free of
/// `dead_code` without adding a suppression.
#[cfg(test)]
const TICK: Duration = Duration::from_millis(50);
/// WO-OPT02B: the floor of every telemetry-runtime deadline park. The host
/// timer resolution is ~15.6 ms, so a park shorter than this is a spin. This is
/// this file's own constant, mirroring `app.rs`'s `APP_LOOP_MIN_PARK`; the two
/// loops are separate lanes and the value is deliberately not imported.
const LOOP_MIN_PARK: Duration = Duration::from_millis(2);
const NETWORK_POLL: Duration = Duration::from_secs(1);
const EXTENDED_POLL: Duration = Duration::from_secs(1);
const EXTENDED_STALE: Duration = Duration::from_secs(3);
const BATTERY_POLL: Duration = Duration::from_secs(5);
const BATTERY_STALE: Duration = Duration::from_secs(15);
/// WO-OPT02B-PARKFIX (F-AUD-OPT02B-1): the coordinator's WAKE bound for feed
/// publications. The publish floor is the app's own consumption cadence, so an
/// uncapped `telemetry_interval` (legal 100..10000 ms, `config.rs:416-421`) let a
/// fresh reading sit unread for a whole interval - 10 s at the ceiling - and
/// doubled the displayed age. This caps the WAKE only: publishing, the publish
/// gate, staleness, the TTLs and the EC-3 cadence math are untouched, and a floor
/// below the cap keeps its own number, so the shipped 300 ms default and the
/// pinned `park_bound` fixture are unaffected.
const PUBLISH_WAKE_CAP: Duration = Duration::from_millis(300);

/// WO-QA-03 (TC-4 follow-up F-b, display-only): the provider-health label the hang
/// detector publishes when a cycle lost probes it could not evaluate. The ONE site that
/// renders the token `probe-indeterminate-<N>` stays in the provider
/// (`providers/hang.rs` L110-112 `probe_indeterminate_reason`, read through the public
/// `Update::availability_reason`); this const is only the matching key by which the
/// panel (`render/renderer.rs` `provider_slot`) and the app's staleness path recognize
/// that label, so the vocabulary has one rendering site and one matching site.
pub const PROBE_INDETERMINATE: &str = "probe-indeterminate";

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub generation: u64,
    pub headset: HeadsetBattery,
    pub controller: ControllerBattery,
    pub audio: Option<audio::AudioState>,
    pub audio_sampled_at: Option<SystemTime>,
    pub net: Option<netif::NetThroughput>,
    pub cpu_temp: Metric,
    pub gpu_temp: Metric,
    pub gpu_readings: Vec<Reading>,
    pub extended_readings: Vec<Reading>,
    pub system_battery: SystemBattery,
    pub game: GameStats,
    pub hung: Vec<HungTarget>,
    pub ping_ms: Metric,
    pub jitter_ms: Metric,
    pub packet_loss: Metric,
    /// Per-provider health: (name, last poll succeeded).
    pub providers: Vec<(String, bool)>,
}

pub struct Telemetry {
    pub updates: LatestUpdates,
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.wake_workers();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Telemetry {
    pub fn update_config(&self, config: &Config) -> u64 {
        let mut current = self.config.write().unwrap_or_else(|e| e.into_inner());
        *current = config.clone();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        drop(current);
        self.wake_workers();
        generation
    }

    fn wake_workers(&self) {
        for worker in &self.threads {
            worker.thread().unpark();
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// EC-3: publish which gated providers have consumers right now (bit per
    /// `GATED_PROVIDERS` entry). Event-driven, from the app's render path; the
    /// scheduler reads it at most once per tick.
    pub fn set_consumers(&self, consumers: u64) {
        if self.consumers.swap(consumers, Ordering::Relaxed) != consumers {
            self.wake_workers();
        }
    }
}

enum SlowEvent {
    Headset(u64, Result<headset::Status, String>),
    Lhm(u64, Box<Result<(lhm::ExtendedSample, SystemTime), String>>),
    Gpu(u64, Result<gpu::Sample, String>),
    Hwinfo(u64, hwinfo::Selectors, Result<hwinfo::Sample, String>),
}

#[derive(Clone)]
struct LatestSender {
    latest: Arc<Mutex<Option<Update>>>,
    notify: mpsc::SyncSender<()>,
}

pub struct LatestUpdates {
    latest: Arc<Mutex<Option<Update>>>,
    notify: mpsc::Receiver<()>,
}

impl LatestUpdates {
    pub fn try_recv(&self) -> Result<Update, mpsc::TryRecvError> {
        self.notify.try_recv()?;
        self.latest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or(mpsc::TryRecvError::Empty)
    }
}

impl LatestSender {
    fn send(&self, update: Update) -> bool {
        *self
            .latest
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(update);
        match self.notify.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => true,
            Err(mpsc::TrySendError::Disconnected(())) => false,
        }
    }
}

fn latest_channel() -> (LatestSender, LatestUpdates) {
    let latest = Arc::new(Mutex::new(None));
    let (notify, receiver) = mpsc::sync_channel(1);
    (
        LatestSender {
            latest: Arc::clone(&latest),
            notify,
        },
        LatestUpdates {
            latest,
            notify: receiver,
        },
    )
}

struct Worker {
    config: Arc<RwLock<Config>>,
    config_generation: Arc<AtomicU64>,
    request_cache: (u64, Arc<Config>),
    consumers: Arc<AtomicU64>,
    tx: LatestSender,
    slow_rx: mpsc::Receiver<SlowEvent>,
    update: Update,
    net: netif::NetProvider,
    performance: performance::Provider,
    headset_success: Option<Instant>,
    lhm_success: Option<Instant>,
    gpu_success: Option<Instant>,
    performance_success: Option<Instant>,
    connections_success: Option<Instant>,
    battery_success: Option<Instant>,
    hwinfo_success: Option<Instant>,
    network_success: Option<Instant>,
    // WO-U46I (#3b): the presentmon worker's last accepted, current-generation, AVAILABLE
    // publication; the same success-stamp shape as `gpu_success` / `network_success`.
    presentmon_success: Option<Instant>,
    controller_attempt: Option<Instant>,
    audio_attempt: Option<Instant>,
    net_attempt: Option<Instant>,
    performance_attempt: Option<Instant>,
    connections_attempt: Option<Instant>,
    battery_attempt: Option<Instant>,
    performance_failures: u32,
    presentmon_rx: mpsc::Receiver<presentmon::Update>,
    hang_rx: mpsc::Receiver<hang::Update>,
    network_rx: mpsc::Receiver<network::Update>,
    hwinfo_cpu_temp: Metric,
    lhm_cpu_temp: Metric,
    native_gpu_temp: Metric,
    lhm_gpu_temp: Metric,
    gpu_backend: String,
    native_gpu_power: Option<Reading>,
    lhm_readings: Vec<Reading>,
    hwinfo_readings: Vec<Reading>,
    performance_readings: Vec<Reading>,
    connection_reading: Option<Reading>,
    hwinfo_diagnostic: Option<HwinfoDiagnostic>,
    lhm_diagnostic: Option<&'static str>,
    generation: u64,
    safe_mode_active: bool,
    publish_dirty: bool,
    last_publish: Option<Instant>,
}

fn current_config(config: &RwLock<Config>) -> Config {
    config.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn current_request(config: &RwLock<Config>, generation: &AtomicU64) -> (u64, Config) {
    loop {
        let before = generation.load(Ordering::SeqCst);
        let config = current_config(config);
        let after = generation.load(Ordering::SeqCst);
        if before == after {
            return (after, config);
        }
    }
}

/// WO-TEL-D2 (S5, U-8): the generation-guarded request read for the five
/// 50 ms-tick hot sites (coordinator tick, four sampling loops); the presentmon
/// worker loop head carries the same-shaped copy in `providers/presentmon.rs`.
/// A `50 ms` tick under an unchanged generation reuses the cached `Arc<Config>`
/// (one refcount bump) instead of a full `Config` clone; the config only ever
/// changes when the generation moves (`Telemetry::update_config` writes before
/// the bump), so the cache is valid for the whole generation. A changed
/// generation takes the shipped `current_request` rule copy above, so the
/// cached value belongs to the generation reported with it and the shipped
/// `SeqCst` pairing is untouched. The network worker keeps its shipped
/// per-iteration `current_request` (F-D2-3: its EC-8 mid-probe pins change
/// config content at a fixed generation).
fn cached_request(
    config: &RwLock<Config>,
    generation: &AtomicU64,
    cache: &mut (u64, Arc<Config>),
) -> (u64, Arc<Config>) {
    let before = generation.load(Ordering::SeqCst);
    if before == cache.0 && before != 0 && generation.load(Ordering::SeqCst) == before {
        return (before, Arc::clone(&cache.1));
    }
    // The fresh read IS the shipped rule copy (no duplicated `SeqCst` pair):
    // `current_request` pairs the lock read with the generation loads, so the
    // value cached here belongs to the generation reported with it.
    let (generation, config) = current_request(config, generation);
    *cache = (generation, Arc::new(config));
    (generation, Arc::clone(&cache.1))
}

fn due(last_attempt: Option<Instant>, period: Duration, now: Instant) -> bool {
    last_attempt
        .map(|last| now.duration_since(last) >= period)
        .unwrap_or(true)
}

/// WO-OPT02B (finding O1, slice 2): how long a running sampling loop parks,
/// derived from the deadline the shipped `due` gate already computes -
/// `last_attempt + interval`, with the loop's own `sampling_interval` /
/// `backoff_interval` result as `interval` (no new cadence, no interval-math
/// change). A cleared stamp (`None`) is the shipped "due now", so the park
/// collapses to the 2 ms floor. `Thread::unpark` INTERRUPTS a `park_timeout`,
/// which a `sleep` cannot be, so the existing edges (`set_consumers`,
/// `update_config`, `Drop`/`wake_workers`) reach a RUNNING loop immediately.
/// The floor keeps the park from spinning when a provider call outlasts the
/// cadence: that iteration is due again, exactly as the shipped tick made it.
fn sampling_loop_park(last_attempt: Option<Instant>, interval: Duration, now: Instant) -> Duration {
    last_attempt
        .and_then(|last| last.checked_add(interval))
        .map_or(LOOP_MIN_PARK, |next_due| {
            next_due.saturating_duration_since(now)
        })
        .max(LOOP_MIN_PARK)
}

/// WO-OPT02B-PARKFIX (F-AUD-OPT02B-2): fold one staleness deadline
/// (`last_success + stale_after`) into the coordinator's earliest-deadline set. A
/// cleared success stamp sets NO deadline - nothing is displayed as fresh - which
/// is the opposite of a cleared ATTEMPT stamp, the shipped `due(None)` = "poll
/// now", so this is deliberately not the `consider` shape of the poll dues.
fn park_deadline_min(
    next: Option<Instant>,
    last_success: Option<Instant>,
    bound: Duration,
) -> Option<Instant> {
    match last_success.and_then(|last| last.checked_add(bound)) {
        Some(deadline) => Some(next.map_or(deadline, |current| current.min(deadline))),
        None => next,
    }
}

fn stale(last_success: Option<Instant>, stale_after: Duration, now: Instant) -> bool {
    last_success
        .map(|last| now.duration_since(last) > stale_after)
        .unwrap_or(false)
}

/// EC-3: providers whose sampling the scheduler gates to a bounded keep-alive
/// interval while no consumer reads them. Every entry has an existing staleness
/// bound, and the keep-alive is derived from that same bound, so publication
/// keeps using the unchanged staleness rules: a gated provider never publishes a
/// "looks-fresh" value it did not just sample.
pub const GATED_PROVIDERS: [&str; 7] = [
    "headset",
    "lhm",
    "gpu",
    "hwinfo",
    "performance",
    "connections",
    "system battery",
];

/// Bit for one gated provider; `0` for provider names the scheduler never gates.
pub fn consumer_bit(provider: &str) -> u64 {
    GATED_PROVIDERS
        .iter()
        .position(|name| *name == provider)
        .map_or(0, |index| 1 << index)
}

/// Test fixture demand for every gated provider.
#[cfg(test)]
pub const ALL_CONSUMERS: u64 = (1 << GATED_PROVIDERS.len()) - 1;

/// WO-TEL-D2 (S1): the never-gated metering providers the consumer word gains
/// demand bits for, so a reading with no displayed consumer collects nothing.
/// The 7-bit gated protocol above is unchanged; these names occupy bits
/// immediately above it in the SAME published word, and a reading-keyed demand
/// map in the app's derivation (`app.rs` `module_extended_consumers`) feeds
/// them from the existing publication path. Providers outside both lists
/// (hang, network probe, the app-loop trio) stay ungated by construction:
/// they carry no bit, so no demand can stop them.
pub const EXTENDED_CONSUMER_PROVIDERS: [&str; 4] = ["controller", "audio", "net", "presentmon"];

/// Demand bit for one metering provider; `0` for names outside
/// `EXTENDED_CONSUMER_PROVIDERS`. Distinct from `consumer_bit`: the gated and
/// extended bit ranges never overlap.
pub fn extended_consumer_bit(provider: &str) -> u64 {
    EXTENDED_CONSUMER_PROVIDERS
        .iter()
        .position(|name| *name == provider)
        .map_or(0, |index| 1 << (GATED_PROVIDERS.len() + index))
}

/// Test affordance: every metering provider consumed at once (the full demand
/// word the fixtures publish so a shipped pin can drive every gated arm).
/// Production never publishes it - the app derives the word from the displayed
/// modules (`app.rs` `provider_consumer_bits | provider_demand_bits`).
#[cfg(test)]
pub const ALL_EXTENDED_CONSUMERS: u64 =
    ((1 << EXTENDED_CONSUMER_PROVIDERS.len()) - 1) << GATED_PROVIDERS.len();

/// Demand view of one published word: true when the provider's own bit (gated
/// or extended) is set; names with no bit are never demand-gated.
pub fn demanded(consumers: u64, provider: &str) -> bool {
    let bit = consumer_bit(provider) | extended_consumer_bit(provider);
    bit != 0 && consumers & bit != 0
}

fn gpu_stale_after(cfg: &Config) -> Duration {
    cfg.telemetry_interval
        .saturating_mul(3)
        .max(Duration::from_secs(3))
}

/// WO-U46I (#3b): the presentmon staleness bound, derived from the provider's own publication
/// cadence exactly as `gpu_stale_after` and the network-probe arm derive theirs (no new config
/// surface): `presentmon_interval * 3`, floored at 3 s.
fn presentmon_stale_after(cfg: &Config) -> Duration {
    cfg.presentmon_interval
        .saturating_mul(3)
        .max(Duration::from_secs(3))
}

/// WO-TEL-D3-B1 (L2, core tier): the explicit core-unavailable reading rows a gpu park publishes.
/// The four keys are the core set — GPU utilization + VRAM utilization/used/total — i.e. the keys
/// `providers/gpu.rs::project` publishes from a real sample (L194-220). Each row carries
/// `Availability::Unavailable` and `has_value = false`, the shipped no-value shape: the renderer
/// routes it to `DisplayState::Unavailable` (`renderer.rs:1705-1723`) and every reading lookup
/// requires `has_value`, so a parked core reading is STATED,
/// never silent, and can never present a stale value as fresh. `freshness = Current` labels the
/// availability statement (made now), not a measurement.
fn core_gpu_unavailable_readings() -> Vec<Reading> {
    let at = SystemTime::now();
    [
        (MetricKey::GPUUtilization, ValueKind::Percent),
        (MetricKey::VRAMUtilization, ValueKind::Percent),
        (MetricKey::VRAMUsed, ValueKind::Bytes),
        (MetricKey::VRAMTotal, ValueKind::Bytes),
    ]
    .into_iter()
    .map(|(key, kind)| Reading {
        key,
        kind,
        availability: Availability::Unavailable,
        freshness: Freshness::Current,
        hardware_id: "gpu:core".to_string(),
        sampled_at: Some(at),
        ..Reading::default()
    })
    .collect()
}

/// Keep-alive bound of a gated provider: its own existing stale-after interval.
/// `None` means the provider has no staleness rule and is therefore never gated.
fn keep_alive_interval(cfg: &Config, provider: &str) -> Option<Duration> {
    let bound = match provider {
        "headset" => cfg.headset_stale_after,
        "lhm" => cfg.lhm_stale_after,
        "gpu" => gpu_stale_after(cfg),
        "hwinfo" => cfg.hwinfo_stale_after,
        "performance" | "connections" => EXTENDED_STALE,
        "system battery" => BATTERY_STALE,
        _ => return None,
    };
    Some(bound.max(Duration::from_millis(1)))
}

/// EC-3 sampling gate. A consumed provider keeps the cadence the caller already
/// used (the exact pre-EC-3 expression); with no consumer it samples at the
/// keep-alive bound, and a rejoining consumer restores full cadence on the next
/// scheduled tick because `due` is re-evaluated with the full interval.
pub fn sampling_interval(cfg: &Config, consumers: u64, provider: &str, full: Duration) -> Duration {
    let bit = consumer_bit(provider);
    if bit != 0 && consumers & bit == 0 {
        return keep_alive_interval(cfg, provider).unwrap_or(full).max(full);
    }
    full
}

/// WO-TEL-D2 (S4): the provider-outcome backoff that replaces the shipped
/// failure-inverts-cadence shape (`temps_need_refresh`). Consecutive provider
/// failures grow the demanded cadence by doubling from `base` toward the
/// provider's own staleness bound `bound` — the same bound EC-3 derives — so a
/// permanently failing provider is polled no faster than the keep-alive need,
/// never at the full cadence. A success returns `base`. `failures == 0` is the
/// healthy path and returns `base` exactly, so a demanded, healthy provider is
/// indistinguishable from the shipped cadence. The doubling shift is clamped so
/// `base << shift` cannot wrap a `u32` milliseconds value: the clamp saturates
/// far above every shipped bound (128 × base ≥ bound within 8 doublings).
pub fn backoff_interval(base: Duration, bound: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return base;
    }
    let shift = failures.min(8);
    base.saturating_mul(1u32 << shift)
        .min(bound.max(base))
        .max(base)
}

fn enabled(cfg: &Config, provider: &str) -> bool {
    if cfg.safe_mode {
        return false;
    }
    match provider {
        "headset" => cfg.headset_enabled,
        "lhm" => cfg.lhm_mode != "off",
        "controller" => cfg.controller_enabled,
        "audio" => cfg.audio_enabled,
        "net" => true,
        "network_probe" => cfg.network_probe_enabled,
        "gpu" => cfg.gpu_provider != "off",
        "presentmon" => cfg.presentmon_enabled && cfg.presentmon_target_mode != "disabled",
        "hwinfo" => hwinfo_configured(cfg),
        _ => false,
    }
}

fn hwinfo_configured(cfg: &Config) -> bool {
    !cfg.hwinfo_cpu_temp_sensor.is_empty()
        || !cfg.hwinfo_total_power_sensor.is_empty()
        || !cfg.hwinfo_cpu_power_sensor.is_empty()
        || !cfg.hwinfo_gpu_power_sensor.is_empty()
}

fn hwinfo_selectors(cfg: &Config) -> hwinfo::Selectors {
    let selector = |sensor: &str, reading: &str| hwinfo::Selector {
        sensor_label: sensor.into(),
        reading_label: reading.into(),
    };
    hwinfo::Selectors {
        cpu_temp: selector(&cfg.hwinfo_cpu_temp_sensor, &cfg.hwinfo_cpu_temp_reading),
        total_power: selector(
            &cfg.hwinfo_total_power_sensor,
            &cfg.hwinfo_total_power_reading,
        ),
        cpu_package_power: selector(&cfg.hwinfo_cpu_power_sensor, &cfg.hwinfo_cpu_power_reading),
        gpu_board_power: selector(&cfg.hwinfo_gpu_power_sensor, &cfg.hwinfo_gpu_power_reading),
    }
}

fn lhm_selectors(cfg: &Config) -> lhm::ExtendedSelectors {
    let get = |key: &str| cfg.lhm_sensors.get(key).cloned().unwrap_or_default();
    lhm::ExtendedSelectors {
        vrm_temp: get("vrm_temp_sensor"),
        chipset_temp: get("chipset_temp_sensor"),
        motherboard_temp: get("motherboard_temp_sensor"),
        cpu_fan_control: get("cpu_fan_control_sensor"),
        cpu_fan_rpm: get("cpu_fan_rpm_sensor"),
        pump_control: get("pump_control_sensor"),
        pump_rpm: get("pump_rpm_sensor"),
        total_power: get("total_power_sensor"),
        cpu_package_power: get("cpu_power_sensor"),
        gpu_board_power: get("gpu_power_sensor"),
    }
}

fn lhm_url(cfg: &Config) -> Option<&str> {
    if !enabled(cfg, "lhm") {
        None
    } else if cfg.lhm_url == "auto" {
        Some(DEFAULT_LHM_URL)
    } else {
        Some(&cfg.lhm_url)
    }
}

fn temperature(sensor: &Option<lhm::Sensor>, at: SystemTime) -> Metric {
    sensor
        .as_ref()
        .filter(|sensor| sensor.valid)
        .map(|sensor| Metric::valid(sensor.value, at))
        .unwrap_or_default()
}

fn prefer_cpu_temperature(hwinfo: Metric, lhm: Metric) -> Metric {
    [hwinfo, lhm]
        .into_iter()
        .find(|metric| metric.valid && !metric.stale)
        .or_else(|| [hwinfo, lhm].into_iter().find(|metric| metric.valid))
        .unwrap_or_default()
}

fn prefer_native_temperature(native: Metric, fallback: Metric) -> Metric {
    if native.valid && !native.stale {
        native
    } else if fallback.valid && !fallback.stale {
        fallback
    } else if native.valid {
        native
    } else {
        fallback
    }
}

fn apply_lhm_freshness(
    lhm_cpu_temp: &mut Metric,
    lhm_gpu_temp: &mut Metric,
    last_success: Option<Instant>,
    stale_after: Duration,
    now: Instant,
) {
    let is_stale = stale(last_success, stale_after, now);
    lhm_cpu_temp.stale = lhm_cpu_temp.valid && is_stale;
    lhm_gpu_temp.stale = lhm_gpu_temp.valid && is_stale;
}

const HWINFO_INACTIVE: &str =
    "shared-memory mapping inactive; start HWiNFO Sensors and enable Shared Memory Support";
const HWINFO_STALE: &str = "shared memory stale; HWiNFO free-edition shared-memory monitoring may stop after 12 hours, so restart monitoring or use a licensed edition";
const HWINFO_UNAVAILABLE: &str =
    "shared-memory sample unavailable; verify HWiNFO Sensors and Shared Memory Support";
const HWINFO_SELECTION_UNAVAILABLE: &str =
    "configured exact label pair unavailable or ambiguous; verify both original labels";
const LHM_ENDPOINT_UNAVAILABLE: &str =
    "endpoint unavailable; start LibreHardwareMonitor and enable its loopback web server";
const LHM_DATA_UNAVAILABLE: &str =
    "endpoint returned unusable sensor data; verify LibreHardwareMonitor and its web server";
const LHM_SELECTION_UNAVAILABLE: &str =
    "configured sensor selection unavailable; verify the configured sensor IDs";

#[derive(Clone, Debug, PartialEq, Eq)]
enum HwinfoDiagnostic {
    Inactive,
    Stale,
    Unavailable,
    Selection {
        selectors: Box<hwinfo::Selectors>,
        issues: Vec<hwinfo::SelectorIssue>,
    },
}

impl HwinfoDiagnostic {
    fn message(&self) -> &'static str {
        match self {
            Self::Inactive => HWINFO_INACTIVE,
            Self::Stale => HWINFO_STALE,
            Self::Unavailable => HWINFO_UNAVAILABLE,
            Self::Selection { .. } => HWINFO_SELECTION_UNAVAILABLE,
        }
    }
}

fn hwinfo_diagnostic(error: &str) -> HwinfoDiagnostic {
    if error.contains("stale") {
        HwinfoDiagnostic::Stale
    } else if error.contains("inactive") {
        HwinfoDiagnostic::Inactive
    } else {
        HwinfoDiagnostic::Unavailable
    }
}

fn set_hwinfo_diagnostic(
    current: &mut Option<HwinfoDiagnostic>,
    diagnostic: Option<HwinfoDiagnostic>,
) -> bool {
    if *current == diagnostic {
        return false;
    }
    match &diagnostic {
        Some(HwinfoDiagnostic::Selection { issues, .. }) => crate::log_warn!(
            "HWiNFO unavailable: {}; issues={issues:?}",
            HWINFO_SELECTION_UNAVAILABLE
        ),
        Some(diagnostic) => crate::log_warn!("HWiNFO unavailable: {}", diagnostic.message()),
        None if current.is_some() => crate::log_info!("HWiNFO recovered"),
        None => {}
    }
    *current = diagnostic;
    true
}

fn lhm_diagnostic(error: &str) -> &'static str {
    if error.contains("JSON") || error.contains("UTF-8") || error.contains("returned no sensors") {
        LHM_DATA_UNAVAILABLE
    } else {
        LHM_ENDPOINT_UNAVAILABLE
    }
}

fn set_provider_diagnostic(
    current: &mut Option<&'static str>,
    provider: &str,
    diagnostic: Option<&'static str>,
) {
    if *current == diagnostic {
        return;
    }
    match diagnostic {
        Some(message) => crate::log_warn!("{provider} unavailable: {message}"),
        None if current.is_some() => crate::log_info!("{provider} recovered"),
        None => {}
    }
    *current = diagnostic;
}

impl Worker {
    fn tick(&mut self) -> bool {
        let (generation, cfg) = cached_request(
            &self.config,
            &self.config_generation,
            &mut self.request_cache,
        );
        let now = Instant::now();

        if generation != self.generation {
            self.reset_state(generation);
        }

        let mut applied = false;
        while let Ok(event) = self.slow_rx.try_recv() {
            self.apply_slow_event(event, generation, now);
            applied = true;
        }
        while let Ok(update) = self.presentmon_rx.try_recv() {
            self.apply_presentmon_update(update, generation, now);
            applied = true;
        }
        while let Ok(update) = self.hang_rx.try_recv() {
            self.apply_hang_update(update, generation);
            applied = true;
        }
        while let Ok(update) = self.network_rx.try_recv() {
            self.apply_network_update(update, generation, &cfg, now);
            applied = true;
        }
        self.publish_dirty |= applied;

        if cfg.safe_mode {
            self.hwinfo_diagnostic = None;
            if !self.safe_mode_active {
                self.reset_state(generation);
                self.safe_mode_active = true;
                self.publish_dirty = true;
            }
            self.update = Update {
                generation,
                ..Default::default()
            };
            return self.publish(generation, &cfg, now);
        }
        self.safe_mode_active = false;

        let consumers = self.consumers.load(Ordering::Relaxed);
        self.poll_controller(&cfg, consumers, now);
        self.poll_audio(&cfg, consumers, now);
        self.poll_network(&cfg, consumers, now);
        self.poll_performance(&cfg, consumers, now);
        self.poll_connections(&cfg, consumers, now);
        self.poll_battery(&cfg, consumers, now);
        self.apply_freshness(&cfg, now);
        if self.publish_dirty || self.publication_due(&cfg, now) {
            self.rebuild_extended_readings(&cfg);
        }
        self.update.generation = generation;
        self.publish(generation, &cfg, now)
    }

    /// WO-OPT02B (finding O1, slice 2): the coordinator's park bound - the
    /// earliest deadline it owns, made ONLY of the shipped gates: the publish
    /// floor at `telemetry_interval` (the app's own consumption cadence, the
    /// change-or-due gate of `publish`) and every coordinator poll arm at its own
    /// `sampling_interval` / `backoff_interval` result, counted only when that arm
    /// is enabled and demanded (a gated arm with no consumer performs no poll, so
    /// it sets no deadline). A cleared attempt stamp is the shipped "due now", so
    /// the bound collapses to the floor. No new cadence and no interval-math
    /// change; the `unpark` edges (`set_consumers`, `update_config`,
    /// `Drop`/`wake_workers`) interrupt the park, so a demand-word, generation or
    /// shutdown edge lands at once.
    ///
    /// WO-OPT02B-PARKFIX (F-AUD-OPT02B-1, -2): the delivered bound is
    /// `min(PUBLISH_WAKE_CAP-capped publish floor, arm gates, arm TTL deadlines)`,
    /// floored at 2 ms. The publish floor is capped because it is the app's
    /// consumption cadence and never a wake budget: uncapped, a legal 10 s
    /// `telemetry_interval` parked the coordinator for 10 s, so a fresh reading
    /// stayed unread that long and the displayed age doubled. The three coordinator
    /// poll arms now also contribute `last_success + stale_after`, read from the
    /// SAME stamps and bounds `apply_freshness` compares (performance
    /// `EXTENDED_STALE`, connections `EXTENDED_STALE`, system battery
    /// `BATTERY_STALE`), so a Current->Stale flip lands AT its deadline instead of
    /// up to one arm interval late. Both additions are WAKE bounds only: the publish
    /// gate, the intervals, the TTLs, the staleness math and the EC-3 gates are
    /// untouched, and the provider-loop arms (headset, lhm, hwinfo, gpu) stay out of
    /// the set because their own cadence gates already pace their loops.
    fn park_bound(&self, cfg: &Config, consumers: u64, now: Instant) -> Duration {
        if self.publish_dirty {
            return LOOP_MIN_PARK;
        }
        let mut next: Option<Instant> = None;
        let mut consider = |at: Option<Instant>, interval: Duration| {
            let deadline = match at {
                Some(last) => last.checked_add(interval),
                None => Some(now),
            };
            if let Some(deadline) = deadline {
                next = Some(next.map_or(deadline, |current| current.min(deadline)));
            }
        };
        if let Some(last) = self.last_publish {
            // WO-OPT02B-PARKFIX (F-AUD-OPT02B-1): the term is the WAKE cap of the
            // publish floor, not the floor itself - `publication_due` keeps the full
            // `telemetry_interval`, so no publish cadence moved.
            consider(Some(last), cfg.telemetry_interval.min(PUBLISH_WAKE_CAP));
        }
        if enabled(cfg, "controller") && demanded(consumers, "controller") {
            consider(self.controller_attempt, cfg.controller_poll);
        }
        if enabled(cfg, "audio") && demanded(consumers, "audio") {
            consider(self.audio_attempt, cfg.audio_poll);
        }
        if enabled(cfg, "net") && demanded(consumers, "net") {
            consider(self.net_attempt, NETWORK_POLL);
        }
        if demanded(consumers, "performance") {
            let full = backoff_interval(EXTENDED_POLL, EXTENDED_STALE, self.performance_failures);
            consider(
                self.performance_attempt,
                sampling_interval(cfg, consumers, "performance", full),
            );
        }
        if demanded(consumers, "connections") {
            consider(
                self.connections_attempt,
                sampling_interval(cfg, consumers, "connections", EXTENDED_POLL),
            );
        }
        if demanded(consumers, "system battery") {
            consider(
                self.battery_attempt,
                sampling_interval(cfg, consumers, "system battery", BATTERY_POLL),
            );
        }
        // WO-OPT02B-PARKFIX (F-AUD-OPT02B-2): the same arms' staleness deadlines,
        // gated by the same demand predicate as their poll due (a row nobody
        // displays sets no wake), from the fields and bounds `apply_freshness`
        // already reads: performance `:1432`, connections `:1436`, system battery
        // `:1440`. No TTL value and no staleness rule changed.
        if demanded(consumers, "performance") {
            next = park_deadline_min(next, self.performance_success, EXTENDED_STALE);
        }
        if demanded(consumers, "connections") {
            next = park_deadline_min(next, self.connections_success, EXTENDED_STALE);
        }
        if demanded(consumers, "system battery") {
            next = park_deadline_min(next, self.battery_success, BATTERY_STALE);
        }
        next.map_or(LOOP_MIN_PARK, |deadline| {
            deadline.saturating_duration_since(now)
        })
        .max(LOOP_MIN_PARK)
    }

    /// WO-TEL-D2 (S5, U-8): publish on change-or-due. A data change (applied
    /// event, poll attempt, generation reset, safe-mode entry) publishes on the
    /// tick that made it; staleness and cadence riding publish at the
    /// `telemetry_interval` boundary — the app's own consumption cadence
    /// (`app.rs` snapshot gate), so the app drains a value no staler than the
    /// pre-20-Hz stream ever showed it. The capacity-1 coalescing channel and
    /// the generation stamp ride unchanged. A send that fails (disconnected)
    /// still consumes the due slot; the coordinator loop then exits on the
    /// `false`, exactly as the shipped unconditional send did.
    fn publish(&mut self, generation: u64, cfg: &Config, now: Instant) -> bool {
        debug_assert_eq!(self.update.generation, generation);
        let due = self.publication_due(cfg, now);
        if !self.publish_dirty && !due {
            return true;
        }
        self.publish_dirty = false;
        self.last_publish = Some(now);
        self.tx.send(self.update.clone())
    }

    fn publication_due(&self, cfg: &Config, now: Instant) -> bool {
        self.last_publish
            .is_none_or(|last| now.saturating_duration_since(last) >= cfg.telemetry_interval)
    }

    fn reset_state(&mut self, generation: u64) {
        self.update = Update {
            generation,
            ..Default::default()
        };
        self.net = netif::NetProvider::new();
        self.performance = performance::Provider::new();
        self.headset_success = None;
        self.lhm_success = None;
        self.gpu_success = None;
        self.performance_success = None;
        self.connections_success = None;
        self.battery_success = None;
        self.hwinfo_success = None;
        self.network_success = None;
        self.presentmon_success = None;
        self.controller_attempt = None;
        self.audio_attempt = None;
        self.net_attempt = None;
        self.performance_attempt = None;
        self.connections_attempt = None;
        self.battery_attempt = None;
        self.performance_failures = 0;
        self.hwinfo_cpu_temp = Metric::default();
        self.lhm_cpu_temp = Metric::default();
        self.native_gpu_temp = Metric::default();
        self.lhm_gpu_temp = Metric::default();
        self.gpu_backend.clear();
        self.native_gpu_power = None;
        self.lhm_readings.clear();
        self.hwinfo_readings.clear();
        self.performance_readings.clear();
        self.connection_reading = None;
        self.lhm_diagnostic = None;
        self.generation = generation;
        self.publish_dirty = true;
        self.last_publish = None;
    }

    fn apply_slow_event(&mut self, event: SlowEvent, generation: u64, now: Instant) {
        match event {
            SlowEvent::Headset(event_generation, result) if event_generation == generation => {
                self.apply_headset(result, now)
            }
            SlowEvent::Lhm(event_generation, result) if event_generation == generation => {
                self.apply_lhm(*result, now)
            }
            SlowEvent::Gpu(event_generation, result) if event_generation == generation => {
                self.apply_gpu(result, now)
            }
            SlowEvent::Hwinfo(event_generation, selectors, result)
                if event_generation == generation =>
            {
                self.apply_hwinfo(selectors, result, now)
            }
            _ => {}
        }
    }

    fn apply_headset(&mut self, result: Result<headset::Status, String>, now: Instant) {
        match result {
            Ok(status) => {
                self.update.headset = HeadsetBattery {
                    present: true,
                    online: status.online,
                    stale: false,
                    charging: status.charging,
                    percent: status.percent,
                    raw_level: status.raw_level,
                };
                self.headset_success = Some(now);
                self.set_provider("headset", true);
            }
            Err(e) => {
                crate::log_debug!("headset query failed: {}", e);
                self.set_provider("headset", false);
            }
        }
    }

    fn apply_presentmon_update(
        &mut self,
        update: presentmon::Update,
        generation: u64,
        now: Instant,
    ) {
        if update.generation != generation {
            return;
        }
        self.update.game = update.game;
        self.set_provider("presentmon", update.available);
        // WO-U46I (#3b): the success stamp advances ONLY on an accepted, current-generation
        // publication that reports the provider AVAILABLE — the shipped rule of
        // `apply_network_update` (`if update.available { self.network_success = Some(now) }`) and
        // of `apply_gpu`'s Ok arm. A stale generation returned above; an `available: false`
        // publication (park, no target, disabled, expired capture) is not a success, so it can
        // never keep a silent worker's last FPS looking fresh.
        if update.available {
            self.presentmon_success = Some(now);
        }
        if !update.available && !update.detail.is_empty() {
            if update.error {
                crate::log_warn!("PresentMon unavailable: {}", update.detail);
            } else {
                crate::log_debug!("PresentMon unavailable: {}", update.detail);
            }
        }
    }

    fn apply_hang_update(&mut self, update: hang::Update, generation: u64) {
        if update.generation != generation {
            return;
        }
        // WO-QA-03 (TC-4 F-b, display-only): carry the named loss cause on the health
        // record the panel already reads. The cause is read here, before `targets` moves
        // into the record. Clear-then-carry per publication mirrors the provider-side
        // per-cycle reset, so the count cannot leak across publications. The entry is
        // available = true because the cycle SUCCEEDED (`Ok(cycle)` is the only publisher
        // of a non-zero count), so the token is a CAUSE label: never a DOWN, never a
        // target.
        let reason = update.availability_reason();
        self.update.hung = update.targets;
        self.set_provider("Hung window detector", update.available);
        self.remove_provider_prefix(PROBE_INDETERMINATE);
        if let Some(reason) = reason {
            self.set_provider(&reason, true);
        }
        if let Some(error) = update.error {
            crate::log_debug!("hung-window detector unavailable: {}", error);
        }
    }

    fn apply_network_update(
        &mut self,
        update: network::Update,
        generation: u64,
        cfg: &Config,
        now: Instant,
    ) {
        if update.generation != generation {
            return;
        }
        self.update.ping_ms = update.ping;
        self.update.jitter_ms = update.jitter;
        self.update.packet_loss = update.loss;
        if cfg.network_probe_enabled && !cfg.safe_mode {
            if update.available {
                self.network_success = Some(now);
            }
            self.set_provider("network probe", update.available);
            if let Some(error) = update.error {
                crate::log_debug!("network probe unavailable: {}", error);
            }
        } else {
            self.network_success = None;
            self.remove_provider("network probe");
        }
    }

    fn apply_lhm(
        &mut self,
        result: Result<(lhm::ExtendedSample, SystemTime), String>,
        now: Instant,
    ) {
        match result {
            Ok((selection, sampled_at)) => {
                self.lhm_cpu_temp = temperature(&selection.legacy.cpu_temp, sampled_at);
                self.lhm_gpu_temp = temperature(&selection.legacy.gpu_temp, sampled_at);
                self.lhm_readings = lhm_readings(&selection, sampled_at);
                let selection_failed =
                    !selection.legacy.errors.is_empty() || !selection.errors.is_empty();
                set_provider_diagnostic(
                    &mut self.lhm_diagnostic,
                    "LibreHardwareMonitor",
                    selection_failed.then_some(LHM_SELECTION_UNAVAILABLE),
                );
                self.lhm_success = Some(now);
                self.set_provider("lhm", true);
            }
            Err(error) => {
                set_provider_diagnostic(
                    &mut self.lhm_diagnostic,
                    "LibreHardwareMonitor",
                    Some(lhm_diagnostic(&error)),
                );
                self.set_provider("lhm", false);
            }
        }
    }

    /// WO-TEL-D3-B1 (L2, core tier): publish the explicit core-unavailable row set ONCE per park
    /// transition. GPU utilization and VRAM are CORE readings, so a parked/disabled/failing gpu
    /// capability states that in the reading rows instead of leaving silence — and never as a
    /// per-tick stream. The published row set is its own dedupe, i.e. the shipped publish-once
    /// rule of `providers/network.rs:241-253` (flag 249, reset 253) and `providers/hang.rs:898-915`
    /// (flag 911, reset 915) with the flag state derived from the content it publishes, so no new
    /// shared state exists: a repeat parked tick rewrites nothing, sets no dirty flag, and the
    /// change-or-due publish gate (`Self::publish`) sees no change.
    fn publish_core_gpu_unavailable(&mut self) {
        let already_published = self.update.gpu_readings.len() == 4
            && self
                .update
                .gpu_readings
                .iter()
                .all(|reading| !reading.has_value);
        if already_published {
            return;
        }
        self.update.gpu_readings = core_gpu_unavailable_readings();
        self.publish_dirty = true;
    }

    fn apply_gpu(&mut self, result: Result<gpu::Sample, String>, now: Instant) {
        match result {
            Ok(sample) => {
                if self.gpu_backend != sample.backend {
                    crate::log_info!("native GPU provider selected: {}", sample.backend);
                    self.gpu_backend = sample.backend;
                }
                self.update.gpu_readings = sample.readings;
                self.native_gpu_temp = sample.temperature;
                self.native_gpu_power = sample.power_w.map(|power| {
                    Reading::current_number(
                        MetricKey::GPUPower,
                        ValueKind::Watts,
                        power,
                        "native-gpu",
                        sample.sampled_at,
                    )
                });
                self.gpu_success = Some(now);
                self.set_provider("gpu", true);
            }
            Err(e) => {
                crate::log_debug!("native GPU sample failed: {}", e);
                // WO-TEL-D3-B1 (L2): a failing/absent/disabled gpu capability states the core
                // tier explicitly (bounded, one publication, never silence, never stale-as-fresh).
                self.publish_core_gpu_unavailable();
                self.set_provider("gpu", false);
            }
        }
    }

    fn apply_hwinfo(
        &mut self,
        selectors: hwinfo::Selectors,
        result: Result<hwinfo::Sample, String>,
        now: Instant,
    ) {
        match result {
            Ok(sample) => {
                self.hwinfo_cpu_temp = sample
                    .cpu_temp_c
                    .map(|value| Metric::valid(value, sample.sampled_at))
                    .unwrap_or_default();
                self.hwinfo_readings = power_readings(
                    sample.total_power_w,
                    sample.cpu_package_power_w,
                    sample.gpu_board_power_w,
                    "hwinfo",
                    sample.sampled_at,
                );
                let diagnostic =
                    (!sample.issues.is_empty()).then_some(HwinfoDiagnostic::Selection {
                        selectors: Box::new(selectors),
                        issues: sample.issues,
                    });
                set_hwinfo_diagnostic(&mut self.hwinfo_diagnostic, diagnostic);
                self.hwinfo_success = Some(now);
                self.set_provider("hwinfo", true);
            }
            Err(error) => {
                set_hwinfo_diagnostic(&mut self.hwinfo_diagnostic, Some(hwinfo_diagnostic(&error)));
                self.set_provider("hwinfo", false);
            }
        }
    }

    fn poll_controller(&mut self, cfg: &Config, consumers: u64, now: Instant) {
        if !enabled(cfg, "controller") {
            self.controller_attempt = None;
            self.update.controller = ControllerBattery::default();
            self.remove_provider("controller");
            return;
        }
        // WO-TEL-D2 (S2, U-7): no displayed CONTROLLER_BATTERY consumer => the
        // poll does not run; the shipped disabled arm above stays byte-identical.
        if !demanded(consumers, "controller") {
            self.publish_dirty |= self.controller_attempt.take().is_some();
            self.update.controller = ControllerBattery::default();
            self.remove_provider("controller");
            return;
        }
        if !due(self.controller_attempt, cfg.controller_poll, now) {
            return;
        }
        self.controller_attempt = Some(now);
        self.publish_dirty = true;
        match xinput::query(cfg.controller_index) {
            Ok(Some((index, battery))) => {
                self.update.controller = ControllerBattery {
                    connected: true,
                    index: index as i32,
                    type_name: if battery.wired {
                        "WIRED".into()
                    } else {
                        "XINPUT".into()
                    },
                    percent: battery.percent,
                };
                self.set_provider("controller", true);
            }
            Ok(None) => {
                self.update.controller = ControllerBattery::default();
                self.set_provider("controller", true);
            }
            Err(e) => {
                crate::log_debug!("controller query failed: {}", e);
                self.update.controller = ControllerBattery::default();
                self.set_provider("controller", false);
            }
        }
    }

    fn poll_audio(&mut self, cfg: &Config, consumers: u64, now: Instant) {
        if !enabled(cfg, "audio") {
            self.audio_attempt = None;
            self.update.audio = None;
            self.remove_provider("audio");
            return;
        }
        // WO-TEL-D2 (S2, U-2): no displayed AUDIO/MIC_STATUS consumer => the COM
        // endpoint read does not run; the shipped disabled arm stays byte-identical.
        if !demanded(consumers, "audio") {
            self.publish_dirty |= self.audio_attempt.take().is_some();
            self.update.audio = None;
            self.update.audio_sampled_at = None;
            self.remove_provider("audio");
            return;
        }
        if !due(self.audio_attempt, cfg.audio_poll, now) {
            return;
        }
        self.audio_attempt = Some(now);
        self.publish_dirty = true;
        match audio::read() {
            Ok(state) => {
                self.update.audio = Some(state);
                self.update.audio_sampled_at = Some(SystemTime::now());
                self.set_provider("audio", true);
            }
            Err(e) => {
                crate::log_debug!("audio query failed: {}", e);
                self.update.audio = None;
                self.update.audio_sampled_at = None;
                self.set_provider("audio", false);
            }
        }
    }

    fn poll_network(&mut self, cfg: &Config, consumers: u64, now: Instant) {
        if !enabled(cfg, "net") {
            self.net_attempt = None;
            self.update.net = None;
            self.remove_provider("net");
            return;
        }
        // WO-TEL-D2 (S2, U-11): the 1 Hz netif poll stops when no NET_* module
        // and no NET bar is displayed (the persistence the demand release leaves
        // today). Discard its counters on release: the first resumed poll captures
        // a fresh baseline, not an average across an unobserved interval.
        if !demanded(consumers, "net") {
            self.publish_dirty |= self.net_attempt.take().is_some() || self.update.net.is_some();
            self.net.reset();
            self.update.net = None;
            self.remove_provider("net");
            return;
        }
        if !due(self.net_attempt, NETWORK_POLL, now) {
            return;
        }
        self.net_attempt = Some(now);
        self.publish_dirty = true;
        match self.net.update() {
            Ok(sample) => {
                self.update.net = sample;
                self.set_provider("net", true);
            }
            Err(e) => {
                crate::log_debug!("network sample failed: {}", e);
                self.update.net = None;
                self.set_provider("net", false);
            }
        }
    }

    fn poll_performance(&mut self, cfg: &Config, consumers: u64, now: Instant) {
        // WO-TEL-D2 (S2/S4, U-5..U-8 family): zero-demand => no sampling; the
        // EC-3 keep-alive math below stays the shipped function. U-12: a PDH
        // sample failure makes `providers/performance.rs` drop the query, so the
        // NEXT poll re-opens `PdhOpenQueryW` + the 3 counters; the consecutive-
        // failure streak grows the interval toward the provider's own
        // staleness bound, bounding that re-open to the keep-alive cadence.
        if !demanded(consumers, "performance") {
            return;
        }
        let full = backoff_interval(EXTENDED_POLL, EXTENDED_STALE, self.performance_failures);
        let interval = sampling_interval(cfg, consumers, "performance", full);
        if !due(self.performance_attempt, interval, now) {
            return;
        }
        self.performance_attempt = Some(now);
        self.publish_dirty = true;
        match self.performance.sample() {
            Ok(Some(sample)) => {
                self.performance_failures = 0;
                self.performance_readings = vec![
                    Reading::current_number(
                        MetricKey::DiskReadBytesPerSec,
                        ValueKind::ByteRate,
                        sample.disk_read_bytes_per_sec,
                        "physical-disk-total",
                        sample.sampled_at,
                    ),
                    Reading::current_number(
                        MetricKey::DiskWriteBytesPerSec,
                        ValueKind::ByteRate,
                        sample.disk_write_bytes_per_sec,
                        "physical-disk-total",
                        sample.sampled_at,
                    ),
                    Reading::current_number(
                        MetricKey::PageReadsPerSec,
                        ValueKind::Count,
                        sample.page_reads_per_sec,
                        "pdh-page-reads-approximation",
                        sample.sampled_at,
                    ),
                ];
                self.performance_success = Some(now);
                self.set_provider("performance", true);
            }
            Ok(None) => {
                self.performance_failures = 0;
                self.set_provider("performance", true);
            }
            Err(error) => {
                self.performance_failures = self.performance_failures.saturating_add(1);
                crate::log_debug!("performance sample failed: {}", error);
                self.set_provider("performance", false);
            }
        }
    }

    fn poll_connections(&mut self, cfg: &Config, consumers: u64, now: Instant) {
        // WO-TEL-D2 (S2, U-5): `CONNECTIONS` in no displayed module => no poll;
        // the TCP-table copy stops entirely at demand 0.
        if !demanded(consumers, "connections") {
            return;
        }
        let interval = sampling_interval(cfg, consumers, "connections", EXTENDED_POLL);
        if !due(self.connections_attempt, interval, now) {
            return;
        }
        self.connections_attempt = Some(now);
        self.publish_dirty = true;
        match connections::sample() {
            Ok(sample) => {
                self.connection_reading = Some(Reading::current_number(
                    MetricKey::EstablishedConnections,
                    ValueKind::Count,
                    f64::from(sample.established),
                    "tcp-established",
                    sample.sampled_at,
                ));
                self.connections_success = Some(now);
                self.set_provider("connections", true);
            }
            Err(error) => {
                crate::log_debug!("connections sample failed: {}", error);
                self.set_provider("connections", false);
            }
        }
    }

    fn poll_battery(&mut self, cfg: &Config, consumers: u64, now: Instant) {
        // WO-TEL-D2 (S2, U-6): `SYSTEM_BATTERY` in no displayed module => no
        // `GetSystemPowerStatus` call; the EC-3 floor below stays the shipped math.
        if !demanded(consumers, "system battery") {
            return;
        }
        let interval = sampling_interval(cfg, consumers, "system battery", BATTERY_POLL);
        if !due(self.battery_attempt, interval, now) {
            return;
        }
        self.battery_attempt = Some(now);
        self.publish_dirty = true;
        match system_power::sample() {
            Ok(sample) => {
                self.update.system_battery = SystemBattery {
                    ac: match sample.ac {
                        system_power::AcState::Offline => AcState::Offline,
                        system_power::AcState::Online => AcState::Online,
                        system_power::AcState::Unknown => AcState::Unknown,
                    },
                    battery_present: sample.battery_present,
                    charging: sample.charging,
                    percent: sample.battery_percent,
                    freshness: Freshness::Current,
                    sampled_at: Some(sample.sampled_at),
                };
                self.battery_success = Some(now);
                self.set_provider("system battery", true);
            }
            Err(error) => {
                crate::log_debug!("system battery sample failed: {}", error);
                self.set_provider("system battery", false);
            }
        }
    }

    fn apply_freshness(&mut self, cfg: &Config, now: Instant) {
        if enabled(cfg, "headset") {
            if self.update.headset.present {
                self.update.headset.stale =
                    stale(self.headset_success, cfg.headset_stale_after, now);
            }
        } else {
            self.update.headset = HeadsetBattery::default();
            self.headset_success = None;
            self.remove_provider("headset");
        }

        if enabled(cfg, "lhm") {
            apply_lhm_freshness(
                &mut self.lhm_cpu_temp,
                &mut self.lhm_gpu_temp,
                self.lhm_success,
                cfg.lhm_stale_after,
                now,
            );
        } else {
            self.lhm_cpu_temp = Metric::default();
            self.lhm_gpu_temp = Metric::default();
            self.lhm_success = None;
            self.lhm_diagnostic = None;
            self.remove_provider("lhm");
        }

        if enabled(cfg, "gpu") {
            let gpu_stale = stale(self.gpu_success, gpu_stale_after(cfg), now);
            for reading in &mut self.update.gpu_readings {
                if gpu_stale {
                    reading.freshness = Freshness::Stale;
                }
            }
            if self.native_gpu_temp.valid && gpu_stale {
                self.native_gpu_temp.stale = true;
            }
            mark_reading_stale(self.native_gpu_power.as_mut(), gpu_stale);
        } else {
            // WO-TEL-D3-B1 (L2): the disabled/parked gpu capability publishes the four CORE
            // rows as explicit unavailable, ONCE (the shipped publish-once rule, deduped by the
            // published row set itself), replacing the shipped `gpu_readings.clear()` = silent
            // absence. The rest of the shipped disabled arm is byte-identical: temperature,
            // power, backend cache, success stamp and the health row removal stay as accepted
            // (GPU temperature/power are NOT core readings - `HANDOFF.md` §2A).
            self.publish_core_gpu_unavailable();
            self.native_gpu_temp = Metric::default();
            self.gpu_success = None;
            self.gpu_backend.clear();
            self.native_gpu_power = None;
            self.remove_provider("gpu");
        }
        mark_readings_stale(
            &mut self.performance_readings,
            stale(self.performance_success, EXTENDED_STALE, now),
        );
        mark_reading_stale(
            self.connection_reading.as_mut(),
            stale(self.connections_success, EXTENDED_STALE, now),
        );
        if self.update.system_battery.sampled_at.is_some() {
            self.update.system_battery.freshness =
                if stale(self.battery_success, BATTERY_STALE, now) {
                    Freshness::Stale
                } else {
                    Freshness::Current
                };
        }
        mark_readings_stale(
            &mut self.lhm_readings,
            stale(self.lhm_success, cfg.lhm_stale_after, now),
        );
        if enabled(cfg, "hwinfo") {
            let hwinfo_stale = stale(self.hwinfo_success, cfg.hwinfo_stale_after, now);
            if self.hwinfo_cpu_temp.valid {
                self.hwinfo_cpu_temp.stale = hwinfo_stale;
            }
            mark_readings_stale(&mut self.hwinfo_readings, hwinfo_stale);
        } else {
            self.hwinfo_cpu_temp = Metric::default();
            self.hwinfo_readings.clear();
            self.hwinfo_success = None;
            self.hwinfo_diagnostic = None;
            self.remove_provider("hwinfo");
        }
        self.update.cpu_temp = prefer_cpu_temperature(self.hwinfo_cpu_temp, self.lhm_cpu_temp);
        self.update.gpu_temp = prefer_native_temperature(self.native_gpu_temp, self.lhm_gpu_temp);
        if enabled(cfg, "presentmon") {
            // WO-U46I (#3b): presentmon joins the freshness contract on its own success stamp,
            // the shipped shape of the arms above. Past the provider's bound the DISPLAYED rows
            // (FPS, 1%-low, 0.1%-low, frame time) read stale; past twice that bound they read
            // unavailable, the 1x/2x doubling the provider's own `stale_projection` uses
            // (`providers/presentmon.rs:2220-2247`), so a silent worker's last number can never
            // keep presenting as live. A recovery publication replaces `update.game` wholesale,
            // which clears the stale marks: fresh again.
            let bound = presentmon_stale_after(cfg);
            let is_stale = stale(self.presentmon_success, bound, now);
            let expired = stale(self.presentmon_success, bound.saturating_mul(2), now);
            for metric in [
                &mut self.update.game.fps,
                &mut self.update.game.one_percent,
                &mut self.update.game.point_one_low,
                &mut self.update.game.frame_time_ms,
            ] {
                if expired {
                    *metric = Metric::invalid();
                } else {
                    metric.stale = metric.valid && (metric.stale || is_stale);
                }
            }
        } else {
            self.update.game = GameStats::default();
            self.presentmon_success = None;
            self.remove_provider("presentmon");
        }
        if enabled(cfg, "network_probe") {
            let stale_after = cfg
                .network_probe_interval
                .saturating_mul(3)
                .max(Duration::from_secs(3));
            let network_stale = stale(self.network_success, stale_after, now);
            for metric in [
                &mut self.update.ping_ms,
                &mut self.update.jitter_ms,
                &mut self.update.packet_loss,
            ] {
                metric.stale = metric.valid && (metric.stale || network_stale);
            }
        } else {
            self.update.ping_ms = Metric::default();
            self.update.jitter_ms = Metric::default();
            self.update.packet_loss = Metric::default();
            self.network_success = None;
            self.remove_provider("network probe");
        }
    }

    fn rebuild_extended_readings(&mut self, cfg: &Config) {
        let mut readings = std::mem::take(&mut self.update.extended_readings);
        readings.clear();
        readings.extend(
            self.lhm_readings
                .iter()
                .filter(|reading| {
                    !matches!(
                        reading.key,
                        MetricKey::TotalPower | MetricKey::CPUPower | MetricKey::GPUPower
                    )
                })
                .cloned(),
        );
        readings.extend(self.performance_readings.iter().cloned());
        readings.extend(self.connection_reading.iter().cloned());
        for key in [MetricKey::TotalPower, MetricKey::CPUPower] {
            if let Some(reading) = preferred_reading(
                current_reading(&self.hwinfo_readings, key),
                current_reading(&self.lhm_readings, key),
                any_reading(&self.hwinfo_readings, key),
                any_reading(&self.lhm_readings, key),
            ) {
                readings.push(reading.clone());
            }
        }
        let current_gpu = choose_gpu_power(
            self.native_gpu_power.as_ref(),
            &self.hwinfo_readings,
            &self.lhm_readings,
        );
        let gpu = current_gpu.or_else(|| {
            self.native_gpu_power
                .as_ref()
                .or_else(|| any_reading(&self.hwinfo_readings, MetricKey::GPUPower))
                .or_else(|| any_reading(&self.lhm_readings, MetricKey::GPUPower))
        });
        if let Some(gpu) = gpu {
            readings.push(gpu.clone());
        }
        let cpu = current_reading(&readings, MetricKey::CPUPower);
        if let (Some(cpu), Some(gpu)) = (cpu, current_gpu) {
            if let Some(combined) = bounded_power_sum(cpu.number, gpu.number) {
                readings.push(Reading::current_number(
                    MetricKey::CPUGPUPower,
                    ValueKind::Watts,
                    combined,
                    "derived-current-cpu-gpu",
                    cpu.sampled_at
                        .max(gpu.sampled_at)
                        .unwrap_or_else(SystemTime::now),
                ));
            }
        }
        derive_fan_percent(
            &mut readings,
            MetricKey::CPUFanControl,
            MetricKey::CPUFanRpm,
            MetricKey::CPUFanPercent,
            cfg.cpu_fan_max_rpm,
        );
        derive_fan_percent(
            &mut readings,
            MetricKey::PumpControl,
            MetricKey::PumpRpm,
            MetricKey::PumpPercent,
            cfg.pump_max_rpm,
        );
        self.update.extended_readings = readings;
    }

    fn set_provider(&mut self, name: &str, ok: bool) {
        if let Some(entry) = self.update.providers.iter_mut().find(|(n, _)| n == name) {
            entry.1 = ok;
        } else {
            self.update.providers.push((name.to_string(), ok));
        }
    }

    fn remove_provider(&mut self, name: &str) {
        self.update.providers.retain(|(n, _)| n != name);
    }

    fn remove_provider_prefix(&mut self, prefix: &str) {
        self.update
            .providers
            .retain(|(n, _)| !n.starts_with(prefix));
    }
}

fn spawn_headset(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
    query: fn(Duration) -> Result<headset::Status, String>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-headset".into())
        .spawn(move || {
            let mut request = (0u64, Arc::new(current_config(&config)));
            let mut last_attempt = None;
            let mut last_generation = 0u64;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = cached_request(&config, &generation, &mut request);
                if generation != last_generation {
                    last_generation = generation;
                    last_attempt = None;
                }
                if !enabled(&cfg, "headset")
                    || !demanded(consumers.load(Ordering::Relaxed), "headset")
                {
                    // WO-TEL-D2 (S2, U-1 family): the zero-demand park reuses the
                    // disabled arm (clear + native park), so a reading
                    // with no displayed consumer performs zero HID queries and
                    // the generation-only clear stays byte-identical.
                    last_attempt = None;
                    std::thread::park();
                    continue;
                } else {
                    let now = Instant::now();
                    let interval = sampling_interval(
                        &cfg,
                        consumers.load(Ordering::Relaxed),
                        "headset",
                        cfg.headset_poll,
                    );
                    if due(last_attempt, interval, now) {
                        last_attempt = Some(now);
                        if !send_slow(
                            &tx,
                            SlowEvent::Headset(generation, query(cfg.headset_query_timeout)),
                        ) {
                            return;
                        }
                    }
                    // WO-OPT02B: the running arm parks to this loop's own next
                    // deadline (the `due` gate's `last_attempt + interval`) and is
                    // interruptible by the shipped `unpark` edges.
                    std::thread::park_timeout(sampling_loop_park(last_attempt, interval, now));
                }
            }
        })
        .expect("headset telemetry thread")
}

// The LHM sampler's fn-pointer type is factored into an alias: four parameters plus a tuple
// result trip clippy's `type_complexity` lint inline, and a suppression is not permitted.
type LhmSampler = fn(
    &str,
    &std::collections::BTreeMap<String, String>,
    &lhm::ExtendedSelectors,
    Duration,
) -> Result<(lhm::ExtendedSample, SystemTime), String>;

fn spawn_lhm(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
    sample: LhmSampler,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-lhm".into())
        .spawn(move || {
            let mut request = (0u64, Arc::new(current_config(&config)));
            let mut last_attempt = None;
            let mut last_generation = 0u64;
            // WO-TEL-D2 (S4, U-1): the shipped `temps_need_refresh` store read
            // here inverted the cadence — an endpoint yielding nothing was
            // polled at the FULL rate (removed mechanism; the audit refutes any
            // production-rate claim from the soak log). The provider's own
            // outcome now drives a backoff: consecutive sampler `Err`s double
            // the interval from the configured cadence up to the existing
            // `lhm_stale_after` bound; a success resets it. Demand (S2): no
            // displayed temperature consumer parks the loop in the
            // shipped disabled arm — zero HTTP.
            let mut failures = 0u32;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = cached_request(&config, &generation, &mut request);
                if generation != last_generation {
                    last_generation = generation;
                    last_attempt = None;
                }
                // WO-TEL-D2 (S4, U-1): the failure streak deliberately survives a
                // generation bump (the cleared stamp above still samples
                // immediately; the next success resets the streak). Keeping the
                // shipped branch shape byte-exact is the structural pin
                // `telemetry_sampling_loops_clear_cadence_only_on_generation_change`.
                if let Some(url) =
                    lhm_url(&cfg).filter(|_| demanded(consumers.load(Ordering::Relaxed), "lhm"))
                {
                    let now = Instant::now();
                    let settled_interval = (cfg.lhm_stale_after / 2).max(cfg.lhm_interval);
                    let base = if cfg.lhm_mode == "on" {
                        cfg.lhm_interval
                    } else {
                        settled_interval
                    };
                    let full = backoff_interval(
                        base,
                        keep_alive_interval(&cfg, "lhm").unwrap_or(base),
                        failures,
                    );
                    let interval =
                        sampling_interval(&cfg, consumers.load(Ordering::Relaxed), "lhm", full);
                    if due(last_attempt, interval, now) {
                        last_attempt = Some(now);
                        let result = sample(
                            url,
                            &cfg.lhm_sensors,
                            &lhm_selectors(&cfg),
                            Duration::from_secs(2),
                        );
                        match &result {
                            Ok(_) => failures = 0,
                            Err(_) => failures = failures.saturating_add(1),
                        }
                        if !send_slow(&tx, SlowEvent::Lhm(generation, Box::new(result))) {
                            return;
                        }
                    }
                    // WO-OPT02B: park to the loop's own next deadline, which is the
                    // `due` gate's `last_attempt + interval` - so the backoff ladder
                    // paces the loop exactly as the shipped tick did, and the
                    // `unpark` edges interrupt it.
                    std::thread::park_timeout(sampling_loop_park(last_attempt, interval, now));
                } else {
                    last_attempt = None;
                    std::thread::park();
                    continue;
                }
            }
        })
        .expect("LHM telemetry thread")
}

fn spawn_hwinfo(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
    sample: fn(&hwinfo::Selectors, Duration) -> Result<hwinfo::Sample, String>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-hwinfo".into())
        .spawn(move || {
            let mut request = (0u64, Arc::new(current_config(&config)));
            let mut last_attempt = None;
            let mut last_generation = 0u64;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = cached_request(&config, &generation, &mut request);
                if generation != last_generation {
                    last_generation = generation;
                    last_attempt = None;
                }
                // WO-TEL-D2 (S2): zero display demand parks the loop in the
                // shipped disabled arm — the shared-memory mapping is never opened.
                if enabled(&cfg, "hwinfo") && demanded(consumers.load(Ordering::Relaxed), "hwinfo")
                {
                    let now = Instant::now();
                    let interval = sampling_interval(
                        &cfg,
                        consumers.load(Ordering::Relaxed),
                        "hwinfo",
                        EXTENDED_POLL,
                    );
                    if due(last_attempt, interval, now) {
                        last_attempt = Some(now);
                        let selectors = hwinfo_selectors(&cfg);
                        let result = sample(&selectors, cfg.hwinfo_stale_after);
                        if !send_slow(&tx, SlowEvent::Hwinfo(generation, selectors, result)) {
                            return;
                        }
                    }
                    // WO-OPT02B: park to the loop's own next deadline; the shipped
                    // `unpark` edges interrupt it.
                    std::thread::park_timeout(sampling_loop_park(last_attempt, interval, now));
                } else {
                    last_attempt = None;
                    std::thread::park();
                    continue;
                }
            }
        })
        .expect("HWiNFO telemetry thread")
}

fn send_slow(tx: &mpsc::SyncSender<SlowEvent>, event: SlowEvent) -> bool {
    !matches!(tx.try_send(event), Err(mpsc::TrySendError::Disconnected(_)))
}

fn mark_reading_stale(reading: Option<&mut Reading>, is_stale: bool) {
    if let Some(reading) = reading {
        reading.freshness = if is_stale {
            Freshness::Stale
        } else {
            Freshness::Current
        };
    }
}

fn mark_readings_stale(readings: &mut [Reading], is_stale: bool) {
    for reading in readings {
        mark_reading_stale(Some(reading), is_stale);
    }
}

fn current_reading(readings: &[Reading], key: MetricKey) -> Option<&Reading> {
    readings.iter().find(|reading| {
        reading.key == key
            && reading.has_value
            && reading.availability == Availability::Available
            && reading.freshness == Freshness::Current
    })
}

fn any_reading(readings: &[Reading], key: MetricKey) -> Option<&Reading> {
    readings.iter().find(|reading| {
        reading.key == key && reading.has_value && reading.availability == Availability::Available
    })
}

fn preferred_reading<'a>(
    primary_current: Option<&'a Reading>,
    fallback_current: Option<&'a Reading>,
    primary_any: Option<&'a Reading>,
    fallback_any: Option<&'a Reading>,
) -> Option<&'a Reading> {
    primary_current
        .or(fallback_current)
        .or(primary_any)
        .or(fallback_any)
}

fn choose_gpu_power<'a>(
    native: Option<&'a Reading>,
    hwinfo: &'a [Reading],
    lhm: &'a [Reading],
) -> Option<&'a Reading> {
    native
        .filter(|reading| reading.freshness == Freshness::Current)
        .or_else(|| current_reading(hwinfo, MetricKey::GPUPower))
        .or_else(|| current_reading(lhm, MetricKey::GPUPower))
}

fn reading(
    sensor: &Option<lhm::Sensor>,
    key: MetricKey,
    kind: ValueKind,
    at: SystemTime,
) -> Option<Reading> {
    sensor
        .as_ref()
        .map(|sensor| Reading::current_number(key, kind, sensor.value, &sensor.id, at))
}

fn lhm_readings(sample: &lhm::ExtendedSample, at: SystemTime) -> Vec<Reading> {
    [
        reading(&sample.vrm_temp, MetricKey::VRMTemp, ValueKind::Celsius, at),
        reading(
            &sample.chipset_temp,
            MetricKey::ChipsetTemp,
            ValueKind::Celsius,
            at,
        ),
        reading(
            &sample.motherboard_temp,
            MetricKey::MotherboardTemp,
            ValueKind::Celsius,
            at,
        ),
        reading(
            &sample.cpu_fan_control,
            MetricKey::CPUFanControl,
            ValueKind::Percent,
            at,
        ),
        reading(
            &sample.cpu_fan_rpm,
            MetricKey::CPUFanRpm,
            ValueKind::Rpm,
            at,
        ),
        reading(
            &sample.pump_control,
            MetricKey::PumpControl,
            ValueKind::Percent,
            at,
        ),
        reading(&sample.pump_rpm, MetricKey::PumpRpm, ValueKind::Rpm, at),
        reading(
            &sample.total_power,
            MetricKey::TotalPower,
            ValueKind::Watts,
            at,
        ),
        reading(
            &sample.cpu_package_power,
            MetricKey::CPUPower,
            ValueKind::Watts,
            at,
        ),
        reading(
            &sample.gpu_board_power,
            MetricKey::GPUPower,
            ValueKind::Watts,
            at,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn power_readings(
    total: Option<f64>,
    cpu: Option<f64>,
    gpu: Option<f64>,
    hardware: &str,
    at: SystemTime,
) -> Vec<Reading> {
    [
        (MetricKey::TotalPower, total),
        (MetricKey::CPUPower, cpu),
        (MetricKey::GPUPower, gpu),
    ]
    .into_iter()
    .filter_map(|(key, value)| {
        value
            .filter(|value| value.is_finite() && (0.0..=100_000.0).contains(value))
            .map(|value| Reading::current_number(key, ValueKind::Watts, value, hardware, at))
    })
    .collect()
}

fn bounded_power_sum(cpu: f64, gpu: f64) -> Option<f64> {
    if ![cpu, gpu]
        .into_iter()
        .all(|value| value.is_finite() && (0.0..=100_000.0).contains(&value))
    {
        return None;
    }
    let sum = cpu + gpu;
    sum.is_finite().then_some(sum)
}

fn derive_fan_percent(
    readings: &mut Vec<Reading>,
    control: MetricKey,
    rpm: MetricKey,
    output: MetricKey,
    max_rpm: u32,
) {
    let source = any_reading(readings, control)
        .map(|reading| (reading.number, reading))
        .or_else(|| {
            (max_rpm > 0)
                .then(|| any_reading(readings, rpm))
                .flatten()
                .map(|reading| (reading.number / f64::from(max_rpm) * 100.0, reading))
        });
    if let Some((value, source)) = source {
        let mut derived = Reading::current_percent(
            output,
            value.clamp(0.0, 100.0),
            "derived-fan-percent",
            source.sampled_at.unwrap_or_else(SystemTime::now),
        );
        derived.freshness = source.freshness;
        readings.push(derived);
    }
}

// Seam adapter for the GPU loop: its sampler is a method on the loop-owned stateful
// `gpu::Provider`, so the fn-pointer seam takes the provider as an argument. Byte-equal body.
fn gpu_sample(provider: &mut gpu::Provider, selector: &str) -> Result<gpu::Sample, String> {
    provider.sample(selector)
}

fn spawn_gpu(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
    sample: fn(&mut gpu::Provider, &str) -> Result<gpu::Sample, String>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-gpu".into())
        .spawn(move || {
            let mut request = (0u64, Arc::new(current_config(&config)));
            let mut provider = gpu::Provider::new("off");
            let mut last_attempt = None;
            let mut last_generation = 0u64;
            // WO-TEL-D2 (S4, U-10): `providers/gpu.rs:69` clears the backend
            // cache on a failed sample, so the next poll re-runs the vendor
            // open chain (`nvapi64.dll`/`amdadlx64.dll`/`nvml.dll` loads). The
            // consecutive-failure streak doubles the demanded interval up to the
            // provider's own staleness bound (`gpu_stale_after`), bounding that
            // re-open to the keep-alive cadence; success resets it. Demand (S2):
            // no GPU bar/temperature consumer parks the loop in the
            // shipped disabled arm, which also unloads the vendor handles.
            let mut failures = 0u32;
            // WO-TEL-D3-B1 (L2): the park publishes its explicit core-unavailable statement
            // ONCE per park transition, using the shipped `disabled_published` shape
            // (`providers/network.rs:210` + `241-253`, `providers/hang.rs:844` + `898-915`).
            let mut parked_published = false;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = cached_request(&config, &generation, &mut request);
                if generation != last_generation {
                    last_generation = generation;
                    last_attempt = None;
                    // WO-U46I (#16): a config apply bumps the generation AND clears the
                    // coordinator's published row set (`reset_state`), so a loop parked across
                    // that apply must be able to restate the explicit core-unavailable rows for
                    // the NEW generation. Without this reset the new generation shows silent
                    // absence for the core readings until demand returns; stale-generation
                    // publications stay rejected by the coordinator's generation guard.
                    parked_published = false;
                }
                // WO-TEL-D2 (S4, U-10): streak survives a bump (same rule as the
                // LHM loop above); the cleared stamp samples immediately.
                if enabled(&cfg, "gpu") && demanded(consumers.load(Ordering::Relaxed), "gpu") {
                    parked_published = false;
                    let now = Instant::now();
                    let full =
                        backoff_interval(cfg.telemetry_interval, gpu_stale_after(&cfg), failures);
                    let interval =
                        sampling_interval(&cfg, consumers.load(Ordering::Relaxed), "gpu", full);
                    if due(last_attempt, interval, now) {
                        last_attempt = Some(now);
                        let result = sample(&mut provider, &cfg.gpu_provider);
                        match &result {
                            Ok(_) => failures = 0,
                            Err(_) => failures = failures.saturating_add(1),
                        }
                        if !send_slow(&tx, SlowEvent::Gpu(generation, result)) {
                            return;
                        }
                    }
                    // WO-OPT02B: park to the loop's own next deadline (the `due`
                    // gate's `last_attempt + interval`, backoff included); the
                    // shipped `unpark` edges interrupt it.
                    std::thread::park_timeout(sampling_loop_park(last_attempt, interval, now));
                } else {
                    last_attempt = None;
                    provider = gpu::Provider::new("off");
                    // WO-TEL-D3-B1 (L2, core tier): GPU utilization + VRAM are core readings, so
                    // parking states it explicitly once (bounded, <= one publish per park
                    // transition, the loop's own 50 ms tick = the shipped park cadence). The
                    // reason is named, never silent; the coordinator turns it into the four
                    // explicit unavailable core rows (`apply_gpu` Err arm + `apply_freshness`
                    // disabled arm), deduped by the published row set.
                    if !parked_published {
                        parked_published = true;
                        let reason = if cfg.safe_mode {
                            "gpu core readings unavailable: safe mode"
                        } else if !enabled(&cfg, "gpu") {
                            "gpu core readings unavailable: native GPU provider disabled"
                        } else {
                            "gpu core readings unavailable: no display consumer"
                        };
                        if !send_slow(&tx, SlowEvent::Gpu(generation, Err(reason.into()))) {
                            return;
                        }
                    }
                    std::thread::park();
                    continue;
                }
            }
        })
        .expect("GPU telemetry thread")
}

/// Start the coordinator and owned provider workers.
pub fn spawn(cfg: &Config, initial_consumers: u64) -> Telemetry {
    let config = Arc::new(RwLock::new(cfg.clone()));
    let generation = Arc::new(AtomicU64::new(1));
    let consumers = Arc::new(AtomicU64::new(initial_consumers));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = latest_channel();
    let (slow_tx, slow_rx) = mpsc::sync_channel(8);
    let mut threads = vec![spawn_headset(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&consumers),
        Arc::clone(&shutdown),
        slow_tx.clone(),
        headset::query,
    )];
    threads.push(spawn_lhm(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&consumers),
        Arc::clone(&shutdown),
        slow_tx.clone(),
        lhm::sample_extended,
    ));
    threads.push(spawn_gpu(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&consumers),
        Arc::clone(&shutdown),
        slow_tx.clone(),
        gpu_sample,
    ));
    threads.push(spawn_hwinfo(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&consumers),
        Arc::clone(&shutdown),
        slow_tx.clone(),
        hwinfo::sample,
    ));
    let (presentmon_rx, presentmon_thread) = presentmon::spawn(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&consumers),
        Arc::clone(&shutdown),
    );
    let (hang_rx, hang_thread) = hang::spawn(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&shutdown),
    );
    let (network_rx, network_thread) = network::spawn(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&shutdown),
    );
    threads.extend([presentmon_thread, hang_thread, network_thread]);

    let worker = Worker {
        config: Arc::clone(&config),
        config_generation: Arc::clone(&generation),
        request_cache: (0u64, Arc::new(cfg.clone())),
        consumers: Arc::clone(&consumers),
        tx,
        slow_rx,
        update: Update::default(),
        net: netif::NetProvider::new(),
        performance: performance::Provider::new(),
        headset_success: None,
        lhm_success: None,
        gpu_success: None,
        performance_success: None,
        connections_success: None,
        battery_success: None,
        hwinfo_success: None,
        network_success: None,
        presentmon_success: None,
        controller_attempt: None,
        audio_attempt: None,
        net_attempt: None,
        performance_attempt: None,
        connections_attempt: None,
        battery_attempt: None,
        presentmon_rx,
        hang_rx,
        network_rx,
        hwinfo_cpu_temp: Metric::default(),
        lhm_cpu_temp: Metric::default(),
        native_gpu_temp: Metric::default(),
        lhm_gpu_temp: Metric::default(),
        gpu_backend: String::new(),
        native_gpu_power: None,
        lhm_readings: Vec::new(),
        hwinfo_readings: Vec::new(),
        performance_readings: Vec::new(),
        connection_reading: None,
        hwinfo_diagnostic: None,
        lhm_diagnostic: None,
        performance_failures: 0,
        generation: 1,
        safe_mode_active: cfg.safe_mode,
        publish_dirty: true,
        last_publish: None,
    };
    let coordinator_shutdown = Arc::clone(&shutdown);
    let coordinator = std::thread::Builder::new()
        .name("telemetry".into())
        .spawn(move || {
            let _com = match audio::initialize_thread() {
                Ok(com) => Some(com),
                Err(e) => {
                    crate::log_debug!("audio initialization failed: {}", e);
                    None
                }
            };
            let mut worker = worker;
            coordinator_run(&mut worker, &coordinator_shutdown);
        })
        .expect("telemetry thread");
    threads.push(coordinator);
    Telemetry {
        updates: rx,
        config,
        generation,
        consumers,
        shutdown,
        threads,
    }
}

/// WO-OPT02B (finding O1, slice 2): the coordinator loop, retired from the fixed
/// 50 ms tick. The shipped order is untouched and load-bearing: every iteration
/// runs `Worker::tick`, which CLEARS the inbound state by draining each of the
/// four publication feeds to EMPTY (`slow_rx`, `presentmon_rx`, `hang_rx`,
/// `network_rx` in that order, each a full `while let Ok(..) = try_recv()`
/// sweep) before it polls, marks freshness and publishes; a publication that
/// lands after a sweep is read on the next iteration, which is why the sweep
/// order and the full drain are pinned. The tick is followed by a park to
/// `Worker::park_bound` - the earliest deadline the coordinator owns - and
/// `Thread::unpark` interrupts that park, so the demand-word, generation and
/// shutdown edges (`set_consumers`, `update_config`, `Drop`/`wake_workers`) land
/// at once, which a `sleep` made impossible. Publication feeds the runtime cannot
/// be woken for (the presentmon / hang / network senders keep their frozen
/// signatures) are bounded by the publish floor - `telemetry_interval` capped at
/// `PUBLISH_WAKE_CAP`, so a long interval binds on the app's own consumption.
fn coordinator_run(worker: &mut Worker, shutdown: &AtomicBool) -> bool {
    while !shutdown.load(Ordering::Relaxed) {
        if !worker.tick() {
            return false;
        }
        let cfg = current_config(&worker.config);
        let consumers = worker.consumers.load(Ordering::Relaxed);
        std::thread::park_timeout(worker.park_bound(&cfg, consumers, Instant::now()));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::OnceLock;

    #[test]
    fn auto_lhm_url_resolves_and_off_disables() {
        let mut cfg = Config::default();
        assert_eq!(lhm_url(&cfg), Some(DEFAULT_LHM_URL));
        cfg.lhm_mode = "off".into();
        assert_eq!(lhm_url(&cfg), None);
    }

    #[test]
    fn attempts_not_successes_control_cadence() {
        let now = Instant::now();
        let failed_attempt = Some(now);
        assert!(!due(
            failed_attempt,
            Duration::from_secs(10),
            now + Duration::from_secs(9)
        ));
        assert!(due(
            failed_attempt,
            Duration::from_secs(10),
            now + Duration::from_secs(10)
        ));
    }

    #[test]
    fn staleness_and_enablement_are_deterministic() {
        let now = Instant::now();
        assert!(!stale(
            Some(now),
            Duration::from_secs(3),
            now + Duration::from_secs(3)
        ));
        assert!(stale(
            Some(now),
            Duration::from_secs(3),
            now + Duration::from_secs(4)
        ));

        let cfg = Config {
            controller_enabled: false,
            ..Config::default()
        };
        assert!(!enabled(&cfg, "controller"));
        let cfg = Config {
            safe_mode: true,
            ..Config::default()
        };
        assert!(!enabled(&cfg, "headset"));
        assert!(!enabled(&cfg, "lhm"));
        assert!(!enabled(&cfg, "audio"));
        assert!(!enabled(&cfg, "net"));
    }

    #[test]
    fn lhm_values_transition_from_current_to_stale() {
        let now = Instant::now();
        let sampled_at = SystemTime::now();
        let mut lhm_cpu_temp = Metric::valid(67.0, sampled_at);
        let mut lhm_gpu_temp = Metric::valid(74.0, sampled_at);
        apply_lhm_freshness(
            &mut lhm_cpu_temp,
            &mut lhm_gpu_temp,
            Some(now),
            Duration::from_secs(3),
            now + Duration::from_secs(2),
        );
        assert!(!lhm_cpu_temp.stale);
        apply_lhm_freshness(
            &mut lhm_cpu_temp,
            &mut lhm_gpu_temp,
            Some(now),
            Duration::from_secs(3),
            now + Duration::from_secs(4),
        );
        assert!(lhm_cpu_temp.stale && lhm_gpu_temp.stale);
        assert!(!Update::default().cpu_temp.valid);
    }

    #[test]
    fn native_gpu_temperature_wins_until_stale() {
        let at = SystemTime::UNIX_EPOCH;
        let fallback = Metric::valid(60.0, at);
        let mut native = Metric::valid(70.0, at);
        assert_eq!(prefer_native_temperature(native, fallback).value, 70.0);
        native.stale = true;
        assert_eq!(prefer_native_temperature(native, fallback).value, 60.0);
        let unavailable = Metric::default();
        let retained = prefer_native_temperature(native, unavailable);
        assert_eq!(retained.value, 70.0);
        assert!(retained.stale);
        assert_eq!(retained.updated, Some(at));
        let mut stale_fallback = fallback;
        stale_fallback.stale = true;
        assert_eq!(
            prefer_native_temperature(native, stale_fallback).value,
            70.0
        );
        assert_eq!(
            prefer_native_temperature(Metric::default(), fallback).value,
            60.0
        );
    }

    #[test]
    fn config_update_replaces_shared_runtime_config() {
        let shared = Arc::new(RwLock::new(Config::default()));
        let generation = Arc::new(AtomicU64::new(1));
        let (_tx, updates) = latest_channel();
        let telemetry = Telemetry {
            updates,
            config: Arc::clone(&shared),
            generation: Arc::clone(&generation),
            consumers: Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            shutdown: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        let changed = Config {
            audio_enabled: false,
            controller_enabled: false,
            ..Config::default()
        };
        assert_eq!(telemetry.update_config(&changed), 2);
        let current = current_config(&shared);
        assert!(!current.audio_enabled);
        assert!(!current.controller_enabled);
        assert_eq!(generation.load(Ordering::SeqCst), 2);
    }

    fn test_worker(cfg: Config) -> Worker {
        let config = Arc::new(RwLock::new(cfg.clone()));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _updates) = latest_channel();
        let (_slow_tx, slow_rx) = mpsc::sync_channel(1);
        let (_presentmon_tx, presentmon_rx) = mpsc::channel();
        let (_hang_tx, hang_rx) = mpsc::channel();
        let (_network_tx, network_rx) = mpsc::channel();
        Worker {
            config,
            config_generation: generation,
            request_cache: (0u64, Arc::new(cfg)),
            consumers: Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            tx,
            slow_rx,
            update: Update {
                generation: 1,
                ..Default::default()
            },
            net: netif::NetProvider::new(),
            performance: performance::Provider::new(),
            headset_success: None,
            lhm_success: None,
            gpu_success: None,
            performance_success: None,
            connections_success: None,
            battery_success: None,
            hwinfo_success: None,
            network_success: None,
            presentmon_success: None,
            controller_attempt: None,
            audio_attempt: None,
            net_attempt: None,
            performance_attempt: None,
            connections_attempt: None,
            battery_attempt: None,
            performance_failures: 0,
            presentmon_rx,
            hang_rx,
            network_rx,
            hwinfo_cpu_temp: Metric::default(),
            lhm_cpu_temp: Metric::default(),
            native_gpu_temp: Metric::default(),
            lhm_gpu_temp: Metric::default(),
            gpu_backend: String::new(),
            native_gpu_power: None,
            lhm_readings: Vec::new(),
            hwinfo_readings: Vec::new(),
            performance_readings: Vec::new(),
            connection_reading: None,
            hwinfo_diagnostic: None,
            lhm_diagnostic: None,
            generation: 1,
            safe_mode_active: false,
            publish_dirty: true,
            last_publish: None,
        }
    }

    #[test]
    fn delayed_old_generation_sensor_results_are_discarded() {
        let mut worker = test_worker(Config::default());
        worker.native_gpu_temp = Metric::valid(55.0, SystemTime::UNIX_EPOCH);
        worker.lhm_readings.push(Reading::current_celsius(
            MetricKey::VRMTemp,
            60.0,
            "old-selector",
            SystemTime::UNIX_EPOCH,
        ));
        worker.reset_state(2);
        assert!(!worker.native_gpu_temp.valid);
        assert!(worker.lhm_readings.is_empty());

        let sample = gpu::Sample {
            backend: "old-backend".into(),
            readings: Vec::new(),
            temperature: Metric::valid(99.0, SystemTime::UNIX_EPOCH),
            power_w: None,
            sampled_at: SystemTime::UNIX_EPOCH,
        };
        worker.apply_slow_event(SlowEvent::Gpu(1, Ok(sample)), 2, Instant::now());
        worker.apply_slow_event(
            SlowEvent::Headset(
                1,
                Ok(headset::Status {
                    online: true,
                    percent: 100,
                    ..Default::default()
                }),
            ),
            2,
            Instant::now(),
        );
        worker.apply_slow_event(
            SlowEvent::Lhm(
                1,
                Box::new(Ok((lhm::ExtendedSample::default(), SystemTime::UNIX_EPOCH))),
            ),
            2,
            Instant::now(),
        );
        worker.apply_slow_event(
            SlowEvent::Hwinfo(
                1,
                hwinfo::Selectors::default(),
                Ok(hwinfo::Sample {
                    cpu_temp_c: Some(99.0),
                    total_power_w: Some(999.0),
                    cpu_package_power_w: Some(999.0),
                    gpu_board_power_w: Some(999.0),
                    issues: Vec::new(),
                    sampled_at: SystemTime::UNIX_EPOCH,
                }),
            ),
            2,
            Instant::now(),
        );
        assert!(!worker.native_gpu_temp.valid);
        assert!(!worker.update.headset.present);
        assert!(worker.update.gpu_readings.is_empty());
        assert!(worker.lhm_readings.is_empty());
        assert!(worker.hwinfo_readings.is_empty());
        assert!(!worker.hwinfo_cpu_temp.valid);
    }

    #[test]
    fn delayed_provider_updates_keep_their_generation_and_are_discarded() {
        let mut cfg = Config {
            network_probe_enabled: true,
            ..Config::default()
        };
        let mut worker = test_worker(cfg.clone());
        worker.reset_state(2);
        let target = HungTarget {
            hwnd: 1,
            pid: 2,
            creation_time: 3,
            image_path: "old.exe".into(),
            process_name: "old.exe".into(),
            title: "old".into(),
            ..Default::default()
        };
        worker.apply_presentmon_update(
            presentmon::Update {
                generation: 1,
                available: true,
                game: GameStats {
                    active: true,
                    process_name: "old.exe".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            2,
            Instant::now(),
        );
        worker.apply_hang_update(
            hang::Update {
                generation: 1,
                available: true,
                targets: vec![target.clone()],
                error: None,
                ..Default::default()
            },
            2,
        );
        worker.apply_network_update(
            network::Update {
                generation: 1,
                available: true,
                ping: Metric::valid(1.0, SystemTime::UNIX_EPOCH),
                ..Default::default()
            },
            2,
            &cfg,
            Instant::now(),
        );
        assert!(!worker.update.game.active);
        assert!(worker.update.hung.is_empty());
        assert!(!worker.update.ping_ms.valid);

        worker.apply_hang_update(
            hang::Update {
                generation: 2,
                available: true,
                targets: vec![target],
                error: None,
                ..Default::default()
            },
            2,
        );
        assert_eq!(worker.update.hung.len(), 1);
        cfg.safe_mode = true;
        worker.reset_state(3);
        assert!(
            worker.update.hung.is_empty(),
            "reload clears retained hang state"
        );
    }

    /// WO-QA-03 (TC-4 follow-up F-b, display-only): the named loss cause reaches the
    /// provider-health record the panel reads. Acceptance (1) record layer:
    /// `Update { probe_indeterminate: 3, targets: [], available: true }` puts
    /// `probe-indeterminate-3` on the record, and the record says it with ZERO targets
    /// and with the entry `available = true`, so the operator sees the named cause, never
    /// a phantom target and never a phantom DOWN.
    #[test]
    fn the_named_loss_cause_lands_on_the_provider_health_record() {
        let mut worker = test_worker(Config::default());
        worker.reset_state(1);
        worker.apply_hang_update(
            hang::Update {
                generation: 1,
                targets: Vec::new(),
                available: true,
                error: None,
                probe_indeterminate: 3,
            },
            1,
        );
        assert!(
            worker.update.hung.is_empty(),
            "the loss cause publishes no target"
        );
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .find(|(name, _)| name == "Hung window detector")
                .map(|(_, available)| *available),
            Some(true),
            "the provider itself stays available: the cycle succeeded"
        );
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .find(|(name, _)| name.starts_with(PROBE_INDETERMINATE))
                .map(|(name, available)| (name.clone(), *available)),
            Some(("probe-indeterminate-3".to_string(), true)),
            "the token reaches the record the panel reads, as an available entry"
        );
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .filter(|(_, available)| !available)
                .count(),
            0,
            "the loss cause is never an unavailable-provider count"
        );

        worker.apply_hang_update(
            hang::Update {
                generation: 2,
                targets: Vec::new(),
                available: true,
                error: None,
                probe_indeterminate: 0,
            },
            2,
        );
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .filter(|(name, _)| name.starts_with(PROBE_INDETERMINATE))
                .count(),
            0,
            "a quiet cycle drops the token: no count leaks across publish cycles"
        );
        assert!(
            worker
                .update
                .providers
                .iter()
                .any(|(name, available)| name == "Hung window detector" && *available),
            "the quiet cycle keeps the provider available, so the surface returns to OK"
        );

        worker.apply_hang_update(
            hang::Update {
                generation: 3,
                targets: Vec::new(),
                available: false,
                error: Some("hung-window poll budget exceeded"),
                probe_indeterminate: 0,
            },
            3,
        );
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .filter(|(name, _)| name.starts_with(PROBE_INDETERMINATE))
                .count(),
            0,
            "a hard-failure publication keeps its own class, never this token"
        );
        assert!(
            worker
                .update
                .providers
                .iter()
                .any(|(name, available)| name == "Hung window detector" && !available),
            "the unavailable provider stays the counted DOWN cause"
        );
    }

    /// WO-DOCS-F4 (EC-8 pattern: `providers/mod.rs::tests::ec8_docs_checklist_names_every_provider`
    /// L30, EC-5 precedent `providers/cpu.rs::ec5_docs_name_the_single_derivation_and_family`
    /// L216): the user manual must keep the shipped probe-indeterminate loss-cause vocabulary —
    /// the on-panel label per slot band, the full-cause template, the count's silence about the
    /// targets the same cycle publishes, the discrimination from `DOWN` and from a hung target,
    /// and the display precedence exactly as
    /// `render::renderer::provider_slot` implements it (device state > `N DOWN` > token > `OK`,
    /// L1934-1958). Vocabulary drift in the manual reddens this test.
    #[test]
    fn the_manual_names_the_probe_indeterminate_loss_cause() {
        const MANUAL: &str = include_str!("../docs/INSTRUCTION-MANUAL.md");
        assert!(
            MANUAL.contains("`probe-indeterminate-<N>`"),
            "the manual must template the full loss cause as `probe-indeterminate-<N>`"
        );
        assert!(
            MANUAL.contains("`probe-i...`"),
            "the manual must name the widest-slot label `probe-i...` of the four-slot panel"
        );
        assert!(
            MANUAL.contains("the first three slots show `probe-...`"),
            "the manual must name the narrow-slot label `probe-...` (38 px slot interiors)"
        );
        assert!(
            MANUAL.contains("lost checks never become a hung target, so the row names the loss"),
            "the manual must state the exact invariant: the lost checks never become a target"
        );
        assert!(
            MANUAL.contains("nothing about whether a hung target was published"),
            "the manual must keep the count silent about a published hung target"
        );
        assert!(
            MANUAL.contains("never means a hung target, and it never means `DOWN`"),
            "the manual must distinguish the token from `DOWN` and from a hung target"
        );
        assert!(
            MANUAL.contains(
                "device state first, then `N DOWN`, then `probe-indeterminate-<N>`, then `OK`"
            ),
            "the manual must pin the display precedence in the provider_slot arm order (renderer.rs L1934-1958)"
        );
    }

    /// WO-DOCS-F4: the manual keeps the agreed terminal-mark wording of the repeated-press
    /// suppression VERBATIM as fixed by WO-B-03-FIX-AUDIT F-4, plus the TC-3 re-arm rule
    /// (a fresh publication re-arms). Wording drift reddens this test.
    #[test]
    fn the_manual_names_the_terminal_mark_scope() {
        const MANUAL: &str = include_str!("../docs/INSTRUCTION-MANUAL.md");
        assert!(
            MANUAL.contains(
                "identities whose last action on the current publish generation reached `wait-timeout`"
            ),
            "the manual must state the agreed terminal-mark scope verbatim"
        );
        assert!(
            MANUAL.contains(
                "The window becomes pressable again when a new check cycle republishes it."
            ),
            "the manual must state the re-arm on a fresh publication"
        );
    }

    #[test]
    fn safe_mode_clears_expansion_and_rebaselines() {
        let mut worker = test_worker(Config::default());
        worker.update.headset.present = true;
        worker.performance_readings.push(Reading::current_number(
            MetricKey::PageReadsPerSec,
            ValueKind::Count,
            10.0,
            "pdh",
            SystemTime::UNIX_EPOCH,
        ));
        worker.connection_reading = Some(Reading::current_number(
            MetricKey::EstablishedConnections,
            ValueKind::Count,
            4.0,
            "tcp",
            SystemTime::UNIX_EPOCH,
        ));
        worker.performance_attempt = Some(Instant::now());
        worker.reset_state(2);
        worker.safe_mode_active = true;
        assert!(!worker.update.headset.present);
        assert!(worker.performance_readings.is_empty());
        assert!(worker.connection_reading.is_none());
        assert!(worker.performance_attempt.is_none());
        worker.reset_state(3);
        worker.safe_mode_active = false;
        assert!(
            worker.performance_attempt.is_none(),
            "recovery must establish a new PDH baseline"
        );
    }

    #[test]
    fn latest_state_channel_is_capacity_one_and_coalesces() {
        let (sender, updates) = latest_channel();
        for generation in 1..=100 {
            assert!(sender.send(Update {
                generation,
                ..Default::default()
            }));
        }
        assert_eq!(updates.try_recv().unwrap().generation, 100);
        assert!(matches!(updates.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(updates);
        assert!(!sender.send(Update::default()));
    }

    #[test]
    fn safe_mode_runtime_owns_and_joins_all_threads() {
        let runtime = spawn(
            &Config {
                safe_mode: true,
                ..Config::default()
            },
            0,
        );
        assert_eq!(runtime.threads.len(), 8);
        drop(runtime);
    }

    #[test]
    fn hwinfo_is_polled_only_for_complete_configured_pairs() {
        let mut cfg = Config::default();
        assert!(!hwinfo_configured(&cfg));
        cfg.hwinfo_cpu_temp_sensor = "CPU Exact".into();
        cfg.hwinfo_cpu_temp_reading = "Package Temperature".into();
        assert!(hwinfo_configured(&cfg));
        let selectors = hwinfo_selectors(&cfg);
        assert_eq!(selectors.cpu_temp.sensor_label, "CPU Exact");
        cfg.hwinfo_cpu_temp_sensor.clear();
        cfg.hwinfo_cpu_temp_reading.clear();
        cfg.hwinfo_cpu_power_sensor = "CPU".into();
        cfg.hwinfo_cpu_power_reading = "Package Power".into();
        assert!(hwinfo_configured(&cfg));
        let selectors = hwinfo_selectors(&cfg);
        assert_eq!(selectors.cpu_package_power.sensor_label, "CPU");
    }

    #[test]
    fn cpu_temperature_precedence_preserves_hwinfo_then_lhm_current_and_stale() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let hwinfo = Metric::valid(69.0, at);
        let lhm = Metric::valid(67.0, at);
        assert_eq!(prefer_cpu_temperature(hwinfo, lhm).value, 69.0);

        let mut stale_hwinfo = hwinfo;
        stale_hwinfo.stale = true;
        assert_eq!(prefer_cpu_temperature(stale_hwinfo, lhm).value, 67.0);
        let mut stale_lhm = lhm;
        stale_lhm.stale = true;
        let retained = prefer_cpu_temperature(stale_hwinfo, stale_lhm);
        assert_eq!(retained.value, 69.0);
        assert!(retained.stale);
        assert!(!prefer_cpu_temperature(Metric::default(), Metric::default()).valid);
    }

    #[test]
    fn hwinfo_selector_generation_change_invalidates_temperature() {
        let cfg = Config {
            hwinfo_cpu_temp_sensor: "old sensor".into(),
            hwinfo_cpu_temp_reading: "old reading".into(),
            ..Config::default()
        };
        let mut worker = test_worker(cfg);
        worker.hwinfo_cpu_temp = Metric::valid(70.0, SystemTime::UNIX_EPOCH);
        worker.config.write().unwrap().hwinfo_cpu_temp_sensor = "new sensor".into();
        let generation = worker.config_generation.fetch_add(1, Ordering::SeqCst) + 1;
        worker.reset_state(generation);
        assert!(!worker.hwinfo_cpu_temp.valid);

        worker.apply_slow_event(
            SlowEvent::Hwinfo(
                generation - 1,
                hwinfo::Selectors::default(),
                Ok(hwinfo::Sample {
                    cpu_temp_c: Some(99.0),
                    total_power_w: None,
                    cpu_package_power_w: None,
                    gpu_board_power_w: None,
                    issues: Vec::new(),
                    sampled_at: SystemTime::UNIX_EPOCH,
                }),
            ),
            generation,
            Instant::now(),
        );
        assert!(!worker.hwinfo_cpu_temp.valid);
    }

    #[test]
    fn safe_mode_reset_clears_cpu_temperature_sources() {
        let mut worker = test_worker(Config::default());
        let at = SystemTime::UNIX_EPOCH;
        worker.hwinfo_cpu_temp = Metric::valid(69.0, at);
        worker.lhm_cpu_temp = Metric::valid(67.0, at);
        worker.update.cpu_temp =
            prefer_cpu_temperature(worker.hwinfo_cpu_temp, worker.lhm_cpu_temp);
        worker.reset_state(2);
        assert!(!worker.hwinfo_cpu_temp.valid);
        assert!(!worker.lhm_cpu_temp.valid);
        assert!(!worker.update.cpu_temp.valid);
    }

    #[test]
    fn provider_diagnostics_are_actionable_and_do_not_echo_details() {
        assert_eq!(
            hwinfo_diagnostic("HWiNFO shared memory is stale: private"),
            HwinfoDiagnostic::Stale
        );
        assert_eq!(
            hwinfo_diagnostic("HWiNFO shared-memory mapping is inactive: private"),
            HwinfoDiagnostic::Inactive
        );
        assert_eq!(
            hwinfo_diagnostic("private parser detail"),
            HwinfoDiagnostic::Unavailable
        );
        assert!(HWINFO_STALE.contains("12 hours"));
        assert!(LHM_ENDPOINT_UNAVAILABLE.contains("loopback web server"));
        assert_eq!(lhm_diagnostic("LHM JSON: private"), LHM_DATA_UNAVAILABLE);
        assert_eq!(
            lhm_diagnostic("private connection error"),
            LHM_ENDPOINT_UNAVAILABLE
        );
    }

    #[test]
    fn hwinfo_partial_selector_failure_keeps_valid_temperature_and_power() {
        let selectors = hwinfo::Selectors {
            cpu_temp: hwinfo::Selector {
                sensor_label: "CPU".into(),
                reading_label: "Temperature".into(),
            },
            total_power: hwinfo::Selector {
                sensor_label: "System".into(),
                reading_label: "Bad Total".into(),
            },
            gpu_board_power: hwinfo::Selector {
                sensor_label: "GPU".into(),
                reading_label: "Power".into(),
            },
            ..Default::default()
        };
        let issues = vec![hwinfo::SelectorIssue {
            field: hwinfo::SelectorField::TotalPower,
            kind: hwinfo::SelectorIssueKind::MissingOrAmbiguous,
        }];
        let mut worker = test_worker(Config::default());
        worker.apply_hwinfo(
            selectors.clone(),
            Ok(hwinfo::Sample {
                cpu_temp_c: Some(67.0),
                total_power_w: None,
                cpu_package_power_w: None,
                gpu_board_power_w: Some(90.0),
                issues: issues.clone(),
                sampled_at: SystemTime::UNIX_EPOCH,
            }),
            Instant::now(),
        );
        assert_eq!(worker.hwinfo_cpu_temp.value, 67.0);
        assert!(current_reading(&worker.hwinfo_readings, MetricKey::TotalPower).is_none());
        assert_eq!(
            current_reading(&worker.hwinfo_readings, MetricKey::GPUPower)
                .unwrap()
                .number,
            90.0
        );
        assert_eq!(
            worker.hwinfo_diagnostic,
            Some(HwinfoDiagnostic::Selection {
                selectors: Box::new(selectors),
                issues
            })
        );
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .find(|(name, _)| name == "hwinfo"),
            Some(&("hwinfo".into(), true))
        );
    }

    #[test]
    fn hwinfo_diagnostic_survives_generation_and_changes_once_per_selector() {
        let mut worker = test_worker(Config::default());
        let mut selectors = hwinfo::Selectors {
            cpu_temp: hwinfo::Selector {
                sensor_label: "first".into(),
                reading_label: "temperature".into(),
            },
            ..Default::default()
        };
        let issues = vec![hwinfo::SelectorIssue {
            field: hwinfo::SelectorField::CpuTemp,
            kind: hwinfo::SelectorIssueKind::MissingOrAmbiguous,
        }];
        let first = HwinfoDiagnostic::Selection {
            selectors: Box::new(selectors.clone()),
            issues: issues.clone(),
        };
        assert!(set_hwinfo_diagnostic(
            &mut worker.hwinfo_diagnostic,
            Some(first.clone())
        ));
        worker.reset_state(2);
        assert_eq!(worker.hwinfo_diagnostic, Some(first.clone()));
        assert!(!set_hwinfo_diagnostic(
            &mut worker.hwinfo_diagnostic,
            Some(first)
        ));

        selectors.cpu_temp.sensor_label = "second".into();
        let changed = HwinfoDiagnostic::Selection {
            selectors: Box::new(selectors),
            issues,
        };
        assert!(set_hwinfo_diagnostic(
            &mut worker.hwinfo_diagnostic,
            Some(changed.clone())
        ));
        assert!(!set_hwinfo_diagnostic(
            &mut worker.hwinfo_diagnostic,
            Some(changed)
        ));

        worker.apply_freshness(&Config::default(), Instant::now());
        assert!(worker.hwinfo_diagnostic.is_none());
    }

    #[test]
    fn power_precedence_and_fan_calibration_preserve_state() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let hwinfo =
            Reading::current_number(MetricKey::CPUPower, ValueKind::Watts, 70.0, "hwinfo", at);
        let lhm = Reading::current_number(MetricKey::CPUPower, ValueKind::Watts, 60.0, "lhm", at);
        assert_eq!(
            preferred_reading(Some(&hwinfo), Some(&lhm), Some(&hwinfo), Some(&lhm))
                .unwrap()
                .number,
            70.0
        );
        let mut stale_hwinfo = hwinfo.clone();
        stale_hwinfo.freshness = Freshness::Stale;
        assert_eq!(
            preferred_reading(None, Some(&lhm), Some(&stale_hwinfo), Some(&lhm))
                .unwrap()
                .number,
            60.0
        );

        let mut fan = vec![
            Reading::current_percent(MetricKey::CPUFanControl, 40.0, "control", at),
            Reading::current_number(MetricKey::CPUFanRpm, ValueKind::Rpm, 1800.0, "rpm", at),
        ];
        derive_fan_percent(
            &mut fan,
            MetricKey::CPUFanControl,
            MetricKey::CPUFanRpm,
            MetricKey::CPUFanPercent,
            2000,
        );
        assert_eq!(
            current_reading(&fan, MetricKey::CPUFanPercent)
                .unwrap()
                .number,
            40.0
        );
        fan.remove(0);
        fan.retain(|reading| reading.key != MetricKey::CPUFanPercent);
        derive_fan_percent(
            &mut fan,
            MetricKey::CPUFanControl,
            MetricKey::CPUFanRpm,
            MetricKey::CPUFanPercent,
            2000,
        );
        assert_eq!(
            current_reading(&fan, MetricKey::CPUFanPercent)
                .unwrap()
                .number,
            90.0
        );
        fan.retain(|reading| reading.key != MetricKey::CPUFanPercent);
        derive_fan_percent(
            &mut fan,
            MetricKey::CPUFanControl,
            MetricKey::CPUFanRpm,
            MetricKey::CPUFanPercent,
            0,
        );
        assert!(current_reading(&fan, MetricKey::CPUFanPercent).is_none());

        fan[0].freshness = Freshness::Stale;
        derive_fan_percent(
            &mut fan,
            MetricKey::CPUFanControl,
            MetricKey::CPUFanRpm,
            MetricKey::CPUFanPercent,
            2000,
        );
        assert_eq!(
            any_reading(&fan, MetricKey::CPUFanPercent)
                .unwrap()
                .freshness,
            Freshness::Stale
        );

        let native =
            Reading::current_number(MetricKey::GPUPower, ValueKind::Watts, 100.0, "native", at);
        let hwinfo_gpu = power_readings(None, None, Some(90.0), "hwinfo", at);
        let lhm_gpu = power_readings(None, None, Some(80.0), "lhm", at);
        assert_eq!(
            choose_gpu_power(Some(&native), &hwinfo_gpu, &lhm_gpu)
                .unwrap()
                .number,
            100.0
        );
        assert_eq!(
            choose_gpu_power(None, &hwinfo_gpu, &lhm_gpu)
                .unwrap()
                .number,
            90.0
        );
        let mut stale_native = native.clone();
        stale_native.freshness = Freshness::Stale;
        assert_eq!(
            choose_gpu_power(Some(&stale_native), &hwinfo_gpu, &lhm_gpu)
                .unwrap()
                .number,
            90.0
        );
        let mut stale_hwinfo = hwinfo_gpu.clone();
        stale_hwinfo[0].freshness = Freshness::Stale;
        assert_eq!(
            choose_gpu_power(Some(&stale_native), &stale_hwinfo, &lhm_gpu)
                .unwrap()
                .number,
            80.0
        );
        assert_eq!(bounded_power_sum(100_000.0, 100_000.0), Some(200_000.0));
        assert_eq!(bounded_power_sum(100_000.1, 1.0), None);
        assert_eq!(bounded_power_sum(f64::MAX, f64::MAX), None);
        assert_eq!(bounded_power_sum(f64::NAN, 1.0), None);
        assert!(power_readings(Some(100_000.1), None, None, "hwinfo", at).is_empty());
    }

    #[test]
    fn hwinfo_freshness_duration_uses_monotonic_success_time() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let started = Instant::now();
        let cfg = Config {
            hwinfo_cpu_temp_sensor: "CPU".into(),
            hwinfo_cpu_temp_reading: "Temperature".into(),
            hwinfo_stale_after: Duration::from_secs(5),
            ..Config::default()
        };
        let mut worker = test_worker(cfg.clone());
        worker.hwinfo_cpu_temp = Metric::valid(67.0, at);
        worker.hwinfo_readings = power_readings(Some(300.0), None, None, "hwinfo", at);
        worker.hwinfo_success = Some(started);
        worker.apply_freshness(&cfg, started + Duration::from_secs(5));
        assert!(!worker.hwinfo_cpu_temp.stale);
        assert_eq!(worker.hwinfo_readings[0].freshness, Freshness::Current);
        worker.apply_freshness(&cfg, started + Duration::from_secs(6));
        assert!(worker.hwinfo_cpu_temp.stale);
        assert_eq!(worker.hwinfo_cpu_temp.updated, Some(at));
        assert_eq!(worker.hwinfo_readings[0].freshness, Freshness::Stale);
        assert_eq!(worker.hwinfo_readings[0].sampled_at, Some(at));
    }

    /// Mirrors a scheduler loop's sample decisions over an injected clock, using
    /// the same `due`/`sampling_interval` pair the provider loops and worker poll
    /// sites call. `rejoin` flips the provider's consumer bit at that elapsed time.
    fn scheduled_samples(
        cfg: &Config,
        consumers: u64,
        provider: &str,
        full: Duration,
        horizon: Duration,
        rejoin: Option<Duration>,
    ) -> Vec<Instant> {
        let base = Instant::now();
        let mut consumers = consumers;
        let mut last = None;
        let mut samples = Vec::new();
        let mut elapsed = Duration::ZERO;
        while elapsed <= horizon {
            if rejoin.is_some_and(|at| elapsed >= at) {
                consumers |= consumer_bit(provider);
            }
            let interval = sampling_interval(cfg, consumers, provider, full);
            if due(last, interval, base + elapsed) {
                last = Some(base + elapsed);
                samples.push(base + elapsed);
            }
            elapsed += TICK;
        }
        samples
    }

    #[test]
    fn gated_provider_sampling_uses_keep_alive_bound_not_full_cadence() {
        let cfg = Config::default();
        let horizon = Duration::from_secs(300);
        for (provider, full) in [
            ("headset", cfg.headset_poll),
            ("lhm", cfg.lhm_interval),
            ("gpu", cfg.telemetry_interval),
            ("hwinfo", EXTENDED_POLL),
            ("performance", EXTENDED_POLL),
            ("connections", EXTENDED_POLL),
            ("system battery", BATTERY_POLL),
        ] {
            let keep_alive = keep_alive_interval(&cfg, provider).expect("gated provider bound");
            let gated = scheduled_samples(&cfg, 0, provider, full, horizon, None);
            let consumed = scheduled_samples(&cfg, ALL_CONSUMERS, provider, full, horizon, None);
            assert_eq!(
                gated.len(),
                (horizon.as_millis() / keep_alive.as_millis()) as usize + 1,
                "{provider} must sample at the keep-alive bound"
            );
            assert_eq!(
                consumed.len(),
                (horizon.as_millis() / full.as_millis()) as usize + 1,
                "{provider} keeps the configured cadence while consumed"
            );
            assert!(
                gated.len() < consumed.len(),
                "{provider}: keep-alive cadence must be slower than full cadence"
            );
        }
    }

    #[test]
    fn consumer_rejoin_restores_full_cadence_on_the_next_scheduled_tick() {
        let cfg = Config::default();
        let full = cfg.headset_poll;
        let rejoin = Duration::from_secs(30);
        let samples = scheduled_samples(
            &cfg,
            0,
            "headset",
            full,
            Duration::from_secs(120),
            Some(rejoin),
        );
        // Gated: sample at 0. The rejoin at 30s reopens the gate, and the last
        // sample is already older than the full cadence, so 30s samples at once;
        // every later gap is exactly the configured cadence.
        assert_eq!(samples.len(), 8, "0, 30, 45, 60, 75, 90, 105, 120 seconds");
        assert_eq!(samples[1], samples[0] + rejoin, "rejoin samples next tick");
        for pair in samples[2..].windows(2) {
            assert_eq!(pair[1] - pair[0], full, "full cadence resumes after rejoin");
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct PublishedFlags {
        gpu_temp_valid: bool,
        gpu_temp_stale: bool,
        provider_available: bool,
    }

    /// Record/replay harness: an injected provider input sequence (the first
    /// `healthy_samples` results succeed, the rest fail) is scheduled by the
    /// given rule and pushed through the real publish path. `pre_ec3` selects the
    /// pre-EC-3 interval expression so the two recordings are comparable.
    fn published_flag_record(
        cfg: &Config,
        consumers: u64,
        pre_ec3: bool,
        healthy_samples: usize,
        horizon: Duration,
    ) -> Vec<PublishedFlags> {
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        let mut last_attempt = None;
        let mut samples = 0usize;
        let mut record = Vec::new();
        let mut elapsed = Duration::ZERO;
        while elapsed <= horizon {
            let now = base + elapsed;
            let full = cfg.telemetry_interval;
            let interval = if pre_ec3 {
                full
            } else {
                sampling_interval(cfg, consumers, "gpu", full)
            };
            if due(last_attempt, interval, now) {
                last_attempt = Some(now);
                let result = if samples < healthy_samples {
                    samples += 1;
                    Ok(gpu::Sample {
                        backend: "test".into(),
                        readings: Vec::new(),
                        temperature: Metric::valid(70.0, SystemTime::UNIX_EPOCH),
                        power_w: None,
                        sampled_at: SystemTime::UNIX_EPOCH,
                    })
                } else {
                    Err("injected provider input sequence exhausted".into())
                };
                worker.apply_gpu(result, now);
            }
            worker.apply_freshness(cfg, now);
            record.push(PublishedFlags {
                gpu_temp_valid: worker.update.gpu_temp.valid,
                gpu_temp_stale: worker.update.gpu_temp.stale,
                provider_available: worker
                    .update
                    .providers
                    .iter()
                    .find(|(name, _)| name == "gpu")
                    .is_some_and(|(_, ok)| *ok),
            });
            elapsed += TICK;
        }
        record
    }

    #[test]
    fn consumer_present_publishing_is_parity_with_pre_ec3_cadence() {
        // Anti-masking proof, part 1: with a consumer present the gate is inert,
        // so every published stale/valid/availability flag for the same injected
        // input sequence is identical to the pre-EC-3 scheduler behavior.
        let cfg = Config::default();
        let horizon = Duration::from_secs(20);
        for healthy in [0, 1, 12, usize::MAX] {
            let pre_ec3 = published_flag_record(&cfg, 0, true, healthy, horizon);
            let consumed = published_flag_record(&cfg, ALL_CONSUMERS, false, healthy, horizon);
            assert_eq!(
                pre_ec3, consumed,
                "parity broken for gpu publishing (healthy samples: {healthy})"
            );
        }
        // The record is not vacuous: a dying provider does go stale and stops
        // claiming availability, while a live provider never reports stale.
        let dying = published_flag_record(&cfg, 0, true, 1, horizon);
        assert!(dying.iter().any(|flags| flags.gpu_temp_stale));
        assert!(dying.iter().any(|flags| !flags.provider_available));
        let live = published_flag_record(&cfg, 0, true, usize::MAX, horizon);
        assert!(live
            .iter()
            .all(|flags| flags.gpu_temp_valid && !flags.gpu_temp_stale));
        // Gated publishing follows the same rules: at the keep-alive bound (the
        // provider's own stale-after interval) the published value is genuinely
        // current, so gating can never produce a stale-behaving "looks fresh" flag.
        let gated = published_flag_record(&cfg, 0, false, usize::MAX, horizon);
        assert!(gated
            .iter()
            .all(|flags| flags.gpu_temp_valid && !flags.gpu_temp_stale));
    }

    #[test]
    fn gated_provider_publishes_stale_at_the_existing_boundary_never_looks_fresh() {
        // Anti-masking proof, part 2: gated sampling keeps a published value
        // genuinely current (the keep-alive bound IS the provider's own
        // stale-after interval); when sampling stops, the UNCHANGED staleness
        // rule flips it to stale at the same boundary, never to "looks fresh".
        let cfg = Config::default();
        assert_eq!(
            keep_alive_interval(&cfg, "headset").unwrap(),
            cfg.headset_stale_after,
            "keep-alive is derived from the existing stale-after bound"
        );
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        worker.apply_headset(
            Ok(headset::Status {
                online: true,
                percent: 55,
                ..Default::default()
            }),
            base,
        );
        worker.apply_freshness(&cfg, base + cfg.headset_stale_after);
        assert!(
            !worker.update.headset.stale,
            "the existing rule keeps the exclusive stale-after boundary"
        );
        worker.apply_freshness(
            &cfg,
            base + cfg.headset_stale_after + Duration::from_millis(1),
        );
        assert!(
            worker.update.headset.stale,
            "a gated provider publishes the stale flag, not a fresh-looking value"
        );
        assert!(worker.update.headset.present);
        assert_eq!(
            worker
                .update
                .providers
                .iter()
                .find(|(name, _)| name == "headset")
                .map(|(_, ok)| *ok),
            Some(true),
            "availability publication is unchanged by gating"
        );
    }

    #[test]
    fn every_gated_provider_has_a_positive_keep_alive_and_none_is_fully_gated() {
        let cfg = Config::default();
        let bounds = [
            ("headset", cfg.headset_stale_after),
            ("lhm", cfg.lhm_stale_after),
            ("gpu", gpu_stale_after(&cfg)),
            ("hwinfo", cfg.hwinfo_stale_after),
            ("performance", EXTENDED_STALE),
            ("connections", EXTENDED_STALE),
            ("system battery", BATTERY_STALE),
        ];
        assert_eq!(GATED_PROVIDERS.len(), bounds.len());
        for (provider, bound) in bounds {
            assert_eq!(
                keep_alive_interval(&cfg, provider),
                Some(bound),
                "{provider} keep-alive must equal its existing staleness bound"
            );
            let full = Duration::from_millis(1);
            let gated = sampling_interval(&cfg, 0, provider, full);
            assert!(gated > Duration::ZERO, "{provider} must never be gated off");
            assert!(
                gated >= full,
                "{provider} gating never outruns the configured cadence"
            );
            assert_eq!(
                sampling_interval(&cfg, ALL_CONSUMERS, provider, full),
                full,
                "{provider} keeps the configured cadence while consumed"
            );
        }
        for provider in [
            "controller",
            "audio",
            "net",
            "network probe",
            "presentmon",
            "hang",
        ] {
            assert_eq!(
                consumer_bit(provider),
                0,
                "{provider} has no staleness rule in this scheduler, so it is not gated"
            );
            assert_eq!(
                sampling_interval(&cfg, 0, provider, EXTENDED_POLL),
                EXTENDED_POLL
            );
        }
    }

    #[test]
    fn consumer_set_publication_is_visible_to_the_scheduler() {
        let shared = Arc::new(RwLock::new(Config::default()));
        let (_tx, updates) = latest_channel();
        let telemetry = Telemetry {
            updates,
            config: shared,
            generation: Arc::new(AtomicU64::new(1)),
            consumers: Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            shutdown: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        let consumers = Arc::clone(&telemetry.consumers);
        let gated = consumer_bit("headset") | consumer_bit("performance");
        telemetry.set_consumers(gated);
        assert_eq!(consumers.load(Ordering::Relaxed), gated);
        let cfg = Config::default();
        assert_eq!(
            sampling_interval(
                &cfg,
                consumers.load(Ordering::Relaxed),
                "headset",
                cfg.headset_poll
            ),
            cfg.headset_poll
        );
        assert_eq!(
            sampling_interval(
                &cfg,
                consumers.load(Ordering::Relaxed),
                "lhm",
                cfg.lhm_interval
            ),
            keep_alive_interval(&cfg, "lhm")
                .unwrap()
                .max(cfg.lhm_interval)
        );
    }

    /// The shipped text of one sampling loop function (LF-normalized), from its
    /// signature up to the next top-level `fn`.
    fn shipped_loop_source(fn_head: &str) -> String {
        let source = include_str!("telemetry.rs").replace("\r\n", "\n");
        let start = source
            .find(fn_head)
            .expect("the shipped sampling loop exists");
        let rest = &source[start..];
        let end = ["\nfn ", "\npub fn "]
            .iter()
            .filter_map(|needle| rest.find(*needle))
            .min()
            .expect("the shipped loop is followed by a fn");
        rest[..end].to_string()
    }

    /// WO-TEL-B (EC8-F1 re-arm) STRUCTURAL pin of the shipped source of all four
    /// sampling loops. Per loop it proves: a single loop-local generation tracker; the
    /// cadence clear present exactly once and only inside the
    /// `if generation != last_generation` branch; exactly three `last_attempt = None;`
    /// statements (declaration, generation branch, disabled arm) so no unconditional
    /// per-iteration reset exists; the order loop-head generation read -> generation
    /// branch -> `due` cadence gate; and that the emitted `SlowEvent` carries the loop's
    /// own generation.
    ///
    /// This test is a structural inspection of production source (the EC-8/EC-5 `include_str!`
    /// pin pattern). The headset `SlowEvent::Headset(generation, ..)` token below is the shipped
    /// one-line form after the WO-TEL-C sampler seam (rustfmt collapsed the shortened call); the
    /// structural claims are unchanged. Production-sensitive BEHAVIORAL coverage of the same
    /// reset now exists on the real loops, through the WO-TEL-C injected-sampler seams:
    /// `telemetry_headset_loop_behavioral_generation_bump_rearms_sampling`,
    /// `telemetry_lhm_loop_behavioral_generation_bump_rearms_sampling`,
    /// `telemetry_hwinfo_loop_behavioral_generation_bump_rearms_sampling`,
    /// `telemetry_gpu_loop_behavioral_generation_bump_rearms_sampling`.
    #[test]
    fn telemetry_sampling_loops_clear_cadence_only_on_generation_change() {
        // WO-TEL-D2 cite-lockstep (disclosed, not weakened): the loop-head READ
        // token names the cached request read (U-8: the 50 ms tick reuses the
        // `Arc<Config>` under an unchanged generation; the shipped `SeqCst`
        // before/after pair is the same rule). The generation-branch CLEAR block,
        // the `last_attempt = None;` count (declaration + branch + disabled arm;
        // the zero-demand park REUSES the shipped disabled arm, adding no clear
        // site), the read-branch-gate order, and the event tokens are the same
        // structural claims as shipped.
        const READ: &str =
            "let (generation, cfg) = cached_request(&config, &generation, &mut request);";
        // WO-U46I (#16): the gpu loop's generation branch gained the park-flag reset, so the
        // shipped three-line text is claimed as the branch PREFIX (branch head + generation
        // store + cadence clear, exactly once) for every loop, and the park reset is pinned
        // explicitly on the branch body below. Nothing is weakened: one branch, one clear, the
        // same order, and the gpu-only reset is now a stated claim.
        const BRANCH: &str = "if generation != last_generation {\n                    last_generation = generation;\n                    last_attempt = None;";
        for (fn_head, event) in [
            ("fn spawn_headset(", "SlowEvent::Headset(generation,"),
            ("fn spawn_lhm(", "SlowEvent::Lhm(generation,"),
            ("fn spawn_hwinfo(", "SlowEvent::Hwinfo(generation,"),
            ("fn spawn_gpu(", "SlowEvent::Gpu(generation,"),
        ] {
            let loop_src = shipped_loop_source(fn_head);
            assert_eq!(
                loop_src.matches("let mut last_generation = 0u64;").count(),
                1,
                "{fn_head}: exactly one loop-local generation tracker"
            );
            assert_eq!(
                loop_src.matches(BRANCH).count(),
                1,
                "{fn_head}: the cadence clear exists exactly once, inside the generation-change branch"
            );
            let branch_at = loop_src
                .find(BRANCH)
                .expect("{fn_head}: the generation-change branch");
            let branch_body = &loop_src[branch_at
                ..branch_at
                    + loop_src[branch_at..]
                        .find("\n                }")
                        .expect("{fn_head}: the generation-change branch closes")];
            assert_eq!(
                branch_body.matches("parked_published = false;").count(),
                usize::from(fn_head == "fn spawn_gpu("),
                "{fn_head}: the park-flag reset exists exactly once, in the gpu loop's generation branch only"
            );
            assert_eq!(
                loop_src.matches("last_attempt = None;").count(),
                3,
                "{fn_head}: declaration + generation branch + disabled arm only, never an unconditional clear"
            );
            let read = loop_src
                .find(READ)
                .expect("{fn_head}: the loop-head generation read");
            let branch = loop_src
                .find(BRANCH)
                .expect("{fn_head}: the generation-change branch");
            let gate = loop_src
                .find("if due(last_attempt, interval, now) {")
                .expect("{fn_head}: the cadence gate");
            assert!(
                read < branch && branch < gate,
                "{fn_head}: generation read, then the conditional clear, then the cadence gate"
            );
            assert!(
                !loop_src.contains(&format!("{READ}\n                last_attempt = None;")),
                "{fn_head}: the clear must not sit unconditionally at the loop head"
            );
            assert!(
                loop_src.contains(event),
                "{fn_head}: the published event carries the loop's own generation"
            );
        }
    }
    // WO-TEL-C: BEHAVIORAL coverage of the four sampling loops' generation re-arm, driven on the
    // REAL production loop bodies through the fn-pointer sampler seams (the four injected samplers
    // named as the minimal seam in WO-TEL-B §8). Shared machinery first, then one behavioral test
    // per loop. In every test the shipped `spawn_*` loop runs on its own thread and ONLY the
    // sampler is injected (a scripted fn performing zero device I/O, proved by
    // `assert_injected_sampler_is_io_free`): the loop head, the `current_request` generation read,
    // the WO-TEL-B generation tracker and branch clear, the `enabled` arm, the
    // `sampling_interval`/`due` cadence gate, the `TICK` pacing and the `send_slow` publication
    // are shipped production code, and every assertion runs on the REAL `SlowEvent` stream at the
    // production `tx` receiver. Channel handshake: the sampler signals entry, then blocks until
    // the test releases it (loud 5 s budget, never a hang). No `thread::sleep` in these tests:
    // every budget is a bounded `recv_timeout`. Precedent: the WO-F3-NET network-worker slow probe
    // (`src/providers/network.rs`, the `SlowProbe`/`install_slow_probe`/`block_in_probe` trio).

    struct ScriptedSampler<R> {
        enter: Sender<()>,
        release: Mutex<Receiver<()>>,
        outcomes: Mutex<VecDeque<R>>,
    }

    static HEADSET_BEHAVIORAL_SAMPLER: OnceLock<ScriptedSampler<Result<headset::Status, String>>> =
        OnceLock::new();
    static LHM_BEHAVIORAL_SAMPLER: OnceLock<
        ScriptedSampler<Result<(lhm::ExtendedSample, SystemTime), String>>,
    > = OnceLock::new();
    static HWINFO_BEHAVIORAL_SAMPLER: OnceLock<ScriptedSampler<Result<hwinfo::Sample, String>>> =
        OnceLock::new();
    static GPU_BEHAVIORAL_SAMPLER: OnceLock<ScriptedSampler<Result<gpu::Sample, String>>> =
        OnceLock::new();

    /// "The real loop reached the next thing I drove it to" budget (the loop ticks every TICK).
    const BEHAVIORAL_WAIT: Duration = Duration::from_secs(2);
    /// The driven cadence for the config-driven loops (a test input to the real cadence gate).
    const BEHAVIORAL_DRIVEN: Duration = Duration::from_millis(400);
    /// A bounded multi-tick window in which a cadence-respecting loop must not enter the sampler.
    const BEHAVIORAL_SUPPRESS_WINDOW: Duration = Duration::from_millis(250);
    /// A cadence that cannot fall due inside a behavioral test, so any entry proves a clear.
    const BEHAVIORAL_LONG: Duration = Duration::from_secs(30);
    /// The HWiNFO loop's cadence is the shipped `EXTENDED_POLL` (1 s), so its mid-cadence bump
    /// budget is half of that: an entry inside it proves the clear, not a due interval.
    const BEHAVIORAL_HWINFO_HALF: Duration = Duration::from_millis(500);

    fn install_scripted_sampler<R>(
        slot: &'static OnceLock<ScriptedSampler<R>>,
        outcomes: Vec<R>,
    ) -> (Receiver<()>, Sender<()>) {
        let (enter_tx, enter_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        assert!(
            slot.set(ScriptedSampler {
                enter: enter_tx,
                release: Mutex::new(release_rx),
                outcomes: Mutex::new(outcomes.into_iter().collect()),
            })
            .is_ok(),
            "each scripted-sampler slot is installed exactly once (one behavioral test per slot)"
        );
        (enter_rx, release_tx)
    }

    fn sampled_in_slot<R>(slot: &OnceLock<ScriptedSampler<R>>) -> R {
        let sampler = slot
            .get()
            .expect("the test installs the scripted sampler before spawning the real loop");
        sampler
            .enter
            .send(())
            .expect("the scripted sampler signals its entry to the test");
        sampler
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("the test releases the scripted sampler (loud 5 s budget, never a hang)");
        sampler
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("the scripted Ok/Err sequence is not exhausted")
    }

    fn behavioral_headset_sampler(_timeout: Duration) -> Result<headset::Status, String> {
        sampled_in_slot(&HEADSET_BEHAVIORAL_SAMPLER)
    }

    fn behavioral_lhm_sampler(
        _url: &str,
        _overrides: &std::collections::BTreeMap<String, String>,
        _selectors: &lhm::ExtendedSelectors,
        _timeout: Duration,
    ) -> Result<(lhm::ExtendedSample, SystemTime), String> {
        sampled_in_slot(&LHM_BEHAVIORAL_SAMPLER)
    }

    fn behavioral_hwinfo_sampler(
        _selectors: &hwinfo::Selectors,
        _max_age: Duration,
    ) -> Result<hwinfo::Sample, String> {
        sampled_in_slot(&HWINFO_BEHAVIORAL_SAMPLER)
    }

    fn behavioral_gpu_sampler(
        _provider: &mut gpu::Provider,
        _selector: &str,
    ) -> Result<gpu::Sample, String> {
        sampled_in_slot(&GPU_BEHAVIORAL_SAMPLER)
    }

    /// Zero-real-I/O proof for an injected sampler: the shipped text of the injected fn itself
    /// carries no call-shaped token of a live provider entry point, HID, HTTP, socket, pipe,
    /// shared-memory, GPU-library or process API. It touches only its scripted slot.
    const INJECTED_SAMPLER_IO_TOKENS: [&str; 19] = [
        "headset::query(",
        "lhm::sample_extended(",
        "hwinfo::sample(",
        "gpu_sample(",
        "provider.sample(",
        "::new(",
        "http::",
        "crate::http",
        "tcpstream",
        "udpsocket",
        "hidd_",
        "hidapi",
        "createfile",
        "deviceiocontrol",
        "readfile",
        "writefile",
        "nvapi",
        "adlx",
        "namedpipe",
    ];

    fn assert_injected_sampler_is_io_free(fn_name: &str) {
        let source = include_str!("telemetry.rs").replace("\r\n", "\n");
        let head = format!("fn {fn_name}(");
        let start = source
            .find(&head)
            .expect("the injected sampler exists in the shipped source");
        let close = "\n    }";
        let rest = &source[start..];
        let end = rest
            .find(close)
            .expect("the injected sampler is a braced fn in the tests region");
        let body = rest[..end + close.len()].to_lowercase();
        for token in INJECTED_SAMPLER_IO_TOKENS {
            assert!(
                !body.contains(token),
                "the injected sampler `{fn_name}` must perform zero real I/O; found `{token}`"
            );
        }
    }

    fn headset_event(event: SlowEvent) -> (u64, Result<(bool, i32), String>) {
        match event {
            SlowEvent::Headset(generation, result) => (
                generation,
                result.map(|status| (status.online, status.percent)),
            ),
            _ => panic!("the headset loop published a non-headset event"),
        }
    }

    fn lhm_event(event: SlowEvent) -> (u64, Result<SystemTime, String>) {
        match event {
            SlowEvent::Lhm(generation, result) => (generation, result.map(|(_, at)| at)),
            _ => panic!("the LHM loop published a non-LHM event"),
        }
    }

    fn hwinfo_event(event: SlowEvent) -> (u64, String, Result<(f64, SystemTime), String>) {
        match event {
            SlowEvent::Hwinfo(generation, selectors, result) => (
                generation,
                selectors.cpu_temp.sensor_label,
                result.map(|sample| (sample.cpu_temp_c.unwrap_or_default(), sample.sampled_at)),
            ),
            _ => panic!("the HWiNFO loop published a non-HWiNFO event"),
        }
    }

    fn gpu_event(event: SlowEvent) -> (u64, Result<(String, SystemTime), String>) {
        match event {
            SlowEvent::Gpu(generation, result) => (
                generation,
                result.map(|sample| (sample.backend, sample.sampled_at)),
            ),
            _ => panic!("the GPU loop published a non-GPU event"),
        }
    }

    fn retune<T>(config: &Arc<RwLock<Config>>, change: T)
    where
        T: FnOnce(&mut Config),
    {
        let mut next = config.read().unwrap().clone();
        change(&mut next);
        *config.write().unwrap() = next;
    }

    fn marker_time(index: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(index)
    }

    /// BEHAVIORAL (WO-TEL-C) headset loop through the sampler seam. REAL code: the whole
    /// `spawn_headset` body - loop head, generation read, WO-TEL-B tracker + conditional clear,
    /// `enabled`, `sampling_interval`/`due` cadence gate, `TICK` pacing, `SlowEvent::Headset`
    /// publication. INJECTED: only `behavioral_headset_sampler` (production `spawn` passes the
    /// real `headset::query` in that slot). Proves on the real path: (i) sampler entered and `Ok`
    /// published with the loop's generation and the scripted payload; (ii) with no bump the second
    /// sample is suppressed until the driven interval passes; (iii) a mid-cadence generation bump
    /// with the provider still enabled emits the next-tick sample (the WO-TEL-B §8 claim); (iv) an
    /// `Err` sampler outcome is published with the correct generation.
    #[test]
    fn telemetry_headset_loop_behavioral_generation_bump_rearms_sampling() {
        assert_injected_sampler_is_io_free("behavioral_headset_sampler");
        let status = |online: bool, percent: i32| headset::Status {
            online,
            percent,
            ..Default::default()
        };
        let (enter_rx, release_tx) = install_scripted_sampler(
            &HEADSET_BEHAVIORAL_SAMPLER,
            vec![
                Ok(status(true, 100)),
                Ok(status(true, 100)),
                Ok(status(false, 33)),
                Err("synthetic headset query failure".to_string()),
            ],
        );
        let config = Arc::new(RwLock::new(Config {
            headset_enabled: true,
            headset_poll: BEHAVIORAL_DRIVEN,
            headset_query_timeout: Duration::from_millis(1),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_headset(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            Arc::clone(&shutdown),
            tx,
            behavioral_headset_sampler,
        );
        let cadence_start = Instant::now();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(i) the real headset loop sampled through the seam");
        release_tx.send(()).expect("release #1");
        let (gen1, payload1) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(i) the loop published SlowEvent::Headset"),
        );
        assert_eq!(gen1, 1, "(i) the first event carries the loop's generation");
        assert_eq!(
            payload1.expect("(i) the scripted Ok status is published"),
            (true, 100),
            "(i) the payload comes from the injected script, not a device read"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW)
                .is_err(),
            "(ii) under an unchanged generation the loop must stay suppressed across at least {} TICKs of the {BEHAVIORAL_DRIVEN:?} cadence",
            BEHAVIORAL_SUPPRESS_WINDOW.as_millis() / TICK.as_millis(),
        );
        enter_rx
            .recv_timeout(BEHAVIORAL_DRIVEN + Duration::from_millis(600))
            .expect("(ii) the second sample becomes due once the driven interval passes");
        release_tx.send(()).expect("release #2");
        let (gen2, payload2) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(ii) the due sample is published"),
        );
        assert_eq!(
            gen2, 1,
            "(ii) the cadence-respecting sample keeps the unchanged generation"
        );
        assert!(payload2.is_ok(), "(ii) the scripted Ok is published");
        assert!(
            cadence_start.elapsed() >= BEHAVIORAL_DRIVEN - 3 * TICK,
            "(ii) the second sample arrived no earlier than the driven cadence, elapsed {:?}",
            cadence_start.elapsed()
        );
        retune(&config, |cfg| cfg.headset_poll = BEHAVIORAL_LONG);
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(iii) the generation bump cleared `last_attempt`, so the real loop samples on the next tick under a 30 s cadence");
        release_tx.send(()).expect("release #3");
        let (gen3, payload3) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iii) the re-armed next-tick sample is published"),
        );
        assert_eq!(
            gen3, 2,
            "(iii) the re-armed sample is published on the new generation"
        );
        assert_eq!(
            payload3.expect("(iii) the scripted Ok is published"),
            (false, 33),
            "(iii) the payload is the script's third outcome"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW)
                .is_err(),
            "(iv) with the generation unchanged the loop keeps its cadence: an unconditional per-iteration clear would enter the sampler every tick"
        );
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(iv) the second bump re-arms the sampler");
        release_tx.send(()).expect("release #4");
        let (gen4, payload4) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iv) the Err outcome is published on the loop's own tx stream"),
        );
        assert_eq!(
            gen4, 3,
            "(iv) the published Err carries the current generation"
        );
        assert_eq!(
            payload4
                .expect_err("(iv) the scripted Err is published as-is")
                .as_str(),
            "synthetic headset query failure",
            "(iv) the failure text comes from the script"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the headset loop joins within the sampler's loud 5 s budget");
    }

    /// BEHAVIORAL (WO-TEL-C) LHM loop through the sampler seam. REAL code: the whole `spawn_lhm`
    /// body - `lhm_url` gate, loop head, generation read, WO-TEL-B tracker + conditional clear,
    /// the `lhm_mode` settled-cadence interval selection (WO-TEL-D2: with the
    /// `temps_need_refresh` inversion removed, `lhm_mode "on"` keeps the full configured
    /// cadence — the driven cadences below are unchanged), `sampling_interval`/`due`, `TICK`
    /// pacing and `SlowEvent::Lhm` publication. INJECTED: only `behavioral_lhm_sampler`
    /// (production passes the real `lhm::sample_extended`); the synthetic URL is never dialed, so
    /// zero HTTP. Same four claims as the headset test, on the LHM event stream.
    #[test]
    fn telemetry_lhm_loop_behavioral_generation_bump_rearms_sampling() {
        assert_injected_sampler_is_io_free("behavioral_lhm_sampler");
        let (enter_rx, release_tx) = install_scripted_sampler(
            &LHM_BEHAVIORAL_SAMPLER,
            vec![
                Ok((lhm::ExtendedSample::default(), marker_time(1))),
                Ok((lhm::ExtendedSample::default(), marker_time(2))),
                Ok((lhm::ExtendedSample::default(), marker_time(3))),
                Err("synthetic LHM sample failure".to_string()),
            ],
        );
        let config = Arc::new(RwLock::new(Config {
            lhm_mode: "on".into(),
            lhm_url: "http://127.0.0.1:1/synthetic-lhm-never-dialed".into(),
            lhm_interval: BEHAVIORAL_DRIVEN,
            lhm_stale_after: BEHAVIORAL_LONG,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_lhm(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            Arc::clone(&shutdown),
            tx,
            behavioral_lhm_sampler,
        );
        let cadence_start = Instant::now();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(i) the real LHM loop sampled through the seam");
        release_tx.send(()).expect("release #1");
        let (gen1, payload1) = lhm_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(i) the loop published SlowEvent::Lhm"),
        );
        assert_eq!(gen1, 1, "(i) the first event carries the loop's generation");
        assert_eq!(
            payload1.expect("(i) the scripted Ok sample is published"),
            marker_time(1),
            "(i) the payload timestamp comes from the injected script, not from LHM"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW)
                .is_err(),
            "(ii) under an unchanged generation the LHM loop must stay suppressed across at least {} TICKs of the {BEHAVIORAL_DRIVEN:?} cadence",
            BEHAVIORAL_SUPPRESS_WINDOW.as_millis() / TICK.as_millis(),
        );
        enter_rx
            .recv_timeout(BEHAVIORAL_DRIVEN + Duration::from_millis(600))
            .expect("(ii) the second sample becomes due once the driven interval passes");
        release_tx.send(()).expect("release #2");
        let (gen2, payload2) = lhm_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(ii) the due sample is published"),
        );
        assert_eq!(
            gen2, 1,
            "(ii) the cadence-respecting sample keeps the unchanged generation"
        );
        assert_eq!(
            payload2.expect("(ii) the scripted Ok is published"),
            marker_time(2),
            "(ii) the payload is the script's second outcome"
        );
        assert!(
            cadence_start.elapsed() >= BEHAVIORAL_DRIVEN - 3 * TICK,
            "(ii) the second sample arrived no earlier than the driven cadence, elapsed {:?}",
            cadence_start.elapsed()
        );
        retune(&config, |cfg| cfg.lhm_interval = BEHAVIORAL_LONG);
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(iii) the generation bump cleared `last_attempt`, so the real LHM loop samples on the next tick under a 30 s cadence");
        release_tx.send(()).expect("release #3");
        let (gen3, payload3) = lhm_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iii) the re-armed next-tick sample is published"),
        );
        assert_eq!(
            gen3, 2,
            "(iii) the re-armed sample is published on the new generation"
        );
        assert_eq!(
            payload3.expect("(iii) the scripted Ok is published"),
            marker_time(3),
            "(iii) the payload is the script's third outcome"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW)
                .is_err(),
            "(iv) with the generation unchanged the LHM loop keeps its cadence: an unconditional per-iteration clear would enter the sampler every tick"
        );
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(iv) the second bump re-arms the sampler");
        release_tx.send(()).expect("release #4");
        let (gen4, payload4) = lhm_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iv) the Err outcome is published on the loop's own tx stream"),
        );
        assert_eq!(
            gen4, 3,
            "(iv) the published Err carries the current generation"
        );
        assert_eq!(
            payload4
                .expect_err("(iv) the scripted Err is published as-is")
                .as_str(),
            "synthetic LHM sample failure",
            "(iv) the failure text comes from the script"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the LHM loop joins within the sampler's loud 5 s budget");
    }

    /// BEHAVIORAL (WO-TEL-C) HWiNFO loop through the sampler seam. REAL code: the whole
    /// `spawn_hwinfo` body - `hwinfo_configured` gate, loop head, generation read, WO-TEL-B
    /// tracker + conditional clear, the shipped `EXTENDED_POLL` cadence with
    /// `sampling_interval`/`due`, `hwinfo_selectors` (its output is published with the event),
    /// `TICK` pacing and `SlowEvent::Hwinfo` publication. INJECTED: only
    /// `behavioral_hwinfo_sampler` (production passes the real `hwinfo::sample`), so the HWiNFO
    /// shared-memory IPC is never touched.
    #[test]
    fn telemetry_hwinfo_loop_behavioral_generation_bump_rearms_sampling() {
        assert_injected_sampler_is_io_free("behavioral_hwinfo_sampler");
        let sample = |cpu_temp_c: f64, index: u64| hwinfo::Sample {
            cpu_temp_c: Some(cpu_temp_c),
            total_power_w: None,
            cpu_package_power_w: None,
            gpu_board_power_w: None,
            issues: Vec::new(),
            sampled_at: marker_time(index),
        };
        let (enter_rx, release_tx) = install_scripted_sampler(
            &HWINFO_BEHAVIORAL_SAMPLER,
            vec![
                Ok(sample(11.0, 1)),
                Ok(sample(22.0, 2)),
                Ok(sample(33.0, 3)),
                Err("synthetic HWiNFO sample failure".to_string()),
            ],
        );
        let config = Arc::new(RwLock::new(Config {
            hwinfo_cpu_temp_sensor: "CPU Exact".into(),
            hwinfo_cpu_temp_reading: "Package Temperature".into(),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_hwinfo(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            Arc::clone(&shutdown),
            tx,
            behavioral_hwinfo_sampler,
        );
        let cadence_start = Instant::now();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(i) the real HWiNFO loop sampled through the seam");
        release_tx.send(()).expect("release #1");
        let (gen1, selector, payload1) = hwinfo_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(i) the loop published SlowEvent::Hwinfo"),
        );
        assert_eq!(gen1, 1, "(i) the first event carries the loop's generation");
        assert_eq!(
            selector, "CPU Exact",
            "(i) the published selector set is built by the real `hwinfo_selectors` from the live config"
        );
        assert_eq!(
            payload1.expect("(i) the scripted Ok sample is published"),
            (11.0, marker_time(1)),
            "(i) the payload comes from the injected script, not from HWiNFO"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_HWINFO_HALF)
                .is_err(),
            "(ii) under an unchanged generation the HWiNFO loop must stay suppressed across at least {} TICKs of the shipped {EXTENDED_POLL:?} cadence",
            BEHAVIORAL_HWINFO_HALF.as_millis() / TICK.as_millis(),
        );
        enter_rx
            .recv_timeout(EXTENDED_POLL + Duration::from_millis(600))
            .expect("(ii) the second sample becomes due once the shipped interval passes");
        release_tx.send(()).expect("release #2");
        let (gen2, _, payload2) = hwinfo_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(ii) the due sample is published"),
        );
        assert_eq!(
            gen2, 1,
            "(ii) the cadence-respecting sample keeps the unchanged generation"
        );
        assert_eq!(
            payload2.expect("(ii) the scripted Ok is published"),
            (22.0, marker_time(2)),
            "(ii) the payload is the script's second outcome"
        );
        assert!(
            cadence_start.elapsed() >= EXTENDED_POLL - 3 * TICK,
            "(ii) the second sample arrived no earlier than the shipped cadence, elapsed {:?}",
            cadence_start.elapsed()
        );
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_HWINFO_HALF)
            .expect("(iii) the mid-cadence generation bump cleared `last_attempt`, so the real HWiNFO loop samples on the next tick, far inside its {EXTENDED_POLL:?} cadence");
        release_tx.send(()).expect("release #3");
        let (gen3, _, payload3) = hwinfo_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iii) the re-armed next-tick sample is published"),
        );
        assert_eq!(
            gen3, 2,
            "(iii) the re-armed sample is published on the new generation"
        );
        assert_eq!(
            payload3.expect("(iii) the scripted Ok is published"),
            (33.0, marker_time(3)),
            "(iii) the payload is the script's third outcome"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_HWINFO_HALF)
                .is_err(),
            "(iv) with the generation unchanged the HWiNFO loop keeps its cadence: an unconditional per-iteration clear would enter the sampler every tick"
        );
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_HWINFO_HALF)
            .expect("(iv) the second bump re-arms the sampler");
        release_tx.send(()).expect("release #4");
        let (gen4, _, payload4) = hwinfo_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iv) the Err outcome is published on the loop's own tx stream"),
        );
        assert_eq!(
            gen4, 3,
            "(iv) the published Err carries the current generation"
        );
        assert_eq!(
            payload4
                .expect_err("(iv) the scripted Err is published as-is")
                .as_str(),
            "synthetic HWiNFO sample failure",
            "(iv) the failure text comes from the script"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the HWiNFO loop joins within the sampler's loud 5 s budget");
    }

    /// BEHAVIORAL (WO-TEL-C) GPU loop through the sampler seam. REAL code: the whole `spawn_gpu`
    /// body - the loop-owned `gpu::Provider::new("off")` construction (inert: no backend open),
    /// `enabled`, loop head, generation read, WO-TEL-B tracker + conditional clear,
    /// `sampling_interval`/`due`, `TICK` pacing and `SlowEvent::Gpu` publication. INJECTED: only
    /// `behavioral_gpu_sampler` in the slot where production `spawn` passes the `gpu_sample`
    /// adapter (whose body is the shipped `provider.sample(selector)`); the injected sampler
    /// ignores the provider, so no GPU library is loaded or called.
    #[test]
    fn telemetry_gpu_loop_behavioral_generation_bump_rearms_sampling() {
        assert_injected_sampler_is_io_free("behavioral_gpu_sampler");
        let sample = |backend: &str, index: u64| gpu::Sample {
            backend: backend.into(),
            readings: Vec::new(),
            temperature: Metric::valid(11.0, marker_time(index)),
            power_w: None,
            sampled_at: marker_time(index),
        };
        let (enter_rx, release_tx) = install_scripted_sampler(
            &GPU_BEHAVIORAL_SAMPLER,
            vec![
                Ok(sample("synthetic-injected-one", 1)),
                Ok(sample("synthetic-injected-two", 2)),
                Ok(sample("synthetic-injected-three", 3)),
                Err("synthetic GPU sample failure".to_string()),
            ],
        );
        let config = Arc::new(RwLock::new(Config {
            gpu_provider: "nvapi".into(),
            telemetry_interval: BEHAVIORAL_DRIVEN,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_gpu(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            Arc::clone(&shutdown),
            tx,
            behavioral_gpu_sampler,
        );
        let cadence_start = Instant::now();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(i) the real GPU loop sampled through the seam");
        release_tx.send(()).expect("release #1");
        let (gen1, payload1) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(i) the loop published SlowEvent::Gpu"),
        );
        assert_eq!(gen1, 1, "(i) the first event carries the loop's generation");
        assert_eq!(
            payload1.expect("(i) the scripted Ok sample is published"),
            ("synthetic-injected-one".to_string(), marker_time(1)),
            "(i) the backend name and timestamp come from the injected script, not a GPU backend"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW)
                .is_err(),
            "(ii) under an unchanged generation the GPU loop must stay suppressed across at least {} TICKs of the {BEHAVIORAL_DRIVEN:?} cadence",
            BEHAVIORAL_SUPPRESS_WINDOW.as_millis() / TICK.as_millis(),
        );
        enter_rx
            .recv_timeout(BEHAVIORAL_DRIVEN + Duration::from_millis(600))
            .expect("(ii) the second sample becomes due once the driven interval passes");
        release_tx.send(()).expect("release #2");
        let (gen2, payload2) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(ii) the due sample is published"),
        );
        assert_eq!(
            gen2, 1,
            "(ii) the cadence-respecting sample keeps the unchanged generation"
        );
        assert_eq!(
            payload2.expect("(ii) the scripted Ok is published"),
            ("synthetic-injected-two".to_string(), marker_time(2)),
            "(ii) the payload is the script's second outcome"
        );
        assert!(
            cadence_start.elapsed() >= BEHAVIORAL_DRIVEN - 3 * TICK,
            "(ii) the second sample arrived no earlier than the driven cadence, elapsed {:?}",
            cadence_start.elapsed()
        );
        retune(&config, |cfg| cfg.telemetry_interval = BEHAVIORAL_LONG);
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(iii) the generation bump cleared `last_attempt`, so the real GPU loop samples on the next tick under a 30 s cadence");
        release_tx.send(()).expect("release #3");
        let (gen3, payload3) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iii) the re-armed next-tick sample is published"),
        );
        assert_eq!(
            gen3, 2,
            "(iii) the re-armed sample is published on the new generation"
        );
        assert_eq!(
            payload3.expect("(iii) the scripted Ok is published"),
            ("synthetic-injected-three".to_string(), marker_time(3)),
            "(iii) the payload is the script's third outcome"
        );
        assert!(
            enter_rx
                .recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW)
                .is_err(),
            "(iv) with the generation unchanged the GPU loop keeps its cadence: an unconditional per-iteration clear would enter the sampler every tick"
        );
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(iv) the second bump re-arms the sampler");
        release_tx.send(()).expect("release #4");
        let (gen4, payload4) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iv) the Err outcome is published on the loop's own tx stream"),
        );
        assert_eq!(
            gen4, 3,
            "(iv) the published Err carries the current generation"
        );
        assert_eq!(
            payload4
                .expect_err("(iv) the scripted Err is published as-is")
                .as_str(),
            "synthetic GPU sample failure",
            "(iv) the failure text comes from the script"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the GPU loop joins within the sampler's loud 5 s budget");
    }

    // WO-TEL-D2: proofs for the ratified demand-gating scope. Seam loops (P1/P2
    // on the REAL production loop bodies, only the sampler injected, WO-TEL-C
    // pattern: a fresh scripted-sampler slot per test, channel handshake, loud
    // bounded budgets, zero real I/O); coordinator polls (P1/P2/P4 driven with
    // the shipped `due`/`apply_freshness` machinery on synthetic `now`s);
    // backoff (pure function + loop-level outcome ladders). The demand word is
    // a plain `Arc<AtomicU64>` the app's render tick publishes in production
    // (`app.rs:183/323`), so a store flips demand exactly like a slot cycle.

    static ZD_HEADSET_SAMPLER: OnceLock<ScriptedSampler<Result<headset::Status, String>>> =
        OnceLock::new();
    static ZD_LHM_SAMPLER: OnceLock<
        ScriptedSampler<Result<(lhm::ExtendedSample, SystemTime), String>>,
    > = OnceLock::new();
    static ZD_HWINFO_SAMPLER: OnceLock<ScriptedSampler<Result<hwinfo::Sample, String>>> =
        OnceLock::new();
    static ZD_GPU_SAMPLER: OnceLock<ScriptedSampler<Result<gpu::Sample, String>>> = OnceLock::new();
    static BO_LHM_SAMPLER: OnceLock<
        ScriptedSampler<Result<(lhm::ExtendedSample, SystemTime), String>>,
    > = OnceLock::new();
    static BO_GPU_SAMPLER: OnceLock<ScriptedSampler<Result<gpu::Sample, String>>> = OnceLock::new();
    static PK_GPU_SAMPLER: OnceLock<ScriptedSampler<Result<gpu::Sample, String>>> = OnceLock::new();

    fn zd_headset_sampler(_timeout: Duration) -> Result<headset::Status, String> {
        sampled_in_slot(&ZD_HEADSET_SAMPLER)
    }

    fn zd_lhm_sampler(
        _url: &str,
        _overrides: &std::collections::BTreeMap<String, String>,
        _selectors: &lhm::ExtendedSelectors,
        _timeout: Duration,
    ) -> Result<(lhm::ExtendedSample, SystemTime), String> {
        sampled_in_slot(&ZD_LHM_SAMPLER)
    }

    fn zd_hwinfo_sampler(
        _selectors: &hwinfo::Selectors,
        _max_age: Duration,
    ) -> Result<hwinfo::Sample, String> {
        sampled_in_slot(&ZD_HWINFO_SAMPLER)
    }

    fn zd_gpu_sampler(
        _provider: &mut gpu::Provider,
        _selector: &str,
    ) -> Result<gpu::Sample, String> {
        sampled_in_slot(&ZD_GPU_SAMPLER)
    }

    fn pk_gpu_sampler(
        _provider: &mut gpu::Provider,
        _selector: &str,
    ) -> Result<gpu::Sample, String> {
        sampled_in_slot(&PK_GPU_SAMPLER)
    }

    fn bo_lhm_sampler(
        _url: &str,
        _overrides: &std::collections::BTreeMap<String, String>,
        _selectors: &lhm::ExtendedSelectors,
        _timeout: Duration,
    ) -> Result<(lhm::ExtendedSample, SystemTime), String> {
        sampled_in_slot(&BO_LHM_SAMPLER)
    }

    fn bo_gpu_sampler(
        _provider: &mut gpu::Provider,
        _selector: &str,
    ) -> Result<gpu::Sample, String> {
        sampled_in_slot(&BO_GPU_SAMPLER)
    }

    fn lhm_outcomes(count: usize) -> Vec<Result<(lhm::ExtendedSample, SystemTime), String>> {
        (0..count)
            .map(|_| Err("synthetic LHM endpoint failure (backoff ladder)".to_string()))
            .collect()
    }

    fn gpu_outcomes(count: usize) -> Vec<Result<gpu::Sample, String>> {
        (0..count)
            .map(|_| Err("synthetic GPU open failure (backoff ladder)".to_string()))
            .collect()
    }

    /// A scripted Ok outcome for the zero-demand loops (content irrelevant; the
    /// payload is never inspected beyond `is_ok`).
    fn headset_ok(index: u64) -> Result<headset::Status, String> {
        Ok(headset::Status {
            online: true,
            percent: index as i32,
            ..Default::default()
        })
    }

    fn lhm_ok(index: u64) -> Result<(lhm::ExtendedSample, SystemTime), String> {
        Ok((lhm::ExtendedSample::default(), marker_time(index)))
    }

    fn hwinfo_ok(index: u64) -> Result<hwinfo::Sample, String> {
        Ok(hwinfo::Sample {
            cpu_temp_c: Some(index as f64),
            total_power_w: None,
            cpu_package_power_w: None,
            gpu_board_power_w: None,
            issues: Vec::new(),
            sampled_at: marker_time(index),
        })
    }

    fn gpu_ok(index: u64) -> Result<gpu::Sample, String> {
        Ok(gpu::Sample {
            backend: format!("synthetic-{index}"),
            readings: Vec::new(),
            temperature: Metric::valid(60.0 + index as f64, marker_time(index)),
            power_w: None,
            sampled_at: marker_time(index),
        })
    }

    /// P1+P2 for the four seam loops: with demand 0 the REAL loop parks (zero
    /// sampler entries over a window spanning several keep-alive bounds), and
    /// publishing the demand bit starts the sampler on the next tick within
    /// `BEHAVIORAL_WAIT` (the production bound is <=100 ms publication + one
    /// 50 ms `TICK`; the budget here is 2 s). Falsifiability: the shipped
    /// keep-alive floor sampled at `t = 0` with demand 0 (`due(None) = true`),
    /// so removing the demand gate makes the window assertion red; freezing the
    /// demand word makes the resume assertion red.
    #[test]
    fn telemetry_headset_loop_zero_demand_parks_and_resume_samples_within_bound() {
        assert_injected_sampler_is_io_free("zd_headset_sampler");
        let (enter_rx, release_tx) =
            install_scripted_sampler(&ZD_HEADSET_SAMPLER, vec![headset_ok(1), headset_ok(2)]);
        let config = Arc::new(RwLock::new(Config {
            headset_enabled: true,
            headset_poll: BEHAVIORAL_DRIVEN,
            headset_stale_after: Duration::from_millis(200),
            headset_query_timeout: Duration::from_millis(1),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_headset(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            zd_headset_sampler,
        );
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "(P1) the real headset loop enters the sampler ZERO times across 900 ms (> 2 x the 200 ms keep-alive bound) while no consumer reads the headset"
        );
        consumers.store(consumer_bit("headset"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) the demand bit starts the sampler on the next tick");
        release_tx.send(()).expect("release #1");
        let (gen1, _) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(P2) the demanded sample is published"),
        );
        assert_eq!(gen1, 1, "(P2) the event carries the loop generation");
        consumers.store(0, Ordering::Relaxed);
        loop_thread.thread().unpark();
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "(P1) demand release parks the loop again (zero further entries)"
        );
        consumers.store(consumer_bit("headset"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) re-demand resumes sampling");
        release_tx.send(()).expect("release #2");
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the headset loop joins within the sampler's loud 5 s budget");
    }

    #[test]
    fn telemetry_lhm_loop_zero_demand_parks_and_resume_samples_within_bound() {
        assert_injected_sampler_is_io_free("zd_lhm_sampler");
        let (enter_rx, release_tx) =
            install_scripted_sampler(&ZD_LHM_SAMPLER, vec![lhm_ok(1), lhm_ok(2)]);
        let config = Arc::new(RwLock::new(Config {
            lhm_mode: "on".into(),
            lhm_url: "http://127.0.0.1:1/synthetic-lhm-never-dialed".into(),
            lhm_interval: BEHAVIORAL_DRIVEN,
            lhm_stale_after: Duration::from_millis(200),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_lhm(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            zd_lhm_sampler,
        );
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "(P1) the real LHM loop enters the sampler ZERO times across 900 ms (> 2 x the 200 ms keep-alive bound) while no consumer reads temperatures"
        );
        consumers.store(consumer_bit("lhm"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) the demand bit starts the sampler on the next tick");
        release_tx.send(()).expect("release #1");
        let (gen1, _) = lhm_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(P2) the demanded sample is published"),
        );
        assert_eq!(gen1, 1, "(P2) the event carries the loop generation");
        consumers.store(0, Ordering::Relaxed);
        loop_thread.thread().unpark();
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "(P1) demand release parks the loop again (zero further entries)"
        );
        consumers.store(consumer_bit("lhm"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) re-demand resumes sampling");
        release_tx.send(()).expect("release #2");
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the LHM loop joins within the sampler's loud 5 s budget");
    }

    #[test]
    fn telemetry_hwinfo_loop_zero_demand_parks_and_resume_samples_within_bound() {
        assert_injected_sampler_is_io_free("zd_hwinfo_sampler");
        let (enter_rx, release_tx) =
            install_scripted_sampler(&ZD_HWINFO_SAMPLER, vec![hwinfo_ok(1), hwinfo_ok(2)]);
        let config = Arc::new(RwLock::new(Config {
            hwinfo_cpu_temp_sensor: "CPU Exact".into(),
            hwinfo_cpu_temp_reading: "Package Temperature".into(),
            hwinfo_stale_after: Duration::from_millis(200),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_hwinfo(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            zd_hwinfo_sampler,
        );
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(1_600)).is_err(),
            "(P1) the real HWiNFO loop enters the sampler ZERO times across 1.6 s (>= the shipped 1 s EXTENDED_POLL cadence, whose first sample fires immediately under the keep-alive rule) while no consumer reads HWiNFO-backed readings"
        );
        consumers.store(consumer_bit("hwinfo"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) the demand bit starts the sampler on the next tick");
        release_tx.send(()).expect("release #1");
        let (gen1, selector, _) = hwinfo_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(P2) the demanded sample is published"),
        );
        assert_eq!(gen1, 1, "(P2) the event carries the loop generation");
        assert_eq!(
            selector, "CPU Exact",
            "(P2) selectors come from the live config"
        );
        consumers.store(0, Ordering::Relaxed);
        loop_thread.thread().unpark();
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(1_600)).is_err(),
            "(P1) demand release parks the loop again (zero further entries)"
        );
        consumers.store(consumer_bit("hwinfo"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) re-demand resumes sampling");
        release_tx.send(()).expect("release #2");
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the HWiNFO loop joins within the sampler's loud 5 s budget");
    }

    #[test]
    fn telemetry_gpu_loop_zero_demand_parks_and_resume_samples_within_bound() {
        assert_injected_sampler_is_io_free("zd_gpu_sampler");
        let (enter_rx, release_tx) =
            install_scripted_sampler(&ZD_GPU_SAMPLER, vec![gpu_ok(1), gpu_ok(2)]);
        let config = Arc::new(RwLock::new(Config {
            gpu_provider: "auto".into(),
            telemetry_interval: BEHAVIORAL_DRIVEN,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_gpu(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            zd_gpu_sampler,
        );
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "(P1) the real GPU loop enters the sampler ZERO times across 900 ms (>= 2 x the 400 ms driven cadence and >= the 3 s keep-alive floor's first-sample at t=0) while no consumer reads GPU-backed surfaces; with demand 0 the shipped disabled arm also keeps the vendor provider unloaded (no open chain)"
        );
        consumers.store(consumer_bit("gpu"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) the demand bit starts the sampler on the next tick");
        release_tx.send(()).expect("release #1");
        let (gen1, _) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(P2) the demanded sample is published"),
        );
        assert_eq!(gen1, 1, "(P2) the event carries the loop generation");
        consumers.store(0, Ordering::Relaxed);
        loop_thread.thread().unpark();
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "(P1) demand release parks the loop again (zero further entries)"
        );
        consumers.store(consumer_bit("gpu"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(P2) re-demand resumes sampling");
        release_tx.send(()).expect("release #2");
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the GPU loop joins within the sampler's loud 5 s budget");
    }

    /// WO-TEL-D3-B1 (L2, core tier) RED-FIRST falsifier of the silent-absence gap (the DG-2
    /// framing ruled by `WO-TEL-D3-A-AUDIT.md` §C4: config decides capability, and the
    /// directive-compatible default is a BOUNDED EXPLICIT-unavailable core row, never silence).
    /// When the gpu provider is parked (`gpu_provider off`), the four core rows
    /// `GPUUtilization`/`VRAMUtilization`/`VRAMUsed`/`VRAMTotal` are published as EXPLICIT
    /// unavailable, ONCE - the shipped publish-once shape (`providers/network.rs:210` +
    /// `241-253`, `providers/hang.rs:844` + `898-915`) applied here with the published row set
    /// itself as the dedupe, so no per-tick publish stream and no busy loop exists. At
    /// `ec3df0d` the disabled arm cleared the rows (`apply_freshness:1157`) = silent absence, so
    /// this test is RED at that baseline.
    #[test]
    fn gpu_disabled_park_publishes_explicit_core_unavailable_rows_once() {
        let cfg = Config {
            gpu_provider: "off".into(),
            ..Config::default()
        };
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        worker.apply_slow_event(
            SlowEvent::Gpu(
                1,
                Err("gpu core readings unavailable: native GPU provider disabled".into()),
            ),
            1,
            base,
        );
        let rows = worker.update.gpu_readings.clone();
        for key in [
            MetricKey::GPUUtilization,
            MetricKey::VRAMUtilization,
            MetricKey::VRAMUsed,
            MetricKey::VRAMTotal,
        ] {
            assert!(
                rows.iter().any(|reading| reading.key == key
                    && !reading.has_value
                    && reading.availability == Availability::Unavailable),
                "the parked gpu provider publishes an EXPLICIT unavailable core row for {key:?}, never silent absence"
            );
        }
        assert_eq!(
            rows.len(),
            4,
            "the core-unavailable publication is exactly the four core rows (no optional sensor rows ride it)"
        );
        let stamp = rows
            .iter()
            .find(|reading| reading.key == MetricKey::GPUUtilization)
            .and_then(|reading| reading.sampled_at)
            .expect("the explicit unavailable row carries its publication stamp");
        for tick in 1..=40 {
            worker.apply_freshness(&cfg, base + Duration::from_millis(50 * tick));
            assert_eq!(
                worker.update.gpu_readings.len(),
                4,
                "the core rows stay published across the park (tick {}), never cleared to silence",
                tick
            );
        }
        assert_eq!(
            worker
                .update
                .gpu_readings
                .iter()
                .find(|reading| reading.key == MetricKey::GPUUtilization)
                .and_then(|reading| reading.sampled_at),
            Some(stamp),
            "the park publishes ONCE: 40 coordinator ticks never re-stamp the core-unavailable rows (no per-tick publish stream)"
        );
        assert!(
            worker.update.gpu_readings.iter().all(|r| !r.has_value),
            "no core-unavailable row carries a value, so a parked provider can never present stale as fresh"
        );
    }

    /// WO-TEL-D3-B1 (L2) RED-FIRST seam proof on the REAL gpu loop through the shipped
    /// WO-TEL-C sampler seam: entering the park publishes exactly ONE explicit core-unavailable
    /// event carrying the loop's generation, and the sampler is entered ZERO times while parked.
    /// At `ec3df0d` the park arm (`telemetry.rs:1740-1743`) published nothing => RED there.
    /// Demand/gate state is driven directly (the shipped seam contract), so this proof is
    /// independent of the L1 layout rule.
    #[test]
    fn gpu_loop_park_publishes_one_core_unavailable_event() {
        assert_injected_sampler_is_io_free("pk_gpu_sampler");
        let (enter_rx, _release_tx) = install_scripted_sampler(&PK_GPU_SAMPLER, Vec::new());
        let config = Arc::new(RwLock::new(Config {
            gpu_provider: "off".into(),
            telemetry_interval: BEHAVIORAL_DRIVEN,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(consumer_bit("gpu")));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_gpu(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            pk_gpu_sampler,
        );
        let (generation_id, outcome) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(L2) the park publishes one explicit core-unavailable event"),
        );
        assert_eq!(
            generation_id, 1,
            "(L2) the park event carries the live generation"
        );
        assert!(
            outcome.is_err(),
            "(L2) the park event states the core-unavailable reason (an Err payload), not a sample"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "(L2) the park publishes ONCE: no repeat publish stream across >= 8 TICKs"
        );
        assert!(
            enter_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "(L2) the parked loop enters the sampler ZERO times (no busy loop)"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the GPU loop joins within the sampler's loud 5 s budget");
    }

    /// P-backoff (U-1): the demanded LHM loop facing consecutive provider
    /// failures grows its interval toward the keep-alive bound — the shipped
    /// inversion polled a failing endpoint at FULL cadence forever. Falsified
    /// by restoring the inversion (full cadence on failure -> entry count
    /// crosses the bound) and by dropping the backoff call.
    #[test]
    fn telemetry_lhm_loop_provider_outcome_backoff_is_bounded_by_keep_alive() {
        assert_injected_sampler_is_io_free("bo_lhm_sampler");
        let (enter_rx, release_tx) = install_scripted_sampler(&BO_LHM_SAMPLER, lhm_outcomes(40));
        let config = Arc::new(RwLock::new(Config {
            lhm_mode: "on".into(),
            lhm_url: "http://127.0.0.1:1/synthetic-lhm-never-dialed".into(),
            lhm_interval: Duration::from_millis(50),
            lhm_stale_after: Duration::from_millis(1_000),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(consumer_bit("lhm")));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(64);
        let loop_thread = spawn_lhm(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            bo_lhm_sampler,
        );
        let start = Instant::now();
        let mut entries = 0usize;
        while start.elapsed() < Duration::from_millis(3_500) {
            match enter_rx.recv_timeout(Duration::from_millis(1_200)) {
                Ok(()) => {
                    entries += 1;
                    release_tx.send(()).expect("release ladder step");
                }
                Err(_) => break,
            }
        }
        // Ladder with base 50 ms, bound 1000 ms: 0,50,150,350,750,1750,2750,3500
        // = 8 entries in 3.5 s (loop tick adds slack). The unbacked cadence
        // (shipped inversion shape) enters ~70 times in the same window.
        assert!(
            (3..=14).contains(&entries),
            "(U-1) consecutive sampler failures back off within the keep-alive bound: {entries} entries in 3.5 s at a 50 ms full cadence (bounds: >=3 proves sampling continues; <=14 proves the bound)"
        );
        // The published Err stream keeps the current generation (diagnostics
        // path unchanged).
        let (gen, _) = lhm_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("the ladder publishes its Err outcomes"),
        );
        assert_eq!(
            gen, 1,
            "the failed-outcome events carry the live generation"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the LHM loop joins within the sampler's loud 5 s budget");
    }

    /// P-backoff (U-10): the same ladder bounds the GPU vendor re-open chain.
    #[test]
    fn telemetry_gpu_loop_provider_outcome_backoff_is_bounded_by_keep_alive() {
        assert_injected_sampler_is_io_free("bo_gpu_sampler");
        let (enter_rx, release_tx) = install_scripted_sampler(&BO_GPU_SAMPLER, gpu_outcomes(80));
        let config = Arc::new(RwLock::new(Config {
            gpu_provider: "auto".into(),
            telemetry_interval: Duration::from_millis(50),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(consumer_bit("gpu")));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(64);
        let loop_thread = spawn_gpu(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            bo_gpu_sampler,
        );
        let start = Instant::now();
        let mut entries = 0usize;
        while start.elapsed() < Duration::from_millis(3_500) {
            match enter_rx.recv_timeout(Duration::from_millis(1_200)) {
                Ok(()) => {
                    entries += 1;
                    release_tx.send(()).expect("release ladder step");
                }
                Err(_) => break,
            }
        }
        // gpu_stale_after = max(3 x 50 ms, 3 s) = 3 s: ladder 0,50,150,350,750,
        // 1750,3500 = 7 entries in 3.5 s; unbacked = ~70.
        assert!(
            (3..=14).contains(&entries),
            "(U-10) consecutive vendor-open failures back off within the staleness bound: {entries} entries in 3.5 s at a 50 ms full cadence"
        );
        let (gen, _) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("the ladder publishes its Err outcomes"),
        );
        assert_eq!(
            gen, 1,
            "the failed-outcome events carry the live generation"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the GPU loop joins within the sampler's loud 5 s budget");
    }

    /// P-backoff (U-1/U-10/U-12 shared mechanism): the pure ladder.
    #[test]
    fn provider_outcome_backoff_is_exponential_bounded_and_never_faster_than_configured() {
        let base = Duration::from_millis(300);
        let bound = Duration::from_secs(3);
        assert_eq!(
            backoff_interval(base, bound, 0),
            base,
            "healthy keeps full cadence"
        );
        assert_eq!(backoff_interval(base, bound, 1), Duration::from_millis(600));
        assert_eq!(
            backoff_interval(base, bound, 2),
            Duration::from_millis(1_200)
        );
        assert_eq!(
            backoff_interval(base, bound, 3),
            Duration::from_millis(2_400)
        );
        assert_eq!(
            backoff_interval(base, bound, 4),
            bound,
            "4.8 s clamps to the provider's own staleness bound"
        );
        assert_eq!(
            backoff_interval(base, bound, 64),
            bound,
            "a long failure streak can never outrun the bound, and the clamped shift never wraps"
        );
        assert_eq!(
            backoff_interval(base, Duration::from_millis(100), 3),
            base,
            "a bound below the configured cadence leaves the cadence intact"
        );
    }

    /// P1+P2 (coordinator, gated set): connections, system battery,
    /// performance — with demand 0 the polls record no attempt (no
    /// `GetTcpTable2`, no `GetSystemPowerStatus`, no PDH open/collect) and
    /// publish no provider health rows; demanding a reading samples on the same
    /// synthetic tick (resume `due` fires on the stale stamp).
    #[test]
    fn zero_demand_gated_coordinator_polls_sample_never_and_demand_samples_immediately() {
        let cfg = Config::default();
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        let consumers = 0u64;
        for step in 1..=12u32 {
            let now = base + Duration::from_secs(step as u64);
            worker.poll_connections(&cfg, consumers, now);
            worker.poll_battery(&cfg, consumers, now);
            worker.poll_performance(&cfg, consumers, now);
            assert!(
                worker.connections_attempt.is_none()
                    && worker.battery_attempt.is_none()
                    && worker.performance_attempt.is_none(),
                "P1: no demand records no attempt (step {step})"
            );
            assert!(
                worker
                    .update
                    .providers
                    .iter()
                    .all(|(name, _)| name != "connections"
                        && name != "system battery"
                        && name != "performance"),
                "P1: no demand touches no provider health row (step {step})"
            );
        }
        let demanded = consumer_bit("connections")
            | consumer_bit("system battery")
            | consumer_bit("performance");
        let now = base + Duration::from_secs(13);
        worker.poll_connections(&cfg, demanded, now);
        worker.poll_battery(&cfg, demanded, now);
        worker.poll_performance(&cfg, demanded, now);
        assert!(
            worker.connections_attempt.is_some()
                && worker.battery_attempt.is_some()
                && worker.performance_attempt.is_some(),
            "P2: the demand stamp makes every gated poll due on its first demanded call"
        );
        assert!(
            worker
                .update
                .providers
                .iter()
                .any(|(name, _)| name == "connections"),
            "P2: the demanded connections poll runs (real `GetTcpTable2` on this host)"
        );
    }

    /// P1+P2 (coordinator, extended set): controller (xinput), audio (COM),
    /// netif (`GetIfTable2`) — zero demand records no attempt and publishes no
    /// health rows; demand samples on the first tick.
    #[test]
    fn zero_demand_metering_coordinator_polls_sample_never_and_demand_samples_immediately() {
        let cfg = Config::default();
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        for step in 1..=12u32 {
            let now = base + Duration::from_secs(step as u64);
            worker.poll_controller(&cfg, 0, now);
            worker.poll_audio(&cfg, 0, now);
            worker.poll_network(&cfg, 0, now);
            assert!(
                worker.controller_attempt.is_none()
                    && worker.audio_attempt.is_none()
                    && worker.net_attempt.is_none(),
                "P1: no demand records no attempt (step {step})"
            );
            assert!(
                worker
                    .update
                    .providers
                    .iter()
                    .all(|(name, _)| name != "controller" && name != "audio" && name != "net"),
                "P1: no demand touches no provider health row (step {step})"
            );
        }
        let demanded = extended_consumer_bit("controller")
            | extended_consumer_bit("audio")
            | extended_consumer_bit("net");
        let now = base + Duration::from_secs(13);
        worker.poll_controller(&cfg, demanded, now);
        worker.poll_audio(&cfg, demanded, now);
        worker.poll_network(&cfg, demanded, now);
        assert!(
            worker.controller_attempt.is_some()
                && worker.audio_attempt.is_some()
                && worker.net_attempt.is_some(),
            "P2: the demand stamp makes every metering poll due on its first demanded call (U-11 persistence-stop, U-2/U-7)"
        );
    }

    /// P4: a poll-stopped gated provider publishes STALE-marked values and
    /// never returns to `Current` while demand stays 0 — the shipped
    /// `apply_freshness` stamps from `last_success`, so zero-poll cannot make
    /// an old value look fresh (this is the invariant the D2 gate relies on).
    #[test]
    fn gated_poll_stops_publish_stale_marked_and_never_returns_to_current() {
        let cfg = Config::default();
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        let demanded = consumer_bit("connections") | consumer_bit("system battery");
        worker.poll_connections(&cfg, demanded, base);
        worker.poll_battery(&cfg, demanded, base);
        worker.apply_freshness(&cfg, base);
        // Demand release: attempts frozen, successes age past the bounds.
        for step in 1..=8u32 {
            let later = base + Duration::from_secs(step as u64 * 4);
            worker.poll_connections(&cfg, 0, later);
            worker.poll_battery(&cfg, 0, later);
            worker.apply_freshness(&cfg, later);
            if let Some(reading) = &worker.connection_reading {
                let expected = if step >= 1 {
                    // The first poll succeeded at `base`; from base+4 s the
                    // 3 s EXTENDED_STALE bound has passed.
                    Freshness::Stale
                } else {
                    Freshness::Current
                };
                assert_eq!(
                    reading.freshness,
                    expected,
                    "P4: the stopped connections reading stamps {expected:?} at +{} s and never returns to Current",
                    step * 4
                );
            }
            if worker.update.system_battery.sampled_at.is_some() {
                let expected = if step * 4 > 15 {
                    Freshness::Stale
                } else {
                    Freshness::Current
                };
                assert_eq!(
                    worker.update.system_battery.freshness,
                    expected,
                    "P4: the stopped battery reading follows the shipped 15 s stamp at +{} s",
                    step * 4
                );
            }
        }
        let late = base + Duration::from_secs(120);
        worker.poll_connections(&cfg, 0, late);
        worker.poll_battery(&cfg, 0, late);
        worker.apply_freshness(&cfg, late);
        if let Some(reading) = &worker.connection_reading {
            assert_eq!(
                reading.freshness,
                Freshness::Stale,
                "P4: 2 minutes without demand keeps the reading Stale — stale never fresh again"
            );
        }
        if worker.update.system_battery.sampled_at.is_some() {
            assert_eq!(
                worker.update.system_battery.freshness,
                Freshness::Stale,
                "P4: the system battery reading stamps Stale past its 15 s bound and stays Stale"
            );
        }
    }

    /// P3: two consumers of one provider (the THERMALS slot pair of displayed
    /// readings AND a second reading-keyed consumer of the same sensors map to the
    /// SAME single demand bit) resolve to exactly ONE collection op per interval: the
    /// shared-`scheduled_samples` count equals the single-consumer formula.
    /// WO-REMOVE-ALERTS: the shipped second consumer was the enabled temperature alert
    /// category; the equality being pinned is the bit sharing, which is what survives
    /// (the retired alert leg is gone, so the display demand is the only producer).
    #[test]
    fn two_consumers_of_one_provider_resolve_to_one_collection_op() {
        let cfg = Config::default();
        let horizon = Duration::from_secs(300);
        let full = cfg.lhm_interval;
        let gated = scheduled_samples(&cfg, consumer_bit("lhm"), "lhm", full, horizon, None);
        assert_eq!(
            gated.len(),
            (horizon.as_millis() / full.as_millis()) as usize + 1,
            "P3: one demanded provider samples exactly horizon/full+1 times — every display consumer of that provider shares this single op"
        );
        // The app's derivation collapses both consumers onto that one bit
        // (`app.rs` tests pin the union equality; here the word-level sharing):
        let display = consumer_bit("hwinfo") | consumer_bit("lhm") | consumer_bit("gpu");
        let second = consumer_bit("hwinfo") | consumer_bit("lhm") | consumer_bit("gpu");
        assert_eq!(
            display | second,
            display,
            "P3: a second consumer of the same providers is the same bits as the display demand — no second op exists"
        );
        // Extended sharing: the AUDIO and MIC_STATUS readings share one audio op.
        let audio_bit = extended_consumer_bit("audio");
        assert_ne!(audio_bit, 0, "audio carries an extended demand bit");
        assert_eq!(
            sampling_interval(&cfg, audio_bit, "audio", EXTENDED_POLL),
            EXTENDED_POLL,
            "P3: the extended word never enters the gated math — one audio op, no EC-3 rate effect"
        );
    }

    /// S1 bit registry shape: the extended bits sit directly above the shipped
    /// 7, never collide with `consumer_bit`, and name the four metering
    /// providers of the ratified list.
    #[test]
    fn extended_consumer_bits_sit_above_the_gated_seven_without_collision() {
        assert_eq!(GATED_PROVIDERS.len(), 7, "the shipped gated word");
        assert_eq!(ALL_CONSUMERS, 0b111_1111);
        assert_eq!(ALL_EXTENDED_CONSUMERS, 0b1111 << 7);
        for provider in EXTENDED_CONSUMER_PROVIDERS {
            let bit = extended_consumer_bit(provider);
            assert_ne!(bit, 0, "{provider} has an extended demand bit");
            assert_eq!(
                consumer_bit(provider),
                0,
                "{provider} is not a gated provider (shipped pins intact)"
            );
            assert_eq!(
                bit & ALL_CONSUMERS,
                0,
                "{provider} bit is above the gated range"
            );
            assert!(
                demanded(ALL_EXTENDED_CONSUMERS, provider),
                "{provider} demanded by the extended-all word"
            );
            assert!(
                !demanded(ALL_CONSUMERS, provider),
                "{provider} not demanded by the gated-all word"
            );
            assert!(!demanded(0, provider), "{provider} parks at demand 0");
        }
        for provider in GATED_PROVIDERS {
            assert!(demanded(ALL_CONSUMERS, provider));
            assert!(
                !demanded(ALL_EXTENDED_CONSUMERS, provider),
                "{provider} has no extended bit"
            );
            assert!(!demanded(0, provider));
        }
        // Mandatory lanes carry no demand key at all: no word can gate them.
        for provider in ["hang", "network probe", "clock", "cpu", "memory"] {
            assert_eq!(consumer_bit(provider), 0);
            assert_eq!(extended_consumer_bit(provider), 0);
            assert!(
                !demanded(u64::MAX, provider),
                "{provider} is never demand-gated"
            );
        }
    }

    /// U-8 proof A (behavioral): the coordinator publishes on change-or-due,
    /// not per 50 ms tick. With a 5 s `telemetry_interval` a second instant
    /// `tick()` publishes NOTHING (the shipped 20 Hz stream would), a generation
    /// bump publishes IMMEDIATELY (the due gate never delays invalidation), and
    /// an applied slow event publishes on its tick.
    #[test]
    fn coordinator_publishes_on_change_or_due_not_every_tick() {
        let cfg = Config {
            safe_mode: true, // isolates the publish gate: the tick short-circuits
            telemetry_interval: Duration::from_secs(5),
            ..Config::default()
        };
        let config = Arc::new(RwLock::new(cfg.clone()));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, updates) = latest_channel();
        let (_slow_tx, slow_rx) = mpsc::sync_channel(1);
        let (_presentmon_tx, presentmon_rx) = mpsc::channel();
        let (_hang_tx, hang_rx) = mpsc::channel();
        let (_network_tx, network_rx) = mpsc::channel();
        let mut worker = Worker {
            config,
            config_generation: Arc::clone(&generation),
            request_cache: (0u64, Arc::new(cfg.clone())),
            consumers: Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            tx,
            slow_rx,
            update: Update {
                generation: 1,
                ..Default::default()
            },
            net: netif::NetProvider::new(),
            performance: performance::Provider::new(),
            headset_success: None,
            lhm_success: None,
            gpu_success: None,
            performance_success: None,
            connections_success: None,
            battery_success: None,
            hwinfo_success: None,
            network_success: None,
            presentmon_success: None,
            controller_attempt: None,
            audio_attempt: None,
            net_attempt: None,
            performance_attempt: None,
            connections_attempt: None,
            battery_attempt: None,
            performance_failures: 0,
            presentmon_rx,
            hang_rx,
            network_rx,
            hwinfo_cpu_temp: Metric::default(),
            lhm_cpu_temp: Metric::default(),
            native_gpu_temp: Metric::default(),
            lhm_gpu_temp: Metric::default(),
            gpu_backend: String::new(),
            native_gpu_power: None,
            lhm_readings: Vec::new(),
            hwinfo_readings: Vec::new(),
            performance_readings: Vec::new(),
            connection_reading: None,
            hwinfo_diagnostic: None,
            lhm_diagnostic: None,
            generation: 1,
            safe_mode_active: cfg.safe_mode,
            publish_dirty: true,
            last_publish: None,
        };
        assert!(
            worker.tick(),
            "the first tick publishes (reset-due) and keeps the loop alive"
        );
        assert!(
            updates.try_recv().is_ok(),
            "U-8: the first tick's publish is drained"
        );
        assert!(
            worker.tick(),
            "the second tick keeps the loop alive with no change"
        );
        assert!(
            matches!(updates.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "U-8: an instant second tick publishes NOTHING (the shipped 20 Hz stream published every tick)"
        );
        generation.fetch_add(1, Ordering::SeqCst);
        assert!(worker.tick());
        let update = updates
            .try_recv()
            .expect("U-8: a generation change publishes IMMEDIATELY (change, not due)");
        assert_eq!(
            update.generation, 2,
            "U-8: the generation stamp rides the publish (invalidation semantics untouched)"
        );
        assert!(
            worker.tick(),
            "the third tick after the change keeps the loop alive"
        );
        assert!(
            worker.tick(),
            "the fourth tick after the change keeps the loop alive"
        );
        assert!(
            matches!(updates.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "U-8: the new generation settles quiet again inside the 5 s due window"
        );
    }

    /// U-8 proof B (structural): the five hot 50 ms sites read through the
    /// generation-guarded cache (no steady-state `Config` clone); the shipped
    /// `current_request` stays for the slow sites, and the publish site is the
    /// single gated `publish` call.
    #[test]
    fn telemetry_loops_read_a_cached_request_under_an_unchanged_generation() {
        for fn_head in [
            "fn spawn_headset(",
            "fn spawn_lhm(",
            "fn spawn_hwinfo(",
            "fn spawn_gpu(",
        ] {
            let loop_src = shipped_loop_source(fn_head);
            assert_eq!(
                loop_src
                    .matches("let (generation, cfg) = cached_request(&config, &generation, &mut request);")
                    .count(),
                1,
                "{fn_head}: exactly one cached loop-head request read"
            );
            assert_eq!(
                loop_src
                    .matches("current_request(&config, &generation)")
                    .count(),
                0,
                "{fn_head}: no per-tick full-Config clone at the loop head"
            );
            assert!(
                loop_src.contains("let mut request = (0u64, Arc::new(current_config(&config)));"),
                "{fn_head}: the cache is seeded from the shipped `current_config`"
            );
        }
        let source = include_str!("telemetry.rs").replace("\r\n", "\n");
        let tick = source
            .find("fn tick(&mut self) -> bool {")
            .expect("the coordinator tick exists");
        let tick_body = &source[tick..tick + source[tick..].find("\n    fn ").unwrap()];
        assert_eq!(
            tick_body.matches("&mut self.request_cache").count(),
            1,
            "U-8: the coordinator tick reads the cache (rustfmt-wrapped args included)"
        );
        assert!(
            tick_body.contains("cached_request("),
            "U-8: the tick's request read is the cached primitive"
        );
        assert_eq!(
            tick_body.matches("self.tx.send(").count(),
            0,
            "U-8: the tick no longer sends an Update clone unconditionally"
        );
        assert_eq!(
            tick_body
                .matches("self.publish(generation, &cfg, now)")
                .count(),
            2,
            "U-8: the safe-mode and normal arms publish through the one gated site"
        );
        assert!(
            source.contains("if !self.publish_dirty && !due {"),
            "U-8: the change-or-due gate is the single publish predicate (in the `publish` fn)"
        );
    }

    /// The shipped text of one `Worker` method (LF-normalized), from its
    /// indented signature up to the next `fn` at the same indentation.
    fn shipped_method_source(fn_head: &str) -> String {
        let source = include_str!("telemetry.rs").replace("\r\n", "\n");
        let start = source
            .find(fn_head)
            .expect("the shipped coordinator poll exists");
        let rest = &source[start..];
        let end = ["\n    fn ", "\n    pub fn "]
            .iter()
            .filter_map(|needle| rest[1..].find(*needle).map(|p| p + 1))
            .min()
            .expect("the shipped poll is followed by a method");
        rest[..end].to_string()
    }

    /// WO-TEL-D2 STRUCTURAL pin (TEL-B `include_str!` occurrence form) of the
    /// coordinator poll arms: every demand-gated poll carries its zero-demand
    /// arm exactly once, and the U-12 PDH re-open backoff wiring sits inside
    /// `poll_performance`. Probes: removing a demand gate (P-idle) or
    /// restoring the fixed EXTENDED_POLL cadence over the failure streak (P16,
    /// inversion-restored) breaks an assert of this pin.
    #[test]
    fn coordinator_poll_demand_gates_and_pdh_backoff_wiring_are_structural() {
        for (fn_head, provider) in [
            (
                "    fn poll_controller(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
                "controller",
            ),
            (
                "    fn poll_audio(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
                "audio",
            ),
            (
                "    fn poll_network(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
                "net",
            ),
            (
                "    fn poll_performance(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
                "performance",
            ),
            (
                "    fn poll_connections(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
                "connections",
            ),
            (
                "    fn poll_battery(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
                "system battery",
            ),
        ] {
            let src = shipped_method_source(fn_head);
            let gate = format!("if !demanded(consumers, \"{provider}\") {{");
            assert_eq!(
                src.matches(&gate).count(),
                1,
                "{provider}: one zero-demand branch"
            );
            let gate_start = src.find(&gate).unwrap();
            let gate_end = gate_start + src[gate_start..].find("\n        }").unwrap();
            assert!(
                src[gate_start..gate_end].contains("return;"),
                "{provider}: no-demand exits before native collection"
            );
            assert!(
                gate_end < src.find("if !due(").unwrap(),
                "{provider}: demand gate precedes cadence and sampling"
            );
            assert!(
                src.contains("if !due("),
                "{provider}: the shipped cadence gate survives behind the demand arm"
            );
        }
        let perf = shipped_method_source(
            "    fn poll_performance(&mut self, cfg: &Config, consumers: u64, now: Instant) {",
        );
        assert_eq!(
            perf
                .matches("backoff_interval(EXTENDED_POLL, EXTENDED_STALE, self.performance_failures)")
                .count(),
            1,
            "U-12: the PDH re-open chain (query drop per failure) is bounded by the provider-outcome backoff"
        );
        assert_eq!(
            perf.matches(
                "self.performance_failures = self.performance_failures.saturating_add(1);"
            )
            .count(),
            1,
            "U-12: sample failures grow the streak; success arms reset it"
        );
        assert_eq!(
            perf.matches("self.performance_failures = 0;").count(),
            2,
            "U-12: both success arms (`Some` and baseline `None`) reset the streak"
        );
    }

    /// WO-TEL-D2 (S1, publish-site wiring pin): the demand word reaches the
    /// published consumers value at BOTH app sites. The four
    /// `app::tests::reading_demand_*` tests pin the derivation (the pure
    /// function); this pin covers the wiring, which the P6 registry probes
    /// (P-reg-render / P-reg-seed) found unpinned - dropping the
    /// `| provider_demand_bits(..)` term at either site makes one count drop,
    /// while the shipped 7-bit call keeps its own shape.
    #[test]
    fn registry_publish_sites_wire_the_demand_word_structurally() {
        let app = include_str!("../src/app.rs");
        assert_eq!(
            app.matches("telemetry_runtime.set_consumers(").count(),
            2,
            "S1: exactly the two shipped publish sites write the consumers word"
        );
        assert_eq!(
            app.matches(") | provider_demand_bits(&slots, &seed_modules, &cfg),")
                .count(),
            1,
            "S1: the seed publish site ORs the demand word into the shipped call"
        );
        assert_eq!(
            app.matches(") | provider_demand_bits(&slots, &slot_modules, &cfg),")
                .count(),
            1,
            "S1: the render publish site ORs the demand word into the shipped call"
        );
        assert!(
            app.contains(
                "set_consumers(\n        provider_consumer_bits(\n            &seed_modules,"
            ),
            "S1: the seed publish site keeps the shipped gated derivation as the left operand"
        );
        assert!(
            app.contains("set_consumers(\n                provider_consumer_bits(\n                    &slot_modules,"),
            "S1: the render publish site keeps the shipped gated derivation as the left operand"
        );
    }

    /// Demand release explicitly clears rate/audio/controller payloads and their
    /// cadence stamps. Publish that change once; reactivation cannot carry a
    /// hidden-interval network rate or delay behind the previous generation.
    #[test]
    fn parked_extended_providers_publish_unavailable_once_and_rearm() {
        let cfg = Config::default();
        let mut worker = test_worker(cfg.clone());
        let base = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        worker.audio_attempt = Some(base);
        worker.net_attempt = Some(base);
        worker.controller_attempt = Some(base);
        worker.update.audio = Some(audio::AudioState {
            volume_percent: 75.0,
            mic_known: true,
            mic_muted: false,
        });
        worker.update.audio_sampled_at = Some(at);
        worker.update.net = Some(netif::NetThroughput {
            in_bps: 100.0,
            out_bps: 200.0,
            sampled_at: at,
        });
        worker.update.controller = ControllerBattery {
            connected: true,
            percent: 75,
            type_name: "BATTERY".into(),
            ..Default::default()
        };
        worker.publish_dirty = false;
        for step in 1..=8u32 {
            let later = base + Duration::from_millis(step as u64);
            worker.poll_audio(&cfg, 0, later);
            worker.poll_network(&cfg, 0, later);
            worker.poll_controller(&cfg, 0, later);
            worker.apply_freshness(&cfg, later);
            assert_eq!(worker.audio_attempt, None, "park clears audio cadence");
            assert_eq!(worker.net_attempt, None, "park clears netif cadence");
            assert_eq!(
                worker.controller_attempt, None,
                "park clears controller cadence"
            );
            assert!(worker.update.audio.is_none());
            assert!(worker.update.audio_sampled_at.is_none());
            assert!(worker.update.net.is_none());
            assert!(!worker.update.controller.connected);
            assert_eq!(worker.update.controller.percent, 0);
            assert_eq!(
                worker.publish_dirty,
                step == 1,
                "only entering park publishes a change"
            );
            worker.publish(1, &cfg, later);
            assert!(!worker.publish_dirty);
        }
        let demand = extended_consumer_bit("audio")
            | extended_consumer_bit("net")
            | extended_consumer_bit("controller");
        let resumed = base + Duration::from_millis(9);
        worker.poll_audio(&cfg, demand, resumed);
        worker.poll_network(&cfg, demand, resumed);
        worker.poll_controller(&cfg, demand, resumed);
        assert_eq!(
            worker.audio_attempt,
            Some(resumed),
            "audio re-arms before its old deadline"
        );
        assert_eq!(
            worker.net_attempt,
            Some(resumed),
            "netif re-arms before its old deadline"
        );
        assert_eq!(
            worker.controller_attempt,
            Some(resumed),
            "controller re-arms before its old deadline"
        );
        assert!(
            worker.update.net.is_none(),
            "first resumed rate sample only establishes a new baseline"
        );
        // Freshness processing cannot restore retired payloads behind the park gate.
        // The independent PresentMon unavailable-publication checks below are retained.
        let freshness =
            shipped_method_source("fn apply_freshness(&mut self, cfg: &Config, now: Instant) {");
        assert!(
            freshness.len() > 500,
            "R-D2-1: the slicer found the shipped stamps"
        );
        assert_eq!(
            freshness.matches("self.update.audio").count(),
            0,
            "R-D2-1: no audio freshness writer exists (the stamp is the published `sampled_at`)"
        );
        assert_eq!(
            freshness.matches("self.update.net").count(),
            0,
            "R-D2-1: no netif freshness writer exists (the stamp is the published `sampled_at`)"
        );
        assert_eq!(
            freshness.matches("self.update.controller").count(),
            0,
            "R-D2-1: no controller freshness writer exists"
        );
        // presentmon leg (structural, the shipped pin form): the park publishes
        // the deduped unavailable row and never reaches the fresh-sample path, so
        // a parked presentmon can publish no fresh game reading.
        let source = include_str!("providers/presentmon.rs").replace("\r\n", "\n");
        let start = source.find("fn run(\n").expect("the shipped worker exists");
        let rest = &source[start..];
        let end = rest
            .find("\n    let mut stderr = String::new();")
            .expect("the shipped loop-end marker closes the worker loop");
        let run = &rest[..end];
        assert_eq!(
            run.matches("capture.unavailable(").count(),
            1,
            "R-D2-1: the park publishes exactly one unavailable row (health=false, the readings are never marked fresh)"
        );
        assert!(
            run.contains("\"no display consumer\""),
            "R-D2-1: the parked presentmon names the demand reason on the record it publishes"
        );
        assert_eq!(
            run.matches("capture.reconcile(").count(),
            1,
            "R-D2-1: the only fresh-sample path is the single reconcile call behind the demand gate"
        );
    }
    #[test]
    fn parked_network_clears_rate_and_attempt_before_reactivation() {
        let cfg = Config::default();
        let mut worker = test_worker(cfg.clone());
        let now = Instant::now();
        worker.net_attempt = Some(now);
        worker.update.net = Some(netif::NetThroughput {
            in_bps: 123.0,
            out_bps: 456.0,
            sampled_at: SystemTime::now(),
        });
        worker.poll_network(&cfg, 0, now);
        assert!(worker.net_attempt.is_none());
        assert!(worker.update.net.is_none());
        worker.poll_network(&cfg, extended_consumer_bit("net"), now);
        assert!(worker.net_attempt.is_some());
        assert!(
            worker.update.net.is_none(),
            "reactivation establishes a baseline, not a hidden-interval rate"
        );
    }

    #[test]
    fn unchanged_unpublished_tick_does_not_rebuild_derived_readings() {
        let cfg = Config::default();
        let mut worker = test_worker(cfg);
        worker.consumers.store(0, Ordering::Relaxed);
        worker.publish_dirty = false;
        worker.last_publish = Some(Instant::now());
        worker
            .update
            .extended_readings
            .push(Reading::current_number(
                MetricKey::CPUPower,
                ValueKind::Watts,
                50.0,
                "test-retained",
                SystemTime::now(),
            ));
        assert!(worker.tick());
        assert_eq!(
            worker.update.extended_readings.len(),
            1,
            "no reconstruction before a publish"
        );
        worker.last_publish = None;
        let _ = worker.tick(); // receiver is intentionally absent in this fixture
        assert!(
            worker.update.extended_readings.is_empty(),
            "due publication reconciles the inputs"
        );
    }

    #[test]
    fn telemetry_shutdown_wakes_disabled_parked_workers() {
        let cfg = Config {
            safe_mode: true,
            ..Config::default()
        };
        let runtime = spawn(&cfg, 0);
        // Drop must wake the native park tokens even when no configuration changed.
        drop(runtime);
    }

    // WO-U46I (#16) park republication + (#3b) presentmon coordinator freshness. The park proof
    // runs the REAL `spawn_gpu` body through the shipped sampler seam (only the sampler injected,
    // WO-TEL-C pattern, every budget a bounded `recv_timeout`); the freshness proof runs the
    // production `apply_presentmon_update` + `apply_freshness` path on synthetic `now`s, the same
    // shape as `published_flag_record` (WO-TEL-D2).

    static PARKGEN_GPU_SAMPLER: OnceLock<ScriptedSampler<Result<gpu::Sample, String>>> =
        OnceLock::new();

    fn parkgen_gpu_sampler(
        _provider: &mut gpu::Provider,
        _selector: &str,
    ) -> Result<gpu::Sample, String> {
        sampled_in_slot(&PARKGEN_GPU_SAMPLER)
    }

    /// WO-U46I (#16) PRE-RED. A config apply while the gpu loop is PARKED bumps the generation,
    /// and `reset_state` clears the published core-unavailable row set, so the park must restate
    /// the explicit unavailable rows for the NEW generation. Pre-fix the generation branch cleared
    /// `last_attempt` only, so a parked loop went SILENT for the new generation (the silent
    /// absence the WO-TEL-D3-B1 publish-once rule was built to remove) => RED: step (iii) times
    /// out. The same-generation publish-once rule, the stale-generation rejection and the
    /// demand-return re-arm are asserted to stay as accepted.
    #[test]
    fn telemetry_gpu_park_republishes_after_a_generation_bump() {
        assert_injected_sampler_is_io_free("parkgen_gpu_sampler");
        let (enter_rx, release_tx) = install_scripted_sampler(
            &PARKGEN_GPU_SAMPLER,
            vec![Ok(gpu::Sample {
                backend: "synthetic-park-resume".into(),
                readings: Vec::new(),
                temperature: Metric::invalid(),
                power_w: None,
                sampled_at: marker_time(7),
            })],
        );
        let config = Arc::new(RwLock::new(Config {
            // The park here is the DEMAND park (`consumers = 0`), with the provider itself
            // enabled, so step (vi) can prove normal collection resumes.
            gpu_provider: "nvapi".into(),
            telemetry_interval: BEHAVIORAL_DRIVEN,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_gpu(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::clone(&consumers),
            Arc::clone(&shutdown),
            tx,
            parkgen_gpu_sampler,
        );
        let (gen1, outcome1) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(i) the park publishes its explicit core-unavailable event"),
        );
        assert_eq!(gen1, 1, "(i) the park event carries the live generation");
        assert!(
            outcome1.is_err(),
            "(i) the park event states the core-unavailable reason (an Err payload)"
        );
        assert!(
            rx.recv_timeout(BEHAVIORAL_SUPPRESS_WINDOW).is_err(),
            "(ii) one publication per park transition: an unchanged generation stays quiet"
        );
        // (iii) the config apply while parked: `update_config` bumps the generation.
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        let (gen2, outcome2) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(iii) the bump re-arms the park statement for the new generation"),
        );
        assert_eq!(
            gen2, 2,
            "(iii) the republished park event carries the NEW generation"
        );
        assert!(
            outcome2.is_err(),
            "(iii) the republished event is the explicit unavailable statement"
        );
        // (iv) repeated applies repeat the statement, each on its own generation.
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        let (gen3, outcome3) =
            gpu_event(rx.recv_timeout(BEHAVIORAL_WAIT).expect(
                "(iv) a second bump republishes again (no lifetime lockout of the park row)",
            ));
        assert_eq!(gen3, 3, "(iv) the second republish carries generation 3");
        assert!(
            outcome3.is_err(),
            "(iv) still the explicit unavailable statement"
        );
        // (v) the shipped stale-generation rejection is preserved on the coordinator side.
        let mut worker = test_worker(Config::default());
        let stale_event = SlowEvent::Gpu(
            99,
            Ok(gpu::Sample {
                backend: "synthetic-stale-generation".into(),
                readings: Vec::new(),
                temperature: Metric::invalid(),
                power_w: None,
                sampled_at: marker_time(8),
            }),
        );
        worker.apply_slow_event(stale_event, 3, Instant::now());
        assert!(
            worker.update.gpu_readings.is_empty(),
            "(v) a stale-generation park/sample event is still rejected by the generation guard"
        );
        // (vi) demand returns: normal collection resumes through the real loop.
        consumers.store(consumer_bit("gpu"), Ordering::Relaxed);
        loop_thread.thread().unpark();
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(vi) the demand-return re-arm enters the sampler");
        release_tx.send(()).expect("(vi) release the scripted Ok");
        let (gen4, outcome4) = gpu_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(vi) the resumed loop publishes a real sample"),
        );
        assert_eq!(
            gen4, 3,
            "(vi) the resumed sample carries the live generation"
        );
        assert_eq!(
            outcome4
                .expect("(vi) the payload is the injected Ok sample")
                .0,
            "synthetic-park-resume",
            "(vi) parking does not break the collection path"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the GPU loop joins within the sampler's loud 5 s budget");
    }

    /// WO-U46I (#3b) PRE-RED (the FPS-stays-fresh defect): presentmon joins the coordinator's
    /// success-stamp + `apply_freshness` contract. Production path, driven with synthetic `now`s
    /// exactly as `gpu_success` is driven in `published_flag_record`: an accepted,
    /// current-generation publication stamps success; worker SILENCE past the provider's own
    /// bound marks the displayed FPS / 1%-low / 0.1%-low / frame-time rows stale, and past twice
    /// that bound unavailable (the provider's own `stale_projection` doubling); a stale-generation
    /// publication and an `available: false` (empty/park) publication never advance the stamp; a
    /// later accepted publication makes the rows fresh again. Pre-fix there is NO presentmon stamp
    /// and `apply_freshness` never touches `update.game`, so a dead worker's last FPS keeps
    /// displaying fresh forever => RED at step (ii).
    #[test]
    fn presentmon_metrics_fresh_stale_unavailable_on_worker_silence() {
        let cfg = Config::default();
        let bound = cfg
            .presentmon_interval
            .saturating_mul(3)
            .max(Duration::from_secs(3));
        let mut worker = test_worker(cfg.clone());
        let game = |worker: &Worker| {
            [
                worker.update.game.fps,
                worker.update.game.one_percent,
                worker.update.game.point_one_low,
                worker.update.game.frame_time_ms,
            ]
        };
        let update = |generation: u64, available: bool| presentmon::Update {
            generation,
            available,
            game: GameStats {
                active: true,
                process_name: "game.exe".into(),
                fps: Metric::valid(60.0, SystemTime::UNIX_EPOCH),
                one_percent: Metric::valid(55.0, SystemTime::UNIX_EPOCH),
                point_one_low: Metric::valid(50.0, SystemTime::UNIX_EPOCH),
                frame_time_ms: Metric::valid(16.7, SystemTime::UNIX_EPOCH),
                ..Default::default()
            },
            ..Default::default()
        };
        let base = Instant::now();
        worker.apply_presentmon_update(update(1, true), 1, base);
        worker.apply_freshness(&cfg, base);
        let stamped = worker
            .presentmon_success
            .expect("(i) an accepted current-generation publication stamps the success");
        assert_eq!(
            stamped, base,
            "(i) the stamp is the publication's own `now`"
        );
        assert!(
            game(&worker)
                .iter()
                .all(|metric| metric.valid && !metric.stale),
            "(i) an accepted current-generation publication shows the rows FRESH"
        );
        // (ii) a stale-generation publication is rejected, so it cannot advance the stamp: after
        // the provider's own silence bound the rows must read STALE, not fresh.
        worker.apply_presentmon_update(update(2, true), 1, base + Duration::from_secs(1));
        assert_eq!(
            worker.presentmon_success,
            Some(stamped),
            "(ii) a stale-generation publication never advances the stamp"
        );
        let silent = base + bound + Duration::from_secs(1);
        worker.apply_freshness(&cfg, silent);
        assert!(
            game(&worker).iter().all(|metric| metric.valid && metric.stale),
            "(ii) worker silence past the bound marks every presentmon row STALE (displayed outcome)"
        );
        // (iii) an `available: false` publication (park / no target / expired capture) that
        // carries fresh-looking values is NOT a success: it must not advance the stamp.
        worker.apply_presentmon_update(update(1, false), 1, silent);
        assert_eq!(
            worker.presentmon_success,
            Some(stamped),
            "(iii) an unavailable (empty/park) publication never advances the stamp"
        );
        worker.apply_freshness(&cfg, silent);
        assert!(
            game(&worker).iter().all(|metric| metric.stale),
            "(iii) an unavailable publication does not make a silent worker look fresh again"
        );
        // (iv) silence past twice the bound: the rows become UNAVAILABLE (no value to display),
        // the same 2x bound the provider's own stale projection uses.
        let expired = base + bound.saturating_mul(2) + Duration::from_secs(1);
        worker.apply_freshness(&cfg, expired);
        assert!(
            game(&worker).iter().all(|metric| !metric.valid),
            "(iv) past twice the bound the rows are stated unavailable, never a stale number"
        );
        // (v) recovery: an accepted current-generation publication makes them fresh again.
        let recovered = expired + Duration::from_secs(1);
        worker.apply_presentmon_update(update(1, true), 1, recovered);
        assert_eq!(
            worker.presentmon_success,
            Some(recovered),
            "(v) the accepted recovery publication advances the stamp"
        );
        worker.apply_freshness(&cfg, recovered);
        assert!(
            game(&worker)
                .iter()
                .all(|metric| metric.valid && !metric.stale),
            "(v) the recovered worker's accepted publication shows the rows FRESH again"
        );
        // (vi) the disable arm clears the stamp and the rows, as every other provider does.
        let disabled = Config {
            presentmon_enabled: false,
            ..Config::default()
        };
        worker.apply_freshness(&disabled, recovered);
        assert!(
            game(&worker).iter().all(|metric| !metric.valid),
            "(vi) a disabled presentmon publishes no game rows"
        );
        assert_eq!(
            worker.presentmon_success, None,
            "(vi) the disabled arm clears the success stamp"
        );
    }

    // WO-OPT02B (finding O1, slice 2): coverage of the dormancy conversion.
    //
    // RED-FIRST set - these compile on the unit-50 base tree and are RED there,
    // because the base's running arms `sleep(TICK)` (which `Thread::unpark`
    // cannot interrupt) and its coordinator paces on the same fixed tick:
    //   `a_running_sampling_loop_adopts_a_demand_change_before_its_tick`,
    //   `coordinator_drains_the_hang_publication_below_the_sampling_tick`,
    //   `coordinator_drains_the_slow_event_feed_below_the_sampling_tick`,
    //   `telemetry_loops_park_to_their_own_deadlines_source_pin`.
    // Mechanism set - GREEN by construction on the delivered tree only, because
    // they drive the new production entry points (`sampling_loop_park`,
    // `Worker::park_bound`, `coordinator_run`) that the base tree does not have,
    // so they cannot compile there. Disclosed in the receipt; the per-feed
    // publication latency of the presentmon and network feeds has no
    // RED-capable spawn-level test for the same reason (presentmon keeps its
    // slice-3 50 ms worker tick, and the network probe's observable health row
    // needs its own >= 250 ms probe cadence, which is above the 50 ms bound).

    /// The WO-OPT02B bound: a wake must land INSIDE the retired 50 ms tick.
    const O2B_EDGE_BOUND: Duration = Duration::from_millis(50);
    /// The same bound minus the host timer's ~15.6 ms granularity, used where the
    /// base tree's fixed sleep is the thing being beaten (base lands at 50 + 15.6).
    const O2B_SUB_TICK_EDGE: Duration = Duration::from_millis(40);
    /// The publish floor the coordinator fixture runs at, so its own deadline park
    /// is far inside `O2B_EDGE_BOUND` and the tick's absence is what is measured.
    const O2B_PUBLISH_FLOOR: Duration = Duration::from_millis(10);
    /// The bounded drain budget of the WO-OPT02B helpers (loud, never a hang).
    const O2B_DRAIN_WAIT: Duration = Duration::from_millis(2_000);

    static O2B_RUNNING_HEADSET_SAMPLER: OnceLock<ScriptedSampler<Result<headset::Status, String>>> =
        OnceLock::new();

    fn o2b_running_headset_sampler(_timeout: Duration) -> Result<headset::Status, String> {
        sampled_in_slot(&O2B_RUNNING_HEADSET_SAMPLER)
    }

    /// B3-1, RED on the base tree. The headset loop runs the shipped production
    /// body with a demanded, enabled provider on a 30 s cadence, so after its
    /// first sample it sits in the RUNNING arm - on the base tree that arm is a
    /// fixed 50 ms `sleep(TICK)`, which `unpark` cannot interrupt, so the shipped
    /// config-apply edge (write, generation bump, `wake_workers`) lands no earlier
    /// than ~50 ms + the host timer. Delivered, the arm is a
    /// `park_timeout(sampling_loop_park(..))` of the same 30 s deadline and the
    /// edge lands at once.
    #[test]
    fn a_running_sampling_loop_adopts_a_demand_change_before_its_tick() {
        assert_injected_sampler_is_io_free("o2b_running_headset_sampler");
        let (enter_rx, release_tx) = install_scripted_sampler(
            &O2B_RUNNING_HEADSET_SAMPLER,
            vec![headset_ok(1), headset_ok(2)],
        );
        let config = Arc::new(RwLock::new(Config {
            headset_enabled: true,
            headset_poll: BEHAVIORAL_LONG,
            headset_query_timeout: Duration::from_millis(1),
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(8);
        let loop_thread = spawn_headset(
            Arc::clone(&config),
            Arc::clone(&generation),
            Arc::new(AtomicU64::new(ALL_CONSUMERS)),
            Arc::clone(&shutdown),
            tx,
            o2b_running_headset_sampler,
        );
        enter_rx
            .recv_timeout(BEHAVIORAL_WAIT)
            .expect("(i) the demanded headset loop samples through the seam");
        release_tx.send(()).expect("release #1");
        let (gen1, _) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(i) the first sample publishes"),
        );
        assert_eq!(gen1, 1, "(i) the first sample carries generation 1");
        // (ii) the edge, applied while the loop is in its running arm.
        retune(&config, |cfg| cfg.headset_poll = BEHAVIORAL_DRIVEN);
        let edge = Instant::now();
        generation.fetch_add(1, Ordering::SeqCst);
        loop_thread.thread().unpark();
        enter_rx.recv_timeout(O2B_SUB_TICK_EDGE).expect(
            "(ii) the demand/generation edge reaches the RUNNING loop before its 50 ms tick; \
             RED on the base tree, where the running arm sleeps the TICK and ignores unpark",
        );
        let latency = edge.elapsed();
        assert!(
            latency < O2B_EDGE_BOUND,
            "(ii) the adopted edge landed in {latency:?}, inside the 50 ms bound"
        );
        release_tx.send(()).expect("release #2");
        let (gen2, _) = headset_event(
            rx.recv_timeout(BEHAVIORAL_WAIT)
                .expect("(ii) the adopted edge publishes its sample"),
        );
        assert_eq!(
            gen2, 2,
            "(ii) the edge-adopted sample is published on the new generation"
        );
        shutdown.store(true, Ordering::Release);
        loop_thread.thread().unpark();
        loop_thread
            .join()
            .expect("the headset loop joins within the sampler's loud 5 s budget");
    }

    /// The production shape with every gated provider unparked and every
    /// window/probe collector off, so the coordinator performs no device I/O and
    /// the only publications are the runtime's own state edges.
    fn o2b_quiet_config() -> Config {
        Config {
            safe_mode: false,
            telemetry_interval: O2B_PUBLISH_FLOOR,
            headset_enabled: false,
            lhm_mode: "off".into(),
            controller_enabled: false,
            audio_enabled: false,
            gpu_provider: "off".into(),
            hwinfo_cpu_temp_sensor: String::new(),
            hang_enabled: false,
            network_probe_enabled: false,
            presentmon_enabled: false,
            ..Config::default()
        }
    }

    /// Drain the app-facing channel until an update matches, returning the update
    /// and the elapsed time of its arrival. Bounded and loud, never a hang.
    fn o2b_drain_within(
        updates: &LatestUpdates,
        budget: Duration,
        want: impl Fn(&Update) -> bool,
    ) -> Option<(Update, Duration)> {
        let at = Instant::now();
        loop {
            if let Ok(update) = updates.try_recv() {
                if want(&update) {
                    return Some((update, at.elapsed()));
                }
            }
            if at.elapsed() >= budget {
                return None;
            }
            std::hint::spin_loop();
        }
    }

    /// B3-2 (hang feed), RED on the base tree. The config-apply edge is the shipped
    /// `update_config` (write, bump, `wake_workers`). The disabled detector arm is
    /// an interruptible park, so it publishes the new generation at once and the
    /// coordinator drains it inside its deadline park. On the base tree the
    /// detector sleeps 50 ms (unpark cannot interrupt a sleep) AND the coordinator
    /// sleeps 50 ms, so the row lands at roughly 130 ms: RED against the bound.
    #[test]
    fn coordinator_drains_the_hang_publication_below_the_sampling_tick() {
        let cfg = o2b_quiet_config();
        let runtime = spawn(&cfg, 0);
        let _ = o2b_drain_within(&runtime.updates, O2B_DRAIN_WAIT, |update| {
            update.generation == 1
        });
        let at = Instant::now();
        let generation = runtime.update_config(&cfg);
        assert_eq!(generation, 2, "(i) the apply bumps the generation");
        let (update, _) =
            o2b_drain_within(&runtime.updates, Duration::from_millis(500), |update| {
                update.generation == 2
                    && update
                        .providers
                        .iter()
                        .any(|(name, ok)| name == "Hung window detector" && !*ok)
            })
            .expect(
                "(ii) the hang feed is drained and published; RED on the base tree, where the \
             detector's 50 ms sleep and the coordinator's 50 ms sleep are both in the path",
            );
        assert_eq!(
            update.generation, 2,
            "(ii) the drained publication carries the applied generation"
        );
        let latency = at.elapsed();
        assert!(
            latency < O2B_EDGE_BOUND,
            "(ii) the hang publication was drained in {latency:?}, inside the 50 ms tick bound"
        );
        drop(runtime);
    }

    /// B3-2 (slow feed), RED on the base tree. The four sampling loops publish
    /// through `send_slow`; the gpu loop's park statement is the payload (the
    /// WO-U46I #16 core-unavailable rows, hardware id `gpu:core`), republished for
    /// the new generation by the shipped `wake_workers` edge. The coordinator's own
    /// tick is the only latency left on the delivered tree.
    #[test]
    fn coordinator_drains_the_slow_event_feed_below_the_sampling_tick() {
        let cfg = o2b_quiet_config();
        let runtime = spawn(&cfg, 0);
        let _ = o2b_drain_within(&runtime.updates, O2B_DRAIN_WAIT, |update| {
            update.generation == 1
        });
        let at = Instant::now();
        let generation = runtime.update_config(&cfg);
        assert_eq!(generation, 2, "(i) the apply bumps the generation");
        let (update, _) =
            o2b_drain_within(&runtime.updates, Duration::from_millis(500), |update| {
                update.generation == 2
                    && update
                        .gpu_readings
                        .iter()
                        .any(|reading| reading.hardware_id == "gpu:core")
            })
            .expect(
                "(ii) the slow feed is drained and published; RED on the base tree, where the \
             coordinator drains only on its 50 ms sleep",
            );
        assert!(
            update.gpu_readings.iter().all(|reading| !reading.has_value),
            "(ii) the drained slow-feed state is the park's explicit unavailable core rows"
        );
        let latency = at.elapsed();
        assert!(
            latency < O2B_EDGE_BOUND,
            "(ii) the send_slow publication was drained in {latency:?}, inside the 50 ms bound"
        );
        drop(runtime);
    }

    /// B3-5: the cadence-parity source pins. Every converted sampling loop parks to
    /// its own deadline and performs no fixed-tick sleep; the coordinator's park
    /// follows the drain, and the four drains keep the shipped order and the shipped
    /// clear-to-EMPTY shape, so no publication can be left behind a park.
    #[test]
    fn telemetry_loops_park_to_their_own_deadlines_source_pin() {
        const PARK: &str =
            "std::thread::park_timeout(sampling_loop_park(last_attempt, interval, now));";
        for fn_head in [
            "fn spawn_headset(",
            "fn spawn_lhm(",
            "fn spawn_hwinfo(",
            "fn spawn_gpu(",
        ] {
            let loop_src = shipped_loop_source(fn_head);
            assert_eq!(
                loop_src.matches("std::thread::sleep(TICK)").count(),
                0,
                "{fn_head}: the fixed 50 ms sleep is gone from the converted arm"
            );
            assert_eq!(
                loop_src.matches(PARK).count(),
                1,
                "{fn_head}: exactly one deadline park in the running arm"
            );
            assert_eq!(
                loop_src.matches("std::thread::park();").count(),
                1,
                "{fn_head}: the disabled/zero-demand arm keeps its unbounded native park"
            );
        }
        let source = include_str!("telemetry.rs").replace("\r\n", "\n");
        let production = source.split("mod tests {").next().unwrap_or_default();
        assert_eq!(
            production.matches("std::thread::sleep(TICK)").count(),
            0,
            "the retired tick has no production site left in this module"
        );
        let run_at = production
            .find("fn coordinator_run(worker: &mut Worker, shutdown: &AtomicBool) -> bool {")
            .expect("the coordinator loop is a function of its own");
        let rest = &production[run_at..];
        let run_body = &rest[..rest.find("\nfn ").unwrap_or(rest.len())];
        let drain = run_body
            .find("worker.tick()")
            .expect("the coordinator drains inside the tick");
        let park = run_body
            .find("std::thread::park_timeout(worker.park_bound(")
            .expect("the coordinator parks to its own earliest deadline");
        assert!(
            drain < park,
            "the drain (all four sweeps) happens before the loop parks, never after"
        );
        assert_eq!(
            run_body.matches("while ").count(),
            1,
            "one loop, one park per iteration (no nested tick loop)"
        );
        let tick_at = production
            .find("fn tick(&mut self) -> bool {")
            .expect("the coordinator tick exists");
        let tick_rest = &production[tick_at..];
        let tick_body = &tick_rest[..tick_rest.find("\n    fn ").unwrap_or(tick_rest.len())];
        let slow = tick_body
            .find("while let Ok(event) = self.slow_rx.try_recv()")
            .expect("the slow feed is swept to EMPTY");
        let presentmon = tick_body
            .find("while let Ok(update) = self.presentmon_rx.try_recv()")
            .expect("the presentmon feed is swept to EMPTY");
        let hang = tick_body
            .find("while let Ok(update) = self.hang_rx.try_recv()")
            .expect("the hang feed is swept to EMPTY");
        let network = tick_body
            .find("while let Ok(update) = self.network_rx.try_recv()")
            .expect("the network feed is swept to EMPTY");
        assert!(
            slow < presentmon && presentmon < hang && hang < network,
            "the four sweeps keep the shipped order, and each clears its channel before the park"
        );
        assert!(
            tick_body.contains("self.publish_dirty |= applied;"),
            "a swept publication marks the state dirty, so the publish that follows is the change, not the tick"
        );
    }

    /// B3-4: the sampling-loop deadline park, as exact `Duration`s.
    #[test]
    fn sampling_loop_park_is_the_loop_deadline_with_the_local_floor() {
        let now = Instant::now();
        let interval = Duration::from_millis(400);
        assert_eq!(
            sampling_loop_park(None, interval, now),
            Duration::from_millis(2),
            "a cleared stamp is the shipped `due(None)` = sample now, so the park is the floor"
        );
        assert_eq!(
            sampling_loop_park(Some(now), interval, now),
            interval,
            "a fresh stamp parks exactly the cadence the `due` gate compares"
        );
        assert_eq!(
            sampling_loop_park(Some(now), interval, now + Duration::from_millis(150)),
            Duration::from_millis(250),
            "elapsed time shortens the park by the same amount"
        );
        assert_eq!(
            sampling_loop_park(Some(now - Duration::from_secs(5)), interval, now),
            Duration::from_millis(2),
            "a provider call that outlasted the cadence floors at 2 ms, never a negative park"
        );
        assert_eq!(
            sampling_loop_park(Some(now), Duration::ZERO, now),
            Duration::from_millis(2),
            "a zero interval floors at 2 ms, so the loop can never spin below the host timer"
        );
        assert!(
            due(Some(now), interval, now + interval),
            "the deadline is inclusive: the park's end is a due tick, so the wake lands AT it"
        );
    }

    /// B3-4: the coordinator's park bound, as exact `Duration`s - only the shipped
    /// gates and the publish floor, floored at the local 2 ms.
    #[test]
    fn coordinator_park_bound_is_the_earliest_deadline_with_the_local_floor() {
        let cfg = Config {
            telemetry_interval: Duration::from_millis(300),
            controller_enabled: true,
            controller_poll: Duration::from_millis(120),
            audio_enabled: false,
            ..Config::default()
        };
        let controller = extended_consumer_bit("controller");
        let mut worker = test_worker(cfg.clone());
        worker.publish_dirty = false;
        worker.consumers.store(controller, Ordering::Relaxed);
        let base = Instant::now();
        worker.last_publish = Some(base);
        worker.controller_attempt = Some(base);
        assert_eq!(
            worker.park_bound(&cfg, controller, base + Duration::from_millis(50)),
            Duration::from_millis(70),
            "the demanded controller arm (120 ms cadence) is the earliest deadline, ahead of the 300 ms publish floor"
        );
        assert_eq!(
            worker.park_bound(&cfg, 0, base + Duration::from_millis(50)),
            Duration::from_millis(250),
            "with no arm demanded, only the publish floor paces the coordinator"
        );
        assert_eq!(
            worker.park_bound(&cfg, 0, base + Duration::from_secs(9)),
            Duration::from_millis(2),
            "past every deadline the bound is the floor, never a negative park"
        );
        worker.publish_dirty = true;
        assert_eq!(
            worker.park_bound(&cfg, 0, base),
            Duration::from_millis(2),
            "a dirty publish state parks at the floor: a change never waits for the cadence"
        );
        worker.publish_dirty = false;
        worker.controller_attempt = None;
        assert_eq!(
            worker.park_bound(&cfg, controller, base),
            Duration::from_millis(2),
            "a cleared stamp is the shipped `due(None)` = poll now"
        );
    }

    // WO-OPT02B-PARKFIX set (audit findings F-AUD-OPT02B-1 and -2, both P2).
    // RED-FIRST on the frozen 02B delta `9b90f791`: the three tests below compile
    // there (they name only the shipped `park_bound`, the shipped constants and
    // explicit `Duration` literals) and are RED there, for the two findings:
    //   `park_bound_caps_the_publish_wake_at_the_legal_ceiling` - the publish
    //     floor is `cfg.telemetry_interval` uncapped, so a legal 10 s config
    //     parks the coordinator for 10 s (the row lands 10 s late, displayed age
    //     doubles to 20 s, a gpu row reads Current for ~40 s of a 30 s bound);
    //   `park_bound_respects_a_poll_arm_staleness_deadline` - the
    //     `apply_freshness` TTL set is absent from the bound, so the
    //     Current->Stale flip of the three coordinator poll arms lands up to one
    //     arm interval late (battery 5 s, performance 3 s, connections 1 s);
    //   `park_bound_publish_wake_cap_is_a_min_and_keeps_the_floor` - row (v) is
    //     RED for the same TTL reason; rows (i)-(iv) are parity rows, GREEN on
    //     both trees, and pin that the cap is a `min` and that the 2 ms floor and
    //     the never-negative rule are unchanged.

    /// F-AUD-OPT02B-1, RED on the frozen delta. The publish floor is the app's
    /// own consumption cadence, so the coordinator's WAKE for it is capped: at
    /// the legal ceiling `telemetry_interval = 10 s` (`config.rs:416-421`, legal
    /// range 100..10000 ms) the frozen tree parks the whole interval. The cap is
    /// a WAKE bound only - the publish gate, the intervals, the TTLs and the
    /// publish-once contract stay exactly as shipped.
    #[test]
    fn park_bound_caps_the_publish_wake_at_the_legal_ceiling() {
        let cfg = Config {
            telemetry_interval: Duration::from_secs(10),
            ..Config::default()
        };
        let mut worker = test_worker(cfg.clone());
        worker.publish_dirty = false;
        worker.consumers.store(0, Ordering::Relaxed);
        let base = Instant::now();
        worker.last_publish = Some(base);
        // The cap as an explicit number, so the RED row IS the finding: the
        // frozen tree lands 10 s, the delivered wake is 300 ms.
        assert_eq!(
            worker.park_bound(&cfg, 0, base),
            Duration::from_millis(300),
            "(i) at the legal 10 s ceiling the wake for a feed publication is the 300 ms cap, \
             never the whole interval (RED on the frozen delta: the park lands at 10 s)"
        );
        assert_eq!(
            worker.park_bound(&cfg, 0, base + Duration::from_millis(200)),
            Duration::from_millis(100),
            "(ii) elapsed time shortens the capped wake by the same amount"
        );
        assert!(
            worker.park_bound(&cfg, 0, base) <= Duration::from_millis(300),
            "(iii) a publication on a feed with no wake edge waits at most 300 ms to be applied, \
             so the worst displayed age is the 300 ms wake plus the app's own 300 ms consumption"
        );
    }

    /// F-AUD-OPT02B-2, RED on the frozen delta for all three coordinator poll
    /// arms. `apply_freshness` flips a row Current->Stale at
    /// `last_success + stale_after` (`:1432` performance and `:1436` connections at
    /// `EXTENDED_STALE`, `:1440` system battery at `BATTERY_STALE`). The frozen
    /// bound carries only each arm's POLL due, so the flip lands up to one arm
    /// interval late. The fix adds the very same fields and the very same bounds to
    /// the wake set: no staleness math, no TTL value, no new config.
    #[test]
    fn park_bound_respects_a_poll_arm_staleness_deadline() {
        let remaining = Duration::from_millis(50);
        // A 100 ms publish floor sits BEHIND the fixture's TTL remnant, so the
        // arm's own staleness deadline is the term that must bind.
        let cfg = Config {
            telemetry_interval: Duration::from_millis(100),
            ..Config::default()
        };
        let base = Instant::now();
        let mut offenders: Vec<String> = Vec::new();
        for (provider, bit, stale_after, poll) in [
            (
                "performance",
                consumer_bit("performance"),
                EXTENDED_STALE,
                EXTENDED_POLL,
            ),
            (
                "connections",
                consumer_bit("connections"),
                EXTENDED_STALE,
                EXTENDED_POLL,
            ),
            (
                "system battery",
                consumer_bit("system battery"),
                BATTERY_STALE,
                BATTERY_POLL,
            ),
        ] {
            let mut worker = test_worker(cfg.clone());
            worker.publish_dirty = false;
            worker.consumers.store(bit, Ordering::Relaxed);
            worker.last_publish = Some(base);
            // The arm polled just now (its poll due is a full `poll` away) and its
            // last success is `stale_after - remaining` old, so the flip is due in
            // `remaining`. `remaining < poll` for all three arms, and no arm's
            // interval equals its TTL (1 s / 1 s / 5 s against 3 s / 3 s / 15 s), so
            // each arm is a real gap and none of them is a parity pin.
            worker.performance_attempt = Some(base);
            worker.connections_attempt = Some(base);
            worker.battery_attempt = Some(base);
            let stamp = Some(base - (stale_after - remaining));
            match provider {
                "performance" => worker.performance_success = stamp,
                "connections" => worker.connections_success = stamp,
                _ => worker.battery_success = stamp,
            }
            assert!(
                remaining < poll,
                "{provider}: the fixture makes the gap visible - the TTL remnant \
                 ({remaining:?}) is inside the arm's own poll interval ({poll:?})"
            );
            let park = worker.park_bound(&cfg, bit, base);
            if park > remaining {
                offenders.push(format!(
                    "{provider}: park {park:?} outlives the staleness deadline {remaining:?} \
                     (arm poll {poll:?})"
                ));
            }
        }
        assert!(
            offenders.is_empty(),
            "each coordinator poll arm's wake lands AT its staleness deadline, never one poll \
             interval late; offenders: {offenders:?} (RED on the frozen delta: every park is the \
             100 ms publish floor, because the TTL set is missing from the bound)"
        );
    }

    /// F-AUD-OPT02B-1/-2 parity: the cap is a `min`, so the shipped default
    /// `telemetry_interval = 300 ms` keeps its own number (the figure the pinned
    /// `coordinator_park_bound_is_the_earliest_deadline_with_the_local_floor`
    /// asserts), a floor below the cap is never raised by it, and the 2 ms floor
    /// and the never-negative rule apply to every term, the new TTL terms
    /// included. Rows (i)-(iv) are GREEN on the frozen delta (parity); row (v) is
    /// RED there, for the F-AUD-OPT02B-2 reason.
    #[test]
    fn park_bound_publish_wake_cap_is_a_min_and_keeps_the_floor() {
        let base = Instant::now();
        let cfg = Config {
            telemetry_interval: Duration::from_millis(300),
            ..Config::default()
        };
        let mut worker = test_worker(cfg.clone());
        worker.publish_dirty = false;
        worker.consumers.store(0, Ordering::Relaxed);
        worker.last_publish = Some(base);
        assert_eq!(
            worker.park_bound(&cfg, 0, base + Duration::from_millis(50)),
            Duration::from_millis(250),
            "(i) a 300 ms config is unaffected: the publish-floor term is still 300 ms"
        );
        let short = Config {
            telemetry_interval: Duration::from_millis(100),
            ..Config::default()
        };
        assert_eq!(
            worker.park_bound(&short, 0, base),
            Duration::from_millis(100),
            "(ii) a floor below the cap is never raised by it: the cap is a `min`"
        );
        worker.publish_dirty = true;
        assert_eq!(
            worker.park_bound(&cfg, 0, base),
            Duration::from_millis(2),
            "(iii) a dirty publish still parks at the 2 ms floor"
        );
        worker.publish_dirty = false;
        assert_eq!(
            worker.park_bound(&cfg, 0, base + Duration::from_secs(9)),
            Duration::from_millis(2),
            "(iv) past every deadline the bound is the floor, never a negative park"
        );
        let battery = consumer_bit("system battery");
        worker.consumers.store(battery, Ordering::Relaxed);
        worker.battery_attempt = Some(base);
        worker.battery_success = Some(base - BATTERY_STALE * 2);
        assert_eq!(
            worker.park_bound(&cfg, battery, base),
            Duration::from_millis(2),
            "(v) a staleness term past its deadline floors at 2 ms, never negative (RED on the \
             frozen delta: no staleness term exists there, so the 5 s battery poll paces the park)"
        );
    }

    /// B3-2, mechanism form: every publication feed of the coordinator loop is
    /// drained inside the retired 50 ms tick, driven on the real loop body with the
    /// four feed senders retained by the test. GREEN-only on the base tree is
    /// impossible: the base has no reachable coordinator loop body.
    #[test]
    fn coordinator_run_drains_every_publication_feed_before_the_next_deadline() {
        struct Feeds {
            worker: Worker,
            slow_tx: mpsc::SyncSender<SlowEvent>,
            presentmon_tx: mpsc::Sender<presentmon::Update>,
            hang_tx: mpsc::Sender<hang::Update>,
            network_tx: mpsc::Sender<network::Update>,
            updates: LatestUpdates,
            shutdown: Arc<AtomicBool>,
        }
        let cfg = Config {
            safe_mode: false,
            telemetry_interval: O2B_PUBLISH_FLOOR,
            // The three feeds whose state the coordinator keeps through
            // `apply_freshness` must be enabled, or the shipped disable arms erase
            // the very rows this test measures. No worker is spawned in this
            // fixture, so enabling the probe dials nothing: the probe lives in the
            // `network` worker, and the coordinator only applies published updates.
            headset_enabled: true,
            presentmon_enabled: true,
            network_probe_enabled: true,
            network_probe_target: "127.0.0.1:1".into(),
            controller_enabled: false,
            audio_enabled: false,
            lhm_mode: "off".into(),
            gpu_provider: "off".into(),
            hang_enabled: false,
            ..Config::default()
        };
        let (slow_tx, slow_rx) = mpsc::sync_channel(8);
        let (presentmon_tx, presentmon_rx) = mpsc::channel();
        let (hang_tx, hang_rx) = mpsc::channel();
        let (network_tx, network_rx) = mpsc::channel();
        let (tx, updates) = latest_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            config: Arc::new(RwLock::new(cfg.clone())),
            config_generation: Arc::new(AtomicU64::new(1)),
            request_cache: (0u64, Arc::new(cfg.clone())),
            consumers: Arc::new(AtomicU64::new(0)),
            tx,
            slow_rx,
            update: Update {
                generation: 1,
                ..Default::default()
            },
            net: netif::NetProvider::new(),
            performance: performance::Provider::new(),
            headset_success: None,
            lhm_success: None,
            gpu_success: None,
            performance_success: None,
            connections_success: None,
            battery_success: None,
            hwinfo_success: None,
            network_success: None,
            presentmon_success: None,
            controller_attempt: None,
            audio_attempt: None,
            net_attempt: None,
            performance_attempt: None,
            connections_attempt: None,
            battery_attempt: None,
            performance_failures: 0,
            presentmon_rx,
            hang_rx,
            network_rx,
            hwinfo_cpu_temp: Metric::default(),
            lhm_cpu_temp: Metric::default(),
            native_gpu_temp: Metric::default(),
            lhm_gpu_temp: Metric::default(),
            gpu_backend: String::new(),
            native_gpu_power: None,
            lhm_readings: Vec::new(),
            hwinfo_readings: Vec::new(),
            performance_readings: Vec::new(),
            connection_reading: None,
            hwinfo_diagnostic: None,
            lhm_diagnostic: None,
            generation: 1,
            safe_mode_active: false,
            publish_dirty: true,
            last_publish: None,
        };
        let feeds = Feeds {
            worker,
            slow_tx,
            presentmon_tx,
            hang_tx,
            network_tx,
            updates,
            shutdown: Arc::clone(&shutdown),
        };
        let mut worker = feeds.worker;
        let loop_shutdown = Arc::clone(&feeds.shutdown);
        let loop_thread = std::thread::Builder::new()
            .name("telemetry-coordinator-fixture".into())
            .spawn(move || coordinator_run(&mut worker, &loop_shutdown))
            .expect("the coordinator loop runs on its own thread");
        let drained = |want: &dyn Fn(&Update) -> bool| {
            o2b_drain_within(&feeds.updates, Duration::from_millis(500), want)
        };
        let _ = drained(&|update| update.generation == 1);
        // (1) the slow feed: a headset sample publishes the `headset` health row.
        let at = Instant::now();
        feeds
            .slow_tx
            .try_send(SlowEvent::Headset(1, headset_ok(3)))
            .expect("the slow feed accepts the event");
        let latency = drained(&|update| {
            update
                .providers
                .iter()
                .any(|(name, ok)| name == "headset" && *ok)
        })
        .expect("(1) the send_slow publication is drained and published");
        assert!(
            at.elapsed() < O2B_EDGE_BOUND && latency.1 < O2B_EDGE_BOUND,
            "(1) the slow feed was drained in {:?}, inside the retired 50 ms tick",
            latency.1
        );
        // (2) the presentmon feed.
        feeds
            .presentmon_tx
            .send(presentmon::Update {
                generation: 1,
                available: true,
                game: GameStats {
                    active: true,
                    process_name: "synthetic.exe".into(),
                    fps: Metric::valid(61.0, SystemTime::UNIX_EPOCH),
                    ..Default::default()
                },
                ..Default::default()
            })
            .expect("the presentmon feed accepts the update");
        let at = Instant::now();
        let (_, latency) = drained(&|update| update.game.fps.valid)
            .expect("(2) the presentmon publication is drained and published");
        assert!(
            at.elapsed() < O2B_EDGE_BOUND && latency < O2B_EDGE_BOUND,
            "(2) the presentmon feed was drained in {latency:?}, inside the retired 50 ms tick"
        );
        // (3) the hang feed.
        feeds
            .hang_tx
            .send(hang::Update {
                generation: 1,
                targets: Vec::new(),
                available: true,
                error: None,
                probe_indeterminate: 0,
            })
            .expect("the hang feed accepts the update");
        let (_, latency) = drained(&|update| {
            update
                .providers
                .iter()
                .any(|(name, ok)| name == "Hung window detector" && *ok)
        })
        .expect("(3) the hang publication is drained and published");
        assert!(
            latency < O2B_EDGE_BOUND,
            "(3) the hang feed was drained in {latency:?}, inside the retired 50 ms tick"
        );
        // (4) the network feed.
        feeds
            .network_tx
            .send(network::Update {
                generation: 1,
                ping: Metric::valid(11.0, SystemTime::UNIX_EPOCH),
                jitter: Metric::valid(2.0, SystemTime::UNIX_EPOCH),
                loss: Metric::valid(0.0, SystemTime::UNIX_EPOCH),
                available: true,
                error: None,
            })
            .expect("the network feed accepts the update");
        let (_, latency) = drained(&|update| update.ping_ms.valid)
            .expect("(4) the network publication is drained and published");
        assert!(
            latency < O2B_EDGE_BOUND,
            "(4) the network feed was drained in {latency:?}, inside the retired 50 ms tick"
        );
        feeds
            .shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        loop_thread
            .join()
            .expect("the coordinator loop exits on the shutdown edge");
    }
}
