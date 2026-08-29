//! LibreHardwareMonitor loopback JSON client — the optional fallback for
//! values native APIs cannot provide (CPU/GPU temperatures only).
//!
//! Contract: the user runs LHM with its web server on 127.0.0.1:8085 (same
//! as the 0.1.0 release). Sensor selection follows the Go implementation's
//! fail-closed heuristics: explicit config overrides first, then name-scored
//! candidates with range validation; ambiguous or out-of-range values are
//! rejected rather than guessed.

#![cfg(windows)]
#![allow(dead_code)]

use std::time::Duration;

use crate::json::Json;

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
    collect_node(doc, "", "", &mut out);
    out
}

fn collect_node(node: &Json, inherited_id: &str, inherited_type: &str, out: &mut Vec<Sensor>) {
    let hardware_id = node
        .get("HardwareId")
        .and_then(|v| v.as_str())
        .unwrap_or(inherited_id);
    let hardware_type = node
        .get("HardwareType")
        .and_then(|v| v.as_str())
        .unwrap_or(inherited_type);
    if let Some(sensor) = parse_leaf(node, hardware_id, hardware_type) {
        out.push(sensor);
    }
    if let Some(children) = node.get("Children").and_then(|c| c.as_arr()) {
        for child in children {
            collect_node(child, hardware_id, hardware_type, out);
        }
    }
}

