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
use super::{LossCause, LossError};

const IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_INTERFACES_VISITED: usize = 256;
const COMPETING_OWNERS: [(&str, &str); 2] = [
    (
        "LCore.exe",
        "competing process LCore.exe is running; exit Logitech Gaming Software before using direct HID",
    ),
    (
        "logi_lamparray_service.AMD64.exe",
        "competing process logi_lamparray_service.AMD64.exe is running; stop Logitech LampArray service before using direct HID",
    ),
];

/// Which competing owners the enumeration saw. A compact ownership result:
/// two flags carried out of the scan instead of a `Vec<String>` of every
/// process name (WO-OPT04, handoff O3 slice 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ObservedOwners {
    lcore: bool,
    lamparray: bool,
}

/// Whole-name, case-insensitive ASCII compare of one UTF-16 process name against
/// an ASCII owner name, in place (no `String`). The length check is load-bearing:
/// `lcore-helper.exe` (16 units) must not match `LCore.exe` (9 units) and
/// `lghub.exe` must not match a prefix -- pinned by
/// `owner_name_matching_is_exact_and_case_insensitive`.
fn owner_name_match(units: &[u16], owner: &str) -> bool {
    let expected = owner.as_bytes();
    units.len() == expected.len()
        && units
            .iter()
            .zip(expected)
            .all(|(&unit, &expected)| unit <= 0x7f && (unit as u8).eq_ignore_ascii_case(&expected))
}

/// Fold one enumeration entry into the compact result, straight from its UTF-16
/// units. OWNER PRIORITY: the shipped gate was
/// `COMPETING_OWNERS.iter().find(...)`, so **LCore.exe wins when both owners are
/// running, independent of process-enumeration order**. A naive single-pass
/// "return on the first match" would hand the verdict to whoever is enumerated
/// first -- a behavior change. So an LCore entry sets its flag and a LampArray
/// entry sets its flag, the scan always runs the full census, and `owner_gate`
/// consults index 0 before index 1. Mutation (iii) of WO-OPT04 A3 (first match
/// wins) turns `owner_gate_prefers_lcore_when_both_owners_are_present` red.
fn classify_owner(observed: &mut ObservedOwners, units: &[u16]) {
    if owner_name_match(units, COMPETING_OWNERS[0].0) {
        observed.lcore = true;
    } else if owner_name_match(units, COMPETING_OWNERS[1].0) {
        observed.lamparray = true;
    }
}

fn owner_gate(observed: Result<ObservedOwners, String>) -> Result<(), LossError> {
    let observed = observed.map_err(|e| {
        LossError::new(
            LossCause::ProbeFail,
            format!("cannot enumerate running processes ({e}); refusing direct HID"),
        )
    })?;
    // Priority order is COMPETING_OWNERS order: LCore first, see classify_owner.
    if observed.lcore {
        return Err(LossError::new(
            LossCause::OwnerConflict,
            COMPETING_OWNERS[0].1.into(),
        ));
    }
    if observed.lamparray {
        return Err(LossError::new(
            LossCause::OwnerConflict,
            COMPETING_OWNERS[1].1.into(),
        ));
    }
    Ok(())
}

fn owner_guarded<T>(
    observed: Result<ObservedOwners, String>,
    operation: impl FnOnce() -> Result<T, LossError>,
) -> Result<T, LossError> {
    owner_gate(observed)?;
    operation()
}

/// Snapshot-creation census counter (WO-OPT04 A1). The lane's precedent shape is
/// `presentmon.rs`'s `WORKER_TICKS`/`worker_tick_count` pair.
#[cfg(test)]
static SNAPSHOT_CREATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn snapshot_creation_count() -> u64 {
    SNAPSHOT_CREATIONS.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
fn snapshot_tick() {
    SNAPSHOT_CREATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(not(test))]
fn snapshot_tick() {}

/// Enumerate the running processes and classify ownership straight from each
/// entry's UTF-16 name. The full census is still walked and the snapshot is
/// created exactly once per call -- only the per-process `String` is gone.
fn observed_owners() -> Result<ObservedOwners, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("process enumeration failed: {e}"))?;
        snapshot_tick();
        let mut observed = ObservedOwners::default();
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
            classify_owner(&mut observed, &entry.szExeFile[..end]);
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    let _ = CloseHandle(snapshot);
                    return Err(format!("process enumeration failed: {error}"));
                }
                break;
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(observed)
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
    )
    .map_err(|error| error.detail)?;
    let info = describe(handle, device_path);
    let _ = CloseHandle(handle);
    info
}

