//! Phase 2 telemetry runtime. Blocking headset HID and LHM HTTP calls have
//! dedicated persistent workers; the coordinator handles fast native polls,
//! retention, staleness, and publication to the render loop.

use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::model::{ControllerBattery, Freshness, HeadsetBattery, Metric, Reading};
use crate::providers::{audio, headset, lhm, netif, xinput};

const DEFAULT_LHM_URL: &str = "http://127.0.0.1:8085/data.json";
const TICK: Duration = Duration::from_millis(50);
const NETWORK_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub headset: HeadsetBattery,
    pub controller: ControllerBattery,
    pub audio: Option<audio::AudioState>,
    pub net: Option<netif::NetThroughput>,
    pub cpu_temp: Metric,
    pub gpu_temp: Metric,
    pub lhm_readings: Vec<Reading>,
    /// Per-provider health: (name, last poll succeeded).
    pub providers: Vec<(String, bool)>,
}

pub struct Telemetry {
    pub updates: mpsc::Receiver<Update>,
    config: Arc<RwLock<Config>>,
}

impl Telemetry {
    pub fn update_config(&self, config: &Config) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
    }
}

enum SlowEvent {
    Headset(Result<headset::Status, String>),
    Lhm(Box<Result<(lhm::Selection, SystemTime), String>>),
}

struct Worker {
    config: Arc<RwLock<Config>>,
    tx: mpsc::Sender<Update>,
    slow_rx: mpsc::Receiver<SlowEvent>,
    update: Update,
    net: netif::NetProvider,
    headset_success: Option<Instant>,
    lhm_success: Option<Instant>,
    controller_attempt: Option<Instant>,
    audio_attempt: Option<Instant>,
    net_attempt: Option<Instant>,
}

