//! Read-only HWiNFO SM2 temperature and power readings.

#![cfg(windows)]
#![allow(dead_code)]

use std::mem::size_of;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{
    OpenMutexW, ReleaseMutex, WaitForSingleObject, MUTEX_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

const SIGNATURE: u32 = u32::from_le_bytes(*b"HWiS");
const DEAD: u32 = u32::from_le_bytes(*b"DEAD");
const HEADER_LEN: usize = 44;
const SENSOR_V1_LEN: usize = 264;
const SENSOR_V2_LEN: usize = 392;
const READING_V1_LEN: usize = 316;
const READING_V2_LEN: usize = 460;
const MAX_MAPPING: usize = 64 * 1024 * 1024;
const MAX_ELEMENTS: usize = 65_536;
const SENSOR_TYPE_TEMPERATURE: u32 = 1;
const SENSOR_TYPE_POWER: u32 = 5;
const INVENTORY_MAX_AGE: Duration = Duration::from_secs(5);
const MAX_FUTURE_SKEW: Duration = Duration::from_secs(2);

const _: [(); 44] = [(); HEADER_LEN];
const _: [(); 264] = [(); SENSOR_V1_LEN];
const _: [(); 392] = [(); SENSOR_V2_LEN];
const _: [(); 316] = [(); READING_V1_LEN];
const _: [(); 460] = [(); READING_V2_LEN];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
    pub sensor_label: String,
    pub reading_label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selectors {
    pub cpu_temp: Selector,
    pub total_power: Selector,
    pub cpu_package_power: Selector,
    pub gpu_board_power: Selector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorField {
    CpuTemp,
    TotalPower,
    CpuPackagePower,
    GpuBoardPower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorIssueKind {
    Incomplete,
    MissingOrAmbiguous,
    InvalidUnitOrValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectorIssue {
    pub field: SelectorField,
    pub kind: SelectorIssueKind,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub cpu_temp_c: Option<f64>,
    pub total_power_w: Option<f64>,
    pub cpu_package_power_w: Option<f64>,
    pub gpu_board_power_w: Option<f64>,
    pub issues: Vec<SelectorIssue>,
    /// HWiNFO's actual sensor poll timestamp.
    pub sampled_at: SystemTime,
}

#[derive(Debug)]
struct Reading {
    sensor: String,
    label: String,
    sensor_type: u32,
    unit: String,
    value: f64,
}

pub fn sample(selectors: &Selectors, max_age: Duration) -> Result<Sample, String> {
    let bytes = copy_mapping(Duration::from_millis(50))?;
    let now = SystemTime::now();
    parse(&bytes, selectors, now, max_age)
}

pub fn inventory() -> Result<Vec<String>, String> {
    let bytes = copy_mapping(Duration::from_millis(50))?;
    inventory_lines(&bytes, SystemTime::now(), INVENTORY_MAX_AGE)
}

fn copy_mapping(timeout: Duration) -> Result<Vec<u8>, String> {
    unsafe {
        let mutex = OwnedHandle(
            OpenMutexW(
                SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
                false,
                w!("Global\\HWiNFO_SM2_MUTEX"),
            )
            .map_err(|_| {
                "HWiNFO is not running or shared memory is inactive; start HWiNFO Sensors and enable Shared Memory Support"
                    .to_string()
            })?,
        );
        let wait = WaitForSingleObject(mutex.0, timeout.as_millis().min(u32::MAX as u128) as u32);
        if wait == WAIT_TIMEOUT {
            return Err("HWiNFO mutex timed out".into());
        }
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            return Err(format!("HWiNFO mutex wait failed ({})", wait.0));
        }
        let _guard = MutexGuard(mutex.0);
        let mapping = OwnedHandle(
            OpenFileMappingW(FILE_MAP_READ.0, false, w!("Global\\HWiNFO_SENS_SM2"))
                .map_err(|_| {
                    "HWiNFO shared-memory mapping is inactive; start HWiNFO Sensors and enable Shared Memory Support"
                        .to_string()
                })?,
        );
        let view = MappedView(MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, 0));
        let pointer = view.0.Value;
        if pointer.is_null() {
            return Err("map HWiNFO shared memory failed".into());
        }
        let mut info = MEMORY_BASIC_INFORMATION::default();
        if VirtualQuery(
            Some(pointer),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        ) == 0
            || !(HEADER_LEN..=MAX_MAPPING).contains(&info.RegionSize)
        {
            return Err("HWiNFO mapping size is invalid".into());
        }
        Ok(std::slice::from_raw_parts(pointer.cast::<u8>(), info.RegionSize).to_vec())
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct MutexGuard(HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
        }
    }
}

struct MappedView(MEMORY_MAPPED_VIEW_ADDRESS);

impl Drop for MappedView {
    fn drop(&mut self) {
        if !self.0.Value.is_null() {
            unsafe {
                let _ = UnmapViewOfFile(self.0);
            }
        }
    }
}

fn parse(
    bytes: &[u8],
    selectors: &Selectors,
    now: SystemTime,
    max_age: Duration,
) -> Result<Sample, String> {
    let (decoded, sampled_at) = decode_readings(bytes, now, max_age)?;

    let mut issues = Vec::new();
    let cpu_temp_c = select_field(
        &decoded,
        &selectors.cpu_temp,
        SENSOR_TYPE_TEMPERATURE,
        SelectorField::CpuTemp,
        &mut issues,
    );
    let total_power_w = select_field(
        &decoded,
        &selectors.total_power,
        SENSOR_TYPE_POWER,
        SelectorField::TotalPower,
        &mut issues,
    );
    let cpu_package_power_w = select_field(
        &decoded,
        &selectors.cpu_package_power,
        SENSOR_TYPE_POWER,
        SelectorField::CpuPackagePower,
        &mut issues,
    );
    let gpu_board_power_w = select_field(
        &decoded,
        &selectors.gpu_board_power,
        SENSOR_TYPE_POWER,
        SelectorField::GpuBoardPower,
        &mut issues,
    );
    Ok(Sample {
        cpu_temp_c,
        total_power_w,
        cpu_package_power_w,
        gpu_board_power_w,
        issues,
        sampled_at,
    })
}

fn inventory_lines(
    bytes: &[u8],
    now: SystemTime,
    max_age: Duration,
) -> Result<Vec<String>, String> {
    let (mut readings, _) = decode_readings(bytes, now, max_age)?;
    readings.retain(valid_reading);
    readings.sort_by(|a, b| {
        (
            &a.sensor,
            &a.label,
            a.sensor_type,
            &a.unit,
            a.value.to_bits(),
        )
            .cmp(&(
                &b.sensor,
                &b.label,
                b.sensor_type,
                &b.unit,
                b.value.to_bits(),
            ))
    });
    Ok(readings
        .into_iter()
        .map(|reading| {
            let kind = if reading.sensor_type == SENSOR_TYPE_TEMPERATURE {
                "Temperature"
            } else {
                "Power"
            };
            format!(
                "sensor={:?} reading={:?} type={} value={} unit={:?}",
                reading.sensor, reading.label, kind, reading.value, reading.unit
            )
        })
        .collect())
}

fn decode_readings(
    bytes: &[u8],
    now: SystemTime,
    max_age: Duration,
) -> Result<(Vec<Reading>, SystemTime), String> {
    if bytes.len() < HEADER_LEN {
        return Err("HWiNFO header is truncated".into());
    }
    let signature = u32_at(bytes, 0)?;
    if signature == DEAD {
        return Err(
            "HWiNFO shared-memory mapping is inactive; start HWiNFO Sensors and enable Shared Memory Support"
                .into(),
        );
    }
    if signature != SIGNATURE {
        return Err(
            "unsupported HWiNFO shared-memory ABI signature; update LCDSirPlus or use a supported HWiNFO version"
                .into(),
        );
    }
    let version = u32_at(bytes, 4)?;
    if !matches!(version, 1 | 2) {
        return Err(format!(
            "unsupported HWiNFO layout version {version}; update LCDSirPlus or use a supported HWiNFO version"
        ));
    }
    let revision = u32_at(bytes, 8)?;
    if revision > 1 {
        return Err(format!(
            "unsupported HWiNFO layout revision {revision}; update LCDSirPlus or use a supported HWiNFO version"
        ));
    }
    let header_len = if revision == 0 { HEADER_LEN } else { 48 };
    if bytes.len() < header_len {
        return Err("HWiNFO revision header is truncated".into());
    }
    let poll = i64_at(bytes, 12)?;
    if poll <= 0 {
        return Err("HWiNFO poll timestamp is invalid".into());
    }
    let sampled_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(poll as u64))
        .ok_or("HWiNFO poll timestamp overflow")?;
    let age = match now.duration_since(sampled_at) {
        Ok(age) => age,
        Err(error) if error.duration() > MAX_FUTURE_SKEW => {
            return Err("HWiNFO poll timestamp is materially in the future".into())
        }
        Err(_) => Duration::ZERO,
    };
    if age > max_age {
        return Err(
            "HWiNFO shared memory is stale; confirm Sensors is running and Shared Memory Support is enabled"
                .into(),
        );
    }

    let sensor_stride = usize_at(bytes, 24)?;
    let reading_stride = usize_at(bytes, 36)?;
    let expected_sensor = if version == 1 {
        SENSOR_V1_LEN
    } else {
        SENSOR_V2_LEN
    };
    let expected_reading = if version == 1 {
        READING_V1_LEN
    } else {
        READING_V2_LEN
    };
    if sensor_stride < expected_sensor || reading_stride < expected_reading {
        return Err("HWiNFO advertised element stride is too small".into());
    }
    let sensors = section(
        bytes,
        usize_at(bytes, 20)?,
        sensor_stride,
        usize_at(bytes, 28)?,
    )?;
    let readings = section(
        bytes,
        usize_at(bytes, 32)?,
        reading_stride,
        usize_at(bytes, 40)?,
    )?;
    if ranges_overlap(sensors.0, sensors.1, readings.0, readings.1)
        || sensors.0 < header_len
        || readings.0 < header_len
    {
        return Err("HWiNFO sections overlap".into());
    }

    let sensor_names: Vec<_> = (0..sensors.2)
        .map(|index| text(bytes, sensors.0 + index * sensor_stride + 8, 128))
        .collect();
    let mut decoded = Vec::new();
    for index in 0..readings.2 {
        let base = readings.0 + index * reading_stride;
        let sensor_type = u32_at(bytes, base)?;
        if !matches!(sensor_type, SENSOR_TYPE_TEMPERATURE | SENSOR_TYPE_POWER) {
            continue;
        }
        let Ok(sensor_index) = usize_at(bytes, base + 4) else {
            continue;
        };
        let Some(Ok(sensor)) = sensor_names.get(sensor_index) else {
            continue;
        };
        let (Ok(label), Ok(unit), Ok(value)) = (
            text(bytes, base + 12, 128),
            text(bytes, base + 268, 16),
            f64_at(bytes, base + 284),
        ) else {
            continue;
        };
        decoded.push(Reading {
            sensor: sensor.clone(),
            label,
            sensor_type,
            unit,
            value,
        });
    }
    Ok((decoded, sampled_at))
}

fn select_field(
    readings: &[Reading],
    selector: &Selector,
    sensor_type: u32,
    field: SelectorField,
    issues: &mut Vec<SelectorIssue>,
) -> Option<f64> {
    match select(readings, selector, sensor_type) {
        Ok(value) => value,
        Err(kind) => {
            issues.push(SelectorIssue { field, kind });
            None
        }
    }
}

fn select(
    readings: &[Reading],
    selector: &Selector,
    sensor_type: u32,
) -> Result<Option<f64>, SelectorIssueKind> {
    if selector.sensor_label.is_empty() && selector.reading_label.is_empty() {
        return Ok(None);
    }
    if selector.sensor_label.is_empty() || selector.reading_label.is_empty() {
        return Err(SelectorIssueKind::Incomplete);
    }
    let mut matches = readings.iter().filter(|reading| {
        reading.sensor_type == sensor_type
            && reading.sensor == selector.sensor_label
            && reading.label == selector.reading_label
    });
    let reading = matches.next();
    if reading.is_none() || matches.next().is_some() {
        return Err(SelectorIssueKind::MissingOrAmbiguous);
    }
    let reading = reading.unwrap();
    if !valid_reading(reading) {
        return Err(SelectorIssueKind::InvalidUnitOrValue);
    }
    Ok(Some(reading.value))
}

fn valid_reading(reading: &Reading) -> bool {
    match reading.sensor_type {
        SENSOR_TYPE_TEMPERATURE => {
            matches!(reading.unit.as_str(), "C" | "°C")
                && reading.value.is_finite()
                && (-50.0..=200.0).contains(&reading.value)
        }
        SENSOR_TYPE_POWER => {
            reading.unit == "W"
                && reading.value.is_finite()
                && (0.0..=100_000.0).contains(&reading.value)
        }
        _ => false,
    }
}

fn section(
    bytes: &[u8],
    offset: usize,
    stride: usize,
    count: usize,
) -> Result<(usize, usize, usize), String> {
    if count > MAX_ELEMENTS || stride == 0 {
        return Err("HWiNFO element count or stride exceeds cap".into());
    }
    let end = offset
        .checked_add(
            stride
                .checked_mul(count)
                .ok_or("HWiNFO section size overflow")?,
        )
        .ok_or("HWiNFO section range overflow")?;
    if end > bytes.len() || end > MAX_MAPPING {
        return Err("HWiNFO section exceeds mapping".into());
    }
    Ok((offset, end, count))
}

fn ranges_overlap(a: usize, b: usize, c: usize, d: usize) -> bool {
    a < d && c < b
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        slice(bytes, offset, 4)?.try_into().unwrap(),
    ))
}

