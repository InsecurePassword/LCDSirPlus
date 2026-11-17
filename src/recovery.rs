//! One-shot recovery of a previously working physical LCD connection.
//! The append-only journal is both the audit log and the durable restart budget.
//! RC-2: the controller also tracks worker liveness (connected-physical vs. the
//! health publication) and heals a silent worker with one named cause plus one
//! reconnect wake; it never touches the worker thread.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenEventW, OpenProcess,
    QueryFullProcessImageNameW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
    EVENT_MODIFY_STATE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

use crate::backends::{BackendKind, BackendState, LossCause};
use crate::model::DeviceStatus;

const PARENT_ENV: &str = "LCDSIRPLUS_CONNECTION_RECOVERY_PARENT";
const READY_TIMEOUT_MS: u32 = 10_000;
const GRACE: Duration = Duration::from_secs(30);
const MAX_JOURNAL_BYTES: u64 = 8192;
/// RC-4 R-1: the shortest legal durable line — `finished` (8) + tab + 10-digit
/// timestamp + tab + an empty detail + newline = 21 bytes. A file whose length
/// plus this minimum exceeds the cap can hold no further line at all, so it is
/// permanently full; that is the rule the cap flag is derived from.
const MIN_JOURNAL_LINE: u64 = 21;

/// Shipped default of `logitech_reconnect_max` (`config.rs` L271; parsed at
/// `parser.rs` L376-L377; validated `>= logitech_reconnect` and `<= 600 s` at
/// `config.rs` L493-L497). Fallback liveness basis only, for a `Recovery`
/// loaded outside the app (tests, diagnostics); `Recovery::start` derives the
/// real value from the resolved config. No new config keys exist.
const DEFAULT_RECONNECT_MAX: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    None,
    Verified,
    Failed,
}

/// Cancellation and the irreversible native action have exactly one winner.
pub struct RestartPermit(AtomicU8);

impl RestartPermit {
    fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    fn cancel(&self) -> bool {
        self.0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn commit(&self, deadline: Instant) -> Result<(), String> {
        if Instant::now() >= deadline {
            self.cancel();
        }
        self.0.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| "Logitech restart cancelled, already attempted, or validation deadline exceeded; no retry".into())
    }

    fn committed(&self) -> bool {
        self.0.load(Ordering::Acquire) == 1
    }

    /// RC-5 read-only label exposure: is the single allowance spent (committed
    /// or cancelled)? The CAS machine is untouched; only the display surface
    /// reads this.
    fn spent(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Fresh,
    ApplicationAttempt,
    ApplicationLaunched(u32),
    Resumed,
    LogitechAttempt,
    LogitechResult,
    Finished,
}

fn transition(stage: Stage, event: &str, detail: &str) -> Result<Stage, String> {
    match (stage, event) {
        (Stage::Fresh, "application-attempt") => Ok(Stage::ApplicationAttempt),
        (Stage::ApplicationAttempt, "application-launched") => detail
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid != 0)
            .map(Stage::ApplicationLaunched)
            .ok_or_else(|| "invalid replacement process ID".into()),
        (Stage::ApplicationLaunched(_), "application-resumed") => Ok(Stage::Resumed),
        (Stage::Resumed, "logitech-attempt") => Ok(Stage::LogitechAttempt),
        (Stage::LogitechAttempt, "logitech-result") => Ok(Stage::LogitechResult),
        (Stage::LogitechAttempt, "logitech-wait-timeout") => Ok(Stage::LogitechAttempt),
        (
            Stage::Fresh
            | Stage::ApplicationAttempt
            | Stage::ApplicationLaunched(_)
            | Stage::Resumed
            | Stage::LogitechAttempt
            | Stage::LogitechResult,
            "finished",
        ) => Ok(Stage::Finished),
        _ => Err("invalid or repeated recovery action; automatic recovery disabled".into()),
    }
}

struct Journal {
    path: PathBuf,
    stage: Stage,
    /// RC-4: the named `connection recovery log is full` condition. The
    /// pre-write cap gate sets it beside the `Err` that carries the name, so it
    /// survives the fail-closed jump to `Finished` and reaches the device
    /// surface instead of passing silently. It is never read back out of the
    /// file: `load` derives it from the durable length and `record` derives it
    /// from the length each accepted append leaves behind, both by
    /// `length + MIN_JOURNAL_LINE > MAX_JOURNAL_BYTES`. The guarded file CAN
    /// still grow — the load path's own fail-closed line fits whenever any line
    /// fits — so this remaining-room rule, not the gate alone, is what makes
    /// the condition visible in every process.
    full: bool,
}

impl Journal {
    fn load(path: PathBuf) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or("connection recovery log has no parent directory")?;
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("connection recovery log directory cannot be created: {error}")
        })?;
        let metadata = std::fs::symlink_metadata(parent).map_err(|error| {
            format!("connection recovery log directory cannot be inspected: {error}")
        })?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(
                "unsafe connection recovery log directory; automatic recovery disabled".into(),
            );
        }
        let mut journal = Self {
            path,
            stage: Stage::Fresh,
            full: false,
        };
        let mut file = match journal.open(false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(journal),
            Err(error) => return Err(format!("connection recovery log cannot be read: {error}")),
        };
        let mut text = String::new();
        (&mut file)
            .take(MAX_JOURNAL_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        if text.is_empty() || text.len() as u64 > MAX_JOURNAL_BYTES || !text.ends_with('\n') {
            return Err("incomplete connection recovery log; automatic recovery disabled".into());
        }
        // RC-4 R-1: the cap condition is DERIVED from the durable bytes, never
        // read from them, so a journal that can hold no further line names
        // itself in every process and not only in the one whose pre-write gate
        // fired. `length + MIN_JOURNAL_LINE > cap` is by definition "no legal
        // line fits": the append-only file CAN still grow (the load path's own
        // fail-closed line fits while any line fits), so the gate alone can
        // never make the condition visible after a restart.
        journal.full = text.len() as u64 + MIN_JOURNAL_LINE > MAX_JOURNAL_BYTES;
        for line in text.lines() {
            let parts: Vec<_> = line.splitn(3, '\t').collect();
            if parts.len() != 3 || parts[1].parse::<u64>().is_err() {
                return Err("invalid connection recovery log; automatic recovery disabled".into());
            }
            journal.stage = transition(journal.stage, parts[0], parts[2])?;
        }
        Ok(journal)
    }

    fn open(&self, create: bool) -> std::io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(create)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&self.path)?;
        let metadata = file.metadata()?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            || info.nNumberOfLinks != 1
            || metadata.len() > MAX_JOURNAL_BYTES
        {
            return Err(std::io::Error::other(
                "unsafe or oversized connection recovery log",
            ));
        }
        Ok(file)
    }

    fn record(&mut self, event: &str, detail: &str) -> Result<(), String> {
        let next = transition(self.stage, event, detail)?;
        let detail: String = detail
            .chars()
            .take(500)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let line = format!("{event}\t{timestamp}\t{detail}\n");
        let result = (|| {
            let mut file = self.open(self.stage == Stage::Fresh)?;
            if file.metadata()?.len() + line.len() as u64 > MAX_JOURNAL_BYTES {
                self.full = true;
                return Err(std::io::Error::other("connection recovery log is full"));
            }
            file.write_all(line.as_bytes())?;
            file.sync_all()?;
            file.metadata().map(|metadata| metadata.len())
        })();
        let len_after = match result {
            Ok(len_after) => len_after,
            Err(error) => {
                self.stage = Stage::Finished;
                return Err(format!(
                    "connection recovery log write failed; no further actions: {error}"
                ));
            }
        };
        self.stage = next;
        // RC-4 R-1: the append that filled the last byte publishes the named
        // condition in its OWN process, from the length it left behind.
        self.full |= len_after + MIN_JOURNAL_LINE > MAX_JOURNAL_BYTES;
        crate::log_info!("connection recovery: {event}: {detail}");
        Ok(())
    }
}

fn journal_path() -> PathBuf {
    crate::app::log_dir().join("connection-recovery.log")
}

pub struct Recovery {
    journal: Option<Journal>,
    previously_connected: bool,
    lost_since: Option<Instant>,
    waiting_since: Instant,
    last_cause: Option<LossCause>,
    logitech_result: Option<Receiver<Result<(), String>>>,
    permit: Arc<RestartPermit>,
    interrupted: bool,
    worker_timed_out: bool,
    awaiting_reconnect: bool,
    /// RC-2: when the worker last claimed a physical connection and still owes
    /// completed-work health for that device cycle. Health refreshes it;
    /// disconnect/absent/discovering and reconnect clear it.
    liveness_since: Option<Instant>,
    /// RC-2 one-shot: the stall wake for the current connection segment has
    /// been spent; only fresh health re-arms it (repeat-rejection).
    stall_actioned: bool,
    /// RC-2 liveness deadline: exceeds the capped reconnect ladder
    /// (`reconnect_max`) by `GRACE`, so a worker in legitimate backoff,
    /// Disconnected, Absent, or mid-reconnect is never read as stalled.
    stall_deadline: Duration,
    /// RC-5 label exposure: the named cause of a failed recovery action,
    /// recorded at the sites that already log the failure. Reporting only —
    /// it never drives a stage transition, a permit, or a journal write.
    recovery_failure: Option<LossCause>,
}

impl Recovery {
    pub fn start(handoff: Handoff, reconnect_max: Duration) -> Self {
        let mut recovery = Self::load(
            journal_path(),
            handoff == Handoff::Verified,
            std::process::id(),
            Instant::now(),
        );
        // RC-2: the liveness deadline is derived from the capped reconnect
        // ladder the worker already uses, plus `GRACE`, so the worker always
        // gets a full ladder cycle (and its own grace) before a stall counts.
        recovery.stall_deadline = reconnect_max + GRACE;
        if handoff == Handoff::Failed {
            if recovery
                .journal
                .as_ref()
                .is_some_and(|journal| journal.stage == Stage::Fresh)
            {
                recovery.finish("LCDSirPlus relaunch handoff failed; Logitech Gaming Software action skipped; no retry");
            }
            if let Some(journal) = &mut recovery.journal {
                journal.stage = Stage::Finished;
            }
        }
        recovery
    }

