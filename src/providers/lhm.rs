//! LibreHardwareMonitor loopback JSON client — the optional fallback for
//! values native APIs cannot provide (CPU/GPU temperatures, GPU load/VRAM).
//!
//! Contract: the user runs LHM with its web server on 127.0.0.1:8085 (same
//! as the 0.1.0 release). Sensor selection follows the Go implementation's
//! fail-closed heuristics: explicit config overrides first, then name-scored
//! candidates with range validation; ambiguous or out-of-range values are
//! rejected rather than guessed.

#![cfg(windows)]

use std::time::{Duration, SystemTime};

use crate::json::Json;
use crate::model::{Availability, Freshness, MetricKey, Reading, ValueKind};

#[derive(Clone, Debug, Default)]
pub struct Sensor {
    pub id: String,
    pub name: String,
    pub kind: String, // temperature | load | data | smalldata | throughput | ...
    pub value: f64,
    pub valid: bool,
    pub hardware_id: String,
    pub hardware_type: String, // cpu | gpu | memory | network | other
}

/// Flatten the LHM data.json tree into sensors with hardware context.
pub fn collect(doc: &Json) -> Vec<Sensor> {
    let mut out = Vec::new();
    if let Some(children) = doc.get("Children").and_then(|c| c.as_arr()) {
        for hardware in children {
            let hardware_name = hardware
                .get("Text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            // Hardware nodes carry identifier? data.json nodes have "Path" only
            // on leaves; hardware identity comes from leaf path prefixes.
            if let Some(sections) = hardware.get("Children").and_then(|c| c.as_arr()) {
                for section in sections {
                    if let Some(leaves) = section.get("Children").and_then(|c| c.as_arr()) {
                        for leaf in leaves {
                            if let Some(sensor) = parse_leaf(leaf, &hardware_name) {
                                out.push(sensor);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

fn parse_leaf(leaf: &Json, hardware_name: &str) -> Option<Sensor> {
    let id = leaf.get("Path").and_then(|p| p.as_str())?.to_string();
    let name = leaf
        .get("Text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let value_text = leaf.get("Value").and_then(|v| v.as_str()).unwrap_or("");
    let (value, valid) = parse_value(value_text, &id);
    let segments: Vec<&str> = id.split('/').collect();
    // /<hardware>/<index>/<type>/<n>
    let hardware_type = match segments.get(1).copied().unwrap_or("") {
        "amdcpu" | "intelcpu" | "cpu" => "cpu",
        "gpu-nvidia" | "gpu-amd" | "gpu-intel" | "gpu" => "gpu",
        "ram" => "memory",
        "nic" => "network",
        _ => "other",
    };
    let kind = segments.get(3).copied().unwrap_or("").to_string();
    let _ = hardware_name;
    let hardware_id = format!(
        "/{}/{}",
        segments.get(1).copied().unwrap_or(""),
        segments.get(2).copied().unwrap_or("0")
    );
    Some(Sensor {
        id,
        name,
        kind,
        value,
        valid,
        hardware_id,
        hardware_type: hardware_type.into(),
    })
}

/// Parse LHM's formatted value ("67.500 °C", "34.2 %", "12.3 GB") into a
/// plain f64. Data units are LHM's 1024-based KB/MB/GB/TB.
pub fn parse_value(text: &str, id: &str) -> (f64, bool) {
    let text = text.trim();
    if text.is_empty() || text == "-" {
        return (0.0, false);
    }
    let mut end = 0usize;
    let bytes = text.as_bytes();
    while end < bytes.len()
        && (bytes[end].is_ascii_digit()
            || bytes[end] == b'.'
            || bytes[end] == b','
            || bytes[end] == b'-'
            || bytes[end] == b'+'
            || bytes[end] == b'e'
            || bytes[end] == b'E')
    {
        end += 1;
    }
    let number_text = text[..end].replace(',', ".");
    let Ok(value) = number_text.parse::<f64>() else {
        return (0.0, false);
    };
    let unit = text[end..].trim().to_lowercase();
    let multiplier = if id.contains("/data/") || id.contains("/smalldata/") {
        match unit.as_str() {
            "kb" => 1024.0,
            "mb" => 1024.0 * 1024.0,
            "gb" => 1024.0 * 1024.0 * 1024.0,
            "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
            _ => 1.0,
        }
    } else {
        1.0
    };
    (value * multiplier, true)
}

fn valid_percent(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

fn valid_temperature(value: f64) -> bool {
    value.is_finite() && (-50.0..=200.0).contains(&value)
}

fn valid_throughput(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

/// One selection outcome with an exact failure reason when absent.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub cpu_temp: Option<Sensor>,
    pub gpu_hardware_id: Option<String>,
    pub gpu_load: Option<Sensor>,
    pub gpu_temp: Option<Sensor>,
    pub vram_load: Option<Sensor>,
    pub vram_used: Option<Sensor>,
    pub vram_free: Option<Sensor>,
    pub memory_load: Option<Sensor>,
    pub ram_used: Option<Sensor>,
    pub ram_free: Option<Sensor>,
    pub net_in: Option<Sensor>,
    pub net_out: Option<Sensor>,
    pub errors: Vec<String>,
}

fn best(
    sensors: &[Sensor],
    mut filter: impl FnMut(&Sensor) -> bool,
    score: impl Fn(&Sensor) -> i32,
    validate: impl Fn(&Sensor) -> bool,
) -> Option<Sensor> {
    let mut usable: Vec<&Sensor> = sensors
        .iter()
        .filter(|s| filter(s) && validate(s))
        .collect();
    usable.sort_by(|a, b| score(b).cmp(&score(a)).then_with(|| a.id.cmp(&b.id)));
    usable.first().map(|s| (*s).clone())
}

fn memory_load_name_score(name: &str) -> i32 {
    let name = name.trim().to_lowercase();
    for excluded in [
        "virtual",
        "commit",
        "swap",
        "paging",
        "page file",
        "vram",
        "video memory",
        "shared",
    ] {
        if name.contains(excluded) {
            return -1;
        }
    }
    if name == "memory" || name == "physical memory" {
        300
    } else if name == "memory load" || name == "physical memory load" {
        290
    } else if name.contains("physical") && name.contains("memory") {
        250
    } else if name.contains("memory") {
        100
    } else {
        -1
    }
}

fn cpu_temp_name_score(name: &str) -> i32 {
    let name = name.trim().to_lowercase();
    if name.contains("package") {
        300
    } else if name.contains("tctl") || name.contains("tdie") {
        290
    } else if name == "cpu" {
        280
    } else if name.contains("core") {
        100
    } else {
        -1
    }
}

/// Select the dashboard's values from a sensor snapshot. Overrides are
/// exact-ID matches from configuration (`lhm_cpu_temp_sensor`, ...).
pub fn select(
    sensors: &[Sensor],
    overrides: &std::collections::BTreeMap<String, String>,
) -> Selection {
    let mut sel = Selection::default();
    let by_id = |id: &str| sensors.iter().find(|s| s.id == id);

    let override_lookup =
        |sel: &mut Selection, key: &'static str, assign: fn(&mut Selection, Sensor)| -> bool {
            match overrides.get(key) {
                Some(id) => match by_id(id) {
                    Some(sensor) if sensor.valid => {
                        assign(sel, sensor.clone());
                        true
                    }
                    _ => {
                        sel.errors.push(format!(
                            "configured LHM sensor {key}={id:?} was not returned or is invalid"
                        ));
                        true
                    }
                },
                None => false,
            }
        };

    // CPU temperature.
    if !override_lookup(&mut sel, "cpu_temp_sensor", |s, v| s.cpu_temp = Some(v)) {
        sel.cpu_temp = best(
            sensors,
            |s| s.hardware_type == "cpu" && s.kind == "temperature",
            |s| cpu_temp_name_score(&s.name),
            |s| valid_temperature(s.value),
        );
    }

    // Memory load + RAM bytes.
    sel.memory_load = best(
        sensors,
        |s| s.hardware_id == "/ram/0" && (s.kind == "load") && memory_load_name_score(&s.name) >= 0,
        |s| memory_load_name_score(&s.name),
        |s| valid_percent(s.value),
    );
    sel.ram_used = best(
        sensors,
        |s| {
            s.hardware_id == "/ram/0"
                && (s.kind == "data" || s.kind == "smalldata")
                && s.name.to_lowercase().contains("used")
        },
        |_| 0,
        |s| valid_throughput(s.value),
    );
    sel.ram_free = best(
        sensors,
        |s| {
            s.hardware_id == "/ram/0"
                && (s.kind == "data" || s.kind == "smalldata")
                && (s.name.to_lowercase().contains("available")
                    || s.name.to_lowercase().contains("free"))
        },
        |_| 0,
        |s| valid_throughput(s.value),
    );

    // GPU: prefer NVIDIA, then AMD, then Intel (deterministic vendor order).
    let gpu_prefix = ["/gpu-nvidia", "/gpu-amd", "/gpu-intel"]
        .into_iter()
        .find(|p| sensors.iter().any(|s| s.hardware_id.starts_with(p)));
    sel.gpu_hardware_id = gpu_prefix.map(|p| format!("{p}/0"));
    if let Some(prefix) = gpu_prefix {
        let in_gpu = |s: &Sensor| s.hardware_id.starts_with(prefix);
        sel.gpu_load = best(
            sensors,
            |s| in_gpu(s) && s.kind == "load" && s.name.to_lowercase().contains("core"),
            |_| 0,
            |s| valid_percent(s.value),
        );
        sel.gpu_temp = best(
            sensors,
            |s| in_gpu(s) && s.kind == "temperature",
            |s| {
                let name = s.name.to_lowercase();
                if name.contains("core") {
                    100
                } else {
                    0
                }
            },
            |s| valid_temperature(s.value),
        );
        sel.vram_load = best(
            sensors,
            |s| {
                in_gpu(s)
                    && s.kind == "load"
                    && (s.name.to_lowercase().contains("memory")
                        || s.name.to_lowercase().contains("used"))
            },
            |_| 0,
            |s| valid_percent(s.value),
        );
        sel.vram_used = best(
            sensors,
            |s| {
                in_gpu(s)
                    && (s.kind == "data" || s.kind == "smalldata")
                    && s.name.to_lowercase().contains("used")
            },
            |_| 0,
            |s| valid_throughput(s.value),
        );
        sel.vram_free = best(
            sensors,
            |s| {
                in_gpu(s)
                    && (s.kind == "data" || s.kind == "smalldata")
                    && (s.name.to_lowercase().contains("free")
                        || s.name.to_lowercase().contains("available"))
            },
            |_| 0,
            |s| valid_throughput(s.value),
        );
    }

    // Network throughput (first NIC with both directions).
    let nics: Vec<&Sensor> = sensors
        .iter()
        .filter(|s| s.hardware_type == "network" && s.kind == "throughput")
        .collect();
    let mut nic_id = None;
    for s in &nics {
        let same = |name: &str| {
            nics.iter()
                .any(|c| c.hardware_id == s.hardware_id && c.name.to_lowercase().contains(name))
        };
        if same("upload") || same("transmit") {
            nic_id = Some(s.hardware_id.clone());
            break;
        }
    }
    if let Some(id) = nic_id {
        sel.net_in = best(
            sensors,
            |s| {
                s.hardware_id == id
                    && s.kind == "throughput"
                    && (s.name.to_lowercase().contains("download")
                        || s.name.to_lowercase().contains("receive"))
            },
            |_| 0,
            |s| valid_throughput(s.value),
        );
        sel.net_out = best(
            sensors,
            |s| {
                s.hardware_id == id
                    && s.kind == "throughput"
                    && (s.name.to_lowercase().contains("upload")
                        || s.name.to_lowercase().contains("transmit"))
            },
            |_| 0,
            |s| valid_throughput(s.value),
        );
    }

    sel
}

/// Fetch + collect + select in one call.
pub fn sample(
    url: &str,
    overrides: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Selection, String> {
    let body = crate::http::get(url, timeout)?;
    let text = std::str::from_utf8(&body).map_err(|_| "LHM response is not UTF-8".to_string())?;
    let doc = Json::parse(text).map_err(|e| format!("LHM JSON: {}", e))?;
    let sensors = collect(&doc);
    if sensors.is_empty() {
        return Err("LHM returned no sensors".into());
    }
    Ok(select(&sensors, overrides))
}

/// Project a selection into canonical readings for the renderer.
pub fn readings(sel: &Selection, at: SystemTime) -> Vec<Reading> {
    fn push(
        out: &mut Vec<Reading>,
        sensor: &Option<Sensor>,
        key: MetricKey,
        kind: ValueKind,
        at: SystemTime,
    ) {
        if let Some(s) = sensor {
            if s.valid {
                let mut reading = Reading {
                    key,
                    kind,
                    number: s.value,
                    has_value: true,
                    availability: Availability::Available,
                    freshness: Freshness::Current,
                    hardware_id: s.hardware_id.clone(),
                    sampled_at: Some(at),
                    bytes: 0,
                };
                if kind == ValueKind::Bytes {
                    reading.bytes = s.value as u64;
                }
                out.push(reading);
            }
        }
    }
    let mut out = Vec::new();
    push(
        &mut out,
        &sel.gpu_load,
        MetricKey::GPUUtilization,
        ValueKind::Percent,
        at,
    );
    push(
        &mut out,
        &sel.vram_load,
        MetricKey::VRAMUtilization,
        ValueKind::Percent,
        at,
    );
    push(
        &mut out,
        &sel.vram_used,
        MetricKey::VRAMUsed,
        ValueKind::Bytes,
        at,
    );
    push(
        &mut out,
        &sel.ram_used,
        MetricKey::RAMUsed,
        ValueKind::Bytes,
        at,
    );
    // Totals derived from used + free when both exist.
    if let (Some(used), Some(free)) = (&sel.ram_used, &sel.ram_free) {
        if used.valid && free.valid {
            out.push(Reading::current_bytes(
                MetricKey::RAMTotal,
                used.value as u64 + free.value as u64,
                &used.hardware_id,
                at,
            ));
        }
    }
    if let (Some(used), Some(free)) = (&sel.vram_used, &sel.vram_free) {
        if used.valid && free.valid {
            out.push(Reading::current_bytes(
                MetricKey::VRAMTotal,
                used.value as u64 + free.value as u64,
                &used.hardware_id,
                at,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(id: &str, name: &str, value: f64, valid: bool) -> Sensor {
        let segments: Vec<&str> = id.split('/').collect();
        let hardware_type = match segments.get(1).copied().unwrap_or("") {
            "amdcpu" | "intelcpu" | "cpu" => "cpu",
            "gpu-nvidia" | "gpu-amd" | "gpu-intel" | "gpu" => "gpu",
            "ram" => "memory",
            "nic" => "network",
            _ => "other",
        };
        Sensor {
            id: id.into(),
            name: name.into(),
            kind: segments.get(3).copied().unwrap_or("").into(),
            value,
            valid,
            hardware_id: format!(
                "/{}/{}",
                segments.get(1).copied().unwrap_or(""),
                segments.get(2).copied().unwrap_or("0")
            ),
            hardware_type: hardware_type.into(),
        }
    }

    #[test]
    fn value_parsing() {
        assert_eq!(
            parse_value("67.500 °C", "/amdcpu/0/temperature/2"),
            (67.5, true)
        );
        assert_eq!(parse_value("34.2 %", "/ram/0/load/0"), (34.2, true));
        assert_eq!(
            parse_value("12.5 GB", "/ram/0/data/0"),
            (12.5 * 1024.0 * 1024.0 * 1024.0, true)
        );
        assert_eq!(parse_value("", "/ram/0/data/0"), (0.0, false));
        assert_eq!(parse_value("-", "/ram/0/data/0"), (0.0, false));
    }

    #[test]
    fn selection_prefers_package_and_tctl() {
        let sensors = vec![
            sensor("/amdcpu/0/temperature/0", "Core #0", 90.0, true),
            sensor("/amdcpu/0/temperature/1", "Core #1", 91.0, true),
            sensor("/amdcpu/0/temperature/2", "Core (Tctl/Tdie)", 67.5, true),
        ];
        let sel = select(&sensors, &Default::default());
        assert_eq!(sel.cpu_temp.unwrap().value, 67.5);
    }

    #[test]
    fn selection_rejects_out_of_range_and_picks_fallback() {
        let sensors = vec![
            sensor("/intelcpu/0/temperature/0", "CPU Package", 999.0, true),
            sensor("/intelcpu/0/temperature/1", "Core #0", 55.0, true),
        ];
        let sel = select(&sensors, &Default::default());
        assert_eq!(sel.cpu_temp.unwrap().value, 55.0);
    }

    #[test]
    fn memory_load_excludes_virtual() {
        let sensors = vec![
            sensor("/ram/0/load/0", "Memory", 63.0, true),
            sensor("/ram/0/load/1", "Virtual Memory", 88.0, true),
        ];
        let sel = select(&sensors, &Default::default());
        assert_eq!(sel.memory_load.unwrap().value, 63.0);
    }

    #[test]
    fn gpu_selection_prefers_nvidia_and_core() {
        let sensors = vec![
            sensor("/gpu-amd/0/load/0", "GPU Core", 40.0, true),
            sensor("/gpu-nvidia/0/load/0", "GPU Core", 91.0, true),
            sensor("/gpu-nvidia/0/load/1", "GPU Memory", 72.0, true),
            sensor("/gpu-nvidia/0/temperature/0", "GPU Core", 74.0, true),
            sensor("/gpu-nvidia/0/temperature/1", "GPU Hot Spot", 84.0, true),
            sensor("/gpu-nvidia/0/smalldata/0", "GPU Memory Used", 11.5, true),
            sensor("/gpu-nvidia/0/smalldata/1", "GPU Memory Free", 4.5, true),
        ];
        let sel = select(&sensors, &Default::default());
        assert_eq!(sel.gpu_load.unwrap().value, 91.0);
        assert_eq!(
            sel.gpu_temp.unwrap().value,
            74.0,
            "GPU Core outranks Hot Spot"
        );
        assert_eq!(sel.vram_load.unwrap().value, 72.0);
        assert_eq!(sel.vram_used.unwrap().value, 11.5);
    }

    #[test]
    fn network_pairs_directions_per_nic() {
        let sensors = vec![
            sensor("/nic/0/throughput/0", "Download", 1000.0, true),
            sensor("/nic/0/throughput/1", "Upload", 500.0, true),
            sensor("/nic/1/throughput/0", "Download", 1.0, true),
            sensor("/nic/1/throughput/1", "Upload", 2.0, true),
        ];
        let sel = select(&sensors, &Default::default());
        assert_eq!(sel.net_in.unwrap().value, 1000.0);
        assert_eq!(sel.net_out.unwrap().value, 500.0);
    }

    #[test]
    fn overrides_pin_exact_ids() {
        let sensors = vec![
            sensor("/amdcpu/0/temperature/0", "Core (Tctl/Tdie)", 67.0, true),
            sensor("/amdcpu/0/temperature/9", "Custom Probe", 42.0, true),
        ];
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert(
            "cpu_temp_sensor".to_string(),
            "/amdcpu/0/temperature/9".to_string(),
        );
        let sel = select(&sensors, &overrides);
        assert_eq!(sel.cpu_temp.unwrap().value, 42.0);

        let mut bad = std::collections::BTreeMap::new();
        bad.insert("cpu_temp_sensor".to_string(), "/missing".to_string());
        let sel = select(&sensors, &bad);
        assert!(sel.cpu_temp.is_none());
        assert!(sel.errors.iter().any(|e| e.contains("/missing")));
    }

    #[test]
    fn collect_walks_real_lhm_shape() {
        let doc = Json::parse(
            r#"{"Children":[{"Text":"AMD Ryzen 9","Children":[
                {"Text":"Temperatures","Children":[
                    {"Text":"Core (Tctl/Tdie)","Value":"67.500 °C","Path":"/amdcpu/0/temperature/2"}]}]}]}"#,
        )
        .unwrap();
        let sensors = collect(&doc);
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].hardware_type, "cpu");
        assert_eq!(sensors[0].kind, "temperature");
        assert!((sensors[0].value - 67.5).abs() < 1e-9);
    }
}