fn current_config(config: &RwLock<Config>) -> Config {
    config.read().unwrap_or_else(|e| e.into_inner()).clone()
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
        _ => false,
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

fn apply_lhm_freshness(
    update: &mut Update,
    last_success: Option<Instant>,
    stale_after: Duration,
    now: Instant,
) {
    let is_stale = stale(last_success, stale_after, now);
    update.cpu_temp.stale = update.cpu_temp.valid && is_stale;
    update.gpu_temp.stale = update.gpu_temp.valid && is_stale;
    for reading in &mut update.lhm_readings {
        reading.freshness = if is_stale {
            Freshness::Stale
        } else {
            Freshness::Current
        };
    }
}

impl Worker {
    fn tick(&mut self) -> bool {
        let cfg = current_config(&self.config);
        let now = Instant::now();

        while let Ok(event) = self.slow_rx.try_recv() {
            match event {
                SlowEvent::Headset(result) => self.apply_headset(result, now),
                SlowEvent::Lhm(result) => self.apply_lhm(*result, now),
            }
        }

        self.poll_controller(&cfg, now);
        self.poll_audio(&cfg, now);
        self.poll_network(&cfg, now);
        self.apply_freshness(&cfg, now);
        self.tx.send(self.update.clone()).is_ok()
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

    fn apply_lhm(&mut self, result: Result<(lhm::Selection, SystemTime), String>, now: Instant) {
        match result {
            Ok((selection, sampled_at)) => {
                self.update.cpu_temp = temperature(&selection.cpu_temp, sampled_at);
                self.update.gpu_temp = temperature(&selection.gpu_temp, sampled_at);
                self.update.lhm_readings = lhm::readings(&selection, sampled_at);
                self.lhm_success = Some(now);
                self.set_provider("lhm", true);
            }
            Err(e) => {
                crate::log_debug!("LHM sample failed: {}", e);
                self.set_provider("lhm", false);
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
                self.set_provider("audio", true);
            }
            Err(e) => {
                crate::log_debug!("audio query failed: {}", e);
                self.update.audio = None;
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
            apply_lhm_freshness(&mut self.update, self.lhm_success, cfg.lhm_stale_after, now);
        } else {
            self.update.cpu_temp = Metric::default();
            self.update.gpu_temp = Metric::default();
            self.update.lhm_readings.clear();
            self.lhm_success = None;
            self.remove_provider("lhm");
        }
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

fn spawn_headset(config: Arc<RwLock<Config>>, tx: mpsc::Sender<SlowEvent>) {
    std::thread::Builder::new()
        .name("telemetry-headset".into())
        .spawn(move || {
            let mut last_attempt = None;
            loop {
                let cfg = current_config(&config);
                if !enabled(&cfg, "headset") {
                    last_attempt = None;
                } else {
                    let now = Instant::now();
                    if due(last_attempt, cfg.headset_poll, now) {
                        last_attempt = Some(now);
                        if tx
                            .send(SlowEvent::Headset(headset::query(
                                cfg.headset_query_timeout,
                            )))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("headset telemetry thread");
}

fn spawn_lhm(config: Arc<RwLock<Config>>, tx: mpsc::Sender<SlowEvent>) {
    std::thread::Builder::new()
        .name("telemetry-lhm".into())
        .spawn(move || {
            let mut last_attempt = None;
            loop {
                let cfg = current_config(&config);
                if let Some(url) = lhm_url(&cfg) {
                    let now = Instant::now();
                    if due(last_attempt, cfg.lhm_interval, now) {
                        last_attempt = Some(now);
                        let result = lhm::sample(url, &cfg.lhm_sensors, Duration::from_secs(2))
                            .map(|selection| (selection, SystemTime::now()));
                        if tx.send(SlowEvent::Lhm(Box::new(result))).is_err() {
                            return;
                        }
                    }
                } else {
                    last_attempt = None;
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("LHM telemetry thread");
}

/// Start one coordinator and two persistent blocking-I/O workers.
pub fn spawn(cfg: &Config) -> Telemetry {
    let config = Arc::new(RwLock::new(cfg.clone()));
    let (tx, rx) = mpsc::channel();
    let (slow_tx, slow_rx) = mpsc::channel();
    spawn_headset(Arc::clone(&config), slow_tx.clone());
    spawn_lhm(Arc::clone(&config), slow_tx);

    let worker = Worker {
        config: Arc::clone(&config),
        tx,
        slow_rx,
        update: Update::default(),
        net: netif::NetProvider::new(),
        headset_success: None,
        lhm_success: None,
        controller_attempt: None,
        audio_attempt: None,
        net_attempt: None,
    };
    std::thread::Builder::new()
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
            while worker.tick() {
                std::thread::sleep(TICK);
            }
        })
        .expect("telemetry thread");
    Telemetry {
        updates: rx,
        config,
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
        let mut update = Update {
            cpu_temp: Metric::valid(67.0, sampled_at),
            gpu_temp: Metric::valid(74.0, sampled_at),
            lhm_readings: vec![Reading {
                freshness: Freshness::Current,
                ..Default::default()
            }],
            ..Default::default()
        };
        apply_lhm_freshness(
            &mut update,
            Some(now),
            Duration::from_secs(3),
            now + Duration::from_secs(2),
        );
        assert!(!update.cpu_temp.stale);
        assert_eq!(update.lhm_readings[0].freshness, Freshness::Current);
        apply_lhm_freshness(
            &mut update,
            Some(now),
            Duration::from_secs(3),
            now + Duration::from_secs(4),
        );
        assert!(update.cpu_temp.stale && update.gpu_temp.stale);
        assert_eq!(update.lhm_readings[0].freshness, Freshness::Stale);
        assert!(!Update::default().cpu_temp.valid);
    }

    #[test]
    fn config_update_replaces_shared_runtime_config() {
        let shared = Arc::new(RwLock::new(Config::default()));
        let (_tx, updates) = mpsc::channel();
        let telemetry = Telemetry {
            updates,
            config: Arc::clone(&shared),
        };
        let changed = Config {
            audio_enabled: false,
            controller_enabled: false,
            ..Config::default()
        };
        telemetry.update_config(&changed);
        let current = current_config(&shared);
        assert!(!current.audio_enabled);
        assert!(!current.controller_enabled);
    }
}
