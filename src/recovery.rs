//! One-shot recovery of a previously working physical LCD connection.
//! The append-only journal is both the audit log and the durable restart budget.

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

use crate::backends::BackendKind;

const PARENT_ENV: &str = "LCDSIRPLUS_CONNECTION_RECOVERY_PARENT";
const READY_TIMEOUT_MS: u32 = 10_000;
const GRACE: Duration = Duration::from_secs(30);
const MAX_JOURNAL_BYTES: u64 = 8192;

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
                return Err(std::io::Error::other("connection recovery log is full"));
            }
            file.write_all(line.as_bytes())?;
            file.sync_all()
        })();
        if let Err(error) = result {
            self.stage = Stage::Finished;
            return Err(format!(
                "connection recovery log write failed; no further actions: {error}"
            ));
        }
        self.stage = next;
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
    logitech_result: Option<Receiver<Result<(), String>>>,
    permit: Arc<RestartPermit>,
    interrupted: bool,
    worker_timed_out: bool,
    awaiting_reconnect: bool,
}

impl Recovery {
    pub fn start(handoff: Handoff) -> Self {
        let mut recovery = Self::load(
            journal_path(),
            handoff == Handoff::Verified,
            std::process::id(),
            Instant::now(),
        );
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
            logitech_result: None,
            permit: Arc::new(RestartPermit::new()),
            interrupted: false,
            worker_timed_out: false,
            awaiting_reconnect: false,
        }
    }

    pub fn healthy(&mut self, kind: BackendKind) {
        if self.awaiting_reconnect || !matches!(kind, BackendKind::Hid | BackendKind::Sdk) {
            return;
        }
        self.previously_connected = true;
        self.lost_since = None;
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

    pub fn disconnected(&mut self, now: Instant) {
        if self.previously_connected {
            self.lost_since.get_or_insert(now);
        }
    }

    pub fn reconnect_started(&mut self) {
        // The single backend worker queues this before health from its new device.
        self.awaiting_reconnect = false;
    }

    pub fn suspend(&mut self) {
        self.permit.cancel();
        self.previously_connected = false;
        self.lost_since = None;
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
        if !self.record(
            "application-attempt",
            "LCDSirPlus lost a previously working physical connection; restart once",
        ) {
            return false;
        }
        match launch() {
            Ok(pid) => self.record("application-launched", &pid.to_string()),
            Err(error) => {
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

    /// Called only by the responsive dashboard loop. True requests normal app shutdown.
    pub fn tick(&mut self, now: Instant, enabled: bool, request_reconnect: impl FnOnce()) -> bool {
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
                            request_reconnect();
                            // Grace begins after the retry request, not before LCore restarts.
                            self.waiting_since = Instant::now();
                        } else if self.previously_connected && self.lost_since.is_none() {
                            self.finish("physical LCD connection recovered; restart allowance remains consumed");
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.logitech_result = None;
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
                    Err(error) => self.finish(&format!("Logitech Gaming Software restart worker could not start: {error}; no retry")),
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
        recovery.disconnected(now);
        recovery.healthy(BackendKind::Virtual);
        recovery.disconnected(now + GRACE);
        assert_eq!(recovery.due(now + GRACE * 10), None);
        assert!(
            !root.journal().exists(),
            "startup does not consume or create the log"
        );

        recovery.healthy(BackendKind::Sdk);
        recovery.disconnected(now);
        recovery.disconnected(now + GRACE / 2);
        assert_eq!(recovery.due(now + GRACE - Duration::from_millis(1)), None);
        assert_eq!(recovery.due(now + GRACE), Some(Stage::Fresh));
        recovery.healthy(BackendKind::Sdk);
        assert_eq!(recovery.due(now + GRACE * 10), None);
        recovery.disconnected(now + GRACE * 10);
        recovery.suspend();
        assert_eq!(recovery.due(now + GRACE * 20), None);
    }

    #[test]
    fn each_restart_is_persisted_before_action_and_never_repeated_across_launches() {
        let root = TestDir::new();
        let now = Instant::now();
        let mut first = Recovery::load(root.journal(), false, 1, now);
        first.healthy(BackendKind::Sdk);
        first.disconnected(now);
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
        later.disconnected(now);
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
        replacement.disconnected(now);
        assert_eq!(replacement.due(now + GRACE * 10), None);
        assert!(!replacement.record("logitech-attempt", "forbidden after success"));
        drop(replacement);
        assert_eq!(
            Journal::load(root.journal()).unwrap().stage,
            Stage::Finished
        );
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
            recovery.disconnected(now);
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
        recovery.disconnected(now);
        assert_eq!(recovery.due(now + GRACE * 10), None);
        drop(recovery);
        let mut later = Recovery::load(root.journal(), false, 99, now);
        later.healthy(BackendKind::Sdk);
        later.disconnected(now);
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
        later.disconnected(now);
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
}
