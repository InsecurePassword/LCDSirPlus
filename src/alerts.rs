//! Deterministic configured alert episodes and acknowledgement.

use std::collections::HashMap;
use std::time::Instant;

use crate::config::Config;
use crate::model::{Alert, Metric, MetricKey, Snapshot};

#[derive(Default)]
pub struct Manager {
    seen: HashMap<String, Episode>,
}

#[derive(Clone)]
struct Episode {
    alert: Alert,
    first_seen: Instant,
    recovery_started: Option<Instant>,
}

impl Manager {
    pub fn clear_disabled_categories(&mut self, cfg: &Config) {
        self.seen.retain(|id, _| match id.as_str() {
            "cpu-temp" | "gpu-temp" => cfg.temperature_warning_enabled,
            "memory" | "vram" => cfg.memory_warning_enabled,
            "headset-battery" => cfg.headset_enabled && !cfg.safe_mode,
            _ => true,
        });
    }

    pub fn evaluate(&mut self, snapshot: &mut Snapshot, cfg: &Config, now: Instant) {
        let mut active = Vec::new();
        let mut observed = std::collections::HashSet::new();
        if cfg.temperature_warning_enabled {
            if metric_alert(
                &mut active,
                "cpu-temp",
                "CPU TEMP HIGH",
                snapshot.cpu_temp,
                cfg.cpu_temp_warning,
                cfg.cpu_temp_critical,
                "C",
            ) {
                observed.insert("cpu-temp");
            }
            if metric_alert(
                &mut active,
                "gpu-temp",
                "GPU TEMP HIGH",
                snapshot.gpu_temp,
                cfg.gpu_temp_warning,
                cfg.gpu_temp_critical,
                "C",
            ) {
                observed.insert("gpu-temp");
            }
        }
        if cfg.memory_warning_enabled {
            if metric_alert(
                &mut active,
                "memory",
                "MEMORY HIGH",
                snapshot
                    .readings
                    .lookup(MetricKey::RAMUtilization, "")
                    .map(|reading| reading.legacy_metric())
                    .unwrap_or_default(),
                cfg.memory_warning,
                0.0,
                "%",
            ) {
                observed.insert("memory");
            }
            if metric_alert(
                &mut active,
                "vram",
                "VRAM HIGH",
                snapshot
                    .readings
                    .lookup(MetricKey::VRAMUtilization, "")
                    .map(|reading| reading.legacy_metric())
                    .unwrap_or_default(),
                cfg.vmem_warning,
                0.0,
                "%",
            ) {
                observed.insert("vram");
            }
        }
        if cfg.headset_enabled
            && !cfg.safe_mode
            && snapshot.headset.present
            && !snapshot.headset.stale
        {
            observed.insert("headset-battery");
            if snapshot.headset.online && snapshot.headset.percent <= cfg.headset_warn_percent {
                active.push(Alert {
                    id: "headset-battery".into(),
                    title: "HEADSET BATTERY LOW".into(),
                    detail: format!("{}% remaining", snapshot.headset.percent),
                    severity: if snapshot.headset.percent <= cfg.headset_critical_percent {
                        3
                    } else {
                        2
                    },
                    acknowledged: false,
                });
            }
        }

        let active_ids: std::collections::HashSet<String> =
            active.iter().map(|alert| alert.id.clone()).collect();
        for mut alert in active {
            if let Some(old) = self.seen.get(&alert.id) {
                alert.acknowledged = old.alert.acknowledged;
                self.seen.insert(
                    alert.id.clone(),
                    Episode {
                        alert,
                        first_seen: old.first_seen,
                        recovery_started: None,
                    },
                );
            } else {
                self.seen.insert(
                    alert.id.clone(),
                    Episode {
                        alert,
                        first_seen: now,
                        recovery_started: None,
                    },
                );
            }
        }
        for id in observed {
            if !active_ids.contains(id) {
                if let Some(episode) = self.seen.get_mut(id) {
                    episode.recovery_started.get_or_insert(now);
                }
            }
        }
        self.seen.retain(|id, episode| {
            active_ids.contains(id)
                || episode.recovery_started.is_none()
                || (episode.alert.severity == 3
                    && now.saturating_duration_since(episode.recovery_started.unwrap())
                        < cfg.critical_alert_linger)
        });
        let mut episodes: Vec<&Episode> = self.seen.values().collect();
        episodes.sort_by(|a, b| {
            b.alert
                .severity
                .cmp(&a.alert.severity)
                .then_with(|| a.first_seen.cmp(&b.first_seen))
                .then_with(|| a.alert.id.cmp(&b.alert.id))
        });
        snapshot.alerts = episodes
            .into_iter()
            .map(|episode| episode.alert.clone())
            .collect();
    }

