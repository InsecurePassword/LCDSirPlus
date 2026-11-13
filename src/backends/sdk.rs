//! Trusted Logitech LCD SDK transport. Native calls are confined to one
//! owner thread and every request owns its buffers so a timed-out call can be
//! quarantined without retaining borrowed Rust data.

#![cfg(windows)]

use std::collections::HashSet;
use std::ffi::{c_void, CString, OsStr};
use std::fs;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use windows::core::{PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, GetLastError, SetLastError, BOOL, ERROR_ACCESS_DENIED,
    ERROR_NO_MORE_FILES, ERROR_SUCCESS, ERROR_TIMEOUT, GENERIC_READ, GENERIC_WRITE, HANDLE, HWND,
    LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetFileInformationByHandle, GetFileVersionInfoSizeW,
    GetFileVersionInfoW, VerQueryValueW, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, VS_FIXEDFILEINFO,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
    SMTO_ERRORONEXIT, WM_NULL,
};

use super::{LossCause, LossError};

const LCD_TYPE_MONO: i32 = 1;
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const PIXELS: usize = crate::model::WIDTH * crate::model::HEIGHT;
const EXPECTED_SIGNER_ORGANIZATION: &str = "Logitech Inc";
const EXPECTED_COMPANY: &str = "Logitech Inc.";
const EXPECTED_PRODUCT: &str = "Logitech Gaming Framework";
const EXPORTS: [&str; 6] = [
    "LogiLcdInit",
    "LogiLcdIsConnected",
    "LogiLcdIsButtonPressed",
    "LogiLcdMonoSetBackground",
    "LogiLcdUpdate",
    "LogiLcdShutdown",
];
static CIRCUIT_OPEN: AtomicBool = AtomicBool::new(false);
static SDK_OWNER_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECOVERY_BLOCKED: AtomicBool = AtomicBool::new(false);

pub fn circuit_open() -> bool {
    CIRCUIT_OPEN.load(Ordering::Acquire)
}

pub fn recovery_blocked() -> bool {
    RECOVERY_BLOCKED.load(Ordering::Acquire)
}

fn claim_sdk_owner() -> Result<(), LossError> {
    SDK_OWNER_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| {
            LossError::new(
                LossCause::OwnerConflict,
                "Logitech SDK discovery/owner operation is already active".into(),
            )
        })
}

fn wait_ready(
    receiver: &Receiver<Result<(), LossError>>,
    timeout: Duration,
) -> Result<(), LossError> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            CIRCUIT_OPEN.store(true, Ordering::Release);
            Err(LossError::new(
                LossCause::CallTimeout,
                "Logitech SDK discovery/initialization timed out; owner quarantined and circuit opened".into(),
            ))
        }
    }
}

#[derive(Clone, Debug)]
pub struct LCore {
    pub path: PathBuf,
    pub pid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
    size: u64,
    links: u32,
}

struct PinnedFile {
    file: File,
    identity: FileIdentity,
}

struct PinnedSdk {
    path: PathBuf,
    dll: PinnedFile,
    _lcore: PinnedFile,
    _boundaries: Vec<File>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthenticodeIdentity {
    organization: String,
    thumbprint: Vec<u8>,
}

pub fn lcore() -> Result<Option<LCore>, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("LCore discovery: {e}"))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if let Err(error) = Process32FirstW(snapshot, &mut entry) {
            let _ = CloseHandle(snapshot);
            return Err(format!("enumerate LCore.exe processes: {error}"));
        }
        let mut found = Vec::new();
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case("LCore.exe") {
                found.push(entry.th32ProcessID);
            }
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    let _ = CloseHandle(snapshot);
                    return Err(format!("enumerate LCore.exe processes: {error}"));
                }
                break;
            }
        }
        let _ = CloseHandle(snapshot);
        if found.len() > 1 {
            return Err("multiple running LCore.exe processes make SDK selection ambiguous".into());
        }
        let Some(pid) = found.first().copied() else {
            return Ok(None);
        };
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("open LCore.exe process {pid}: {e}"))?;
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result.map_err(|e| format!("query LCore.exe path: {e}"))?;
        let path = fs::canonicalize(PathBuf::from(String::from_utf16_lossy(
            &buffer[..length as usize],
        )))
        .map_err(|e| format!("canonicalize LCore.exe: {e}"))?;
        Ok(Some(LCore { path, pid }))
    }
}

/// RC-3 probe bound: ONE bounded read-only send against the discovered LCore
/// identity. Finite by construction (never `INFINITE`); same scale as the
/// existing 500 ms survival check, far inside the 5 s exit bound.
const ALIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Bounded read-only probe outcome; classification pattern is the shipped
/// hung-window probe in `providers/hang.rs` (WM_NULL-class send, finite
/// timeout, `SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT`, last-error split).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Aliveness {
    /// The window answered within the bound: its thread pump is alive.
    Responsive,
    /// The send timed out: the window exists and its pump is not responding.
    Unresponsive,
    /// No probeable window, or the send failed for a non-timeout reason.
    Indeterminate,
}

/// The single classification that governs the one guarded restart (RC-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LcoreClass {
    /// (c) The discovered LCore identity is absent.
    Absent,
    /// (a) Present and responsive: skip.
    Responsive,
    /// (b) Present with a probeable window whose pump is unresponsive: proceed
    /// to the existing terminate+relaunch sequence.
    Unresponsive,
    /// Present but unprobeable (no visible top-level window, enumeration
    /// failure, or non-timeout send failure): explicit named fallback, never
    /// a silent proceed.
    ProbeFail,
}

impl LcoreClass {
    /// Journal-safe label riding the existing `logitech-result` detail on the
    /// accepted RC-1 channel; no new journal event types exist.
    fn label(self) -> &'static str {
        match self {
            Self::Absent => "device-absent",
            Self::Responsive => "responsive",
            Self::Unresponsive => "unresponsive",
            Self::ProbeFail => "probe-fail",
        }
    }

    /// The named skip for every non-proceeding class; `None` is the one class
    /// that proceeds. A skip is always named (anti-masking).
    fn skip(self) -> Option<String> {
        let reason = match self {
            Self::Absent => "LCore.exe is not running",
            Self::Responsive => "LCore.exe is responsive",
            Self::ProbeFail => "LCore.exe has no probeable window",
            Self::Unresponsive => return None,
        };
        Some(format!(
            "{reason}; Logitech restart skipped ({})",
            self.label()
        ))
    }
}

/// One classification from presence + probe outcome: exactly one decision.
fn classify_lcore(present: bool, aliveness: Aliveness) -> LcoreClass {
    if !present {
        return LcoreClass::Absent;
    }
    match aliveness {
        Aliveness::Responsive => LcoreClass::Responsive,
        Aliveness::Unresponsive => LcoreClass::Unresponsive,
        Aliveness::Indeterminate => LcoreClass::ProbeFail,
    }
}

/// Probe seam: unit tests classify synthetic window states through an injected
/// implementation, so a real LCore process is never touched by tests. The
/// production wiring is `NativeAliveness`.
trait AlivenessApi {
    /// One visible top-level window owned by `pid`, chosen deterministically.
    fn visible_window_for(&mut self, pid: u32) -> Option<usize>;