fn parse_leaf(leaf: &Json, inherited_id: &str, inherited_type: &str) -> Option<Sensor> {
    let id = leaf
        .get("SensorId")
        .or_else(|| leaf.get("Path"))
        .and_then(|p| p.as_str())?
        .to_string();
    let name = leaf
        .get("Text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let (value, valid) = match leaf.get("RawValue").and_then(|v| v.as_f64()) {
        Some(value) => (value, value.is_finite()),
        None => parse_value(
            leaf.get("Value").and_then(|v| v.as_str()).unwrap_or(""),
            &id,
        ),
    };
    let segments: Vec<&str> = id.split('/').collect();
    // /<hardware>/<index>/<type>/<n>
    let hardware_type = match segments.get(1).copied().unwrap_or("") {
        "amdcpu" | "intelcpu" | "cpu" => "cpu",
        "gpu-nvidia" | "gpu-amd" | "gpu-intel" | "gpu" => "gpu",
        "ram" => "memory",
        "nic" => "network",
        _ => "other",
    };
    let kind = leaf
        .get("Type")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| segments.get(3).copied().unwrap_or(""))
        .to_ascii_lowercase();
    let legacy_hardware_id = format!(
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
        hardware_id: if inherited_id.is_empty() {
            legacy_hardware_id
        } else {
            inherited_id.to_string()
        },
        hardware_type: if inherited_type.is_empty() {
            hardware_type.into()
        } else {
            inherited_type.to_ascii_lowercase()
        },
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

fn valid_temperature(value: f64) -> bool {
    value.is_finite() && (-50.0..=200.0).contains(&value)
}

/// One selection outcome with an exact failure reason when absent.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub cpu_temp: Option<Sensor>,
    pub gpu_temp: Option<Sensor>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ExtendedSelectors {
    pub vrm_temp: String,
    pub chipset_temp: String,
    pub motherboard_temp: String,
    pub cpu_fan_control: String,
    pub cpu_fan_rpm: String,
    pub pump_control: String,
    pub pump_rpm: String,
    pub total_power: String,
    pub cpu_package_power: String,
    pub gpu_board_power: String,
}

#[derive(Clone, Debug, Default)]
pub struct ExtendedSample {
    pub legacy: Selection,
    pub vrm_temp: Option<Sensor>,
    pub chipset_temp: Option<Sensor>,
    pub motherboard_temp: Option<Sensor>,
    pub cpu_fan_control: Option<Sensor>,
    pub cpu_fan_rpm: Option<Sensor>,
    pub pump_control: Option<Sensor>,
    pub pump_rpm: Option<Sensor>,
    pub total_power: Option<Sensor>,
    pub cpu_package_power: Option<Sensor>,
    pub gpu_board_power: Option<Sensor>,
    pub errors: Vec<String>,
}

fn exact(
    sensors: &[Sensor],
    id: &str,
    kind: &str,
    range: std::ops::RangeInclusive<f64>,
) -> Result<Option<Sensor>, String> {
    if id.is_empty() {
        return Ok(None);
    }
    let mut matches = sensors.iter().filter(|sensor| sensor.id == id);
    let sensor = matches.next();
    if sensor.is_none() || matches.next().is_some() {
        return Err(format!("LHM sensor ID {id:?} was missing or ambiguous"));
    }
    let sensor = sensor.unwrap();
    if !sensor.valid
        || sensor.kind != kind
        || !sensor.value.is_finite()
        || !range.contains(&sensor.value)
    {
        return Err(format!("LHM sensor ID {id:?} has invalid type or value"));
    }
    Ok(Some(sensor.clone()))
}

pub fn select_extended(
    sensors: &[Sensor],
    overrides: &std::collections::BTreeMap<String, String>,
    selectors: &ExtendedSelectors,
) -> ExtendedSample {
    let mut out = ExtendedSample {
        legacy: select(sensors, overrides),
        ..Default::default()
    };
    macro_rules! assign {
        ($field:ident, $kind:literal, $max:expr) => {
            match exact(sensors, &selectors.$field, $kind, 0.0..=$max) {
                Ok(value) => out.$field = value,
                Err(error) => out.errors.push(error),
            }
        };
    }
    assign!(vrm_temp, "temperature", 200.0);
    assign!(chipset_temp, "temperature", 200.0);
    assign!(motherboard_temp, "temperature", 200.0);
    assign!(cpu_fan_control, "control", 100.0);
    assign!(cpu_fan_rpm, "fan", 100_000.0);
    assign!(pump_control, "control", 100.0);
    assign!(pump_rpm, "fan", 100_000.0);
    assign!(total_power, "power", 100_000.0);
    assign!(cpu_package_power, "power", 100_000.0);
    assign!(gpu_board_power, "power", 100_000.0);
    out
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

    let override_lookup =
        |sel: &mut Selection, key: &'static str, assign: fn(&mut Selection, Sensor)| -> bool {
            match overrides.get(key) {
                Some(id) => {
                    let mut matches = sensors.iter().filter(|sensor| sensor.id == *id);
                    match (matches.next(), matches.next()) {
                        (Some(sensor), None)
                            if sensor.valid
                                && sensor.kind == "temperature"
                                && valid_temperature(sensor.value) =>
                        {
                            assign(sel, sensor.clone());
                            true
                        }
                        _ => {
                            sel.errors.push(format!(
                            "configured LHM sensor {key}={id:?} was missing, ambiguous, or invalid"
                        ));
                            true
                        }
                    }
                }
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

    // GPU: prefer NVIDIA, then AMD, then Intel (deterministic vendor order).
    let gpu_prefix = ["/gpu-nvidia", "/gpu-amd", "/gpu-intel"]
        .into_iter()
        .find(|p| sensors.iter().any(|s| s.hardware_id.starts_with(p)));
    let gpu_overridden = override_lookup(&mut sel, "gpu_temp_sensor", |s, v| s.gpu_temp = Some(v));
    if !gpu_overridden {
        let Some(prefix) = gpu_prefix else {
            return sel;
        };
        let in_gpu = |s: &Sensor| s.hardware_id.starts_with(prefix);
        sel.gpu_temp = best(
            sensors,
            |s| in_gpu(s) && s.kind == "temperature",
            |s| i32::from(s.name.to_lowercase().contains("core")) * 100,
            |s| valid_temperature(s.value),
        );
    }

    sel
}

pub fn sample_extended(
    url: &str,
    overrides: &std::collections::BTreeMap<String, String>,
    selectors: &ExtendedSelectors,
    timeout: Duration,
) -> Result<(ExtendedSample, std::time::SystemTime), String> {
    let body = crate::http::get(url, timeout)?;
    let text = std::str::from_utf8(&body).map_err(|_| "LHM response is not UTF-8".to_string())?;
    let doc = Json::parse(text).map_err(|e| format!("LHM JSON: {e}"))?;
    let sensors = collect(&doc);
    if sensors.is_empty() {
        return Err("LHM returned no sensors".into());
    }
    Ok((
        select_extended(&sensors, overrides, selectors),
        std::time::SystemTime::now(),
    ))
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
    fn gpu_temperature_selection_prefers_nvidia_and_core_only() {
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
        assert_eq!(
            sel.gpu_temp.unwrap().value,
            74.0,
            "GPU Core outranks Hot Spot"
        );
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

        let gpu = vec![sensor(
            "/gpu-amd/0/temperature/9",
            "Custom GPU Probe",
            51.0,
            true,
        )];
        let mut gpu_override = std::collections::BTreeMap::new();
        gpu_override.insert(
            "gpu_temp_sensor".to_string(),
            "/gpu-amd/0/temperature/9".to_string(),
        );
        assert_eq!(select(&gpu, &gpu_override).gpu_temp.unwrap().value, 51.0);

        let duplicate = vec![
            sensor("/amdcpu/0/temperature/9", "First", 42.0, true),
            sensor("/amdcpu/0/temperature/9", "Second", 43.0, true),
        ];
        let selected = select(&duplicate, &overrides);
        assert!(selected.cpu_temp.is_none());
        assert!(selected
            .errors
            .iter()
            .any(|error| error.contains("ambiguous")));
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

    #[test]
    fn modern_recursive_shape_and_exact_extended_selectors_fail_closed() {
        let doc = Json::parse(
            r#"{"Children":[{"HardwareId":"board0","HardwareType":"Motherboard","Children":[
                {"Children":[{"SensorId":"/board/vrm","Type":"Temperature","RawValue":61.25,"Text":"VRM"},
                {"SensorId":"/board/power","Type":"Power","RawValue":315.5,"Text":"Total"}]}]}]}"#,
        )
        .unwrap();
        let mut sensors = collect(&doc);
        assert_eq!(sensors[0].hardware_id, "board0");
        assert_eq!(sensors[0].kind, "temperature");
        let selectors = ExtendedSelectors {
            vrm_temp: "/board/vrm".into(),
            total_power: "/board/power".into(),
            ..Default::default()
        };
        let selected = select_extended(&sensors, &Default::default(), &selectors);
        assert_eq!(selected.vrm_temp.unwrap().value, 61.25);
        assert_eq!(selected.total_power.unwrap().value, 315.5);
        sensors.push(sensors[0].clone());
        let duplicate = select_extended(&sensors, &Default::default(), &selectors);
        assert!(duplicate.vrm_temp.is_none());
        assert!(!duplicate.errors.is_empty());
    }
}
