//! Windows direct-HID transport for the G13 vendor collection.
//!
//! Enumeration is strict: exactly one candidate matching VID/PID, usage
//! page/usage, and the 8/992-byte report contract is accepted. All I/O is
//! overlapped with bounded timeouts; stuck operations are canceled and
//! drained before the handle closes.

#![cfg(windows)]

use std::time::Duration;

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

use super::g13::{
    G13_INPUT_REPORT_ID, G13_INPUT_REPORT_LENGTH, G13_OUTPUT_REPORT_LENGTH, G13_PRODUCT_ID,
    G13_USAGE, G13_USAGE_PAGE, G13_VENDOR_ID,
};

const IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_INTERFACES_VISITED: usize = 256;

#[derive(Debug, Clone)]
pub struct CandidateInfo {
    pub device_path: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_report_length: u16,
    pub output_report_length: u16,
}

impl CandidateInfo {
    fn matches(&self) -> bool {
        self.vid == G13_VENDOR_ID
            && self.pid == G13_PRODUCT_ID
            && self.usage_page == G13_USAGE_PAGE
            && self.usage == G13_USAGE
            && self.input_report_length as usize == G13_INPUT_REPORT_LENGTH
            && self.output_report_length as usize == G13_OUTPUT_REPORT_LENGTH
    }
}

#[derive(Debug)]
#[allow(dead_code)] // device_path retained for Phase 4 evidence records
pub struct Rejection {
    pub device_path: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub scanned: usize,
    pub candidates: Vec<CandidateInfo>,
    pub rejections: Vec<Rejection>,
}

/// Enumerate HID interfaces and collect G13 candidates with exact rejection
/// reasons. Read-only: opens each interface briefly to query attributes.
pub fn discover() -> Discovery {
    let mut result = Discovery::default();
    unsafe {
        let Ok(hdev) = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_HID),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) else {
            return result;
        };
        let mut index = 0u32;
        loop {
            if index as usize >= MAX_INTERFACES_VISITED {
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
            result.scanned += 1;

            match interface_detail(hdev, &iface) {
                Ok(device_path) => match open_attributes(&device_path) {
                    Ok(info) => {
                        if info.matches() {
                            result.candidates.push(info);
                        } else {
                            result.rejections.push(Rejection {
                                device_path,
                                reason: format!(
                                    "identity mismatch vid={:04x} pid={:04x} usage={:04x}:{:04x} reports={}/{}",
                                    info.vid,
                                    info.pid,
                                    info.usage_page,
                                    info.usage,
                                    info.input_report_length,
                                    info.output_report_length
                                ),
                            });
                        }
                    }
                    Err(reason) => result.rejections.push(Rejection {
                        device_path,
                        reason,
                    }),
                },
                Err(reason) => result.rejections.push(Rejection {
                    device_path: String::new(),
                    reason,
                }),
            }
            if !result.candidates.is_empty() {
                // Contract: accept exactly one candidate.
                break;
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    result
}

unsafe fn interface_detail(
    hdev: HDEVINFO,
    iface: &SP_DEVICE_INTERFACE_DATA,
) -> Result<String, String> {
    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, iface, None, 0, Some(&mut required), None);
    if required == 0 {
        return Err("interface detail size query failed".into());
    }
    let mut buffer = vec![0u8; required as usize];
    // cbSize must equal the struct's fixed size (8 on x64): cbSize + one wchar.
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
        return Err("interface detail query failed".into());
    }
    // DevicePath is a null-terminated wide string starting at offset 4.
    let wide_ptr = buffer.as_ptr().add(4) as *const u16;
    let mut len = 0usize;
    while *wide_ptr.add(len) != 0 {
        len += 1;
        if len > 1024 {
            return Err("device path too long".into());
        }
    }
    let slice = std::slice::from_raw_parts(wide_ptr, len);
    Ok(String::from_utf16_lossy(slice))
}

unsafe fn open_attributes(device_path: &str) -> Result<CandidateInfo, String> {
    let handle = open_path(device_path)?;
    let info = describe(handle, device_path);
    let _ = CloseHandle(handle);
    info
}

unsafe fn open_path(device_path: &str) -> Result<HANDLE, String> {
    let wide: Vec<u16> = device_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    CreateFileW(
        PCWSTR::from_raw(wide.as_ptr()),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
        None,
        OPEN_EXISTING,
        FILE_FLAG_OVERLAPPED,
        None,
    )
    .map_err(|e| format!("open failed: {}", e))
}

unsafe fn describe(handle: HANDLE, device_path: &str) -> Result<CandidateInfo, String> {
    let mut attrs = HIDD_ATTRIBUTES::default();
    if !HidD_GetAttributes(handle, &mut attrs).as_bool() {
        return Err("HidD_GetAttributes failed".into());
    }
    let mut preparsed = PHIDP_PREPARSED_DATA::default();
    if !HidD_GetPreparsedData(handle, &mut preparsed).as_bool() {
        return Err("HidD_GetPreparsedData failed".into());
    }
    let mut caps = HIDP_CAPS::default();
    let status = HidP_GetCaps(preparsed, &mut caps);
    let _ = HidD_FreePreparsedData(preparsed);
    if status.0 < 0 {
        return Err("HidP_GetCaps failed".into());
    }
    Ok(CandidateInfo {
        device_path: device_path.to_string(),
        vid: attrs.VendorID,
        pid: attrs.ProductID,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        input_report_length: caps.InputReportByteLength,
        output_report_length: caps.OutputReportByteLength,
    })
}

