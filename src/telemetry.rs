//! Slow-I/O telemetry worker: headset HID, LHM HTTP, XInput battery,
//! Core Audio state, and network throughput. Providers run on their own
//! cadences here so the render loop never blocks; results are published
//! as plain data over a channel.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::model::{ControllerBattery, HeadsetBattery, Reading};
use crate::providers::{audio, headset, lhm, netif, xinput};

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub headset: HeadsetBattery,
    pub controller: ControllerBattery,
    pub audio: Option<audio::AudioState>,
    pub net: netif::NetThroughput,
    pub lhm_readings: Vec<Reading>,
    /// Per-provider health: (name, last poll succeeded).
    pub providers: Vec<(String, bool)>,
}

struct Worker {
    cfg: Config,
    tx: mpsc::Sender<Update>,
    update: Update,
    net: netif::NetProvider,
    // Last successful sample times per provider.
    headset_at: Option<Instant>,
    lhm_at: Option<Instant>,
    controller_at: Option<Instant>,
    audio_at: Option<Instant>,
}

impl Worker {
    fn due(last: Option<Instant>, period: Duration) -> bool {
        last.map(|t| t.elapsed() >= period).unwrap_or(true)
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let system_now = SystemTime::now();

        if self.cfg.headset_enabled && Self::due(self.headset_at, self.cfg.headset_poll) {
            match headset::query() {
                Ok(status) => {
                    self.update.headset = HeadsetBattery {
                        present: true,
                        online: status.online,
                        stale: false,
                        charging: status.charging,
                        percent: status.percent,
                        raw_level: status.raw_level,
                    };
                    self.headset_at = Some(now);
                    self.set_provider("headset", true);
                }
                Err(e) => {
                    crate::log_debug!("headset query failed: {}", e);
                    self.set_provider("headset", false);
                }
            }
        }
        // Staleness: mark when no success within the configured window.
        if self.update.headset.present {
            if let Some(t) = self.headset_at {
                self.update.headset.stale = t.elapsed() > self.cfg.headset_stale_after;
            }
        }

        if Self::due(self.controller_at, self.cfg.controller_poll) {
            match xinput::query(self.cfg.controller_index) {
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
                    self.controller_at = Some(now);
                    self.set_provider("controller", true);
                }
                Ok(None) => {
                    self.update.controller = ControllerBattery::default();
                    self.controller_at = Some(now);
                    self.set_provider("controller", true);
                }
                Err(e) => {
                    crate::log_debug!("controller query failed: {}", e);
                    self.set_provider("controller", false);
                }
            }
        }

        if self.cfg.audio_enabled && Self::due(self.audio_at, self.cfg.audio_poll) {
            match audio::read() {
                Ok(state) => {
                    self.update.audio = Some(state);
                    self.audio_at = Some(now);
                    self.set_provider("audio", true);
                }
                Err(e) => {
                    crate::log_debug!("audio query failed: {}", e);
                    self.set_provider("audio", false);
                }
            }
        }

        if Self::due(None, Duration::from_secs(1)) {
            self.update.net = self.net.update();
            self.set_provider("net", true);
        }

        if self.cfg.lhm_mode != "off" && Self::due(self.lhm_at, self.cfg.lhm_interval) {
            match lhm::sample(
                &self.cfg.lhm_url,
                &self.cfg.lhm_sensors,
                Duration::from_millis(2000),
            ) {
                Ok(sel) => {
                    self.update.lhm_readings = lhm::readings(&sel, system_now);
                    self.lhm_at = Some(now);
                    self.set_provider("lhm", true);
                }
                Err(e) => {
                    crate::log_debug!("LHM sample failed: {}", e);
                    self.set_provider("lhm", false);
                }
            }
        }

        let _ = self.tx.send(self.update.clone());
    }

    fn set_provider(&mut self, name: &str, ok: bool) {
        if let Some(entry) = self.update.providers.iter_mut().find(|(n, _)| n == name) {
            entry.1 = ok;
        } else {
            self.update.providers.push((name.to_string(), ok));
        }
    }
}

/// Start the telemetry worker; receive the latest update via `try_recv`.
pub fn spawn(cfg: &Config) -> mpsc::Receiver<Update> {
    let (tx, rx) = mpsc::channel();
    let worker = Worker {
        cfg: cfg.clone(),
        tx,
        update: Update::default(),
        net: netif::NetProvider::new(),
        headset_at: None,
        lhm_at: None,
        controller_at: None,
        audio_at: None,
    };
    std::thread::Builder::new()
        .name("telemetry".into())
        .spawn(move || {
            let mut worker = worker;
            loop {
                worker.tick();
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .expect("telemetry thread");
    rx
}