    /// ONE bounded read-only WM_NULL-class send; requests no action from the
    /// target and never blocks past `timeout`.
    fn probe_window(&mut self, hwnd: usize, timeout: Duration) -> Aliveness;
}

/// One enumeration + one bounded send: no ladder, no repeats (the app's
/// 30x100 ms relaunch ladder is not ported).
fn probe_aliveness(api: &mut impl AlivenessApi, pid: u32) -> Aliveness {
    let Some(hwnd) = api.visible_window_for(pid) else {
        return Aliveness::Indeterminate;
    };
    api.probe_window(hwnd, ALIVENESS_PROBE_TIMEOUT)
}

#[derive(Default)]
struct WindowPick {
    pid: u32,
    hwnd: Option<usize>,
}

unsafe extern "system" fn pick_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let pick = &mut *(lparam.0 as *mut WindowPick);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let value = hwnd.0 as usize;
    if pid == pick.pid && pick.hwnd.is_none_or(|current| value < current) {
        pick.hwnd = Some(value);
    }
    BOOL(1)
}

struct NativeAliveness;

impl AlivenessApi for NativeAliveness {
    fn visible_window_for(&mut self, pid: u32) -> Option<usize> {
        let mut pick = WindowPick { pid, hwnd: None };
        unsafe {
            // Enumeration failure leaves no window: the caller classifies it
            // `Indeterminate` and journals the named `probe-fail` fallback.
            let _ = EnumWindows(
                Some(pick_window),
                LPARAM((&mut pick as *mut WindowPick) as isize),
            );
        }
        pick.hwnd
    }

    fn probe_window(&mut self, hwnd: usize, timeout: Duration) -> Aliveness {
        let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
        unsafe {
            SetLastError(ERROR_SUCCESS);
            let result = SendMessageTimeoutW(
                HWND(hwnd as *mut _),
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                timeout_ms,
                None,
            );
            if result.0 != 0 {
                Aliveness::Responsive
            } else if GetLastError() == ERROR_TIMEOUT {
                Aliveness::Unresponsive
            } else {
                Aliveness::Indeterminate
            }
        }
    }
}

/// One identity-bound restart. Called off the dashboard thread, never elevated.
pub fn restart_lcore(
    permit: &crate::recovery::RestartPermit,
    deadline: Instant,
) -> Result<(), String> {
    let owner = lcore()?.ok_or_else(|| {
        LcoreClass::Absent
            .skip()
            .expect("the absent class is always a named skip")
    })?;
    unsafe {
        let process = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            owner.pid,
        )
        .map_err(|error| format!("cannot open Logitech Gaming Software for restart: {error}"))?;
        let result = (|| {
            let mut session = 0;
            let mut current_session = 0;
            ProcessIdToSessionId(owner.pid, &mut session).map_err(|error| error.to_string())?;
            ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session)
                .map_err(|error| error.to_string())?;
            if session != current_session {
                return Err("LCore.exe belongs to another session; restart skipped".into());
            }
            let mut path = vec![0u16; 32768];
            let mut length = path.len() as u32;
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
            .map_err(|error| error.to_string())?;
            let actual = fs::canonicalize(PathBuf::from(String::from_utf16_lossy(
                &path[..length as usize],
            )))
            .map_err(|error| error.to_string())?;
            if actual != owner.path || WaitForSingleObject(process, 0) != WAIT_TIMEOUT {
                return Err("LCore.exe identity changed or exited; restart skipped".into());
            }
            // Reuse all existing signature, version, directory and file-identity gates.
            let _pinned = discover_pinned(&owner)?;
            if !lcore()?
                .is_some_and(|current| current.pid == owner.pid && current.path == owner.path)
            {
                return Err("LCore.exe discovery changed before restart; restart skipped".into());
            }
            // RC-3: one bounded read-only aliveness classification of the
            // re-pinned identity, before the one-shot permit is consumed. Only
            // (b) present-and-pump-unresponsive proceeds; (a) and the
            // unprobeable fallback return named skips (anti-masking).
            let class = classify_lcore(true, probe_aliveness(&mut NativeAliveness, owner.pid));
            if let Some(skip) = class.skip() {
                return Err(skip);
            }
            permit.commit(deadline)?;
            TerminateProcess(process, 0)
                .map_err(|error| format!("cannot close Logitech Gaming Software: {error}"))?;
            if WaitForSingleObject(process, 5_000) != WAIT_OBJECT_0 {
                return Err(
                    "Logitech Gaming Software did not exit within 5 seconds; relaunch skipped"
                        .into(),
                );
            }
            let mut child = std::process::Command::new(&owner.path)
                .current_dir(
                    owner
                        .path
                        .parent()
                        .ok_or("LCore.exe has no parent directory")?,
                )
                .spawn()
                .map_err(|error| {
                    format!("Logitech Gaming Software closed but could not relaunch: {error}")
                })?;
            std::thread::sleep(Duration::from_millis(500));
            if child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err("relaunched Logitech Gaming Software exited; no retry".into());
            }
            Ok(())
        })();
        let _ = CloseHandle(process);
        result
    }
}

fn expected_dll_path(owner: &LCore) -> Result<PathBuf, String> {
    if !owner
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n.eq_ignore_ascii_case("LCore.exe"))
    {
        return Err("trusted SDK discovery requires the running LCore.exe path".into());
    }
    let install = owner
        .path
        .parent()
        .ok_or("LCore.exe has no install directory")?;
    if !install
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n.eq_ignore_ascii_case("Logitech Gaming Software"))
    {
        return Err(format!(
            "LCore.exe is outside the canonical Logitech Gaming Software directory: {}",
            owner.path.display()
        ));
    }
    Ok(install.join(r"SDK\LCD\x64\LogitechLcd.dll"))
}

fn discover_pinned(owner: &LCore) -> Result<PinnedSdk, String> {
    let expected = expected_dll_path(owner)?;
    let roots = program_files_roots()?;
    reject_reparse_and_writable_chain(&owner.path, &roots)?;
    reject_reparse_and_writable_chain(&expected, &roots)?;
    let boundaries = pin_parent_chain(&expected, &roots)?;
    let lcore = pin_file(&owner.path)?;
    let dll = pin_file(&expected)?;
    let path = validate_candidate(&owner.path, &expected)?;
    let lcore_after = file_identity(&lcore.file, false)?;
    let dll_after = file_identity(&dll.file, false)?;
    if lcore.identity != lcore_after || dll.identity != dll_after {
        return Err("trusted object identity changed during validation".into());
    }
    Ok(PinnedSdk {
        path,
        dll,
        _lcore: lcore,
        _boundaries: boundaries,
    })
}

fn validate_candidate(lcore: &Path, candidate: &Path) -> Result<PathBuf, String> {
    validate_candidate_with(lcore, candidate, &WindowsTrust)
}