fn usize_at(bytes: &[u8], offset: usize) -> Result<usize, String> {
    usize::try_from(u32_at(bytes, offset)?).map_err(|_| "HWiNFO integer overflow".into())
}

fn i64_at(bytes: &[u8], offset: usize) -> Result<i64, String> {
    Ok(i64::from_le_bytes(
        slice(bytes, offset, 8)?.try_into().unwrap(),
    ))
}

fn f64_at(bytes: &[u8], offset: usize) -> Result<f64, String> {
    Ok(f64::from_le_bytes(
        slice(bytes, offset, 8)?.try_into().unwrap(),
    ))
}

fn text(bytes: &[u8], offset: usize, len: usize) -> Result<String, String> {
    let field = slice(bytes, offset, len)?;
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    ansi_to_string(&field[..end])
}

fn ansi_to_string(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    unsafe {
        let needed = MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        if needed <= 0 {
            return Err("decode HWiNFO original ANSI label failed".into());
        }
        let mut wide = vec![0; needed as usize];
        if MultiByteToWideChar(
            CP_ACP,
            MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
            bytes,
            Some(&mut wide),
        ) != needed
        {
            return Err("decode HWiNFO original ANSI label failed".into());
        }
        String::from_utf16(&wide).map_err(|_| "HWiNFO original ANSI label is invalid".into())
    }
}

