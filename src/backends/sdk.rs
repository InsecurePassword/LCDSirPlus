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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// F-REC-3 (WO-U46A P1): the active-owner claim is a GENERATION slot, not a
/// boolean. `NO_OWNER` (0) means unclaimed; any other value is the generation
/// of the ONE authorized owner thread. Every claim transition is an identity
/// operation: a claim takes the slot for a fresh generation, the resolver's
/// swap takes it for the recovery generation (and registers the generation it
/// displaced), and the only writer that clears it is the identity-gated
/// `release_sdk_owner`, whose compare-exchange can never drop a newer owner's
/// claim. This replaces the frozen tree's unconditional tail clear
/// (`src/backends/sdk.rs:1519` on 42132a7, red-proven by
/// `a_stale_return_never_clears_the_active_owner_claim`).
static SDK_OWNER_ACTIVE: AtomicU64 = AtomicU64::new(NO_OWNER);
/// Monotonic owner-generation allocator. `next_owner_generation` is its only
/// writer; generations are never recycled (u64, and the app lives far shorter
/// than 2^64 owner sessions), so an identity registered today can never
/// collide with a future owner's.
static SDK_OWNER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// The empty identity: no claim, no window registration, no census entry.
const NO_OWNER: u64 = 0;
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
    window_generation: NO_OWNER,
    detached: 0,
    detached_generations: Vec::new(),
    bound_reported: false,
});

/// Consecutive device-present + LCore-responsive ticks required before the
/// first fresh-open attempt (N=3, config-free).
const RECOVERY_PROBE_TICKS: usize = 3;
/// Cap of the exponential attempt gap, in consecutive ticks: 16 × N, so a
/// permanently failing resolver stops attempting after 48 ladder passes
/// between attempts at any cadence.
const RECOVERY_ATTEMPT_GAP_CAP_TICKS: usize = 48;
/// F-REC-2 (WO-U46A P2): the resource bound on simultaneously RETAINED
/// detached owner threads. A detached owner is not free memory: it keeps its
/// worker thread (a 1 MiB default stack reservation), its half of the
/// `sync_channel` with the queued `Envelope` (a `PIXELS` = 6,880 B payload),
/// its `PinnedSdk` file handles (`dll` + `_lcore` + the pinned parent chain,
/// ~7 handles for the Logitech Gaming Software path), one reference on the
/// loaded `LogitechLcd.dll` module, and the native call frame it is blocked in
/// inside LCore (LCore keeps the `LogiLcdInit` session for it, so the thread
/// and everything it references live until the call returns, i.e. the
/// process's lifetime).
///
/// Budget: 8 retained owners add at most 8 threads to the 19-25-thread live
/// shape (instance 66676: 19 threads, 390 handles, priv 45-46 MB; the recorded
/// live shape band is 20-25 threads / 478 handles) = +~35% threads, 8 MiB of
/// stack reservation = ~17% of the live private set, ~56 handles = +~12%, and
/// ~56 KB of queued payloads. Reaching the bound takes at least 8 native
/// timeouts, each gated by the `3 << attempts` / 48-tick backoff, so it marks
/// a genuinely pathological device, not a transient: the 54984 incident class
/// (unbounded accumulation, process live until reboot) is what the frozen
/// saturating counter (`src/backends/sdk.rs:120`/`src/backends/sdk.rs:146` on
/// 42132a7) allowed.
///
/// SCOPE (WO-U46G item 2, audit finding #18): the census is PROCESS-WIDE, not
/// per-episode. `recovered` resets the episode shape and keeps the ledger, so
/// the tradeoff is explicit: an episode that strands owners permanently
/// consumes headroom for every later episode, and at the bound the
/// authorization stops process-wide until a retained owner actually returns.
/// That is the intended cost - a stranded owner is retained memory until the
/// process exits - and it is bounded, self-healing (every retirement restores
/// one slot) and never a lifetime lockout.
///
/// Below the bound recovery stays fully re-armable (the census releases as
/// owners return). At the bound the resolver FAILS CLOSED: no fresh open is
/// authorized, and exactly one `log_warn` records it (recorded, silent-to-
/// desktop - the desktop notification path is app.rs's own). No thread is ever
/// force-terminated and no memory a pending native call references is freed;
/// LCore is never touched and the app never restarts.
const DETACHED_OWNER_BOUND: usize = 8;

/// The one recovery audit line. The journal's event vocabulary
/// (`application-*`, `logitech-*`, `finished`) is untouched; the line rides
/// the existing `loss-cause=` detail convention (RC-1) with the
/// `circuit-open` label, mirroring the RC-3 label rule.
pub const CIRCUIT_RECOVERY_AUDIT: &str = "Logitech SDK circuit closed: the fresh owner completed a call after the unreturning-owner episode; loss-cause=circuit-open";

/// F-REC-2 (WO-U46A P2): the one-shot detached-owner bound diagnostic. It rides
/// the existing `log_warn` channel (recorded in the log; the desktop
/// notification path is app.rs's own, so this line is silent-to-desktop), keeps
/// the `loss-cause=` detail convention (RC-1), and names no journal event
/// vocabulary (the RC-3 label rule). The number in the text is the bound the
/// resolver actually enforces (pinned by
/// `the_detached_owner_bound_stops_authorizing_and_reports_once`).
const DETACHED_OWNER_BOUND_AUDIT: &str = "Logitech SDK recovery suspended: 8 detached owner threads are retained at the resource bound, so no fresh owner is authorized until one returns; no thread is terminated; loss-cause=circuit-open";

#[derive(Default)]
struct CircuitRecovery {
    /// Consecutive ticks that saw the device present AND LCore responsive.
    responsive_ticks: usize,
    /// Fresh-open attempts spent in this episode.
    attempts: usize,
    /// The recovery window: at most ONE quarantined owner (slot exclusivity,
    /// WO-U45A-FIX binding amendment). `quarantine` registers the slot by
    /// assignment, it never stacks, so a second poisoning of the fresh owner
    /// cannot lock the window the way the F-1 counter increment did.
    in_flight: usize,
    /// F-REC-3 (WO-U46A P1): the IDENTITY the window slot is registered to -
    /// the generation of the owner whose native call is unreturning. A return
    /// may release the window only when it carries this generation, so a
    /// displaced owner returning late can never clear the live owner's window
    /// (the frozen count-based `retired`, `src/backends/sdk.rs:142` on 42132a7,
    /// did exactly that).
    window_generation: u64,
    /// Owner threads the resolver displaced out of the slot (their calls are
    /// unreturnable, so they are detached). Tracked separately from the
    /// window: they never gate authorization below the bound, and a thread
    /// that ever returns self-retires out of this census, restoring headroom.
    /// F-REC-2 (WO-U46A P2): this census is also the resource bound
    /// (`DETACHED_OWNER_BOUND`) - at the cap, authorization fails closed.
    detached: usize,
    /// F-REC-3 (WO-U46A P1): the identity of each retained detached owner, so
    /// a returning thread releases exactly its own entry (`swap_remove` on a
    /// generation match) and never a newer owner's. Its length is the
    /// `detached` census: the bound gates the only site that appends
    /// (the resolver's displacement), so the census is bounded by
    /// `DETACHED_OWNER_BOUND` structurally.
    detached_generations: Vec<u64>,
    /// F-REC-2: latch for the ONE-shot bound diagnostic; set when the census
    /// reaches the bound, consumed by `take_bound_report`, cleared by
    /// `recovered` so the next episode that reaches the bound reports again.
    bound_reported: bool,
}

impl CircuitRecovery {
    /// One owner quarantined by a timed-out native call, registered by
    /// identity. Slot exclusivity: the window registers the ONE quarantined
    /// owner, so a repeat quarantine - the fresh owner's own call timing out -
    /// keeps the window live (F-1 repair, assignment-only) instead of stacking
    /// a permanent block, and it re-homes the window on the owner that is
    /// actually in flight.
    fn quarantine(&mut self, generation: u64) {
        self.in_flight = 1;
        self.window_generation = generation;
        self.responsive_ticks = 0;
    }

    /// The resolver took the slot away from a live owner: that thread is
    /// displaced/detached, counted apart from the window, under its own
    /// identity. Reaching the bound latches the one-shot diagnostic.
    fn detach(&mut self, generation: u64) {
        self.detached += 1;
        if generation != NO_OWNER {
            self.detached_generations.push(generation);
        }
        if self.detached >= DETACHED_OWNER_BOUND {
            self.bound_reported = true;
        }
    }

    /// F-REC-3 (WO-U46A P1): a returning thread retires ONLY the bookkeeping
    /// that carries its own generation - its window registration and/or its
    /// detached census entry. A generation that owns neither retires nothing
    /// (a stale return is a no-op on a newer owner's window, census and
    /// headroom), which is the exact repair of the frozen tree's count-based
    /// routing (F-REC-3).
    fn retire(&mut self, generation: u64) {
        if self.window_generation == generation {
            self.in_flight = 0;
            self.window_generation = NO_OWNER;
        }
        if let Some(index) = self
            .detached_generations
            .iter()
            .position(|&owner| owner == generation)
        {
            self.detached_generations.swap_remove(index);
            self.detached -= 1;
        }
    }

    /// F-REC-2: the census is at the detached-owner bound: no fresh open may be
    /// authorized (fail closed).
    fn bound_hit(&self) -> bool {
        self.detached >= DETACHED_OWNER_BOUND
    }

    /// Consume the one-shot bound report: true exactly once per reaching of the
    /// bound. Called outside the machine lock, so the log line is emitted with
    /// no lock held.
    fn take_bound_report(&mut self) -> bool {
        std::mem::replace(&mut self.bound_reported, false)
    }

    /// The episode is over: the EPISODE shape resets to the shipped baseline -
    /// window identity, streak, attempts and the report latch. WO-U46G item 2
    /// (audit finding #18): the detached-owner census is the process's resource
    /// ledger, not episode state, so it is KEPT. A displaced owner whose native
    /// call never returns is retained memory for the life of the process
    /// (thread, channel half, pinned handles, module reference, native frame),
    /// so it keeps consuming headroom in every later episode and leaves only
    /// through `retire(g)` on its own actual retirement
    /// (`src/backends/sdk.rs:233`). The frozen `*self = Self::default()` erased
    /// it on every close, so the bound counted a single episode only and the
    /// 54984-class accumulation it exists to stop was re-armed for free.
    fn recovered(&mut self) {
        let detached = self.detached;
        let detached_generations = std::mem::take(&mut self.detached_generations);
        *self = Self {
            detached,
            detached_generations,
            ..Default::default()
        };
    }