trait TrustProvider {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String>;
    fn roots(&self) -> Result<Vec<PathBuf>, String>;
    fn require_immutable(&self, path: &Path, roots: &[PathBuf]) -> Result<(), String>;
    fn signer(&self, path: &Path) -> Result<AuthenticodeIdentity, String>;
    fn version(&self, path: &Path) -> Result<[u16; 4], String>;
    fn metadata(&self, path: &Path) -> Result<(String, String), String>;
    fn pe(&self, path: &Path) -> Result<(), String>;
}

struct WindowsTrust;

impl TrustProvider for WindowsTrust {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
        fs::canonicalize(path)
            .map_err(|e| format!("canonical Logitech LCD SDK DLL is unavailable: {e}"))
    }
    fn roots(&self) -> Result<Vec<PathBuf>, String> {
        program_files_roots()
    }
    fn require_immutable(&self, path: &Path, roots: &[PathBuf]) -> Result<(), String> {
        reject_reparse_and_writable_chain(path, roots)
    }
    fn signer(&self, path: &Path) -> Result<AuthenticodeIdentity, String> {
        verify_authenticode(path)
    }
    fn version(&self, path: &Path) -> Result<[u16; 4], String> {
        file_version(path)
    }
    fn metadata(&self, path: &Path) -> Result<(String, String), String> {
        Ok((
            version_string(path, "CompanyName")?,
            version_string(path, "ProductName")?,
        ))
    }
    fn pe(&self, path: &Path) -> Result<(), String> {
        validate_pe(path)
    }
}

fn validate_candidate_with(
    lcore: &Path,
    candidate: &Path,
    trust: &dyn TrustProvider,
) -> Result<PathBuf, String> {
    let roots = trust.roots()?;
    if !roots.iter().any(|root| path_within(candidate, root)) {
        return Err(format!(
            "SDK candidate is lexically outside Program Files: {}",
            candidate.display()
        ));
    }
    trust.require_immutable(candidate, &roots)?;
    let dll = trust.canonicalize(candidate)?;
    if !dll
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n.eq_ignore_ascii_case("LogitechLcd.dll"))
    {
        return Err("SDK candidate has the wrong filename".into());
    }
    if !roots.iter().any(|root| path_within(&dll, root)) {
        return Err(format!(
            "SDK candidate is outside Program Files: {}",
            dll.display()
        ));
    }
    trust.require_immutable(&dll, &roots)?;
    let signer = trust.signer(&dll)?;
    if signer.organization != EXPECTED_SIGNER_ORGANIZATION {
        return Err(format!(
            "SDK signer organization is not exactly {EXPECTED_SIGNER_ORGANIZATION}"
        ));
    }
    let lcore_signer = trust.signer(lcore)?;
    if lcore_signer.organization != EXPECTED_SIGNER_ORGANIZATION {
        return Err(format!(
            "LCore signer organization is not exactly {EXPECTED_SIGNER_ORGANIZATION}"
        ));
    }
    if signer.thumbprint != lcore_signer.thumbprint {
        return Err("LCore and SDK signer certificates differ".into());
    }
    let exe_version = trust.version(lcore)?;
    let dll_version = trust.version(&dll)?;
    if exe_version != dll_version {
        return Err(format!(
            "LCore/SDK full file versions differ: {exe_version:?} vs {dll_version:?}"
        ));
    }
    for path in [lcore, dll.as_path()] {
        let (company, product) = trust.metadata(path)?;
        if company != EXPECTED_COMPANY || product != EXPECTED_PRODUCT {
            return Err(format!(
                "Logitech product metadata is incompatible for {}: company={company:?} product={product:?}",
                path.display()
            ));
        }
    }
    trust.pe(&dll)?;
    Ok(dll)
}

fn program_files_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for folder in [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86] {
        let raw = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, HANDLE::default()) }
            .map_err(|e| format!("resolve authoritative Program Files root: {e}"))?;
        let value = unsafe { raw.to_string() }
            .map_err(|e| format!("decode authoritative Program Files root: {e}"));
        unsafe { CoTaskMemFree(Some(raw.as_ptr().cast())) };
        let root = fs::canonicalize(value?)
            .map_err(|e| format!("canonicalize authoritative Program Files root: {e}"))?;
        let text = root.to_string_lossy();
        let local = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if local.len() < 3 || local.as_bytes()[1] != b':' {
            return Err(format!(
                "Program Files root is not a local drive path: {text}"
            ));
        }
        let drive: Vec<u16> = format!("{}\\", &local[..2])
            .encode_utf16()
            .chain(Some(0))
            .collect();
        if unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) } != DRIVE_FIXED {
            return Err(format!(
                "Program Files root is not on a fixed drive: {text}"
            ));
        }
        if !roots.iter().any(|known| known == &root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        Err("authoritative Program Files roots are unavailable".into())
    } else {
        Ok(roots)
    }
}

fn path_within(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy().to_ascii_lowercase();
    let root = root
        .to_string_lossy()
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    path == root || path.starts_with(&(root + "\\"))
}

fn reject_reparse_and_writable_chain(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let root = roots
        .iter()
        .find(|root| path_within(path, root))
        .ok_or("SDK path escaped Program Files")?;
    let mut current = root.clone();
    let root_metadata = fs::symlink_metadata(&current)
        .map_err(|e| format!("inspect trusted root {}: {e}", current.display()))?;
    if root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || can_open_for_write(&current, true)?
    {
        return Err(format!(
            "Program Files root is not immutable: {}",
            current.display()
        ));
    }
    for component in path.components().skip(root.components().count()) {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|e| format!("inspect trusted path {}: {e}", current.display()))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(format!(
                "trusted SDK path contains a reparse point: {}",
                current.display()
            ));
        }
        if can_open_for_write(&current, metadata.is_dir())? {
            return Err(format!(
                "current token can modify trusted SDK path: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

fn can_open_for_write(path: &Path, directory: bool) -> Result<bool, String> {
    let wide = wide(path);
    let flags = if directory {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        Default::default()
    };
    match unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0),
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    } {
        Ok(handle) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            Ok(true)
        }
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => Ok(false),
        Err(error) => Err(format!("check write access to {}: {error}", path.display())),
    }
}

fn pin_file(path: &Path) -> Result<PinnedFile, String> {
    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| format!("pin trusted file {}: {e}", path.display()))?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    let identity = file_identity(&file, false)?;
    Ok(PinnedFile { file, identity })
}

fn pin_directory(path: &Path) -> Result<File, String> {
    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| format!("pin trusted directory {}: {e}", path.display()))?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    let _ = file_identity(&file, true)?;
    Ok(file)
}

fn file_identity(file: &File, directory: bool) -> Result<FileIdentity, String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle().cast()), &mut info) }
        .map_err(|e| format!("pinned object identity is unavailable: {e}"))?;
    let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if is_directory != directory || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err("pinned object is not the required regular non-reparse type".into());
    }
    if !directory && info.nNumberOfLinks != 1 {
        return Err("trusted executable/DLL must have exactly one hard link".into());
    }
    Ok(FileIdentity {
        volume: info.dwVolumeSerialNumber,
        index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        size: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        links: info.nNumberOfLinks,
    })
}