fn slice(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], String> {
    bytes
        .get(offset..offset.checked_add(len).ok_or("HWiNFO range overflow")?)
        .ok_or_else(|| "HWiNFO data is truncated".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PCSTR;
    use windows::Win32::Globalization::WideCharToMultiByte;

    fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn acp(text: &str) -> Vec<u8> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let needed = WideCharToMultiByte(CP_ACP, 0, &wide, None, PCSTR::null(), None);
            assert!(needed > 0);
            let mut bytes = vec![0; needed as usize];
            assert_eq!(
                WideCharToMultiByte(CP_ACP, 0, &wide, Some(&mut bytes), PCSTR::null(), None,),
                needed
            );
            bytes
        }
    }
    fn fixture(version: u32) -> Vec<u8> {
        let sensor_stride = if version == 1 {
            SENSOR_V1_LEN
        } else {
            SENSOR_V2_LEN
        };
        let reading_stride = if version == 1 {
            READING_V1_LEN
        } else {
            READING_V2_LEN
        };
        let sensor_offset = 48;
        let reading_offset = sensor_offset + sensor_stride;
        let mut bytes = vec![0; reading_offset + reading_stride];
        put_u32(&mut bytes, 0, SIGNATURE);
        put_u32(&mut bytes, 4, version);
        put_u32(&mut bytes, 8, 1);
        bytes[12..20].copy_from_slice(&1000i64.to_le_bytes());
        put_u32(&mut bytes, 20, sensor_offset as u32);
        put_u32(&mut bytes, 24, sensor_stride as u32);
        put_u32(&mut bytes, 28, 1);
        put_u32(&mut bytes, 32, reading_offset as u32);
        put_u32(&mut bytes, 36, reading_stride as u32);
        put_u32(&mut bytes, 40, 1);
        bytes[sensor_offset + 8..sensor_offset + 14].copy_from_slice(b"System");
        put_u32(&mut bytes, reading_offset, SENSOR_TYPE_POWER);
        bytes[reading_offset + 12..reading_offset + 23].copy_from_slice(b"Total Power");
        bytes[reading_offset + 268] = b'W';
        bytes[reading_offset + 284..reading_offset + 292].copy_from_slice(&321.5f64.to_le_bytes());
        bytes
    }

    fn put_text(bytes: &mut [u8], at: usize, len: usize, value: &[u8]) {
        bytes[at..at + len].fill(0);
        bytes[at..at + value.len()].copy_from_slice(value);
    }

    fn put_reading(
        bytes: &mut [u8],
        at: usize,
        sensor_type: u32,
        sensor_index: u32,
        label: &[u8],
        unit: &[u8],
        value: f64,
    ) {
        put_u32(bytes, at, sensor_type);
        put_u32(bytes, at + 4, sensor_index);
        put_text(bytes, at + 12, 128, label);
        put_text(bytes, at + 268, 16, unit);
        bytes[at + 284..at + 292].copy_from_slice(&value.to_le_bytes());
    }

    fn inventory_fixture(version: u32) -> Vec<u8> {
        let sensor_stride = if version == 1 {
            SENSOR_V1_LEN
        } else {
            SENSOR_V2_LEN
        };
        let reading_stride = if version == 1 {
            READING_V1_LEN
        } else {
            READING_V2_LEN
        };
        let sensor_offset = 48;
        let reading_offset = sensor_offset + sensor_stride * 2;
        let mut bytes = vec![0; reading_offset + reading_stride * 8];
        put_u32(&mut bytes, 0, SIGNATURE);
        put_u32(&mut bytes, 4, version);
        put_u32(&mut bytes, 8, 1);
        bytes[12..20].copy_from_slice(&1000i64.to_le_bytes());
        put_u32(&mut bytes, 20, sensor_offset as u32);
        put_u32(&mut bytes, 24, sensor_stride as u32);
        put_u32(&mut bytes, 28, 2);
        put_u32(&mut bytes, 32, reading_offset as u32);
        put_u32(&mut bytes, 36, reading_stride as u32);
        put_u32(&mut bytes, 40, 8);
        put_text(&mut bytes, sensor_offset + 8, 128, b"Z \"Sensor\"\\");
        put_text(
            &mut bytes,
            sensor_offset + sensor_stride + 8,
            128,
            b"A Sensor",
        );

        let mut row = |index, sensor_type, sensor_index, label, unit, value| {
            put_reading(
                &mut bytes,
                reading_offset + reading_stride * index,
                sensor_type,
                sensor_index,
                label,
                unit,
                value,
            );
        };
        row(0, SENSOR_TYPE_POWER, 0, b"Quoted \"Power\"\\", b"W", 12.5);
        row(1, SENSOR_TYPE_TEMPERATURE, 1, b"Temperature", b"C", 60.0);
        row(2, SENSOR_TYPE_POWER, 1, b"Package Power", b"W", 80.0);
        row(3, 2, 1, b"Voltage", b"V", 1.0);
        row(4, SENSOR_TYPE_TEMPERATURE, 1, b"Fahrenheit", b"F", 140.0);
        row(5, SENSOR_TYPE_POWER, 1, b"Not finite", b"W", f64::NAN);
        row(6, SENSOR_TYPE_TEMPERATURE, 1, b"Out of range", b"C", 200.1);
        row(7, SENSOR_TYPE_POWER, 99, b"Bad sensor", b"W", 1.0);
        bytes
    }

    fn temperature_fixture(version: u32, unit: &[u8], value: f64) -> Vec<u8> {
        let mut bytes = fixture(version);
        let sensor_stride = if version == 1 {
            SENSOR_V1_LEN
        } else {
            SENSOR_V2_LEN
        };
        let reading = 48 + sensor_stride;
        put_text(&mut bytes, 56, 128, b"CPU Sensor");
        put_u32(&mut bytes, reading, SENSOR_TYPE_TEMPERATURE);
        put_text(&mut bytes, reading + 12, 128, b"CPU Temperature");
        put_text(&mut bytes, reading + 268, 16, unit);
        bytes[reading + 284..reading + 292].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    fn literal_v2_fixture() -> Vec<u8> {
        // HWiNFO SM2 v2/revision 1: header 48, sensor stride 392, reading stride 460.
        let mut bytes = vec![0; 1360];
        bytes[0..4].copy_from_slice(b"HWiS");
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..20].copy_from_slice(&1000i64.to_le_bytes());
        bytes[20..24].copy_from_slice(&48u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&392u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&440u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&460u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&2u32.to_le_bytes());
        bytes[56..66].copy_from_slice(b"CPU Sensor");

        bytes[440..444].copy_from_slice(&1u32.to_le_bytes());
        bytes[452..467].copy_from_slice(b"CPU Temperature");
        bytes[708] = b'C';
        bytes[724..732].copy_from_slice(&67.5f64.to_le_bytes());

        bytes[900..904].copy_from_slice(&5u32.to_le_bytes());
        bytes[912..923].copy_from_slice(b"Total Power");
        bytes[1168] = b'W';
        bytes[1184..1192].copy_from_slice(&321.5f64.to_le_bytes());
        bytes
    }

    fn issue(sample: &Sample, field: SelectorField) -> Option<SelectorIssueKind> {
        sample
            .issues
            .iter()
            .find(|issue| issue.field == field)
            .map(|issue| issue.kind)
    }

    fn selectors() -> Selectors {
        Selectors {
            total_power: Selector {
                sensor_label: "System".into(),
                reading_label: "Total Power".into(),
            },
            ..Default::default()
        }
    }

    fn temperature_selectors() -> Selectors {
        Selectors {
            cpu_temp: Selector {
                sensor_label: "CPU Sensor".into(),
                reading_label: "CPU Temperature".into(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn parses_packed_v1_and_v2() {
        for version in [1, 2] {
            let sample = parse(
                &fixture(version),
                &selectors(),
                UNIX_EPOCH + Duration::from_secs(1001),
                Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(sample.total_power_w, Some(321.5));
            assert!(sample.issues.is_empty());
            assert_eq!(sample.sampled_at, UNIX_EPOCH + Duration::from_secs(1000));
        }
    }

    #[test]
    fn inventories_v1_and_v2_with_sorting_escaping_and_validity_filters() {
        let expected = vec![
            r#"sensor="A Sensor" reading="Package Power" type=Power value=80 unit="W""#,
            r#"sensor="A Sensor" reading="Temperature" type=Temperature value=60 unit="C""#,
            r#"sensor="Z \"Sensor\"\\" reading="Quoted \"Power\"\\" type=Power value=12.5 unit="W""#,
        ];
        for version in [1, 2] {
            assert_eq!(
                inventory_lines(
                    &inventory_fixture(version),
                    UNIX_EPOCH + Duration::from_secs(1001),
                    Duration::from_secs(2),
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn official_v2_literals_and_independent_fixture_match_parser_contract() {
        assert_eq!(SIGNATURE, 0x5369_5748);
        assert_eq!(HEADER_LEN, 44);
        assert_eq!(SENSOR_V1_LEN, 264);
        assert_eq!(SENSOR_V2_LEN, 392);
        assert_eq!(READING_V1_LEN, 316);
        assert_eq!(READING_V2_LEN, 460);
        assert_eq!(SENSOR_TYPE_TEMPERATURE, 1);
        assert_eq!(SENSOR_TYPE_POWER, 5);

        let selectors = Selectors {
            cpu_temp: Selector {
                sensor_label: "CPU Sensor".into(),
                reading_label: "CPU Temperature".into(),
            },
            total_power: Selector {
                sensor_label: "CPU Sensor".into(),
                reading_label: "Total Power".into(),
            },
            cpu_package_power: Selector {
                sensor_label: "CPU Sensor".into(),
                reading_label: "Missing CPU Power".into(),
            },
            ..Default::default()
        };
        let sample = parse(
            &literal_v2_fixture(),
            &selectors,
            UNIX_EPOCH + Duration::from_secs(1001),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(sample.cpu_temp_c, Some(67.5));
        assert_eq!(sample.total_power_w, Some(321.5));
        assert_eq!(sample.cpu_package_power_w, None);
        assert_eq!(
            issue(&sample, SelectorField::CpuPackagePower),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );
    }

    #[test]
    fn temperature_selectors_use_original_acp_identity_not_v2_utf8_display_labels() {
        let mut bytes = temperature_fixture(2, b"C", 67.5);
        let sensor_offset = 48;
        let reading_offset = sensor_offset + SENSOR_V2_LEN;
        let sensor = "Systéme";
        let label = "Température été";
        let sensor_ansi = acp(sensor);
        let label_ansi = acp(label);
        bytes[sensor_offset + 8..sensor_offset + 136].fill(0);
        bytes[sensor_offset + 8..sensor_offset + 8 + sensor_ansi.len()]
            .copy_from_slice(&sensor_ansi);
        bytes[reading_offset + 12..reading_offset + 140].fill(0);
        bytes[reading_offset + 12..reading_offset + 12 + label_ansi.len()]
            .copy_from_slice(&label_ansi);
        bytes[sensor_offset + SENSOR_V1_LEN..sensor_offset + SENSOR_V1_LEN + 12]
            .copy_from_slice(b"Display only");
        bytes[reading_offset + READING_V1_LEN..reading_offset + READING_V1_LEN + 12]
            .copy_from_slice(b"Display only");
        let selectors = Selectors {
            cpu_temp: Selector {
                sensor_label: sensor.into(),
                reading_label: label.into(),
            },
            ..Default::default()
        };
        let sample = parse(
            &bytes,
            &selectors,
            UNIX_EPOCH + Duration::from_secs(1001),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(sample.cpu_temp_c, Some(67.5));
    }

    #[test]
    fn accepts_exact_unique_temperature_with_c_and_degree_c_units() {
        let now = UNIX_EPOCH + Duration::from_secs(1001);
        for unit in [b"C".as_slice(), acp("°C").as_slice()] {
            for value in [-50.0, 67.5, 200.0] {
                let sample = parse(
                    &temperature_fixture(1, unit, value),
                    &temperature_selectors(),
                    now,
                    Duration::from_secs(2),
                )
                .unwrap();
                assert_eq!(sample.cpu_temp_c, Some(value));
                assert_eq!(sample.sampled_at, UNIX_EPOCH + Duration::from_secs(1000));
            }
        }
    }

    #[test]
    fn temperature_selection_rejects_duplicate_wrong_type_unit_nan_range_and_case() {
        let now = UNIX_EPOCH + Duration::from_secs(1001);
        let parse_temp = |bytes: &[u8], selectors: &Selectors| {
            parse(bytes, selectors, now, Duration::from_secs(2))
        };

        let mut duplicate = temperature_fixture(1, b"C", 67.5);
        let reading = 48 + SENSOR_V1_LEN;
        let old_len = duplicate.len();
        duplicate.resize(old_len + READING_V1_LEN, 0);
        let row = duplicate[reading..reading + READING_V1_LEN].to_vec();
        duplicate[old_len..].copy_from_slice(&row);
        put_u32(&mut duplicate, 40, 2);
        let sample = parse_temp(&duplicate, &temperature_selectors()).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::CpuTemp),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );

        let mut wrong_type = temperature_fixture(1, b"W", 67.5);
        put_u32(&mut wrong_type, reading, SENSOR_TYPE_POWER);
        let sample = parse_temp(&wrong_type, &temperature_selectors()).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::CpuTemp),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );
        let sample = parse_temp(
            &temperature_fixture(1, b"V", 67.5),
            &temperature_selectors(),
        )
        .unwrap();
        assert_eq!(
            issue(&sample, SelectorField::CpuTemp),
            Some(SelectorIssueKind::InvalidUnitOrValue)
        );
        for value in [f64::NAN, -50.1, 200.1] {
            let sample = parse_temp(
                &temperature_fixture(1, b"C", value),
                &temperature_selectors(),
            )
            .unwrap();
            assert_eq!(
                issue(&sample, SelectorField::CpuTemp),
                Some(SelectorIssueKind::InvalidUnitOrValue)
            );
        }
        let mut wrong_case = temperature_selectors();
        wrong_case.cpu_temp.sensor_label = "cpu sensor".into();
        let sample = parse_temp(&temperature_fixture(1, b"C", 67.5), &wrong_case).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::CpuTemp),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );
    }

    #[test]
    fn rejects_truncation_overflow_overlap_unsupported_dead_and_stale() {
        let now = UNIX_EPOCH + Duration::from_secs(1001);
        assert!(parse(&fixture(1)[..40], &selectors(), now, Duration::from_secs(2)).is_err());
        let mut bytes = fixture(1);
        put_u32(&mut bytes, 28, u32::MAX);
        assert!(parse(&bytes, &selectors(), now, Duration::from_secs(2)).is_err());
        let mut bytes = fixture(1);
        put_u32(&mut bytes, 24, SENSOR_V1_LEN as u32 - 1);
        assert!(parse(&bytes, &selectors(), now, Duration::from_secs(2)).is_err());
        let mut bytes = fixture(1);
        put_u32(&mut bytes, 32, 48);
        assert!(parse(&bytes, &selectors(), now, Duration::from_secs(2)).is_err());
        let mut bytes = fixture(1);
        put_u32(&mut bytes, 4, 3);
        let error = parse(&bytes, &selectors(), now, Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("unsupported HWiNFO layout version 3"));
        assert!(error.contains("update LCDSirPlus"));
        let mut bytes = fixture(1);
        put_u32(&mut bytes, 0, DEAD);
        let error = parse(&bytes, &selectors(), now, Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("inactive"));
        assert!(error.contains("enable Shared Memory Support"));
        let error = parse(
            &fixture(1),
            &selectors(),
            UNIX_EPOCH + Duration::from_secs(1003),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.contains("stale"));
        assert!(error.contains("Shared Memory Support is enabled"));
        assert!(!error.contains("license"));

        let sample = parse(
            &fixture(1),
            &selectors(),
            UNIX_EPOCH + Duration::from_secs(999),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(sample.sampled_at, UNIX_EPOCH + Duration::from_secs(1000));
        let error = parse(
            &fixture(1),
            &selectors(),
            UNIX_EPOCH + Duration::from_secs(997),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.contains("materially in the future"));
    }

    #[test]
    fn rejects_ambiguous_and_invalid_power_readings() {
        let now = UNIX_EPOCH + Duration::from_secs(1001);
        let mut invalid = fixture(1);
        let reading = 48 + SENSOR_V1_LEN;
        put_u32(&mut invalid, reading + 4, 1);
        let sample = parse(&invalid, &selectors(), now, Duration::from_secs(2)).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::TotalPower),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );
        let mut invalid = fixture(1);
        put_u32(&mut invalid, reading, 1);
        let sample = parse(&invalid, &selectors(), now, Duration::from_secs(2)).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::TotalPower),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );
        let mut invalid = fixture(1);
        invalid[reading + 268] = b'V';
        let sample = parse(&invalid, &selectors(), now, Duration::from_secs(2)).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::TotalPower),
            Some(SelectorIssueKind::InvalidUnitOrValue)
        );
        let mut invalid = fixture(1);
        invalid[reading + 284..reading + 292].copy_from_slice(&f64::NAN.to_le_bytes());
        let sample = parse(&invalid, &selectors(), now, Duration::from_secs(2)).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::TotalPower),
            Some(SelectorIssueKind::InvalidUnitOrValue)
        );
        for value in [100_000.0_f64, 100_000.1] {
            let mut bounded = fixture(1);
            bounded[reading + 284..reading + 292].copy_from_slice(&value.to_le_bytes());
            let sample = parse(&bounded, &selectors(), now, Duration::from_secs(2)).unwrap();
            if value == 100_000.0 {
                assert_eq!(sample.total_power_w, Some(value));
            } else {
                assert_eq!(
                    issue(&sample, SelectorField::TotalPower),
                    Some(SelectorIssueKind::InvalidUnitOrValue)
                );
            }
        }
        let mut duplicate = fixture(1);
        let old_len = duplicate.len();
        duplicate.resize(old_len + READING_V1_LEN, 0);
        let row = duplicate[reading..reading + READING_V1_LEN].to_vec();
        duplicate[old_len..].copy_from_slice(&row);
        put_u32(&mut duplicate, 40, 2);
        let sample = parse(&duplicate, &selectors(), now, Duration::from_secs(2)).unwrap();
        assert_eq!(
            issue(&sample, SelectorField::TotalPower),
            Some(SelectorIssueKind::MissingOrAmbiguous)
        );
    }
}
