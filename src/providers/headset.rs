//! SteelSeries wireless battery status over explicitly supported USB HID collections.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidP_GetCaps,
    GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use std::sync::atomic::{AtomicU8, Ordering};

pub const STEELSERIES_VID: u16 = 0x1038;

const MAX_VISITED: usize = 256;
const MAX_CANDIDATES: usize = 64;
const READ_ATTEMPTS: usize = 5;
const CANCEL_GRACE_MS: u32 = 25;
const QUERY_ACTIVE: u8 = 1;
const QUERY_QUARANTINED: u8 = 2;

static QUERY_GATE: QueryGate = QueryGate::new();

#[derive(Debug, PartialEq, Eq)]
enum GateError {
    Busy,
    Quarantined,
}

impl GateError {
    fn message(&self) -> &'static str {
        match self {
            Self::Busy => "SteelSeries battery query already in progress",
            Self::Quarantined => {
                "SteelSeries battery provider unavailable: HID cancellation still pending"
            }
        }
    }
}

struct QueryGate {
    state: AtomicU8,
}

impl QueryGate {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
        }
    }

    fn enter(&self) -> Result<QueryGuard<'_>, GateError> {
        match self
            .state
            .compare_exchange(0, QUERY_ACTIVE, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => Ok(QueryGuard { gate: self }),
            Err(state) if state & QUERY_QUARANTINED != 0 => Err(GateError::Quarantined),
            Err(_) => Err(GateError::Busy),
        }
    }

    fn quarantine(&self) {
        let previous = self.state.fetch_or(QUERY_QUARANTINED, Ordering::SeqCst);
        debug_assert_eq!(previous, QUERY_ACTIVE);
    }

    fn permits_candidate_probe(&self) -> bool {
        self.state.load(Ordering::SeqCst) == QUERY_ACTIVE
    }

    fn reaper_complete(&self) {
        self.state.fetch_and(!QUERY_QUARANTINED, Ordering::SeqCst);
    }
}

struct QueryGuard<'a> {
    gate: &'a QueryGate,
}