fn require_same_identity(expected: &FileIdentity, loaded: &FileIdentity) -> Result<(), String> {
    if expected != loaded || loaded.links != 1 {
        Err("loaded SDK module identity differs from pinned verified DLL".into())
    } else {
        Ok(())
    }
}

fn pin_parent_chain(path: &Path, roots: &[PathBuf]) -> Result<Vec<File>, String> {
    let root = roots
        .iter()
        .find(|root| path_within(path, root))
        .ok_or("trusted path escaped Program Files")?;
    let mut pins = vec![pin_directory(root)?];
    let mut current = root.clone();
    let parent = path.parent().ok_or("trusted file has no parent")?;
    for component in parent.components().skip(root.components().count()) {
        current.push(component);
        pins.push(pin_directory(&current)?);
    }
    Ok(pins)
}

fn verify_authenticode(path: &Path) -> Result<AuthenticodeIdentity, String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Security::Cryptography::{
        CertGetCertificateContextProperty, CertGetNameStringW, CERT_NAME_ATTR_TYPE,
        CERT_SHA256_HASH_PROP_ID,
    };
    use windows::Win32::Security::WinTrust::*;

    let wide = wide(path);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL
            | WTD_DISABLE_MD2_MD4
            | WTD_REVOCATION_CHECK_CHAIN,
        dwUIContext: WTD_UICONTEXT_EXECUTE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    unsafe {
        let status = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        if status != 0 || data.hWVTStateData.is_invalid() {
            data.dwStateAction = WTD_STATEACTION_CLOSE;
            let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
            return Err(format!(
                "cached whole-chain Authenticode verification failed: 0x{status:08x}"
            ));
        }
        let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
        let signer = if provider.is_null() {
            std::ptr::null_mut()
        } else {
            WTHelperGetProvSignerFromChain(provider, 0, false, 0)
        };
        let provider_cert = if signer.is_null() {
            std::ptr::null_mut()
        } else {
            WTHelperGetProvCertFromChain(signer, 0)
        };
        let cert = if provider_cert.is_null() {
            std::ptr::null()
        } else {
            (*provider_cert).pCert
        };
        let oid = b"2.5.4.10\0";
        let required = if cert.is_null() {
            0
        } else {
            CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                Some(PCSTR(oid.as_ptr()).0.cast()),
                None,
            )
        };
        let mut organization = String::new();
        if required > 1 && required <= 1024 {
            let mut output = vec![0u16; required as usize];
            if CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                Some(PCSTR(oid.as_ptr()).0.cast()),
                Some(&mut output),
            ) == required
            {
                organization = String::from_utf16_lossy(&output[..required as usize - 1]);
            }
        }
        let mut hash_len = 0u32;
        let hash_size = if cert.is_null() {
            Err(windows::core::Error::from_win32())
        } else {
            CertGetCertificateContextProperty(cert, CERT_SHA256_HASH_PROP_ID, None, &mut hash_len)
        };
        let mut thumbprint = vec![0u8; hash_len as usize];
        let hash_result = hash_size.and_then(|_| {
            CertGetCertificateContextProperty(
                cert,
                CERT_SHA256_HASH_PROP_ID,
                Some(thumbprint.as_mut_ptr().cast()),
                &mut hash_len,
            )
        });
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        hash_result.map_err(|_| "signer SHA-256 thumbprint is unavailable")?;
        thumbprint.truncate(hash_len as usize);
        if organization.is_empty() || thumbprint.len() != 32 {
            return Err("verified signer identity is incomplete".into());
        }
        Ok(AuthenticodeIdentity {
            organization,
            thumbprint,
        })
    }
}

fn file_version(path: &Path) -> Result<[u16; 4], String> {
    let wide = wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return Err(format!(
                "version metadata is unavailable: {}",
                path.display()
            ));
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), 0, size, data.as_mut_ptr().cast())
            .map_err(|e| format!("read version metadata {}: {e}", path.display()))?;
        let query = [b'\\' as u16, 0];
        let mut value: *mut c_void = std::ptr::null_mut();
        let mut length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut value,
            &mut length,
        )
        .as_bool()
        {
            return Err(format!("query version metadata failed: {}", path.display()));
        }
        if value.is_null() || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32 {
            return Err(format!("invalid version metadata: {}", path.display()));
        }
        let info = &*(value.cast::<VS_FIXEDFILEINFO>());
        Ok([
            (info.dwFileVersionMS >> 16) as u16,
            info.dwFileVersionMS as u16,
            (info.dwFileVersionLS >> 16) as u16,
            info.dwFileVersionLS as u16,
        ])
    }
}

fn version_string(path: &Path, key: &str) -> Result<String, String> {
    let wide_path = wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide_path.as_ptr()), None);
        if size == 0 {
            return Err(format!(
                "version metadata is unavailable: {}",
                path.display()
            ));
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(
            PCWSTR(wide_path.as_ptr()),
            0,
            size,
            data.as_mut_ptr().cast(),
        )
        .map_err(|e| format!("read version metadata {}: {e}", path.display()))?;
        let translation_query: Vec<u16> = "\\VarFileInfo\\Translation"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut translation: *mut c_void = std::ptr::null_mut();
        let mut translation_length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(translation_query.as_ptr()),
            &mut translation,
            &mut translation_length,
        )
        .as_bool()
            || translation.is_null()
            || translation_length < 4
        {
            return Err(format!(
                "version translation is unavailable: {}",
                path.display()
            ));
        }
        let language = *(translation.cast::<u16>());
        let codepage = *(translation.cast::<u16>().add(1));
        let query: Vec<u16> = format!("\\StringFileInfo\\{language:04x}{codepage:04x}\\{key}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut value: *mut c_void = std::ptr::null_mut();
        let mut length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut value,
            &mut length,
        )
        .as_bool()
            || value.is_null()
            || length <= 1
            || length > 32768
        {
            return Err(format!("{key} metadata is unavailable: {}", path.display()));
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            value.cast::<u16>(),
            length as usize - 1,
        )))
    }
}

