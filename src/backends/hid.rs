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
    CloseHandle, ERROR_IO_PENDING, ERROR_NO_MORE_FILES, ERROR_OPERATION_ABORTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::g13::{
    G13_INPUT_REPORT_ID, G13_INPUT_REPORT_LENGTH, G13_OUTPUT_REPORT_LENGTH, G13_PRODUCT_ID,
    G13_USAGE, G13_USAGE_PAGE, G13_VENDOR_ID,
};

const IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_INTERFACES_VISITED: usize = 256;
const COMPETING_OWNERS: [(&str, &str); 2] = [
    (
        "LCore.exe",
        "Logitech Gaming Software (LCore.exe) owns the G13 LCD; exit Logitech Gaming Software to release the G13 LCD",
    ),
    (
        "logi_lamparray_service.AMD64.exe",
        "Logitech LampArray service owns the G13 LCD; stop Logitech LampArray service to release the G13 LCD for direct HID",
    ),
];

fn owner_gate(processes: Result<Vec<String>, String>) -> Result<(), String> {
    let processes = processes
        .map_err(|e| format!("cannot verify G13 LCD ownership ({e}); refusing direct HID"))?;
    if let Some((_, message)) = COMPETING_OWNERS.iter().find(|(owner, _)| {
        processes
            .iter()
            .any(|name| name.eq_ignore_ascii_case(owner))
    }) {
        return Err((*message).into());
    }
    Ok(())
}

fn owner_guarded<T>(
    processes: Result<Vec<String>, String>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    owner_gate(processes)?;
    operation()
}

fn running_process_names() -> Result<Vec<String>, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("process enumeration failed: {e}"))?;
        let mut names = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let first = Process32FirstW(snapshot, &mut entry);
        if let Err(error) = first {
            let _ = CloseHandle(snapshot);
            return Err(format!("process enumeration failed: {error}"));
        }
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(entry.szExeFile.len());
            names.push(String::from_utf16_lossy(&entry.szExeFile[..end]));
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    let _ = CloseHandle(snapshot);
                    return Err(format!("process enumeration failed: {error}"));
                }
                break;
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(names)
    }
}

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
pub struct Rejection {
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
                    Err(reason) => result.rejections.push(Rejection { reason }),
                },
                Err(reason) => result.rejections.push(Rejection { reason }),
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
    let handle = open_path(
        device_path,
        FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
    )?;
    let info = describe(handle, device_path);
    let _ = CloseHandle(handle);
    info
}