fn validate_input_id(id: u8) -> Result<(), String> {
    if id != G13_INPUT_REPORT_ID {
        return Err(format!(
            "HID input report ID 0x{:02x}, want 0x{:02x}",
            id, G13_INPUT_REPORT_ID
        ));
    }
    Ok(())
}

/// An opened, validated G13 vendor collection.
pub struct HidDevice {
    handle: HANDLE,
    event: HANDLE,
}

// HANDLE is a raw wrapper; the device thread owns it exclusively.
unsafe impl Send for HidDevice {}

impl HidDevice {
    /// Open the first matching G13 collection. Returns discovery evidence
    /// alongside the device so callers can log exact selection reasons.
    pub fn open() -> Result<(HidDevice, Discovery), String> {
        let discovery = discover();
        let Some(candidate) = discovery.candidates.first() else {
            let detail = discovery
                .rejections
                .iter()
                .map(|r| r.reason.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(if detail.is_empty() {
                "HID discovery: no G13 collection matched VID/PID, usage, and 8/992-byte reports"
                    .into()
            } else {
                format!(
                    "HID discovery: no G13 collection matched VID/PID, usage, and 8/992-byte reports (scanned {} interfaces: {})",
                    discovery.scanned, detail
                )
            });
        };
        let handle = unsafe { open_path(&candidate.device_path)? };
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEventW failed: {}", e))?;
        Ok((HidDevice { handle, event }, discovery))
    }

    /// Write one 992-byte output report with a bounded timeout.
    pub fn write_report(&self, report: &[u8; G13_OUTPUT_REPORT_LENGTH]) -> Result<(), String> {
        let mut overlapped = self.new_overlapped();
        let result = unsafe {
            windows::Win32::Storage::FileSystem::WriteFile(
                self.handle,
                Some(report.as_slice()),
                None,
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                match unsafe { WaitForSingleObject(self.event, IO_TIMEOUT.as_millis() as u32) } {
                    WAIT_OBJECT_0 => {
                        let mut transferred = 0u32;
                        unsafe {
                            GetOverlappedResult(self.handle, &overlapped, &mut transferred, false)
                        }
                        .map_err(|e| format!("HID write result: {}", e))?;
                        if transferred as usize != G13_OUTPUT_REPORT_LENGTH {
                            return Err(format!(
                                "HID write completed {} of {} bytes",
                                transferred, G13_OUTPUT_REPORT_LENGTH
                            ));
                        }
                        Ok(())
                    }
                    WAIT_TIMEOUT => {
                        self.cancel_and_drain(&overlapped);
                        Err("HID write timed out".into())
                    }
                    event => Err(format!("HID write wait failed: {:?}", event)),
                }
            }
            Err(e) => Err(format!("HID write: {}", e)),
        }
    }

    /// Read one 8-byte input report. `Ok(false)` means timeout with no data.
    pub fn read_input_timeout(
        &self,
        buffer: &mut [u8; G13_INPUT_REPORT_LENGTH],
        timeout: Duration,
    ) -> Result<bool, String> {
        let mut overlapped = self.new_overlapped();
        let result = unsafe {
            windows::Win32::Storage::FileSystem::ReadFile(
                self.handle,
                Some(buffer.as_mut_slice()),
                None,
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => {
                validate_input_id(buffer[0])?;
                Ok(true)
            }
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                match unsafe { WaitForSingleObject(self.event, timeout.as_millis() as u32) } {
                    WAIT_OBJECT_0 => {
                        let mut transferred = 0u32;
                        unsafe {
                            GetOverlappedResult(self.handle, &overlapped, &mut transferred, false)
                        }
                        .map_err(|e| format!("HID read result: {}", e))?;
                        if transferred as usize != G13_INPUT_REPORT_LENGTH {
                            return Err(format!(
                                "HID read completed {} of {} bytes",
                                transferred, G13_INPUT_REPORT_LENGTH
                            ));
                        }
                        validate_input_id(buffer[0])?;
                        Ok(true)
                    }
                    WAIT_TIMEOUT => {
                        self.cancel_and_drain(&overlapped);
                        Ok(false)
                    }
                    event => Err(format!("HID read wait failed: {:?}", event)),
                }
            }
            Err(e) => Err(format!("HID read: {}", e)),
        }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn new_overlapped(&self) -> OVERLAPPED {
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = self.event;
        overlapped
    }

    fn cancel_and_drain(&self, overlapped: &OVERLAPPED) {
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
            let mut drained = 0u32;
            let _ = GetOverlappedResult(self.handle, overlapped, &mut drained, true);
        }
    }

    /// Blank the panel, cancel any pending I/O, and close handles.
    pub fn close(&mut self) {
        unsafe {
            let blank = super::g13::blank_report();
            #[allow(clippy::field_reassign_with_default)]
            let mut overlapped = self.new_overlapped();
            if windows::Win32::Storage::FileSystem::WriteFile(
                self.handle,
                Some(blank.as_slice()),
                None,
                Some(&mut overlapped),
            )
            .is_ok()
            {
                let _ = WaitForSingleObject(self.event, 500);
            }
            let _ = CancelIoEx(self.handle, None);
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.event);
        }
    }
}