impl Drop for QueryGuard<'_> {
    fn drop(&mut self) {
        self.gate.state.fetch_and(!QUERY_ACTIVE, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Protocol {
    Legacy12,
    Arctis9,
    ProWireless,
    SevenPlus,
    NovaDiscrete,
    NovaDirect,
    Nova5,
    GameBuds,
}

#[derive(Clone, Copy)]
struct Profile {
    pid: u16,
    interface_number: i32,
    usage_page: u16,
    usage: u16,
    protocol: Protocol,
}

const fn exact(
    pid: u16,
    interface_number: i32,
    page: u16,
    usage: u16,
    protocol: Protocol,
) -> Profile {
    Profile {
        pid,
        interface_number,
        usage_page: page,
        usage,
        protocol,
    }
}

static PROFILES: [Profile; 32] = [
    exact(0x12B3, 3, 0xFF43, 0x0202, Protocol::Legacy12),
    exact(0x12B6, 3, 0xFF43, 0x0202, Protocol::Legacy12),
    exact(0x12D7, 3, 0xFF43, 0x0202, Protocol::Legacy12),
    exact(0x12D5, 3, 0xFF43, 0x0202, Protocol::Legacy12),
    exact(0x12C2, 0, 0xFFC0, 0x0001, Protocol::Arctis9),
    exact(0x1290, 0, 0xFF00, 0x0001, Protocol::ProWireless),
    exact(0x220E, 3, 0xFFC0, 0x0001, Protocol::SevenPlus),
    exact(0x2212, 3, 0xFFC0, 0x0001, Protocol::SevenPlus),
    exact(0x2216, 3, 0xFFC0, 0x0001, Protocol::SevenPlus),
    exact(0x2236, 3, 0xFFC0, 0x0001, Protocol::SevenPlus),
    exact(0x2202, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x2206, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x220A, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x223A, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x227A, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x22A4, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x22AB, 3, 0xFFC0, 0x0001, Protocol::NovaDiscrete),
    exact(0x22A1, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x227E, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x2258, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x229E, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x22A9, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x22A5, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x22A7, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x2298, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x22AD, 3, 0xFFC0, 0x0001, Protocol::NovaDirect),
    exact(0x2232, 3, 0xFFC0, 0x0001, Protocol::Nova5),
    exact(0x2253, 3, 0xFFC0, 0x0001, Protocol::Nova5),
    exact(0x2264, 3, 0xFFC0, 0x0001, Protocol::Nova5),
    exact(0x2269, 3, 0xFFC0, 0x0001, Protocol::Nova5),
    exact(0x226D, 3, 0xFFC0, 0x0001, Protocol::Nova5),
    exact(0x230A, 3, 0xFFC0, 0x0001, Protocol::GameBuds),
];

struct FileHandle(HANDLE);

impl Drop for FileHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct EventHandle(HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Status {
    pub online: bool,
    pub charging: bool,
    /// Device-reported coarse level (0..4), or -1 for direct/scaled/offline values.
    pub raw_level: i32,
    pub percent: i32,
}

#[derive(Clone, Debug)]
pub struct HidInterface {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub interface_number: i32,
    pub input_len: u16,
    pub output_len: u16,
}

/// Decode the original 7+/7P+ 0xB0 response. HID may prefix one zero report ID.
pub fn parse_status(data: &[u8]) -> Result<Status, String> {
    if data.len() < 4 {
        return Err(format!(
            "no compatible SteelSeries battery status report received: response too short ({} bytes)",
            data.len()
        ));
    }
    let mut limit = data.len() - 3;
    if limit > 2 {
        limit = 2;
    }
    for offset in 0..limit {
        if data[offset] != 0xB0 {
            continue;
        }
        let (connection, raw, charge) = (data[offset + 1], data[offset + 2], data[offset + 3]);
        if connection == 0x01 {
            return Ok(Status {
                raw_level: -1,
                online: false,
                charging: false,
                percent: 0,
            });
        }
        let (percent, _granularity) = if raw <= 4 {
            (raw as i32 * 25, 25)
        } else if raw <= 100 {
            (raw as i32, 1)
        } else {
            continue;
        };
        return Ok(Status {
            online: true,
            charging: charge == 0x01,
            raw_level: raw as i32,
            percent,
        });
    }
    Err("no compatible SteelSeries battery status report received".into())
}

fn profile_for(info: &HidInterface) -> Option<&'static Profile> {
    if info.vid != STEELSERIES_VID {
        return None;
    }
    PROFILES.iter().find(|profile| {
        profile.pid == info.pid
            && profile.interface_number == info.interface_number
            && info.usage_page == profile.usage_page
            && info.usage == profile.usage
            && info.output_len as usize >= minimum_output_len(profile.protocol)
            && info.input_len as usize >= minimum_input_len(profile.protocol)
    })
}

fn minimum_output_len(protocol: Protocol) -> usize {
    match protocol {
        Protocol::ProWireless => 31,
        _ => 2,
    }
}

fn minimum_input_len(protocol: Protocol) -> usize {
    match protocol {
        Protocol::Legacy12 | Protocol::SevenPlus | Protocol::NovaDiscrete => 4,
        Protocol::Arctis9 | Protocol::NovaDirect | Protocol::Nova5 => 5,
        Protocol::ProWireless => 1,
        Protocol::GameBuds => 7,
    }
}

/// Exact profile admission with 2212 kept ahead of every other supported receiver.
pub fn candidate_score(info: &HidInterface) -> Option<i32> {
    profile_for(info).map(|_| if info.pid == 0x2212 { 1 } else { 0 })
}

/// Rank candidates best-first (stable by lowercase path on ties).
pub fn rank_candidates(mut interfaces: Vec<HidInterface>) -> Vec<HidInterface> {
    let mut scored: Vec<(i32, HidInterface)> = interfaces
        .drain(..)
        .filter_map(|i| candidate_score(&i).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.path.to_lowercase().cmp(&b.1.path.to_lowercase()))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Status,
    ProBattery,
}

#[derive(Debug, PartialEq)]
enum ParseDecision {
    Done(Status),
    Next(Step),
}

fn build_request(protocol: Protocol, step: Step, output_len: usize) -> Result<Vec<u8>, String> {
    let command: &[u8] = match (protocol, step) {
        (Protocol::Legacy12, Step::Status) => &[0x06, 0x12],
        (Protocol::Arctis9, Step::Status) => &[0x00, 0x20],
        (Protocol::ProWireless, Step::Status) => &[0x41, 0xAA],
        (Protocol::ProWireless, Step::ProBattery) => &[0x40, 0xAA],
        (Protocol::SevenPlus, Step::Status)
        | (Protocol::NovaDiscrete, Step::Status)
        | (Protocol::NovaDirect, Step::Status)
        | (Protocol::Nova5, Step::Status)
        | (Protocol::GameBuds, Step::Status) => &[0x00, 0xB0],
        _ => return Err("invalid SteelSeries battery query step".into()),
    };
    let minimum = minimum_output_len(protocol).max(command.len());
    if output_len < minimum {
        return Err(format!(
            "SteelSeries HID output report too short: {output_len} bytes, need {minimum}"
        ));
    }
    let mut output = vec![0; output_len];
    output[..command.len()].copy_from_slice(command);
    Ok(output)
}

fn parse_response(protocol: Protocol, step: Step, data: &[u8]) -> Result<ParseDecision, String> {
    match (protocol, step) {
        (Protocol::SevenPlus, Step::Status) => parse_status(data).map(ParseDecision::Done),
        (Protocol::Legacy12, Step::Status) => {
            let data = normalize_legacy(data);
            if data.len() < 4 {
                return Err("Legacy12 battery response too short".into());
            }
            if data[2] == 0x01 {
                return Ok(ParseDecision::Done(offline()));
            }
            direct(data[3], false).map(ParseDecision::Done)
        }
        (Protocol::Arctis9, Step::Status) => {
            let data = normalize_legacy(data);
            if data.len() < 5 {
                return Err("Arctis 9 battery response too short".into());
            }
            let raw = data[3];
            if !(0x64..=0x9A).contains(&raw) {
                return Err("invalid Arctis 9 battery level".into());
            }
            Ok(ParseDecision::Done(Status {
                online: true,
                charging: data[4] == 0x01,
                raw_level: -1,
                percent: (i32::from(raw) - 0x64) * 100 / (0x9A - 0x64),
            }))
        }
        (Protocol::ProWireless, Step::Status) => {
            let data = normalize_legacy(data);
            match data.first() {
                Some(0x02) => Ok(ParseDecision::Done(offline())),
                Some(0x04) => Ok(ParseDecision::Next(Step::ProBattery)),
                _ => Err("invalid Pro Wireless connection status".into()),
            }
        }
        (Protocol::ProWireless, Step::ProBattery) => {
            let data = normalize_legacy(data);
            let Some(&raw) = data.first() else {
                return Err("Pro Wireless battery response too short".into());
            };
            band(raw, false).map(ParseDecision::Done)
        }
        (Protocol::NovaDiscrete, Step::Status) => {
            let offset = marker_offset(data, 4)?;
            let state = data[offset + 3];
            if state == 0 {
                return Ok(ParseDecision::Done(offline()));
            }
            band(data[offset + 2], matches!(state, 1 | 2)).map(ParseDecision::Done)
        }
        (Protocol::NovaDirect, Step::Status) => {
            let offset = marker_offset(data, 4)?;
            let state = data[offset + 3];
            if state == 0 {
                return Ok(ParseDecision::Done(offline()));
            }
            direct(data[offset + 2], matches!(state, 1 | 2)).map(ParseDecision::Done)
        }
        (Protocol::Nova5, Step::Status) => {
            let offset = marker_offset(data, 5)?;
            nova_connection(data[offset + 1])?;
            if data[offset + 1] == 0x02 {
                return Ok(ParseDecision::Done(offline()));
            }
            direct(data[offset + 3], data[offset + 4] == 0x01).map(ParseDecision::Done)
        }
        (Protocol::GameBuds, Step::Status) => {
            let offset = marker_offset(data, 7)?;
            let left_status = data[offset + 3];
            let right_status = data[offset + 4];
            if !matches!(left_status, 0x02 | 0x03) || !matches!(right_status, 0x02 | 0x03) {
                return Err("invalid GameBuds connection status".into());
            }
            let mut percent = None;
            if left_status == 0x03 {
                percent = Some(valid_percent(data[offset + 5])?);
            }
            if right_status == 0x03 {
                let right = valid_percent(data[offset + 6])?;
                percent = Some(percent.map_or(right, |left| left.min(right)));
            }
            Ok(ParseDecision::Done(match percent {
                Some(percent) => Status {
                    online: true,
                    charging: false,
                    raw_level: -1,
                    percent,
                },
                None => offline(),
            }))
        }
        _ => Err("invalid SteelSeries battery response step".into()),
    }
}

fn normalize_legacy(data: &[u8]) -> &[u8] {
    data.strip_prefix(&[0x00]).unwrap_or(data)
}

fn marker_offset(data: &[u8], needed: usize) -> Result<usize, String> {
    for offset in 0..=1 {
        if data.get(offset) == Some(&0xB0) && data.len() >= offset + needed {
            return Ok(offset);
        }
    }
    Err("no compatible SteelSeries battery status report received".into())
}

fn nova_connection(value: u8) -> Result<(), String> {
    if matches!(value, 0x02 | 0x03) {
        Ok(())
    } else {
        Err("invalid Nova connection status".into())
    }
}

fn valid_percent(raw: u8) -> Result<i32, String> {
    if raw <= 100 {
        Ok(raw as i32)
    } else {
        Err("invalid direct battery percentage".into())
    }
}

fn direct(raw: u8, charging: bool) -> Result<Status, String> {
    Ok(Status {
        online: true,
        charging,
        raw_level: -1,
        percent: valid_percent(raw)?,
    })
}

fn band(raw: u8, charging: bool) -> Result<Status, String> {
    if raw > 4 {
        return Err("invalid four-band battery level".into());
    }
    Ok(Status {
        online: true,
        charging,
        raw_level: raw as i32,
        percent: raw as i32 * 25,
    })
}

fn offline() -> Status {
    Status {
        online: false,
        charging: false,
        raw_level: -1,
        percent: 0,
    }
}

/// Enumerate admitted SteelSeries HID interfaces with full identity details.
pub fn enumerate() -> Vec<HidInterface> {
    let mut out = Vec::new();
    unsafe {
        let Ok(hdev) = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_HID),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) else {
            return out;
        };
        let mut index = 0u32;
        loop {
            if !enumeration_has_capacity(index as usize, out.len()) {
                break;
            }
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(hdev, None, &GUID_DEVINTERFACE_HID, index, &mut iface)
                .is_err()
            {
                break;
            }
            index += 1;
            let Some(path) = interface_path(hdev, &iface) else {
                continue;
            };
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let Ok(handle) = CreateFileW(
                PCWSTR::from_raw(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
                None,
                OPEN_EXISTING,
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) else {
                continue;
            };
            if let Some(info) = inspect(handle, path).filter(|info| profile_for(info).is_some()) {
                out.push(info);
            }
            let _ = CloseHandle(handle);
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    out
}

fn enumeration_has_capacity(visited: usize, admitted: usize) -> bool {
    visited < MAX_VISITED && admitted < MAX_CANDIDATES
}

unsafe fn interface_path(hdev: HDEVINFO, iface: &SP_DEVICE_INTERFACE_DATA) -> Option<String> {
    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, iface, None, 0, Some(&mut required), None);
    if required == 0 {
        return None;
    }
    let mut buffer = vec![0u8; required as usize];
    buffer[0..4].copy_from_slice(&8u32.to_ne_bytes());
    if SetupDiGetDeviceInterfaceDetailW(
        hdev,
        iface,
        Some(buffer.as_ptr() as *mut _),
        required,
        None,
        None,
    )
    .is_err()
    {
        return None;
    }
    let wide_ptr = buffer.as_ptr().add(4) as *const u16;
    let mut len = 0usize;
    while *wide_ptr.add(len) != 0 {
        len += 1;
        if len > 1024 {
            return None;
        }
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        wide_ptr, len,
    )))
}