fn validate_pe(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("read SDK PE: {e}"))?;
    let u16_at = |at: usize| -> Option<u16> {
        Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
    };
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(0..2) != Some(b"MZ") {
        return Err("SDK DLL has no DOS header".into());
    }
    let pe = u32_at(0x3c).ok_or("truncated SDK DOS header")? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0") || u16_at(pe + 4) != Some(0x8664) {
        return Err("SDK DLL is not an AMD64 PE image".into());
    }
    let sections = u16_at(pe + 6).ok_or("truncated SDK PE header")? as usize;
    let optional_size = u16_at(pe + 20).ok_or("truncated SDK PE header")? as usize;
    let optional = pe + 24;
    if u16_at(optional) != Some(0x20b) {
        return Err("SDK DLL is not PE32+".into());
    }
    let export_rva = u32_at(optional + 112).ok_or("SDK export directory is absent")?;
    let section_table = optional + optional_size;
    let rva_offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|index| {
            let at = section_table + index * 40;
            let virtual_size = u32_at(at + 8)?;
            let virtual_address = u32_at(at + 12)?;
            let raw_size = u32_at(at + 16)?;
            let raw = u32_at(at + 20)?;
            let span = virtual_size.max(raw_size);
            (rva >= virtual_address && rva < virtual_address.checked_add(span)?)
                .then_some((raw + rva - virtual_address) as usize)
        })
    };
    let export = rva_offset(export_rva).ok_or("SDK export directory is invalid")?;
    let count = u32_at(export + 24).ok_or("truncated SDK export directory")? as usize;
    if count > 65536 {
        return Err("SDK export table is unreasonably large".into());
    }
    let names = rva_offset(u32_at(export + 32).ok_or("truncated SDK export directory")?)
        .ok_or("SDK export name table is invalid")?;
    let mut actual = HashSet::new();
    for index in 0..count {
        let name_rva = u32_at(names + index * 4).ok_or("truncated SDK export name table")?;
        let start = rva_offset(name_rva).ok_or("invalid SDK export name")?;
        let end = bytes[start..]
            .iter()
            .position(|b| *b == 0)
            .map(|n| start + n)
            .ok_or("unterminated SDK export name")?;
        if end - start <= 256 {
            actual.insert(String::from_utf8_lossy(&bytes[start..end]).into_owned());
        }
    }
    for required in EXPORTS {
        if !actual.contains(required) {
            return Err(format!("SDK DLL is missing export {required}"));
        }
    }
    Ok(())
}

type Init = unsafe extern "system" fn(*const u16, i32) -> u8;
type IsConnected = unsafe extern "system" fn(i32) -> u8;
type IsButton = unsafe extern "system" fn(i32) -> u8;
type SetBackground = unsafe extern "system" fn(*const u8) -> u8;
type Update = unsafe extern "system" fn();
type Shutdown = unsafe extern "system" fn();

trait Runtime {
    fn execute(&mut self, request: &Request) -> Reply;
    fn shutdown(&mut self);
}

struct Native {
    library: windows::Win32::Foundation::HMODULE,
    connected: IsConnected,
    button: IsButton,
    background: SetBackground,
    update: Update,
    shutdown_fn: Shutdown,
    _pinned: PinnedSdk,
}

impl Native {
    unsafe fn load(pinned: PinnedSdk, friendly_name: &str) -> Result<Self, LossError> {
        let path_wide = wide(&pinned.path);
        let library = LoadLibraryExW(
            PCWSTR(path_wide.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .map_err(|e| {
            LossError::new(
                LossCause::OpenFailed {
                    code: e.code().0 as u32,
                },
                format!("secure LoadLibraryExW {}: {e}", pinned.path.display()),
            )
        })?;
        let mut module_path = vec![0u16; 32768];
        let module_len = GetModuleFileNameW(library, &mut module_path) as usize;
        if module_len == 0 || module_len >= module_path.len() {
            let _ = FreeLibrary(library);
            CIRCUIT_OPEN.store(true, Ordering::Release);
            RECOVERY_BLOCKED.store(true, Ordering::Release);
            return Err(LossError::new(
                LossCause::CircuitOpen,
                "loaded SDK module path is unavailable; circuit opened".into(),
            ));
        }
        let loaded_path = PathBuf::from(String::from_utf16_lossy(&module_path[..module_len]));
        let loaded = match pin_file(&loaded_path) {
            Ok(file) => file,
            Err(error) => {
                let _ = FreeLibrary(library);
                CIRCUIT_OPEN.store(true, Ordering::Release);
                RECOVERY_BLOCKED.store(true, Ordering::Release);
                return Err(LossError::new(
                    LossCause::CircuitOpen,
                    format!("loaded SDK module cannot be pinned: {error}; circuit opened"),
                ));
            }
        };
        if let Err(error) = require_same_identity(&pinned.dll.identity, &loaded.identity) {
            let _ = FreeLibrary(library);
            CIRCUIT_OPEN.store(true, Ordering::Release);
            RECOVERY_BLOCKED.store(true, Ordering::Release);
            return Err(LossError::new(
                LossCause::CircuitOpen,
                format!("{error}; circuit opened"),
            ));
        }
        macro_rules! proc {
            ($name:literal, $ty:ty) => {{
                let name = CString::new($name).unwrap();
                let Some(address) = GetProcAddress(library, PCSTR(name.as_ptr().cast())) else {
                    let _ = FreeLibrary(library);
                    return Err(LossError::new(
                        LossCause::ProbeFail,
                        format!("SDK DLL lost required export {}", $name),
                    ));
                };
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address)
            }};
        }
        let init: Init = proc!("LogiLcdInit", Init);
        let native = Native {
            library,
            connected: proc!("LogiLcdIsConnected", IsConnected),
            button: proc!("LogiLcdIsButtonPressed", IsButton),
            background: proc!("LogiLcdMonoSetBackground", SetBackground),
            update: proc!("LogiLcdUpdate", Update),
            shutdown_fn: proc!("LogiLcdShutdown", Shutdown),
            _pinned: pinned,
        };
        let friendly: Vec<u16> = friendly_name.encode_utf16().chain(Some(0)).collect();
        if init(friendly.as_ptr(), LCD_TYPE_MONO) == 0 {
            let _ = FreeLibrary(native.library);
            return Err(LossError::new(
                LossCause::ProbeFail,
                "LogiLcdInit returned false".into(),
            ));
        }
        Ok(native)
    }
}

impl Runtime for Native {
    fn execute(&mut self, request: &Request) -> Reply {
        unsafe {
            match request {
                Request::Submit(frame) => {
                    if frame.len() != PIXELS || (self.background)(frame.as_ptr()) == 0 {
                        return Reply::Error("LogiLcdMonoSetBackground returned false".into());
                    }
                    (self.update)();
                    Reply::Unit
                }
                Request::Poll => {
                    (self.update)();
                    Reply::Poll {
                        connected: (self.connected)(LCD_TYPE_MONO) != 0,
                        buttons: [1, 2, 4, 8].map(|mask| (self.button)(mask) != 0),
                    }
                }
                Request::Close { blank } => {
                    if *blank {
                        let pixels = [0u8; PIXELS];
                        if (self.background)(pixels.as_ptr()) == 0 {
                            return Reply::Error(
                                "LogiLcdMonoSetBackground blank returned false".into(),
                            );
                        }
                        (self.update)();
                    }
                    Reply::Unit
                }
            }
        }
    }