    /// One ladder tick. Returns true only on the tick that authorizes the
    /// one fresh-open attempt: the streak must be unbroken (`device-present`
    /// and responsive every tick; any miss restarts it), the recovery window
    /// may hold at most ONE quarantined owner (cap = 1), the detached-owner
    /// census must be under the bound (F-REC-2: at the cap the resolver fails
    /// closed), and the gap between attempts grows exponentially, bounded, in
    /// ladder ticks - the `telemetry::backoff_interval` shape on the ladder's
    /// own clock.
    ///
    /// WO-U46G item 1 (audit finding #1): the authorization is EPISODE-scoped,
    /// not window-scoped. The caller has already proven the episode
    /// (`CIRCUIT_OPEN` + `OWNER_QUARANTINED` + not `RECOVERY_BLOCKED`,
    /// `src/backends/sdk.rs:363`), so a window that is OCCUPIED (the owner's
    /// call has not returned) and a window that is EMPTY (the owner's late
    /// NON-health reply ran its self-retire tail, `:235`, while the circuit
    /// stayed open) are the same sick device: the shipped gate
    /// (`in_flight != 1`) made that second shape permanent - the tick never
    /// fired again, the resolver returned `None`, and the circuit-open refusal
    /// ran for every mode until the process restarted. The close rule is
    /// untouched: only a health round-trip from the current generation ends an
    /// episode (`circuit_closed_on_health`, `src/backends/sdk.rs:344`), so
    /// authorizing an empty window can never declare recovery on its own.
    fn tick(&mut self, present: bool, responsive: bool) -> bool {
        if !present || !responsive {
            self.responsive_ticks = 0;
            return false;
        }
        self.responsive_ticks += 1;
        if self.in_flight > 1 || self.bound_hit() {
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

    // The identity-free seams of the frozen 45a scenario set (WO-U46A): the
    // 45a tests drive the anonymous shape, where the window is "the one
    // registered owner" and the census is a count. They exist for tests only,
    // so production is always identity-bound.

    #[cfg(test)]
    fn quarantined(&mut self) {
        self.quarantine(NO_OWNER);
    }

    #[cfg(test)]
    fn displaced(&mut self) {
        self.detach(NO_OWNER);
    }

    #[cfg(test)]
    fn retired(&mut self) {
        if self.in_flight == 1 {
            self.in_flight = 0;
            self.window_generation = NO_OWNER;
        } else {
            self.detached = self.detached.saturating_sub(1);
        }
    }
}

/// The next owner generation: monotonic and never `NO_OWNER`.
fn next_owner_generation() -> u64 {
    SDK_OWNER_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Quarantine bookkeeping beside the circuit flag: the episode marker plus the
/// in-flight window registration, registered at most once per abandoned
/// session (a drain of timed-out calls on one owner counts one owner, not
/// many) and under that owner's OWN generation, so the window is homed on the
/// session that is actually in flight (F-REC-3, WO-U46A P1).
fn owner_quarantined(abandoned: &AtomicBool, generation: u64) {
    OWNER_QUARANTINED.store(true, Ordering::Release);
    if !abandoned.swap(true, Ordering::AcqRel) {
        CIRCUIT_RECOVERY.lock().unwrap().quarantine(generation);
    }
}

/// Verified device recovery: a health-publishing round-trip (a poll, or a
/// finished display update) completed by the CURRENT authorized owner
/// generation. The first such completion through an open circuit closes the
/// circuit, resets the episode, and writes the one recovery audit line. Steady
/// health on a closed circuit is silent. A completed native call by itself is
/// NOT health (WO-U46A P3): the caller's `is_device_health` gate excludes the
/// close/shutdown completion, semantic errors and a displaced owner's success.
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
/// session opened; its first health-publishing round-trip closes the circuit
/// (WO-U46A P3 - the close/shutdown completion is not health), and the
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
    let authorized = CIRCUIT_RECOVERY.lock().unwrap().tick(true, responsive);
    // F-REC-2 (WO-U46A P2): the bound diagnostic is ONE recorded line per
    // reaching of the bound, taken from the machine latch with no lock held,
    // on the ladder's own cadence (never per tick, never per attempt).
    if CIRCUIT_RECOVERY.lock().unwrap().take_bound_report() {
        crate::log_warn!("{DETACHED_OWNER_BOUND_AUDIT}");
    }
    if !authorized {
        return None;
    }
    open_session(&owner, friendly_name, next_owner_generation(), true).ok()
}

pub fn circuit_open() -> bool {
    CIRCUIT_OPEN.load(Ordering::Acquire)
}

pub fn recovery_blocked() -> bool {
    RECOVERY_BLOCKED.load(Ordering::Acquire)
}

/// Take the active-owner claim for a NEW generation (the normal open path).
/// The claim is a generation slot, so the returned identity is what every
/// later bookkeeping step - quarantine, displacement, retirement, release -
/// is bound to (F-REC-3, WO-U46A P1). One claim holder at a time: a second
/// opener is refused (the shape the single-opener gate in
/// `src/backends/mod.rs:718` relies on).
fn claim_sdk_owner() -> Result<u64, LossError> {
    let generation = next_owner_generation();
    SDK_OWNER_ACTIVE
        .compare_exchange(NO_OWNER, generation, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| generation)
        .map_err(|_| {
            LossError::new(
                LossCause::OwnerConflict,
                "Logitech SDK discovery/owner operation is already active".into(),
            )
        })
}

/// The resolver's claim (the fresh-open path): take the slot for the recovery
/// generation. The generation it displaces is registered in the detached
/// census BY IDENTITY, so only that owner's own return can release it
/// (F-REC-3). An empty slot displaces nobody: no census entry is invented.
fn claim_sdk_owner_for_recovery(generation: u64) {
    let displaced = SDK_OWNER_ACTIVE.swap(generation, Ordering::AcqRel);
    if displaced != NO_OWNER {
        CIRCUIT_RECOVERY.lock().unwrap().detach(displaced);
    }
}

/// The ONLY writer that clears the active-owner claim, and only for the
/// generation that holds it: a compare-exchange, so a displaced owner's return
/// can never drop a newer owner's claim (the frozen tree's unconditional
/// `store(false)` tail did). Returns whether this generation released the slot.
fn release_sdk_owner(generation: u64) -> bool {
    SDK_OWNER_ACTIVE
        .compare_exchange(generation, NO_OWNER, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// The owner thread's self-retire tail (WO-U46A P1), shared by the spawned
/// owner and its tests: retire ONLY the bookkeeping this thread's generation
/// owns (window registration and/or detached census entry), then release the
/// claim if - and only if - this thread still holds it. A stale (displaced)
/// owner's return is a no-op on the live owner's window, census entry,
/// headroom and claim.
fn owner_self_retire(abandoned: &AtomicBool, generation: u64) {
    if abandoned.swap(false, Ordering::AcqRel) {
        CIRCUIT_RECOVERY.lock().unwrap().retire(generation);
    }
    release_sdk_owner(generation);
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
            // WO-U46G item 4 (audit finding #7): the whole mapping expression is
            // checked. The span add already used `checked_add`; the file-offset
            // expression (`raw + rva - virtual_address`) did not, so a hostile
            // `raw` near `u32::MAX` overflowed it (a panic under the debug
            // profile, a wrapped offset under release) and the wrapped value
            // then indexed `bytes[start..]` out of bounds. The offset is
            // computed in u64 and bounded by the file length, so an unmappable
            // RVA is a `None` and the caller's existing descriptive-Err `ok_or`
            // names it; no panic, no wrapped offset.
            let end = virtual_address.checked_add(span)?;
            if rva < virtual_address || rva >= end {
                return None;
            }
            let offset = (raw as u64) + u64::from(rva - virtual_address);
            (offset < bytes.len() as u64).then_some(offset as usize)
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
        // WO-U46G item 4: the slice is bounded by `bytes.get`, so a start the
        // mapping placed at or past the file end is a rejection, never a panic.
        let end = bytes
            .get(start..)
            .ok_or("SDK export name is outside the file")?
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
    /// self-retire tail, so the window registers an owner exactly once and
    /// releases it on return.
    abandoned: Arc<AtomicBool>,
    /// F-REC-3 (WO-U46A P1): this session's owner IDENTITY - the generation it
    /// claimed the active-owner slot with. Every bookkeeping step of the
    /// session (quarantine registration, retirement, claim release) is bound to
    /// it, and the health gate (WO-U46A P3) reads it: only the generation that
    /// currently holds the claim can declare device recovery.
    generation: u64,
}

impl SdkDevice {
    pub fn open(owner: &LCore, friendly_name: &str) -> Result<Self, LossError> {
        if CIRCUIT_OPEN.load(Ordering::Acquire) {
            return Err(LossError::new(
                LossCause::CircuitOpen,
                "Logitech SDK circuit is open after an unreturning native owner".into(),
            ));
        }
        let generation = claim_sdk_owner()?;
        open_session(owner, friendly_name, generation, false)
    }
}

/// The open body behind the circuit gate and the owner claim. `generation` is
/// the OWNER IDENTITY this session runs under: the normal path brings the
/// generation `claim_sdk_owner` took; `quarantined_owner_allowed` is the
/// fresh-open resolver path (F-REC-1), which takes the slot for its own
/// generation and registers the owner it displaces in the detached census BY
/// IDENTITY (slot exclusivity: exactly one session owns the claim). A detached
/// owner is never in the recovery window: the window keeps its ONE quarantined
/// owner, so a poisoning of the fresh owner re-arms instead of locking (F-1,
/// WO-U45A-FIX binding amendment). The trust pipeline, path-boundary, identity
/// and LCore-identity checks inside are the existing open path, unchanged.
fn open_session(
    owner: &LCore,
    friendly_name: &str,
    generation: u64,
    quarantined_owner_allowed: bool,
) -> Result<SdkDevice, LossError> {
    if quarantined_owner_allowed {
        claim_sdk_owner_for_recovery(generation);
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
            // Self-retire cleanly (WO-U45A, identity-bound by WO-U46A P1): a
            // thread that ever returns self-retires BEFORE releasing the claim,
            // and the machine releases only the bookkeeping that carries THIS
            // thread's generation - its window registration and/or its own
            // detached census entry (F-1 amendment: self-retire restores
            // headroom). The frozen tree's unconditional claim clear (F-5,
            // `src/backends/sdk.rs:1519` on 42132a7) is gone: a returning
            // displaced owner can no longer drop the claim the resolver's fresh
            // session holds, so the single-owner claim invariant is structural
            // and not merely advisory.
            owner_self_retire(&closure_abandoned, generation);
        });
    if let Err(error) = spawn {
        release_sdk_owner(generation);
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
                generation,
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
            // its circuit store, under this session's generation.
            OWNER_QUARANTINED.store(true, Ordering::Release);
            if !abandoned.swap(true, Ordering::AcqRel) {
                CIRCUIT_RECOVERY.lock().unwrap().quarantine(generation);
            }
            Err(error)
        }
    }
}

impl SdkDevice {
    /// WO-U46A P3: the circuit-close semantics. Health is a DEVICE ROUND-TRIP -
    /// a poll (`Reply::Poll`: `LogiLcdUpdate`, `LogiLcdIsConnected` and
    /// `LogiLcdIsButtonPressed` all returned) or a finished display update
    /// (`Reply::SubmitDone`: `LogiLcdMonoSetBackground` + `LogiLcdUpdate`
    /// returned and the payload buffer was released) - completed by the owner
    /// that CURRENTLY holds the active-owner claim (`self.generation` equals
    /// the claim's generation, F-REC-3 identity). Excluded by construction:
    ///
    /// - `Reply::Unit`: the only Unit that reaches a session is the close/
    ///   shutdown completion (`src/backends/sdk.rs:1386` on 42132a7) - a
    ///   completed shutdown is not verified device recovery. A Submit's Unit is
    ///   `owner_loop`'s EC-2 conversion into `SubmitDone` before it lands.
    /// - `Reply::Error`: a semantic native failure, never health.
    /// - a displaced/stale owner's completion: its generation no longer owns
    ///   the claim, so its success is not the episode's recovery.
    ///
    /// A scripted session with no claim at all (generation `NO_OWNER` while the
    /// slot is empty) is the shipped test shape and stays authorized.
    fn is_device_health(&self, reply: &Reply) -> bool {
        match reply {
            Reply::Poll { .. } | Reply::SubmitDone(_) => {
                SDK_OWNER_ACTIVE.load(Ordering::Acquire) == self.generation
            }
            Reply::Unit | Reply::Error(_) => false,
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
                    owner_quarantined(&self.abandoned, self.generation);
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
                // WO-U46A P3: a completed native call is not automatically
                // device recovery. Only a health-publishing round-trip
                // completed by the CURRENT authorized owner closes the circuit
                // (WO-U45A's "first successful send/poll through the fresh
                // owner closes the circuit"). The close/shutdown `Reply::Unit`,
                // a semantic `Error`, and a displaced owner's completion are
                // not health, so they leave the episode, the window census and
                // the audit line untouched.
                if self.is_device_health(&reply) {
                    circuit_closed_on_health();
                }
                Ok(reply)
            }
            Err(_) => {
                CIRCUIT_OPEN.store(true, Ordering::Release);
                owner_quarantined(&self.abandoned, self.generation);
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

    /// Vec-shaped public surface (the hardware-test transport calls it with an
    /// owned report): the bytes are copied into the reusable payload, so a
    /// steady send allocates no payload either.
    pub fn submit(&self, pixels: Vec<u8>) -> Result<(), LossError> {
        self.submit_bytes(&pixels)
    }

    pub(super) fn submit_bytes(&self, pixels: &[u8]) -> Result<(), LossError> {
        let mut payload = self.take_payload();
        payload.clear();
        payload.reserve(pixels.len());
        payload.extend_from_slice(pixels);
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

    /// WO-U46K (the registered contention-flake family): the ONE test-region acquisition seam
    /// for the shared `CIRCUIT_RECOVERY` machine. Two properties, both load-bearing:
    ///
    /// * TOLERANCE: a poisoned shared mutex must never cascade across independent scenarios.
    ///   Every scenario that acquires it is guarded by `circuit_scenario_guard()` and
    ///   re-initializes the FULL machine shape through `circuit_episode_reset` first (all
    ///   seven fields: `recovered()` plus the declared `detached` / `detached_generations`
    ///   clears), so a tolerant lock cannot mask state corruption - the machine is
    ///   overwritten before it is read. No scenario uses poison itself as the tested signal.
    /// * NO SELF-POISONING: a `MutexGuard` dropped while a panic unwinds re-poisons the mutex,
    ///   and the PRODUCTION acquisition sites (the 8 rows of the 46a audit: `quarantine`,
    ///   `circuit_closed_on_health`, the three resolver ticks, `detach`, `retire`, the
    ///   `open_session` dispatch site) all `unwrap()`. So a test that asserts while holding
    ///   the guard aborts every later scenario - in production code as well, which is the
    ///   registered ~13-row cascade. Callers therefore take a SNAPSHOT, let the guard drop at
    ///   the end of the block, and assert on the snapshot: the same fields, read at the same
    ///   program point, under the same scenario guard.
    fn recovery_lock() -> std::sync::MutexGuard<'static, CircuitRecovery> {
        CIRCUIT_RECOVERY
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
            generation: NO_OWNER,
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
        SDK_OWNER_ACTIVE.store(NO_OWNER, Ordering::Release);
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
            SDK_OWNER_ACTIVE.store(NO_OWNER, Ordering::Release);
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
            generation: NO_OWNER,
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = device.submit_bytes(&lit_frame(0).logitech_bytes());
            });
            start_rx
                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                .expect("the driven read window opened on the owner's first read");
            forwarded_rx
                .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
                .expect("the first send's envelope reached the owner");
            scope.spawn(|| {
                let _ = device.submit_bytes(&lit_frame(1).logitech_bytes());
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
    fn changed_sdk_submit_reuses_comparison_key_and_session_payload() {
        use crate::backends::{alloc_count, PhysicalDevice};
        let _guard = circuit_scenario_guard();
        let recorder = Recording::new(0);
        let observed = Arc::clone(&recorder.observed);
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || owner_loop(Box::new(recorder), rx));
        let mut transport = PhysicalDevice::Sdk(SdkDevice {
            requests: Some(tx),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: NO_OWNER,
        });

        let mut counts = [0; 2];
        let mut keys = [0; 3];
        let mut retained = [0; 3];
        for index in [0, 1, 2] {
            let mut input = lit_frame(index);
            input.pixels[index] = [1, 17, 255][index];
            let compared = input.logitech_bytes();
            keys[index] = compared.as_ptr() as usize;
            let (sent, additional) = {
                let _window = alloc_count::window();
                let sent = transport.submit(&input, compared).unwrap();
                (sent, alloc_count::snapshot().0)
            };
            retained[index] = sent.as_ptr() as usize;
            if index != 0 {
                counts[index - 1] = additional;
            }
            let PhysicalDevice::Sdk(device) = &transport else {
                unreachable!()
            };
            let payload = device.payload.lock().unwrap();
            assert_eq!(payload.as_deref(), Some(sent.as_slice()));
            assert_eq!(sent.len(), PIXELS);
            assert_eq!(sent[index], 255);
            assert!(sent
                .iter()
                .enumerate()
                .all(|(i, &value)| i == index || value == 0));
        }

        let PhysicalDevice::Sdk(device) = &mut transport else {
            unreachable!()
        };
        drop(device.requests.take().unwrap());
        worker.join().unwrap();
        let observations = observed.lock().unwrap();
        assert_eq!(observations.len(), 3);
        let ptr = observations[0].0;
        for (index, &(payload_ptr, lit, single)) in observations.iter().enumerate() {
            assert_eq!((payload_ptr, lit, single), (ptr, index, true));
            assert_ne!(payload_ptr, keys[index], "keep a separate session payload");
        }
        assert_eq!(counts, [0, 0], "no new 6880-byte buffer after comparison");
        assert_eq!(retained, keys, "retain the same already-compared Vec");
    }

    #[test]
    fn sdk_payload_buffer_is_reused_per_send_after_warm_up() {
        // Acceptance (1) at the SDK owner boundary: after the first send has
        // created the session payload, steady sends allocate zero 6880-byte
        // pixel buffers and the owner observes one stable payload pointer
        // (measured reuse). The disclosed residual is the fixed per-send
        // metadata (the reply sync_channel node), counted, not assumed.
        // WO-U46K (R-1 standing contract, extended to the complete writer census): this
        // scenario drives production `SdkDevice` calls, and production `call_timeout` writes
        // the guarded circuit family from them - `owner_quarantined` on a timeout and
        // `circuit_closed_on_health` on a health reply (the 46a-audited production sites).
        // Unguarded, its health reply ran `recovered()` on a shared machine a concurrent
        // circuit scenario had registered an owner in - the registered primary red.
        let _guard = circuit_scenario_guard();
        let recorder = Recording::measured(0);
        let observed = Arc::clone(&recorder.observed);
        let submits = Arc::clone(&recorder.submits);
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || owner_loop(Box::new(recorder), rx));
        let mut device = SdkDevice {
            requests: Some(tx),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: NO_OWNER,
        };

        // WO-QA-01 (F-1): arm the window on this test thread; the recording owner
        // joins it at its first execute, and the guard's drop releases the slots.
        let _window = crate::backends::alloc_count::window();
        crate::backends::alloc_count::watch_thread();
        crate::backends::alloc_count::reset();
        let mut warm = crate::backends::alloc_count::Counts::default();
        for index in 0..2 {
            let key = lit_frame(index).logitech_bytes();
            let before = crate::backends::alloc_count::snapshot();
            device.submit_bytes(&key).unwrap();
            warm = crate::backends::alloc_count::subtract(
                crate::backends::alloc_count::snapshot(),
                before,
            );
            assert_eq!(warm.1, 0, "the SDK boundary allocates no HID key class");
        }
        for index in 2..12 {
            let key = lit_frame(index).logitech_bytes();
            let before = crate::backends::alloc_count::snapshot();
            device.submit_bytes(&key).unwrap();
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
        // WO-U46K (R-1, as in `sdk_payload_buffer_is_reused_per_send_after_warm_up`): the
        // driven sends are production `SdkDevice` calls, so the circuit family is written
        // through the production sites and the scenario serializes with the circuit set.
        let _guard = circuit_scenario_guard();
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
        // WO-U46K (R-1, as in the two sibling EC-2 scenarios): the driven sends are
        // production `SdkDevice` calls, so this scenario serializes with the circuit set.
        // The guard is taken HERE, above the timer, so the 2 s bound below measures the
        // driven chain alone and never the queue behind a circuit scenario.
        let _guard = circuit_scenario_guard();
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
    /// callers only: it writes the circuit family - WO-U46A P1 extends the
    /// family to the owner-claim slot, so a guarded scenario starts from
    /// "no owner claimed" and a `generation: NO_OWNER` scripted owner is the
    /// authorized owner of its own scenario.
    fn circuit_episode_reset() {
        CIRCUIT_OPEN.store(false, Ordering::Release);
        OWNER_QUARANTINED.store(false, Ordering::Release);
        RECOVERY_BLOCKED.store(false, Ordering::Release);
        SDK_OWNER_ACTIVE.store(NO_OWNER, Ordering::Release);
        let mut machine = recovery_lock();
        machine.recovered();
        // WO-U46G item 2 (declared adaptation): `recovered()` now RETAINS the
        // process-wide detached census by design, so the scenario-normalizing
        // reset clears the census explicitly to keep every 45a/46a scenario
        // isolated at a full-headroom start.
        machine.detached = 0;
        machine.detached_generations.clear();
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
        machine.displaced();
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
        // WO-U46G item 2 (the single permitted 45a semantic change): the EPISODE
        // shape resets - window identity, streak, attempts, latch - and the
        // detached census does NOT, because it is the process's resource
        // ledger: a retained owner leaves only through `retire(g)` on its own
        // retirement (`src/backends/sdk.rs:233`).
        assert_eq!(
            machine.detached, 1,
            "the census is not part of the episode shape: the retained owner stays counted"
        );
        machine.retired();
        assert_eq!(
            machine.detached, 0,
            "the retained owner's retirement is what restores the headroom"
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
        recovery_lock().quarantined();
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: NO_OWNER,
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
        let machine = {
            let state = recovery_lock();
            (state.responsive_ticks, state.attempts, state.in_flight)
        };
        assert_eq!(
            machine,
            (0, 0, 0),
            "the recovery reset the machine to the shipped shape"
        );
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
        // completion that is health (POLL/SUBMITDONE from the current owner;
        // WO-U46A P3 removed the close-only `Unit` from that set) closes the
        // flag; a semantic error reply keeps the episode open.
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: NO_OWNER,
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
        // WO-U46A P3 (legitimate 45a extension): this test's closing step was
        // `close(true)` + "the first Unit completion closes the circuit", which
        // asserted exactly the close-only-completion semantics P3 corrects. The
        // health-publishing round-trip takes its place; the error-not-health
        // property above is unchanged.
        device.poll().expect("the scripted poll completes");
        assert!(
            !circuit_open(),
            "the first health-publishing completion through the same owner closes the circuit"
        );
        drop(device);
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
            generation: NO_OWNER,
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
        let machine = {
            let state = recovery_lock();
            state.in_flight
        };
        assert_eq!(
            machine, 1,
            "cap: exactly one in-flight quarantined owner per abandoned session"
        );
        // The owner returns: release the gate, drain the queued slot, close.
        // WO-U46A P3 (legitimate 45a extension): the frozen test asserted
        // "the first successful call through the returning owner closed the
        // circuit" on that drained `close(true)` completion, which is the
        // close-only `Reply::Unit` - P3 rules a completed shutdown out of the
        // health set, so the episode now provably survives it and recovery
        // lands on the fresh owner's health round-trip below.
        release_tx
            .try_send(())
            .expect("the gated owner accepted the release");
        device
            .close(true)
            .expect("the drained owner answers the close");
        assert!(
            circuit_open(),
            "the close-only completion through the returning owner is not health: the episode stays live"
        );
        thread.join().unwrap();
        let (fresh_requests, fresh_receiver) = mpsc::sync_channel(1);
        let fresh_calls = Arc::new(Mutex::new(Vec::new()));
        let fresh_thread =
            std::thread::spawn(move || owner_loop(Box::new(Fake(fresh_calls)), fresh_receiver));
        let fresh_device = SdkDevice {
            requests: Some(fresh_requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: NO_OWNER,
        };
        fresh_device
            .poll()
            .expect("the fresh owner's health round-trip lands");
        assert!(
            !circuit_open(),
            "the first health-publishing call through the fresh owner closes the circuit"
        );
        drop(fresh_device);
        fresh_thread.join().unwrap();
        let machine = {
            let state = recovery_lock();
            state.in_flight
        };
        assert_eq!(machine, 0, "the quarantined owner self-retired cleanly");
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
        let machine = {
            let state = recovery_lock();
            (state.responsive_ticks, state.attempts, state.in_flight)
        };
        assert_eq!(
            machine,
            (0, 0, 0),
            "the refused arms recorded no ticks and consumed no attempts"
        );
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
            let mut machine = recovery_lock();
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
            generation: NO_OWNER,
        };
        device.poll().expect("the scripted poll completes");
        assert!(!circuit_open(), "the first success closed the circuit");
        assert!(
            !OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker cleared with the circuit"
        );
        let machine = {
            let state = recovery_lock();
            (
                state.responsive_ticks,
                state.attempts,
                state.in_flight,
                state.detached,
            )
        };
        // WO-U46G item 2 (declared adaptation): the 45a expectation was
        // `(0, 0, 0, 0)` - the frozen `recovered()` erased the census.
        // The closed episode resets the window and the episode shape;
        // the ONE displaced owner this scenario registered stays in the
        // process-wide census until it retires.
        assert_eq!(
            machine,
            (0, 0, 0, 1),
            "the closed episode resets the window; the process-wide census keeps the retained owner"
        );
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

    // WO-U46A-OWNER-GENERATION regression set (Step 0: applicability proof).
    //
    // Every test in this block was FIRST run red against the frozen tree
    // 42132a7 in the frozen API shape - the count-based `CircuitRecovery`
    // seams, the boolean `SDK_OWNER_ACTIVE` claim, the 3-field `SdkDevice`
    // literal, and statement-for-statement mirrors of the indicted production
    // sites - so each red row is the defect and never a compile error. The
    // reds are recorded in `u46a-prefix-reds.log` (per-name `--exact` runs,
    // `RED_EXIT[..]=101`). The final forms below bind the same scenarios to
    // the repaired identity seams (`quarantine`/`detach`/`retire` by
    // generation, `claim_sdk_owner_for_recovery`, `release_sdk_owner`,
    // `owner_self_retire`, the generation-bearing `SdkDevice`), keeping each
    // scenario, name and assertion.
    //
    // Indicted frozen sites: the boolean claim (`src/backends/sdk.rs:74`), the
    // count-based `retired` routing (`src/backends/sdk.rs:142`), the saturating
    // detached census (`src/backends/sdk.rs:120`), the resolver's claim swap +
    // displacement (`src/backends/sdk.rs:1473`), the self-retire tail and its
    // unconditional claim clear (`src/backends/sdk.rs:1515`,
    // `src/backends/sdk.rs:1519`), the health rule that accepts any non-Error
    // completion (`src/backends/sdk.rs:1612`), and the close-only
    // `Reply::Unit` completion (`src/backends/sdk.rs:1386`).

    /// WO-U46A P2: the storm scale the 54984 incident witnessed (unbounded
    /// detached owners, the process lives until reboot). The bound tests drive
    /// the census through the counting seams; no test spawns thousands of real
    /// threads (the real-thread tests below use at most 2 each, disclosed).
    const DETACHED_OWNER_STORM: usize = 16;

    #[test]
    fn a_returning_owner_leaves_a_healthy_replacement_in_full_possession() {
        // P1 permutation 1: A poisons, the resolver displaces A and B opens
        // healthy, then A's late native call returns. A's self-retire tail
        // retires ONLY A's own bookkeeping; B's window, census and claim
        // survive, so the single-opener gate keeps exactly one active claim.
        // FROZEN RED: `sdk.rs:3279` `(0, 1) != (0, 0)` - the count-based retire
        // released the window and left A's census entry, and the mirrored tail
        // cleared B's claim (`RED_EXIT=101`).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        let a_generation = claim_sdk_owner().expect("A opens as the authorized owner");
        // A poisons: the window registers A's own identity.
        recovery_lock().quarantine(a_generation);
        // The resolver displaces A: the swap registers A's generation in the
        // detached census and installs B as the claim holder.
        let b_generation = next_owner_generation();
        claim_sdk_owner_for_recovery(b_generation);
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            b_generation,
            "the replacement owner B holds the claim"
        );
        // A's thread returns late, through the production self-retire tail.
        let a_abandoned = Arc::new(AtomicBool::new(true));
        let (running_tx, running_rx) = mpsc::sync_channel(1);
        let thread = {
            let a_abandoned = Arc::clone(&a_abandoned);
            std::thread::spawn(move || {
                owner_self_retire(&a_abandoned, a_generation);
                running_tx
                    .try_send(())
                    .expect("the returning owner announced its tail");
            })
        };
        running_rx
            .recv_timeout(DRIVEN_HANDSHAKE_BUDGET)
            .expect("the returning owner ran its tail");
        thread.join().unwrap();
        let machine = {
            let state = recovery_lock();
            (
                state.in_flight,
                state.window_generation,
                state.detached,
                state.detached_generations.is_empty(),
            )
        };
        assert_eq!(
            (machine.0, machine.1, machine.2),
            (0, NO_OWNER, 0),
            "A's return releases BOTH of A's registrations and nothing of B's"
        );
        assert!(machine.3, "the census holds no ghost of a retired owner");
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            b_generation,
            "a stale return can never clear the live replacement owner's claim"
        );
        assert_eq!(
            claim_sdk_owner().err().unwrap().cause,
            LossCause::OwnerConflict,
            "exactly one active claim survives a stale return (the single-opener gate)"
        );
        assert!(
            release_sdk_owner(b_generation),
            "the claim's own generation releases it"
        );
    }

    #[test]
    fn a_returning_displaced_owner_cannot_clear_the_live_owners_window() {
        // P1 permutation 2: A returns while B is quarantined. A's retirement
        // cannot clear B's window registration, and B's episode stays bounded
        // and re-armable on the existing gap.
        // FROZEN RED: `sdk.rs:3307` `0 != 1` - A's `retired()` took the
        // `in_flight == 1` arm and cleared B's window (F-REC-3).
        let mut machine = CircuitRecovery::default();
        let a_generation = next_owner_generation();
        let b_generation = next_owner_generation();
        machine.quarantine(a_generation); // A poisons: the window registers A
        machine.detach(a_generation); // A is displaced into the detached census
        machine.quarantine(b_generation); // B's own poisoning re-homes the window
        machine.retire(a_generation); // A's late return
        assert_eq!(
            (machine.in_flight, machine.window_generation),
            (1, b_generation),
            "a stale return cannot clear the live owner B's window registration"
        );
        assert_eq!(
            (machine.detached, machine.detached_generations.len()),
            (0, 0),
            "A's retirement released A's own census entry (headroom restored)"
        );
        assert!(!machine.tick(true, true), "B's episode: tick 1 of 3");
        assert!(!machine.tick(true, true), "B's episode: tick 2 of 3");
        assert!(
            machine.tick(true, true),
            "B's episode stays bounded and re-armable after a stale return"
        );
    }

    #[test]
    fn a_stale_return_cannot_consume_a_newer_owner_s_census_entry() {
        // P1 permutation 3: B recovers before A returns, a next episode
        // registers C, and only then does A come back. A retires nothing: C's
        // window and C's census entry both survive, and the resolver keeps
        // authorizing for C's episode.
        // FROZEN RED: `sdk.rs:3336` `(0, 1) != (1, 1)` - A's retirement ate
        // C's window registration.
        let mut machine = CircuitRecovery::default();
        let a_generation = next_owner_generation();
        let c_generation = next_owner_generation();
        machine.quarantine(a_generation); // A poisons
        machine.detach(a_generation); // A displaced by B
        machine.recovered(); // B's health round-trip closed the episode
        machine.quarantine(c_generation); // C poisons in the next episode
        machine.detach(c_generation); // C displaced by D
        machine.retire(a_generation); // A's late return
        assert_eq!(
            (
                machine.in_flight,
                machine.window_generation,
                machine.detached
            ),
            (1, c_generation, 1),
            "a stale return retires no window and no census entry it does not own"
        );
        assert_eq!(
            machine.detached_generations,
            vec![c_generation],
            "the census keeps exactly the owner that is still retained"
        );
        assert!(!machine.tick(true, true), "C's episode: tick 1 of 3");
        assert!(!machine.tick(true, true), "C's episode: tick 2 of 3");
        assert!(
            machine.tick(true, true),
            "the episode the stale return landed in keeps its authorization"
        );
    }

    #[test]
    fn a_stale_return_never_clears_the_active_owner_claim() {
        // P1: a retirement whose identity does not own the claim is a NO-OP on
        // the claim (stale retirement = no-op on a newer claim), and it retires
        // only its own census entry.
        // FROZEN RED: `sdk.rs:3359` - the frozen tail's unconditional claim
        // clear dropped B's claim, so a second opener was accepted.
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        let b_generation = claim_sdk_owner().expect("the replacement owner B claims the slot");
        let a_generation = next_owner_generation(); // A: a displaced identity
        {
            let mut machine = recovery_lock();
            machine.quarantine(b_generation);
            machine.detach(a_generation); // A is the retained detached owner
        }
        // A returns through the production self-retire tail.
        let a_abandoned = Arc::new(AtomicBool::new(true));
        owner_self_retire(&a_abandoned, a_generation);
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            b_generation,
            "the claim of the current generation owner survives a stale owner's return"
        );
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation, state.detached)
        };
        assert_eq!(
            (machine.0, machine.1),
            (1, b_generation),
            "a stale retirement is a no-op on the live window registration"
        );
        assert_eq!(
            machine.2, 0,
            "the stale retirement released only A's own census entry"
        );
        assert!(
            !release_sdk_owner(a_generation),
            "the identity-gated release refuses a generation that does not own the claim"
        );
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            b_generation,
            "a refused release leaves the claim untouched"
        );
        assert!(
            release_sdk_owner(b_generation),
            "the owning generation releases its own claim"
        );
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            NO_OWNER,
            "the slot is empty again"
        );
    }