    pub fn acknowledge_highest(&mut self, snapshot: &mut Snapshot) -> bool {
        let Some(id) = snapshot
            .alerts
            .iter()
            .find(|alert| alert.severity >= 2 && !alert.acknowledged)
            .map(|alert| alert.id.clone())
        else {
            return false;
        };
        if let Some(episode) = self.seen.get_mut(&id) {
            episode.alert.acknowledged = true;
        }
        if let Some(alert) = snapshot.alerts.iter_mut().find(|alert| alert.id == id) {
            alert.acknowledged = true;
        }
        true
    }
}

fn metric_alert(
    alerts: &mut Vec<Alert>,
    id: &str,
    title: &str,
    metric: Metric,
    warning: f64,
    critical: f64,
    suffix: &str,
) -> bool {
    // A current below-threshold sample is still an observation that can retire an episode.
    if !metric.valid || metric.stale {
        return false;
    }
    let (severity, threshold) = if critical > 0.0 && metric.value >= critical {
        (3, critical)
    } else if warning > 0.0 && metric.value >= warning {
        (2, warning)
    } else {
        return true;
    };
    alerts.push(Alert {
        id: id.into(),
        title: title.into(),
        detail: format!(
            "{:.0}{suffix} (limit {:.0}{suffix})",
            metric.value, threshold
        ),
        severity,
        acknowledged: false,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Availability, Freshness, Reading};
    use std::time::{Duration, SystemTime};

    fn category_snapshot(memory_percent: f64) -> Snapshot {
        Snapshot {
            cpu_temp: Metric::valid(96.0, SystemTime::UNIX_EPOCH),
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            headset: crate::model::HeadsetBattery {
                present: true,
                online: true,
                percent: 10,
                ..Default::default()
            },
            readings: crate::model::ReadingsSnapshot {
                metrics: vec![
                    Reading::current_percent(
                        MetricKey::RAMUtilization,
                        memory_percent,
                        "ram",
                        SystemTime::UNIX_EPOCH,
                    ),
                    Reading::current_percent(
                        MetricKey::VRAMUtilization,
                        memory_percent,
                        "gpu",
                        SystemTime::UNIX_EPOCH,
                    ),
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn assert_alert_ids(mut snapshot: Snapshot, cfg: &Config, expected: &[&str]) {
        Manager::default().evaluate(&mut snapshot, cfg, Instant::now());
        let mut ids: Vec<_> = snapshot
            .alerts
            .iter()
            .map(|alert| alert.id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, expected);
    }

    #[test]
    fn unavailable_stale_linger_ack_and_rearm_are_episode_scoped() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let start = Instant::now();
        let mut snapshot = Snapshot::default();
        manager.evaluate(&mut snapshot, &cfg, start);
        assert!(snapshot.alerts.is_empty());

        snapshot.gpu_temp = Metric {
            value: 91.0,
            valid: true,
            stale: true,
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, start);
        assert!(snapshot.alerts.is_empty());
        snapshot.gpu_temp.stale = false;
        manager.evaluate(&mut snapshot, &cfg, start);
        assert_eq!(snapshot.alerts[0].id, "gpu-temp");
        assert_eq!(snapshot.alerts[0].severity, 3);
        assert!(manager.acknowledge_highest(&mut snapshot));
        assert!(snapshot.alerts[0].acknowledged);

        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(2));
        assert!(
            snapshot.alerts[0].acknowledged,
            "linger preserves episode acknowledgement"
        );
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(4));
        assert_eq!(snapshot.alerts.len(), 1);
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(5));
        assert!(snapshot.alerts.is_empty());
        snapshot.gpu_temp.value = 91.0;
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(6));
        assert!(!snapshot.alerts[0].acknowledged, "new episode rearms");
    }

    #[test]
    fn configured_alerts_sort_by_severity_then_stable_identity() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let mut snapshot = Snapshot {
            cpu_temp: Metric::valid(86.0, SystemTime::UNIX_EPOCH),
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, Instant::now());
        assert_eq!(snapshot.alerts[0].id, "gpu-temp");
        assert_eq!(snapshot.alerts[1].id, "cpu-temp");
    }

    #[test]
    fn reload_invalidation_and_long_unknown_interval_preserve_ack_with_zero_linger() {
        let mut manager = Manager::default();
        let cfg = Config {
            critical_alert_linger: Duration::ZERO,
            telemetry_interval: Duration::from_secs(60),
            ..Config::default()
        };
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        assert!(manager.acknowledge_highest(&mut snapshot));

        let reloaded = Config {
            preview_scale: 2,
            ..cfg.clone()
        };
        manager.clear_disabled_categories(&reloaded);
        snapshot = Snapshot::default();
        manager.evaluate(&mut snapshot, &reloaded, now + cfg.telemetry_interval);
        assert!(snapshot.alerts[0].acknowledged);

        snapshot.gpu_temp = Metric::valid(91.0, SystemTime::UNIX_EPOCH);
        manager.evaluate(
            &mut snapshot,
            &reloaded,
            now + cfg.telemetry_interval + Duration::from_secs(1),
        );
        assert!(snapshot.alerts[0].acknowledged);
        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(
            &mut snapshot,
            &reloaded,
            now + cfg.telemetry_interval + Duration::from_secs(2),
        );
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn unknown_categories_preserve_state_until_each_has_current_recovery() {
        let mut manager = Manager::default();
        let cfg = Config {
            critical_alert_linger: Duration::ZERO,
            ..Config::default()
        };
        let now = Instant::now();
        let mut snapshot = category_snapshot(100.0);
        manager.evaluate(&mut snapshot, &cfg, now);
        for episode in manager.seen.values_mut() {
            episode.alert.acknowledged = true;
        }

        snapshot = Snapshot::default();
        snapshot.gpu_temp = Metric {
            value: 91.0,
            valid: true,
            stale: true,
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(30));
        assert_eq!(snapshot.alerts.len(), 5);
        assert!(snapshot.alerts.iter().all(|alert| alert.acknowledged));

        snapshot.cpu_temp = Metric::valid(40.0, SystemTime::UNIX_EPOCH);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(31));
        assert!(!manager.seen.contains_key("cpu-temp"));
        assert!(manager.seen.contains_key("gpu-temp"));
        assert!(manager.seen.contains_key("memory"));
        assert!(manager.seen.contains_key("headset-battery"));

        snapshot.headset = crate::model::HeadsetBattery {
            present: true,
            online: false,
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(32));
        assert!(!manager.seen.contains_key("headset-battery"));
        assert!(manager.seen.contains_key("memory"));
    }

    #[test]
    fn observed_recovery_still_expires_if_following_samples_are_unknown() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(1));
        assert_eq!(snapshot.alerts.len(), 1);

        snapshot.gpu_temp = Metric::default();
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(4));
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn headset_disable_and_safe_mode_retire_only_headset_and_reenable_rearms() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = category_snapshot(100.0);
        manager.evaluate(&mut snapshot, &cfg, now);
        manager
            .seen
            .get_mut("headset-battery")
            .unwrap()
            .alert
            .acknowledged = true;
        manager.seen.get_mut("memory").unwrap().alert.acknowledged = true;

        let disabled = Config {
            headset_enabled: false,
            ..cfg.clone()
        };
        manager.clear_disabled_categories(&disabled);
        assert!(!manager.seen.contains_key("headset-battery"));
        assert!(manager.seen["memory"].alert.acknowledged);
        manager.evaluate(&mut snapshot, &disabled, now + Duration::from_secs(1));
        assert!(!manager.seen.contains_key("headset-battery"));

        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(2));
        assert!(!manager.seen["headset-battery"].alert.acknowledged);
        manager
            .seen
            .get_mut("headset-battery")
            .unwrap()
            .alert
            .acknowledged = true;

        let safe = Config {
            safe_mode: true,
            ..cfg
        };
        manager.clear_disabled_categories(&safe);
        assert!(!manager.seen.contains_key("headset-battery"));
        assert!(manager.seen["memory"].alert.acknowledged);
        manager.evaluate(&mut snapshot, &safe, now + Duration::from_secs(3));
        assert!(!manager.seen.contains_key("headset-battery"));
    }

    #[test]
    fn warning_recovery_is_immediate() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(85.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        assert_eq!(snapshot.alerts[0].severity, 2);

        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_millis(1));
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn critical_recovery_after_long_unknown_gets_the_full_linger() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        assert!(manager.acknowledge_highest(&mut snapshot));

        snapshot.gpu_temp = Metric::default();
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(100));
        assert!(snapshot.alerts[0].acknowledged);

        snapshot.gpu_temp = Metric::valid(40.0, SystemTime::UNIX_EPOCH);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(101));
        assert!(snapshot.alerts[0].acknowledged);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(103));
        assert_eq!(snapshot.alerts.len(), 1);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(104));
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn critical_reactivation_restarts_recovery_without_rearming_ack() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        assert!(manager.acknowledge_highest(&mut snapshot));

        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(1));
        snapshot.gpu_temp.value = 91.0;
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(3));
        assert!(snapshot.alerts[0].acknowledged);

        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(10));
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(12));
        assert_eq!(snapshot.alerts.len(), 1);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(13));
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn zero_linger_removes_critical_on_first_recovery() {
        let mut manager = Manager::default();
        let cfg = Config {
            critical_alert_linger: Duration::ZERO,
            ..Config::default()
        };
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(30));
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn active_threshold_change_updates_severity_and_recovery_policy() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, now);
        assert!(manager.acknowledge_highest(&mut snapshot));

        let raised_critical = Config {
            gpu_temp_critical: 95.0,
            ..cfg
        };
        manager.evaluate(
            &mut snapshot,
            &raised_critical,
            now + Duration::from_secs(1),
        );
        assert_eq!(snapshot.alerts[0].severity, 2);
        assert!(snapshot.alerts[0].acknowledged);

        snapshot.gpu_temp.value = 40.0;
        manager.evaluate(
            &mut snapshot,
            &raised_critical,
            now + Duration::from_secs(2),
        );
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn disabling_and_reenabling_temperature_rearms_only_temperature() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = category_snapshot(100.0);
        manager.evaluate(&mut snapshot, &cfg, now);
        manager.seen.get_mut("memory").unwrap().alert.acknowledged = true;
        manager
            .seen
            .get_mut("headset-battery")
            .unwrap()
            .alert
            .acknowledged = true;

        let disabled = Config {
            temperature_warning_enabled: false,
            ..cfg.clone()
        };
        manager.clear_disabled_categories(&disabled);
        assert!(!manager.seen.contains_key("cpu-temp"));
        assert!(!manager.seen.contains_key("gpu-temp"));
        assert!(manager.seen["headset-battery"].alert.acknowledged);
        manager.evaluate(&mut snapshot, &disabled, now + Duration::from_secs(1));
        assert!(
            snapshot
                .alerts
                .iter()
                .find(|alert| alert.id == "memory")
                .unwrap()
                .acknowledged
        );

        manager.clear_disabled_categories(&cfg);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(2));
        assert!(
            !snapshot
                .alerts
                .iter()
                .find(|alert| alert.id == "cpu-temp")
                .unwrap()
                .acknowledged
        );
        assert!(
            snapshot
                .alerts
                .iter()
                .find(|alert| alert.id == "memory")
                .unwrap()
                .acknowledged
        );
    }

    #[test]
    fn disabling_memory_clears_only_memory_episodes() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let now = Instant::now();
        let mut snapshot = category_snapshot(100.0);
        manager.evaluate(&mut snapshot, &cfg, now);
        manager.seen.get_mut("cpu-temp").unwrap().alert.acknowledged = true;
        manager.seen.get_mut("memory").unwrap().alert.acknowledged = true;
        manager.seen.get_mut("vram").unwrap().alert.acknowledged = true;

        let disabled = Config {
            memory_warning_enabled: false,
            ..cfg.clone()
        };
        manager.clear_disabled_categories(&disabled);
        assert!(!manager.seen.contains_key("memory"));
        assert!(!manager.seen.contains_key("vram"));
        assert!(manager.seen.contains_key("cpu-temp"));
        assert!(manager.seen.contains_key("gpu-temp"));
        assert!(manager.seen.contains_key("headset-battery"));

        manager.clear_disabled_categories(&cfg);
        manager.evaluate(&mut snapshot, &cfg, now + Duration::from_secs(1));
        assert!(manager.seen["cpu-temp"].alert.acknowledged);
        assert!(!manager.seen["memory"].alert.acknowledged);
        assert!(!manager.seen["vram"].alert.acknowledged);
    }

    #[test]
    fn warning_categories_are_independent_and_do_not_disable_headset_alerts() {
        for (temperature, memory, expected) in [
            (
                true,
                true,
                &["cpu-temp", "gpu-temp", "headset-battery", "memory", "vram"][..],
            ),
            (true, false, &["cpu-temp", "gpu-temp", "headset-battery"]),
            (false, true, &["headset-battery", "memory", "vram"]),
            (false, false, &["headset-battery"]),
        ] {
            let cfg = Config {
                temperature_warning_enabled: temperature,
                memory_warning_enabled: memory,
                ..Config::default()
            };
            assert_alert_ids(category_snapshot(100.0), &cfg, expected);
        }
    }

    #[test]
    fn ram_and_vram_alert_at_inclusive_thresholds_only_when_current_and_enabled() {
        let defaults = Config::default();
        assert_alert_ids(
            category_snapshot(99.99),
            &defaults,
            &["cpu-temp", "gpu-temp", "headset-battery"],
        );
        assert_alert_ids(
            category_snapshot(100.0),
            &defaults,
            &["cpu-temp", "gpu-temp", "headset-battery", "memory", "vram"],
        );

        let lower = Config {
            memory_warning: 75.0,
            vmem_warning: 75.0,
            ..Config::default()
        };
        assert_alert_ids(
            category_snapshot(75.0),
            &lower,
            &["cpu-temp", "gpu-temp", "headset-battery", "memory", "vram"],
        );

        let disabled = Config {
            memory_warning_enabled: false,
            ..Config::default()
        };
        assert_alert_ids(
            category_snapshot(100.0),
            &disabled,
            &["cpu-temp", "gpu-temp", "headset-battery"],
        );

        let mut stale = category_snapshot(100.0);
        for reading in &mut stale.readings.metrics {
            reading.freshness = Freshness::Stale;
        }
        assert_alert_ids(
            stale,
            &defaults,
            &["cpu-temp", "gpu-temp", "headset-battery"],
        );

        let mut unavailable = category_snapshot(100.0);
        for reading in &mut unavailable.readings.metrics {
            reading.availability = Availability::Unavailable;
        }
        assert_alert_ids(
            unavailable,
            &defaults,
            &["cpu-temp", "gpu-temp", "headset-battery"],
        );

        assert_alert_ids(
            category_snapshot(f64::NAN),
            &defaults,
            &["cpu-temp", "gpu-temp", "headset-battery"],
        );
    }

    #[test]
    fn acknowledgement_uses_the_first_alert_in_display_order() {
        let mut manager = Manager::default();
        let cfg = Config {
            cpu_temp_warning: 80.0,
            gpu_temp_warning: 80.0,
            cpu_temp_critical: 90.0,
            gpu_temp_critical: 90.0,
            ..Config::default()
        };
        let mut snapshot = Snapshot {
            cpu_temp: Metric::valid(81.0, SystemTime::UNIX_EPOCH),
            gpu_temp: Metric::valid(81.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, Instant::now());
        assert_eq!(snapshot.alerts[0].id, "cpu-temp");
        assert!(manager.acknowledge_highest(&mut snapshot));
        assert!(snapshot.alerts[0].acknowledged);
        assert!(!snapshot.alerts[1].acknowledged);
    }

    #[test]
    fn wall_clock_rollback_does_not_extend_linger() {
        let mut manager = Manager::default();
        let cfg = Config::default();
        let start = Instant::now();
        let mut snapshot = Snapshot {
            now: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(100)),
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        manager.evaluate(&mut snapshot, &cfg, start);
        snapshot.gpu_temp.value = 40.0;
        snapshot.now = Some(SystemTime::UNIX_EPOCH);
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(2));
        assert_eq!(snapshot.alerts.len(), 1);
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(4));
        assert_eq!(snapshot.alerts.len(), 1);
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(5));
        assert!(snapshot.alerts.is_empty());
    }
}
