//! SteelSeries Arctis headset battery over USB HID (7P+ family).
//!
//! Protocol ported from the proven original Go implementation: open the
//! ranked vendor collection, send the read-only `0xB0` status request, and
//! decode the response honestly — the device reports a coarse 0..4 level
//! (mapped to 0/25/50/75/100%) or a direct percent; intermediate values are
//! never invented.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidD_GetProductString,
    HidP_GetCaps, GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
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

pub const STEELSERIES_VID: u16 = 0x1038;

/// Product IDs of known Arctis receivers (Go: knownArctisProductIDs).
pub const KNOWN_PIDS: [u16; 5] = [0x220C, 0x220E, 0x2212, 0x2216, 0x2236];

const MAX_VISITED: usize = 256;
const MAX_CANDIDATES: usize = 64;
const READ_ATTEMPTS: usize = 5;

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
    /// Device-reported coarse level (0..4) or direct percent (<=100).
    /// -1 means the headset is present at the receiver but offline.
    pub raw_level: i32,
    pub percent: i32,
}

#[derive(Clone, Debug)]
pub struct HidInterface {
    pub path: String,
    pub product: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub interface_number: i32,
    pub input_len: u16,
    pub output_len: u16,
}

/// Decode the read-only 0xB0 response. HID may prefix one zero report ID.
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