    #[test]
    fn the_active_owner_claim_has_exactly_one_generation_gated_writer() {
        // P1 structural census: the claim has NO unconditional writer in
        // production - exactly two identity-gated compare-exchanges (claim,
        // release) and one swap (the resolver's displacement) - and the runtime
        // census keeps exactly one claim holder at a time.
        // FROZEN RED: `sdk.rs:3373` `10 != 0` - the frozen tree wrote the
        // claim unconditionally (production tail + spawn rollback, plus the
        // scenario sites), and its CAS count was 1, not 2.
        let source = include_str!("./sdk.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("the test module marker");
        // rustfmt wraps method chains at the column boundary, so the writer
        // census glues every `.`-prefixed continuation back onto its receiver:
        // each claim transition counts as one token sequence, wrapped or not.
        let flat: String = production
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" .", ".");
        assert_eq!(
            flat.matches("SDK_OWNER_ACTIVE.store(").count(),
            0,
            "no production site writes the owner claim unconditionally"
        );
        assert_eq!(
            flat.matches("SDK_OWNER_ACTIVE.compare_exchange").count(),
            2,
            "the claim has exactly two writers: the identity-gated claim and release"
        );
        assert_eq!(
            flat.matches("SDK_OWNER_ACTIVE.swap(").count(),
            1,
            "the claim has exactly one taker: the resolver's displacement swap"
        );
        assert_eq!(
            flat.matches("SDK_OWNER_ACTIVE.load(").count(),
            1,
            "the claim has exactly one reader: the health gate"
        );
        assert!(
            production.contains("fn release_sdk_owner"),
            "the claim clear lives in the identity-gated release"
        );
        assert!(
            production.contains("fn owner_self_retire"),
            "the self-retire tail is one shared production function"
        );
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        let first = claim_sdk_owner().expect("the slot is free");
        assert_ne!(
            first, NO_OWNER,
            "a claim carries a real generation, never the empty identity"
        );
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            first,
            "the claim is held by exactly that generation"
        );
        assert_eq!(
            claim_sdk_owner().err().unwrap().cause,
            LossCause::OwnerConflict,
            "a second owner never shares the claim"
        );
        let second = next_owner_generation();
        claim_sdk_owner_for_recovery(second);
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            second,
            "the resolver's swap installed the recovery generation as the one holder"
        );
        assert!(
            !release_sdk_owner(first),
            "the displaced generation cannot clear the new claim"
        );
        assert!(
            release_sdk_owner(second),
            "the holder releases its own claim"
        );
    }

    #[test]
    fn repeated_quarantine_cycles_never_stack_the_window_and_the_census_tracks() {
        // P1 census preservation across repeated quarantine/replacement cycles:
        // the window stays assignment-only (never stacks, always homed on the
        // owner in flight) and the detached census tracks the retained owners
        // exactly. Pre-red baseline: green on the frozen tree (the 45a
        // amendment property, re-proven under identity binding).
        let mut machine = CircuitRecovery::default();
        for round in 0..8 {
            let generation = next_owner_generation();
            machine.quarantine(generation);
            assert_eq!(
                machine.in_flight, 1,
                "round {round}: a repeat quarantine never stacks the window"
            );
            assert_eq!(
                machine.window_generation, generation,
                "round {round}: the window is registered to the owner in flight"
            );
        }
        for round in 0..4 {
            machine.detach(next_owner_generation());
            assert_eq!(
                (machine.in_flight, machine.detached),
                (1, round + 1),
                "round {round}: displaced owners land in the separate census"
            );
        }
        assert!(
            !machine.tick(true, false),
            "an unresponsive tick never authorizes, at any census shape"
        );
        assert!(
            !machine.tick(true, true),
            "post-miss streak: tick 1 of 3 at any census shape under the bound"
        );
        assert!(
            !machine.tick(true, true),
            "post-miss streak: tick 2 of 3 at any census shape under the bound"
        );
        assert!(
            machine.tick(true, true),
            "the 45a amendment: authorization stays on the bounded N=3 gap"
        );
    }

    #[test]
    fn the_current_owner_s_successful_poll_closes_the_circuit() {
        // P3 case 1: a poll round-trip from the authorized owner IS verified
        // device health: it closes the circuit and clears the episode marker.
        // Pre-red baseline: green on the frozen tree (the property P3 keeps).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        let generation = claim_sdk_owner().expect("the owner holds the claim");
        recovery_lock().quarantine(generation);
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation,
        };
        device.poll().expect("the scripted poll round-trip lands");
        assert!(
            !circuit_open(),
            "a successful poll from the current owner closes the circuit"
        );
        assert!(
            !OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker cleared with the circuit"
        );
        drop(device);
        thread.join().unwrap();
        release_sdk_owner(generation);
        circuit_episode_reset();
    }

    #[test]
    fn the_current_owner_s_successful_display_update_closes_the_circuit() {
        // P3 case 2: a finished display update (`Reply::SubmitDone`: the native
        // background + update calls completed and the buffer was released) is a
        // health-publishing round-trip.
        // Pre-red baseline: green on the frozen tree (the property P3 keeps).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        let generation = claim_sdk_owner().expect("the owner holds the claim");
        recovery_lock().quarantine(generation);
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation,
        };
        device
            .submit(vec![7; PIXELS])
            .expect("the scripted display update round-trip lands");
        assert!(
            !circuit_open(),
            "a successful display update from the current owner closes the circuit"
        );
        drop(device);
        thread.join().unwrap();
        release_sdk_owner(generation);
        circuit_episode_reset();
    }

    #[test]
    fn a_close_only_unit_completion_is_not_health_and_never_closes_the_circuit() {
        // P3 case 3: the close/shutdown path completes with `Reply::Unit`
        // (`src/backends/sdk.rs:1386` on 42132a7). A completed shutdown is not
        // verified device recovery: the circuit, the episode marker and the
        // window census all stay as they were.
        // FROZEN RED: `sdk.rs:3513`, and the run emitted the recovery audit
        // line from the close-only completion (the defect, in the log).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        let generation = claim_sdk_owner().expect("the owner holds the claim");
        recovery_lock().quarantine(generation);
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let mut device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation,
        };
        device
            .close(true)
            .expect("the scripted close-only completion lands");
        thread.join().unwrap();
        assert!(
            circuit_open(),
            "a close-only Reply::Unit is a completed shutdown, not device health"
        );
        assert!(
            OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker survives a close-only completion"
        );
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation)
        };
        assert_eq!(
            machine,
            (1, generation),
            "the window census survives a close-only completion"
        );
        device.requests.take();
        // And real recovery still lands: a health round-trip closes the circuit.
        let (poll_requests, poll_receiver) = mpsc::sync_channel(1);
        let poll_calls = Arc::new(Mutex::new(Vec::new()));
        let poll_thread =
            std::thread::spawn(move || owner_loop(Box::new(Fake(poll_calls)), poll_receiver));
        let poll_device = SdkDevice {
            requests: Some(poll_requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation,
        };
        poll_device
            .poll()
            .expect("the scripted poll round-trip lands");
        assert!(
            !circuit_open(),
            "a health-publishing round-trip closes the circuit the close-only completion left open"
        );
        drop(poll_device);
        poll_thread.join().unwrap();
        release_sdk_owner(generation);
        circuit_episode_reset();
    }

    #[test]
    fn a_semantic_error_completion_is_not_health_and_keeps_the_circuit_open() {
        // P3 case 4: a semantic `Reply::Error` (the native call returned false)
        // is a completed call, not device health.
        // Pre-red baseline: green on the frozen tree (the property P3 keeps).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        let generation = claim_sdk_owner().expect("the owner holds the claim");
        recovery_lock().quarantine(generation);
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation,
        };
        let error = device
            .submit(Vec::new())
            .expect_err("the injected semantic error rides back");
        assert_eq!(error.cause, LossCause::TransferIncomplete { code: 0 });
        assert!(
            circuit_open(),
            "a semantic error completion keeps the circuit open"
        );
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation)
        };
        assert_eq!(
            machine,
            (1, generation),
            "the episode census survives a semantic error"
        );
        drop(device);
        thread.join().unwrap();
        release_sdk_owner(generation);
        circuit_episode_reset();
    }

    #[test]
    fn a_stale_owner_success_is_not_recovery_and_keeps_the_circuit_open() {
        // P3 case 5: a DISPLACED owner's successful call is not the episode's
        // recovery. B holds the claim (the resolver's swap,
        // `src/backends/sdk.rs:1473` on 42132a7); A's session completes a poll;
        // the circuit, the episode marker, B's window and A's own census entry
        // all survive it. Recovery then lands on B's own health round-trip.
        // FROZEN RED: `sdk.rs:3608`, with the recovery audit line emitted for
        // the stale owner's poll (the defect, in the log).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        let stale_generation = next_owner_generation(); // A: displaced
        let fresh_generation = claim_sdk_owner().expect("the replacement owner B holds the claim");
        {
            let mut machine = recovery_lock();
            machine.quarantine(stale_generation);
            machine.detach(stale_generation);
        }
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(true)), // A is the quarantined owner
            generation: stale_generation,
        };
        device
            .poll()
            .expect("the displaced owner's call completes (the send path is not flag-gated)");
        assert!(
            circuit_open(),
            "a stale owner's success never declares recovery"
        );
        assert!(
            OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker survives a stale owner's success"
        );
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation, state.detached)
        };
        assert_eq!(
            machine,
            (1, stale_generation, 1),
            "the window registration and the census entry survive a stale success"
        );
        drop(device);
        thread.join().unwrap();
        // Actual recovery: the CURRENT generation owner's health round-trip.
        let (fresh_requests, fresh_receiver) = mpsc::sync_channel(1);
        let fresh_calls = Arc::new(Mutex::new(Vec::new()));
        let fresh_thread =
            std::thread::spawn(move || owner_loop(Box::new(Fake(fresh_calls)), fresh_receiver));
        let fresh_device = SdkDevice {
            requests: Some(fresh_requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: fresh_generation,
        };
        fresh_device
            .poll()
            .expect("the fresh owner's health round-trip lands");
        assert!(
            !circuit_open(),
            "the current generation owner's success closes the circuit the stale owner's success left open"
        );
        drop(fresh_device);
        fresh_thread.join().unwrap();
        release_sdk_owner(fresh_generation);
        circuit_episode_reset();
    }

    #[test]
    fn a_fresh_owner_success_closes_the_circuit_on_one_audit_line_and_clears_the_episode() {
        // P3 case 6: actual recovery - the CURRENT (fresh) owner's success
        // closes the circuit, clears the episode marker and resets BOTH
        // censuses, on the single audit line.
        // Pre-red baseline: green on the frozen tree (the recovery shape P3 keeps).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        CIRCUIT_OPEN.store(true, Ordering::Release);
        OWNER_QUARANTINED.store(true, Ordering::Release);
        let stale_generation = next_owner_generation();
        let fresh_generation = claim_sdk_owner().expect("the fresh owner B holds the claim");
        {
            let mut machine = recovery_lock();
            machine.quarantine(stale_generation);
            machine.detach(stale_generation);
        }
        let (requests, receiver) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::spawn(move || owner_loop(Box::new(Fake(calls)), receiver));
        let device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: fresh_generation,
        };
        device
            .poll()
            .expect("the fresh owner's poll round-trip lands");
        assert!(
            !circuit_open(),
            "the fresh owner's success closes the circuit"
        );
        assert!(
            !OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker cleared"
        );
        let machine = {
            let state = recovery_lock();
            (
                state.responsive_ticks,
                state.attempts,
                state.in_flight,
                state.detached,
                state.detached_generations.len(),
                state.bound_hit(),
            )
        };
        // WO-U46G item 2 (declared adaptation): the 46a expectation was
        // `(0, 0, 0, 0, 0)` - the frozen `recovered()` erased the
        // census. The displaced owner this scenario registered is still
        // retained (its native call never returned), so it stays
        // identified in the process-wide ledger after the close.
        assert_eq!(
            (machine.0, machine.1, machine.2, machine.3, machine.4),
            (0, 0, 0, 1, 1),
            "the closed episode resets the window; the census keeps the owner that is still retained"
        );
        assert!(
            !machine.5,
            "one retained owner is under the bound: the next episode is armable"
        );
        let needle = ["log_info!(\"", "{CIRCUIT_RECOVERY_AUDIT}", "\")"].concat();
        assert_eq!(
            include_str!("./sdk.rs").matches(needle.as_str()).count(),
            1,
            "the recovery audit stays exactly one emit site"
        );
        drop(device);
        thread.join().unwrap();
        release_sdk_owner(fresh_generation);
        circuit_episode_reset();
    }

    #[test]
    fn the_detached_owner_bound_stops_authorizing_and_reports_once() {
        // P2: the detached census is a resource bound, not a saturating
        // counter. At the bound the resolver FAILS CLOSED: no fresh open is
        // authorized at any gap, no attempt is consumed, and the diagnostic is
        // ONE recorded line (a `log_warn`, silent-to-desktop).
        // FROZEN RED: `sdk.rs:3690` "tick 2: at the bound no fresh open is
        // authorized" - the frozen `tick` ignored the census and authorized
        // with 16 retained owner threads.
        let mut machine = CircuitRecovery::default();
        machine.quarantine(next_owner_generation());
        assert!(
            !machine.bound_hit(),
            "an empty census is under the bound: recovery is armable"
        );
        for owner in 0..DETACHED_OWNER_STORM {
            machine.detach(next_owner_generation());
            assert_eq!(
                machine.detached,
                owner + 1,
                "the census counts the retained owners"
            );
        }
        assert_eq!(
            machine.detached_generations.len(),
            DETACHED_OWNER_STORM,
            "every retained owner is identified: the census IS the bound's state"
        );
        assert!(machine.bound_hit(), "the census is at the bound");
        for tick in 0..(DETACHED_OWNER_STORM * 4) {
            assert!(
                !machine.tick(true, true),
                "tick {tick}: at the bound no fresh open is authorized (fail closed)"
            );
        }
        assert_eq!(
            machine.attempts, 0,
            "the bound consumed no attempts: authorization stopped before the gap"
        );
        assert!(
            machine.take_bound_report(),
            "reaching the bound is recorded exactly once"
        );
        assert!(
            !machine.take_bound_report(),
            "ONE log_warn: the latch stays consumed, never per tick"
        );
        // The bound's documented budget and the recorded line agree, and the
        // line follows the journal-label rules (RC-1/RC-3 conventions).
        assert!(
            DETACHED_OWNER_BOUND < machine.detached,
            "the incident storm scale exceeds the enforced bound, so the bound is what stopped it"
        );
        assert!(
            DETACHED_OWNER_BOUND_AUDIT.contains(&format!(
                "{DETACHED_OWNER_BOUND} detached owner threads are retained"
            )),
            "the recorded line names the bound the resolver enforces"
        );
        assert!(
            DETACHED_OWNER_BOUND_AUDIT.contains("loss-cause=circuit-open"),
            "the bound line rides the existing loss-cause detail convention"
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
                !DETACHED_OWNER_BOUND_AUDIT.contains(event),
                "the bound line names no journal event: {event}"
            );
        }
        let needle = ["log_warn!(\"", "{DETACHED_OWNER_BOUND_AUDIT}", "\")"].concat();
        assert_eq!(
            include_str!("./sdk.rs")
                .split("#[cfg(test)]\nmod tests {")
                .next()
                .expect("the test module marker")
                .matches(needle.as_str())
                .count(),
            1,
            "exactly one bound diagnostic emit site in production"
        );
        // The bound never touches the recovery machinery that stays live: the
        // window keeps its one owner, and clearing the census re-arms.
        assert_eq!(machine.in_flight, 1, "the window keeps exactly one owner");
    }

    #[test]
    fn the_detached_owner_bound_restores_headroom_as_owners_return() {
        // P2: below the bound recovery stays FULLY re-armable, and the census
        // accounts exactly the owners that are still retained: a returning
        // owner releases exactly its own entry.
        // FROZEN RED: `sdk.rs:3712` `16 != 15` - the frozen `retired()` took
        // the window arm first, so the returning owners released no census
        // entry at all (and the storm's window died).
        let mut machine = CircuitRecovery::default();
        let episode_generation = next_owner_generation();
        machine.quarantine(episode_generation);
        let retained: Vec<u64> = (0..DETACHED_OWNER_STORM)
            .map(|_| next_owner_generation())
            .collect();
        for generation in &retained {
            machine.detach(*generation);
        }
        assert!(machine.bound_hit(), "the storm is at the bound");
        for (index, generation) in retained
            .iter()
            .take(DETACHED_OWNER_STORM / 2 + 1)
            .enumerate()
        {
            machine.retire(*generation); // half of them, plus one, return
            assert_eq!(
                machine.detached,
                DETACHED_OWNER_STORM - index - 1,
                "a returning owner releases exactly its own census entry"
            );
        }
        assert!(
            !machine.bound_hit(),
            "the census is under the bound as owners return"
        );
        assert_eq!(
            (machine.in_flight, machine.window_generation),
            (1, episode_generation),
            "the episode owner's window survived the storm and the returns"
        );
        assert!(!machine.tick(true, true), "under the bound: tick 1 of 3");
        assert!(!machine.tick(true, true), "under the bound: tick 2 of 3");
        assert!(
            machine.tick(true, true),
            "under the bound the resolver authorizes again (fully re-armable)"
        );
        for generation in &retained[DETACHED_OWNER_STORM / 2 + 1..] {
            machine.retire(*generation);
        }
        machine.retire(episode_generation);
        assert_eq!(
            (
                machine.in_flight,
                machine.detached,
                machine.detached_generations.len()
            ),
            (0, 0, 0),
            "the closed episode leaves no census ghost, so the next episode starts at full headroom"
        );
    }

    // WO-U46G-RECOVERY-RECLAIM regression set (ponytail audit findings #1, #6,
    // #7, #18; user handoff 2026-11-20 sections 4 and 5).
    //
    // Step 0 applicability on the pre-fix tree 12ff06d, re-found on disk and
    // quoted row-for-row in `WO-U46G-RECEIPT.md` section 0:
    //   * `tick` `src/backends/sdk.rs:281` - `if self.in_flight != 1 ||
    //     self.bound_hit()` authorizes ONLY an OCCUPIED window;
    //   * `retire` `src/backends/sdk.rs:233-246` - `:235` zeroes the window when
    //     the registered owner itself returns;
    //   * `circuit_closed_on_health` `src/backends/sdk.rs:344-350` - the close
    //     needs a health round-trip from the CURRENT generation;
    //   * the resolver arms `src/backends/sdk.rs:363-367` + the `!authorized`
    //     return at `src/backends/sdk.rs:385`;
    //   * the refusal `src/backends/mod.rs:736`, entered by EVERY mode once
    //     `src/backends/mod.rs:718` is true, so the normal arbitration at
    //     `src/backends/mod.rs:741` is unreachable while the circuit is open.
    // The fixed point: CIRCUIT_OPEN + OWNER_QUARANTINED + an EMPTY window
    // (in_flight 0) => `tick` false => the resolver returns None => the
    // circuit-open refusal for every mode, forever, panel blank until the
    // process restarts. The shipped test
    // `a_full_dispatch_window_quarantines_once_and_the_returning_owner_self_retires`
    // (`src/backends/sdk.rs:3133`) lands in exactly that state and escapes only
    // through a hand-built session literal (`src/backends/sdk.rs:3223`); no
    // shipped test ever drove the ladder OUT of it. Every test below drives
    // production functions only, and the scenario set takes
    // `circuit_scenario_guard()` per the R-1 standing contract (the wedge
    // scenario uses 2 real threads, disclosed).

    #[test]
    fn an_open_episode_with_an_empty_window_authorizes_the_next_ladder_tick() {
        // Audit #1, the wedge authorization rule: an OPEN, UNBLOCKED episode
        // whose window is EMPTY (the quarantined owner retired; the device is
        // still sick) authorizes on the next ladder tick at the grown gap,
        // exactly like an occupied window. Pre-fix RED: the `in_flight != 1`
        // gate at `src/backends/sdk.rs:281` made the tick never fire again, so
        // the episode was a permanent refusal.
        let mut machine = CircuitRecovery::default();
        let owner = next_owner_generation();
        machine.quarantine(owner);
        assert!(!machine.tick(true, true), "episode tick 1 of the gap 3");
        assert!(!machine.tick(true, true), "episode tick 2 of the gap 3");
        // The owner's call returns with a NON-health reply, so the episode
        // survives, and its self-retire tail empties the window (:235).
        machine.retire(owner);
        assert_eq!(
            (machine.in_flight, machine.window_generation),
            (0, NO_OWNER),
            "the wedged shape: the circuit is open, the episode marker is set, the window is EMPTY"
        );
        assert!(
            machine.tick(true, true),
            "the third consecutive ladder tick authorizes the fresh open with an empty window (pre-fix: the wedge - the tick never fires again)"
        );
    }

    #[test]
    fn the_wedge_is_rescued_by_the_next_ladder_tick_on_a_real_owner_sequence() {
        // Audit #1 end to end, on the production sequence only: a real owner
        // thread poisons inside a native call (the dispatch-window quarantine
        // site), its late NON-health reply keeps the episode live, its
        // self-retire tail empties the window, and the NEXT ladder tick
        // authorizes the fresh open through the production `tick`. The fresh
        // owner's health round-trip then closes the circuit on the one audit
        // line, and a stale second retirement registers nothing.
        // Pre-fix RED: the authorization assert (the shipped test's own
        // `src/backends/sdk.rs:3223` escape, driven from the ladder instead).
        let _guard = circuit_scenario_guard();
        circuit_episode_reset();
        let (requests, receiver) = mpsc::sync_channel(1);
        let (stall_reply, _stall_keep) = mpsc::sync_channel(1);
        let (running_tx, running_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        requests
            .try_send(Envelope {
                request: Request::Submit(vec![0; PIXELS]),
                reply: stall_reply,
            })
            .expect("the channel starts empty");
        let a_generation = claim_sdk_owner().expect("A opens as the authorized owner");
        let abandoned = Arc::new(AtomicBool::new(false));
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
        let mut device = SdkDevice {
            requests: Some(requests),
            payload: Mutex::new(None),
            abandoned: Arc::clone(&abandoned),
            generation: a_generation,
        };
        // The production timeout site: the gated owner never replies inside the
        // bound, so the call is a timeout, A is quarantined under A's own
        // generation, and the circuit opens.
        let error = device
            .call_timeout(Request::Poll, Duration::from_millis(1))
            .err()
            .unwrap();
        assert_eq!(error.cause, LossCause::CallTimeout, "{error}");
        assert!(
            error
                .detail
                .contains("owner quarantined and circuit opened"),
            "{error}"
        );
        assert!(
            circuit_open() && OWNER_QUARANTINED.load(Ordering::Acquire),
            "the full dispatch window quarantines A and opens the circuit"
        );
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation)
        };
        assert_eq!(
            machine,
            (1, a_generation),
            "the window registers A by identity"
        );
        // A's call returns; the queued slot drains and answers the close with a
        // NON-health completion, so the episode survives (46a P3).
        release_tx
            .try_send(())
            .expect("the gated owner accepted the release");
        device
            .close(true)
            .expect("the drained owner answers the close");
        assert!(
            circuit_open(),
            "a close-only completion is not health: the episode stays live"
        );
        thread.join().unwrap();
        // A's self-retire tail (the production tail of `open_session:1726`):
        // the window empties while the circuit and the episode marker stay.
        owner_self_retire(&abandoned, a_generation);
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation)
        };
        assert_eq!(
            machine,
            (0, NO_OWNER),
            "the wedged shape on the production sequence: circuit open, episode live, window EMPTY"
        );
        assert!(
            circuit_open() && OWNER_QUARANTINED.load(Ordering::Acquire),
            "the device is still sick: only a health round-trip closes the episode"
        );
        // The ladder keeps ticking (one tick per pass): the authorization rule
        // is episode-scoped, so the empty window authorizes at the gap.
        let machine = {
            let mut state = recovery_lock();
            (
                state.tick(true, true),
                state.tick(true, true),
                state.tick(true, true),
            )
        };
        assert!(!machine.0, "ladder tick 1 of the gap 3");
        assert!(!machine.1, "ladder tick 2 of the gap 3");
        assert!(
            machine.2,
            "THE WEDGE: the next ladder tick authorizes the fresh open with an EMPTY window (pre-fix: the refusal row forever, panel blank to restart)"
        );
        // The authorized fresh open, in the resolver's own arm shape
        // (`claim_sdk_owner_for_recovery` + `open_session`'s claim swap): the
        // slot is empty (A released it), so nobody is displaced.
        let b_generation = next_owner_generation();
        claim_sdk_owner_for_recovery(b_generation);
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.detached)
        };
        assert_eq!(
            machine,
            (0, 0),
            "an empty slot displaces nobody: no census entry is invented (:423-426)"
        );
        let (fresh_requests, fresh_receiver) = mpsc::sync_channel(1);
        let fresh_calls = Arc::new(Mutex::new(Vec::new()));
        let fresh_thread =
            std::thread::spawn(move || owner_loop(Box::new(Fake(fresh_calls)), fresh_receiver));
        let fresh_device = SdkDevice {
            requests: Some(fresh_requests),
            payload: Mutex::new(None),
            abandoned: Arc::new(AtomicBool::new(false)),
            generation: b_generation,
        };
        fresh_device
            .poll()
            .expect("the fresh owner's health round-trip lands");
        assert!(
            !circuit_open(),
            "the rescued episode closes on the fresh owner's health"
        );
        assert!(
            !OWNER_QUARANTINED.load(Ordering::Acquire),
            "the episode marker cleared with the circuit"
        );
        // A stale second retirement of the old owner registers nothing, and a
        // closed circuit authorizes no resolver pass.
        owner_self_retire(&abandoned, a_generation);
        let machine = {
            let state = recovery_lock();
            (state.in_flight, state.window_generation, state.detached)
        };
        assert_eq!(
            machine,
            (0, NO_OWNER, 0),
            "a stale return retires no window and invents no census entry"
        );
        assert_eq!(
            SDK_OWNER_ACTIVE.load(Ordering::Acquire),
            b_generation,
            "the fresh owner keeps the claim"
        );
        assert!(
            try_circuit_recovery("fixture").is_none(),
            "a closed circuit authorizes no resolver pass (it returns before any discovery)"
        );
        drop(fresh_device);
        fresh_thread.join().unwrap();
        release_sdk_owner(b_generation);
        circuit_episode_reset();
    }

    #[test]
    fn a_retired_owner_is_rescued_at_the_grown_gap_and_never_before() {
        // Coverage row (a): timeout -> late NON-health reply -> retirement ->
        // the rescue is authorized at the GROWN gap (`3 << attempts`, capped at
        // 48), never before. Pre-fix RED: the grown-gap assert (the empty
        // window never authorized at any gap).
        let mut machine = CircuitRecovery::default();
        let owner = next_owner_generation();
        machine.quarantine(owner);
        assert!(!machine.tick(true, true), "gap 3: tick 1");
        assert!(!machine.tick(true, true), "gap 3: tick 2");
        assert!(
            machine.tick(true, true),
            "the occupied window authorizes attempt 1 (attempts = 1)"
        );
        // The fresh owner's call never returns (the window re-homes), and then
        // it returns late with a non-health reply and retires.
        machine.quarantine(owner);
        machine.retire(owner);
        assert_eq!(
            (
                machine.in_flight,
                machine.window_generation,
                machine.attempts
            ),
            (0, NO_OWNER, 1),
            "the wedge after an attempt: empty window, one attempt spent"
        );
        let gap = RECOVERY_PROBE_TICKS << 1;
        for tick in 0..(gap - 1) {
            assert!(
                !machine.tick(true, true),
                "the rescue is never early: pre-gap tick {tick} of the grown gap {gap} refuses"
            );
        }
        assert!(
            machine.tick(true, true),
            "the rescue fires on the grown gap with an EMPTY window (pre-fix: the wedge)"
        );
        assert_eq!(machine.attempts, 2, "exactly one attempt per gap");
    }

    #[test]
    fn a_stale_retirement_keeps_the_window_one_owner_and_the_census_tracking_both() {
        // Coverage rows (b) and (c): an OLD owner's retirement while a newer
        // owner exists (and after the newer owner was quarantined too) leaves
        // the window at ONE owner, keeps the newer owner's registration, and
        // leaves the census tracking exactly the retained owners.
        let mut machine = CircuitRecovery::default();
        let a_generation = next_owner_generation();
        let b_generation = next_owner_generation();
        machine.quarantine(a_generation); // A poisons
        machine.detach(a_generation); // A is displaced by B's fresh open
        machine.quarantine(b_generation); // B poisons too: the window re-homes
        machine.detach(b_generation); // B is displaced by C's fresh open
        machine.retire(a_generation); // A's late return
        assert_eq!(
            (machine.in_flight, machine.window_generation),
            (1, b_generation),
            "the window stays the ONE owner in flight: a stale retirement clears nothing of B"
        );
        assert_eq!(
            machine.detached_generations,
            vec![b_generation],
            "the census tracks exactly the owner that is still retained"
        );
        assert_eq!(
            machine.detached, 1,
            "A's retirement restored A's own headroom"
        );
        assert!(!machine.tick(true, true), "B's episode: tick 1 of 3");
    }

    #[test]
    fn the_process_wide_census_carries_stranded_owners_across_episodes() {
        // Audit #18, item 2: the detached-owner census is PROCESS-WIDE.
        // Episode 1 strands owner A and recovers through owner B's health;
        // episode 2 starts with census = 1 retained A; repeating the episode
        // consumes headroom to the bound (8), where authorization stops
        // fail-closed with ONE recorded warn; retiring a stranded owner
        // restores headroom, so there is NO lifetime lockout.
        // Pre-fix RED: `recovered()` (`:263-264`, `*self = Self::default()`)
        // erased the census, so every episode started at a full 8 - the
        // 54984-class accumulation the bound exists to stop.
        let mut machine = CircuitRecovery::default();
        let mut stranded: Vec<u64> = Vec::new();
        for episode in 1..DETACHED_OWNER_BOUND {
            let a_generation = next_owner_generation();
            machine.quarantine(a_generation); // A poisons
            machine.detach(a_generation); // the resolver displaced A: A is retained
            stranded.push(a_generation);
            assert_eq!(
                machine.detached, episode,
                "episode {episode}: the stranded owners accumulate process-wide"
            );
            // B's health round-trip closes this episode: the episode shape
            // resets; the census does not.
            machine.recovered();
            assert_eq!(
                (
                    machine.responsive_ticks,
                    machine.attempts,
                    machine.in_flight,
                    machine.window_generation
                ),
                (0, 0, 0, NO_OWNER),
                "the episode shape resets on the close"
            );
            assert_eq!(
                machine.detached, episode,
                "the census SURVIVES the close (pre-fix: erased to 0)"
            );
        }
        let a_generation = next_owner_generation();
        machine.quarantine(a_generation);
        machine.detach(a_generation);
        stranded.push(a_generation);
        assert_eq!(
            machine.detached, DETACHED_OWNER_BOUND,
            "the process-wide census reached the bound"
        );
        assert!(
            machine.bound_hit(),
            "the bound is process-wide, not per-episode"
        );
        assert!(
            machine.take_bound_report(),
            "the reaching of the bound is recorded exactly once"
        );
        for tick in 0..=RECOVERY_ATTEMPT_GAP_CAP_TICKS {
            assert!(
                !machine.tick(true, true),
                "at the bound no fresh open is authorized, at any gap (tick {tick})"
            );
        }
        assert!(
            !machine.take_bound_report(),
            "ONE warn: the latch stays consumed while the bound stands"
        );
        assert_eq!(machine.attempts, 0, "the bound consumed no attempts");
        // No lifetime lockout: a stranded owner retires (its call returns), and
        // the next episode authorizes again at the shipped cadence.
        machine.retire(stranded[0]);
        assert!(
            !machine.bound_hit(),
            "one retirement restores headroom process-wide"
        );
        assert!(
            machine.tick(true, true),
            "authorization resumes on the first tick with headroom: the unbroken healthy streak ran the whole bound, and the bound gate consumes no attempt, so there is NO lifetime lockout"
        );
    }

    // WO-U46G item 4 fixture geometry (offsets `validate_pe` reads).
    const PE_AT: usize = 0x40;
    const OPTIONAL_AT: usize = PE_AT + 24;
    const SECTION0_AT: usize = OPTIONAL_AT + 240;
    const SECTION1_AT: usize = SECTION0_AT + 40;
    const EXPORT_RVA: u32 = 0x2000;
    const EXPORT_AT: usize = 0x1A0;
    const NAMES_AT: usize = 0x1E0;
    const STRINGS_AT: usize = 0x220;
    const PE_LEN: usize = 0x2A0;

    /// WO-U46G item 4: a synthetic AMD64 PE32+ image, built from bytes in the
    /// test (no vendor DLL is read), shaped exactly on the fields `validate_pe`
    /// maps: DOS header -> `e_lfanew` -> PE header (2 sections) -> a 240-byte
    /// optional header whose DataDirectory[0] names the export RVA -> a section
    /// table -> the export directory, its name-RVA table and the name strings.
    /// The two sections exist so a hostile mapping can be applied to the NAME
    /// table while the export-directory mapping stays valid.
    fn pe_fixture() -> Vec<u8> {
        let mut bytes = vec![0u8; PE_LEN];
        bytes[0..2].copy_from_slice(b"MZ");
        let put16 = |bytes: &mut [u8], at: usize, value: u16| {
            bytes[at..at + 2].copy_from_slice(&value.to_le_bytes())
        };
        let put32 = |bytes: &mut [u8], at: usize, value: u32| {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes())
        };
        put32(&mut bytes, 0x3c, PE_AT as u32);
        bytes[PE_AT..PE_AT + 4].copy_from_slice(b"PE\0\0");
        put16(&mut bytes, PE_AT + 4, 0x8664);
        put16(&mut bytes, PE_AT + 6, 2);
        put16(&mut bytes, PE_AT + 20, 240);
        put16(&mut bytes, OPTIONAL_AT, 0x20b);
        put32(&mut bytes, OPTIONAL_AT + 112, EXPORT_RVA);
        bytes[SECTION0_AT..SECTION0_AT + 8].copy_from_slice(b".text\0\0\0");
        put32(&mut bytes, SECTION0_AT + 8, 0x100);
        put32(&mut bytes, SECTION0_AT + 12, EXPORT_RVA);
        put32(&mut bytes, SECTION0_AT + 16, 0x100);
        put32(&mut bytes, SECTION0_AT + 20, EXPORT_AT as u32);
        bytes[SECTION1_AT..SECTION1_AT + 8].copy_from_slice(b".rdata\0\0");
        put32(&mut bytes, SECTION1_AT + 8, 0x10);
        put32(&mut bytes, SECTION1_AT + 12, 0x3000);
        put32(&mut bytes, SECTION1_AT + 16, 0x10);
        put32(&mut bytes, SECTION1_AT + 20, STRINGS_AT as u32);
        put32(&mut bytes, EXPORT_AT + 24, EXPORTS.len() as u32);
        put32(
            &mut bytes,
            EXPORT_AT + 32,
            EXPORT_RVA + (NAMES_AT - EXPORT_AT) as u32,
        );
        let mut at = STRINGS_AT;
        for (index, name) in EXPORTS.iter().enumerate() {
            put32(
                &mut bytes,
                NAMES_AT + index * 4,
                EXPORT_RVA + (at - EXPORT_AT) as u32,
            );
            bytes[at..at + name.len()].copy_from_slice(name.as_bytes());
            at += name.len() + 1;
        }
        bytes
    }

    /// Write one fixture and return `validate_pe`'s rejection text (empty when
    /// the image validates), removing the file. The temp-file shape is the
    /// shipped `native_pin_rejects_hard_linked_file` pattern.
    fn validate_pe_outcome(bytes: &[u8]) -> String {
        let path = std::env::temp_dir().join(format!(
            "lcdsirplus-sdk-pe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, bytes).expect("the fixture is writable");
        let outcome = validate_pe(&path).err().unwrap_or_default();
        fs::remove_file(&path).expect("the fixture is removable");
        outcome
    }

    #[test]
    fn validate_pe_rejects_the_hostile_section_mapping_with_descriptive_errors() {
        // Audit #7, item 4: the section mapping and the export-name slice must
        // return the descriptive validation Err for every hostile edge, never a
        // panic and never a wrapped offset. The valid control image proves the
        // fixtures are non-vacuous.
        // Pre-fix RED: the name-offset fixtures index `bytes[start..]`
        // (`src/backends/sdk.rs:1434`) with `start` past the file end, and the
        // offset expression (`src/backends/sdk.rs:1420`) adds/subtracts u32
        // unchecked: under the release gate it panics out of bounds (the whole
        // test is red), under debug it additionally panics on the arithmetic
        // overflow. Disclosed in the receipt: the pre-fix form is a panic, not
        // a wrong answer.
        let good = pe_fixture();
        assert_eq!(
            validate_pe_outcome(&good),
            "",
            "the control image validates: every rejection below is a mutation of it"
        );
        let put32 = |bytes: &mut Vec<u8>, at: usize, value: u32| {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes())
        };
        // Name offset past the file end: the section window is widened so the
        // name RVA maps one byte beyond the file.
        let mut past = good.clone();
        put32(&mut past, SECTION0_AT + 8, 0x1000);
        put32(&mut past, NAMES_AT, 0x2101);
        assert_eq!(
            validate_pe_outcome(&past),
            "invalid SDK export name",
            "a name offset the section maps past the file end is a rejection, not an index panic"
        );
        // The u32 add/sub edge: a raw pointer near u32::MAX, so
        // `raw + rva - virtual_address` overflows and wraps.
        let mut overflow = good.clone();
        put32(&mut overflow, SECTION1_AT + 8, 0x1000);
        put32(&mut overflow, SECTION1_AT + 20, u32::MAX - 0x10);
        put32(&mut overflow, NAMES_AT + 4, 0x3010);
        assert_eq!(
            validate_pe_outcome(&overflow),
            "invalid SDK export name",
            "an overflowing section mapping is clamped to a rejection, never a wrapped offset"
        );
        // The exact-EOF boundary: the mapped offset lands on the file length.
        let mut exact = good.clone();
        put32(&mut exact, SECTION0_AT + 8, 0x101);
        put32(&mut exact, NAMES_AT, 0x2100);
        assert!(
            validate_pe_outcome(&exact).contains("SDK export name"),
            "an offset at the file end is rejected by the export-name rule"
        );
        // `VirtualAddress + span` overflow: the checked add maps nothing.
        let mut span = good.clone();
        put32(&mut span, OPTIONAL_AT + 112, u32::MAX - 4);
        put32(&mut span, SECTION0_AT + 12, u32::MAX - 4);
        assert_eq!(
            validate_pe_outcome(&span),
            "SDK export directory is invalid",
            "a section whose span overflows u32 maps nothing"
        );
        // Truncated tables: the export directory header and the name table.
        let mut truncated = good.clone();
        truncated.truncate(EXPORT_AT + 24);
        assert_eq!(
            validate_pe_outcome(&truncated),
            "truncated SDK export directory",
            "a file that ends inside the export directory is a rejection"
        );
        let mut names = good.clone();
        put32(&mut names, EXPORT_AT + 32, 0x20FE);
        assert_eq!(
            validate_pe_outcome(&names),
            "truncated SDK export name table",
            "a name table that runs past the file end is a rejection"
        );
    }
}
