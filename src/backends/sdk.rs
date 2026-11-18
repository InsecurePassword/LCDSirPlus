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
use std::sync::Arc;
use std::sync::Mutex;
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
/// F-REC-1 (WO-U45A-REC-RECOVERY): the episode marker for an owner
/// quarantined by a non-returning native call (the 11/18 livelock class).
/// Trust-pipeline circuit openings set `RECOVERY_BLOCKED` beside the flag,
/// never this, so the resolver only ever resolves the quarantine class.
static OWNER_QUARANTINED: AtomicBool = AtomicBool::new(false);
/// Fresh-owner recovery state machine, ticked once per ladder pass by
/// `try_circuit_recovery` (one tick per pass, so no action is ever faster
/// than the configured reconnect cadence).
static CIRCUIT_RECOVERY: Mutex<CircuitRecovery> = Mutex::new(CircuitRecovery {
    responsive_ticks: 0,
    attempts: 0,
    in_flight: 0,
    detached: 0,
});

/// Consecutive device-present + LCore-responsive ticks required before the
/// first fresh-open attempt (N=3, config-free).
const RECOVERY_PROBE_TICKS: usize = 3;
/// Cap of the exponential attempt gap, in consecutive ticks: 16 × N, so a
/// permanently failing resolver stops attempting after 48 ladder passes
/// between attempts at any cadence.
const RECOVERY_ATTEMPT_GAP_CAP_TICKS: usize = 48;

/// The one recovery audit line. The journal's event vocabulary
/// (`application-*`, `logitech-*`, `finished`) is untouched; the line rides
/// the existing `loss-cause=` detail convention (RC-1) with the
/// `circuit-open` label, mirroring the RC-3 label rule.
pub const CIRCUIT_RECOVERY_AUDIT: &str = "Logitech SDK circuit closed: the fresh owner completed a call after the unreturning-owner episode; loss-cause=circuit-open";

#[derive(Default)]
struct CircuitRecovery {
    /// Consecutive ticks that saw the device present AND LCore responsive.
    responsive_ticks: usize,
    /// Fresh-open attempts spent in this episode.
    attempts: usize,
    /// The recovery window: at most ONE quarantined owner (slot exclusivity,
    /// WO-U45A-FIX binding amendment). `quarantined` registers the slot, it
    /// never stacks, so a second poisoning of the fresh owner cannot lock the
    /// window the way the F-1 counter increment did.
    in_flight: usize,
    /// Owner threads the resolver displaced out of the slot (their calls are
    /// unreturnable, so they are detached). Tracked separately from the
    /// window: they never gate authorization, and a thread that ever returns
    /// self-retires out of this census, restoring headroom.
    detached: usize,
}

impl CircuitRecovery {
    /// One owner quarantined by a timed-out native call. Slot exclusivity:
    /// the window registers the ONE quarantined owner, so a repeat
    /// quarantine - the fresh owner's own call timing out - keeps the window
    /// live (F-1 repair) instead of stacking a permanent block.
    fn quarantined(&mut self) {
        self.in_flight = 1;
        self.responsive_ticks = 0;
    }

    /// The resolver took the slot away from a live owner: that thread is
    /// displaced/detached, counted apart from the window.
    fn displaced(&mut self) {
        self.detached += 1;
    }

    /// A thread that had no window slot to release returned cleanly (the
    /// self-retire tail): it leaves the detached census, restoring headroom.
    /// The window owner's return goes through the `in_flight` arm.
    fn retired(&mut self) {
        if self.in_flight == 1 {
            self.in_flight = 0;
        } else {
            self.detached = self.detached.saturating_sub(1);
        }
    }

    /// The episode is over: the shape resets to the shipped baseline.
    fn recovered(&mut self) {
        *self = Self::default();
    }

    /// One ladder tick. Returns true only on the tick that authorizes the
    /// one fresh-open attempt: the streak must be unbroken (`device-present`
    /// and responsive every tick; any miss restarts it), at most one
    /// quarantined owner may be in flight (cap = 1), and the gap between
    /// attempts grows exponentially, bounded, in ladder ticks - the
    /// `telemetry::backoff_interval` shape on the ladder's own clock.
    fn tick(&mut self, present: bool, responsive: bool) -> bool {
        if !present || !responsive {
            self.responsive_ticks = 0;
            return false;
        }
        self.responsive_ticks += 1;
        if self.in_flight != 1 {
            return false;
        }
        let gap =
            (RECOVERY_PROBE_TICKS << self.attempts.min(15)).min(RECOVERY_ATTEMPT_GAP_CAP_TICKS);
        if self.responsive_ticks < gap {
            return false;
        }
        self.attempts += 1;
        self.responsive_ticks = 0;
        true
    }
}

/// Quarantine bookkeeping beside the circuit flag: the episode marker plus
/// the in-flight count, registered at most once per abandoned session so a
/// drain of timed-out calls on one owner counts one owner, not many.
fn owner_quarantined(abandoned: &AtomicBool) {
    OWNER_QUARANTINED.store(true, Ordering::Release);
    if !abandoned.swap(true, Ordering::AcqRel) {
        CIRCUIT_RECOVERY.lock().unwrap().quarantined();
    }
}

/// A completed native call is device health: the first one through an open
/// circuit closes the circuit, resets the episode, and writes the one
/// recovery audit line. Steady health on a closed circuit is silent.
fn circuit_closed_on_health() {
    if CIRCUIT_OPEN.swap(false, Ordering::AcqRel) {
        OWNER_QUARANTINED.store(false, Ordering::Release);
        CIRCUIT_RECOVERY.lock().unwrap().recovered();
        crate::log_info!("{CIRCUIT_RECOVERY_AUDIT}");
    }
}

/// The fresh-owner resolver for a quarantine episode (F-REC-1), called by
/// the ladder's `open_physical` at the head of its circuit-open refusal, so
/// the refusal row shape and the 1/min cadence stay verbatim. One tick per
/// pass: presence via the existing LCore identity discovery, responsiveness
/// via the existing RC-3 bounded read-only probe (LCore is never touched),
/// then the machine's authorization. Returns Some only when a fresh owner
/// session opened; its first successful call closes the circuit, and the
/// worker's existing `Connected`/`PhysicalHealthy` publications are the
/// recovery publish.
pub fn try_circuit_recovery(friendly_name: &str) -> Option<SdkDevice> {
    if !CIRCUIT_OPEN.load(Ordering::Acquire)
        || !OWNER_QUARANTINED.load(Ordering::Acquire)
        || recovery_blocked()
    {
        return None;
    }
    let owner = match lcore() {
        Ok(Some(owner)) => owner,
        _ => {
            CIRCUIT_RECOVERY.lock().unwrap().tick(false, false);
            return None;
        }
    };
    let responsive = classify_lcore(true, probe_aliveness(&mut NativeAliveness, owner.pid))
        == LcoreClass::Responsive;
    if !CIRCUIT_RECOVERY.lock().unwrap().tick(true, responsive) {
        return None;
    }
    open_session(&owner, friendly_name, true).ok()
}

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