unsafe fn open_path(device_path: &str, share: FILE_SHARE_MODE) -> Result<HANDLE, String> {
    let wide: Vec<u16> = device_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    CreateFileW(
        PCWSTR::from_raw(wide.as_ptr()),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        share,
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
    read_event: HANDLE,
    write_event: HANDLE,
}

// HANDLE is a raw wrapper; the device thread owns it exclusively.
unsafe impl Send for HidDevice {}

impl HidDevice {
    /// Open the first matching G13 collection. Returns discovery evidence
    /// alongside the device so callers can log exact selection reasons.
    pub fn open() -> Result<(HidDevice, Discovery), String> {
        owner_gate(running_process_names())?;
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
        let handle = unsafe { open_path(&candidate.device_path, FILE_SHARE_MODE(0))? };
        if let Err(error) = owner_gate(running_process_names()) {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(format!(
                "LCore ownership changed after exclusive HID open: {error}"
            ));
        }
        let (read_event, write_event) = create_io_events(
            handle,
            || unsafe { CreateEventW(None, true, false, None) }.map_err(|e| e.to_string()),
            |owned| unsafe {
                let _ = CloseHandle(owned);
            },
        )?;
        Ok((
            HidDevice {
                handle,
                read_event,
                write_event,
            },
            discovery,
        ))
    }

    /// Write one 992-byte output report with a bounded timeout.
    pub fn write_report(&self, report: &[u8; G13_OUTPUT_REPORT_LENGTH]) -> Result<(), String> {
        owner_guarded(running_process_names(), || {
            self.write_report_unchecked(report)
        })
    }

    fn write_report_unchecked(
        &self,
        report: &[u8; G13_OUTPUT_REPORT_LENGTH],
    ) -> Result<(), String> {
        unsafe { ResetEvent(self.write_event) }
            .map_err(|e| format!("HID reset write event: {e}"))?;
        let mut overlapped = self.new_overlapped(self.write_event);
        let result = unsafe {
            windows::Win32::Storage::FileSystem::WriteFile(
                self.handle,
                Some(report.as_slice()),
                None,
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => validate_transfer(
                "HID write",
                self.overlapped_result(&overlapped, "HID write")?,
                G13_OUTPUT_REPORT_LENGTH,
            ),
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                let transferred =
                    self.wait_pending(self.write_event, &overlapped, IO_TIMEOUT, "HID write")?;
                validate_transfer("HID write", transferred, G13_OUTPUT_REPORT_LENGTH)
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
        unsafe { ResetEvent(self.read_event) }.map_err(|e| format!("HID reset read event: {e}"))?;
        let mut overlapped = self.new_overlapped(self.read_event);
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
                let transferred = self.overlapped_result(&overlapped, "HID read")?;
                validate_transfer("HID read", transferred, G13_INPUT_REPORT_LENGTH)?;
                validate_input_id(buffer[0])?;
                Ok(true)
            }
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                match unsafe { WaitForSingleObject(self.read_event, timeout.as_millis() as u32) } {
                    WAIT_OBJECT_0 => {
                        let transferred = self.overlapped_result(&overlapped, "HID read")?;
                        validate_transfer("HID read", transferred, G13_INPUT_REPORT_LENGTH)?;
                        validate_input_id(buffer[0])?;
                        Ok(true)
                    }
                    WAIT_TIMEOUT => {
                        self.cancel_and_drain(&overlapped, "HID read")?;
                        Ok(false)
                    }
                    result => {
                        self.cancel_and_drain(&overlapped, "HID read")?;
                        Err(format!("HID read wait failed: {result:?}"))
                    }
                }
            }
            Err(e) => Err(format!("HID read: {}", e)),
        }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn new_overlapped(&self, event: HANDLE) -> OVERLAPPED {
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event;
        overlapped
    }

    fn overlapped_result(&self, overlapped: &OVERLAPPED, label: &str) -> Result<u32, String> {
        let mut transferred = 0u32;
        unsafe { GetOverlappedResult(self.handle, overlapped, &mut transferred, false) }
            .map_err(|e| format!("{label} completion: {e}"))?;
        Ok(transferred)
    }

    fn wait_pending(
        &self,
        event: HANDLE,
        overlapped: &OVERLAPPED,
        timeout: Duration,
        label: &str,
    ) -> Result<u32, String> {
        match unsafe { WaitForSingleObject(event, timeout.as_millis() as u32) } {
            WAIT_OBJECT_0 => self.overlapped_result(overlapped, label),
            WAIT_TIMEOUT => {
                self.cancel_and_drain(overlapped, label)?;
                Err(format!("{label} timed out"))
            }
            result => {
                self.cancel_and_drain(overlapped, label)?;
                Err(format!("{label} wait failed: {result:?}"))
            }
        }
    }

    fn cancel_and_drain(&self, overlapped: &OVERLAPPED, label: &str) -> Result<(), String> {
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
            let mut transferred = 0u32;
            match GetOverlappedResult(self.handle, overlapped, &mut transferred, true) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ERROR_OPERATION_ABORTED.to_hresult() => Ok(()),
                Err(error) => Err(format!("{label} cancellation completion: {error}")),
            }
        }
    }

    /// Blank the panel, cancel any pending I/O, and close handles.
    pub fn close(&mut self) -> Result<(), String> {
        let blank_result = self.write_report(&super::g13::blank_report());
        self.close_handles();
        blank_result.map_err(|e| format!("HID blank on close: {e}"))
    }

    /// Close immediately without a final write when LCore has taken ownership.
    pub fn close_without_blank(&mut self) {
        self.close_handles();
    }

    fn close_handles(&mut self) {
        unsafe {
            let _ = CancelIoEx(self.handle, None);
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.read_event);
            let _ = CloseHandle(self.write_event);
        }
    }
}