    fn load(path: PathBuf, relaunched: bool, pid: u32, now: Instant) -> Self {
        let journal = match Journal::load(path) {
            Ok(mut journal) => {
                if relaunched && journal.stage == Stage::ApplicationLaunched(pid) {
                    if let Err(error) = journal.record(
                        "application-resumed",
                        "LCDSirPlus relaunched; checking physical connection",
                    ) {
                        crate::log_error!("{error}");
                    }
                } else if !matches!(journal.stage, Stage::Fresh | Stage::Finished) {
                    if let Err(error) = journal.record("finished", "LCDSirPlus recovery interrupted; remaining actions skipped for LCDSirPlus and Logitech Gaming Software; no crash recovery") {
                        crate::log_error!("{error}");
                    }
                } else if relaunched {
                    crate::log_error!("LCDSirPlus replacement has no matching recovery action; automatic recovery disabled");
                    if journal.stage == Stage::Fresh {
                        if let Err(error) = journal.record("finished", "LCDSirPlus replacement has no matching recovery action; Logitech Gaming Software action skipped; restart allowance remains consumed") {
                            crate::log_error!("{error}");
                        }
                    }
                }
                Some(journal)
            }
            Err(error) => {
                crate::log_error!("{error}");
                None
            }
        };
        Self {
            journal,
            previously_connected: false,
            lost_since: None,
            waiting_since: now,
            last_cause: None,
            logitech_result: None,
            permit: Arc::new(RestartPermit::new()),
            interrupted: false,
            worker_timed_out: false,
            awaiting_reconnect: false,
            liveness_since: None,
            stall_actioned: false,
            stall_deadline: DEFAULT_RECONNECT_MAX + GRACE,
            recovery_failure: None,
        }
    }

    pub fn healthy(&mut self, kind: BackendKind) {
        self.healthy_at(kind, Instant::now());
    }

    fn healthy_at(&mut self, kind: BackendKind, now: Instant) {
        if self.awaiting_reconnect || !matches!(kind, BackendKind::Hid | BackendKind::Sdk) {
            return;
        }
        self.previously_connected = true;
        self.lost_since = None;
        self.last_cause = None;
        // RC-5 F-2: a working device outranks a past recovery failure, so the
        // named (d) label clears on the same real-health gate as the loss cause.
        // (c) `NO RETRY` is untouched: it is driven by the spent permit and the
        // `Finished` journal, i.e. allowance state, not a sticky failure.
        self.recovery_failure = None;
        // Progress refreshes a claimed physical connection instead of disabling
        // its watchdog after the first successful frame. Unclaimed/virtual or
        // deliberately disconnected workers do not acquire a liveness clock.
        self.liveness_since = self.liveness_since.map(|_| now);
        self.stall_actioned = false;
        if self.logitech_result.is_some() {
            self.permit.cancel();
        }
        // A running Logitech restart must finish before recording its outcome.
        if self.logitech_result.is_none()
            && self
                .journal
                .as_ref()
                .is_some_and(|j| matches!(j.stage, Stage::Resumed | Stage::LogitechResult))
        {
            self.finish("physical LCD connection recovered; restart allowance remains consumed");
        }
    }

    pub fn disconnected(&mut self, now: Instant, cause: LossCause) {
        self.liveness_since = None;
        self.last_cause = Some(cause);
        if self.previously_connected {
            self.lost_since.get_or_insert(now);
        }
    }

    /// RC-2 liveness bookkeeping, driven by the app-side backend drain for every
    /// published state. A physical worker that claims `Connected` owes exactly
    /// periodic completed-work health for that device cycle; while it is in backoff,
    /// `Disconnected`, `Absent`, `Discovering` or shutdown, health is
    /// legitimately absent and nothing may count as a stall.
    pub fn note_liveness(&mut self, now: Instant, state: &BackendState) {
        self.liveness_since = match state {
            BackendState::Connected {
                kind: BackendKind::Hid | BackendKind::Sdk,
            } => Some(now),
            _ => None,
        };
    }

    pub fn reconnect_started(&mut self) {
        // The single backend worker queues this before health from its new device.
        self.awaiting_reconnect = false;
        // The wake owns the rebuild: no device is claimed yet, so the liveness
        // clock restarts at the worker's next `Connected`.
        self.liveness_since = None;
    }

    /// RC-4 read-only exposure of the named cap condition for the display
    /// surface: true once the 8192-byte pre-write gate has fired in this
    /// process, or once the durable bytes leave room for no legal line at all
    /// (the R-1 derivation, at load and after every accepted append). Reads
    /// journal state only; it never mutates stage, permit, or journal.
    fn journal_full(&self) -> bool {
        self.journal.as_ref().is_some_and(|journal| journal.full)
    }

    /// RC-5 label exposure for the human-facing surface: (d) a recovery action
    /// failed (named cause), else (c) the one restart allowance is spent and the
    /// journal is `Finished`, so there is no further automatic action. RC-4: the
    /// cap-reached outcome joins (c) — "allowance exhausted / log full" is the
    /// same named terminal outcome, so a full journal never reads as a healthy
    /// silent pass. R-1 is what makes that claim true of a fresh process as
    /// well: `Journal::load` and every accepted append derive the condition
    /// from the bytes in the file (see `MIN_JOURNAL_LINE`) instead of waiting
    /// for a gate that only the refusing process ever reaches. Reads
    /// permit/stage/cause/cap state only; it never mutates stage, permit, or
    /// journal.
    pub fn display_status(&self) -> DeviceStatus {
        if let Some(cause) = self.recovery_failure {
            return DeviceStatus::RecoveryFailed {
                code: cause.short(),
            };
        }
        if self.journal_full()
            || (self.permit.spent()
                && self
                    .journal
                    .as_ref()
                    .is_some_and(|journal| journal.stage == Stage::Finished))
        {
            return DeviceStatus::AllowanceConsumed;
        }
        DeviceStatus::Healthy
    }

    /// RC-5: name a failed recovery action with the RC-1 cause that drove it,
    /// falling back to `probe-fail` when the action itself is the first failure
    /// (no loss cause recorded). Display recording only.
    fn note_action_failure(&mut self) {
        self.recovery_failure = Some(self.last_cause.unwrap_or(LossCause::ProbeFail));
    }

    pub fn suspend(&mut self) {
        self.permit.cancel();
        self.previously_connected = false;
        self.lost_since = None;
        self.liveness_since = None;
        self.stall_actioned = false;
        if self.logitech_result.is_some() {
            self.interrupted = true;
            return;
        }
        if self.journal.as_ref().is_some_and(|j| {
            matches!(
                j.stage,
                Stage::Resumed | Stage::LogitechAttempt | Stage::LogitechResult
            )
        }) {
            self.finish(
                "connection recovery skipped: backend configuration changed or safe mode enabled",
            );
        }
    }

    fn record(&mut self, event: &str, detail: &str) -> bool {
        let Some(journal) = &mut self.journal else {
            return false;
        };
        match journal.record(event, detail) {
            Ok(()) => true,
            Err(error) => {
                crate::log_error!("{error}");
                false
            }
        }
    }

    fn finish(&mut self, detail: &str) {
        self.record(
            "finished",
            &format!("LCDSirPlus/Logitech Gaming Software: {detail}"),
        );
    }

    fn restart_application(&mut self, launch: impl FnOnce() -> Result<u32, String>) -> bool {
        // RC-1: the loss cause rides in the detail of this journal line; recovery
        // outcomes keep their own event labels ("logitech-result", "finished").
        let cause = self
            .last_cause
            .map(|cause| format!("; loss-cause={}", cause.short()))
            .unwrap_or_default();
        if !self.record(
            "application-attempt",
            &format!(
                "LCDSirPlus lost a previously working physical connection; restart once{cause}"
            ),
        ) {
            return false;
        }
        match launch() {
            Ok(pid) => self.record("application-launched", &pid.to_string()),
            Err(error) => {
                self.note_action_failure();
                self.finish(&format!("LCDSirPlus restart failed: {error}; Logitech action skipped because no replacement is running"));
                false
            }
        }
    }

    fn due(&self, now: Instant) -> Option<Stage> {
        let stage = self.journal.as_ref()?.stage;
        match stage {
            Stage::Fresh
                if self
                    .lost_since
                    .is_some_and(|since| now.saturating_duration_since(since) >= GRACE) =>
            {
                Some(stage)
            }
            Stage::Resumed | Stage::LogitechResult
                if now.saturating_duration_since(self.waiting_since) >= GRACE =>
            {
                Some(stage)
            }
            _ => None,
        }
    }

    /// RC-2: is a claimed physical connection silent past the liveness deadline?
    /// Suppressed while a reconnect is already awaited (the wake owns the
    /// rebuild) and while the one-shot for this segment is spent. Frames keep
    /// being submitted throughout: this tick runs in the dashboard loop that
    /// submits them, so the submit path is provably alive while health is not.
    fn stall_due(&self, now: Instant) -> bool {
        !self.awaiting_reconnect
            && !self.stall_actioned
            && self
                .liveness_since
                .is_some_and(|since| now.saturating_duration_since(since) >= self.stall_deadline)
    }