unsafe fn inspect(handle: HANDLE, path: String) -> Option<HidInterface> {
    let mut attrs = HIDD_ATTRIBUTES::default();
    if !HidD_GetAttributes(handle, &mut attrs).as_bool() {
        return None;
    }
    let mut preparsed = PHIDP_PREPARSED_DATA::default();
    if !HidD_GetPreparsedData(handle, &mut preparsed).as_bool() {
        return None;
    }
    let mut caps = HIDP_CAPS::default();
    let status = HidP_GetCaps(preparsed, &mut caps);
    let _ = HidD_FreePreparsedData(preparsed);
    if status.0 < 0 {
        return None;
    }
    Some(HidInterface {
        interface_number: parse_interface_number(&path),
        path,
        vid: attrs.VendorID,
        pid: attrs.ProductID,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        input_len: caps.InputReportByteLength,
        output_len: caps.OutputReportByteLength,
    })
}

/// Extract the USB interface number (`&MI_XX`) from a device path.
pub fn parse_interface_number(path: &str) -> i32 {
    let upper = path.to_uppercase();
    let Some(pos) = upper.find("&MI_") else {
        return -1;
    };
    let hex = &upper[pos + 4..];
    if hex.len() < 2 {
        return -1;
    }
    let mut value: i32 = 0;
    for ch in hex[..2].chars() {
        value <<= 4;
        match ch {
            '0'..='9' => value += ch as i32 - '0' as i32,
            'A'..='F' => value += ch as i32 - 'A' as i32 + 10,
            _ => return -1,
        }
    }
    value
}