unsafe fn open_path(device_path: &str, share: FILE_SHARE_MODE) -> Result<HANDLE, LossError> {
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
    .map_err(|e| {
        LossError::new(
            LossCause::OpenFailed {
                code: e.code().0 as u32,
            },
            format!("open failed: {e}"),
        )
    })
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

fn validate_input_id(id: u8) -> Result<(), LossError> {
    if id != G13_INPUT_REPORT_ID {
        return Err(LossError::new(
            LossCause::ProbeFail,
            format!(
                "HID input report ID 0x{:02x}, want 0x{:02x}",
                id, G13_INPUT_REPORT_ID
            ),
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
    pub fn open() -> Result<(HidDevice, Discovery), LossError> {
        owner_gate(observed_owners())?;
        let discovery = discover();
        let Some(candidate) = discovery.candidates.first() else {
            let detail = discovery
                .rejections
                .iter()
                .map(|r| r.reason.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(LossError::new(
                LossCause::DeviceAbsent,
                if detail.is_empty() {
                    "HID discovery: no G13 collection matched VID/PID, usage, and 8/992-byte reports"
                        .into()
                } else {
                    format!(
                        "HID discovery: no G13 collection matched VID/PID, usage, and 8/992-byte reports (scanned {} interfaces: {})",
                        discovery.scanned, detail
                    )
                },
            ));
        };
        let handle = unsafe { open_path(&candidate.device_path, FILE_SHARE_MODE(0))? };
        if let Err(error) = owner_gate(observed_owners()) {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(LossError::new(
                LossCause::OwnerConflict,
                format!(
                    "running-process check failed after exclusive HID open: {}",
                    error.detail
                ),
            ));
        }
        let (read_event, write_event) = create_io_events(
            handle,
            || unsafe { CreateEventW(None, true, false, None) }.map_err(|e| e.to_string()),
            |owned| unsafe {
                let _ = CloseHandle(owned);
            },
        )
        .map_err(|reason| LossError::new(LossCause::ProbeFail, reason))?;
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
    pub fn write_report(&self, report: &[u8; G13_OUTPUT_REPORT_LENGTH]) -> Result<(), LossError> {
        owner_guarded(observed_owners(), || self.write_report_unchecked(report))
    }

    fn write_report_unchecked(
        &self,
        report: &[u8; G13_OUTPUT_REPORT_LENGTH],
    ) -> Result<(), LossError> {
        unsafe { ResetEvent(self.write_event) }.map_err(|e| {
            LossError::new(LossCause::ProbeFail, format!("HID reset write event: {e}"))
        })?;
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
            Err(e) => Err(LossError::new(
                LossCause::ProbeFail,
                format!("HID write: {e} (code {:#010x})", e.code().0 as u32),
            )),
        }
    }

    /// Read one 8-byte input report. `Ok(false)` means timeout with no data.
    pub fn read_input_timeout(
        &self,
        buffer: &mut [u8; G13_INPUT_REPORT_LENGTH],
        timeout: Duration,
    ) -> Result<bool, LossError> {
        unsafe { ResetEvent(self.read_event) }.map_err(|e| {
            LossError::new(LossCause::ProbeFail, format!("HID reset read event: {e}"))
        })?;
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
                        Err(LossError::new(
                            LossCause::ProbeFail,
                            format!("HID read wait failed: {result:?}"),
                        ))
                    }
                }
            }
            Err(e) => Err(LossError::new(
                LossCause::ProbeFail,
                format!("HID read: {e} (code {:#010x})", e.code().0 as u32),
            )),
        }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn new_overlapped(&self, event: HANDLE) -> OVERLAPPED {
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event;
        overlapped
    }

    fn overlapped_result(&self, overlapped: &OVERLAPPED, label: &str) -> Result<u32, LossError> {
        let mut transferred = 0u32;
        unsafe { GetOverlappedResult(self.handle, overlapped, &mut transferred, false) }.map_err(
            |e| {
                LossError::new(
                    LossCause::ProbeFail,
                    format!("{label} completion: {e} (code {:#010x})", e.code().0 as u32),
                )
            },
        )?;
        Ok(transferred)
    }

    fn wait_pending(
        &self,
        event: HANDLE,
        overlapped: &OVERLAPPED,
        timeout: Duration,
        label: &str,
    ) -> Result<u32, LossError> {
        match unsafe { WaitForSingleObject(event, timeout.as_millis() as u32) } {
            WAIT_OBJECT_0 => self.overlapped_result(overlapped, label),
            WAIT_TIMEOUT => {
                self.cancel_and_drain(overlapped, label)?;
                Err(LossError::new(
                    LossCause::CallTimeout,
                    format!("{label} timed out"),
                ))
            }
            result => {
                self.cancel_and_drain(overlapped, label)?;
                Err(LossError::new(
                    LossCause::ProbeFail,
                    format!("{label} wait failed: {result:?}"),
                ))
            }
        }
    }

    fn cancel_and_drain(&self, overlapped: &OVERLAPPED, label: &str) -> Result<(), LossError> {
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
            let mut transferred = 0u32;
            match GetOverlappedResult(self.handle, overlapped, &mut transferred, true) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ERROR_OPERATION_ABORTED.to_hresult() => Ok(()),
                Err(error) => Err(LossError::new(
                    LossCause::ProbeFail,
                    format!("{label} cancellation completion: {error}"),
                )),
            }
        }
    }

    /// Blank the panel, cancel any pending I/O, and close handles.
    pub fn close(&mut self) -> Result<(), LossError> {
        let blank_result = self.write_report(&super::g13::blank_report());
        self.close_handles();
        blank_result
            .map_err(|e| LossError::new(e.cause, format!("HID blank on close: {}", e.detail)))
    }

    /// Close immediately without a final write when LCore.exe is found running.
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

fn validate_transfer(label: &str, transferred: u32, expected: usize) -> Result<(), LossError> {
    if transferred as usize != expected {
        return Err(LossError::new(
            LossCause::TransferIncomplete { code: transferred },
            format!("{label} completed {transferred} of {expected} bytes"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WO-OPT04 A3 seam: classify supplied names with the SAME production
    /// classification (`classify_owner` -> `owner_name_match`) that the enumeration
    /// runs. The fixture names are encoded to UTF-16 here and fed to the real
    /// comparator, so the tests below keep their discriminating power over the
    /// comparison itself (length check, case-insensitivity, owner priority); they
    /// do not test a fixture-shaped stand-in.
    fn owners_from_names(names: Result<Vec<String>, String>) -> Result<ObservedOwners, String> {
        names.map(|names| {
            let mut observed = ObservedOwners::default();
            let mut units = Vec::new();
            for name in &names {
                units.clear();
                units.extend(name.encode_utf16());
                classify_owner(&mut observed, &units);
            }
            observed
        })
    }

    /// The production verdict over the seam's classification.
    fn owner_gate_names(names: Result<Vec<String>, String>) -> Result<(), LossError> {
        owner_gate(owners_from_names(names))
    }

    /// WO-OPT04 A4 measurement harness: the HEAD (base) form of the enumeration,
    /// verbatim, so the base allocation numbers are measured on the code shape
    /// this unit replaced, not on a paraphrase. `#[cfg(test)]` only.
    fn base_running_process_names() -> Result<Vec<String>, String> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .map_err(|e| format!("process enumeration failed: {e}"))?;
            snapshot_tick();
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

    /// WO-OPT04 A4 measurement harness: the HEAD (base) `owner_gate` over the
    /// `Vec<String>` shape, verbatim. `#[cfg(test)]` only.
    fn base_owner_gate(processes: Result<Vec<String>, String>) -> Result<(), LossError> {
        let processes = processes.map_err(|e| {
            LossError::new(
                LossCause::ProbeFail,
                format!("cannot enumerate running processes ({e}); refusing direct HID"),
            )
        })?;
        if let Some((_, message)) = COMPETING_OWNERS.iter().find(|(owner, _)| {
            processes
                .iter()
                .any(|name| name.eq_ignore_ascii_case(owner))
        }) {
            return Err(LossError::new(LossCause::OwnerConflict, (*message).into()));
        }
        Ok(())
    }

    #[test]
    fn owner_name_matching_is_exact_and_case_insensitive() {
        assert!(owner_gate_names(Ok(vec!["lcore.EXE".into()])).is_err());
        assert!(owner_gate_names(Ok(vec!["LOGI_LAMPARRAY_SERVICE.amd64.EXE".into()])).is_err());
        assert!(owner_gate_names(Ok(vec![
            "lcore-helper.exe".into(),
            "lghub.exe".into(),
            // WO-OPT04 A3 (i): the length check's own cases are names that are a
            // PREFIX of an owner name. Drop the check and the truncating zip makes
            // "LCore" match "LCore.exe" and "logi_lamparray_service" match the
            // LampArray owner, so these two inputs are what turn that mutation red;
            // lcore-helper.exe / lghub.exe are guarded by the content compare.
            "LCore".into(),
            "logi_lamparray_service".into(),
        ]))
        .is_ok());
    }

    #[test]
    fn owner_gate_names_competing_owner_conflict() {
        let refused = owner_gate_names(Ok(vec!["LCore.exe".into()])).unwrap_err();
        assert_eq!(refused.cause, LossCause::OwnerConflict);
    }

    #[test]
    fn enumeration_failure_names_probe_fail() {
        let error = owner_gate(Err("snapshot unavailable".into())).unwrap_err();
        assert_eq!(error.cause, LossCause::ProbeFail);
    }

    #[test]
    fn open_path_failure_names_open_failed_with_the_os_code() {
        // CreateFileW on an absent device path fails without touching hardware.
        let error = unsafe { open_path("\\\\.\\lcdsirplus-absent-unit-test", FILE_SHARE_MODE(0)) }
            .unwrap_err();
        assert!(
            matches!(error.cause, LossCause::OpenFailed { code } if code != 0),
            "the CreateFileW HRESULT rides with the cause: {error:?}"
        );
        let LossCause::OpenFailed { code } = error.cause else {
            panic!("expected open-failed cause");
        };
        assert_eq!(
            code & 0xFFFF,
            windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.0,
            "win32 error 2 inside the HRESULT 0x8007_00xx form"
        );
    }

    #[test]
    fn owner_detection_errors_fail_closed_without_private_data() {
        let error = owner_gate(Err("snapshot unavailable".into())).unwrap_err();
        assert!(error.detail.contains("refusing direct HID"));
        assert!(!error.detail.contains('\\'));
    }

    #[test]
    fn owner_refusal_is_actionable_and_retry_can_recover() {
        let refused = owner_gate_names(Ok(vec!["LCore.exe".into()])).unwrap_err();
        assert!(refused.detail.contains("exit Logitech Gaming Software"));
        let refused =
            owner_gate_names(Ok(vec!["logi_lamparray_service.AMD64.exe".into()])).unwrap_err();
        assert!(refused.detail.contains("stop Logitech LampArray service"));
        assert!(owner_gate_names(Ok(Vec::new())).is_ok());
    }

    #[test]
    fn owner_gate_prefers_lcore_when_both_owners_are_present() {
        // WO-OPT04 A2 (2): the shipped `COMPETING_OWNERS.iter().find(...)` gave
        // index 0 (LCore.exe) the win whatever the enumeration order. LampArray is
        // listed FIRST in each fixture below, so a first-match implementation
        // ("return on the first process seen") picks the LampArray message here and
        // this test goes red - A3 mutation (iii).
        for fixture in [
            vec![
                "logi_lamparray_service.AMD64.exe".into(),
                "LCore.exe".into(),
            ],
            vec![
                "LCore.exe".into(),
                "logi_lamparray_service.AMD64.exe".into(),
            ],
        ] {
            let refused = owner_gate_names(Ok(fixture)).unwrap_err();
            assert_eq!(refused.cause, LossCause::OwnerConflict);
            assert_eq!(refused.detail, COMPETING_OWNERS[0].1, "LCore wins");
        }
        // A LampArray-only census keeps the LampArray verdict (no priority leak).
        let refused =
            owner_gate_names(Ok(vec!["logi_lamparray_service.AMD64.exe".into()])).unwrap_err();
        assert_eq!(refused.detail, COMPETING_OWNERS[1].1);
    }

    #[test]
    fn partial_transfers_are_rejected() {
        let error = validate_transfer("HID write", 991, 992).unwrap_err();
        assert_eq!(error.cause, LossCause::TransferIncomplete { code: 991 });
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
            let result = owner_guarded(owners_from_names(Ok(vec!["LCore.exe".into()])), || {
                writes += 1;
                Ok(())
            });
            assert!(result.is_err(), "{phase}");
            assert_eq!(writes, 0, "{phase}");
        }
        let mut writes = 0;
        owner_guarded(owners_from_names(Ok(Vec::new())), || {
            writes += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(writes, 1);
    }

    #[test]
    fn ownership_snapshot_census_counts_creations_per_gate_call() {
        // WO-OPT04 A1: the census is MEASURED with the `SNAPSHOT_CREATIONS` counter
        // above (the lane's `presentmon.rs` WORKER_TICKS/worker_tick_count shape).
        let baseline = snapshot_creation_count();
        let _ = observed_owners().expect("the live process census is readable in tests");
        let hardened_per_call = snapshot_creation_count() - baseline;
        let baseline = snapshot_creation_count();
        let names = base_running_process_names().expect("the base census is readable in tests");
        let base_per_call = snapshot_creation_count() - baseline;
        assert_eq!(
            hardened_per_call, 1,
            "one snapshot per hardened classification call"
        );
        assert_eq!(base_per_call, 1, "the base form also created exactly one");
        assert_eq!(
            hardened_per_call, base_per_call,
            "creations per call must not increase"
        );
        assert!(
            !names.is_empty(),
            "the live census enumerates processes on this desktop"
        );
        // The per-write guard costs exactly one creation and runs before the write.
        // F-AUD-OPT04-2: two rows here were deleted, not weakened - `writes <= 1` was
        // true by the closure's construction (no power), and comparing the verdict of
        // the guard's snapshot against the flags of an EARLIER snapshot is a tautology
        // on stable state and a latent flake when an owner starts in between.
        let baseline = snapshot_creation_count();
        let guard = owner_guarded(observed_owners(), || Ok(()));
        let guard_per_call = snapshot_creation_count() - baseline;
        assert_eq!(guard_per_call, 1, "one snapshot per per-write guard");
        println!(
            "A1-SITE hid.rs owner gate: creations-per-call hardened={hardened_per_call} base={base_per_call} per-write-guard={guard_per_call} guard-ok={}",
            guard.is_ok()
        );
        println!("A1-SITE sdk.rs lcore_present: creations-per-call=1 (one CreateToolhelp32Snapshot site, sdk.rs:556, straight-line, source census)");
        println!("A1-ITERATION sdk-mode: snapshots-per-connected-loop-iteration=1 (mod.rs:1076 -> ownership_changed mod.rs:946 -> sdk::lcore_present, unconditional)");
        println!("A1-ITERATION hid-mode: 1 per iteration from the same unconditional loop check PLUS 1 per ACTUAL write (the per-write guard); mod.rs:1119 skips the write when the packed frame is unchanged, so a static display pays 1/iteration (F-AUD-OPT04-6)");
        println!("A1-OPEN hid.rs open(): snapshots-per-open=2 (the open-path gate + the post-open check)");
        println!("A1-SECOND button_poll default 50 ms (config.rs:279) = 20 iterations/s: sdk-mode=20 snapshots/s, hid-mode=20/s (no writes) up to 40/s (a write every iteration)");
        println!("A1-SCOPE per-iteration full-process snapshot = OWNER DECISION; this unit rate-limits nothing and coalesces nothing across iterations");
    }

    #[test]
    fn ownership_classification_allocates_no_per_process_string() {
        // WO-OPT04 A4: exact allocation integers, base vs hardened, on the SAME
        // fixed 500-name synthetic corpus. The measurement seam is the lane's own
        // counting allocator (`crate::backends::alloc_count`, cfg(test) only); the
        // window is armed only around each measurement and production bytes are
        // untouched. CPU is NOT measured and no CPU claim is made here.
        use crate::backends::alloc_count;

        // Corpus: 500 names of a uniform 12 bytes, so every byte total is an exact
        // integer; `owned` swaps the last entry for an LCore.exe (9 bytes).
        let corpus: Vec<Vec<u16>> = (0..500usize)
            .map(|index| format!("svc{index:05}.exe").encode_utf16().collect())
            .collect();
        let mut owned = corpus.clone();
        owned[499] = "LCore.exe".encode_utf16().collect();
        // The scaling control: the same shape over 4x the census (names kept at 12
        // units so every print stays comparable).
        let corpus4: Vec<Vec<u16>> = (0..2000usize)
            .map(|index| format!("svc{index:05}.exe").encode_utf16().collect())
            .collect();
        assert!(corpus.iter().all(|units| units.len() == 12));
        assert!(corpus4.iter().all(|units| units.len() == 12));
        assert_eq!(owned[499].len(), 9);
        let message_bytes = COMPETING_OWNERS[0].1.len();

        /// A size-class census expectation: (pixel, key, <=256 B, >256 B, total).
        fn counts(pixel: usize, key: usize, small: usize, big: usize) -> alloc_count::Counts {
            (pixel, key, small, big, pixel + key + small + big)
        }

        let measure = |run: &mut dyn FnMut()| -> alloc_count::Counts {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            run();
            alloc_count::subtract(alloc_count::snapshot(), before)
        };

        let base_shape = |names: &[Vec<u16>], reserve: bool| -> Result<(), LossError> {
            let mut names_vec = if reserve {
                Vec::with_capacity(names.len())
            } else {
                Vec::new()
            };
            for units in names {
                names_vec.push(String::from_utf16_lossy(units));
            }
            base_owner_gate(Ok(names_vec))
        };
        let hardened_shape = |names: &[Vec<u16>]| -> Result<(), LossError> {
            let mut observed = ObservedOwners::default();
            for units in names {
                classify_owner(&mut observed, units);
            }
            owner_gate(Ok(observed))
        };

        // (A) non-vacuity: one known allocation of a known size.
        let known = measure(&mut || {
            let block = Vec::<u8>::with_capacity(1024);
            assert_eq!(block.capacity(), 1024);
            drop(block);
        });
        let known_expected = counts(0, 0, 0, 1);
        assert_eq!(
            known, known_expected,
            "the counter must see a known 1024-byte block"
        );

        // (B) the base Vec<String> growth census (String::new() allocates) and (C) the
        // calibration probes. PRINT ONLY: these pin std's CURRENT doubling growth law
        // and its current 24 B String layout. As asserts they are a flake seed aimed at
        // a future toolchain (F-AUD-OPT04-1), and the claim this unit ships does not
        // need them - it needs "hardened allocates nothing per process" and "base
        // allocates per census entry".
        let growth = measure(&mut || {
            let mut names_vec: Vec<String> = Vec::new();
            for _ in 0..500 {
                names_vec.push(String::new());
            }
            drop(names_vec);
        });
        let cal_small = measure(&mut || {
            drop(Vec::<String>::with_capacity(8));
        });
        let cal_over = measure(&mut || {
            drop(Vec::<String>::with_capacity(11));
        });
        let cal_big = measure(&mut || {
            drop(Vec::<String>::with_capacity(512));
        });
        let conv_expect = |l: usize| -> alloc_count::Counts {
            if l <= 8 {
                // The min-capacity path: one small block.
                return counts(0, 0, 1, 0);
            }
            let (mut pixel, mut key, mut small, mut big) = (0usize, 0usize, 0usize, 0usize);
            if l == alloc_count::PIXELS {
                pixel = 1;
            } else if l == alloc_count::HID_REPORT {
                key = 1;
            } else if l <= 256 {
                small += 1;
            } else {
                big += 1;
            }
            if l.div_ceil(2) <= 256 {
                small += 1;
            } else {
                big += 1;
            }
            counts(pixel, key, small, big)
        };
        let mut sweep_mismatches = 0usize;
        for l in [
            1usize, 2, 8, 9, 12, 100, 200, 255, 256, 257, 300, 400, 500, 511, 512, 513, 514, 515,
            992, 1000, 2000, 6880,
        ] {
            let units: Vec<u16> = "s".repeat(l).encode_utf16().collect();
            let census = measure(&mut || {
                drop(String::from_utf16_lossy(&units));
            });
            let model = conv_expect(l);
            if census != model {
                sweep_mismatches += 1;
            }
            println!("A4-PROBE from_utf16_lossy L={l} census={census:?} model={model:?} match={} (pixel,key,<=256,>256,total)", census == model);
        }
        // A 12-unit process name costs 12 + 6 = 18 B per conversion; LCore.exe (9) 14 B.
        let per_name_bytes = 12 + 12usize.div_ceil(2);
        let lcore_name_bytes = 9 + 9usize.div_ceil(2);
        let string_bytes_ok = 500 * per_name_bytes;
        let string_bytes_owned = 499 * per_name_bytes + lcore_name_bytes;
        let capacity_bytes = 500 * std::mem::size_of::<String>();
        // The 8 measured growth blocks (2 small, 6 big) pin std's doubling sequence
        // from capacity 4: 4+8+16+32+64+128+256+512 = 1020 elements.
        let growth_bytes = 1020 * std::mem::size_of::<String>();

        // (D) base vs hardened on the SAME corpus, and on a 4x corpus.
        let base_fixed = measure(&mut || {
            base_shape(&corpus, true).unwrap();
        });
        let base_push = measure(&mut || {
            base_shape(&corpus, false).unwrap();
        });
        let base_refuse = measure(&mut || {
            assert!(base_shape(&owned, false).is_err());
        });
        let base_quad = measure(&mut || {
            base_shape(&corpus4, false).unwrap();
        });
        // (E) hardened, same corpora: zero allocations; a refusal pays only the message.
        let hardened_ok = measure(&mut || {
            hardened_shape(&corpus).unwrap();
        });
        let hardened_quad = measure(&mut || {
            hardened_shape(&corpus4).unwrap();
        });
        let hardened_refuse = measure(&mut || {
            assert!(hardened_shape(&owned).is_err());
        });
        assert_eq!(
            hardened_ok,
            counts(0, 0, 0, 0),
            "the hardened scan allocates NOTHING for a 500-process census"
        );
        assert_eq!(
            hardened_quad,
            counts(0, 0, 0, 0),
            "the hardened scan allocates NOTHING for a 2000-process census"
        );
        assert_eq!(
            hardened_refuse,
            counts(0, 0, 1, 0),
            "a hardened refusal pays only the shared message block"
        );
        assert!(
            base_fixed.4 > 0 && base_push.4 > 0 && base_refuse.4 > 0,
            "the base form allocates, so the counter is not vacuous on it"
        );
        // Derivation of the 3x floor: the base converts one String per process, so its
        // allocation count is linear in the census size; a size-independent shape (what
        // the hardened scan is) lands at ~1x. 4x names must therefore exceed 3x blocks.
        assert!(
            base_quad.4 > 3 * base_push.4,
            "base allocations scale with the census size: 4x names gave {} blocks vs {} at 1x",
            base_quad.4,
            base_push.4
        );

        println!("A4-CALIB with_capacity 8/11/512 = {cal_small:?}/{cal_over:?}/{cal_big:?} (24 B per element, 256 B class boundary straddled at 192/264); growth census {growth:?} = 8 blocks; per-conversion sweep mismatches={sweep_mismatches}/22");
        println!("A4-ALLOC base(fixed-capacity) allocations-per-call={} bytes-per-call={} ({} conversion blocks + {} B Vec block)", base_fixed.4, string_bytes_ok + capacity_bytes, string_bytes_ok, capacity_bytes);
        println!("A4-ALLOC base(shipped push shape) allocations-per-call={} bytes-per-call={} ({} conversion blocks + {} B Vec growth)", base_push.4, string_bytes_ok + growth_bytes, string_bytes_ok, growth_bytes);
        println!(
            "A4-ALLOC base(refusal) allocations-per-call={} bytes-per-call={}",
            base_refuse.4,
            string_bytes_owned + growth_bytes + message_bytes
        );
        println!(
            "A4-ALLOC base(4x census) allocations-per-call={} vs 1x={} (linear scaling, floor 3x)",
            base_quad.4, base_push.4
        );
        println!("A4-ALLOC hardened(no owner) allocations-per-call=0 at 1x AND at 4x; hardened(refusal) allocations-per-call=1 bytes-per-call={message_bytes} (the shared message String the base pays too)");
        println!("A4-BYTES base byte totals are MEASUREMENT PRINTS, not asserts (F-AUD-OPT04-1: pinning std's growth law/layout is a toolchain flake seed); the asserts are hardened=0, refusal=1 block, base>0, and 4x>3x scaling");
        println!(
            "A4-STRINGS per-process String conversions base=500 (measured 1000 blocks) hardened=0"
        );
        println!("A4-NONVACUOUS known block allocations=1 bytes=1024; base windows are non-zero; growth census 8 blocks = 2 small + 6 big");
        println!("A4-SNAPSHOTS creations per call base=1 hardened=1 (window A1 measured it); hardened does not increase them");
        println!("A4-CPU no CPU claim: CPU effect not established at available resolution");
    }
}