    /// Called only by the responsive dashboard loop. True requests normal app shutdown.
    pub fn tick(&mut self, now: Instant, enabled: bool, request_reconnect: impl FnOnce()) -> bool {
        // The app-side wake may be spent once per tick: by the successful
        // Logitech restart below, or by the RC-2 stall heal, never by both.
        let mut wake = Some(request_reconnect);
        if !enabled {
            self.suspend();
        }
        if let Some(receiver) = &self.logitech_result {
            match receiver.try_recv() {
                Ok(result) => {
                    self.logitech_result = None;
                    let restarted = result.is_ok();
                    let detail = match result {
                        Ok(()) => "Logitech Gaming Software restarted once".to_string(),
                        Err(error) => {
                            self.note_action_failure();
                            format!("Logitech Gaming Software restart failed/skipped: {error}")
                        }
                    };
                    if self.record("logitech-result", &detail) {
                        self.waiting_since = now;
                        if self.interrupted {
                            self.finish("remaining actions skipped: backend configuration changed or safe mode enabled");
                        } else if restarted {
                            self.previously_connected = false;
                            self.lost_since = None;
                            self.awaiting_reconnect = true;
                            if let Some(wake) = wake.take() {
                                wake();
                            }
                            // Grace begins after the retry request, not before LCore restarts.
                            self.waiting_since = Instant::now();
                        } else if self.previously_connected && self.lost_since.is_none() {
                            self.finish("physical LCD connection recovered; restart allowance remains consumed");
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.logitech_result = None;
                    self.note_action_failure();
                    self.finish("Logitech Gaming Software restart worker failed; no retry");
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if !self.worker_timed_out
                        && now.saturating_duration_since(self.waiting_since) >= GRACE
                    {
                        self.worker_timed_out = true;
                        self.permit.cancel();
                        if !self.permit.committed() {
                            self.logitech_result = None;
                            self.finish("Logitech Gaming Software restart timed out before execution; action cancelled; no retry");
                        } else {
                            self.record("logitech-wait-timeout", "Logitech Gaming Software restart already committed; awaiting its result; no additional action");
                        }
                    }
                }
            }
        }
        if !enabled {
            return false;
        }
        if self.stall_due(now) {
            // RC-2 one-shot self-heal: the worker holds a claimed physical
            // device, keeps consuming submitted frames, and publishes no
            // health. Name the stall with the RC-1 vocabulary (it rides in the
            // detail of the next legitimate journal event) and ask the worker
            // to rebuild its own device exactly once through the existing
            // app-side wake. No thread kill, no join escalation, no restart
            // loop: `TerminateThread` is the original's measured weakness and
            // stays out of this port; the worker owns the device lifecycle.
            self.stall_actioned = true;
            self.last_cause = Some(LossCause::WorkerStalled);
            if self.previously_connected {
                self.lost_since.get_or_insert(now);
            }
            crate::log_warn!(
                "backend worker stalled: physical connection published no health within {:.0?}; loss-cause={}; one reconnect requested; no thread action",
                self.stall_deadline,
                LossCause::WorkerStalled.short()
            );
            if let Some(wake) = wake.take() {
                wake();
            }
        }
        let due = self.due(now);
        if due.is_some() && crate::backends::sdk::recovery_blocked() {
            self.finish("Logitech SDK trust validation failed; automatic restarts skipped");
            return false;
        }
        match due {
            Some(Stage::Fresh) => {
                // No automatic restarts for an unplugged/unidentifiable G13.
                if crate::backends::hid::discover().candidates.is_empty() {
                    self.lost_since = Some(now);
                    return false;
                }
                return self.restart_application(launch_replacement);
            }
            Some(Stage::Resumed) => {
                if !self.record("logitech-attempt", "LCDSirPlus is responsive but still disconnected; restart Logitech Gaming Software once") { return false; }
                let (sender, receiver) = mpsc::sync_channel(1);
                let permit = Arc::clone(&self.permit);
                self.waiting_since = now;
                match std::thread::Builder::new().name("lcdsirplus-connection-recovery".into()).spawn(move || {
                    let result = if crate::backends::hid::discover().candidates.is_empty() {
                        Err("G13 is absent or cannot be identified; Logitech restart skipped".into())
                    } else {
                        crate::backends::sdk::restart_lcore(&permit, now + GRACE)
                    };
                    let _ = sender.send(result);
                }) {
                    Ok(_) => self.logitech_result = Some(receiver),
                    Err(error) => {
                        self.note_action_failure();
                        self.finish(&format!("Logitech Gaming Software restart worker could not start: {error}; no retry"))
                    }
                }
            }
            Some(Stage::LogitechResult) => self.finish("physical LCD still disconnected after one LCDSirPlus restart and one Logitech attempt; automatic recovery exhausted; preview retained"),
            _ => {}
        }
        false
    }
}

impl Drop for Recovery {
    fn drop(&mut self) {
        self.permit.cancel();
        if self.permit.committed() {
            if let Some(receiver) = self.logitech_result.take() {
                let detail = match receiver.recv_timeout(Duration::from_secs(10)) {
                    Ok(Ok(())) => "Logitech Gaming Software restarted once before LCDSirPlus shutdown".to_string(),
                    Ok(Err(error)) => format!("Logitech Gaming Software restart failed/skipped: {error}"),
                    Err(_) => "Logitech Gaming Software restart result unconfirmed at LCDSirPlus shutdown; no retry".into(),
                };
                self.record("logitech-result", &detail);
            }
        }
        if self.journal.as_ref().is_some_and(|j| {
            matches!(
                j.stage,
                Stage::Resumed | Stage::LogitechAttempt | Stage::LogitechResult
            )
        }) {
            self.finish("LCDSirPlus exited during connection recovery; remaining actions skipped; no crash recovery");
        }
    }
}

fn launch_replacement() -> Result<u32, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(executable);
    command.args(std::env::args_os().skip(1));
    launch_ready_child(
        &mut command,
        std::process::id(),
        process_creation_time(unsafe { GetCurrentProcess() })?,
    )
    .map(|child| child.id())
}

struct NativeHandle(HANDLE);

impl Drop for NativeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn launch_ready_child(
    command: &mut std::process::Command,
    pid: u32,
    created: u64,
) -> Result<std::process::Child, String> {
    // A unique local event prevents a stale signal from authorizing shutdown.
    let nonce = unsafe { windows::Win32::System::Com::CoCreateGuid() }
        .map_err(|error| error.to_string())?;
    let name = format!("Local\\LCDSirPlus.RecoveryReady.{nonce:?}");
    let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
    let ready = NativeHandle(
        unsafe { CreateEventW(None, true, false, PCWSTR(wide.as_ptr())) }
            .map_err(|error| error.to_string())?,
    );
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(
            "LCDSirPlus replacement readiness event already exists; relaunch skipped".into(),
        );
    }
    let mut child = command
        .env(PARENT_ENV, format!("{pid}:{created}:{name}"))
        .spawn()
        .map_err(|error| error.to_string())?;
    let process = HANDLE(child.as_raw_handle());
    let ready_result =
        unsafe { WaitForMultipleObjects(&[ready.0, process], false, READY_TIMEOUT_MS) };
    if ready_result == WAIT_OBJECT_0 && unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT {
        return Ok(child);
    }
    // Only our just-spawned replacement is cleaned up; the original stays alive.
    let _ = child.kill();
    let _ = child.wait();
    Err("LCDSirPlus replacement did not acquire and verify its parent within 10 seconds or exited; original retained; Logitech action skipped".into())
}

fn process_creation_time(process: HANDLE) -> Result<u64, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }
        .map_err(|error| error.to_string())?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// A replacement waits for the old process (and its instance mutex) to go away.
/// This is not a watcher: a crash or an ordinary launch never enters this path.
pub fn wait_for_parent() -> Result<bool, String> {
    let Some(value) = std::env::var_os(PARENT_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(PARENT_ENV);
    let mut identity = value
        .to_str()
        .ok_or("invalid LCDSirPlus recovery parent identity")?
        .splitn(3, ':');
    let pid = identity
        .next()
        .ok_or("missing LCDSirPlus recovery parent")?;
    let creation_time = identity
        .next()
        .ok_or("missing LCDSirPlus recovery parent creation time")?
        .parse::<u64>()
        .map_err(|_| "invalid LCDSirPlus recovery parent creation time")?;
    let pid = pid
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0 && *pid != std::process::id())
        .ok_or("invalid LCDSirPlus recovery parent")?;
    let ready_name = identity
        .next()
        .filter(|name| name.starts_with("Local\\LCDSirPlus.RecoveryReady."))
        .ok_or("invalid LCDSirPlus recovery readiness event")?;
    unsafe {
        let process = match OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        ) {
            Ok(process) => process,
            Err(error) => return Err(format!("cannot wait for previous LCDSirPlus: {error}")),
        };
        let result = (|| {
            if process_creation_time(process)? != creation_time {
                return Err(
                    "LCDSirPlus recovery parent process identity changed; no recovery escalation"
                        .into(),
                );
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
            let parent = std::fs::canonicalize(PathBuf::from(String::from_utf16_lossy(
                &path[..length as usize],
            )))
            .map_err(|error| error.to_string())?;
            if parent != crate::runtime::canonical_executable()? {
                return Err("LCDSirPlus recovery parent executable mismatch".into());
            }
            if WaitForSingleObject(process, 0) != WAIT_TIMEOUT {
                return Err(
                    "previous LCDSirPlus exited before readiness; no recovery escalation".into(),
                );
            }
            let wide: Vec<_> = ready_name.encode_utf16().chain(Some(0)).collect();
            let ready = NativeHandle(
                OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide.as_ptr())).map_err(|error| {
                    format!("cannot acknowledge LCDSirPlus replacement readiness: {error}")
                })?,
            );
            // Signal only while the validated parent process handle is held.
            SetEvent(ready.0).map_err(|error| error.to_string())?;
            if WaitForSingleObject(process, 60_000) != WAIT_OBJECT_0 {
                return Err(
                    "previous LCDSirPlus did not exit within 60 seconds; replacement skipped"
                        .into(),
                );
            }
            let mut exit_code = 0;
            GetExitCodeProcess(process, &mut exit_code).map_err(|error| error.to_string())?;
            if exit_code != 0 {
                return Err("previous LCDSirPlus did not exit normally; no crash recovery".into());
            }
            Ok(true)
        })();
        let _ = CloseHandle(process);
        result
    }
}