/// One bounded status query: rank, then try each exact candidate under one deadline.
pub fn query(timeout: std::time::Duration) -> Result<Status, String> {
    let _guard = QUERY_GATE.enter().map_err(|error| error.message())?;
    let candidates = rank_candidates(enumerate());
    let Some(_best) = candidates.first() else {
        return Err("compatible SteelSeries wireless receiver not found".into());
    };
    let deadline = std::time::Instant::now() + timeout;
    probe_candidates(&QUERY_GATE, &candidates, deadline, probe)
}

fn probe_candidates(
    gate: &QueryGate,
    candidates: &[HidInterface],
    deadline: std::time::Instant,
    mut probe_candidate: impl FnMut(&HidInterface, std::time::Instant) -> Result<Status, String>,
) -> Result<Status, String> {
    let mut last_error = String::new();
    for candidate in candidates {
        if !gate.permits_candidate_probe() {
            if last_error.is_empty() {
                return Err(GateError::Quarantined.message().into());
            }
            return Err(last_error);
        }
        match probe_candidate(candidate, deadline) {
            Ok(status) => return Ok(status),
            Err(error) => {
                last_error = error;
                if !gate.permits_candidate_probe() {
                    return Err(last_error);
                }
            }
        }
    }
    if last_error.is_empty() {
        last_error = "no compatible SteelSeries battery status report received".into();
    }
    Err(last_error)
}

fn probe(info: &HidInterface, deadline: std::time::Instant) -> Result<Status, String> {
    unsafe {
        let wide: Vec<u16> = info.path.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
        .map_err(|e| format!("open shared SteelSeries HID interface: {}", e))?;
        probe_handle(FileHandle(handle), info, deadline)
    }
}

unsafe fn probe_handle(
    mut file: FileHandle,
    info: &HidInterface,
    deadline: std::time::Instant,
) -> Result<Status, String> {
    let profile = profile_for(info).ok_or("SteelSeries HID interface no longer matches profile")?;
    let mut step = Step::Status;
    file = write_request(file, profile.protocol, step, info.output_len, deadline)?;

    let mut last_error = String::new();
    for _ in 0..READ_ATTEMPTS {
        let (next_file, mut buffer, transferred) =
            read_once(file, info.input_len as usize, deadline)?;
        file = next_file;
        buffer.truncate(transferred);
        match parse_response(profile.protocol, step, &buffer) {
            Ok(ParseDecision::Done(status)) => return Ok(status),
            Ok(ParseDecision::Next(next)) => {
                step = next;
                file = write_request(file, profile.protocol, step, info.output_len, deadline)?;
            }
            Err(e) => last_error = e,
        }
    }
    if last_error.is_empty() {
        last_error = "no compatible SteelSeries battery status report received".into();
    }
    Err(last_error)
}

unsafe fn write_request(
    file: FileHandle,
    protocol: Protocol,
    step: Step,
    output_len: u16,
    deadline: std::time::Instant,
) -> Result<FileHandle, String> {
    let output = build_request(protocol, step, output_len as usize)?;
    let expected = output.len();
    let (file, _, transferred) =
        run_overlapped(file, output, IoKind::Write, deadline, "status request")?;
    if transferred != expected {
        return Err("send status request: short HID write".into());
    }
    Ok(file)
}

fn remaining_ms(deadline: std::time::Instant) -> Result<u32, String> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err("status query timed out".into());
    }
    Ok(remaining.as_millis().clamp(1, u32::MAX as u128) as u32)
}

unsafe fn read_once(
    file: FileHandle,
    input_len: usize,
    deadline: std::time::Instant,
) -> Result<(FileHandle, Vec<u8>, usize), String> {
    run_overlapped(
        file,
        vec![0; input_len],
        IoKind::Read,
        deadline,
        "status response",
    )
}