fn create_io_events(
    device: HANDLE,
    mut create: impl FnMut() -> Result<HANDLE, String>,
    mut close: impl FnMut(HANDLE),
) -> Result<(HANDLE, HANDLE), String> {
    let read = match create() {
        Ok(event) => event,
        Err(error) => {
            close(device);
            return Err(format!("create HID read event: {error}"));
        }
    };
    let write = match create() {
        Ok(event) => event,
        Err(error) => {
            close(read);
            close(device);
            return Err(format!("create HID write event: {error}"));
        }
    };
    Ok((read, write))
}

fn validate_transfer(label: &str, transferred: u32, expected: usize) -> Result<(), String> {
    if transferred as usize != expected {
        return Err(format!(
            "{label} completed {transferred} of {expected} bytes"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_name_matching_is_exact_and_case_insensitive() {
        assert!(owner_gate(Ok(vec!["lcore.EXE".into()])).is_err());
        assert!(owner_gate(Ok(vec!["LOGI_LAMPARRAY_SERVICE.amd64.EXE".into()])).is_err());
        assert!(owner_gate(Ok(vec!["lcore-helper.exe".into(), "lghub.exe".into()])).is_ok());
    }

    #[test]
    fn owner_detection_errors_fail_closed_without_private_data() {
        let error = owner_gate(Err("snapshot unavailable".into())).unwrap_err();
        assert!(error.contains("refusing direct HID"));
        assert!(!error.contains('\\'));
    }

    #[test]
    fn owner_refusal_is_actionable_and_retry_can_recover() {
        let refused = owner_gate(Ok(vec!["LCore.exe".into()])).unwrap_err();
        assert!(refused.contains("exit Logitech Gaming Software"));
        let refused = owner_gate(Ok(vec!["logi_lamparray_service.AMD64.exe".into()])).unwrap_err();
        assert!(refused.contains("stop Logitech LampArray service"));
        assert!(owner_gate(Ok(Vec::new())).is_ok());
    }

    #[test]
    fn partial_transfers_are_rejected() {
        assert!(validate_transfer("HID write", 991, 992).is_err());
        assert!(validate_transfer("HID write", 992, 992).is_ok());
    }

    #[test]
    fn event_creation_failures_close_every_owned_handle() {
        let device = HANDLE(11usize as _);
        let read = HANDLE(12usize as _);
        let mut closed = Vec::new();
        let mut calls = 0;
        let error = create_io_events(
            device,
            || {
                calls += 1;
                if calls == 1 {
                    Ok(read)
                } else {
                    Err("injected".into())
                }
            },
            |handle| closed.push(handle.0 as usize),
        )
        .unwrap_err();
        assert!(error.contains("write event"));
        assert_eq!(closed, vec![12, 11]);

        let mut closed = Vec::new();
        let error = create_io_events(
            device,
            || Err("injected".into()),
            |handle| closed.push(handle.0 as usize),
        )
        .unwrap_err();
        assert!(error.contains("read event"));
        assert_eq!(closed, vec![11]);
    }

    #[test]
    fn lcore_race_seams_perform_zero_writes_after_detection() {
        for phase in ["before-open", "after-open", "before-write", "idle"] {
            let mut writes = 0;
            let result = owner_guarded(Ok(vec!["LCore.exe".into()]), || {
                writes += 1;
                Ok(())
            });
            assert!(result.is_err(), "{phase}");
            assert_eq!(writes, 0, "{phase}");
        }
        let mut writes = 0;
        owner_guarded(Ok(Vec::new()), || {
            writes += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(writes, 1);
    }
}