pub fn startup_failed(detail: &str) {
    match Journal::load(journal_path()) {
        Ok(mut journal) if journal.stage == Stage::Fresh || journal.stage == Stage::ApplicationAttempt || matches!(journal.stage, Stage::ApplicationLaunched(pid) if pid == std::process::id()) => {
            if let Err(error) = journal.record(
                "finished",
                &format!(
                    "LCDSirPlus replacement failed: {detail}; Logitech Gaming Software action skipped; no retry"
                ),
            ) {
                crate::log_error!("{error}");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "lcdsirplus-recovery-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn journal(&self) -> PathBuf {
            self.0.join("connection-recovery.log")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn initial_failure_and_virtual_never_arm_but_a_lost_working_connection_does() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        recovery.disconnected(now, LossCause::DeviceAbsent);
        recovery.healthy(BackendKind::Virtual);
        recovery.disconnected(now + GRACE, LossCause::DeviceAbsent);
        assert_eq!(recovery.due(now + GRACE * 10), None);
        assert!(
            !root.journal().exists(),
            "startup does not consume or create the log"
        );

        recovery.healthy(BackendKind::Sdk);
        recovery.disconnected(now, LossCause::DeviceAbsent);
        recovery.disconnected(now + GRACE / 2, LossCause::DeviceAbsent);
        assert_eq!(recovery.due(now + GRACE - Duration::from_millis(1)), None);
        assert_eq!(recovery.due(now + GRACE), Some(Stage::Fresh));
        recovery.healthy(BackendKind::Sdk);
        assert_eq!(recovery.due(now + GRACE * 10), None);
        recovery.disconnected(now + GRACE * 10, LossCause::DeviceAbsent);
        recovery.suspend();
        assert_eq!(recovery.due(now + GRACE * 20), None);
    }

    #[test]
    fn each_restart_is_persisted_before_action_and_never_repeated_across_launches() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        first.healthy(BackendKind::Sdk);
        first.disconnected(now, LossCause::DeviceAbsent);
        assert!(first.restart_application(|| {
            assert_eq!(
                Journal::load(root.journal()).unwrap().stage,
                Stage::ApplicationAttempt
            );
            Ok(42)
        }));
        drop(first);

        let mut replacement = Recovery::load(root.journal(), true, 42, now);
        assert_eq!(replacement.due(now + GRACE), Some(Stage::Resumed));
        assert!(replacement.record("logitech-attempt", "restart Logitech Gaming Software once"));
        assert_eq!(
            Journal::load(root.journal()).unwrap().stage,
            Stage::LogitechAttempt
        );
        assert!(!replacement.record("logitech-attempt", "a forbidden repeat"));
        // Inject a failed worker result; never restart a real Logitech process in tests.
        let (sender, receiver) = mpsc::sync_channel(1);
        replacement.logitech_result = Some(receiver);
        sender.send(Err("injected access denied".into())).unwrap();
        assert!(!replacement.tick(now + GRACE / 2, true, || panic!(
            "failed restart must not request reconnect"
        )));
        assert_eq!(
            replacement.journal.as_ref().unwrap().stage,
            Stage::LogitechResult
        );
        assert!(!replacement.tick(now + GRACE * 2, true, || panic!("no repeated reconnect")));
        assert_eq!(replacement.journal.as_ref().unwrap().stage, Stage::Finished);
        assert!(!replacement.restart_application(|| panic!("application restarted twice")));
        drop(replacement);

        let mut later = Recovery::load(root.journal(), false, 99, now);
        later.healthy(BackendKind::Hid);
        later.disconnected(now, LossCause::DeviceAbsent);
        assert_eq!(later.due(now + GRACE * 10), None);
        let text = std::fs::read_to_string(root.journal()).unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("application-attempt\t"))
                .count(),
            1
        );
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("logitech-attempt\t"))
                .count(),
            1
        );
        assert!(text
            .contains("Logitech Gaming Software restart failed/skipped: injected access denied"));
        assert!(text.contains("automatic recovery exhausted"));
    }

    #[test]
    fn failed_application_launch_is_logged_and_does_not_escalate_or_retry() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        assert!(!recovery.restart_application(|| Err("injected launch failure".into())));
        assert!(!recovery.restart_application(|| panic!("launch retried")));
        drop(recovery);
        let later = Recovery::load(root.journal(), false, 2, now);
        assert_eq!(later.due(now + GRACE * 10), None);
        let text = std::fs::read_to_string(root.journal()).unwrap();
        assert!(text.contains("LCDSirPlus restart failed: injected launch failure"));
        assert!(text.contains("Logitech action skipped"));
        assert!(!text.contains("logitech-attempt\t"));
    }

    #[test]
    fn success_consumes_the_allowance_even_when_logitech_restart_was_not_needed() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        assert!(first.restart_application(|| Ok(42)));
        drop(first);
        let mut replacement = Recovery::load(root.journal(), true, 42, now);
        replacement.healthy(BackendKind::Sdk);
        replacement.disconnected(now, LossCause::DeviceAbsent);
        assert_eq!(replacement.due(now + GRACE * 10), None);
        assert!(!replacement.record("logitech-attempt", "forbidden after success"));
        drop(replacement);
        assert_eq!(
            Journal::load(root.journal()).unwrap().stage,
            Stage::Finished
        );
    }

    /// RC-5 acceptance (3): the allowance-consumed label (c) is published only
    /// on the exhaustion path, after the permit is spent and the journal has
    /// reached `Finished`.
    #[test]
    fn allowance_consumed_label_appears_only_after_the_permit_is_spent() {
        let root = TestDir::new();
        let now = Instant::now();
        // A journal finished without the permit ever taken must not claim (c):
        // the allowance was never consumed, so the label stays unpublished.
        std::fs::write(root.journal(), "application-attempt\t42\tonce\n").unwrap();
        let mismatched = Recovery::load(root.journal(), true, 99, now);
        assert_eq!(mismatched.journal.as_ref().unwrap().stage, Stage::Finished);
        assert!(!mismatched.permit.spent());
        assert_eq!(mismatched.display_status(), DeviceStatus::Healthy);
        drop(mismatched);
        std::fs::remove_file(root.journal()).unwrap();

        let mut first = Recovery::load(root.journal(), false, 1, now);
        first.disconnected(now, LossCause::CallTimeout);
        assert!(first.restart_application(|| Ok(42)));
        assert_eq!(first.display_status(), DeviceStatus::Healthy);
        drop(first);

        let mut replacement = Recovery::load(root.journal(), true, 42, now);
        assert_eq!(replacement.display_status(), DeviceStatus::Healthy);
        assert!(!replacement.permit.spent());
        assert!(replacement.record("logitech-attempt", "restart Logitech Gaming Software once"));
        replacement.permit.commit(now + GRACE).unwrap();
        assert!(replacement.permit.spent());
        // Permit spent, stage `LogitechAttempt`: (c) stays unpublished.
        assert_eq!(replacement.display_status(), DeviceStatus::Healthy);

        // Inject a successful worker result; never touch a real Logitech process.
        // The result tick runs inside the grace window, so the exhaustion step
        // stays a separate, later tick.
        let (sender, receiver) = mpsc::sync_channel(1);
        replacement.logitech_result = Some(receiver);
        sender.send(Ok(())).unwrap();
        assert!(!replacement.tick(now + GRACE / 2, true, || {}));
        assert_eq!(
            replacement.journal.as_ref().unwrap().stage,
            Stage::LogitechResult
        );
        // Permit spent, stage not `Finished`: (c) stays unpublished.
        assert_eq!(replacement.display_status(), DeviceStatus::Healthy);

        // Exhaustion tick: the journal finishes and (c) is published at last.
        assert!(!replacement.tick(now + GRACE * 4, true, || {
            panic!("no action after exhaustion")
        }));
        assert_eq!(replacement.journal.as_ref().unwrap().stage, Stage::Finished);
        assert_eq!(
            replacement.display_status(),
            DeviceStatus::AllowanceConsumed
        );
        drop(replacement);
    }

    /// RC-5 state (d): a failed recovery action is named with the RC-1 cause
    /// code, and is distinct from the allowance-consumed label (c).
    #[test]
    fn failed_recovery_action_publishes_the_named_cause() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        recovery.disconnected(now, LossCause::ProbeFail);
        assert!(!recovery.restart_application(|| Err("injected launch failure".into())));
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "probe-fail".to_string()
            }
        );
        assert_ne!(
            recovery.display_status().label(),
            DeviceStatus::AllowanceConsumed.label()
        );
        drop(recovery);
    }

    /// RC-5 state (d) on the Logitech step: the worker's own failure names its
    /// cause; a failure is never published as a generic loss.
    #[test]
    fn failed_logitech_restart_publishes_the_named_cause() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        assert!(first.restart_application(|| Ok(42)));
        drop(first);

        let mut replacement = Recovery::load(root.journal(), true, 42, now);
        replacement.disconnected(now, LossCause::CallTimeout);
        assert!(replacement.record("logitech-attempt", "restart Logitech Gaming Software once"));
        let (sender, receiver) = mpsc::sync_channel(1);
        replacement.logitech_result = Some(receiver);
        sender.send(Err("injected access denied".into())).unwrap();
        assert!(!replacement.tick(now + GRACE / 2, true, || {
            panic!("failed restart must not request reconnect")
        }));
        assert_eq!(
            replacement.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "call-timeout".to_string()
            }
        );
        assert!(!replacement
            .display_status()
            .label()
            .contains("injected access denied"));
        drop(replacement);
    }

    /// RC-5 F-2: a working device outranks a past recovery action failure, so
    /// the named (d) label is cleared by the same real-health gate that clears
    /// `last_cause`/`lost_since`; mid-reconnect and virtual kinds never qualify.
    #[test]
    fn recovery_failure_label_clears_on_real_health_only() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        recovery.disconnected(now, LossCause::ProbeFail);
        assert!(!recovery.restart_application(|| Err("injected launch failure".into())));
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "probe-fail".to_string()
            },
            "the failed recovery action publishes (d) first"
        );

        // A virtual device is not a physical connection, so its health must not
        // erase the recorded failure.
        recovery.healthy(BackendKind::Virtual);
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "probe-fail".to_string()
            },
            "virtual health must not clear (d)"
        );

        // While a reconnect is awaited, health is not yet confirmed for this
        // device cycle, so the gate must not clear (d) either.
        recovery.awaiting_reconnect = true;
        recovery.healthy(BackendKind::Hid);
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "probe-fail".to_string()
            },
            "health during a reconnect must not clear (d)"
        );
        recovery.awaiting_reconnect = false;

        // Genuine physical health clears (d): the panel returns to the
        // transient/healthy arms. The permit was never taken in this cycle, so
        // (c) is not published either.
        recovery.healthy(BackendKind::Hid);
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::Healthy,
            "a working device must not keep reporting a past recovery failure"
        );
        drop(recovery);
    }

    /// RC-5 F-2: (c) is allowance state, not a sticky failure — the spent
    /// permit plus the `Finished` journal keep `NO RETRY` published through
    /// health, and health hands over from (d) to (c) without touching either.
    #[test]
    fn allowance_consumed_label_is_not_cleared_by_health() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        assert!(first.restart_application(|| Ok(42)));
        drop(first);

        let mut replacement = Recovery::load(root.journal(), true, 42, now);
        replacement.disconnected(now, LossCause::CallTimeout);
        replacement.previously_connected = true;
        replacement.lost_since = Some(now);
        assert!(replacement.record("logitech-attempt", "restart Logitech Gaming Software once"));
        replacement.permit.commit(now + GRACE).unwrap();
        // Inject a failed worker result; never touch a real Logitech process.
        let (sender, receiver) = mpsc::sync_channel(1);
        replacement.logitech_result = Some(receiver);
        sender.send(Err("injected access denied".into())).unwrap();
        assert!(!replacement.tick(now + GRACE / 2, true, || {}));
        // (d) outranks (c) while the failure is recorded.
        assert_eq!(
            replacement.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "call-timeout".to_string()
            }
        );

        // Exhaustion tick: the journal reaches `Finished` with the permit spent.
        assert!(!replacement.tick(now + GRACE * 4, true, || {
            panic!("no action after exhaustion")
        }));
        assert_eq!(replacement.journal.as_ref().unwrap().stage, Stage::Finished);
        assert_eq!(
            replacement.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "call-timeout".to_string()
            },
            "(d) still outranks (c) once the cycle finishes"
        );

        // Health clears (d) only: the spent allowance and the `Finished` journal
        // are untouched, so (c) keeps publishing.
        replacement.healthy(BackendKind::Hid);
        assert!(
            replacement.permit.spent(),
            "health must not un-spend the permit"
        );
        assert_eq!(
            replacement.journal.as_ref().unwrap().stage,
            Stage::Finished,
            "health must not move the journal off `Finished`"
        );
        assert_eq!(
            replacement.display_status(),
            DeviceStatus::AllowanceConsumed,
            "NO RETRY is allowance state: health must not clear it"
        );
        assert_eq!(replacement.display_status().label(), "NO RETRY");
        drop(replacement);
    }

    /// RC-5 F-2: clearing (d) on health does not disable the recorder. A
    /// repeated action inside the finished cycle is rejected by the journal
    /// machine, so no (d) appears for a blocked action; the next recovery cycle
    /// (a fresh journal) records a new action failure as (d) again.
    #[test]
    fn action_failure_is_recorded_again_after_health_cleared_the_label() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        recovery.disconnected(now, LossCause::ProbeFail);
        assert!(!recovery.restart_application(|| Err("injected launch failure".into())));
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "probe-fail".to_string()
            }
        );
        recovery.healthy(BackendKind::Hid);
        assert_eq!(recovery.display_status(), DeviceStatus::Healthy);

        // This cycle's journal is `Finished`: the repeat action is rejected by
        // the stage machine before it runs, so (d) is not republished.
        recovery.disconnected(now, LossCause::CallTimeout);
        assert!(!recovery.restart_application(|| Err("injected repeat launch failure".into())));
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::Healthy,
            "a journal-blocked action must not publish (d)"
        );
        drop(recovery);

        // A fresh recovery cycle records its own action failure as (d), named by
        // that cycle's loss cause.
        std::fs::remove_file(root.journal()).unwrap();
        let mut next = Recovery::load(root.journal(), false, 1, now);
        next.disconnected(now, LossCause::CallTimeout);
        assert!(!next.restart_application(|| Err("injected launch failure".into())));
        assert_eq!(
            next.display_status(),
            DeviceStatus::RecoveryFailed {
                code: "call-timeout".to_string()
            },
            "a fresh cycle re-records (d) after the cleared one"
        );
        drop(next);
    }

    #[test]
    fn ordinary_launch_and_wrong_replacement_do_not_resume_interrupted_recovery() {
        for (relaunched, pid) in [(false, 42), (true, 99)] {
            let root = TestDir::new();
            let now = Instant::now();
            let mut first = Recovery::load(root.journal(), false, 1, now);
            assert!(first.restart_application(|| Ok(42)));
            drop(first);
            let recovery = Recovery::load(root.journal(), relaunched, pid, now);
            assert_eq!(recovery.due(now + GRACE * 10), None);
            let text = std::fs::read_to_string(root.journal()).unwrap();
            assert!(text
                .contains("remaining actions skipped for LCDSirPlus and Logitech Gaming Software"));
            assert!(text.contains("no crash recovery"));
            assert!(!text.contains("logitech-attempt\t"));
        }
    }

    #[test]
    fn incomplete_corrupt_repeated_and_oversized_logs_fail_closed() {
        for text in [
            "".to_string(),
            "application-attempt\t1\tinterrupted write".to_string(),
            "logitech-attempt\t1\tno prior application restart\n".to_string(),
            "application-attempt\t1\tonce\napplication-attempt\t2\ttwice\n".to_string(),
            "x".repeat(MAX_JOURNAL_BYTES as usize + 1),
        ] {
            let root = TestDir::new();
            std::fs::write(root.journal(), text).unwrap();
            let now = Instant::now();
            let mut recovery = Recovery::load(root.journal(), false, 1, now);
            assert!(recovery.journal.is_none());
            recovery.healthy(BackendKind::Sdk);
            recovery.disconnected(now, LossCause::DeviceAbsent);
            assert_eq!(recovery.due(now + GRACE * 10), None);
            assert!(!recovery.restart_application(|| panic!("corrupt state launched software")));
        }
    }

    #[test]
    fn replacement_without_receipt_persistently_disables_further_restarts() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), true, 42, now);
        recovery.healthy(BackendKind::Sdk);
        recovery.disconnected(now, LossCause::DeviceAbsent);
        assert_eq!(recovery.due(now + GRACE * 10), None);
        drop(recovery);
        let mut later = Recovery::load(root.journal(), false, 99, now);
        later.healthy(BackendKind::Sdk);
        later.disconnected(now, LossCause::DeviceAbsent);
        assert_eq!(later.due(now + GRACE * 10), None);
        assert_eq!(
            Journal::load(root.journal()).unwrap().stage,
            Stage::Finished
        );
        assert!(std::fs::read_to_string(root.journal())
            .unwrap()
            .contains("no matching recovery action"));
    }

    #[test]
    fn unwritable_or_hardlinked_log_prevents_actions() {
        let root = TestDir::new();
        let now = Instant::now();
        let blocked_parent = root.0.join("not-a-directory");
        std::fs::write(&blocked_parent, "blocked").unwrap();
        let mut recovery = Recovery::load(
            blocked_parent.join("connection-recovery.log"),
            false,
            1,
            now,
        );
        assert!(recovery.journal.is_none());
        assert!(!recovery.restart_application(|| panic!("restart without durable logging")));
        let path = root.journal();
        std::fs::write(&path, "application-attempt\t1\tonce\n").unwrap();
        std::fs::hard_link(&path, root.0.join("alias.log")).unwrap();
        assert!(Journal::load(path).is_err());
    }

    #[test]
    fn logitech_timeout_and_shutdown_cancel_without_repeating_the_action() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        assert!(recovery.restart_application(|| Ok(42)));
        drop(recovery);
        let mut recovery = Recovery::load(root.journal(), true, 42, now);
        assert!(recovery.record("logitech-attempt", "one restart"));
        let (_sender, receiver) = mpsc::sync_channel(1);
        recovery.logitech_result = Some(receiver);
        let permit = Arc::clone(&recovery.permit);
        assert!(!recovery.tick(now + GRACE, true, || panic!(
            "timeout must not request reconnect"
        )));
        assert!(permit.commit(now + GRACE * 2).is_err());
        assert_eq!(recovery.due(now + GRACE * 10), None);
        assert!(!recovery.record("logitech-attempt", "forbidden retry"));
        drop(recovery);
        assert!(std::fs::read_to_string(root.journal())
            .unwrap()
            .contains("restart timed out"));
    }

    #[test]
    fn loss_cause_and_logitech_result_are_written_under_distinct_event_names() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        first.healthy(BackendKind::Sdk);
        first.disconnected(now, LossCause::OpenFailed { code: 0x8007_0002 });
        assert!(first.restart_application(|| Ok(42)));
        drop(first);
        let mut replacement = Recovery::load(root.journal(), true, 42, now);
        assert!(replacement.record("logitech-attempt", "one restart"));
        // Inject a failed worker result; never restart a real Logitech process in tests.
        let (sender, receiver) = mpsc::sync_channel(1);
        replacement.logitech_result = Some(receiver);
        sender.send(Err("injected restart denied".into())).unwrap();
        assert!(!replacement.tick(now + GRACE / 2, true, || panic!(
            "failed restart must not request reconnect"
        )));
        drop(replacement);
        let text = std::fs::read_to_string(root.journal()).unwrap();
        let cause_line = text
            .lines()
            .find(|line| line.contains("open-failed(0x80070002)"))
            .expect("the journal must carry the loss cause code");
        let result_line = text
            .lines()
            .find(|line| line.starts_with("logitech-result\t"))
            .expect("the failed Logitech outcome is its own journal line");
        assert_eq!(
            cause_line.split('\t').next().unwrap(),
            "application-attempt"
        );
        assert_ne!(
            cause_line.split('\t').next().unwrap(),
            result_line.split('\t').next().unwrap(),
            "a loss cause and a recovery failure never share an event name"
        );
        assert!(result_line.contains("injected restart denied"));
    }

    #[test]
    fn no_cause_label_is_published_as_healthy_or_finished() {
        for cause in LossCause::all() {
            let root = TestDir::new();
            let now = Instant::now();
            let mut first = Recovery::load(root.journal(), false, 1, now);
            first.healthy(BackendKind::Sdk);
            first.disconnected(now, cause);
            assert!(first.restart_application(|| Ok(42)));
            drop(first);
            let mut replacement = Recovery::load(root.journal(), true, 42, now);
            replacement.healthy(BackendKind::Sdk); // confirms recovery: "finished"
            drop(replacement);
            let label = cause.short();
            let label = label.split('(').next().unwrap();
            let text = std::fs::read_to_string(root.journal()).unwrap();
            for line in text.lines() {
                let event = line.split('\t').next().unwrap();
                assert_ne!(event, "healthy", "health is not a journal event");
                assert_ne!(
                    event, "disconnected",
                    "the cause rides in detail, not a new event"
                );
                if event == "finished" {
                    assert!(
                        !line.contains(label),
                        "cause label {label} leaked into: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn journal_rejects_a_disconnect_event_shape_for_loss_causes() {
        // Fail-closed guarantee: causes may not invent journal event types.
        let root = TestDir::new();
        let mut journal = Journal::load(root.journal()).unwrap();
        assert!(journal.record("disconnected", "device-absent").is_err());
        assert!(journal.record("cause", "probe-fail").is_err());
        assert!(
            !root.journal().exists(),
            "rejected events must not create journal history"
        );
    }

    /// RC-4 acceptance (1)+(ii): a journal that reaches the 8192-byte cap at
    /// record time publishes its OWN named outcome — the
    /// `connection recovery log is full` cause on the log surface, carried by the
    /// accepted (c) allowance-exhausted label on the device surface (zero new
    /// labels, zero new `DeviceStatus` arms) — and fails closed: `Finished`, the
    /// file untouched at the cap, and no further action ever.
    #[test]
    fn journal_at_cap_publishes_the_named_outcome_and_fails_closed() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        // The legal ladder, then the accepted `logitech-wait-timeout` repeat,
        // every line a real durable append, until the pre-write cap gate fires.
        assert!(recovery.record("application-attempt", &"a".repeat(500)));
        assert!(recovery.record("application-launched", "42"));
        assert!(recovery.record("application-resumed", "relaunched"));
        assert!(recovery.record("logitech-attempt", "one restart"));
        let filler = "t".repeat(500);
        let mut appended = 0;
        while recovery.record("logitech-wait-timeout", &filler) {
            appended += 1;
        }
        assert!(appended > 0, "the ladder keeps appending to the cap");
        // The cap gate named its own condition and it survived the fail-closed
        // transition to `Finished`.
        assert!(
            recovery.journal_full(),
            "the cap gate must set the named condition"
        );
        assert_eq!(
            recovery.journal.as_ref().unwrap().stage,
            Stage::Finished,
            "the cap fails closed"
        );
        // The named cause on the log surface: the very string the cap path hands
        // to `log_error!` through `Recovery::record`, proven at the gate and
        // still firing after a reload, so the file is permanently full.
        let mut reloaded = Journal::load(root.journal()).unwrap();
        let error = reloaded
            .record("logitech-wait-timeout", &filler)
            .expect_err("the cap survives a reload");
        assert!(
            error.contains("connection recovery log is full"),
            "the cap must name its own cause: {error}"
        );
        assert!(error.contains("no further actions"));
        // The named outcome on the device surface.
        assert_eq!(
            recovery.display_status(),
            DeviceStatus::AllowanceConsumed,
            "a full journal must never publish a silent pass"
        );
        assert_eq!(
            recovery.display_status().label(),
            DeviceStatus::LABEL_ALLOWANCE
        );
        // No further action, and the journal is never rewritten.
        let durable = std::fs::read(root.journal()).unwrap();
        assert!(
            durable.len() as u64 <= MAX_JOURNAL_BYTES,
            "the cap is never exceeded"
        );
        assert_eq!(recovery.due(now + GRACE * 10), None);
        assert!(
            !recovery.restart_application(|| panic!("a full journal must not launch")),
            "no launch after the cap"
        );
        assert!(!recovery.record("logitech-wait-timeout", &filler));
        assert!(!recovery.tick(now + GRACE * 10, true, || panic!(
            "a full journal must not request a reconnect"
        )));
        assert_eq!(std::fs::read(root.journal()).unwrap(), durable);
        drop(recovery);
        // Across a restart the named outcome survives: the load path's own
        // fail-closed record hits the same cap gate, so the flag is re-derived
        // there (never durable in the file, which is never rewritten).
        let later = Recovery::load(root.journal(), false, 99, now);
        assert!(
            later.journal_full(),
            "a restart must re-name the cap at the same gate"
        );
        assert_eq!(later.journal.as_ref().unwrap().stage, Stage::Finished);
        assert_eq!(
            later.display_status(),
            DeviceStatus::AllowanceConsumed,
            "the named outcome is published in the next process too"
        );
        assert_eq!(later.due(now + GRACE * 10), None);
        assert_eq!(std::fs::read(root.journal()).unwrap(), durable);
    }

    /// RC-4 R-1 (audit §6 F-1, state S5): when the refused line is long, the
    /// cap fires at a durable length in the 21-byte window [8030, 8050], so the
    /// restart's own load-path fail-closed `finished` line (142 B) still fits.
    /// The guarded file therefore DOES grow after the cap event, landing at
    /// 8172..8192 where no legal line ever fits again. Every fresh process must
    /// name that permanent fullness as (c), never publish a healthy silent pass.
    #[test]
    fn permanently_full_journal_names_itself_after_a_restart() {
        let root = TestDir::new();
        let now = Instant::now();

        // Process A: the legal ladder (532+35+42+40 = 649), 13 accepted
        // `logitech-wait-timeout` repeats (13*534), then a 449-byte
        // `logitech-result` line: 649 + 6942 + 449 = 8040, inside the window,
        // durable stage `LogitechResult` (not `Finished`). Every byte is a real
        // durable append through the one writer, `Journal::record`; the fixture
        // is built on `Journal` (no `Drop`) so no process-exit line is appended.
        let mut writer = Journal::load(root.journal()).unwrap();
        writer
            .record("application-attempt", &"a".repeat(500))
            .unwrap();
        writer.record("application-launched", "42").unwrap();
        writer.record("application-resumed", "relaunched").unwrap();
        writer.record("logitech-attempt", "one restart").unwrap();
        for _ in 0..13 {
            writer
                .record("logitech-wait-timeout", &"t".repeat(500))
                .unwrap();
        }
        writer.record("logitech-result", &"r".repeat(421)).unwrap();
        assert_eq!(writer.stage, Stage::LogitechResult);
        drop(writer);

        let before = std::fs::metadata(root.journal()).unwrap().len();
        assert!(
            (8030..=8050).contains(&before),
            "the fixture must land the durable length in the 21-byte S5 window, got {before}"
        );

        // Process B (restart): the load path's own 142-byte fail-closed line
        // fits (8040 + 142 = 8182 <= 8192), so the file grows; 8182 + 21 = 8203
        // > 8192 means no legal line ever fits again. The permit is fresh, so
        // only the cap derivation can publish (c).
        let restarted = Recovery::load(root.journal(), true, 99, now);
        let after = std::fs::metadata(root.journal()).unwrap().len();
        assert_eq!(
            after - before,
            142,
            "the growth is exactly the load path's fail-closed line"
        );
        assert!(
            after as u64 <= MAX_JOURNAL_BYTES,
            "the 8192-byte cap is respected"
        );
        assert!(
            after as u64 + 21 > MAX_JOURNAL_BYTES,
            "the journal is permanently full: no 21-byte-minimum line fits"
        );
        assert_eq!(
            restarted.journal.as_ref().unwrap().stage,
            Stage::Finished,
            "the load path fails closed"
        );
        assert!(
            !restarted.permit.spent(),
            "(c) here must come from the cap, not from a spent allowance"
        );
        assert!(
            restarted.journal_full(),
            "the process that filled the last byte must derive the cap"
        );
        assert_eq!(
            restarted.display_status(),
            DeviceStatus::AllowanceConsumed,
            "a permanently full journal must name itself after a restart"
        );
        assert_eq!(
            restarted.display_status().label(),
            DeviceStatus::LABEL_ALLOWANCE
        );
        assert_eq!(restarted.due(now + GRACE * 10), None);
        drop(restarted);

        // Every later process (S6): the load derivation names it, and the file
        // is never rewritten.
        let settled = std::fs::read(root.journal()).unwrap();
        let later = Recovery::load(root.journal(), true, 99, now);
        assert!(
            later.journal_full(),
            "a later process names the same condition at load"
        );
        assert_eq!(
            later.display_status(),
            DeviceStatus::AllowanceConsumed,
            "the named outcome is published in every process after the restart"
        );
        assert_eq!(
            std::fs::read(root.journal()).unwrap(),
            settled,
            "no new write site: a full journal records nothing at load"
        );
    }

    /// RC-4 R-1 disclosure (audit §6 S3, F-6): the load-time rule is a
    /// remaining-room rule, so a complete durable-`Finished` journal of 8172
    /// bytes or more names "log full" at load (literally true: no legal line
    /// fits). One byte shorter, 8171, it can still take a 21-byte line, and the
    /// accepted RC-5 silent-pass shape is unchanged.
    #[test]
    fn a_finished_journal_names_log_full_only_once_no_line_fits() {
        let root = TestDir::new();
        let now = Instant::now();

        // Durable `Finished` at 8171 B: 649 + 14*534 = 8125, plus a 46-byte
        // `finished` line. 8171 + 21 = 8192 is not over the cap, so a legal
        // line still fits and the journal is not full.
        let mut ladder = Journal::load(root.journal()).unwrap();
        ladder
            .record("application-attempt", &"a".repeat(500))
            .unwrap();
        ladder.record("application-launched", "42").unwrap();
        ladder.record("application-resumed", "relaunched").unwrap();
        ladder.record("logitech-attempt", "one restart").unwrap();
        for _ in 0..14 {
            ladder
                .record("logitech-wait-timeout", &"t".repeat(500))
                .unwrap();
        }
        ladder.record("finished", &"f".repeat(25)).unwrap();
        assert_eq!(ladder.stage, Stage::Finished);
        drop(ladder);

        let roomy = std::fs::metadata(root.journal()).unwrap().len();
        assert_eq!(roomy, 8171, "one 21-byte line must still fit");
        let open = Recovery::load(root.journal(), true, 99, now);
        assert_eq!(open.journal.as_ref().unwrap().stage, Stage::Finished);
        assert!(
            !open.journal_full(),
            "8171 + 21 = 8192: the cap is not reached"
        );
        assert_eq!(
            open.display_status(),
            DeviceStatus::Healthy,
            "a complete journal with room left keeps the accepted RC-5 shape"
        );
        drop(open);
        assert_eq!(
            std::fs::metadata(root.journal()).unwrap().len(),
            roomy,
            "a complete journal records nothing at load"
        );

        // Same ladder, 36-byte detail: 8125 + 57 = 8182 >= 8172, so no legal
        // line ever fits again and the fresh process names the cap.
        std::fs::remove_file(root.journal()).unwrap();
        let mut filled = Journal::load(root.journal()).unwrap();
        filled
            .record("application-attempt", &"a".repeat(500))
            .unwrap();
        filled.record("application-launched", "42").unwrap();
        filled.record("application-resumed", "relaunched").unwrap();
        filled.record("logitech-attempt", "one restart").unwrap();
        for _ in 0..14 {
            filled
                .record("logitech-wait-timeout", &"t".repeat(500))
                .unwrap();
        }
        filled.record("finished", &"f".repeat(36)).unwrap();
        assert_eq!(filled.stage, Stage::Finished);
        drop(filled);

        let complete = std::fs::metadata(root.journal()).unwrap().len();
        assert!(
            (8172..=8192).contains(&complete),
            "the fixture must land a complete journal at a permanently full length, got {complete}"
        );
        let durable = std::fs::read(root.journal()).unwrap();
        let restarted = Recovery::load(root.journal(), true, 99, now);
        assert_eq!(restarted.journal.as_ref().unwrap().stage, Stage::Finished);
        assert!(
            restarted.journal_full(),
            "a durable-Finished journal with no room left names the cap at load"
        );
        assert_eq!(
            restarted.display_status(),
            DeviceStatus::AllowanceConsumed,
            "S3 at >= 8172 B is disclosed: the journal is literally full, so the label is published"
        );
        assert_eq!(
            std::fs::read(root.journal()).unwrap(),
            durable,
            "no new write site: a complete journal is never rewritten"
        );
    }

    /// RC-4 (d): a long RC-1 cause detail is clamped to 500 characters BEFORE
    /// the line is built, so the cap arithmetic runs on the clamped size, the
    /// detail never breaks the durable three-field shape, and the cause rides in
    /// the accepted `application-attempt` detail with zero new journal event
    /// types.
    #[test]
    fn long_rc1_cause_detail_is_clamped_before_the_cap_and_adds_no_event_type() {
        const EVENTS: [&str; 7] = [
            "application-attempt",
            "application-launched",
            "application-resumed",
            "logitech-attempt",
            "logitech-result",
            "logitech-wait-timeout",
            "finished",
        ];
        let root = TestDir::new();
        let mut journal = Journal::load(root.journal()).unwrap();
        // A hostile RC-1 detail: > 10,000 characters carrying tabs and newlines.
        let cause = format!(
            "loss-cause=worker-stalled {}",
            "nested\tfield\n".repeat(800)
        );
        assert!(cause.chars().count() > 10_000, "the raw cause is huge");
        assert!(journal.record("application-attempt", &cause).is_ok());
        journal.record("application-launched", "42").unwrap();
        journal.record("application-resumed", &cause).unwrap();
        journal.record("logitech-attempt", &cause).unwrap();
        let text = std::fs::read_to_string(root.journal()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in &lines {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            assert_eq!(parts.len(), 3, "the clamp neutralises tabs/newlines");
            assert!(
                parts[2].chars().count() <= 500,
                "the clamp runs before the line is built: {}",
                parts[2].len()
            );
            assert!(EVENTS.contains(&parts[0]), "no new journal event type");
        }
        assert_eq!(lines[0].split('\t').next().unwrap(), "application-attempt");
        assert!(
            lines[0].contains("loss-cause=worker-stalled"),
            "the RC-1 cause channel is preserved"
        );
        // The cap arithmetic saw the clamped bytes: filling to the cap with the
        // same cause lands within ONE clamped line of 8192. An unclamped cause
        // (12,034-byte line) could never satisfy this.
        let mut accepted = 0;
        while journal.record("logitech-wait-timeout", &cause).is_ok() {
            accepted += 1;
        }
        assert!(accepted > 0, "clamped lines keep fitting");
        let len = std::fs::metadata(root.journal()).unwrap().len();
        let clamped_line = "logitech-wait-timeout".len() as u64 + 1 + 10 + 1 + 500 + 1;
        assert!(len <= MAX_JOURNAL_BYTES, "the cap is never exceeded");
        assert!(
            len + clamped_line > MAX_JOURNAL_BYTES,
            "the gate fired on the CLAMPED line size, not the raw cause"
        );
        // The clamped detail never breaks the durable shape: the file replays.
        assert_eq!(
            Journal::load(root.journal()).unwrap().stage,
            Stage::LogitechAttempt
        );
    }

    #[test]
    fn recovered_connection_cancels_pending_logitech_action_and_queued_results_win_over_timeout() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        assert!(recovery.restart_application(|| Ok(42)));
        drop(recovery);
        let mut recovery = Recovery::load(root.journal(), true, 42, now);
        assert!(recovery.record("logitech-attempt", "one restart"));
        let (sender, receiver) = mpsc::sync_channel(1);
        recovery.logitech_result = Some(receiver);
        recovery.healthy(BackendKind::Sdk);
        assert!(recovery.permit.commit(now + GRACE).is_err());
        sender
            .send(Err("connection already recovered; restart cancelled".into()))
            .unwrap();
        assert!(!recovery.tick(now + GRACE * 2, true, || panic!(
            "cancelled restart must not request reconnect"
        )));
        assert_eq!(recovery.journal.as_ref().unwrap().stage, Stage::Finished);
        assert!(!std::fs::read_to_string(root.journal())
            .unwrap()
            .contains("restart timed out"));
    }

    #[test]
    fn commit_and_cancel_have_one_winner_and_shutdown_records_committed_result() {
        let permit = RestartPermit::new();
        assert!(permit.cancel());
        assert!(permit.commit(Instant::now() + GRACE).is_err());
        let permit = RestartPermit::new();
        assert!(permit.commit(Instant::now() + GRACE).is_ok());
        assert!(!permit.cancel());
        assert!(permit.commit(Instant::now() + GRACE).is_err());
        let expired = RestartPermit::new();
        assert!(expired.commit(Instant::now() - GRACE).is_err());

        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        assert!(recovery.restart_application(|| Ok(42)));
        drop(recovery);
        let mut recovery = Recovery::load(root.journal(), true, 42, now);
        assert!(recovery.record("logitech-attempt", "one restart"));
        let (sender, receiver) = mpsc::sync_channel(1);
        recovery.logitech_result = Some(receiver);
        assert!(recovery.permit.commit(now + GRACE).is_ok());
        recovery.suspend();
        assert_eq!(
            recovery.journal.as_ref().unwrap().stage,
            Stage::LogitechAttempt
        );
        sender.send(Ok(())).unwrap();
        drop(recovery);
        let text = std::fs::read_to_string(root.journal()).unwrap();
        assert!(text.contains("restarted once before LCDSirPlus shutdown"));
        assert_eq!(
            Journal::load(root.journal()).unwrap().stage,
            Stage::Finished
        );
    }

    #[test]
    fn missing_persistent_parent_is_created_independently_of_diagnostics_and_keeps_consumed_allowance(
    ) {
        let root = TestDir::new();
        let diagnostic = root.0.join("separate-diagnostics");
        std::fs::create_dir(&diagnostic).unwrap();
        let persistent = root
            .0
            .join("fresh-profile")
            .join("LCDSirPlus")
            .join("connection-recovery.log");
        assert!(!persistent.parent().unwrap().exists());
        let now = Instant::now();
        let mut first = Recovery::load(persistent.clone(), false, 1, now);
        assert!(persistent.parent().unwrap().is_dir());
        assert!(!first.restart_application(|| {
            assert_eq!(
                Journal::load(persistent.clone()).unwrap().stage,
                Stage::ApplicationAttempt
            );
            Err("fixture launch denied".into())
        }));
        drop(first);
        assert!(!diagnostic.join("connection-recovery.log").exists());
        std::fs::remove_dir(&diagnostic).unwrap();
        let mut later = Recovery::load(persistent, false, 2, now);
        later.healthy(BackendKind::Sdk);
        later.disconnected(now, LossCause::DeviceAbsent);
        assert_eq!(later.due(now + GRACE * 10), None);
        assert!(!later.restart_application(|| panic!("diagnostic changes reset allowance")));
    }

    #[test]
    fn successful_logitech_result_requests_reconnect_before_starting_verification_grace() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        assert!(first.restart_application(|| Ok(42)));
        drop(first);
        let mut recovery = Recovery::load(root.journal(), true, 42, now);
        assert!(recovery.record("logitech-attempt", "fixture restart"));
        let (sender, receiver) = mpsc::sync_channel(1);
        recovery.logitech_result = Some(receiver);
        sender.send(Ok(())).unwrap();
        let mut requested_at = None;
        assert!(!recovery.tick(now, true, || {
            assert_eq!(
                Journal::load(root.journal()).unwrap().stage,
                Stage::LogitechResult
            );
            requested_at = Some(Instant::now());
        }));
        assert!(recovery.waiting_since >= requested_at.unwrap());
        let grace_start = recovery.waiting_since;
        assert_eq!(
            recovery.due(grace_start + GRACE - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            recovery.due(grace_start + GRACE),
            Some(Stage::LogitechResult)
        );
        assert!(!recovery.tick(grace_start + GRACE, true, || panic!(
            "second reconnect request"
        )));
        assert_eq!(recovery.journal.as_ref().unwrap().stage, Stage::Finished);
    }

    #[test]
    fn committed_restart_invalidates_cached_health_and_waits_for_fresh_reconnect_health() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        assert!(first.restart_application(|| Ok(42)));
        drop(first);
        let mut recovery = Recovery::load(root.journal(), true, 42, now);
        assert!(recovery.record("logitech-attempt", "fixture restart"));
        let (sender, receiver) = mpsc::sync_channel(1);
        recovery.logitech_result = Some(receiver);
        assert!(recovery.permit.commit(now + GRACE).is_ok());
        recovery.healthy(BackendKind::Sdk);
        assert!(
            recovery.permit.committed(),
            "health cannot cancel a committed restart"
        );
        sender.send(Ok(())).unwrap();
        let mut requested = false;
        assert!(!recovery.tick(now, true, || requested = true));
        assert!(
            requested,
            "successful restart must reconnect despite cached health"
        );
        assert_eq!(
            recovery.journal.as_ref().unwrap().stage,
            Stage::LogitechResult
        );
        assert!(
            !recovery.previously_connected,
            "pre-restart health must be invalidated"
        );
        let grace_start = recovery.waiting_since;
        assert!(!recovery.tick(grace_start + GRACE / 2, true, || panic!(
            "second reconnect request"
        )));
        assert_eq!(
            recovery.journal.as_ref().unwrap().stage,
            Stage::LogitechResult
        );
        // An old device may finish a poll and queue health before it handles the wake.
        recovery.healthy(BackendKind::Sdk);
        assert_eq!(
            recovery.journal.as_ref().unwrap().stage,
            Stage::LogitechResult
        );
        assert!(!recovery.previously_connected);
        recovery.reconnect_started();
        assert_eq!(
            recovery.journal.as_ref().unwrap().stage,
            Stage::LogitechResult
        );
        recovery.healthy(BackendKind::Sdk);
        assert_eq!(recovery.journal.as_ref().unwrap().stage, Stage::Finished);
        assert!(recovery.previously_connected);
        assert_eq!(recovery.due(grace_start + GRACE * 10), None);
    }

    // RC-2 seams: injected silence on the health path while the submit path
    // keeps running (the dashboard loop calls `tick` as it submits frames),
    // driven entirely by injected `Instant` values. No sleeps, no threads.
    fn claim(recovery: &mut Recovery, now: Instant, kind: BackendKind) {
        recovery.note_liveness(now, &BackendState::Connected { kind });
    }

    #[test]
    fn injected_worker_silence_journals_one_stall_cause_and_requests_exactly_one_reconnect() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        recovery.healthy(BackendKind::Sdk);
        claim(&mut recovery, now, BackendKind::Sdk);
        let deadline = now + recovery.stall_deadline;
        assert!(
            deadline > now + DEFAULT_RECONNECT_MAX,
            "the liveness deadline must exceed the capped reconnect ladder"
        );
        let mut wakes = 0;
        assert!(
            !recovery.tick(deadline - Duration::from_millis(1), true, || {
                panic!("no wake before the liveness deadline")
            })
        );
        assert!(!recovery.tick(deadline, true, || wakes += 1));
        assert_eq!(wakes, 1, "exactly one reconnect wake for a stalled worker");
        assert_eq!(recovery.last_cause, Some(LossCause::WorkerStalled));
        assert!(recovery.stall_actioned, "one-shot per segment");
        assert_eq!(recovery.lost_since, Some(deadline), "the stall is a loss");
        // The named cause reaches the append-only journal in the detail of the
        // next legitimate recovery event (RC-1 mechanism, no new event types).
        assert!(recovery.restart_application(|| Ok(42)));
        let text = std::fs::read_to_string(root.journal()).unwrap();
        assert_eq!(
            text.matches("worker-stalled").count(),
            1,
            "the stall cause is journaled exactly once"
        );
        assert!(text.contains("loss-cause=worker-stalled"));
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("application-attempt\t"))
                .count(),
            1
        );
    }

    #[test]
    fn second_consecutive_stall_without_health_takes_no_further_action() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        recovery.healthy(BackendKind::Sdk);
        claim(&mut recovery, now, BackendKind::Sdk);
        let deadline = now + recovery.stall_deadline;
        let mut wakes = 0;
        assert!(!recovery.tick(deadline, true, || wakes += 1));
        assert_eq!(wakes, 1);
        // Second consecutive stall in the same segment, still no health: the
        // one-shot is spent and the wake seam panics on a repeat. Offsets stay
        // inside `GRACE` of the stall so the tick never reaches the app-restart
        // stage, which would enumerate real HID devices.
        for offset in [Duration::from_secs(1), GRACE / 2] {
            assert!(!recovery.tick(deadline + offset, true, || {
                panic!("a stalled worker is never woken twice without health")
            }));
        }
        assert_eq!(
            recovery.last_cause,
            Some(LossCause::WorkerStalled),
            "the named cause is retained, not swallowed"
        );
        assert!(recovery.restart_application(|| Ok(42)));
        assert!(!recovery.restart_application(|| panic!("application restarted twice")));
        let text = std::fs::read_to_string(root.journal()).unwrap();
        assert_eq!(
            text.matches("worker-stalled").count(),
            1,
            "repeat stalls add no further journal causes"
        );
    }

    #[test]
    fn the_stall_wake_is_suppressed_during_backoff_absent_and_an_awaited_reconnect() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, now);
        claim(&mut recovery, now, BackendKind::Hid);
        for state in [
            BackendState::Disconnected {
                cause: LossCause::OpenFailed { code: 0 },
                reason: "injected backoff".into(),
            },
            BackendState::Absent {
                reason: "injected ladder terminal".into(),
            },
            BackendState::Discovering,
            BackendState::Connected {
                kind: BackendKind::Virtual,
            },
            BackendState::ShutDown,
        ] {
            recovery.note_liveness(now + Duration::from_secs(1), &state);
            assert!(!recovery.tick(now + recovery.stall_deadline * 2, true, || {
                panic!("health is legitimately absent here, so nothing is stalled")
            }));
        }
        // A reconnect wake that is already in flight owns the rebuild.
        claim(&mut recovery, now, BackendKind::Sdk);
        recovery.awaiting_reconnect = true;
        assert!(!recovery.tick(now + recovery.stall_deadline, true, || {
            panic!("no second wake while a reconnect is awaited")
        }));
        // The worker's own cycle start re-baselines the clock at its next claim.
        recovery.reconnect_started();
        assert!(!recovery.tick(now + recovery.stall_deadline * 2, true, || {
            panic!("no wake before the worker claims a device again")
        }));
        let arm = now + recovery.stall_deadline;
        claim(&mut recovery, arm, BackendKind::Sdk);
        let mut wakes = 0;
        assert!(!recovery.tick(now + recovery.stall_deadline * 2, true, || wakes += 1));
        assert_eq!(wakes, 1, "a fresh claim after the wake is watchable again");
    }

    const FIXTURE_MODE: &str = "LCDSIRPLUS_TEST_HANDOFF_MODE";
    const FIXTURE_STOP: &str = "LCDSIRPLUS_TEST_HANDOFF_STOP";
    const FIXTURE_CRASH: &str = "LCDSIRPLUS_TEST_HANDOFF_CRASH";

    fn fixture_command(mode: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "recovery::tests::native_handoff_fixture",
                "--nocapture",
            ])
            .env(FIXTURE_MODE, mode);
        command
    }

    // Own only fixture children; even assertion failures clean up these processes.
    struct FixtureChild(std::process::Child);

    impl Drop for FixtureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn native_handoff_fixture() {
        let Ok(mode) = std::env::var(FIXTURE_MODE) else {
            return;
        };
        match mode.as_str() {
            "parent" => {
                let name = std::env::var(FIXTURE_STOP).unwrap();
                let mutex_name: Vec<_> = format!("{name}.Mutex")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let _instance = NativeHandle(
                    unsafe {
                        windows::Win32::System::Threading::CreateMutexW(
                            None,
                            false,
                            PCWSTR(mutex_name.as_ptr()),
                        )
                    }
                    .unwrap(),
                );
                let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
                let stop = NativeHandle(
                    unsafe {
                        OpenEventW(
                            windows::Win32::System::Threading::EVENT_ALL_ACCESS,
                            false,
                            PCWSTR(wide.as_ptr()),
                        )
                    }
                    .unwrap(),
                );
                assert_eq!(
                    unsafe { WaitForSingleObject(stop.0, 15_000) },
                    WAIT_OBJECT_0
                );
                if std::env::var(FIXTURE_CRASH).unwrap() == "true" {
                    std::process::exit(37);
                }
            }
            "child" => {
                // Reproduce delayed child scheduling: the old parent must stay alive.
                std::thread::sleep(Duration::from_millis(200));
                let result = wait_for_parent();
                if std::env::var(FIXTURE_CRASH).unwrap() == "true" {
                    assert!(result.unwrap_err().contains("did not exit normally"));
                } else {
                    assert!(result.unwrap());
                    let name = std::env::var(FIXTURE_STOP).unwrap();
                    let mutex_name: Vec<_> = format!("{name}.Mutex")
                        .encode_utf16()
                        .chain(Some(0))
                        .collect();
                    let _instance = NativeHandle(
                        unsafe {
                            windows::Win32::System::Threading::CreateMutexW(
                                None,
                                false,
                                PCWSTR(mutex_name.as_ptr()),
                            )
                        }
                        .unwrap(),
                    );
                    assert_ne!(
                        unsafe { GetLastError() },
                        ERROR_ALREADY_EXISTS,
                        "parent instance mutex was not released"
                    );
                }
                println!("native handoff verified");
            }
            "unready" => {} // Exit before acquiring the parent's process handle.
            _ => panic!("unknown fixture mode"),
        }
    }

    #[test]
    fn native_delayed_child_handoff_holds_parent_until_ready_and_rejects_crash() {
        for crash in [false, true] {
            let nonce = unsafe { windows::Win32::System::Com::CoCreateGuid() }.unwrap();
            let name = format!("Local\\LCDSirPlus.RecoveryFixtureStop.{nonce:?}");
            let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
            let stop = NativeHandle(
                unsafe { CreateEventW(None, true, false, PCWSTR(wide.as_ptr())) }.unwrap(),
            );
            let mut parent = FixtureChild(
                fixture_command("parent")
                    .env(FIXTURE_STOP, &name)
                    .env(FIXTURE_CRASH, crash.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let created = process_creation_time(HANDLE(parent.0.as_raw_handle())).unwrap();
            let started = Instant::now();
            let mut child = FixtureChild(
                launch_ready_child(
                    fixture_command("child")
                        .env(FIXTURE_STOP, &name)
                        .env(FIXTURE_CRASH, crash.to_string())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped()),
                    parent.0.id(),
                    created,
                )
                .unwrap(),
            );
            assert!(
                started.elapsed() >= Duration::from_millis(200),
                "launch returned before child readiness"
            );
            assert!(
                parent.0.try_wait().unwrap().is_none(),
                "parent exited before acquiring its handle"
            );
            // Immediate graceful exit after acknowledgement used to race OpenProcess.
            unsafe { SetEvent(stop.0) }.unwrap();
            let status = parent.0.wait().unwrap();
            assert_eq!(status.code(), Some(if crash { 37 } else { 0 }));
            let status = child.0.wait().unwrap();
            let mut output = String::new();
            child
                .0
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut output)
                .unwrap();
            let mut errors = String::new();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut errors)
                .unwrap();
            assert!(status.success(), "fixture failed: {output}\n{errors}");
            assert!(output.contains("native handoff verified"));
        }
    }

    #[test]
    fn native_child_exit_without_readiness_keeps_original_running() {
        let result = launch_ready_child(
            &mut fixture_command("unready"),
            std::process::id(),
            process_creation_time(unsafe { GetCurrentProcess() }).unwrap(),
        );
        assert!(result.unwrap_err().contains("original retained"));
    }
    #[test]
    fn silence_after_established_health_is_detected_and_progress_rearms_the_clock() {
        let root = TestDir::new();
        let start = Instant::now();
        let mut recovery = Recovery::load(root.journal(), false, 1, start);
        claim(&mut recovery, start, BackendKind::Sdk);
        let first = start + Duration::from_secs(1);
        recovery.healthy_at(BackendKind::Sdk, first);
        let next = first + recovery.stall_deadline - Duration::from_secs(1);
        assert!(!recovery.stall_due(next));
        recovery.healthy_at(BackendKind::Sdk, next);
        let deadline = next + recovery.stall_deadline;
        assert!(!recovery.stall_due(deadline - Duration::from_millis(1)));
        assert!(recovery.stall_due(deadline));
        let mut wakes = 0;
        assert!(!recovery.tick(deadline, true, || wakes += 1));
        assert!(!recovery.tick(deadline + Duration::from_secs(1), true, || wakes += 1));
        assert_eq!(wakes, 1);
        recovery.note_liveness(deadline, &BackendState::Discovering);
        assert!(!recovery.stall_due(deadline + recovery.stall_deadline * 2));
    }
}