#[derive(Clone, Copy)]
enum IoKind {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionEvent {
    Immediate,
    PendingComplete,
    PendingDeadline,
    CancellationComplete,
    CancellationUnresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnershipAction {
    Finish,
    Cancel,
    ReclaimCanceled,
    Quarantine,
}

fn ownership_action(event: CompletionEvent) -> OwnershipAction {
    match event {
        CompletionEvent::Immediate | CompletionEvent::PendingComplete => OwnershipAction::Finish,
        CompletionEvent::PendingDeadline => OwnershipAction::Cancel,
        CompletionEvent::CancellationComplete => OwnershipAction::ReclaimCanceled,
        CompletionEvent::CancellationUnresolved => OwnershipAction::Quarantine,
    }
}

struct PendingIo {
    file: FileHandle,
    event: EventHandle,
    overlapped: OVERLAPPED,
    buffer: Vec<u8>,
}

unsafe fn run_overlapped(
    file: FileHandle,
    buffer: Vec<u8>,
    kind: IoKind,
    deadline: std::time::Instant,
    operation: &str,
) -> Result<(FileHandle, Vec<u8>, usize), String> {
    let event = EventHandle(
        CreateEventW(None, false, false, None).map_err(|e| format!("CreateEventW: {}", e))?,
    );
    let mut state = Box::pin(PendingIo {
        file,
        overlapped: OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        },
        event,
        buffer,
    });
    let wait_ms = remaining_ms(deadline)?;
    let submitted = {
        let state = std::pin::Pin::as_mut(&mut state).get_unchecked_mut();
        match kind {
            IoKind::Read => windows::Win32::Storage::FileSystem::ReadFile(
                state.file.0,
                Some(&mut state.buffer),
                None,
                Some(&mut state.overlapped),
            ),
            IoKind::Write => windows::Win32::Storage::FileSystem::WriteFile(
                state.file.0,
                Some(&state.buffer),
                None,
                Some(&mut state.overlapped),
            ),
        }
    };
    match submitted {
        Ok(()) => {
            debug_assert_eq!(
                ownership_action(CompletionEvent::Immediate),
                OwnershipAction::Finish
            );
            finish_io(state, operation)
        }
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            match WaitForSingleObject(state.event.0, wait_ms) {
                WAIT_OBJECT_0 => {
                    debug_assert_eq!(
                        ownership_action(CompletionEvent::PendingComplete),
                        OwnershipAction::Finish
                    );
                    finish_io(state, operation)
                }
                WAIT_TIMEOUT => {
                    debug_assert_eq!(
                        ownership_action(CompletionEvent::PendingDeadline),
                        OwnershipAction::Cancel
                    );
                    cancel_or_reap(state, format!("{operation} timed out"))
                }
                _ => {
                    debug_assert_eq!(
                        ownership_action(CompletionEvent::PendingDeadline),
                        OwnershipAction::Cancel
                    );
                    cancel_or_reap(state, format!("{operation} wait failed"))
                }
            }
        }
        Err(e) => Err(format!("{operation}: {e}")),
    }
}

unsafe fn finish_io(
    state: std::pin::Pin<Box<PendingIo>>,
    operation: &str,
) -> Result<(FileHandle, Vec<u8>, usize), String> {
    let mut transferred = 0u32;
    GetOverlappedResult(state.file.0, &state.overlapped, &mut transferred, false)
        .map_err(|e| format!("{operation} completion: {e}"))?;
    let state = std::pin::Pin::into_inner(state);
    let state = *state;
    Ok((state.file, state.buffer, transferred as usize))
}

unsafe fn cancel_or_reap<T>(
    state: std::pin::Pin<Box<PendingIo>>,
    error: String,
) -> Result<T, String> {
    let _ = CancelIoEx(state.file.0, Some(&state.overlapped));
    let event = if WaitForSingleObject(state.event.0, CANCEL_GRACE_MS) == WAIT_OBJECT_0 {
        CompletionEvent::CancellationComplete
    } else {
        CompletionEvent::CancellationUnresolved
    };
    match ownership_action(event) {
        OwnershipAction::ReclaimCanceled => {
            let mut transferred = 0;
            let _ = GetOverlappedResult(state.file.0, &state.overlapped, &mut transferred, false);
        }
        OwnershipAction::Quarantine => reap_pending(state),
        _ => unreachable!(),
    }
    Err(error)
}

