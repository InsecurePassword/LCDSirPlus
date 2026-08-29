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
const TICK: Duration = Duration::from_millis(50);
const NETWORK_POLL: Duration = Duration::from_secs(1);
const EXTENDED_POLL: Duration = Duration::from_secs(1);
const EXTENDED_STALE: Duration = Duration::from_secs(3);
const BATTERY_POLL: Duration = Duration::from_secs(5);
const BATTERY_STALE: Duration = Duration::from_secs(15);

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
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Telemetry {
    pub fn update_config(&self, config: &Config) -> u64 {
        let mut current = self.config.write().unwrap_or_else(|e| e.into_inner());
        *current = config.clone();
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
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
    controller_attempt: Option<Instant>,
    audio_attempt: Option<Instant>,
    net_attempt: Option<Instant>,
    performance_attempt: Option<Instant>,
    connections_attempt: Option<Instant>,
    battery_attempt: Option<Instant>,
    presentmon_rx: mpsc::Receiver<presentmon::Update>,
    hang_rx: mpsc::Receiver<hang::Update>,
    network_rx: mpsc::Receiver<network::Update>,
    temps_need_refresh: Arc<AtomicBool>,
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

fn due(last_attempt: Option<Instant>, period: Duration, now: Instant) -> bool {
    last_attempt
        .map(|last| now.duration_since(last) >= period)
        .unwrap_or(true)
}

fn stale(last_success: Option<Instant>, stale_after: Duration, now: Instant) -> bool {
    last_success
        .map(|last| now.duration_since(last) > stale_after)
        .unwrap_or(false)
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
        let (generation, cfg) = current_request(&self.config, &self.config_generation);
        let now = Instant::now();

        if generation != self.generation {
            self.reset_state(generation);
        }

        while let Ok(event) = self.slow_rx.try_recv() {
            self.apply_slow_event(event, generation, now);
        }
        while let Ok(update) = self.presentmon_rx.try_recv() {
            self.apply_presentmon_update(update, generation);
        }
        while let Ok(update) = self.hang_rx.try_recv() {
            self.apply_hang_update(update, generation);
        }
        while let Ok(update) = self.network_rx.try_recv() {
            self.apply_network_update(update, generation, &cfg, now);
        }

        if cfg.safe_mode {
            self.hwinfo_diagnostic = None;
            if !self.safe_mode_active {
                self.reset_state(generation);
                self.safe_mode_active = true;
            }
            self.update = Update {
                generation,
                ..Default::default()
            };
            return self.tx.send(self.update.clone());
        }
        self.safe_mode_active = false;

        self.poll_controller(&cfg, now);
        self.poll_audio(&cfg, now);
        self.poll_network(&cfg, now);
        self.poll_performance(now);
        self.poll_connections(now);
        self.poll_battery(now);
        self.apply_freshness(&cfg, now);
        self.rebuild_extended_readings(&cfg);
        self.update.generation = generation;
        self.tx.send(self.update.clone())
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
        self.controller_attempt = None;
        self.audio_attempt = None;
        self.net_attempt = None;
        self.performance_attempt = None;
        self.connections_attempt = None;
        self.battery_attempt = None;
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
        self.temps_need_refresh.store(true, Ordering::Relaxed);
        self.generation = generation;
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

    fn apply_presentmon_update(&mut self, update: presentmon::Update, generation: u64) {
        if update.generation != generation {
            return;
        }
        self.update.game = update.game;
        self.set_provider("presentmon", update.available);
        if !update.available && !update.detail.is_empty() {
            crate::log_debug!("PresentMon unavailable: {}", update.detail);
        }
    }

    fn apply_hang_update(&mut self, update: hang::Update, generation: u64) {
        if update.generation != generation {
            return;
        }
        self.update.hung = update.targets;
        self.set_provider("Hung window detector", update.available);
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

    fn poll_controller(&mut self, cfg: &Config, now: Instant) {
        if !enabled(cfg, "controller") {
            self.controller_attempt = None;
            self.update.controller = ControllerBattery::default();
            self.remove_provider("controller");
            return;
        }
        if !due(self.controller_attempt, cfg.controller_poll, now) {
            return;
        }
        self.controller_attempt = Some(now);
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

    fn poll_audio(&mut self, cfg: &Config, now: Instant) {
        if !enabled(cfg, "audio") {
            self.audio_attempt = None;
            self.update.audio = None;
            self.remove_provider("audio");
            return;
        }
        if !due(self.audio_attempt, cfg.audio_poll, now) {
            return;
        }
        self.audio_attempt = Some(now);
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

    fn poll_network(&mut self, cfg: &Config, now: Instant) {
        if !enabled(cfg, "net") {
            self.net_attempt = None;
            self.update.net = None;
            self.remove_provider("net");
            return;
        }
        if !due(self.net_attempt, NETWORK_POLL, now) {
            return;
        }
        self.net_attempt = Some(now);
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

    fn poll_performance(&mut self, now: Instant) {
        if !due(self.performance_attempt, EXTENDED_POLL, now) {
            return;
        }
        self.performance_attempt = Some(now);
        match self.performance.sample() {
            Ok(Some(sample)) => {
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
            Ok(None) => self.set_provider("performance", true),
            Err(error) => {
                crate::log_debug!("performance sample failed: {}", error);
                self.set_provider("performance", false);
            }
        }
    }

    fn poll_connections(&mut self, now: Instant) {
        if !due(self.connections_attempt, EXTENDED_POLL, now) {
            return;
        }
        self.connections_attempt = Some(now);
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

    fn poll_battery(&mut self, now: Instant) {
        if !due(self.battery_attempt, BATTERY_POLL, now) {
            return;
        }
        self.battery_attempt = Some(now);
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
            let stale_after = cfg
                .telemetry_interval
                .saturating_mul(3)
                .max(Duration::from_secs(3));
            let gpu_stale = stale(self.gpu_success, stale_after, now);
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
            self.update.gpu_readings.clear();
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
        if !enabled(cfg, "presentmon") {
            self.update.game = GameStats::default();
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
        self.temps_need_refresh.store(
            !self.update.cpu_temp.valid
                || self.update.cpu_temp.stale
                || !self.update.gpu_temp.valid
                || self.update.gpu_temp.stale,
            Ordering::Relaxed,
        );
    }

    fn rebuild_extended_readings(&mut self, cfg: &Config) {
        let mut readings: Vec<Reading> = self
            .lhm_readings
            .iter()
            .filter(|reading| {
                !matches!(
                    reading.key,
                    MetricKey::TotalPower | MetricKey::CPUPower | MetricKey::GPUPower
                )
            })
            .cloned()
            .collect();
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
}

fn spawn_headset(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-headset".into())
        .spawn(move || {
            let mut last_attempt = None;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = current_request(&config, &generation);
                if !enabled(&cfg, "headset") {
                    last_attempt = None;
                } else {
                    let now = Instant::now();
                    if due(last_attempt, cfg.headset_poll, now) {
                        last_attempt = Some(now);
                        if !send_slow(
                            &tx,
                            SlowEvent::Headset(
                                generation,
                                headset::query(cfg.headset_query_timeout),
                            ),
                        ) {
                            return;
                        }
                    }
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("headset telemetry thread")
}

fn spawn_lhm(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    temps_need_refresh: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-lhm".into())
        .spawn(move || {
            let mut last_attempt = None;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = current_request(&config, &generation);
                if let Some(url) = lhm_url(&cfg) {
                    let now = Instant::now();
                    let settled_interval = (cfg.lhm_stale_after / 2).max(cfg.lhm_interval);
                    let interval =
                        if cfg.lhm_mode == "on" || temps_need_refresh.load(Ordering::Relaxed) {
                            cfg.lhm_interval
                        } else {
                            settled_interval
                        };
                    if due(last_attempt, interval, now) {
                        last_attempt = Some(now);
                        let result = lhm::sample_extended(
                            url,
                            &cfg.lhm_sensors,
                            &lhm_selectors(&cfg),
                            Duration::from_secs(2),
                        );
                        if !send_slow(&tx, SlowEvent::Lhm(generation, Box::new(result))) {
                            return;
                        }
                    }
                } else {
                    last_attempt = None;
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("LHM telemetry thread")
}

fn spawn_hwinfo(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-hwinfo".into())
        .spawn(move || {
            let mut last_attempt = None;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = current_request(&config, &generation);
                if enabled(&cfg, "hwinfo") {
                    let now = Instant::now();
                    if due(last_attempt, EXTENDED_POLL, now) {
                        last_attempt = Some(now);
                        let selectors = hwinfo_selectors(&cfg);
                        let result = hwinfo::sample(&selectors, cfg.hwinfo_stale_after);
                        if !send_slow(&tx, SlowEvent::Hwinfo(generation, selectors, result)) {
                            return;
                        }
                    }
                } else {
                    last_attempt = None;
                }
                std::thread::sleep(TICK);
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

fn spawn_gpu(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::SyncSender<SlowEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("telemetry-gpu".into())
        .spawn(move || {
            let mut provider = gpu::Provider::new("off");
            let mut last_attempt = None;
            while !shutdown.load(Ordering::Relaxed) {
                let (generation, cfg) = current_request(&config, &generation);
                if enabled(&cfg, "gpu") {
                    let now = Instant::now();
                    if due(last_attempt, cfg.telemetry_interval, now) {
                        last_attempt = Some(now);
                        if !send_slow(
                            &tx,
                            SlowEvent::Gpu(generation, provider.sample(&cfg.gpu_provider)),
                        ) {
                            return;
                        }
                    }
                } else {
                    last_attempt = None;
                    provider = gpu::Provider::new("off");
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("GPU telemetry thread")
}

/// Start the coordinator and owned provider workers.
pub fn spawn(cfg: &Config) -> Telemetry {
    let config = Arc::new(RwLock::new(cfg.clone()));
    let generation = Arc::new(AtomicU64::new(1));
    let shutdown = Arc::new(AtomicBool::new(false));
    let temps_need_refresh = Arc::new(AtomicBool::new(true));
    let (tx, rx) = latest_channel();
    let (slow_tx, slow_rx) = mpsc::sync_channel(8);
    let mut threads = vec![spawn_headset(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&shutdown),
        slow_tx.clone(),
    )];
    threads.push(spawn_lhm(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&shutdown),
        Arc::clone(&temps_need_refresh),
        slow_tx.clone(),
    ));
    threads.push(spawn_gpu(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&shutdown),
        slow_tx.clone(),
    ));
    threads.push(spawn_hwinfo(
        Arc::clone(&config),
        Arc::clone(&generation),
        Arc::clone(&shutdown),
        slow_tx.clone(),
    ));
    let (presentmon_rx, presentmon_thread) = presentmon::spawn(
        Arc::clone(&config),
        Arc::clone(&generation),
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
        controller_attempt: None,
        audio_attempt: None,
        net_attempt: None,
        performance_attempt: None,
        connections_attempt: None,
        battery_attempt: None,
        presentmon_rx,
        hang_rx,
        network_rx,
        temps_need_refresh,
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
            while !coordinator_shutdown.load(Ordering::Relaxed) && worker.tick() {
                std::thread::sleep(TICK);
            }
        })
        .expect("telemetry thread");
    threads.push(coordinator);
    Telemetry {
        updates: rx,
        config,
        generation,
        shutdown,
        threads,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let config = Arc::new(RwLock::new(cfg));
        let generation = Arc::new(AtomicU64::new(1));
        let (tx, _updates) = latest_channel();
        let (_slow_tx, slow_rx) = mpsc::sync_channel(1);
        let (_presentmon_tx, presentmon_rx) = mpsc::channel();
        let (_hang_tx, hang_rx) = mpsc::channel();
        let (_network_tx, network_rx) = mpsc::channel();
        Worker {
            config,
            config_generation: generation,
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
            controller_attempt: None,
            audio_attempt: None,
            net_attempt: None,
            performance_attempt: None,
            connections_attempt: None,
            battery_attempt: None,
            presentmon_rx,
            hang_rx,
            network_rx,
            temps_need_refresh: Arc::new(AtomicBool::new(false)),
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
        );
        worker.apply_hang_update(
            hang::Update {
                generation: 1,
                available: true,
                targets: vec![target.clone()],
                error: None,
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
        let runtime = spawn(&Config {
            safe_mode: true,
            ..Config::default()
        });
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
}