/// Presence-only ownership check. Full path and SDK trust checks still run on open.
/// This preserves ambiguity/failure handling without allocating a 64 KiB path
/// buffer or canonicalizing an executable on every button-poll iteration.
pub fn lcore_present() -> Result<bool, String> {
    lcore_pid().map(|pid| pid.is_some())
}

fn lcore_name(units: &[u16]) -> bool {
    let expected = b"lcore.exe";
    units.len() == expected.len()
        && units
            .iter()
            .zip(expected)
            .all(|(&unit, expected)| unit <= 0x7f && (unit as u8).eq_ignore_ascii_case(expected))
}

fn lcore_pid() -> Result<Option<u32>, String> {
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
        let mut found = None;
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if lcore_name(&entry.szExeFile[..end]) && found.replace(entry.th32ProcessID).is_some() {
                let _ = CloseHandle(snapshot);
                return Err(
                    "multiple running LCore.exe processes make SDK selection ambiguous".into(),
                );
            }
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                let _ = CloseHandle(snapshot);
                return if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                    Ok(found)
                } else {
                    Err(format!("enumerate LCore.exe processes: {error}"))
                };
            }
        }
    }
}

pub fn lcore() -> Result<Option<LCore>, String> {
    let Some(pid) = lcore_pid()? else {
        return Ok(None);
    };
    unsafe {
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
    /// EC-2: on a successful submit the owner hands the payload buffer back to
    /// the session producer, so the same buffer is refilled per send. The
    /// buffer is free exactly at the reply: the owner serialized the native
    /// call and cannot read it any more.
    SubmitDone(Vec<u8>),
    Poll {
        connected: bool,
        buttons: [bool; 4],
    },
    Error(String),
}

struct Envelope {
    request: Request,
    reply: SyncSender<Reply>,
}

pub struct SdkDevice {
    requests: Option<SyncSender<Envelope>>,
    /// EC-2: the session-owned submit payload. A single producer (the backend
    /// worker thread) fills it and moves it into the request; the owner thread
    /// executes against the moved buffer and returns it, so there is NO
    /// cross-thread sharing: at every instant one thread owns the buffer, and
    /// the producer refills only after the release reply. A timed-out request
    /// keeps its buffer inside the envelope (quarantine rule intact); the next
    /// send starts from a fresh Vec (cold path).
    payload: Mutex<Option<Vec<u8>>>,
    /// F-REC-1: set once when this session's owner is abandoned by a
    /// timed-out call (quarantine), consumed by the owner thread's
    /// self-retire tail, so the in-flight count registers an owner exactly
    /// once and releases it on return.
    abandoned: Arc<AtomicBool>,
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
        open_session(owner, friendly_name, false)
    }
}