    fn shutdown(&mut self) {
        unsafe {
            (self.shutdown_fn)();
            if !self.library.is_invalid() {
                let _ = FreeLibrary(self.library);
                self.library = Default::default();
            }
        }
    }
}

enum Request {
    Submit(Vec<u8>),
    Poll,
    Close { blank: bool },
}

enum Reply {
    Unit,
    Poll { connected: bool, buttons: [bool; 4] },
    Error(String),
}

struct Envelope {
    request: Request,
    reply: SyncSender<Reply>,
}

pub struct SdkDevice {
    requests: Option<SyncSender<Envelope>>,
}

impl SdkDevice {
    pub fn open(owner: &LCore, friendly_name: &str) -> Result<Self, LossError> {
        if CIRCUIT_OPEN.load(Ordering::Acquire) {
            return Err(LossError::new(
                LossCause::CircuitOpen,
                "Logitech SDK circuit is open after an unreturning native owner".into(),
            ));
        }
        claim_sdk_owner()?;
        let expected_owner = owner.clone();
        let friendly_name = friendly_name.to_string();
        let (requests, receiver) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let spawn = std::thread::Builder::new()
            .name("lcdsirplus-sdk-owner".into())
            .spawn(move || unsafe {
                let result = match lcore() {
                    Ok(Some(current))
                        if current.pid == expected_owner.pid
                            && current.path == expected_owner.path =>
                    {
                        discover_pinned(&current)
                            .inspect_err(|_| {
                                RECOVERY_BLOCKED.store(true, Ordering::Release);
                            })
                            .map_err(|error| LossError::new(LossCause::ProbeFail, error))
                    }
                    Ok(_) => Err(LossError::new(
                        LossCause::ProbeFail,
                        "LCore.exe identity changed before SDK discovery".into(),
                    )),
                    Err(error) => Err(LossError::new(LossCause::ProbeFail, error)),
                };
                match result.and_then(|pinned| Native::load(pinned, &friendly_name)) {
                    Ok(native) => {
                        let _ = ready_tx.send(Ok(()));
                        owner_loop(Box::new(native), receiver);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
                SDK_OWNER_ACTIVE.store(false, Ordering::Release);
            });
        if let Err(error) = spawn {
            SDK_OWNER_ACTIVE.store(false, Ordering::Release);
            return Err(LossError::new(
                LossCause::ProbeFail,
                format!("spawn Logitech SDK owner: {error}"),
            ));
        }
        match wait_ready(&ready_rx, CALL_TIMEOUT) {
            Ok(()) => {
                let device = Self {
                    requests: Some(requests),
                };
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match device.poll() {
                        Ok((true, _)) => return Ok(device),
                        Ok((false, _)) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(750));
                        }
                        Ok((false, _)) => {
                            return Err(LossError::new(
                                LossCause::DeviceAbsent,
                                "Logitech SDK monochrome LCD did not connect within 5 seconds"
                                    .into(),
                            ))
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    fn call(&self, request: Request) -> Result<Reply, LossError> {
        self.call_timeout(request, CALL_TIMEOUT)
    }

    fn call_timeout(&self, request: Request, timeout: Duration) -> Result<Reply, LossError> {
        let requests = self.requests.as_ref().ok_or_else(|| {
            LossError::new(
                LossCause::ShutdownRequested,
                "Logitech SDK device is closed".into(),
            )
        })?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout;
        let mut envelope = Envelope {
            request,
            reply: reply_tx,
        };
        loop {
            match requests.try_send(envelope) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    envelope = returned;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TrySendError::Full(_)) => {
                    CIRCUIT_OPEN.store(true, Ordering::Release);
                    return Err(LossError::new(
                        LossCause::CircuitOpen,
                        "Logitech SDK request dispatch timed out; circuit opened".into(),
                    ));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(LossError::new(
                        LossCause::ShutdownRequested,
                        "Logitech SDK owner disconnected".into(),
                    ));
                }
            }
        }
        reply_rx.recv_timeout(timeout).map_err(|_| {
            CIRCUIT_OPEN.store(true, Ordering::Release);
            LossError::new(
                LossCause::CallTimeout,
                "Logitech SDK native call timed out; owner quarantined and circuit opened".into(),
            )
        })
    }

    pub fn submit(&self, pixels: Vec<u8>) -> Result<(), LossError> {
        match self.call(Request::Submit(pixels))? {
            Reply::Unit => Ok(()),
            Reply::Error(error) => Err(LossError::new(
                LossCause::TransferIncomplete { code: 0 },
                error,
            )),
            Reply::Poll { .. } => Err(LossError::new(
                LossCause::ProbeFail,
                "unexpected Logitech SDK submit reply".into(),
            )),
        }
    }

    pub fn poll(&self) -> Result<(bool, [bool; 4]), LossError> {
        match self.call(Request::Poll)? {
            Reply::Poll { connected, buttons } => Ok((connected, buttons)),
            Reply::Error(error) => Err(LossError::new(LossCause::ProbeFail, error)),
            Reply::Unit => Err(LossError::new(
                LossCause::ProbeFail,
                "unexpected Logitech SDK poll reply".into(),
            )),
        }
    }

    pub fn close(&mut self, blank: bool) -> Result<(), LossError> {
        let reply = self.call(Request::Close { blank });
        self.requests.take();
        match reply? {
            Reply::Unit => Ok(()),
            Reply::Error(error) => Err(LossError::new(
                LossCause::TransferIncomplete { code: 0 },
                error,
            )),
            Reply::Poll { .. } => Err(LossError::new(
                LossCause::ProbeFail,
                "unexpected Logitech SDK close reply".into(),
            )),
        }
    }
}

impl Drop for SdkDevice {
    fn drop(&mut self) {
        if self.requests.is_some() {
            let _ = self.close(true);
        }
    }
}

fn owner_loop(mut runtime: Box<dyn Runtime>, requests: Receiver<Envelope>) {
    let mut shut_down = false;
    while let Ok(envelope) = requests.recv() {
        let close = matches!(envelope.request, Request::Close { .. });
        let reply = runtime.execute(&envelope.request);
        if close {
            runtime.shutdown();
            shut_down = true;
            let _ = envelope.reply.send(reply);
            break;
        }
        let _ = envelope.reply.send(reply);
    }
    if !shut_down {
        runtime.shutdown();
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Fake(Arc<Mutex<Vec<&'static str>>>);
    impl Runtime for Fake {
        fn execute(&mut self, request: &Request) -> Reply {
            self.0.lock().unwrap().push(match request {
                Request::Submit(_) => "submit",
                Request::Poll => "poll",
                Request::Close { .. } => "close",
            });
            match request {
                Request::Submit(frame) if frame.is_empty() => Reply::Error("injected".into()),
                Request::Poll => Reply::Poll {
                    connected: true,
                    buttons: [true, false, false, true],
                },
                _ => Reply::Unit,
            }
        }
        fn shutdown(&mut self) {
            self.0.lock().unwrap().push("shutdown");
        }
    }

    struct Slow;
    impl Runtime for Slow {
        fn execute(&mut self, _request: &Request) -> Reply {
            std::thread::sleep(Duration::from_millis(30));
            Reply::Unit
        }
        fn shutdown(&mut self) {}
    }

    struct FakeTrust {
        signer: &'static str,
        company: &'static str,
        product: &'static str,
        lcore_thumbprint: u8,
        dll_thumbprint: u8,
        immutable: bool,
        pe: bool,
        dll_version: [u16; 4],
    }

    impl Default for FakeTrust {
        fn default() -> Self {
            Self {
                signer: EXPECTED_SIGNER_ORGANIZATION,
                company: EXPECTED_COMPANY,
                product: EXPECTED_PRODUCT,
                lcore_thumbprint: 1,
                dll_thumbprint: 1,
                immutable: true,
                pe: true,
                dll_version: [9, 4, 0, 0],
            }
        }
    }

    impl TrustProvider for FakeTrust {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(path.to_path_buf())
        }
        fn roots(&self) -> Result<Vec<PathBuf>, String> {
            Ok(vec![PathBuf::from(r"C:\Program Files")])
        }
        fn require_immutable(&self, _path: &Path, _roots: &[PathBuf]) -> Result<(), String> {
            self.immutable
                .then_some(())
                .ok_or_else(|| "writable path".into())
        }
        fn signer(&self, path: &Path) -> Result<AuthenticodeIdentity, String> {
            Ok(AuthenticodeIdentity {
                organization: self.signer.into(),
                thumbprint: vec![
                    if path.extension().is_some_and(|value| value == "dll") {
                        self.dll_thumbprint
                    } else {
                        self.lcore_thumbprint
                    };
                    32
                ],
            })
        }
        fn version(&self, path: &Path) -> Result<[u16; 4], String> {
            Ok(if path.extension().is_some_and(|value| value == "dll") {
                self.dll_version
            } else {
                [9, 4, 0, 0]
            })
        }
        fn metadata(&self, _path: &Path) -> Result<(String, String), String> {
            Ok((self.company.into(), self.product.into()))
        }
        fn pe(&self, _path: &Path) -> Result<(), String> {
            self.pe.then_some(()).ok_or_else(|| "bad PE".into())
        }
    }

    #[test]
    fn owner_serializes_submit_poll_close_and_shutdown() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel(1);
        let thread_calls = calls.clone();
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(thread_calls)), rx));
        for request in [
            Request::Submit(vec![0; PIXELS]),
            Request::Poll,
            Request::Close { blank: true },
        ] {
            let (reply, receive) = mpsc::sync_channel(1);
            tx.send(Envelope { request, reply }).unwrap();
            receive.recv().unwrap();
        }
        thread.join().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["submit", "poll", "close", "shutdown"]
        );
    }