unsafe fn reap_pending(state: std::pin::Pin<Box<PendingIo>>) {
    QUERY_GATE.quarantine();
    let raw = Box::into_raw(std::pin::Pin::into_inner_unchecked(state)) as usize;
    if std::thread::Builder::new()
        .name("steelseries-hid-reaper".into())
        .spawn(move || {
            let state = unsafe { Box::from_raw(raw as *mut PendingIo) };
            let mut transferred = 0;
            unsafe {
                let _ =
                    GetOverlappedResult(state.file.0, &state.overlapped, &mut transferred, true);
            }
            drop(state);
            QUERY_GATE.reaper_complete();
        })
        .is_err()
    {
        // ponytail: retain one pending I/O after reaper spawn failure; process restart recovers it.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn iface(pid: u16, page: u16, usage: u16, mi: i32) -> HidInterface {
        HidInterface {
            path: format!(r"\\?\hid#vid_1038&pid_{pid:04x}&mi_{:02x}#test", mi.max(0)),
            vid: STEELSERIES_VID,
            pid,
            usage_page: page,
            usage,
            interface_number: mi,
            input_len: 65,
            output_len: 65,
        }
    }

    fn matching(profile: Profile) -> HidInterface {
        iface(
            profile.pid,
            profile.usage_page,
            profile.usage,
            profile.interface_number,
        )
    }

    fn parsed(protocol: Protocol, data: &[u8]) -> Result<Status, String> {
        match parse_response(protocol, Step::Status, data)? {
            ParseDecision::Done(status) => Ok(status),
            ParseDecision::Next(_) => Err("unexpected next step".into()),
        }
    }

    #[test]
    fn profiles_map_all_32_pids_and_8_protocols_exactly() {
        assert_eq!(PROFILES.len(), 32);
        assert_eq!(
            PROFILES
                .iter()
                .map(|profile| profile.pid)
                .collect::<HashSet<_>>()
                .len(),
            32
        );
        assert_eq!(
            PROFILES
                .iter()
                .map(|profile| profile.protocol)
                .collect::<HashSet<_>>()
                .len(),
            8
        );
        for profile in PROFILES {
            let info = matching(profile);
            assert_eq!(
                profile_for(&info).map(|p| p.protocol),
                Some(profile.protocol)
            );
        }
    }

    #[test]
    fn excluded_unknown_and_name_only_devices_are_rejected() {
        let excluded = [
            0x1260, 0x12AD, 0x12E0, 0x12E5, 0x225D, 0x1252, 0x1280, 0x12EC, 0x220C, 0x2200, 0x2204,
            0x2208, 0x2267, 0x230C, 0x231A, 0x1292, 0x1297,
        ];
        for pid in excluded.into_iter().chain([0xFFFF]) {
            let mut name_only = iface(pid, 0xFFC0, 1, 3);
            name_only.path = "Arctis 7P+".into();
            assert!(profile_for(&name_only).is_none());
        }
        let mut wrong_vid = matching(PROFILES[0]);
        wrong_vid.vid = 0x1234;
        assert!(profile_for(&wrong_vid).is_none());
    }

    #[test]
    fn selectors_and_report_lengths_fail_closed() {
        for profile in PROFILES {
            let mut info = matching(profile);
            info.interface_number += 1;
            assert!(profile_for(&info).is_none());

            let mut info = matching(profile);
            info.usage_page = 0x000C;
            assert!(profile_for(&info).is_none());

            let mut info = matching(profile);
            info.usage ^= 1;
            assert!(profile_for(&info).is_none());

            let mut info = matching(profile);
            info.output_len = (minimum_output_len(profile.protocol) - 1) as u16;
            assert!(profile_for(&info).is_none());

            let mut info = matching(profile);
            info.input_len = (minimum_input_len(profile.protocol) - 1) as u16;
            assert!(profile_for(&info).is_none());
        }
    }

    #[test]
    fn enumeration_caps_visited_and_admitted_independently() {
        assert!(enumeration_has_capacity(MAX_VISITED - 1, 0));
        assert!(enumeration_has_capacity(200, MAX_CANDIDATES - 1));
        assert!(!enumeration_has_capacity(MAX_VISITED, 0));
        assert!(!enumeration_has_capacity(0, MAX_CANDIDATES));
    }

    #[test]
    fn query_gate_serializes_releases_and_recovers_after_reaping() {
        let gate = QueryGate::new();
        let first = gate.enter().unwrap();
        assert!(matches!(gate.enter(), Err(GateError::Busy)));
        drop(first);

        let second = gate.enter().unwrap();
        gate.quarantine();
        assert!(matches!(gate.enter(), Err(GateError::Quarantined)));
        drop(second);
        assert!(matches!(gate.enter(), Err(GateError::Quarantined)));

        gate.reaper_complete();
        assert!(gate.enter().is_ok());

        let reaper_first = gate.enter().unwrap();
        gate.quarantine();
        gate.reaper_complete();
        assert!(matches!(gate.enter(), Err(GateError::Busy)));
        drop(reaper_first);
        assert!(gate.enter().is_ok());
    }

    #[test]
    fn candidate_fallback_obeys_quarantine_state() {
        let candidates = [matching(PROFILES[0]), matching(PROFILES[1])];
        let deadline = std::time::Instant::now();

        let gate = QueryGate::new();
        let _query = gate.enter().unwrap();
        assert!(gate.permits_candidate_probe());
        let mut attempts = 0;
        let status = probe_candidates(&gate, &candidates, deadline, |_, _| {
            attempts += 1;
            if attempts == 1 {
                Err("malformed response".into())
            } else {
                Ok(Status {
                    online: true,
                    percent: 50,
                    ..Default::default()
                })
            }
        })
        .unwrap();
        assert_eq!((attempts, status.percent), (2, 50));

        let gate = QueryGate::new();
        let _query = gate.enter().unwrap();
        let mut attempts = 0;
        let error = probe_candidates(&gate, &candidates, deadline, |_, _| {
            attempts += 1;
            gate.quarantine();
            Err("status response timed out".into())
        })
        .unwrap_err();
        assert_eq!((attempts, error.as_str()), (1, "status response timed out"));
        assert!(!gate.permits_candidate_probe());

        let gate = QueryGate::new();
        let _query = gate.enter().unwrap();
        let mut attempts = 0;
        let status = probe_candidates(&gate, &candidates, deadline, |_, _| {
            attempts += 1;
            if attempts == 1 {
                gate.quarantine();
                gate.reaper_complete();
                Err("status response timed out".into())
            } else {
                Ok(Status {
                    online: true,
                    percent: 75,
                    ..Default::default()
                })
            }
        })
        .unwrap();
        assert_eq!((attempts, status.percent), (2, 75));
        assert!(gate.permits_candidate_probe());
    }

    #[test]
    fn spawn_failure_leaves_one_permanent_quarantine() {
        let gate = QueryGate::new();
        let query = gate.enter().unwrap();
        gate.quarantine();
        drop(query);

        assert_eq!(gate.state.load(Ordering::SeqCst), QUERY_QUARANTINED);
        assert!(matches!(gate.enter(), Err(GateError::Quarantined)));
    }

    #[test]
    fn completion_actions_preserve_pending_ownership() {
        assert_eq!(
            ownership_action(CompletionEvent::Immediate),
            OwnershipAction::Finish
        );
        assert_eq!(
            ownership_action(CompletionEvent::PendingComplete),
            OwnershipAction::Finish
        );
        assert_eq!(
            ownership_action(CompletionEvent::PendingDeadline),
            OwnershipAction::Cancel
        );
        assert_eq!(
            ownership_action(CompletionEvent::CancellationComplete),
            OwnershipAction::ReclaimCanceled
        );
        assert_eq!(
            ownership_action(CompletionEvent::CancellationUnresolved),
            OwnershipAction::Quarantine
        );
    }

    #[test]
    fn pid_2212_candidate_priority_and_requests_are_locked() {
        let target = iface(0x2212, 0xFFC0, 1, 3);
        assert_eq!(candidate_score(&target), Some(1));
        assert_eq!(profile_for(&target).unwrap().protocol, Protocol::SevenPlus);
        assert_eq!(
            build_request(Protocol::SevenPlus, Step::Status, 2).unwrap(),
            [0, 0xB0]
        );
        let request = build_request(Protocol::SevenPlus, Step::Status, 65).unwrap();
        assert_eq!(&request[..2], &[0, 0xB0]);
        assert!(request[2..].iter().all(|&byte| byte == 0));
        assert!(build_request(Protocol::SevenPlus, Step::Status, 1).is_err());

        let ranked = rank_candidates(vec![matching(PROFILES[0]), target]);
        assert_eq!(ranked[0].pid, 0x2212);
        assert_eq!(READ_ATTEMPTS, 5);
    }

    #[test]
    #[ignore = "requires a powered-on 1038:2212 headset and sends one bounded HID status request"]
    fn live_1038_2212_status_query() {
        let _guard = QUERY_GATE
            .enter()
            .expect("SteelSeries query gate unavailable");
        let mut exact = enumerate().into_iter().filter(|info| {
            info.vid == STEELSERIES_VID
                && info.pid == 0x2212
                && info.interface_number == 3
                && info.usage_page == 0xFFC0
                && info.usage == 1
        });
        let target = exact
            .next()
            .expect("exact 1038:2212 MI3 FFC0:1 HID interface not present");
        assert!(
            exact.next().is_none(),
            "multiple exact 1038:2212 interfaces"
        );

        let status = probe(
            &target,
            std::time::Instant::now() + std::time::Duration::from_millis(1200),
        )
        .expect("bounded 1038:2212 status query failed");
        assert!(status.online, "1038:2212 headset is offline");
        assert!((0..=100).contains(&status.percent));
        println!(
            "1038:2212 charging={} raw={} percent={}",
            status.charging, status.raw_level, status.percent
        );
    }

    #[test]
    fn seven_plus_parse_status_regression_cases() {
        for raw in 0..=4 {
            let status = parse_status(&[0xB0, 0x03, raw, 0]).unwrap();
            assert_eq!(
                (status.raw_level, status.percent),
                (raw as i32, raw as i32 * 25)
            );
        }
        for raw in [5, 100] {
            let status = parse_status(&[0xB0, 0, raw, 0]).unwrap();
            assert_eq!((status.raw_level, status.percent), (raw as i32, raw as i32));
        }
        assert!(parse_status(&[0xB0, 3, 101, 0]).is_err());
        assert_eq!(parse_status(&[0xB0, 1, 4, 1]).unwrap(), offline());
        assert!(parse_status(&[0xB0, 3, 4, 1]).unwrap().charging);
        assert_eq!(parse_status(&[0, 0xB0, 3, 4, 0]).unwrap().percent, 100);
        assert!(parse_status(&[0x77, 0xB0, 3, 4, 0]).is_ok());
        assert!(parse_status(&[0x77, 0x77, 0xB0, 3, 4, 0]).is_err());
        assert!(parse_status(&[0x77, 0x77, 0x77, 0x77]).is_err());
        assert!(parse_status(&[0xB0, 0, 4]).is_err());
    }

    #[test]
    fn legacy_protocol_parsers_normalize_one_zero_report_id() {
        assert_eq!(
            parsed(Protocol::Legacy12, &[6, 0, 0, 0]).unwrap().percent,
            0
        );
        assert_eq!(
            parsed(Protocol::Legacy12, &[0, 6, 0, 0, 100])
                .unwrap()
                .percent,
            100
        );
        assert_eq!(
            parsed(Protocol::Legacy12, &[6, 0, 1, 100]).unwrap(),
            offline()
        );
        assert!(parsed(Protocol::Legacy12, &[6, 0, 0, 101]).is_err());
        assert!(parsed(Protocol::Legacy12, &[6, 0, 0]).is_err());

        let low = parsed(Protocol::Arctis9, &[6, 0, 0, 0x64, 0]).unwrap();
        let high = parsed(Protocol::Arctis9, &[0, 6, 0, 0, 0x9A, 1]).unwrap();
        assert_eq!((low.raw_level, low.percent), (-1, 0));
        assert_eq!((high.percent, high.charging), (100, true));
        assert!(parsed(Protocol::Arctis9, &[6, 0, 0, 0x63, 0]).is_err());
        assert!(parsed(Protocol::Arctis9, &[6, 0, 0, 0x64]).is_err());
    }

    #[test]
    fn pro_wireless_step_order_offline_and_bands() {
        assert_eq!(
            parse_response(Protocol::ProWireless, Step::Status, &[0, 4]).unwrap(),
            ParseDecision::Next(Step::ProBattery)
        );
        assert_eq!(
            parse_response(Protocol::ProWireless, Step::Status, &[2]).unwrap(),
            ParseDecision::Done(offline())
        );
        assert!(parse_response(Protocol::ProWireless, Step::Status, &[3]).is_err());
        for raw in 0..=4 {
            let ParseDecision::Done(status) =
                parse_response(Protocol::ProWireless, Step::ProBattery, &[0, raw]).unwrap()
            else {
                panic!("battery step did not finish")
            };
            assert_eq!(
                (status.raw_level, status.percent),
                (raw as i32, raw as i32 * 25)
            );
        }
        assert!(parse_response(Protocol::ProWireless, Step::ProBattery, &[5]).is_err());
        assert!(parse_response(Protocol::ProWireless, Step::ProBattery, &[]).is_err());
        assert_eq!(
            build_request(Protocol::ProWireless, Step::Status, 31).unwrap()[..2],
            [0x41, 0xAA]
        );
        assert_eq!(
            build_request(Protocol::ProWireless, Step::ProBattery, 31).unwrap()[..2],
            [0x40, 0xAA]
        );
        assert!(build_request(Protocol::ProWireless, Step::Status, 30).is_err());
    }

    #[test]
    fn nova_discrete_direct_and_five_parsers_are_strict() {
        for state in [1, 2] {
            let status = parsed(Protocol::NovaDiscrete, &[0, 0xB0, 0xFF, 4, state]).unwrap();
            assert_eq!(
                (status.raw_level, status.percent, status.charging),
                (4, 100, true)
            );
        }
        let ordinary = parsed(Protocol::NovaDiscrete, &[0xB0, 0, 3, 0x7F]).unwrap();
        assert_eq!((ordinary.percent, ordinary.charging), (75, false));
        assert_eq!(
            parsed(Protocol::NovaDiscrete, &[0xB0, 0xFF, 5, 0]).unwrap(),
            offline()
        );
        assert!(parsed(Protocol::NovaDiscrete, &[0xB0, 0, 5, 3]).is_err());
        assert!(parsed(Protocol::NovaDiscrete, &[0xB0, 3, 4]).is_err());

        for state in [1, 2] {
            let status = parsed(Protocol::NovaDirect, &[0xB0, 0xFF, 100, state]).unwrap();
            assert_eq!(
                (status.raw_level, status.percent, status.charging),
                (-1, 100, true)
            );
        }
        let ordinary = parsed(Protocol::NovaDirect, &[0xB0, 0, 50, 3]).unwrap();
        assert_eq!((ordinary.percent, ordinary.charging), (50, false));
        assert_eq!(
            parsed(Protocol::NovaDirect, &[0xB0, 0xFF, 101, 0]).unwrap(),
            offline()
        );
        assert!(parsed(Protocol::NovaDirect, &[0xB0, 0, 101, 3]).is_err());
        assert!(parsed(Protocol::NovaDirect, &[0, 0, 0xB0, 3, 50, 0]).is_err());

        let nova5 = parsed(Protocol::Nova5, &[0, 0xB0, 3, 0, 0, 1]).unwrap();
        assert_eq!(
            (nova5.raw_level, nova5.percent, nova5.charging),
            (-1, 0, true)
        );
        assert_eq!(
            parsed(Protocol::Nova5, &[0xB0, 2, 0, 101, 1]).unwrap(),
            offline()
        );
        assert!(parsed(Protocol::Nova5, &[0xB0, 3, 0, 101, 0]).is_err());
        assert!(parsed(Protocol::Nova5, &[0xB0, 3, 0, 50]).is_err());
    }

    #[test]
    fn gamebuds_select_active_minimum_and_reject_invalid_data() {
        assert_eq!(
            parsed(Protocol::GameBuds, &[0xB0, 0, 0, 3, 2, 75, 0])
                .unwrap()
                .percent,
            75
        );
        assert_eq!(
            parsed(Protocol::GameBuds, &[0, 0xB0, 0, 0, 2, 3, 0, 64])
                .unwrap()
                .percent,
            64
        );
        assert_eq!(
            parsed(Protocol::GameBuds, &[0xB0, 0, 0, 3, 3, 80, 45])
                .unwrap()
                .percent,
            45
        );
        assert_eq!(
            parsed(Protocol::GameBuds, &[0xB0, 0, 0, 2, 2, 101, 101]).unwrap(),
            offline()
        );
        assert!(parsed(Protocol::GameBuds, &[0xB0, 0, 0, 1, 2, 50, 0]).is_err());
        assert!(parsed(Protocol::GameBuds, &[0xB0, 0, 0, 3, 2, 101, 0]).is_err());
        assert!(parsed(Protocol::GameBuds, &[0xB0, 0, 0, 3, 2, 50]).is_err());
    }

    #[test]
    fn protocol_request_commands_are_zero_padded() {
        let cases = [
            (Protocol::Legacy12, [0x06, 0x12]),
            (Protocol::Arctis9, [0x00, 0x20]),
            (Protocol::NovaDiscrete, [0x00, 0xB0]),
            (Protocol::NovaDirect, [0x00, 0xB0]),
            (Protocol::Nova5, [0x00, 0xB0]),
            (Protocol::GameBuds, [0x00, 0xB0]),
        ];
        for (protocol, command) in cases {
            let request = build_request(protocol, Step::Status, 65).unwrap();
            assert_eq!(request[..2], command);
            assert!(request[2..].iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn interface_number_parsing() {
        assert_eq!(
            parse_interface_number(r"\\?\hid#vid_1038&pid_2212&mi_03#abc"),
            3
        );
        assert_eq!(parse_interface_number(r"\\?\hid#vid_1038&pid_2212#abc"), -1);
        assert_eq!(parse_interface_number(r"\\?\hid#pid_2212&mi_zz#abc"), -1);
    }
}
