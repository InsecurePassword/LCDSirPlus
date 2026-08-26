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
    last_seen: Instant,
}

impl Manager {
    pub fn evaluate(&mut self, snapshot: &mut Snapshot, cfg: &Config, now: Instant) {
        let mut active = Vec::new();
        metric_alert(
            &mut active,
            "cpu-temp",
            "CPU TEMP HIGH",
            snapshot.cpu_temp,
            cfg.cpu_temp_warning,
            cfg.cpu_temp_critical,
            "C",
        );
        metric_alert(
            &mut active,
            "gpu-temp",
            "GPU TEMP HIGH",
            snapshot.gpu_temp,
            cfg.gpu_temp_warning,
            cfg.gpu_temp_critical,
            "C",
        );
        metric_alert(
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
        );
        metric_alert(
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
        );
        if snapshot.headset.present
            && snapshot.headset.online
            && !snapshot.headset.stale
            && snapshot.headset.percent <= cfg.headset_warn_percent
        {
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
                        last_seen: now,
                    },
                );
            } else {
                self.seen.insert(
                    alert.id.clone(),
                    Episode {
                        alert,
                        first_seen: now,
                        last_seen: now,
                    },
                );
            }
        }
        self.seen.retain(|id, episode| {
            active_ids.contains(id)
                || (episode.alert.severity >= 2
                    && now.saturating_duration_since(episode.last_seen) < cfg.critical_alert_linger)
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
) {
    if !metric.valid || metric.stale {
        return;
    }
    let (severity, threshold) = if critical > 0.0 && metric.value >= critical {
        (3, critical)
    } else if warning > 0.0 && metric.value >= warning {
        (2, warning)
    } else {
        return;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

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
        assert!(snapshot.alerts.is_empty());
        snapshot.gpu_temp.value = 91.0;
        manager.evaluate(&mut snapshot, &cfg, start + Duration::from_secs(5));
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
        assert!(snapshot.alerts.is_empty());
    }
}