    #[test]
    fn owner_propagates_native_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel(1);
        let thread_calls = calls.clone();
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(thread_calls)), rx));
        let (reply, receive) = mpsc::sync_channel(1);
        tx.send(Envelope {
            request: Request::Submit(Vec::new()),
            reply,
        })
        .unwrap();
        assert!(matches!(receive.recv().unwrap(), Reply::Error(error) if error == "injected"));
        drop(tx);
        thread.join().unwrap();
    }

    #[test]
    fn path_boundary_is_component_aware() {
        assert!(path_within(
            Path::new(r"C:\Program Files\Logitech\x"),
            Path::new(r"C:\Program Files")
        ));
        assert!(!path_within(
            Path::new(r"C:\Program FilesX\Logitech"),
            Path::new(r"C:\Program Files")
        ));
    }

    /// Synthetic window states behind the RC-3 probe seam: no real LCore is
    /// ever enumerated, sent to, opened, terminated or relaunched by tests.
    struct ScriptedAliveness {
        windows: Vec<usize>,
        outcome: Aliveness,
        enumerations: usize,
        sends: Vec<(usize, Duration)>,
    }

    impl ScriptedAliveness {
        fn new(windows: Vec<usize>, outcome: Aliveness) -> Self {
            Self {
                windows,
                outcome,
                enumerations: 0,
                sends: Vec::new(),
            }
        }
    }

    impl AlivenessApi for ScriptedAliveness {
        fn visible_window_for(&mut self, _pid: u32) -> Option<usize> {
            self.enumerations += 1;
            self.windows.iter().copied().min()
        }

        fn probe_window(&mut self, hwnd: usize, timeout: Duration) -> Aliveness {
            self.sends.push((hwnd, timeout));
            self.outcome
        }
    }

    #[test]
    fn absent_lcore_is_named_device_absent_and_the_probe_never_starts() {
        // Presence dominates the classification: an absent service is (c)
        // whatever the probe would have said, so the restart returns at the
        // existing discovery gate, before any send, permit.commit, terminate.
        for aliveness in [
            Aliveness::Responsive,
            Aliveness::Unresponsive,
            Aliveness::Indeterminate,
        ] {
            assert_eq!(classify_lcore(false, aliveness), LcoreClass::Absent);
        }
        let class = LcoreClass::Absent;
        assert_eq!(class.label(), LossCause::DeviceAbsent.short());
        let skip = class.skip().expect("absent is a named skip");
        assert!(skip.contains(LossCause::DeviceAbsent.short().as_str()));
        assert!(skip.contains("Logitech restart skipped"));
        // A service with no probeable window performs zero sends and zero
        // terminate calls: the probe is attempted once, never repeated.
        let mut api = ScriptedAliveness::new(Vec::new(), Aliveness::Indeterminate);
        assert_eq!(probe_aliveness(&mut api, 4242), Aliveness::Indeterminate);
        assert_eq!((api.enumerations, api.sends.len()), (1, 0));
    }

    #[test]
    fn responsive_lcore_skips_with_the_responsive_label_and_terminates_nothing() {
        let mut api = ScriptedAliveness::new(vec![7, 3, 9], Aliveness::Responsive);
        let class = classify_lcore(true, probe_aliveness(&mut api, 4242));
        assert_eq!(class, LcoreClass::Responsive);
        let skip = class.skip().expect("responsive is a named skip");
        assert!(skip.contains("responsive"));
        assert!(skip.contains("Logitech restart skipped"));
        assert_eq!(api.enumerations, 1, "one enumeration, no ladder");
        assert_eq!(api.sends.len(), 1, "ONE probe total");
        assert_eq!(api.sends[0].0, 3, "deterministic lowest-window pick");
    }

    #[test]
    fn unresponsive_lcore_proceeds_once_to_the_existing_restart_sequence() {
        let mut api = ScriptedAliveness::new(vec![3], Aliveness::Unresponsive);
        let class = classify_lcore(true, probe_aliveness(&mut api, 4242));
        assert_eq!(class, LcoreClass::Unresponsive);
        assert!(
            class.skip().is_none(),
            "(b) is the only class that proceeds"
        );
        assert_eq!(api.enumerations, 1);
        assert_eq!(api.sends.len(), 1, "one probe, then one decision");
        // Proceeding falls through to the byte-identical `permit.commit` +
        // terminate + 5 s bound + relaunch + 500 ms survival sequence, so the
        // CAS permit is consumed exactly once per restart (the RC-1 permit
        // single-winner tests in recovery.rs keep that assertion green).
    }

    #[test]
    fn no_probeable_window_uses_the_named_probe_fail_fallback_not_a_silent_proceed() {
        let mut api = ScriptedAliveness::new(Vec::new(), Aliveness::Responsive);
        let class = classify_lcore(true, probe_aliveness(&mut api, 4242));
        assert_eq!(class, LcoreClass::ProbeFail);
        assert_eq!(class.label(), LossCause::ProbeFail.short());
        let skip = class.skip().expect("the fallback is named");
        assert!(skip.contains(LossCause::ProbeFail.short().as_str()));
        assert!(skip.contains("Logitech restart skipped"));
        assert_eq!((api.enumerations, api.sends.len()), (1, 0));
    }

    #[test]
    fn indeterminate_send_falls_back_to_the_named_probe_fail_label() {
        let mut api = ScriptedAliveness::new(vec![5], Aliveness::Indeterminate);
        assert_eq!(
            classify_lcore(true, probe_aliveness(&mut api, 4242)),
            LcoreClass::ProbeFail
        );
        assert_eq!(api.sends.len(), 1, "one bounded send, no repeat");
    }

    #[test]
    fn probe_bound_is_documented_and_finite_never_infinite() {
        let mut api = ScriptedAliveness::new(vec![5], Aliveness::Responsive);
        probe_aliveness(&mut api, 4242);
        assert_eq!(api.sends[0].1, ALIVENESS_PROBE_TIMEOUT);
        assert_eq!(ALIVENESS_PROBE_TIMEOUT, Duration::from_millis(500));
        assert!(
            ALIVENESS_PROBE_TIMEOUT.as_millis() < u32::MAX as u128,
            "the send bound is finite, never INFINITE"
        );
        assert!(
            ALIVENESS_PROBE_TIMEOUT < Duration::from_secs(5),
            "the probe stays inside the existing 5 s exit bound scale"
        );
    }

    #[test]
    fn rc3_labels_ride_journal_detail_and_never_become_event_names() {
        for class in [
            LcoreClass::Absent,
            LcoreClass::Responsive,
            LcoreClass::Unresponsive,
            LcoreClass::ProbeFail,
        ] {
            let label = class.label();
            assert!(label.len() <= 32, "the detail clamp stays bounded: {label}");
            for event in [
                "healthy",
                "finished",
                "disconnected",
                "logitech-attempt",
                "logitech-result",
                "application-attempt",
            ] {
                assert_ne!(label, event, "labels ride detail, never an event name");
            }
        }
    }

    #[test]
    fn every_skip_names_its_label_and_only_unresponsive_proceeds() {
        for class in [
            LcoreClass::Absent,
            LcoreClass::Responsive,
            LcoreClass::ProbeFail,
        ] {
            let skip = class.skip().unwrap_or_default();
            assert!(!skip.is_empty(), "no silent skip: {class:?}");
            assert!(
                skip.contains(class.label()),
                "the skip names its own label: {skip}"
            );
            assert!(skip.contains("Logitech restart skipped"));
        }
        assert!(LcoreClass::Unresponsive.skip().is_none());
    }

    #[test]
    fn trust_pipeline_accepts_only_complete_fake_evidence() {
        let exe = Path::new(r"C:\Program Files\Logitech Gaming Software\LCore.exe");
        let dll =
            Path::new(r"C:\Program Files\Logitech Gaming Software\SDK\LCD\x64\LogitechLcd.dll");
        assert_eq!(
            validate_candidate_with(exe, dll, &FakeTrust::default()).unwrap(),
            dll
        );

        let mut trust = FakeTrust {
            signer: "Not Logitech Inc.",
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            immutable: false,
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            pe: false,
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            dll_version: [9, 4, 0, 1],
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            dll_thumbprint: 2,
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            company: "Not Logitech Inc.",
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
        trust = FakeTrust {
            product: "Logitech Gaming Framework Extra",
            ..Default::default()
        };
        assert!(validate_candidate_with(exe, dll, &trust).is_err());
    }

    #[test]
    fn native_pin_rejects_hard_linked_file() {
        let root = std::env::temp_dir().join(format!(
            "lcdsirplus-sdk-hardlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let original = root.join("LogitechLcd.dll");
        let alias = root.join("alias.dll");
        fs::write(&original, b"test").unwrap();
        fs::hard_link(&original, &alias).unwrap();
        assert!(pin_file(&original).err().unwrap().contains("hard link"));
        fs::remove_file(alias).unwrap();
        fs::remove_file(original).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn loaded_object_identity_rejects_substitution() {
        let expected = FileIdentity {
            volume: 1,
            index: 2,
            size: 3,
            links: 1,
        };
        assert!(require_same_identity(&expected, &expected).is_ok());
        let swapped = FileIdentity {
            index: 4,
            ..expected.clone()
        };
        assert!(require_same_identity(&expected, &swapped).is_err());
    }

    #[test]
    #[ignore = "read-only installed Logitech signature/version/object policy"]
    fn installed_sdk_policy_read_only() {
        let roots = program_files_roots().unwrap();
        let Some(exe) = roots
            .into_iter()
            .map(|root| root.join(r"Logitech Gaming Software\LCore.exe"))
            .find(|path| path.is_file())
        else {
            return;
        };
        let pinned = discover_pinned(&LCore { path: exe, pid: 0 }).unwrap();
        assert_eq!(pinned.dll.identity.links, 1);
    }

    #[test]
    fn unreturning_owner_opens_the_circuit_without_borrowed_data() {
        CIRCUIT_OPEN.store(false, Ordering::Release);
        let (requests, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || owner_loop(Box::new(Slow), receiver));
        let mut device = SdkDevice {
            requests: Some(requests),
        };
        let error = device
            .call_timeout(Request::Submit(vec![0; PIXELS]), Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(error.cause, LossCause::CallTimeout);
        assert!(error.detail.contains("timed out") && circuit_open());
        device.requests.take();
        thread.join().unwrap();
        CIRCUIT_OPEN.store(false, Ordering::Release);
    }

    #[test]
    fn open_refused_while_the_circuit_is_open_names_circuit_open() {
        CIRCUIT_OPEN.store(true, Ordering::Release);
        let owner = LCore {
            path: PathBuf::from(r"C:\fixture\Logitech Gaming Software\LCore.exe"),
            pid: 0,
        };
        let error = SdkDevice::open(&owner, "fixture").err().unwrap();
        assert_eq!(error.cause, LossCause::CircuitOpen);
        CIRCUIT_OPEN.store(false, Ordering::Release);
    }

    #[test]
    fn stalled_owned_discovery_opens_circuit_and_blocks_overlap() {
        CIRCUIT_OPEN.store(false, Ordering::Release);
        SDK_OWNER_ACTIVE.store(false, Ordering::Release);
        claim_sdk_owner().unwrap();
        assert_eq!(
            claim_sdk_owner().err().unwrap().cause,
            LossCause::OwnerConflict
        );
        let (ready, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let owned = String::from("owned validation data");
            std::thread::sleep(Duration::from_millis(20));
            assert_eq!(owned, "owned validation data");
            let _ = ready.send(Ok(()));
            SDK_OWNER_ACTIVE.store(false, Ordering::Release);
        });
        let error = wait_ready(&receiver, Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(error.cause, LossCause::CallTimeout);
        assert!(circuit_open() && claim_sdk_owner().is_err());
        thread.join().unwrap();
        CIRCUIT_OPEN.store(false, Ordering::Release);
    }
}