/// Candidate ranking, ported from the Go `candidateScore`.
pub fn candidate_score(info: &HidInterface) -> Option<i32> {
    if info.vid != STEELSERIES_VID {
        return None;
    }
    let name = info.product.trim().to_lowercase();
    let known_pid = KNOWN_PIDS.contains(&info.pid);
    let is_arctis = name.contains("arctis");
    let vendor_usage = info.usage_page >= 0xFF00;
    if info.usage_page == 0x000C
        || (!vendor_usage && info.usage_page != 0)
        || (!known_pid && !is_arctis)
    {
        return None;
    }
    let mut score = 0;
    if known_pid {
        score += 80;
    }
    if is_arctis {
        score += 70;
    }
    if name.contains("7p+") || name.contains("7p plus") {
        score += 80;
    } else if name.contains("7+") || name.contains("7 plus") {
        score += 55;
    }
    if info.usage_page == 0xFFC0 && info.usage == 0x0001 {
        score += 100;
    } else if vendor_usage {
        score += 20;
    }
    if info.interface_number == 3 {
        score += 35;
    }
    if info.output_len >= 2 {
        score += 10;
    }
    if info.input_len >= 4 {
        score += 10;
    }
    Some(score)
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

/// Enumerate every present HID interface with full identity details.
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
            if index as usize >= MAX_VISITED || out.len() >= MAX_CANDIDATES {
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
            // Open with attributes-query access (no write), inspect, close.
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
            if let Some(info) = inspect(handle, path) {
                out.push(info);
            }
            let _ = CloseHandle(handle);
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    out
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
    let mut product_buf = [0u16; 256];
    let product = if HidD_GetProductString(
        handle,
        product_buf.as_mut_ptr() as _,
        (product_buf.len() * 2) as u32,
    )
    .as_bool()
    {
        let end = product_buf.iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf16_lossy(&product_buf[..end])
    } else {
        String::new()
    };
    Some(HidInterface {
        interface_number: parse_interface_number(&path),
        path,
        product,
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

/// One bounded status query: rank, open best candidate, 0xB0 request, read.
pub fn query(timeout: std::time::Duration) -> Result<Status, String> {
    let candidates = rank_candidates(enumerate());
    let Some(_best) = candidates.first() else {
        return Err("compatible SteelSeries Arctis receiver not found".into());
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut last_error = String::new();
    for candidate in &candidates {
        match probe(candidate, deadline) {
            Ok(status) => return Ok(status),
            Err(e) => last_error = e,
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
        let result = probe_handle(handle, info, deadline);
        let _ = CloseHandle(handle);
        result
    }
}

unsafe fn probe_handle(
    handle: HANDLE,
    info: &HidInterface,
    deadline: std::time::Instant,
) -> Result<Status, String> {
    let event = EventHandle(
        CreateEventW(None, false, false, None).map_err(|e| format!("CreateEventW: {}", e))?,
    );
    let out_len = info.output_len.max(2) as usize;
    let mut out = vec![0u8; out_len];
    out[1] = 0xB0;

    let write = || -> Result<usize, String> {
        let wait_ms = remaining_ms(deadline)?;
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        match windows::Win32::Storage::FileSystem::WriteFile(
            handle,
            Some(out.as_slice()),
            None,
            Some(&mut overlapped),
        ) {
            Ok(()) => Ok(out_len),
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                match WaitForSingleObject(event.0, wait_ms) {
                    WAIT_OBJECT_0 => {
                        let mut transferred = 0u32;
                        GetOverlappedResult(handle, &overlapped, &mut transferred, false)
                            .map_err(|e| format!("write completion: {}", e))?;
                        Ok(transferred as usize)
                    }
                    WAIT_TIMEOUT => {
                        let _ = CancelIoEx(handle, Some(&overlapped));
                        let mut drained = 0u32;
                        let _ = GetOverlappedResult(handle, &overlapped, &mut drained, true);
                        Err("status request timed out".into())
                    }
                    _ => {
                        let _ = CancelIoEx(handle, Some(&overlapped));
                        let mut drained = 0u32;
                        let _ = GetOverlappedResult(handle, &overlapped, &mut drained, true);
                        Err("status request wait failed".into())
                    }
                }
            }
            Err(e) => Err(format!("status request: {}", e)),
        }
    };

    if write()? != out_len {
        return Err("send status request: short HID write".into());
    }

    let in_len = info.input_len.max(4) as usize;
    let mut last_error = String::new();
    for _ in 0..READ_ATTEMPTS {
        let mut buffer = vec![0u8; in_len];
        let transferred = read_once(handle, event.0, &mut buffer, deadline)?;
        buffer.truncate(transferred);
        match parse_status(&buffer) {
            Ok(status) => {
                return Ok(status);
            }
            Err(e) => last_error = e,
        }
    }
    if last_error.is_empty() {
        last_error = "no compatible SteelSeries battery status report received".into();
    }
    Err(last_error)
}

fn remaining_ms(deadline: std::time::Instant) -> Result<u32, String> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err("status query timed out".into());
    }
    Ok(remaining.as_millis().clamp(1, u32::MAX as u128) as u32)
}

unsafe fn read_once(
    handle: HANDLE,
    event: HANDLE,
    buffer: &mut [u8],
    deadline: std::time::Instant,
) -> Result<usize, String> {
    let wait_ms = remaining_ms(deadline)?;
    let mut overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    match windows::Win32::Storage::FileSystem::ReadFile(
        handle,
        Some(buffer),
        None,
        Some(&mut overlapped),
    ) {
        Ok(()) => Ok(buffer.len()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            match WaitForSingleObject(event, wait_ms) {
                WAIT_OBJECT_0 => {
                    let mut transferred = 0u32;
                    GetOverlappedResult(handle, &overlapped, &mut transferred, false)
                        .map_err(|e| format!("read completion: {}", e))?;
                    Ok(transferred as usize)
                }
                WAIT_TIMEOUT => {
                    let _ = CancelIoEx(handle, Some(&overlapped));
                    let mut drained = 0u32;
                    let _ = GetOverlappedResult(handle, &overlapped, &mut drained, true);
                    Err("status response timed out".into())
                }
                _ => {
                    let _ = CancelIoEx(handle, Some(&overlapped));
                    let mut drained = 0u32;
                    let _ = GetOverlappedResult(handle, &overlapped, &mut drained, true);
                    Err("status response wait failed".into())
                }
            }
        }
        Err(e) => Err(format!("read status response: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(pid: u16, product: &str, usage_page: u16, usage: u16, mi: i32) -> HidInterface {
        HidInterface {
            path: format!(
                "\\\\?\\hid#vid_1038&pid_{:04x}&mi_{:02x}#test",
                pid,
                mi.max(0)
            ),
            product: product.into(),
            vid: STEELSERIES_VID,
            pid,
            usage_page,
            usage,
            interface_number: mi,
            input_len: 65,
            output_len: 65,
        }
    }

    #[test]
    fn parse_status_online_bands() {
        // coarse band: raw 3 -> 75%
        let s = parse_status(&[0xB0, 0x03, 0x03, 0x03]).unwrap();
        assert!(s.online);
        assert_eq!((s.raw_level, s.percent), (3, 75));
        // zero-prefixed report ID: raw 4 -> 100%, charging
        let s = parse_status(&[0x00, 0xB0, 0x03, 0x04, 0x01]).unwrap();
        assert!(s.online && s.charging);
        assert_eq!((s.raw_level, s.percent), (4, 100));
        // direct percent (raw 88)
        let s = parse_status(&[0xB0, 0x00, 88, 0x00]).unwrap();
        assert_eq!(s.percent, 88);
    }

    #[test]
    fn parse_status_rejects_short_noise_and_implausible() {
        for data in [
            &[][..],
            &[0xB0, 0x00, 0x01][..],
            &[0x00, 0x22, 0x10, 0x77][..],
            &[0xB0, 0x03, 101, 0x00][..],
        ] {
            assert!(parse_status(data).is_err());
        }
    }

    #[test]
    fn parse_status_offline_and_short() {
        let s = parse_status(&[0xB0, 0x01, 0x00, 0x00]).unwrap();
        assert!(!s.online);
        assert_eq!(s.raw_level, -1);
        assert!(parse_status(&[0xB0, 0x00]).is_err());
        assert!(parse_status(&[]).is_err());
    }

    #[test]
    fn scoring_prefers_7p_plus_vendor_collection() {
        let best = iface(0x2212, "Arctis 7P+", 0xFFC0, 0x0001, 3);
        let other = iface(0x2212, "Arctis 7P+", 0xFFC0, 0x0001, 0);
        let generic = iface(0x2212, "USB Audio", 0x0000, 0x0001, 0);
        assert!(candidate_score(&best).unwrap() > candidate_score(&other).unwrap());
        assert!(candidate_score(&other).unwrap() > candidate_score(&generic).unwrap());
    }

    #[test]
    fn scoring_rejects_non_targets() {
        assert!(candidate_score(&iface(0x1234, "Razer Mouse", 0xFF00, 0x0001, 0)).is_none());
        assert!(candidate_score(&iface(0x1038, "Other Device", 0x000C, 0x0001, 0)).is_none());
        assert!(candidate_score(&iface(0x1038, "Headset", 0x0001, 0x0001, 0)).is_none());
    }

    #[test]
    fn ranking_orders_best_first() {
        let ranked = rank_candidates(vec![
            iface(0x2212, "Arctis 7P+", 0xFFC0, 0x0001, 0),
            iface(0x2212, "Arctis 7P+", 0xFFC0, 0x0001, 3),
        ]);
        assert_eq!(ranked[0].interface_number, 3);
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