/// The open body behind the circuit gate and the owner claim.
/// `quarantined_owner_allowed` is the fresh-open resolver path
/// (F-REC-1): the claim slot is held by the episode's quarantined owner,
/// so the resolver takes the slot directly (slot exclusivity: exactly one
/// session owns it). The owner thread it displaces is unreturnable, so it is
/// DETACHED and counted in the machine's detached census, never in the
/// recovery window: the window keeps its ONE quarantined owner, so a
/// poisoning of the fresh owner re-arms instead of locking (F-1, WO-U45A-FIX
/// binding amendment). The trust pipeline, path-boundary, identity and
/// LCore-identity checks inside are the existing open path, unchanged.
fn open_session(
    owner: &LCore,
    friendly_name: &str,
    quarantined_owner_allowed: bool,
) -> Result<SdkDevice, LossError> {
    if quarantined_owner_allowed && SDK_OWNER_ACTIVE.swap(true, Ordering::AcqRel) {
        CIRCUIT_RECOVERY.lock().unwrap().displaced();
    }
    let expected_owner = owner.clone();
    let friendly_name = friendly_name.to_string();
    let abandoned = Arc::new(AtomicBool::new(false));
    let closure_abandoned = Arc::clone(&abandoned);
    let (requests, receiver) = mpsc::sync_channel(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let spawn = std::thread::Builder::new()
        .name("lcdsirplus-sdk-owner".into())
        .spawn(move || unsafe {
            let result = match lcore() {
                Ok(Some(current))
                    if current.pid == expected_owner.pid && current.path == expected_owner.path =>
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
            // Self-retire cleanly (WO-U45A): a thread that ever returns
            // self-retires before clearing the claim, and the machine routes
            // it - the window owner releases the ONE in-flight slot, a thread
            // with no slot to release leaves the detached census (F-1,
            // WO-U45A-FIX binding amendment: self-retire restores headroom).
            if closure_abandoned.swap(false, Ordering::AcqRel) {
                CIRCUIT_RECOVERY.lock().unwrap().retired();
            }
            // F-5 (WO-U45A-FIX): this store also clears a claim the resolver's fresh session now holds, because a returning detached owner keeps no slot of its own - the flag is advisory (the single backend worker is the only opener), and the window/detached censuses above are the exact accounting.
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
            let device = SdkDevice {
                requests: Some(requests),
                payload: Mutex::new(None),
                abandoned,
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
                            "Logitech SDK monochrome LCD did not connect within 5 seconds".into(),
                        ))
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) => {
            // The `wait_ready` timeout quarantines the owner thread that is
            // still running the trusted init: register the episode beside
            // its circuit store.
            OWNER_QUARANTINED.store(true, Ordering::Release);
            if !abandoned.swap(true, Ordering::AcqRel) {
                CIRCUIT_RECOVERY.lock().unwrap().quarantined();
            }
            Err(error)
        }
    }
}

impl SdkDevice {
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
                    owner_quarantined(&self.abandoned);
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
        match reply_rx.recv_timeout(timeout) {
            Ok(reply) => {
                // A Unit/Poll completion is a native call that returned: the
                // first one through an open circuit closes it (WO-U45A:
                // "first successful send/poll through the fresh owner closes
                // the circuit"). A semantic Error reply is not health.
                if !matches!(reply, Reply::Error(_)) {
                    circuit_closed_on_health();
                }
                Ok(reply)
            }
            Err(_) => {
                CIRCUIT_OPEN.store(true, Ordering::Release);
                owner_quarantined(&self.abandoned);
                Err(LossError::new(
                    LossCause::CallTimeout,
                    "Logitech SDK native call timed out; owner quarantined and circuit opened"
                        .into(),
                ))
            }
        }
    }

    /// EC-2: take the session payload buffer for one send. The caller clears
    /// first, then reserves, so a recycled buffer (len == capacity) never
    /// re-grows: the cold path (first send, or a send whose buffer was
    /// quarantined by a timeout) starts from a fresh Vec and reserves.
    fn take_payload(&self) -> Vec<u8> {
        self.payload.lock().unwrap().take().unwrap_or_default()
    }

    fn return_payload(&self, payload: Vec<u8>) {
        *self.payload.lock().unwrap() = Some(payload);
    }

    /// EC-2: the frame-boundary submit. The payload bytes are written into the
    /// session's reusable buffer (the same 0->0, nonzero->255 rule as
    /// `Frame::logitech_bytes`, straight into the reserved capacity), sent, and
    /// the buffer returns with the owner's release reply: steady sends allocate
    /// no payload.
    pub fn submit_frame(&self, frame: &crate::render::Frame) -> Result<(), LossError> {
        let mut payload = self.take_payload();
        payload.clear();
        payload.reserve(PIXELS);
        payload.extend(
            frame
                .pixels
                .iter()
                .map(|&value| if value != 0 { 255 } else { 0 }),
        );
        self.submit_payload(payload)
    }

    /// Vec-shaped public surface (the hardware-test transport calls it with an
    /// owned report): the bytes are copied into the reusable payload, so a
    /// steady send allocates no payload either.
    pub fn submit(&self, pixels: Vec<u8>) -> Result<(), LossError> {
        let mut payload = self.take_payload();
        payload.clear();
        payload.reserve(pixels.len());
        payload.extend_from_slice(&pixels);
        self.submit_payload(payload)
    }

    fn submit_payload(&self, payload: Vec<u8>) -> Result<(), LossError> {
        match self.call(Request::Submit(payload))? {
            Reply::Unit => Ok(()),
            Reply::SubmitDone(released) => {
                self.return_payload(released);
                Ok(())
            }
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
            Reply::Unit | Reply::SubmitDone(_) => Err(LossError::new(
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
            Reply::Poll { .. } | Reply::SubmitDone(_) => Err(LossError::new(
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
        let mut reply = runtime.execute(&envelope.request);
        if close {
            runtime.shutdown();
            shut_down = true;
            let _ = envelope.reply.send(reply);
            break;
        }
        if matches!(reply, Reply::Unit) {
            if let Request::Submit(payload) = envelope.request {
                // EC-2: the native call is complete; release the payload
                // buffer to the session producer with the reply.
                reply = Reply::SubmitDone(payload);
            }
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

    /// WO-QA-06 conversion of the Q-4 residue (WO-QA-04 R2 case H + the sibling sites
    /// `2015:9`/`2056:9`): `CIRCUIT_OPEN` and `SDK_OWNER_ACTIVE` are suite-global production
    /// statics; the WO-QA-06 trio below was the complete test-region writer census, and
    /// WO-U45A-REC-RECOVERY extended the circuit family (`OWNER_QUARANTINED`, `RECOVERY_BLOCKED`,
    /// `CIRCUIT_RECOVERY`) - every WO-U45A scenario that writes it takes `circuit_scenario_guard()`
    /// per this standing contract (R-1), so the trio's bodies stay byte-identical. libtest runs them
    /// on parallel threads, so a sibling's `store(false)` can land between production's
    /// `store(true)` (1381) and this family's `circuit_open()` reads - WO-QA-06 measured it
    /// live: 15% of 32-way-load windows, the registered site `1964:9` fired under load, 0% at
    /// suite density (chain 5/189). The guard serializes the three scenarios: while one runs,
    /// no sibling mutates the statics, so every read observes the value its own scenario
    /// produced. Assertions, the 1 ms bound and the driven gates stay verbatim.
    static CIRCUIT_SCENARIO_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The guard is taken at the scenario head and released when the test body returns. A
    /// sibling panic poisons the lock; that panic is already the suite's red, so the survivor
    /// proceeds on the inner value - loud, never a silent green.
    fn circuit_scenario_guard() -> std::sync::MutexGuard<'static, ()> {
        CIRCUIT_SCENARIO_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn unreturning_owner_opens_the_circuit_without_borrowed_data() {
        let _guard = circuit_scenario_guard();
        CIRCUIT_OPEN.store(false, Ordering::Release);
        let (requests, receiver) = mpsc::sync_channel(1);
        // WO-QA-04 driven time: the owner's stall is a gate, not wall clock.
        // The worker announces the moment it enters the native call, then
        // blocks until this test releases it, so the owner's reply is provably
        // absent for the whole 1 ms bound: a stretched 1 ms timer can no
        // longer expire after the reply lands (the recorded `sdk.rs:1942:14`
        // mechanism). The release is sent after the assertions, so the late
        // completion still lands after the circuit opened - the scenario's
        // point. The enqueue lives inside production `call_timeout`, so the
        // announcement is consumed here, right before the release.
        let (running_tx, running_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            owner_loop(
                Box::new(Unreturning {
                    inner: Slow,
                    running: running_tx,
                    release: release_rx,
                }),
                receiver,
            )
        });
        let mut device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        let error = device
            .call_timeout(Request::Submit(vec![0; PIXELS]), Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(error.cause, LossCause::CallTimeout);
        assert!(error.detail.contains("timed out") && circuit_open());
        // The release can only reach the gate once the owner is inside the
        // native call, so the bound's expiry is never a race.
        running_rx.recv_timeout(DRIVEN_HANDSHAKE_BUDGET).expect(
            "the gated owner is inside the native call, so the caller's bound cannot be beaten",
        );
        release_tx
            .try_send(())
            .expect("the gated owner accepted the release");
        device.requests.take();
        thread.join().unwrap();
        CIRCUIT_OPEN.store(false, Ordering::Release);
    }

    /// WO-QA-04: `Slow`'s 30 ms wall-clock stall made into a gate. The gated
    /// owner announces on entering the native call and hands back `Slow`'s
    /// reply only when the test releases it, so the caller's 1 ms
    /// `call_timeout` bound expires with the reply provably absent, in every
    /// scheduling interleaving. `Slow`'s behavior stays verbatim behind the
    /// gate, so the late completion lands after the circuit opened.
    struct Unreturning {
        inner: Slow,
        running: SyncSender<()>,
        release: Receiver<()>,
    }

    impl Runtime for Unreturning {
        fn execute(&mut self, request: &Request) -> Reply {
            if matches!(request, Request::Submit(_)) {
                self.running
                    .try_send(())
                    .expect("the gated owner announced itself before stalling");
                self.release
                    .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                    .expect("the gated owner was released by the test");
            }
            self.inner.execute(request)
        }
        fn shutdown(&mut self) {
            self.inner.shutdown()
        }
    }

    #[test]
    fn open_refused_while_the_circuit_is_open_names_circuit_open() {
        let _guard = circuit_scenario_guard();
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
        let _guard = circuit_scenario_guard();
        CIRCUIT_OPEN.store(false, Ordering::Release);
        SDK_OWNER_ACTIVE.store(false, Ordering::Release);
        claim_sdk_owner().unwrap();
        assert_eq!(
            claim_sdk_owner().err().unwrap().cause,
            LossCause::OwnerConflict
        );
        // WO-QA-02 driven time: the owner task is stalled on a gate, not on a
        // 20 ms wall-clock sleep. The recorded 32-way failure was the caller's
        // 1 ms `recv_timeout` expiring LATE, after the worker's 20 ms send had
        // already landed, so `wait_ready(..).err()` yielded `None`. Gated, the
        // worker completes only when this test releases it, so nothing can be in
        // the channel at the bound: the bound's expiry is no longer a race.
        let (ready, receiver) = mpsc::sync_channel(1);
        let (running_tx, running_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let owned = String::from("owned validation data");
            running_tx
                .try_send(())
                .expect("the gated owner announced itself before stalling");
            release_rx
                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                .expect("the gated owner was released by the test");
            assert_eq!(owned, "owned validation data");
            let _ = ready.send(Ok(()));
            SDK_OWNER_ACTIVE.store(false, Ordering::Release);
        });
        running_rx
            .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
            .expect("the gated owner is running, so the caller's bound cannot be beaten");
        let error = wait_ready(&receiver, Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(error.cause, LossCause::CallTimeout);
        assert!(circuit_open() && claim_sdk_owner().is_err());
        // The late completion lands after the circuit opened: the overlap is closed.
        release_tx
            .try_send(())
            .expect("the gated owner accepted the release");
        thread.join().unwrap();
        CIRCUIT_OPEN.store(false, Ordering::Release);
    }

    // EC-2 seams: the payload boundary is measured with the same counting
    // allocator as the queue boundary (crate::backends::alloc_count), and the
    // scripted owner records (payload pointer, lit pixel index, single-lit)
    // per execute, so buffer reuse and exclusivity are observed, not assumed.
    use std::sync::atomic::AtomicUsize;

    /// WO-QA-02: a liveness budget on the driven handshakes only — it panics
    /// loudly if a handshake never lands. No assertion and no scenario property
    /// depends on it; the value mirrors the WO-QA-01 alloc-window 10 s budget.
    const DRIVEN_HANDSHAKE_BUDGET: Duration = Duration::from_secs(10);

    type PayloadObservation = (usize, usize, bool);

    struct Recording {
        observed: Arc<Mutex<Vec<PayloadObservation>>>,
        submits: Arc<AtomicUsize>,
        first_read_ms: u64,
        /// WO-QA-01: only the instance the alloc-window test drives registers its
        /// owner thread with the counting harness, so a parallel SDK test's owner
        /// thread can never join a foreign measurement window.
        measured: bool,
        /// WO-QA-02 driven-time seam: when installed, the owner's first `Submit`
        /// execute announces its read window on `start` and holds that window open
        /// until the producer signals that the second send's fill has landed, so
        /// the exclusivity scenario is an event chain with no wall-clock window to
        /// lose. `None` keeps the shipped `first_read_ms` window untouched.
        read_window: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    }

    impl Recording {
        fn new(first_read_ms: u64) -> Self {
            Self {
                observed: Arc::new(Mutex::new(Vec::new())),
                submits: Arc::new(AtomicUsize::new(0)),
                first_read_ms,
                measured: false,
                read_window: None,
            }
        }

        /// The SDK payload alloc-window driver: registers its owner thread.
        fn measured(first_read_ms: u64) -> Self {
            Self {
                measured: true,
                ..Self::new(first_read_ms)
            }
        }

        /// WO-QA-02: the driven read-window instance for the payload-exclusivity
        /// scenario (the same scripted owner, driven instead of timed).
        fn driven(start: mpsc::SyncSender<()>, landed: mpsc::Receiver<()>) -> Self {
            Self {
                read_window: Some((start, landed)),
                ..Self::new(0)
            }
        }
    }

    impl Runtime for Recording {
        fn execute(&mut self, request: &Request) -> Reply {
            // Register the owner thread with the counting harness (first call),
            // so the measured window covers exactly producer + owner. WO-QA-01 gates
            // this to the measured instance (see the field doc).
            if self.measured {
                crate::backends::alloc_count::watch_thread();
            }
            match request {
                Request::Submit(frame) => {
                    let first_submit = self.submits.fetch_add(1, Ordering::SeqCst) == 0;
                    if first_submit {
                        if let Some((start, landed)) = &self.read_window {
                            // WO-QA-02 driven read window: announce that the owner is
                            // reading this payload, then hold the window open until the
                            // producer's second fill has landed inside it.
                            start
                                .try_send(())
                                .expect("the driven read window announced the owner's read");
                            landed
                                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                                .expect("the driven read window was closed by the producer's fill");
                        }
                    }
                    if first_submit && self.first_read_ms > 0 {
                        // The scripted "owner is reading this buffer" window.
                        std::thread::sleep(Duration::from_millis(self.first_read_ms));
                    }
                    let lit = frame.iter().position(|&b| b == 255).unwrap_or(usize::MAX);
                    let single = frame.iter().filter(|b| **b == 255).count() == 1;
                    self.observed
                        .lock()
                        .unwrap()
                        .push((frame.as_ptr() as usize, lit, single));
                    Reply::Unit
                }
                Request::Poll => Reply::Poll {
                    connected: true,
                    buttons: [false; 4],
                },
                Request::Close { .. } => Reply::Unit,
            }
        }
        fn shutdown(&mut self) {}
    }

    fn lit_frame(index: usize) -> crate::render::Frame {
        let mut frame = crate::render::Frame::new();
        frame.pixels[index] = 1;
        frame
    }

    /// WO-QA-02 driven-time scenario for the payload-exclusivity boundary. Same
    /// scripted owner, same payload protocol, no wall-clock term: the owner's read
    /// window opens on its first `Submit` execute (announce) and closes only once
    /// the second send's fill has provably landed inside it. The proof of "landed"
    /// is the relay thread's per-envelope signal, so the ordering is an event
    /// chain: forward #1 (frame 0 enqueued) -> owner announces its read window ->
    /// frame 1 filled + enqueued -> forward #2 -> window closed. Returns the
    /// per-execute observations plus the submit counter, so the caller's
    /// assertions read measured events, not a timer.
    fn drive_payload_exclusivity() -> (Vec<PayloadObservation>, Arc<AtomicUsize>) {
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let (landed_tx, landed_rx) = mpsc::sync_channel(1);
        let recorder = Recording::driven(start_tx, landed_rx);
        let observed = Arc::clone(&recorder.observed);
        let submits = Arc::clone(&recorder.submits);
        let (dev_tx, relay_rx) = mpsc::sync_channel(1);
        let (owner_tx, owner_rx) = mpsc::sync_channel(1);
        let (forwarded_tx, forwarded_rx) = mpsc::sync_channel(2);
        let relay = std::thread::spawn(move || {
            while let Ok(envelope) = relay_rx.recv() {
                let _ = forwarded_tx.try_send(());
                if owner_tx.send(envelope).is_err() {
                    break;
                }
            }
        });
        let owner = std::thread::spawn(move || owner_loop(Box::new(recorder), owner_rx));
        let mut device = SdkDevice {
            requests: Some(dev_tx),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = device.submit_frame(&lit_frame(0));
            });
            start_rx
                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                .expect("the driven read window opened on the owner's first read");
            forwarded_rx
                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                .expect("the first send's envelope reached the owner");
            scope.spawn(|| {
                let _ = device.submit_frame(&lit_frame(1));
            });
            forwarded_rx
                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                .expect("the second send's fill landed inside the driven read window");
            landed_tx
                .try_send(())
                .expect("the driven read window was closed by the producer");
        });
        drop(device.requests.take().unwrap());
        relay.join().unwrap();
        owner.join().unwrap();
        let events = observed.lock().unwrap().clone();
        (events, submits)
    }

    #[test]
    fn sdk_payload_buffer_is_reused_per_send_after_warm_up() {
        // Acceptance (1) at the SDK owner boundary: after the first send has
        // created the session payload, steady sends allocate zero 6880-byte
        // pixel buffers and the owner observes one stable payload pointer
        // (measured reuse). The disclosed residual is the fixed per-send
        // metadata (the reply sync_channel node), counted, not assumed.
        let recorder = Recording::measured(0);
        let observed = Arc::clone(&recorder.observed);
        let submits = Arc::clone(&recorder.submits);
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || owner_loop(Box::new(recorder), rx));
        let mut device = SdkDevice {
            requests: Some(tx),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };

        // WO-QA-01 (F-1): arm the window on this test thread; the recording owner
        // joins it at its first execute, and the guard's drop releases the slots.
        let _window = crate::backends::alloc_count::window();
        crate::backends::alloc_count::watch_thread();
        crate::backends::alloc_count::reset();
        let mut warm = crate::backends::alloc_count::Counts::default();
        for index in 0..2 {
            let before = crate::backends::alloc_count::snapshot();
            device.submit_frame(&lit_frame(index)).unwrap();
            warm = crate::backends::alloc_count::subtract(
                crate::backends::alloc_count::snapshot(),
                before,
            );
            assert_eq!(warm.1, 0, "the SDK boundary allocates no HID key class");
        }
        for index in 2..12 {
            let before = crate::backends::alloc_count::snapshot();
            device.submit_frame(&lit_frame(index)).unwrap();
            let steady = crate::backends::alloc_count::subtract(
                crate::backends::alloc_count::snapshot(),
                before,
            );
            assert_eq!(
                steady.0, 0,
                "steady SDK send {index}: zero 6880-byte payload allocations, measured: {steady:?}"
            );
        }
        let observed = observed.lock().unwrap().clone();
        assert_eq!(observed.len(), 12);
        assert!(
            observed.iter().all(|entry| entry.2),
            "every execute saw exactly one lit pixel: the payload fill is intact"
        );
        let first_ptr = observed[0].0;
        assert_eq!(
            observed[1..]
                .iter()
                .filter(|entry| entry.0 == first_ptr)
                .count(),
            11,
            "the owner observed one stable payload pointer across sends: reuse, measured"
        );
        let lit_indices: Vec<usize> = observed.iter().map(|entry| entry.1).collect();
        assert_eq!(
            lit_indices,
            (0..12).collect::<Vec<_>>(),
            "content order matches send order"
        );
        println!(
            "WO-A1-02 EC-2 sdk-payload measured: warm2={warm:?} (per-send metadata counted), sends=12, distinct_ptrs={}",
            observed
                .iter()
                .map(|entry| entry.0)
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
        assert_eq!(submits.load(Ordering::SeqCst), 12);
        drop(device.requests.take().unwrap());
        thread.join().unwrap();
    }

    #[test]
    fn sdk_payload_exclusivity_no_fill_during_owner_read() {
        // Acceptance (3) shape at the SDK payload boundary (mirrors
        // owner_serializes_submit_poll_close_and_shutdown: scripted runtime +
        // counters): the owner reads frame 0's buffer for 20 ms; the producer
        // of frame 1 fills inside that window. Under the move-and-release
        // protocol the second send runs on its own buffer, so the first
        // observation stays 0 and the order is [0, 1]. Under a shared-buffer
        // protocol (the mutation probe) the second fill lands inside the
        // first read and the first observation flips to 1.
        // WO-QA-02: "for 20 ms" / "5 ms later" is driven, not timed — the window
        // is opened by the owner's first execute and closed when the second fill
        // provably lands inside it (see `drive_payload_exclusivity`), so the
        // scenario keeps its meaning with no wall-clock term to lose.
        let (observed, submits) = drive_payload_exclusivity();
        assert_eq!(observed.len(), 2);
        assert_eq!(
            submits.load(Ordering::SeqCst),
            2,
            "counters: the owner ran both executes"
        );
        assert_eq!(
            observed[0].1, 0,
            "the first execute read frame 0's content: the second send's fill never wrote a buffer the owner may read"
        );
        assert_eq!(
            observed[1].1, 1,
            "the second execute read frame 1's content"
        );
        assert!(observed[0].2 && observed[1].2, "no torn reads");
        println!(
            "WO-A1-02 EC-2 sdk exclusivity measured: payload ptrs = [0x{:x}, 0x{:x}] (distinct => no buffer shared during the read window)",
            observed[0].0, observed[1].0
        );
    }

    #[test]
    fn the_driven_time_payload_exclusivity_needs_no_real_time_headroom() {
        // WO-QA-02 proof test (the WO-QA-01 §(B) pattern): the driven SDK payload
        // timeline is a pure event chain — zero sleeps — so it completes far inside
        // 2 s of real time under any CPU oversubscription, and it measures the
        // driven read order, so it cannot go green on an empty run.
        let started = Instant::now();
        let (observed, submits) = drive_payload_exclusivity();
        let elapsed = started.elapsed();
        assert_eq!(observed.len(), 2, "both scripted executes ran");
        assert_eq!(
            observed[0].1, 0,
            "the owner's first read in driven time was frame 0's content"
        );
        assert_eq!(
            observed[1].1, 1,
            "the owner's second read in driven time was frame 1's content"
        );
        assert_eq!(
            submits.load(Ordering::SeqCst),
            2,
            "counters: the owner ran both executes"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the driven SDK payload timeline needed {elapsed:?} of real time; it must need no real-time headroom"
        );
    }
    #[test]
    fn lcore_presence_matching_is_exact_and_ascii_case_insensitive() {
        assert!(lcore_name(&"LCore.Exe".encode_utf16().collect::<Vec<_>>()));
        assert!(!lcore_name(
            &"LCore-helper.exe".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!lcore_name(
            &"LCore.exe ".encode_utf16().collect::<Vec<_>>()
        ));
    }

    // WO-U45A-REC-RECOVERY seams: the fresh-owner recovery machine is pure
    // (driven ticks; no clock, no real discovery, no probe of a live LCore),
    // and the guarded scenarios below take `circuit_scenario_guard()` per
    // the R-1 standing contract because they write the circuit statics.

    /// Scenario-normalizing reset of the circuit episode state. Guarded
    /// callers only: it writes the circuit family.
    fn circuit_episode_reset() {
        CIRCUIT_OPEN.store(false, Ordering::Release);
        OWNER_QUARANTINED.store(false, Ordering::Release);
        RECOVERY_BLOCKED.store(false, Ordering::Release);
        CIRCUIT_RECOVERY.lock().unwrap().recovered();
    }

    #[test]
    fn fresh_open_authorizes_after_three_responsive_present_ticks_and_caps_at_one() {
        // Design floor (1)+(2): N=3 consecutive device-present AND
        // LCore-responsive ticks authorize ONE fresh-open attempt, and the
        // quarantined-owner cap is 1: a second owner in flight closes the
        // window until one self-retires.
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        assert!(!machine.tick(true, true), "tick 1 of 3");
        assert!(!machine.tick(true, true), "tick 2 of 3");
        assert!(
            machine.tick(true, true),
            "the third consecutive tick authorizes the one fresh-open attempt"
        );
        // WO-U45A-FIX (audit F-1, binding amendment - per-in-flight
        // invariant): the attempt's own owner times out and quarantines too.
        // Slot exclusivity keeps the window at ONE quarantined owner (the
        // displaced thread is counted in the detached census, never in the
        // window), so the episode RE-ARMS: the grown gap (3 << 1 = 6) refuses
        // five healthy ticks and the sixth authorizes attempt 2.
        machine.quarantined();
        assert_eq!(
            machine.in_flight, 1,
            "no overlap: a second quarantine never stacks the window"
        );
        for tick in 0..5 {
            assert!(
                !machine.tick(true, true),
                "re-arm: pre-gap tick {tick} refuses, the gap is 3 << 1 = 6"
            );
        }
        assert!(
            machine.tick(true, true),
            "a second poisoning re-arms the window: attempt 2 fires on the sixth healthy tick"
        );
        // From the reset streak, attempt 3 needs gap 12: eleven refuse,
        // the twelfth authorizes.
        for tick in 0..11 {
            assert!(!machine.tick(true, true), "gap 12: tick {tick} refuses");
        }
        assert!(
            machine.tick(true, true),
            "the twelfth consecutive tick authorizes attempt 3"
        );
    }

    #[test]
    fn a_not_responsive_or_absent_tick_never_authorizes_and_clears_the_streak() {
        // Design floor (1) arms: probe-not-responsive => NO attempt;
        // device-absent => NO attempt. Both clear the streak, so the
        // attempt window needs N=3 unbroken healthy ticks.
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        assert!(!machine.tick(true, true));
        assert!(
            !machine.tick(true, false),
            "an unresponsive probe tick never authorizes"
        );
        assert!(!machine.tick(true, true), "the streak restarted at 1");
        assert!(
            !machine.tick(false, true),
            "a device-absent tick never authorizes"
        );
        assert!(!machine.tick(true, true));
        assert!(!machine.tick(true, true));
        assert!(
            machine.tick(true, true),
            "three unbroken ticks authorize after the misses"
        );
    }

    #[test]
    fn the_attempt_gap_is_exponential_bounded_and_never_faster_than_the_cadence() {
        // Design floor (2): attempts follow the provider-outcome backoff
        // shape (`telemetry::backoff_interval`: doubling, clamped, bounded)
        // on the ladder's own tick clock, so the span between attempts is
        // always >= N ladder passes >= the configured reconnect cadence.
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        let mut previous = 0usize;
        for attempt in 0..12usize {
            let gap = (RECOVERY_PROBE_TICKS << attempt).min(RECOVERY_ATTEMPT_GAP_CAP_TICKS);
            assert!(gap >= RECOVERY_PROBE_TICKS, "never faster than N ticks");
            assert!(gap <= RECOVERY_ATTEMPT_GAP_CAP_TICKS, "bounded");
            assert!(
                gap > previous || gap == RECOVERY_ATTEMPT_GAP_CAP_TICKS,
                "exponential, then flat at the cap"
            );
            previous = gap;
            for tick in 0..(gap - 1) {
                assert!(
                    !machine.tick(true, true),
                    "attempt {attempt}: pre-gap tick {tick} refuses"
                );
            }
            assert!(
                machine.tick(true, true),
                "attempt {attempt}: the gap tick authorizes"
            );
        }
    }

    #[test]
    fn recovered_resets_the_episode_to_the_shipped_shape() {
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        machine.quarantined();
        machine.tick(true, true);
        machine.recovered();
        assert_eq!(
            (
                machine.responsive_ticks,
                machine.attempts,
                machine.in_flight
            ),
            (0, 0, 0),
            "a closed circuit starts the next episode from N=3, attempt 0"
        );
        machine.quarantined();
        assert!(!machine.tick(true, true));
        assert!(!machine.tick(true, true));
        assert!(machine.tick(true, true));
    }

    #[test]
    fn the_first_successful_call_on_an_open_circuit_closes_it_and_resets_the_episode() {
        // Design floor (3): the first successful send/poll closes the
        // circuit, clears the episode marker and resets the machine; steady
        // health on a closed circuit stays silent (no second transition).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        CIRCUIT_RECOVERY.lock().unwrap().quarantined();
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        assert!(device.poll().is_ok(), "the scripted poll completes");
        assert!(
            !circuit_open(),
            "the first successful poll closed the circuit"
        );
        assert!(
            !OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker cleared with the circuit"
        );
        {
            let machine = CIRCUIT_RECOVERY.lock().unwrap();
            assert_eq!(
                (
                    machine.responsive_ticks,
                    machine.attempts,
                    machine.in_flight
                ),
                (0, 0, 0),
                "the recovery reset the machine to the shipped shape"
            );
        }
        assert!(device.poll().is_ok());
        assert!(
            !circuit_open(),
            "steady health never re-opens the transition"
        );
        drop(device);
        thread.join().unwrap();
        circuit_episode_reset();
    }

    #[test]
    fn an_error_reply_is_not_health_and_keeps_the_circuit_open() {
        // The send-path vs circuit-flag census (panel question): the send
        // path is NOT flag-gated - a returning owner's call lands. Only a
        // completion that is health (Unit/Poll/SubmitDone) closes the flag;
        // a semantic error reply keeps the episode open.
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let mut device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        // Fake::execute answers an empty-payload Submit with Reply::Error.
        let error = device
            .submit(Vec::new())
            .expect_err("the injected error rides back");
        assert_eq!(error.cause, LossCause::TransferIncomplete { code: 0 });
        assert!(
            circuit_open(),
            "a semantic error reply is not health: the circuit stays open"
        );
        device.close(true).expect("the scripted close completes");
        assert!(
            !circuit_open(),
            "the first Unit completion through the same owner closes the circuit"
        );
        thread.join().unwrap();
        circuit_episode_reset();
    }

    #[test]
    fn a_full_dispatch_window_quarantines_once_and_the_returning_owner_self_retires() {
        // WO-U45A floor (2): the in-flight count registers the abandoned
        // owner exactly once (a second timed-out call on the same session
        // adds zero), and the owner's self-retire tail releases the slot
        // when its call returns - cap = 1 per episode, detached,
        // retire-on-return.
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        let (requests, receiver) = mpsc::sync_channel(1);
        // A gated, non-returning Submit enters the owner (announced), a
        // second Poll fills the single slot behind it, so the next dispatch
        // sees a Full channel; the 1 ms bound fires the dispatch-window
        // quarantine site without any 3 s wall term.
        let (stall_reply_a, _stall_receive_a) = mpsc::sync_channel(1);
        let (stall_reply_b, _stall_receive_b) = mpsc::sync_channel(1);
        let (running_tx, running_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        requests
            .try_send(Envelope {
                request: Request::Submit(vec![0; PIXELS]),
                reply: stall_reply_a,
            })
            .expect("the channel starts empty");
        let thread = std::thread::spawn(move || {
            owner_loop(
                Box::new(Unreturning {
                    inner: Slow,
                    running: running_tx,
                    release: release_rx,
                }),
                receiver,
            )
        });
        running_rx
            .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
            .expect("the gated owner dequeued the Submit and announced itself");
        requests
            .try_send(Envelope {
                request: Request::Poll,
                reply: stall_reply_b,
            })
            .expect("the single slot fills behind the gated owner");
        let mut device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        let error = device
            .call_timeout(Request::Poll, Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(error.cause, LossCause::CircuitOpen, "{error}");
        assert!(
            error.detail.contains("request dispatch timed out"),
            "{error}"
        );
        assert!(
            circuit_open() && OWNER_QUARANTINED.load(Ordering::Acquire),
            "the full dispatch window quarantines the owner and opens the circuit"
        );
        // A second timed-out call on the same session must not count twice.
        let _ = device.call_timeout(Request::Poll, Duration::from_millis(1));
        assert_eq!(
            CIRCUIT_RECOVERY.lock().unwrap().in_flight,
            1,
            "cap: exactly one in-flight quarantined owner per abandoned session"
        );
        // The owner returns: release the gate, close through the drained
        // slot; the Unit close closes the circuit and the self-retire tail
        // releases the in-flight slot.
        release_tx
            .try_send(())
            .expect("the gated owner accepted the release");
        device
            .close(true)
            .expect("the drained owner answers the close");
        assert!(
            !circuit_open(),
            "the first successful call through the returning owner closed the circuit"
        );
        thread.join().unwrap();
        assert_eq!(
            CIRCUIT_RECOVERY.lock().unwrap().in_flight,
            0,
            "the quarantined owner self-retired cleanly"
        );
        circuit_episode_reset();
    }

    #[test]
    fn the_resolver_refuses_every_arm_outside_the_quarantine_episode() {
        // Design floor (4): the circuit-open refusal of opens stays while
        // the episode is unresolved - the resolver is the only way through
        // the gate, and every refused arm returns BEFORE any real
        // discovery/probe, so tests never touch the live LCore.
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        assert!(
            try_circuit_recovery("fixture").is_none(),
            "circuit closed: no episode"
        );
        CIRCUIT_OPEN.store(true, Ordering::Release);
        assert!(
            try_circuit_recovery("fixture").is_none(),
            "circuit open without an owner quarantine (trust/identity class): the refusal stays"
        );
        OWNER_QUARANTINED.store(true, Ordering::Release);
        RECOVERY_BLOCKED.store(true, Ordering::Release);
        assert!(
            try_circuit_recovery("fixture").is_none(),
            "a trust-blocked episode never fresh-opens"
        );
        RECOVERY_BLOCKED.store(false, Ordering::Release);
        OWNER_QUARANTINED.store(false, Ordering::Release);
        // Normal opens stay refused while the circuit is open.
        let owner = LCore {
            path: PathBuf::from(r"C:\fixture\Logitech Gaming Software\LCore.exe"),
            pid: 0,
        };
        assert_eq!(
            SdkDevice::open(&owner, "fixture").err().unwrap().cause,
            LossCause::CircuitOpen
        );
        let machine = CIRCUIT_RECOVERY.lock().unwrap();
        assert_eq!(
            (
                machine.responsive_ticks,
                machine.attempts,
                machine.in_flight
            ),
            (0, 0, 0),
            "the refused arms recorded no ticks and consumed no attempts"
        );
        drop(machine);
        circuit_episode_reset();
    }

    #[test]
    fn the_recovery_audit_line_is_one_emit_and_names_no_journal_event() {
        // Design floor (3)+(5): ONE recovery audit line; the journal event
        // vocabulary is untouched (labels ride the detail, the RC-3 rule),
        // and the line carries the existing `loss-cause=` detail convention.
        // The needle is assembled at runtime so this test's own text can
        // never match the census.
        let source = include_str!("./sdk.rs");
        let needle = ["log_info!(\"", "{CIRCUIT_RECOVERY_AUDIT}", "\")"].concat();
        assert_eq!(
            source.matches(needle.as_str()).count(),
            1,
            "exactly one recovery audit emit site in the file"
        );
        for event in [
            "healthy",
            "finished",
            "disconnected",
            "logitech-attempt",
            "logitech-result",
            "logitech-wait-timeout",
            "application-attempt",
            "application-launched",
            "application-resumed",
        ] {
            assert!(
                !CIRCUIT_RECOVERY_AUDIT.contains(event),
                "the audit line names no journal event: {event}"
            );
        }
        assert!(
            CIRCUIT_RECOVERY_AUDIT.contains("loss-cause=circuit-open"),
            "the audit rides the existing loss-cause detail convention"
        );
    }

    // WO-U45A-FIX (audit F-1, binding amendment) regression set: the
    // per-in-flight safety invariant - at most ONE quarantined owner in the
    // recovery window, displaced/detached owner threads counted apart, and
    // attempts bounded by the existing 3 << attempts / 48-tick backoff.

    #[test]
    fn a_second_unreturning_quarantine_rearms_the_resolver() {
        // (a) Re-arming after a SECOND poisoning, on per-in-flight
        // authorization (not any lifetime count): two unreturning owners keep
        // the window at one owner, so the N=3 trigger fires, and a third
        // poisoning re-arms again on the grown gap.
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        machine.quarantined();
        assert_eq!(
            machine.in_flight, 1,
            "authorization reads the window, and the window holds one owner"
        );
        assert!(!machine.tick(true, true), "tick 1 of 3");
        assert!(!machine.tick(true, true), "tick 2 of 3");
        assert!(
            machine.tick(true, true),
            "two poisonings, one window: the third healthy tick authorizes the fresh open"
        );
        machine.quarantined();
        for tick in 0..5 {
            assert!(
                !machine.tick(true, true),
                "third poisoning: pre-gap tick {tick} refuses, the gap is 3 << 1 = 6"
            );
        }
        assert!(
            machine.tick(true, true),
            "a third poisoning re-arms too: attempt 2 fires on the gap tick"
        );
        assert_eq!(machine.in_flight, 1, "never an overlap, at any poisoning");
    }

    #[test]
    fn repeated_healthy_ticks_rearm_at_the_grown_gap_and_never_before() {
        // (b) Repeated healthy-tick semantics: every authorization resets the
        // streak, so the next attempt waits the FULL grown gap (3 << attempts,
        // capped at 48 ticks) - no premature attempt, and that bounded shape
        // is what keeps a single episode's attempts bounded. Detached owners
        // are census-only: they never gate a tick.
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        machine.displaced();
        machine.displaced();
        for attempt in 0..6usize {
            let gap = (RECOVERY_PROBE_TICKS << attempt).min(RECOVERY_ATTEMPT_GAP_CAP_TICKS);
            for tick in 0..(gap - 1) {
                assert!(
                    !machine.tick(true, true),
                    "attempt {attempt}: pre-gap healthy tick {tick} of {gap} refuses"
                );
            }
            assert!(
                machine.tick(true, true),
                "attempt {attempt}: the {gap}th consecutive healthy tick authorizes"
            );
        }
        assert_eq!(machine.attempts, 6, "exactly one attempt per gap");
        assert_eq!(
            machine.in_flight, 1,
            "the window stayed one owner across all six gaps"
        );
        assert_eq!(
            machine.detached, 2,
            "detached owners never gate authorization, so the gaps ran"
        );
    }

    #[test]
    fn a_second_overlapping_quarantine_can_never_stack_the_window() {
        // (c) In-flight cap enforcement (safety property 1): the window
        // registers the ONE quarantined owner, so an overlapping quarantine
        // is structurally impossible; displaced owners live in a separate
        // census, and a self-retire routes to the census that holds the slot.
        let mut machine = CircuitRecovery::default();
        for round in 0..8 {
            machine.quarantined();
            assert_eq!(
                machine.in_flight, 1,
                "round {round}: a repeat quarantine never stacks the window"
            );
        }
        machine.displaced();
        machine.displaced();
        assert_eq!(
            (machine.in_flight, machine.detached),
            (1, 2),
            "displaced threads are tracked apart from the window"
        );
        machine.retired();
        assert_eq!(
            (machine.in_flight, machine.detached),
            (0, 2),
            "the window owner's self-retire releases the slot (headroom restored)"
        );
        machine.retired();
        assert_eq!(
            (machine.in_flight, machine.detached),
            (0, 1),
            "a thread with no slot retires out of the detached census, not the window"
        );
        assert!(
            !machine.tick(true, true),
            "an empty window is no episode: the resolver authorizes nothing"
        );
    }

    #[test]
    fn the_closed_episode_closes_cleanly_and_the_next_episode_is_rearmable() {
        // (d) End-to-end recovery behavior: the first successful call through
        // a healthy owner closes the circuit, clears the episode marker and
        // resets BOTH censuses, on the single audit line; the next quarantine
        // re-arms from the shipped shape, so recovery is not a one-shot.
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        {
            let mut machine = CIRCUIT_RECOVERY.lock().unwrap();
            machine.quarantined();
            machine.displaced();
        }
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        device.poll().expect("the scripted poll completes");
        assert!(!circuit_open(), "the first success closed the circuit");
        assert!(
            !OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker cleared with the circuit"
        );
        {
            let machine = CIRCUIT_RECOVERY.lock().unwrap();
            assert_eq!(
                (
                    machine.responsive_ticks,
                    machine.attempts,
                    machine.in_flight,
                    machine.detached
                ),
                (0, 0, 0, 0),
                "the closed episode resets the window AND the detached census"
            );
        }
        let needle = ["log_info!(\"", "{CIRCUIT_RECOVERY_AUDIT}", "\")"].concat();
        assert_eq!(
            include_str!("./sdk.rs").matches(needle.as_str()).count(),
            1,
            "the recovery audit stays exactly one emit site"
        );
        let mut machine = CircuitRecovery::default();
        machine.quarantined();
        assert!(!machine.tick(true, true), "next episode: tick 1 of 3");
        assert!(!machine.tick(true, true), "next episode: tick 2 of 3");
        assert!(
            machine.tick(true, true),
            "the next episode is re-armable at the shipped N=3 cadence"
        );
        drop(device);
        thread.join().unwrap();
        circuit_episode_reset();
    }
}
