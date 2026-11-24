//! PresentMon console discovery, target selection, owned capture, and CSV statistics.

#![cfg(windows)]

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND, FILETIME, HANDLE,
    MAX_PATH, WAIT_TIMEOUT,
};
use windows::Win32::System::Com::{CoCreateGuid, CoTaskMemFree};
use windows::Win32::System::Diagnostics::Etw::{
    ControlTraceW, CONTROLTRACE_HANDLE, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_PROPERTIES,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::Config;
use crate::history::{mean, tail_means};
use crate::json::Json;
use crate::model::{GameStats, Metric};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STALE_AFTER: Duration = Duration::from_secs(5);
const TARGET_POLL: Duration = Duration::from_secs(1);
const ACTIVITY_WINDOW: Duration = Duration::from_secs(3);
const ACTIVITY_EXPIRY: Duration = Duration::from_secs(10);
const MAX_CANDIDATES: usize = 64;
const MAX_ACTIVITY_BUCKETS: usize = 64;
const MAX_IDENTITY_QUERIES_PER_TICK: usize = 64;
const WORKLOAD_BUCKETS: usize = 3;
const WORKLOAD_VALIDITY: f64 = 0.95;
const SWITCH_MARGIN: f64 = 0.15;
const SWITCH_WINDOWS: u8 = 2;
const CATALOG_REFRESH: Duration = Duration::from_secs(5);
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_APPLICATIONS: usize = 4096;
const MAX_CATALOG_PATHS: usize = 64;
const MAX_CATALOG_PATH_BYTES: usize = 32 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const LINE_QUEUE_CAPACITY: usize = 2048;
const MAX_LINES_PER_TICK: usize = 512;
// At most 128 MiB of CSV payload can wait in the bounded channel, below the prior 512 MiB bound.
const _: () = assert!(LINE_QUEUE_CAPACITY * MAX_LINE_BYTES == 128 * 1024 * 1024);
// 600 seconds at 1000 FPS plus the full queued backlog draining at one observed timestamp.
const MAX_WINDOW_FRAMES: usize = 600 * 1_000 + LINE_QUEUE_CAPACITY;
const _: () = assert!(MAX_WINDOW_FRAMES == 602_048);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_DELAY: Duration = Duration::from_secs(10);
/// WO-OPT02C (finding O1, slice 3): the bound of a worker park that has nothing to
/// wait for, named from the shipped target-poll cadence (`TARGET_POLL`), not a magic
/// number. It is a WAKE bound, never an adoption bound: the demand edge
/// (`telemetry.rs:121-125` stores the word, then `wake_workers`), the config edge
/// (`telemetry.rs:99-106`) and the shutdown edge (`telemetry.rs:88-96`, store ->
/// unpark -> join) each `Thread::unpark` this worker's handle, which `telemetry.rs:2133-2149`
/// owns in `Telemetry::threads`, so a publish lands without waiting for the park.
const PRESENTMON_IDLE_BOUND: Duration = TARGET_POLL;
/// WO-OPT02C: the floor of every presentmon worker park. The host timer resolution is
/// ~15.6 ms, so a park shorter than this is a spin. Local constant mirroring
/// `telemetry.rs::LOOP_MIN_PARK`; the value is deliberately not imported across files.
const WORKER_MIN_PARK: Duration = Duration::from_millis(2);
/// WO-OPT02C-PUMPCAP (F-AUD-OPT02C-1): the longest step of the live-capture pump.
/// WO-OPT02C-PUMPCAP2 (S14): the pump step IS this constant, not a clamp of the
/// deadline. Two measured facts fix that shape: a `min` cap alone leaves 64.2
/// iterations/s in the past-`STALE_AFTER` shape, because `worker_idle_deadline`
/// collapses a deadline set whose gates are already due to `WORKER_MIN_PARK` (2 ms)
/// and a `min` cannot lift a collapsed set - the 50 ms step is what bounds that spin
/// (F-AUD-OPT02C-2, measured 16.1/s); and `Thread::unpark` cannot interrupt a running
/// `recv_timeout`, so in the running-capture shape this timeout IS the demand-adoption
/// bound (100 ms app publish cadence + this step = the shipped 150 ms promise).
/// Parity: the live-capture pump step is the shipped 50 ms step - the retired arm's
/// `sleep(Duration::from_millis(50))` - only the no-capture branch parks to the
/// deadline, and the running-capture shape gains NO wake-count reduction: its win is
/// `sleep` -> data-driven pump plus the shipped `line_overflow` reset reachability.
const PRESENTMON_PUMP_BOUND: Duration = Duration::from_millis(50);
const STOP_TIMEOUT_MS: u32 = 2_000;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 1024;
const PRESENTMON_SHA256: &str = "9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191";
const PRESENTMON_SIZE: u64 = 956_768;
const PRESENTMON_SIGNER: &str = "Intel Corporation";

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub creation_time: u64,
    pub name: String,
    image_path: String,
}

impl fmt::Debug for ProcessInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessInfo")
            .field("pid", &self.pid)
            .field("creation_time", &self.creation_time)
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub generation: u64,
    pub game: GameStats,
    pub available: bool,
    pub error: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
struct Frame {
    at: SystemTime,
    observed_at: Instant,
    application: String,
    pid: u32,
    frame_ms: f64,
    cpu_start_ms: Option<f64>,
    cpu_busy_ms: Option<f64>,
    gpu_busy_ms: Option<f64>,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            at: SystemTime::UNIX_EPOCH,
            observed_at: Instant::now(),
            application: String::new(),
            pid: 0,
            frame_ms: 0.0,
            cpu_start_ms: None,
            cpu_busy_ms: None,
            gpu_busy_ms: None,
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CandidateKey {
    pid: u32,
    creation_time: u64,
    application: String,
    image_path: String,
}

impl fmt::Debug for CandidateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CandidateKey")
            .field("pid", &self.pid)
            .field("creation_time", &self.creation_time)
            .field("application", &self.application)
            .finish()
    }
}

impl CandidateKey {
    fn new(process: &ProcessInfo) -> Self {
        Self {
            pid: process.pid,
            creation_time: process.creation_time,
            application: process.name.to_ascii_lowercase(),
            image_path: process.image_path.to_ascii_lowercase(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct StreamKey {
    pid: u32,
    application: String,
}

impl StreamKey {
    fn new(pid: u32, application: &str) -> Self {
        Self {
            pid,
            application: application.to_ascii_lowercase(),
        }
    }
}

struct ProcessHandle(HANDLE);

impl ProcessHandle {
    fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct RecentActivity {
    process: ProcessInfo,
    handle: Option<ProcessHandle>,
    buckets: VecDeque<ActivityBucket>,
    last_seen: Option<Instant>,
}

#[derive(Clone, Debug)]
struct ActivityBucket {
    observed_at: Instant,
    trace_second: Option<u64>,
    rows: u32,
    frame_ms: f64,
    cpu_busy_ms: f64,
    cpu_valid_rows: u32,
    cpu_valid_frame_ms: f64,
    gpu_busy_ms: f64,
    gpu_valid_rows: u32,
    gpu_valid_frame_ms: f64,
}

#[derive(Clone, Copy, Debug)]
struct WorkloadScore {
    value: f64,
    window: u64,
}

#[derive(Clone, Debug)]
struct RankedCandidate {
    process: ProcessInfo,
    workload: Option<WorkloadScore>,
}

/// WO-OPT06 (A2): the only foreground input the ranking decision reads. `pid` is the value the
/// shipped `foreground_process()` chain left behind on the ranking path: the real pid when the
/// full identity query succeeded, and `0` - "no foreground preference" - on every path where that
/// chain returned `Err` or a default (A1). It carries no identity, so it can never be used where
/// a pid + creation-time identity is validated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ForegroundInfo {
    pid: u32,
}

#[derive(Clone, Debug)]
struct ChallengerState {
    key: CandidateKey,
    window: u64,
    wins: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogClassification {
    Unavailable,
    Game,
    NotGame,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CatalogUnavailableReason {
    #[default]
    NotChecked,
    PathUnavailable,
    FileUnavailable,
    UnsafeFile,
    InputOversized,
    ReadFailed,
    MetadataChanged,
    InvalidUtf8,
    InvalidJson,
    RootShape,
    RecordShape,
    ApplicationCount,
    PathCount,
}

impl CatalogUnavailableReason {
    fn code(self) -> &'static str {
        match self {
            Self::NotChecked => "not_checked",
            Self::PathUnavailable => "path_unavailable",
            Self::FileUnavailable => "file_unavailable",
            Self::UnsafeFile => "unsafe_file",
            Self::InputOversized => "input_oversized",
            Self::ReadFailed => "read_failed",
            Self::MetadataChanged => "metadata_changed",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::InvalidJson => "invalid_json",
            Self::RootShape => "root_shape",
            Self::RecordShape => "record_shape",
            Self::ApplicationCount => "application_count",
            Self::PathCount => "path_count",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogDiagnostic {
    Available(usize),
    Unavailable(CatalogUnavailableReason),
}

impl CatalogDiagnostic {
    fn message(self) -> String {
        match self {
            Self::Available(count) => format!("PresentMon catalog available qualified={count}"),
            Self::Unavailable(reason) => {
                format!("PresentMon catalog unavailable reason={}", reason.code())
            }
        }
    }
}

#[derive(Clone, Default)]
enum CatalogState {
    #[default]
    Unavailable,
    Available(Vec<String>),
}

static UNAVAILABLE_CATALOG: CatalogState = CatalogState::Unavailable;

fn selection_catalog<'a>(cfg: &Config, catalog: &'a CatalogState) -> &'a CatalogState {
    if cfg.presentmon_persist && cfg.presentmon_target_mode == "presenting" {
        &UNAVAILABLE_CATALOG
    } else {
        catalog
    }
}

impl CatalogState {
    fn classify(&self, process: &ProcessInfo) -> CatalogClassification {
        let CatalogState::Available(paths) = self else {
            return CatalogClassification::Unavailable;
        };
        let Some(path) = normalize_catalog_path(&process.image_path) else {
            return CatalogClassification::NotGame;
        };
        if paths.binary_search(&path).is_ok() {
            CatalogClassification::Game
        } else {
            CatalogClassification::NotGame
        }
    }

    fn allows(&self, process: &ProcessInfo) -> bool {
        self.classify(process) != CatalogClassification::NotGame
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CatalogMetadata {
    volume: u32,
    index: u64,
    size: u64,
    modified: u64,
}

struct CatalogLoad {
    state: CatalogState,
    metadata: Option<CatalogMetadata>,
    reason: CatalogUnavailableReason,
}

#[derive(Default)]
struct CatalogCache {
    state: CatalogState,
    metadata: Option<CatalogMetadata>,
    checked_at: Option<Instant>,
    reason: CatalogUnavailableReason,
    last_diagnostic: Option<CatalogDiagnostic>,
}

impl CatalogCache {
    fn refresh(&mut self, now: Instant) {
        if self.checked_at.is_some_and(|checked| {
            now.checked_duration_since(checked)
                .is_none_or(|elapsed| elapsed < CATALOG_REFRESH)
        }) {
            return;
        }
        self.checked_at = Some(now);
        let Some(path) = nvidia_catalog_path() else {
            self.state = CatalogState::Unavailable;
            self.metadata = None;
            self.reason = CatalogUnavailableReason::PathUnavailable;
            self.report();
            return;
        };
        if catalog_file(&path)
            .map(|(_, metadata)| metadata)
            .is_ok_and(|metadata| Some(metadata) == self.metadata)
        {
            return;
        }
        let loaded = load_catalog_with(|| read_catalog_attempt(&path));
        self.state = loaded.state;
        self.metadata = loaded.metadata;
        self.reason = loaded.reason;
        self.report();
    }

    fn report(&mut self) {
        let diagnostic = match &self.state {
            CatalogState::Available(paths) => CatalogDiagnostic::Available(paths.len()),
            CatalogState::Unavailable => CatalogDiagnostic::Unavailable(self.reason),
        };
        if self.last_diagnostic != Some(diagnostic) {
            crate::log_debug!("{}", diagnostic.message());
            self.last_diagnostic = Some(diagnostic);
        }
    }
}

fn load_catalog_with(
    mut attempt: impl FnMut() -> Result<
        (CatalogMetadata, Vec<u8>, CatalogMetadata),
        CatalogUnavailableReason,
    >,
) -> CatalogLoad {
    for retry in 0..=1 {
        let (before, bytes, after) = match attempt() {
            Ok(attempt) => attempt,
            Err(reason) => {
                return CatalogLoad {
                    state: CatalogState::Unavailable,
                    metadata: None,
                    reason,
                }
            }
        };
        if before != after {
            if retry == 0 {
                continue;
            }
            return CatalogLoad {
                state: CatalogState::Unavailable,
                metadata: None,
                reason: CatalogUnavailableReason::MetadataChanged,
            };
        }
        return match parse_catalog(&bytes) {
            Ok(paths) => CatalogLoad {
                state: CatalogState::Available(paths),
                metadata: Some(after),
                reason: CatalogUnavailableReason::NotChecked,
            },
            Err(reason) => CatalogLoad {
                state: CatalogState::Unavailable,
                metadata: Some(after),
                reason,
            },
        };
    }
    unreachable!()
}

fn parse_catalog(bytes: &[u8]) -> Result<Vec<String>, CatalogUnavailableReason> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CatalogUnavailableReason::InputOversized);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| CatalogUnavailableReason::InvalidUtf8)?;
    let document = Json::parse(text).map_err(|_| CatalogUnavailableReason::InvalidJson)?;
    let Json::Obj(root) = &document else {
        return Err(CatalogUnavailableReason::RootShape);
    };
    let applications = catalog_field(root, "Applications")
        .and_then(Json::as_arr)
        .ok_or(CatalogUnavailableReason::RootShape)?;
    if applications.len() > MAX_CATALOG_APPLICATIONS {
        return Err(CatalogUnavailableReason::ApplicationCount);
    }
    let mut qualified = Vec::new();
    let mut recognized = false;
    for application in applications {
        let Json::Obj(fields) = application else {
            continue;
        };
        let Some(fields) = catalog_application_fields(fields) else {
            continue;
        };
        let Some(detected) = catalog_field(fields, "DetectedFiles").and_then(Json::as_arr) else {
            continue;
        };
        if detected.len() > MAX_CATALOG_PATHS {
            return Err(CatalogUnavailableReason::PathCount);
        }
        let Some(creative) = catalog_field(fields, "IsCreativeApplication").and_then(Json::as_bool)
        else {
            continue;
        };
        let Some(ops) = catalog_field(fields, "IsOpsSupported").and_then(Json::as_bool) else {
            continue;
        };
        let Some(_) = catalog_field(fields, "IsFingerprintDetected").and_then(Json::as_bool) else {
            continue;
        };
        let Some(manual) = catalog_field(fields, "IsManuallyAdded").and_then(Json::as_bool) else {
            continue;
        };
        if detected.iter().any(|path| path.as_str().is_none()) {
            continue;
        }
        recognized = true;
        for path in detected {
            let path = path.as_str().expect("validated catalog path type");
            if !creative && ops && !manual {
                if let Some(path) = normalize_catalog_path(path) {
                    qualified.push(path);
                }
            }
        }
    }
    if !recognized {
        return Err(CatalogUnavailableReason::RecordShape);
    }
    qualified.sort_unstable();
    qualified.dedup();
    Ok(qualified)
}

fn catalog_application_fields(fields: &[(String, Json)]) -> Option<&[(String, Json)]> {
    const REQUIRED: [&str; 5] = [
        "DetectedFiles",
        "IsCreativeApplication",
        "IsOpsSupported",
        "IsFingerprintDetected",
        "IsManuallyAdded",
    ];
    let has_semantic_field = |fields: &[(String, Json)]| {
        fields
            .iter()
            .any(|(key, _)| REQUIRED.contains(&key.as_str()))
    };
    let mut candidates = std::iter::once(fields)
        .chain(fields.iter().filter_map(|(_, value)| match value {
            Json::Obj(fields) => Some(fields.as_slice()),
            _ => None,
        }))
        .filter(|fields| has_semantic_field(fields));
    let fields = candidates.next()?;
    candidates.next().is_none().then_some(fields)
}

fn catalog_field<'a>(fields: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    let mut matches = fields.iter().filter(|(key, _)| key == name);
    let value = &matches.next()?.1;
    matches.next().is_none().then_some(value)
}

fn normalize_catalog_path(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_CATALOG_PATH_BYTES || text.chars().any(char::is_control)
    {
        return None;
    }
    let mut components = Path::new(text).components();
    let letter = match components.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) => letter.to_ascii_lowercase(),
            _ => return None,
        },
        _ => return None,
    };
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    let mut normalized = format!("{}:\\", letter as char);
    let mut count = 0;
    for component in components {
        let Component::Normal(component) = component else {
            return None;
        };
        let component = component.to_str()?;
        if component.is_empty()
            || component.contains(':')
            || component.ends_with(['.', ' '])
            || component.chars().any(char::is_control)
        {
            return None;
        }
        if count != 0 {
            normalized.push('\\');
        }
        normalized.push_str(&component.to_lowercase());
        count += 1;
    }
    (count != 0).then_some(normalized)
}

impl ActivityBucket {
    fn new(frame: &Frame, trace_second: Option<u64>) -> Self {
        let mut bucket = Self {
            observed_at: frame.observed_at,
            trace_second,
            rows: 0,
            frame_ms: 0.0,
            cpu_busy_ms: 0.0,
            cpu_valid_rows: 0,
            cpu_valid_frame_ms: 0.0,
            gpu_busy_ms: 0.0,
            gpu_valid_rows: 0,
            gpu_valid_frame_ms: 0.0,
        };
        bucket.record(frame);
        bucket
    }

    fn record(&mut self, frame: &Frame) {
        self.observed_at = frame.observed_at;
        self.rows = self.rows.saturating_add(1);
        self.frame_ms += frame.frame_ms;
        if let Some(cpu_busy_ms) = frame.cpu_busy_ms {
            self.cpu_busy_ms += cpu_busy_ms.min(frame.frame_ms);
            self.cpu_valid_rows = self.cpu_valid_rows.saturating_add(1);
            self.cpu_valid_frame_ms += frame.frame_ms;
        }
        if let Some(gpu_busy_ms) = frame.gpu_busy_ms {
            self.gpu_busy_ms += gpu_busy_ms;
            self.gpu_valid_rows = self.gpu_valid_rows.saturating_add(1);
            self.gpu_valid_frame_ms += frame.frame_ms;
        }
    }

    fn component_valid(&self, valid_rows: u32, valid_frame_ms: f64) -> bool {
        self.rows != 0
            && valid_rows as f64 / self.rows as f64 >= WORKLOAD_VALIDITY
            && self.frame_ms > 0.0
            && valid_frame_ms / self.frame_ms >= WORKLOAD_VALIDITY
    }

    fn components(&self) -> (Option<f64>, Option<f64>) {
        let gpu = self
            .component_valid(self.gpu_valid_rows, self.gpu_valid_frame_ms)
            .then(|| (self.gpu_busy_ms / 1000.0).clamp(0.0, 1.0));
        let cpu = self
            .component_valid(self.cpu_valid_rows, self.cpu_valid_frame_ms)
            .then(|| (self.cpu_busy_ms / self.frame_ms).clamp(0.0, 1.0));
        (gpu, cpu)
    }
}

impl RecentActivity {
    fn new(process: ProcessInfo, handle: Option<ProcessHandle>) -> Self {
        Self {
            process,
            handle,
            buckets: VecDeque::new(),
            last_seen: None,
        }
    }

    fn record(&mut self, frame: &Frame) {
        let trace_second = frame.cpu_start_ms.and_then(|value| {
            (value <= u64::MAX as f64).then_some((value / 1000.0).floor() as u64)
        });
        let bucket = self.buckets.iter_mut().find(|bucket| {
            bucket.trace_second == trace_second
                && (trace_second.is_some() || bucket.observed_at == frame.observed_at)
        });
        if let Some(bucket) = bucket {
            bucket.record(frame);
        } else {
            if self.buckets.len() == MAX_ACTIVITY_BUCKETS {
                self.buckets.pop_front();
            }
            self.buckets
                .push_back(ActivityBucket::new(frame, trace_second));
        }
        self.last_seen = Some(frame.observed_at);
    }

    fn last_seen(&self) -> Option<Instant> {
        self.last_seen
    }

    fn score(&self, now: Instant) -> (usize, u32) {
        self.buckets
            .iter()
            .filter(|bucket| {
                now.checked_duration_since(bucket.observed_at)
                    .is_none_or(|age| age <= ACTIVITY_WINDOW)
            })
            .fold((0usize, 0u32), |(buckets, rows), bucket| {
                (buckets + 1, rows.saturating_add(bucket.rows))
            })
    }

    fn workload_score(&self) -> Option<WorkloadScore> {
        let latest = self
            .buckets
            .iter()
            .filter_map(|bucket| bucket.trace_second)
            .max()?;
        let mut complete: Vec<_> = self
            .buckets
            .iter()
            .filter(|bucket| bucket.trace_second.is_some_and(|second| second < latest))
            .collect();
        complete.sort_by_key(|bucket| bucket.trace_second);
        if complete.len() < WORKLOAD_BUCKETS {
            return None;
        }
        let complete = &complete[complete.len() - WORKLOAD_BUCKETS..];
        let first = latest.checked_sub(WORKLOAD_BUCKETS as u64)?;
        if complete
            .iter()
            .enumerate()
            .any(|(offset, bucket)| bucket.trace_second != Some(first + offset as u64))
        {
            return None;
        }
        let gpu = workload_median(complete.iter().filter_map(|bucket| bucket.components().0));
        let cpu = workload_median(complete.iter().filter_map(|bucket| bucket.components().1));
        let value = match (gpu, cpu) {
            (Some(occupancy), Some(duty)) => 0.65 * occupancy / (occupancy + 0.05) + 0.35 * duty,
            (Some(occupancy), None) => occupancy / (occupancy + 0.05),
            (None, Some(duty)) => duty,
            (None, None) => return None,
        };
        Some(WorkloadScore {
            value: value.clamp(0.0, 1.0),
            window: complete.last()?.trace_second?,
        })
    }
}

fn workload_median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<_> = values.collect();
    if values.len() != WORKLOAD_BUCKETS {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[WORKLOAD_BUCKETS / 2])
}

#[derive(Default)]
struct ActivityTracker {
    candidates: HashMap<CandidateKey, RecentActivity>,
    streams: HashMap<StreamKey, CandidateKey>,
}

impl ActivityTracker {
    fn record(
        &mut self,
        frame: &Frame,
        protected: &ProcessInfo,
        catalog: &CatalogState,
        queries: &mut usize,
        valid: &mut impl FnMut(&RecentActivity) -> bool,
        resolve: &mut impl FnMut(u32) -> Result<(ProcessInfo, Option<ProcessHandle>), String>,
    ) -> Option<ProcessInfo> {
        let stream = StreamKey::new(frame.pid, &frame.application);
        if let Some(key) = self.streams.get(&stream).cloned() {
            if self.candidates.get(&key).is_some_and(valid) {
                let activity = self.candidates.get_mut(&key)?;
                activity.record(frame);
                return Some(activity.process.clone());
            }
            self.remove_key(&key);
        }
        if *queries >= MAX_IDENTITY_QUERIES_PER_TICK {
            return None;
        }
        *queries += 1;
        let (process, handle) = resolve(frame.pid).ok()?;
        if process.creation_time == 0 || !process.name.eq_ignore_ascii_case(&frame.application) {
            return None;
        }
        if !catalog.allows(&process) {
            return None;
        }
        let key = CandidateKey::new(&process);
        if !self.candidates.contains_key(&key) && self.candidates.len() == MAX_CANDIDATES {
            if let Some(evict) = self
                .candidates
                .iter()
                .filter(|(_, activity)| activity.process != *protected)
                .min_by(|(left_key, left), (right_key, right)| {
                    left.last_seen()
                        .cmp(&right.last_seen())
                        .then_with(|| left_key.application.cmp(&right_key.application))
                        .then_with(|| left_key.pid.cmp(&right_key.pid))
                        .then_with(|| left_key.creation_time.cmp(&right_key.creation_time))
                })
                .map(|(key, _)| key.clone())
            {
                self.remove_key(&evict);
            }
        }
        if self.candidates.len() == MAX_CANDIDATES && !self.candidates.contains_key(&key) {
            return None;
        }
        self.streams.insert(stream, key.clone());
        let activity = self
            .candidates
            .entry(key)
            .or_insert_with(|| RecentActivity::new(process.clone(), handle));
        activity.record(frame);
        Some(activity.process.clone())
    }

    #[cfg(test)]
    fn record_test(&mut self, frame: &Frame, creation_time: u64) -> Option<ProcessInfo> {
        let application = frame.application.clone();
        let mut queries = 0;
        self.record(
            frame,
            &ProcessInfo::default(),
            &CatalogState::Unavailable,
            &mut queries,
            &mut |_| true,
            &mut |pid| {
                Ok((
                    ProcessInfo {
                        pid,
                        creation_time,
                        name: application.clone(),
                        image_path: String::new(),
                    },
                    None,
                ))
            },
        )
    }

    #[cfg(test)]
    fn record_test_process(&mut self, frame: &Frame, process: &ProcessInfo) -> Option<ProcessInfo> {
        let mut queries = 0;
        self.record(
            frame,
            &ProcessInfo::default(),
            &CatalogState::Unavailable,
            &mut queries,
            &mut |_| true,
            &mut |_| Ok((process.clone(), None)),
        )
    }

    fn validate_processes(&mut self) {
        self.validate_with(|activity| {
            activity
                .handle
                .as_ref()
                .is_some_and(ProcessHandle::is_running)
        });
    }

    fn validate_with(&mut self, mut valid: impl FnMut(&RecentActivity) -> bool) {
        let invalid: Vec<_> = self
            .candidates
            .iter()
            .filter(|(_, activity)| !valid(activity))
            .map(|(key, _)| key.clone())
            .collect();
        for key in invalid {
            self.remove_key(&key);
        }
    }

    fn prune(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .candidates
            .iter_mut()
            .filter_map(|(key, activity)| {
                (!activity.last_seen().is_some_and(|last| {
                    now.checked_duration_since(last)
                        .is_none_or(|age| age <= ACTIVITY_EXPIRY)
                }))
                .then(|| key.clone())
            })
            .collect();
        for key in expired {
            self.remove_key(&key);
        }
    }

    fn remove(&mut self, process: &ProcessInfo) {
        self.remove_key(&CandidateKey::new(process));
    }

    fn remove_key(&mut self, key: &CandidateKey) {
        self.candidates.remove(key);
        self.streams.retain(|_, candidate| candidate != key);
    }

    fn contains(&self, process: &ProcessInfo) -> bool {
        self.candidates.contains_key(&CandidateKey::new(process))
    }

    fn apply_catalog(&mut self, catalog: &CatalogState) {
        if matches!(catalog, CatalogState::Unavailable) {
            return;
        }
        let rejected: Vec<_> = self
            .candidates
            .iter()
            .filter(|(_, activity)| !catalog.allows(&activity.process))
            .map(|(key, _)| key.clone())
            .collect();
        for key in rejected {
            self.remove_key(&key);
        }
    }

    #[cfg(test)]
    fn strongest(&mut self, now: Instant, foreground_pid: u32) -> ProcessInfo {
        self.ranked(
            now,
            ForegroundInfo {
                pid: foreground_pid,
            },
            &ProcessInfo::default(),
            &CatalogState::Unavailable,
        )
        .map(|candidate| candidate.process)
        .unwrap_or_default()
    }

    #[cfg(test)]
    fn select(
        &mut self,
        now: Instant,
        foreground_pid: u32,
        current: &ProcessInfo,
        selected_last: Option<Instant>,
    ) -> ProcessInfo {
        if current.pid != 0
            && selected_last.is_some_and(|last| {
                now.checked_duration_since(last)
                    .is_none_or(|age| age <= ACTIVITY_EXPIRY)
            })
        {
            return current.clone();
        }
        if current.pid != 0 {
            self.remove(current);
        }
        self.strongest(now, foreground_pid)
    }

    fn ranked(
        &mut self,
        now: Instant,
        foreground: ForegroundInfo,
        current: &ProcessInfo,
        catalog: &CatalogState,
    ) -> Option<RankedCandidate> {
        self.prune(now);
        ranking_decision(self, now, foreground, current, catalog)
    }

    fn best_workload(
        &self,
        now: Instant,
        required_pid: u32,
        catalog: &CatalogState,
    ) -> Option<RankedCandidate> {
        self.candidates
            .iter()
            .filter(|(key, activity)| {
                (required_pid == 0 || key.pid == required_pid)
                    && activity.score(now).1 != 0
                    && catalog.allows(&activity.process)
            })
            .filter_map(|(key, activity)| {
                activity
                    .workload_score()
                    .map(|workload| (key, activity, workload))
            })
            .max_by(
                |(left_key, left, left_workload), (right_key, right, right_workload)| {
                    left_workload
                        .value
                        .total_cmp(&right_workload.value)
                        .then_with(|| left.score(now).cmp(&right.score(now)))
                        .then_with(|| right_key.application.cmp(&left_key.application))
                        .then_with(|| right_key.pid.cmp(&left_key.pid))
                        .then_with(|| right_key.creation_time.cmp(&left_key.creation_time))
                },
            )
            .map(|(_, activity, workload)| RankedCandidate {
                process: activity.process.clone(),
                workload: Some(workload),
            })
    }

    fn best_activity(
        &self,
        now: Instant,
        required_pid: u32,
        catalog: &CatalogState,
    ) -> Option<RankedCandidate> {
        self.candidates
            .iter()
            .filter(|(key, activity)| {
                (required_pid == 0 || key.pid == required_pid)
                    && activity.score(now).1 != 0
                    && catalog.allows(&activity.process)
            })
            .max_by(|(left_key, left), (right_key, right)| {
                left.score(now)
                    .cmp(&right.score(now))
                    .then_with(|| right_key.application.cmp(&left_key.application))
                    .then_with(|| right_key.pid.cmp(&left_key.pid))
                    .then_with(|| right_key.creation_time.cmp(&left_key.creation_time))
            })
            .map(|(_, activity)| RankedCandidate {
                process: activity.process.clone(),
                workload: None,
            })
    }

    fn workload_score(&self, process: &ProcessInfo) -> Option<WorkloadScore> {
        self.candidates
            .get(&CandidateKey::new(process))?
            .workload_score()
    }
}

/// WO-OPT06 (A2): the presenting-ranking decision as a PURE function of the tracker's candidates
/// and the `ForegroundInfo` the Windows layer built, so the eligibility and tie-break semantics
/// are testable over foreground states a live desktop cannot produce on demand (foreground
/// absent, foreground = self, = a present candidate, = a process outside the activity set, the
/// shipped `Err`-shaped 0, a reused pid, and the tie-breaks). The body is the shipped `ranked`
/// decision verbatim; only `self` became a `&ActivityTracker` parameter and the pid became
/// `foreground.pid`. `ranked` keeps the shipped `prune` before it.
fn ranking_decision(
    activity: &ActivityTracker,
    now: Instant,
    foreground: ForegroundInfo,
    current: &ProcessInfo,
    catalog: &CatalogState,
) -> Option<RankedCandidate> {
    if let Some(best) = activity.best_workload(now, 0, catalog) {
        if current.pid != 0 {
            if let Some(workload) = activity.workload_score(current) {
                if best.process != *current
                    && best.workload?.value >= workload.value + SWITCH_MARGIN
                {
                    return Some(best);
                }
                return Some(RankedCandidate {
                    process: current.clone(),
                    workload: Some(workload),
                });
            }
        }
        if let Some(foreground) = activity.best_workload(now, foreground.pid, catalog) {
            if best.process == foreground.process
                || best.workload?.value - foreground.workload?.value < SWITCH_MARGIN
            {
                return Some(foreground);
            }
        }
        return Some(best);
    }
    activity
        .best_activity(now, foreground.pid, catalog)
        .or_else(|| activity.best_activity(now, 0, catalog))
}

#[derive(Default)]
struct Parser {
    header: Option<HashMap<String, usize>>,
}

impl Parser {
    fn parse_line(&mut self, line: &str, at: SystemTime) -> Result<Option<Frame>, String> {
        if line.len() > MAX_LINE_BYTES {
            return Err("PresentMon CSV line exceeds 64 KiB".into());
        }
        let fields = csv_fields(line)?;
        if fields.is_empty() {
            return Ok(None);
        }
        if self.header.is_none() {
            if !fields.iter().any(|v| v.eq_ignore_ascii_case("ProcessID")) {
                return Ok(None);
            }
            let header: HashMap<_, _> = fields
                .iter()
                .enumerate()
                .map(|(i, h)| (h.trim().to_ascii_lowercase(), i))
                .collect();
            if !["frametime", "msbetweenpresents", "msbetweendisplaychange"]
                .iter()
                .any(|name| header.contains_key(*name))
            {
                return Err(
                    "CSV header has no frame-time column (expected FrameTime for --v2_metrics)"
                        .into(),
                );
            }
            self.header = Some(header);
            return Ok(None);
        }
        // WO-OPT08 (A2): every lookup name below is written already-lowercase, so the probe
        // borrows a &'static str; the column indices were resolved once when the header was
        // accepted. The old `name.to_ascii_lowercase()` owned a fresh String per candidate name
        // for EVERY row. Alias order and the empty-field fallback are unchanged.
        let get = |names: &[&str]| -> &str {
            let header = self.header.as_ref().unwrap();
            names
                .iter()
                .find_map(|name| header.get(*name).and_then(|i| fields.get(*i)))
                .map(|v| v.trim())
                .unwrap_or("")
        };
        let pid = get(&["processid"])
            .parse::<u32>()
            .map_err(|e| format!("PresentMon ProcessID: {e}"))?;
        let text = get(&["frametime", "msbetweenpresents", "msbetweendisplaychange"]);
        if text.is_empty() || text.eq_ignore_ascii_case("NA") {
            return Ok(None);
        }
        let frame_ms = text
            .parse::<f64>()
            .map_err(|e| format!("PresentMon frame metric: {e}"))?;
        if !frame_ms.is_finite() {
            return Err("PresentMon frame metric must be finite".into());
        }
        if !(0.0..=10_000.0).contains(&frame_ms) || frame_ms == 0.0 {
            return Ok(None);
        }
        let cpu_start_ms = optional_metric(
            get(&["cpustarttime", "cpustarttimeinms"]),
            "CPUStartTime",
            u64::MAX as f64,
        )?;
        let cpu_busy_ms = optional_metric(get(&["cpubusy", "mscpubusy"]), "CPUBusy", 10_000.0)?;
        let gpu_busy_ms = optional_metric(get(&["gpubusy", "msgpubusy"]), "GPUBusy", 10_000.0)?;
        Ok(Some(Frame {
            at,
            observed_at: Instant::now(),
            // The frame outlives the input line, so this field keeps its owned String.
            application: get(&["application"]).to_string(),
            pid,
            frame_ms,
            cpu_start_ms,
            cpu_busy_ms,
            gpu_busy_ms,
        }))
    }
}

fn optional_metric(text: &str, name: &str, maximum: f64) -> Result<Option<f64>, String> {
    if text.is_empty()
        || ["NA", "N/A", "unavailable"]
            .iter()
            .any(|missing| text.eq_ignore_ascii_case(missing))
    {
        return Ok(None);
    }
    let value = text
        .parse::<f64>()
        .map_err(|error| format!("PresentMon {name}: {error}"))?;
    if !value.is_finite() {
        return Err(format!("PresentMon {name} must be finite"));
    }
    Ok((0.0..=maximum).contains(&value).then_some(value))
}

/// WO-OPT08 (A2): the row split borrows every field that contains no quote character, so a
/// 23-column PresentMon row allocates only the `Vec` itself. A field with a quote is the ONE
/// case that must be built, because unescaping changes its bytes (`""` becomes `"`), so the
/// value cannot alias the input; it is owned for that field only. The quote toggle, the doubled
/// quote escape, the `unterminated quoted ...` error, the line trim, the per-field content and
/// the trailing field push are the shipped ones, in the shipped order.
fn csv_fields(line: &str) -> Result<Vec<Cow<'_, str>>, String> {
    let trimmed = line.trim();
    let mut fields: Vec<Cow<'_, str>> = Vec::new();
    let mut start = 0usize;
    let mut copied = 0usize;
    let mut owned: Option<String> = None;
    let mut quoted = false;
    let mut chars = trimmed.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' if quoted && chars.peek().is_some_and(|&(_, next)| next == '"') => {
                chars.next();
                let text = owned.get_or_insert_with(String::new);
                if copied < index {
                    text.push_str(&trimmed[copied..index]);
                }
                text.push('"');
                copied = index + 2;
            }
            '"' => {
                quoted = !quoted;
                // A quote changes this field's bytes, so from here it is the owned case.
                let text = owned.get_or_insert_with(String::new);
                if copied < index {
                    text.push_str(&trimmed[copied..index]);
                }
                copied = index + 1;
            }
            ',' if !quoted => {
                let value = match owned.take() {
                    Some(mut text) => {
                        if copied < index {
                            text.push_str(&trimmed[copied..index]);
                        }
                        Cow::Owned(text)
                    }
                    None => Cow::Borrowed(&trimmed[start..index]),
                };
                fields.push(value);
                start = index + 1;
                copied = start;
            }
            _ => {}
        }
    }
    if quoted {
        return Err("unterminated quoted PresentMon CSV field".into());
    }
    let end = trimmed.len();
    fields.push(match owned.take() {
        Some(mut text) => {
            if copied < end {
                text.push_str(&trimmed[copied..end]);
            }
            Cow::Owned(text)
        }
        None => Cow::Borrowed(&trimmed[start..]),
    });
    Ok(fields)
}

/// Model-shape census counters (WO-OPT05 A1). The lane's precedent shape is
/// `WORKER_TICKS`/`worker_tick_count` (`:1476-1492`) and the unit-56 `SNAPSHOT_CREATIONS` in
/// `hid.rs`: a per-THREAD map counted at the named production site, so a census reads the
/// calling test's own model calls and not the whole process (the shipped suite runs tests in
/// parallel). One entry per test thread in the test binary, `cfg(test)` only, never compiled
/// into the shipped artifact. They count CALLS, not time: no CPU is measured here and no CPU
/// claim is made from these rows.
#[cfg(test)]
static MODEL_SHAPE_BUILDS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, (u64, u64)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn model_build_map(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, (u64, u64)>> {
    MODEL_SHAPE_BUILDS.get_or_init(std::sync::Mutex::default)
}

/// `(model builds, stale-path model builds)` for the calling thread. The discarded-stale count
/// is derived in the census as `stale builds - publications`, both measured, so no counter
/// needs a site in code that no longer discards anything.
#[cfg(test)]
fn model_shape_counts() -> (u64, u64) {
    let map = model_build_map();
    let map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.get(&std::thread::current().id())
        .copied()
        .unwrap_or((0, 0))
}

#[cfg(test)]
fn model_shape_reset() {
    let map = model_build_map();
    let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.insert(std::thread::current().id(), (0, 0));
}

#[cfg(test)]
fn model_build_tick() {
    let map = model_build_map();
    let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(std::thread::current().id()).or_insert((0, 0)).0 += 1;
}

#[cfg(test)]
fn stale_build_tick() {
    let map = model_build_map();
    let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(std::thread::current().id()).or_insert((0, 0)).1 += 1;
}

#[cfg(not(test))]
fn model_build_tick() {}

#[cfg(not(test))]
fn stale_build_tick() {}

struct Stats {
    frames: VecDeque<(Instant, f64)>,
    session_start: Option<SystemTime>,
    stutters: i32,
    threshold: f64,
    window: Duration,
    /// WO-OPT05 (A2.2): the window filter's reusable scratch. Cleared and refilled by
    /// `model`, then sorted IN PLACE for the two tail means, so a warm `model` call
    /// allocates no window vector, no 1 s vector and no sort clone. Retained capacity
    /// only: `Vec::clear` never shrinks, so this is per-`Stats` private memory sized by
    /// the window occupancy (reported in WO-OPT05-RECEIPT.md), never released to the
    /// allocator.
    scratch: Vec<f64>,
}

impl Stats {
    fn new(threshold: f64, window: Duration) -> Self {
        Self {
            frames: VecDeque::new(),
            session_start: None,
            stutters: 0,
            threshold,
            window,
            scratch: Vec::new(),
        }
    }

    fn add(&mut self, frame: &Frame) -> Result<(), &'static str> {
        let cutoff = frame.observed_at.checked_sub(self.window);
        while self
            .frames
            .front()
            .is_some_and(|(at, _)| cutoff.is_some_and(|cutoff| *at < cutoff))
        {
            self.frames.pop_front();
        }
        if self.frames.len() >= MAX_WINDOW_FRAMES {
            return Err("PresentMon statistics capacity reached before window expiry");
        }
        self.session_start.get_or_insert(frame.at);
        self.frames.push_back((frame.observed_at, frame.frame_ms));
        if frame.frame_ms >= self.threshold {
            self.stutters = self.stutters.saturating_add(1);
        }
        Ok(())
    }

    /// The published model. `&mut self` for ONE reason: the reusable `scratch` window
    /// buffer, sorted in place. Nothing else on `Stats` is touched, so the numbers are
    /// the same function of the same frames as the shipped `&self` form.
    fn model(
        &mut self,
        now: SystemTime,
        observed_now: Instant,
        window: Duration,
        process: &str,
    ) -> GameStats {
        model_build_tick();
        let cutoff = observed_now.checked_sub(window);
        let recent_cutoff = observed_now.checked_sub(Duration::from_secs(1));
        // One scan: the window filter goes into the reused scratch buffer, and the last-1 s
        // set is recorded as the CONTIGUOUS SUFFIX of it that starts at the first in-recent
        // frame. `mean` then runs on a slice of the scratch: the same values in the same
        // order as the shipped `mean(&recent)`, so it is bit-identical, with no vector.
        // (A running sum is NOT exact here: `Iterator::sum` and a manual `+=` fold give
        // different NaN payloads for `inf + -inf`, proved by the A3 matrix.)
        self.scratch.clear();
        let mut recent_start: Option<usize> = None;
        let mut recent_in_window = 0usize;
        let mut recent_outside_window = 0usize;
        let mut last_window = 0.0f64;
        for (at, value) in self.frames.iter() {
            let in_window = cutoff.is_none_or(|cutoff| *at >= cutoff);
            let in_recent = recent_cutoff.is_none_or(|cutoff| *at >= cutoff);
            if in_window {
                if in_recent {
                    if recent_start.is_none() {
                        recent_start = Some(self.scratch.len());
                    }
                    recent_in_window += 1;
                }
                self.scratch.push(*value);
                last_window = *value;
            } else if in_recent {
                recent_outside_window += 1;
            }
        }
        if self.scratch.is_empty() {
            return GameStats::default();
        }
        let current = match recent_start {
            // The shipped fallback: an empty recent set takes the LAST frame time of the
            // window in arrival order, never the sorted tail.
            None if recent_outside_window == 0 => last_window,
            Some(start)
                if recent_outside_window == 0 && start + recent_in_window == self.scratch.len() =>
            {
                mean(&self.scratch[start..])
            }
            // A window narrower than 1 s, or non-monotonic stamps, makes the recent set
            // reach outside the window: the shipped vector path is kept verbatim there.
            _ => {
                let recent: Vec<f64> = self
                    .frames
                    .iter()
                    .filter(|(at, _)| recent_cutoff.is_none_or(|cutoff| *at >= cutoff))
                    .map(|(_, value)| *value)
                    .collect();
                if recent.is_empty() {
                    last_window
                } else {
                    mean(&recent)
                }
            }
        };
        // A2.1: one ordering yields both tail means (see `history::tail_means`).
        let (one, point_one) = tail_means(&mut self.scratch);
        GameStats {
            active: true,
            process_name: process.to_string(),
            game_name: Path::new(process)
                .file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or(process)
                .to_string(),
            fps: Metric::valid(1000.0 / current, now),
            one_percent: Metric::valid(1000.0 / one, now),
            point_one_low: Metric::valid(1000.0 / point_one, now),
            frame_time_ms: Metric::valid(current, now),
            stutters: self.stutters,
            session_start: self.session_start,
        }
    }
}

pub fn spawn(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-presentmon".into())
        .spawn(move || run(config, generation, consumers, shutdown, tx))
        .expect("PresentMon telemetry thread");
    (rx, thread)
}

fn current_request(config: &RwLock<Config>, generation: &AtomicU64) -> (u64, Config) {
    loop {
        let before = generation.load(Ordering::SeqCst);
        let config = config.read().unwrap_or_else(|e| e.into_inner()).clone();
        if before == generation.load(Ordering::SeqCst) {
            return (before, config);
        }
    }
}

/// WO-TEL-D2 (S5, U-8): the worker-loop head (WO-OPT02C: parked on its own deadline)
/// reuses the cached `Arc<Config>`
/// under an unchanged generation (no 20 Hz `Config` clone); a changed
/// generation takes the module's shipped `current_request` rule copy, so the
/// cached value belongs to the generation reported with it. Generation
/// semantics are untouched.
fn cached_request(
    config: &RwLock<Config>,
    generation: &AtomicU64,
    cache: &mut (u64, Arc<Config>),
) -> (u64, Arc<Config>) {
    let before = generation.load(Ordering::SeqCst);
    if before == cache.0 && before != 0 && generation.load(Ordering::SeqCst) == before {
        return (before, Arc::clone(&cache.1));
    }
    let (generation, config) = current_request(config, generation);
    *cache = (generation, Arc::new(config));
    (generation, Arc::clone(&cache.1))
}

fn confirm_start_request(
    config: &RwLock<Config>,
    live_generation: &AtomicU64,
    shutdown: &AtomicBool,
    expected_config: &Config,
    expected_generation: u64,
    expected_target: &ProcessInfo,
    expected_key: &str,
) -> Result<(), String> {
    let (generation, config) = current_request(config, live_generation);
    let target = if config.presentmon_target_mode == "presenting" {
        ProcessInfo::default()
    } else {
        select_target(&config)
            .map_err(|error| format!("PresentMon final target selection failed: {error}"))?
    };
    if shutdown.load(Ordering::Acquire)
        || generation != expected_generation
        || live_generation.load(Ordering::SeqCst) != expected_generation
        || config != *expected_config
        || target != *expected_target
        || capture_key(&config, &target) != expected_key
    {
        return Err("PresentMon request changed or shutdown began before capture start".into());
    }
    Ok(())
}

/// EC-6: named child-exit causes observed at the presentmon child boundary.
/// Stable short tokens (RC-1 vocabulary discipline, `CatalogUnavailableReason::code` style);
/// the reason text carries class + numeric exit code only, never child argv or paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildExit {
    Normal,
    Failed(u32),
    DidNotExitInTime,
}

/// `ExitStatus::code()` is always `Some` on Windows; the sentinel keeps a code-less (signal) death
/// named as a failure instead of masking it as a normal exit. Not reachable on the pinned target.
const NO_EXIT_CODE: u32 = u32::MAX;

impl ChildExit {
    fn classify(status: &ExitStatus) -> Self {
        match status.code() {
            Some(0) => Self::Normal,
            Some(code) => Self::Failed(code as u32),
            None => Self::Failed(NO_EXIT_CODE),
        }
    }

    fn token(self) -> String {
        match self {
            Self::Normal => "exited_normal".into(),
            Self::Failed(code) => format!("exited_failed_{code}"),
            Self::DidNotExitInTime => "did_not_exit_in_time".into(),
        }
    }

    fn reason(self) -> String {
        format!("presentmon child {}", self.token())
    }

    /// Full provider-unavailable reason for a child that stopped while capturing (site 1).
    fn capture_end_reason(self, target: &str) -> String {
        format!(
            "{} while capturing {target}; the target may have exited",
            self.reason()
        )
    }
}

fn wait_bounded(
    timeout: Duration,
    mut exited: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if exited()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_child_bounded(child: &mut Child, timeout: Duration) -> Result<ChildExit, String> {
    let mut observed: Option<ExitStatus> = None;
    let confirmed = wait_bounded(timeout, || {
        child
            .try_wait()
            .map(|status| {
                let done = status.is_some();
                if done {
                    observed = status;
                }
                done
            })
            .map_err(|error| format!("query child status: {error}"))
    })?;
    match (confirmed, observed) {
        (true, Some(status)) => Ok(ChildExit::classify(&status)),
        // confirmed only happens once a status was observed, so this arm is unreachable.
        (true, None) => Ok(ChildExit::DidNotExitInTime),
        (false, _) => Err(format!(
            "exit was not confirmed within {} ms: {}",
            timeout.as_millis(),
            ChildExit::DidNotExitInTime.token()
        )),
    }
}

fn stop_child_bounded(child: &mut Child, timeout: Duration) -> Result<ChildExit, String> {
    if let Ok(Some(status)) = child.try_wait() {
        return Ok(ChildExit::classify(&status));
    }
    if let Err(kill) = child.kill() {
        return match child.try_wait() {
            Ok(Some(status)) => Ok(ChildExit::classify(&status)),
            Ok(None) => Err(format!(
                "kill child: {kill}; child is still running: {}",
                ChildExit::DidNotExitInTime.token()
            )),
            Err(wait) => Err(format!("kill child: {kill}; confirm exit: {wait}")),
        };
    }
    wait_child_bounded(child, timeout).map_err(|error| format!("after kill: {error}"))
}

fn join_thread_bounded<T>(
    thread: std::thread::JoinHandle<T>,
    deadline: Instant,
) -> Result<T, &'static str> {
    while !thread.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !thread.is_finished() {
        return Err("reader timed out and was detached");
    }
    thread.join().map_err(|_| "reader panicked")
}

/// WO-OPT02C (C1.3): the worker's next wake, built ONLY from gates this file already
/// computes. The earliest remaining deadline wins; a gate that is already due collapses
/// to `WORKER_MIN_PARK` (never negative, never a spin); a gate further away than
/// `PRESENTMON_IDLE_BOUND` is capped to it, because the demand / config / shutdown
/// `unpark` edges are what make a long park safe; and an empty set (nothing to wait
/// for) is the idle bound. Pure: no clocks read, no state touched, exact `Duration`s.
fn worker_idle_deadline(deadlines: &[Option<Instant>], now: Instant) -> Duration {
    let earliest = deadlines
        .iter()
        .flatten()
        .fold(None, |acc: Option<Duration>, deadline| {
            let remaining = deadline.saturating_duration_since(now);
            Some(acc.map_or(remaining, |acc| acc.min(remaining)))
        });
    match earliest {
        Some(remaining) if remaining <= WORKER_MIN_PARK => WORKER_MIN_PARK,
        Some(remaining) => remaining.min(PRESENTMON_IDLE_BOUND),
        None => PRESENTMON_IDLE_BOUND,
    }
}

#[cfg(test)]
static WORKER_TICKS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, u64>>,
> = std::sync::OnceLock::new();

/// WO-OPT02C (C3 seam): test-only per-worker iteration counter, so the wake census and
/// the dormancy tests measure THIS worker's loop and not the process. Bounded: one
/// entry per worker thread in the test binary, never compiled into the release artifact.
#[cfg(test)]
fn worker_tick() {
    let ticks = WORKER_TICKS.get_or_init(std::sync::Mutex::default);
    let mut ticks = ticks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *ticks.entry(std::thread::current().id()).or_insert(0) += 1;
}

#[cfg(test)]
fn worker_tick_count(thread: std::thread::ThreadId) -> u64 {
    let ticks = WORKER_TICKS.get_or_init(std::sync::Mutex::default);
    let ticks = ticks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ticks.get(&thread).copied().unwrap_or(0)
}

#[cfg(not(test))]
fn worker_tick() {}

fn run(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    consumers: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<Update>,
) {
    let mut capture = Capture::default();
    let mut current_generation = 0;
    let mut request = (
        0u64,
        Arc::new(config.read().unwrap_or_else(|e| e.into_inner()).clone()),
    );
    while !shutdown.load(Ordering::Relaxed) {
        worker_tick();
        let (request_generation, cfg) = cached_request(&config, &generation, &mut request);
        if request_generation != current_generation {
            if capture.stop_and_report_cleanup(&tx, request_generation, Instant::now()) {
                // WO-OPT02C (C1.3): the cleanup-pending retry parks on the gate this arm
                // itself stamps - `stop_and_report_cleanup` -> `schedule_retry` sets
                // `retry_after` (RETRY_DELAY) on a failed stop - capped to the idle bound.
                std::thread::park_timeout(worker_idle_deadline(
                    &[capture.retry_after],
                    Instant::now(),
                ));
                continue;
            }
            capture = Capture::default();
            current_generation = request_generation;
        }
        // WO-TEL-D2 (S2, U-3): no displayed presentmon-backed module
        // (`app.rs module_extended_consumers`, fall-through included) => ZERO
        // target polls AND ZERO child-spawn retries: the reconcile gate (target
        // selection, executable resolution, the child spawn retry ladder) is
        // never reached. The park reuses the shipped disabled-arm shape:
        // `stop_and_report_cleanup` once, one deduped `unavailable` publication
        // (health: `providers["presentmon"]` false => the availability
        // predicate `app.rs:745-763` keeps falling through, fed by publication,
        // not by fresh polls), then the dormancy park (WO-OPT02C: the idle bound,
        // not the retired 50 ms step). Demand return re-enters reconcile: the 1 s
        // target-poll gate is due (`last_target_poll` stale), so selection runs on
        // the worker tick the demand `unpark` edge drives (<=100 ms render cadence +
        // an immediate wake).
        if !crate::telemetry::demanded(consumers.load(Ordering::Relaxed), "presentmon") {
            if capture.stop_and_report_cleanup(&tx, request_generation, Instant::now()) {
                // WO-OPT02C (C1.3): same shipped retry gate as the generation arm.
                std::thread::park_timeout(worker_idle_deadline(
                    &[capture.retry_after],
                    Instant::now(),
                ));
                continue;
            }
            capture.unavailable(&tx, request_generation, "no display consumer".into());
            // WO-OPT02C (C1.3/C1.1): the zero-demand park has NOTHING in this file to
            // wait for - the reconcile gates are behind the demand gate, so a
            // target-poll deadline that is already due must NOT collapse the park
            // (`last_target_poll` ages, it is never stamped here). The set is empty,
            // so the park is the idle bound (`TARGET_POLL`), and the demand edge is
            // what pays for it: `telemetry.rs::set_consumers` stores the word and
            // `wake_workers` unparks this handle, so adoption is the publish cadence,
            // not the park. The shipped narrative's "50 ms loop" is now "an immediate
            // unpark", which is why the 150 ms re-arm bound stays an UPPER bound.
            std::thread::park_timeout(worker_idle_deadline(&[], Instant::now()));
            continue;
        }
        capture.reconcile(&cfg, request_generation, &tx, &|target, key| {
            confirm_start_request(
                &config,
                &generation,
                &shutdown,
                &cfg,
                request_generation,
                target,
                key,
            )
        });
        // WO-OPT02C (C1.3/C1.4): the tail step is the worker's own deadline, not a 20 Hz
        // tick. With a capture running, the line channel IS a real wake edge (the reader
        // thread's send returns `recv_timeout`), so the CSV pump is data-driven and no
        // queued line can age into the `line_overflow` reset; with no capture there is
        // nothing to drain, so the worker parks to its gates.
        if capture.capture_running() {
            // WO-OPT02C-PUMPCAP2 (S14, F-AUD-OPT02C-1/-2): the plain form of the pump
            // bound. `step.min(BOUND)` alone was measured at 64.2 iterations/s in the
            // past-`STALE_AFTER` shape - `worker_idle_deadline` collapses a deadline set
            // whose gates are permanently due (a frozen game, a stalled pipe) to
            // `WORKER_MIN_PARK` and a `min` cannot lift it, so the bound is carried as a
            // constant, not as a clamp of a deadline that is already gone. And because
            // `Thread::unpark` cannot cut a running `recv_timeout`, this step is the
            // demand-adoption bound of the running-capture shape. It is the shipped 50 ms
            // step (base parity); the deadline stays where it pays, in the no-capture
            // `park_timeout` branch below.
            capture.pump_step(PRESENTMON_PUMP_BOUND);
        } else {
            // F-AUD-OPT02C2-3: the deadline is computed where it is consumed, so the
            // running-capture branch cannot read it (no behavior change).
            let step = capture.idle_deadline(&cfg, Instant::now());
            std::thread::park_timeout(step);
        }
    }
    let mut stderr = String::new();
    for _ in 0..3 {
        let current = capture.stop();
        if !current.is_empty() {
            stderr = current;
        }
        if !capture.teardown_pending() {
            break;
        }
    }
    if capture.teardown_pending() || !capture.stop_diagnostic.is_empty() {
        capture.failure(
            &tx,
            generation.load(Ordering::SeqCst),
            with_diagnostic("PresentMon final shutdown incomplete".into(), stderr),
        );
    }
}

#[derive(Default)]
struct Capture {
    child: Option<Child>,
    owned_session: Option<String>,
    lines: Option<mpsc::Receiver<String>>,
    /// WO-OPT02C (C1.4): one line stashed by `pump_step` (the channel is the worker's
    /// data edge, and `recv_timeout` CONSUMES), handed to the next `drain_with`, so the
    /// data-driven step never drops a CSV line. Cleared with `lines` on every stop/start.
    pump_line: Option<String>,
    line_overflow: Option<Arc<AtomicBool>>,
    reader: Option<std::thread::JoinHandle<()>>,
    stderr: Option<Arc<std::sync::Mutex<BoundedDiagnostic>>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    target: ProcessInfo,
    key: String,
    parser: Parser,
    stats: Option<Stats>,
    last_target_poll: Option<Instant>,
    last_frame: Option<Instant>,
    last_publish: Option<Instant>,
    last_unavailable: String,
    activity: ActivityTracker,
    catalog: CatalogCache,
    challenger: Option<ChallengerState>,
    started_at: Option<Instant>,
    retry_after: Option<Instant>,
    stop_diagnostic: String,
}

impl Capture {
    /// WO-OPT02C (C1.3): this worker's own deadline set, from the stamps the shipped
    /// gates already read: the target-poll cadence, the catalog refresh, the retry
    /// cooldown, the staleness and expiry edges, the publish gate and the startup
    /// timeout. `VERIFIER_BOUND` needs no term: the Authenticode verification runs
    /// INSIDE `reconcile` (`run_bounded_verifier` blocks this thread in
    /// `recv_timeout(VERIFIER_BOUND)`), so the worker is never parked with a pending
    /// verifier; after a timeout the shipped decline is retried by the target-poll gate,
    /// which IS in this set.
    fn idle_deadline(&self, cfg: &Config, now: Instant) -> Duration {
        worker_idle_deadline(
            &[
                self.last_target_poll.map(|last| last + TARGET_POLL),
                self.catalog.checked_at.map(|last| last + CATALOG_REFRESH),
                self.retry_after,
                self.last_frame.map(|last| last + STALE_AFTER),
                self.last_frame.map(|last| last + STALE_AFTER + STALE_AFTER),
                self.last_publish
                    .filter(|_| self.last_frame > self.last_publish)
                    .map(|last| last + cfg.presentmon_interval),
                self.started_at.map(|started| started + STARTUP_TIMEOUT),
            ],
            now,
        )
    }

    /// WO-OPT02C (C1.4): a capture whose CSV pipe is live. Its lines must keep draining:
    /// the shipped `line_overflow` reset fires once the bounded queue passes
    /// `LINE_QUEUE_CAPACITY`, so this is the one shape with a data edge, not a timer.
    fn capture_running(&self) -> bool {
        self.child.is_some() && self.lines.is_some()
    }

    /// WO-OPT02C (C1.4): the inter-iteration step while a capture runs. The reader
    /// thread's `send` is a real wake edge, so the worker waits on the channel to its own
    /// deadline; the received line is stashed for the next `drain_with` (no CSV line is
    /// dropped by the wait). With an empty queue the timeout IS the park.
    fn pump_step(&mut self, step: Duration) {
        let line = self
            .lines
            .as_ref()
            .and_then(|lines| lines.recv_timeout(step).ok());
        if line.is_some() {
            self.pump_line = line;
        }
    }

    fn reconcile(
        &mut self,
        cfg: &Config,
        generation: u64,
        tx: &mpsc::Sender<Update>,
        confirm_start: &dyn Fn(&ProcessInfo, &str) -> Result<(), String>,
    ) {
        let now = Instant::now();
        let presenting = cfg.presentmon_target_mode == "presenting";
        if cfg.safe_mode || !cfg.presentmon_enabled || cfg.presentmon_target_mode == "disabled" {
            if self.stop_and_report_cleanup(tx, generation, now) {
                return;
            }
            self.unavailable(tx, generation, "disabled".into());
            return;
        }
        let should_select = self
            .last_target_poll
            .map(|last| now.duration_since(last) >= TARGET_POLL)
            .unwrap_or(true);
        if should_select {
            self.last_target_poll = Some(now);
            let selected = if presenting {
                Ok(ProcessInfo::default())
            } else {
                select_target(cfg)
            };
            match selected {
                Ok(target) if target.pid != 0 || presenting => {
                    let key = capture_key(cfg, &target);
                    if self.child.is_none() || key != self.key {
                        if retry_pending(self.retry_after, now) {
                            self.unavailable(tx, generation, "PresentMon restart cooldown".into());
                            return;
                        }
                        if self.stop_and_report_cleanup(tx, generation, now) {
                            return;
                        }
                        match resolve_executable(&cfg.presentmon_path).and_then(|executable| {
                            self.start(&executable, target.clone(), cfg, key, confirm_start)
                        }) {
                            Ok(()) => {
                                self.last_unavailable.clear();
                                if !presenting {
                                    let _ = tx.send(Update {
                                        generation,
                                        game: GameStats {
                                            active: true,
                                            process_name: target.name.clone(),
                                            game_name: Path::new(&target.name)
                                                .file_stem()
                                                .and_then(|v| v.to_str())
                                                .unwrap_or(&target.name)
                                                .to_string(),
                                            session_start: Some(SystemTime::now()),
                                            ..Default::default()
                                        },
                                        detail: format!("starting capture for {}", target.name),
                                        ..Default::default()
                                    });
                                }
                            }
                            Err(e) => {
                                self.schedule_retry(now);
                                self.failure(tx, generation, e);
                                return;
                            }
                        }
                    }
                }
                Ok(_) => {
                    if self.stop_and_report_cleanup(tx, generation, now) {
                        return;
                    }
                    self.unavailable(tx, generation, "waiting for target process".into());
                    return;
                }
                Err(error) => {
                    let _ = self.stop();
                    self.failure(
                        tx,
                        generation,
                        format!("PresentMon target selection failed: {error}"),
                    );
                    return;
                }
            }
        }

        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let exit = ChildExit::classify(&status);
                    let target = if presenting {
                        "local frame presenters".into()
                    } else {
                        self.target.name.clone()
                    };
                    let stderr = self.stop();
                    self.schedule_retry(now);
                    self.failure(
                        tx,
                        generation,
                        with_diagnostic(exit.capture_end_reason(&target), stderr),
                    );
                    return;
                }
                Err(e) => {
                    let stderr = self.stop();
                    self.schedule_retry(now);
                    self.failure(
                        tx,
                        generation,
                        with_diagnostic(format!("PresentMon process status failed: {e}"), stderr),
                    );
                    return;
                }
                Ok(None) => {}
            }
        }
        if self
            .line_overflow
            .as_ref()
            .is_some_and(|overflow| overflow.swap(false, Ordering::AcqRel))
        {
            let stderr = self.stop();
            self.schedule_retry(now);
            self.failure(
                tx,
                generation,
                with_diagnostic(
                    "PresentMon output overflowed; statistics reset".into(),
                    stderr,
                ),
            );
            return;
        }
        if presenting {
            self.catalog.refresh(now);
            self.activity
                .apply_catalog(selection_catalog(cfg, &self.catalog.state));
            self.activity.validate_processes();
            self.prepare_presenting(cfg, generation, tx, now, foreground_ranking_pid);
        }
        if let Err(error) = self.drain(cfg, generation, tx, now) {
            let stderr = self.stop();
            self.schedule_retry(now);
            self.failure(tx, generation, with_diagnostic(error, stderr));
            return;
        }
        if startup_timed_out(
            presenting,
            self.last_frame,
            self.started_at,
            self.parser.header.is_some(),
            now,
        ) {
            let reason = if self.parser.header.is_some() {
                format!(
                    "PresentMon produced a CSV header but no matching frames for {} in 45 seconds",
                    self.target.name
                )
            } else {
                "PresentMon produced no CSV header in 45 seconds".into()
            };
            let stderr = self.stop();
            self.schedule_retry(now);
            self.failure(tx, generation, with_diagnostic(reason, stderr));
            return;
        }
        if !presenting {
            if let (Some(last), true) = (self.last_frame, self.stats.is_some()) {
                let idle = now.duration_since(last);
                // WO-OPT05 (A2.3): the phase and the detail string are decided FIRST, so
                // the model is built only on a real transition. The shipped shape is kept
                // exactly: `idle <= STALE_AFTER` keeps the shipped early return, the two
                // detail strings are verbatim, and the `last_unavailable` bookkeeping is
                // the shipped write-then-send.
                let Some(phase) = stale_phase(idle) else {
                    return;
                };
                if stale_publishes(&self.last_unavailable, phase) {
                    if let Some(stats) = self.stats.as_mut() {
                        let detail = stale_detail(phase);
                        let game = stale_game(
                            stats,
                            SystemTime::now(),
                            now,
                            cfg.presentmon_window,
                            &self.target.name,
                            phase,
                        );
                        self.last_unavailable = detail.to_string();
                        let _ = tx.send(Update {
                            generation,
                            game,
                            detail: detail.to_string(),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        if presenting {
            self.pick_presenter(cfg, now, foreground_ranking_pid);
            if self.target.pid == 0 {
                self.unavailable(
                    tx,
                    generation,
                    "waiting for an active frame presenter".into(),
                );
            }
        }
    }

    fn prepare_presenting(
        &mut self,
        cfg: &Config,
        generation: u64,
        tx: &mpsc::Sender<Update>,
        now: Instant,
        foreground_pid: impl FnOnce() -> u32,
    ) {
        if self.target.pid != 0 && !self.activity.contains(&self.target) {
            self.select_presenter(ProcessInfo::default(), cfg);
        }
        if let (Some(last), true) = (self.last_frame, self.stats.is_some()) {
            let idle = now.duration_since(last);
            // WO-OPT05 (A2.3): the phase and the detail string are decided before any
            // model exists, so a stale iteration that the shipped dedupe would discard
            // builds nothing. The shipped `ACTIVITY_EXPIRY` action stays on EVERY
            // projected iteration, transition or not, exactly as the shipped dedupe left
            // it, and the fresh case stays a no-op (no early return here in the shipped form).
            if let Some(phase) = stale_phase(idle) {
                if stale_publishes(&self.last_unavailable, phase) {
                    if let Some(stats) = self.stats.as_mut() {
                        let detail = stale_detail(phase);
                        let game = stale_game(
                            stats,
                            SystemTime::now(),
                            now,
                            cfg.presentmon_window,
                            &self.target.name,
                            phase,
                        );
                        self.last_unavailable = detail.to_string();
                        let _ = tx.send(Update {
                            generation,
                            game,
                            detail: detail.to_string(),
                            ..Default::default()
                        });
                    }
                }
                if idle > ACTIVITY_EXPIRY {
                    let expired = self.target.clone();
                    self.activity.remove(&expired);
                    self.select_presenter(ProcessInfo::default(), cfg);
                }
            }
        }
        if self.target.pid == 0 {
            self.pick_presenter(cfg, now, foreground_pid);
        }
    }

    fn pick_presenter(&mut self, cfg: &Config, now: Instant, foreground_pid: impl FnOnce() -> u32) {
        if self.activity.candidates.is_empty() {
            self.challenger = None;
            return;
        }
        let Some(candidate) = self.activity.ranked(
            now,
            ForegroundInfo {
                pid: foreground_pid(),
            },
            &self.target,
            selection_catalog(cfg, &self.catalog.state),
        ) else {
            self.challenger = None;
            return;
        };
        if self.target.pid == 0 {
            self.select_presenter(candidate.process, cfg);
            return;
        }
        if candidate.process == self.target {
            self.challenger = None;
            return;
        }
        let Some(workload) = candidate.workload else {
            self.challenger = None;
            return;
        };
        let current = self
            .activity
            .workload_score(&self.target)
            .map(|score| score.value)
            .unwrap_or(0.0);
        if workload.value < current + SWITCH_MARGIN {
            self.challenger = None;
            return;
        }
        let key = CandidateKey::new(&candidate.process);
        match &mut self.challenger {
            Some(challenger)
                if challenger.key == key && workload.window == challenger.window + 1 =>
            {
                challenger.window = workload.window;
                challenger.wins = challenger.wins.saturating_add(1);
            }
            Some(challenger) if challenger.key == key && workload.window == challenger.window => {}
            _ => {
                self.challenger = Some(ChallengerState {
                    key,
                    window: workload.window,
                    wins: 1,
                });
            }
        }
        if self
            .challenger
            .as_ref()
            .is_some_and(|challenger| challenger.wins >= SWITCH_WINDOWS)
        {
            self.select_presenter(candidate.process, cfg);
        }
    }

    fn start(
        &mut self,
        executable: &PinnedExecutable,
        target: ProcessInfo,
        cfg: &Config,
        key: String,
        confirm_start: &dyn Fn(&ProcessInfo, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.teardown_pending() {
            return Err("PresentMon teardown is still pending; refusing a new capture".into());
        }
        let current = pin_executable(&executable.path)?;
        if current.identity != executable.identity {
            return Err("PresentMon path identity changed after validation".into());
        }
        // Command cannot launch an existing file handle. Holding both file identities and every
        // parent without delete sharing narrows the remaining same-user path race to CreateProcess.
        let presenting = cfg.presentmon_target_mode == "presenting";
        let session_name = new_session_name(if presenting {
            std::process::id()
        } else {
            target.pid
        })?;
        let args = presentmon_args((!presenting).then_some(target.pid), &session_name);
        let _held_through_spawn = (
            &executable.file,
            &executable.parents,
            &current.file,
            &current.parents,
        );
        let mut command = Command::new(&executable.path);
        command
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        confirm_start(&target, &key)?;
        let child = command.spawn();
        let child = match child {
            Ok(child) => {
                self.owned_session = Some(session_name);
                child
            }
            Err(error) => {
                return Err(format!(
                    "start PresentMon {}: {error}",
                    executable.path.display()
                ));
            }
        };
        self.child = Some(child);
        let Some(stdout) = self.child.as_mut().and_then(|child| child.stdout.take()) else {
            return Err(self.stop_error("PresentMon stdout pipe unavailable".into()));
        };
        let Some(stderr) = self.child.as_mut().and_then(|child| child.stderr.take()) else {
            return Err(self.stop_error("PresentMon stderr pipe unavailable".into()));
        };
        let (line_tx, line_rx) = mpsc::sync_channel(LINE_QUEUE_CAPACITY);
        let overflow = Arc::new(AtomicBool::new(false));
        let reader_overflow = Arc::clone(&overflow);
        let reader = match std::thread::Builder::new()
            .name("presentmon-output".into())
            .spawn(move || read_output(BufReader::new(stdout), line_tx, reader_overflow))
        {
            Ok(reader) => reader,
            Err(error) => {
                return Err(self.stop_error(format!("start PresentMon output reader: {error}")));
            }
        };
        self.pump_line = None;
        self.lines = Some(line_rx);
        self.line_overflow = Some(overflow);
        self.reader = Some(reader);
        let diagnostic = Arc::new(std::sync::Mutex::new(BoundedDiagnostic::default()));
        let diagnostic_output = Arc::clone(&diagnostic);
        let stderr_reader = match std::thread::Builder::new()
            .name("presentmon-stderr".into())
            .spawn(move || read_diagnostic(stderr, diagnostic_output))
        {
            Ok(reader) => reader,
            Err(error) => {
                return Err(self.stop_error(format!("start PresentMon stderr reader: {error}")));
            }
        };
        self.stderr = Some(diagnostic);
        self.stderr_reader = Some(stderr_reader);
        self.target = target;
        self.key = key;
        self.parser = Parser::default();
        self.stats =
            (!presenting).then(|| Stats::new(cfg.stutter_threshold_ms, cfg.presentmon_window));
        self.activity = ActivityTracker::default();
        self.catalog = CatalogCache::default();
        self.challenger = None;
        self.last_frame = None;
        self.last_publish = None;
        self.started_at = Some(Instant::now());
        self.retry_after = None;
        Ok(())
    }

    fn drain(
        &mut self,
        cfg: &Config,
        generation: u64,
        tx: &mpsc::Sender<Update>,
        now: Instant,
    ) -> Result<(), String> {
        self.drain_with(
            cfg,
            generation,
            tx,
            now,
            &mut |activity| {
                activity
                    .handle
                    .as_ref()
                    .is_some_and(ProcessHandle::is_running)
            },
            &mut |pid| open_process_info(pid).map(|(process, handle)| (process, Some(handle))),
        )
    }

    fn drain_with(
        &mut self,
        cfg: &Config,
        generation: u64,
        tx: &mpsc::Sender<Update>,
        now: Instant,
        valid: &mut impl FnMut(&RecentActivity) -> bool,
        resolve: &mut impl FnMut(u32) -> Result<(ProcessInfo, Option<ProcessHandle>), String>,
    ) -> Result<(), String> {
        let presenting = cfg.presentmon_target_mode == "presenting";
        let Some(lines) = &self.lines else {
            return Ok(());
        };
        let mut identity_queries = 0;
        let mut selected_invalid = false;
        for _ in 0..MAX_LINES_PER_TICK {
            // WO-OPT02C (C1.4): a line the data-driven step already took from the
            // channel is drained first, so `pump_step` never loses CSV data.
            let line = match self.pump_line.take() {
                Some(line) => line,
                None => match lines.try_recv() {
                    Ok(line) => line,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err("PresentMon output reader disconnected".into());
                    }
                },
            };
            let mut frame = match self.parser.parse_line(&line, SystemTime::now()) {
                Ok(Some(frame)) => frame,
                Ok(None) => continue,
                Err(error) => return Err(format!("PresentMon CSV invalid: {error}")),
            };
            frame.observed_at = now;
            if presenting {
                if !candidate_allowed(
                    frame.pid,
                    &frame.application,
                    &cfg.presentmon_exclude,
                    std::process::id(),
                    own_process_name(),
                ) {
                    continue;
                }
                let protected = self.target.clone();
                let identity = self.activity.record(
                    &frame,
                    &protected,
                    selection_catalog(cfg, &self.catalog.state),
                    &mut identity_queries,
                    valid,
                    resolve,
                );
                if self.target.pid != 0 && !self.activity.contains(&self.target) {
                    selected_invalid = true;
                }
                if !selected_invalid && identity.as_ref() == Some(&self.target) {
                    self.stats
                        .as_mut()
                        .expect("selected presenter has statistics")
                        .add(&frame)
                        .map_err(str::to_string)?;
                    self.last_frame = Some(now);
                    self.last_unavailable.clear();
                }
            } else {
                if !same_process(&frame, &self.target) {
                    continue;
                }
                self.stats
                    .as_mut()
                    .unwrap()
                    .add(&frame)
                    .map_err(str::to_string)?;
                self.last_frame = Some(now);
                self.last_unavailable.clear();
            }
        }
        if selected_invalid {
            self.select_presenter(ProcessInfo::default(), cfg);
        }
        if self
            .line_overflow
            .as_ref()
            .is_some_and(|overflow| overflow.load(Ordering::Acquire))
        {
            return Err("PresentMon output overflowed; statistics reset".into());
        }
        if self.last_frame > self.last_publish
            && self
                .last_publish
                .map(|last| now.duration_since(last) >= cfg.presentmon_interval)
                .unwrap_or(true)
        {
            self.last_publish = Some(now);
            // WO-OPT05 A2.2: `as_mut()` is only for the scratch buffer. The mutable
            // borrow of `self.stats` and the shared borrow of `self.target.name` are
            // DISJOINT FIELDS of `Capture`, so the process name goes in by reference:
            // no `to_string()`/`clone()` is added to this path (that allocation is the
            // one this unit removes, not one to reintroduce).
            let game = self.stats.as_mut().unwrap().model(
                SystemTime::now(),
                now,
                cfg.presentmon_window,
                &self.target.name,
            );
            let _ = tx.send(Update {
                generation,
                available: game.active,
                error: false,
                detail: self.target.name.clone(),
                game,
            });
        }
        Ok(())
    }

    fn select_presenter(&mut self, target: ProcessInfo, cfg: &Config) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.challenger = None;
        self.stats = (self.target.pid != 0)
            .then(|| Stats::new(cfg.stutter_threshold_ms, cfg.presentmon_window));
        self.last_frame = None;
        self.last_publish = None;
        self.last_unavailable.clear();
    }

    fn stop(&mut self) -> String {
        self.stop_with(
            |child| stop_child_bounded(child, Duration::from_millis(STOP_TIMEOUT_MS.into())),
            stop_etw_session,
        )
    }

    fn stop_with(
        &mut self,
        stop_child: impl FnOnce(&mut Child) -> Result<ChildExit, String>,
        mut cleanup: impl FnMut(&str) -> Result<(), String>,
    ) -> String {
        let mut shutdown = Vec::new();
        self.stop_diagnostic.clear();
        if let Some(mut child) = self.child.take() {
            if let Err(error) = stop_child(&mut child) {
                self.stop_diagnostic = format!("teardown: {error}");
                self.child = Some(child);
                // WO-U46I (#13): an unconfirmed child stop keeps the child handle (the retry
                // ladder owns that resource), but the ETW session is THIS instance's own named
                // registration, and the presenting-mode child carries no `--terminate_on_proc_exit`
                // (`presentmon_args`), so the session must not stay registered behind the retry.
                // The cleanup is attempted here and its outcome is reported as a SEPARATE segment
                // next to the child-stop outcome.
                self.cleanup_owned_session(&mut cleanup);
                return String::new();
            }
        }
        self.lines = None;
        self.pump_line = None;
        self.line_overflow = None;
        let deadline = Instant::now() + Duration::from_millis(STOP_TIMEOUT_MS.into());
        if let Some(reader) = self.reader.take() {
            if let Err(error) = join_thread_bounded(reader, deadline) {
                shutdown.push(format!("output {error}"));
            }
        }
        let stderr_finished = self.stderr_reader.take().is_none_or(|reader| {
            join_thread_bounded(reader, deadline)
                .map_err(|error| shutdown.push(format!("stderr {error}")))
                .is_ok()
        });
        let stderr = take_diagnostic(self.stderr.take(), stderr_finished);
        if !shutdown.is_empty() {
            self.stop_diagnostic = format!("shutdown: {}", shutdown.join("; "));
        }
        self.cleanup_owned_session(&mut cleanup);
        self.target = ProcessInfo::default();
        self.key.clear();
        self.stats = None;
        self.activity = ActivityTracker::default();
        self.catalog = CatalogCache::default();
        self.challenger = None;
        self.last_frame = None;
        self.last_publish = None;
        self.started_at = None;
        stderr
    }

    /// WO-U46I (#13): clean up exactly THIS instance's owned named session. Idempotent (a cleared
    /// `owned_session` is never cleaned again) and never an unrelated session. The child-handle
    /// ownership is a separate resource and is not part of this outcome; a refused cleanup keeps
    /// the claim so `teardown_pending` schedules the retry, as shipped.
    fn cleanup_owned_session(&mut self, cleanup: &mut impl FnMut(&str) -> Result<(), String>) {
        let cleanup_error = self
            .owned_session
            .as_deref()
            .and_then(|session| cleanup(session).err());
        if let Some(error) = cleanup_error {
            if !self.stop_diagnostic.is_empty() {
                self.stop_diagnostic.push_str("; ");
            }
            self.stop_diagnostic
                .push_str(&format!("ETW cleanup: {error}"));
        } else {
            self.owned_session = None;
        }
    }

    fn teardown_pending(&self) -> bool {
        self.child.is_some() || self.owned_session.is_some()
    }

    fn stop_error(&mut self, reason: String) -> String {
        let stderr = self.stop();
        with_diagnostic(reason, stderr)
    }

    fn stop_and_report_cleanup(
        &mut self,
        tx: &mpsc::Sender<Update>,
        generation: u64,
        now: Instant,
    ) -> bool {
        if self.teardown_pending() && retry_pending(self.retry_after, now) {
            self.unavailable(tx, generation, "PresentMon cleanup retry cooldown".into());
            return true;
        }
        let stderr = self.stop();
        let failed = self.teardown_pending();
        if !self.stop_diagnostic.is_empty() {
            self.failure(
                tx,
                generation,
                with_diagnostic("PresentMon shutdown incomplete".into(), stderr),
            );
        }
        if failed {
            self.schedule_retry(now);
        }
        failed
    }

    fn unavailable(&mut self, tx: &mpsc::Sender<Update>, generation: u64, detail: String) {
        self.set_unavailable(tx, generation, detail, false);
    }

    fn failure(&mut self, tx: &mpsc::Sender<Update>, generation: u64, detail: String) {
        let detail = if self.stop_diagnostic.is_empty() {
            detail
        } else {
            format!("{detail}; {}", self.stop_diagnostic)
        };
        self.set_unavailable(tx, generation, detail, true);
    }

    fn set_unavailable(
        &mut self,
        tx: &mpsc::Sender<Update>,
        generation: u64,
        detail: String,
        error: bool,
    ) {
        if self.last_unavailable != detail {
            self.last_unavailable = detail.clone();
            let _ = tx.send(Update {
                generation,
                error,
                detail,
                ..Default::default()
            });
        }
    }

    fn schedule_retry(&mut self, now: Instant) {
        self.retry_after = Some(now + RETRY_DELAY);
    }
}

fn retry_pending(retry_after: Option<Instant>, now: Instant) -> bool {
    retry_after.map(|retry| now < retry).unwrap_or(false)
}

/// Which stale arm a frame-idle age selects. The shipped bounds are unchanged:
/// `idle <= STALE_AFTER` is fresh (no projection at all), `idle > 2 * STALE_AFTER` is the
/// expired/unavailable arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StalePhase {
    Stale,
    Expired,
}

fn stale_phase(idle: Duration) -> Option<StalePhase> {
    if idle <= STALE_AFTER {
        return None;
    }
    Some(if idle > STALE_AFTER + STALE_AFTER {
        StalePhase::Expired
    } else {
        StalePhase::Stale
    })
}

/// The two shipped detail strings, verbatim.
fn stale_detail(phase: StalePhase) -> &'static str {
    match phase {
        StalePhase::Stale => "capture stale: no frames for 5 seconds",
        StalePhase::Expired => "capture unavailable: stale data expired",
    }
}

/// WO-OPT05 (A2.3): the publication decision, computed BEFORE any model exists.
/// `true` = the state/detail is a transition, so the model is genuinely needed;
/// `false` = the shipped dedupe would throw the model away, so it is never built.
fn stale_publishes(last_unavailable: &str, phase: StalePhase) -> bool {
    last_unavailable != stale_detail(phase)
}

/// The shipped stale projection's payload, reached only on a transition. `stats` and
/// `process` come from DISJOINT fields of `Capture` at both call sites (`self.stats`
/// vs `self.target.name`), which is why no process-name clone appears on this path.
/// The expired arm is the shipped `GameStats::default()`; the stale arm is the shipped
/// model over `window + STALE_AFTER` with `metric.stale = metric.valid` folding.
fn stale_game(
    stats: &mut Stats,
    now: SystemTime,
    observed_now: Instant,
    window: Duration,
    process: &str,
    phase: StalePhase,
) -> GameStats {
    match phase {
        StalePhase::Expired => GameStats::default(),
        StalePhase::Stale => {
            stale_build_tick();
            let mut game = stats.model(now, observed_now, window + STALE_AFTER, process);
            for metric in [
                &mut game.fps,
                &mut game.one_percent,
                &mut game.point_one_low,
                &mut game.frame_time_ms,
            ] {
                metric.stale = metric.valid;
            }
            game
        }
    }
}

#[derive(Default)]
struct BoundedDiagnostic {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_diagnostic(mut reader: impl Read, output: Arc<std::sync::Mutex<BoundedDiagnostic>>) {
    let mut buffer = [0u8; 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
        let remaining = MAX_STDERR_BYTES.saturating_sub(output.bytes.len());
        output
            .bytes
            .extend_from_slice(&buffer[..count.min(remaining)]);
        output.truncated |= count > remaining;
    }
}

fn diagnostic_text(output: &BoundedDiagnostic) -> String {
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    for variable in ["USERPROFILE", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(variable) {
            if !value.is_empty() {
                text = text.replace(&value, &format!("%{variable}%"));
            }
        }
    }
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text: String = normalized.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    if output.truncated || normalized.chars().count() > MAX_DIAGNOSTIC_CHARS {
        text.push_str(" [truncated]");
    }
    text
}

fn take_diagnostic(
    output: Option<Arc<std::sync::Mutex<BoundedDiagnostic>>>,
    reader_finished: bool,
) -> String {
    if !reader_finished {
        return String::new();
    }
    output
        .map(|output| diagnostic_text(&output.lock().unwrap_or_else(|error| error.into_inner())))
        .unwrap_or_default()
}

fn with_diagnostic(reason: String, diagnostic: String) -> String {
    if diagnostic.is_empty() {
        reason
    } else {
        format!("{reason}; stderr: {diagnostic}")
    }
}

fn read_output(mut reader: impl BufRead, tx: mpsc::SyncSender<String>, dropped: Arc<AtomicBool>) {
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(_) => {
                dropped.store(true, Ordering::Release);
                return;
            }
        };
        if buffer.is_empty() {
            if !overflow && !line.is_empty() {
                if let Ok(text) = String::from_utf8(line) {
                    send_line(&tx, &dropped, text);
                } else {
                    dropped.store(true, Ordering::Release);
                }
            }
            return;
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(buffer.len());
        if !overflow && line.len() + end <= MAX_LINE_BYTES {
            line.extend_from_slice(&buffer[..end]);
        } else {
            overflow = true;
            dropped.store(true, Ordering::Release);
        }
        let complete = buffer.get(end.wrapping_sub(1)) == Some(&b'\n');
        reader.consume(end);
        if complete {
            if !overflow {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                if let Ok(text) = String::from_utf8(std::mem::take(&mut line)) {
                    if !send_line(&tx, &dropped, text) {
                        return;
                    }
                } else {
                    dropped.store(true, Ordering::Release);
                }
            }
            line.clear();
            overflow = false;
        }
    }
}

fn send_line(tx: &mpsc::SyncSender<String>, dropped: &AtomicBool, line: String) -> bool {
    match tx.try_send(line) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(_)) => {
            dropped.store(true, Ordering::Release);
            true
        }
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn capture_key(cfg: &Config, target: &ProcessInfo) -> String {
    let target_pid = if cfg.presentmon_target_mode == "presenting" {
        0
    } else {
        target.pid
    };
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        cfg.presentmon_path,
        cfg.presentmon_target_mode,
        target_pid,
        if cfg.presentmon_target_mode == "presenting" {
            0
        } else {
            target.creation_time
        },
        cfg.presentmon_interval.as_millis(),
        cfg.presentmon_window.as_millis(),
        cfg.stutter_threshold_ms,
        cfg.presentmon_target_mode == "presenting" && cfg.presentmon_persist
    )
}

fn startup_timed_out(
    presenting: bool,
    last_frame: Option<Instant>,
    started_at: Option<Instant>,
    has_header: bool,
    now: Instant,
) -> bool {
    last_frame.is_none()
        && started_at.is_some_and(|started| now.duration_since(started) > STARTUP_TIMEOUT)
        && (!presenting || !has_header)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
    size: u64,
}

struct PinnedExecutable {
    path: PathBuf,
    file: File,
    parents: Vec<File>,
    identity: FileIdentity,
}

fn new_session_name(pid: u32) -> Result<String, String> {
    let guid =
        unsafe { CoCreateGuid() }.map_err(|error| format!("create ETW session ID: {error}"))?;
    Ok(format!("LCDSirPlus-{pid}-{:032x}", guid.to_u128()))
}

fn presentmon_args(pid: Option<u32>, session_name: &str) -> Vec<OsString> {
    if let Some(pid) = pid {
        return [
            "--process_id".into(),
            pid.to_string().into(),
            "--output_stdout".into(),
            "--no_console_stats".into(),
            "--terminate_on_proc_exit".into(),
            "--session_name".into(),
            session_name.into(),
            "--v2_metrics".into(),
            "--exclude_dropped".into(),
        ]
        .into();
    }
    [
        "--output_stdout".into(),
        "--v2_metrics".into(),
        "--exclude_dropped".into(),
        "--session_name".into(),
        session_name.into(),
        "--no_console_stats".into(),
    ]
    .into()
}

fn trace_stop_succeeded(status: u32) -> bool {
    [
        ERROR_SUCCESS.0,
        ERROR_MORE_DATA.0,
        ERROR_WMI_INSTANCE_NOT_FOUND.0,
    ]
    .contains(&status)
}

fn stop_etw_session(session_name: &str) -> Result<(), String> {
    let wide: Vec<u16> = session_name.encode_utf16().chain(Some(0)).collect();
    let mut block = [0u64; 600];
    let status = unsafe {
        let properties = block.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        (*properties).Wnode.BufferSize = std::mem::size_of_val(&block) as u32;
        (*properties).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        (*properties).LogFileNameOffset = (*properties).LoggerNameOffset + 2048;
        ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            windows::core::PCWSTR(wide.as_ptr()),
            properties,
            EVENT_TRACE_CONTROL_STOP,
        )
    };
    if trace_stop_succeeded(status.0) {
        Ok(())
    } else {
        Err(format!(
            "stop exact session {session_name}: Win32 {}",
            status.0
        ))
    }
}

fn resolve_executable(setting: &str) -> Result<PinnedExecutable, String> {
    if !setting.trim().is_empty() && !setting.eq_ignore_ascii_case("auto") {
        return validate_executable(Path::new(setting));
    }

    let current = std::env::current_exe()
        .map_err(|e| format!("resolve LCDSirPlus executable for PresentMon: {e}"))?;
    let (candidate, root) = colocated_location(&current)
        .ok_or("LCDSirPlus executable has no parent for PresentMon discovery")?;
    validate_auto_executable(&candidate, &root)
}

fn colocated_location(current: &Path) -> Option<(PathBuf, PathBuf)> {
    current
        .parent()
        .map(|dir| (dir.join("PresentMon.exe"), dir.to_path_buf()))
}

fn validate_auto_executable(path: &Path, root: &Path) -> Result<PinnedExecutable, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("PresentMon trusted root {}: {e}", root.display()))?;
    require_fixed_local_drive(&root)?;
    let executable = validate_executable(path)?;
    if !executable.path.starts_with(&root) {
        return Err("PresentMon candidate escapes its trusted root".into());
    }
    Ok(executable)
}

fn validate_executable(path: &Path) -> Result<PinnedExecutable, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("PresentMon path {}: {e}", path.display()))?;
    require_fixed_local_drive(&canonical)?;
    let name = canonical
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let accepted = name == "presentmon.exe"
        || (name.starts_with("presentmon-") && name.contains("-x64") && name.ends_with(".exe"));
    if !accepted || name.contains("service") || name.contains("gui") {
        return Err("path is not a PresentMon console executable".into());
    }
    let pinned = pin_canonical_executable(canonical)?;
    // WO-U46I (#3a): the signature check goes through the bounded wrapper, so the 50 ms worker
    // never blocks on `WinVerifyTrustEx`. The wrapper receives an OWNED duplicate of the pinned
    // handle, so the pinned-identity and SHA-256 checks below keep the shipped order and content.
    verify_authenticode_bounded(
        &pinned.path,
        pinned.file.try_clone().map_err(|error| {
            format!("open the pinned PresentMon for signature verification: {error}")
        })?,
    )?;
    if pinned.identity.size != PRESENTMON_SIZE
        || held_sha256(&pinned.file, pinned.identity.size)? != PRESENTMON_SHA256
    {
        return Err("PresentMon does not match the pinned v2.5.1 artifact".into());
    }
    Ok(pinned)
}

fn local_drive_letter(path: &Path) -> Option<u8> {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
            _ => None,
        },
        _ => None,
    }
}

fn nvidia_catalog_path() -> Option<PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    let raw =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, HANDLE::default()) }
            .ok()?;
    let root = unsafe { raw.to_string() }.ok().map(PathBuf::from);
    unsafe { CoTaskMemFree(Some(raw.as_ptr().cast())) };
    let root = root?;
    require_fixed_local_drive(&root).ok()?;
    Some(
        root.join("NVIDIA Corporation")
            .join("NVIDIA App")
            .join("NvBackend")
            .join("ApplicationStorage.json"),
    )
}

fn catalog_file(path: &Path) -> Result<(File, CatalogMetadata), CatalogUnavailableReason> {
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    require_fixed_local_drive(path).map_err(|_| CatalogUnavailableReason::UnsafeFile)?;
    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|_| CatalogUnavailableReason::FileUnavailable)?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle().cast()), &mut info) }
        .map_err(|_| CatalogUnavailableReason::UnsafeFile)?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(CatalogUnavailableReason::UnsafeFile);
    }
    verify_handle_path(&file, path).map_err(|_| CatalogUnavailableReason::UnsafeFile)?;
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    if size > MAX_CATALOG_BYTES as u64 {
        return Err(CatalogUnavailableReason::InputOversized);
    }
    Ok((
        file,
        CatalogMetadata {
            volume: info.dwVolumeSerialNumber,
            index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            size,
            modified: ((info.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | info.ftLastWriteTime.dwLowDateTime as u64,
        },
    ))
}

fn read_catalog_attempt(
    path: &Path,
) -> Result<(CatalogMetadata, Vec<u8>, CatalogMetadata), CatalogUnavailableReason> {
    let (mut file, before) = catalog_file(path)?;
    let mut bytes = Vec::with_capacity(before.size as usize);
    (&mut file)
        .take(MAX_CATALOG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CatalogUnavailableReason::ReadFailed)?;
    if bytes.len() > MAX_CATALOG_BYTES || bytes.len() as u64 != before.size {
        return Err(CatalogUnavailableReason::MetadataChanged);
    }
    let (_, after) = catalog_file(path)?;
    Ok((before, bytes, after))
}

fn require_fixed_local_drive(path: &Path) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;

    let letter = local_drive_letter(path)
        .ok_or("PresentMon must use a local drive path, not a UNC or device namespace")?;
    let root: Vec<u16> = format!("{}:\\", (letter as char).to_ascii_uppercase())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    if unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) } != DRIVE_FIXED {
        return Err("PresentMon must be on a fixed local drive".into());
    }
    Ok(())
}

fn pin_executable(path: &Path) -> Result<PinnedExecutable, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("PresentMon path {}: {e}", path.display()))?;
    require_fixed_local_drive(&canonical)?;
    pin_canonical_executable(canonical)
}

fn pin_canonical_executable(path: PathBuf) -> Result<PinnedExecutable, String> {
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, OPEN_EXISTING,
    };

    let parents = pin_parent_chain(path.parent().ok_or("PresentMon path has no parent")?)?;
    let wide = wide(&path);
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| format!("pin PresentMon {}: {e}", path.display()))?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle().cast()), &mut info) }
        .map_err(|e| format!("PresentMon file identity is unavailable: {e}"))?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0
        || info.nNumberOfLinks != 1
    {
        return Err("PresentMon must be a regular non-reparse, single-link file".into());
    }
    verify_handle_path(&file, &path)?;
    Ok(PinnedExecutable {
        path,
        file,
        parents,
        identity: FileIdentity {
            volume: info.dwVolumeSerialNumber,
            index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            size: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        },
    })
}

fn pin_parent_chain(parent: &Path) -> Result<Vec<File>, String> {
    let mut pins = Vec::new();
    let mut current = PathBuf::new();
    let mut rooted = false;
    for component in parent.components() {
        current.push(component.as_os_str());
        match component {
            Component::Prefix(_) => {}
            Component::RootDir => {
                rooted = true;
                pins.push(pin_directory(&current)?);
            }
            Component::Normal(_) if rooted => pins.push(pin_directory(&current)?),
            _ => return Err("PresentMon canonical path contains an invalid component".into()),
        }
    }
    if pins.is_empty() {
        return Err("PresentMon parent path could not be pinned".into());
    }
    Ok(pins)
}

fn pin_directory(path: &Path) -> Result<File, String> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| format!("pin PresentMon parent {}: {e}", path.display()))?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle().cast()), &mut info) }
        .map_err(|e| format!("PresentMon parent identity is unavailable: {e}"))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err("PresentMon parent must be a regular non-reparse directory".into());
    }
    verify_handle_path(&file, path)?;
    Ok(file)
}

fn verify_handle_path(file: &File, expected: &Path) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let mut resolved = vec![0u16; 32_768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(file.as_raw_handle().cast()),
            &mut resolved,
            Default::default(),
        )
    } as usize;
    if length == 0 || length >= resolved.len() {
        return Err("PresentMon pinned path identity is unavailable".into());
    }
    resolved.truncate(length);
    let actual = PathBuf::from(OsString::from_wide(&resolved));
    require_fixed_local_drive(&actual)?;
    if !same_drive_path(&actual, expected) {
        return Err("PresentMon pinned path identity changed".into());
    }
    Ok(())
}

fn same_drive_path(left: &Path, right: &Path) -> bool {
    fn text(path: &Path) -> String {
        let value = path.as_os_str().to_string_lossy();
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
    }
    text(left).eq_ignore_ascii_case(&text(right))
}

fn held_sha256(file: &File, size: u64) -> Result<String, String> {
    let mut hash = crate::sha256::Sha256::new();
    let mut offset = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    while offset < size {
        let wanted = (size - offset).min(buffer.len() as u64) as usize;
        let read = file
            .seek_read(&mut buffer[..wanted], offset)
            .map_err(|e| format!("read pinned PresentMon: {e}"))?;
        if read == 0 {
            return Err("pinned PresentMon changed size while hashing".into());
        }
        hash.update(&buffer[..read]);
        offset += read as u64;
    }
    Ok(crate::sha256::hex(&hash.finish()))
}

/// WO-U46I (#3a): the 50 ms capture worker must not block on `WinVerifyTrustEx` — the trust call
/// performs verification work with no OS bound, and it is reached from the capture start inside
/// the worker loop. The check runs on a dedicated verifier thread and the worker waits a BOUNDED
/// time. The gate slot bounds OUTSTANDING WORK per provider instance: at most one verifier thread
/// is ever in flight, and a timeout fails closed while KEEPING the in-flight thread counted, so a
/// later attempt declines instead of stranding a new thread per timeout. The gate is consumed by
/// the verifier's COMPLETION, never by a timeout-ack, so nothing accumulates across polling,
/// retries or reconfiguration. The bounded precedent is `providers/discord.rs:2205-2226`, whose
/// single-outstanding flag is cleared ONLY by the verifier thread, so after one timeout the flag
/// stays set forever (a permanent lockout); that latch defect is not replicated here.
const VERIFIER_BOUND: Duration = Duration::from_secs(5);
const VERIFIER_BUSY: &str =
    "PresentMon Authenticode verifier is already running; capture not started";
const VERIFIER_TIMEOUT: &str = "PresentMon Authenticode verification timed out; the verifier is \
                                still running; capture not started";

thread_local! {
    /// One gate slot per provider instance: `run` (the capture worker thread) is the only caller,
    /// and a reconfiguration reuses that thread, so the slot spans reconfigures by construction.
    static AUTHENTICODE_GATE: std::cell::RefCell<Option<mpsc::Receiver<Result<(), String>>>> =
        const { std::cell::RefCell::new(None) };
}

/// The gate state machine: Idle (`None`) / Running (a receiver slot). See the const block above.
fn run_bounded_verifier<R, F>(
    gate: &mut Option<mpsc::Receiver<R>>,
    bound: Duration,
    work: F,
) -> Result<R, String>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    if let Some(pending) = gate.as_ref() {
        match pending.try_recv() {
            // The previous verifier returned (or died): THIS completion consumes the gate, so the
            // current attempt is entitled to spawn its own verification.
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => *gate = None,
            // Still in flight: decline without spawning, so the outstanding count stays one.
            Err(mpsc::TryRecvError::Empty) => return Err(VERIFIER_BUSY.into()),
        }
    }
    let (tx, rx) = mpsc::channel();
    let Ok(_verifier) = std::thread::Builder::new()
        .name("presentmon-authenticode".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
    else {
        return Err(
            "PresentMon Authenticode verifier thread could not start; capture not started".into(),
        );
    };
    let receiver = gate.insert(rx);
    match receiver.recv_timeout(bound) {
        Ok(result) => {
            *gate = None;
            Ok(result)
        }
        // FAIL CLOSED with the thread counted: the slot keeps the receiver, so the next attempt
        // declines until this verifier actually returns, and the diagnostic names the outcome.
        Err(_) => Err(VERIFIER_TIMEOUT.into()),
    }
}

/// The bounded wrapper the capture start calls. The inputs are OWNED by the verifier thread (a
/// duplicated file handle + an owned path), so no borrow crosses and the worker never blocks on
/// the OS call itself.
fn verify_authenticode_bounded(path: &Path, file: File) -> Result<(), String> {
    let path = path.to_path_buf();
    AUTHENTICODE_GATE
        .with(|gate| {
            run_bounded_verifier(&mut gate.borrow_mut(), VERIFIER_BOUND, move || {
                verify_authenticode(&file, &path)
            })
        })
        .and_then(|result| result)
}

fn verify_authenticode(file: &File, path: &Path) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Security::Cryptography::{
        CertGetNameStringW, CERT_NAME_SIMPLE_DISPLAY_TYPE,
    };
    use windows::Win32::Security::WinTrust::*;

    let path_wide = wide(path);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: windows::core::PCWSTR(path_wide.as_ptr()),
        hFile: HANDLE(file.as_raw_handle().cast()),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_DISABLE_MD2_MD4,
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
                "PresentMon Authenticode signature is not valid: 0x{status:08x}"
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
        let required = if cert.is_null() {
            0
        } else {
            CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None)
        };
        let simple_name = if required > 1 && required <= 1024 {
            let mut output = vec![0u16; required as usize];
            if CertGetNameStringW(
                cert,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                None,
                Some(&mut output),
            ) == required
            {
                String::from_utf16_lossy(&output[..required as usize - 1])
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrustEx(HWND::default(), &mut action, &mut data);
        if simple_name != PRESENTMON_SIGNER {
            return Err(format!(
                "PresentMon Authenticode signer must be {PRESENTMON_SIGNER}"
            ));
        }
    }
    Ok(())
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn select_target(cfg: &Config) -> Result<ProcessInfo, String> {
    let target = if cfg.presentmon_target_mode == "process_name" {
        find_named_process(&cfg.presentmon_process_name)?
    } else {
        foreground_process()?
    };
    if !candidate_allowed(
        target.pid,
        &target.name,
        &cfg.presentmon_exclude,
        std::process::id(),
        own_process_name(),
    ) {
        Ok(ProcessInfo::default())
    } else {
        Ok(target)
    }
}

fn excluded(name: &str, exclusions: &[String]) -> bool {
    exclusions
        .iter()
        .any(|excluded| excluded.eq_ignore_ascii_case(name))
}

fn own_process_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "LCDSirPlus.exe".into())
    })
}

fn candidate_allowed(
    pid: u32,
    application: &str,
    exclusions: &[String],
    own_pid: u32,
    own_name: &str,
) -> bool {
    let application = application.trim();
    pid != 0
        && pid != own_pid
        && !application.is_empty()
        && !application.chars().any(char::is_control)
        && !["unknown", "<unknown>", "[unknown]", "n/a"]
            .iter()
            .any(|name| application.eq_ignore_ascii_case(name))
        && !application.eq_ignore_ascii_case(own_name)
        && !application.eq_ignore_ascii_case("LCDSirPlus.exe")
        && !application.eq_ignore_ascii_case("LCDSirPlus.Console.exe")
        && !excluded(application, exclusions)
}

fn same_process(frame: &Frame, target: &ProcessInfo) -> bool {
    frame.pid == target.pid
        && (frame.application.is_empty() || frame.application.eq_ignore_ascii_case(&target.name))
}

fn foreground_process() -> Result<ProcessInfo, String> {
    unsafe {
        let window = GetForegroundWindow();
        if window.0.is_null() {
            return Ok(ProcessInfo::default());
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        process_info(pid)
    }
}

/// WO-OPT06 (A1): the value the presenting-ranking closures pass as the foreground preference.
/// The shipped expression was `foreground_process().map(|p| p.pid).unwrap_or(0)`, so ranking saw
/// `0` - "no preference" - on EVERY failure path of the full identity chain, and the pid only when
/// that chain succeeded. This helper reproduces exactly that outcome with the cheapest queries that
/// decide it, and drops the work ranking discards (the 64 KiB UTF-16 buffer, the image-path query,
/// the creation-time query and the owned path/name strings).
///
/// Dropped rows 4-8 are safe for a pid under TWO conditions, and the comment states both, because
/// the second is NOT re-checked per iteration (F-AUD-OPT06-1):
/// (a) ENTRY: `ActivityTracker::record` admits a candidate only after `open_process_info`
///     succeeded, so rows 4-8 passed at least once; and
/// (b) CONTINUATION: every iteration that ranks runs `validate_processes` FIRST, and the
///     foreground pid is filtered through the candidates.
/// For a pid that never passed (a), `required_pid` filtering yields nothing = the shipped 0.
/// For a pid that passed once and LATER denies the image query while still running, (b) does not
/// catch it: `validate_processes` checks handle LIVENESS (`ProcessHandle::is_running`), not a
/// re-validation of the image/creation-time identity. Shipped ranking then yields `0` (no
/// preference) and this path yields the pid. That is the residual, measured as of 2026-11-23:
/// row-4-shaped processes (OpenProcess succeeds, `QueryFullProcessImageNameW` fails, the production
/// mask `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE`, `PROCESS_NAME_WIN32`) are **0 of
/// ~417 pids** across three back-to-back probes (the auditor's own probe twice, the manager's
/// interleaved). An earlier 2-of-420 sighting (`VGC.exe`, `LCore.exe`) is NOT reproducible: the
/// round-1 probe's source is not preserved, `VGC.exe` is not running here, and `LCore.exe` answers
/// the image query with BOTH the 32,768-unit and 260-unit buffers (last error 0). On a
/// NON-ELEVATED host the protected/SYSTEM class fails at `OpenProcess` and lands in row 3a, so a
/// genuine row-4 sighting needs an elevated or PPL-adjacent probe (denied at the QUERY) or the
/// exit race. Population today: 0 - and 0 today proves NOTHING about an anti-cheat title, because
/// the image-query policy is per-process and per-build. No such pid is presenting in any count, so
/// no live pair exists here. Registered as F-AUD-OPT06-1 (P2) with the owner-manual
/// repro (run a game under a protected driver and confirm the presenting choice is unchanged).
/// WO-OPT07 (A1) RULED THAT ROW (NEQ), so the shipped 0 is BEHAVIOUR, not an equivalence, and the
/// row stays OPEN for the owner: in the two branches where the foreground value is read -
/// `best_workload(now, foreground.pid, ..)` here at :1064, reached from TWO producers,
/// `prepare_presenting` -> `pick_presenter` with `self.target` = default (`current.pid == 0`: the
/// startup state, the target-not-in-set reset at :2049-2051, the expiry reset at :2080-2084) AND
/// `reconcile:2029-2030` -> `pick_presenter:2097` with `current.pid != 0` and NO workload score for
/// the current target (:1052), so the divergence is WIDER than the first producer alone - and
/// `best_activity(now, foreground.pid, ..)` at :1074 (producer: no candidate carries a workload
/// score, i.e. a CSV without valid CPU/GPU occupancy (:640-645), fewer than 3 consecutive complete
/// trace seconds, or per-second gaps) -
/// the shipped 0 and the pid pick DIFFERENT winners, pinned by
/// `ranking_foreground_divergent_row_flips_the_presenting_choice`. The round-1 closing proposal
/// ("gate the ranking input on the pid being a CURRENT candidate, a tracked flag") is a NO-OP for
/// this row and was dropped: the row's own premises make that predicate true (the pid was admitted
/// at entry by (a) and is kept running by (b)), so the flag yields the pid, not the shipped 0; and
/// for a pid OUTSIDE the candidate set the value is already outcome-equal to 0 through the
/// `required_pid` filtering (:977, :1012). The only mechanism that reproduces the shipped 0 is
/// re-running the identity query per ranking iteration, which restores the 5 allocations / 1
/// over-256-byte block (65,536 B) / 4 small blocks per call measured in unit 58's A3 census - a
/// revert of this unit, not a fix, so it is NOT done here. No proxy (name compare, creation-time
/// re-read, handle cache) is substituted: a proxy that diverges is a silent behaviour change.
/// Identity validation of candidates, targets, challengers and the selected presenter is NOT
/// relaxed anywhere: only the ranking input is. The relaxation applies to EVERY pid on the
/// dropped rows 4-8, and its outcome matches the shipped one for rows 1-3 (kept) and for every
/// pid that is not a current candidate; for the divergent current-candidate row it does NOT
/// match - that gap is F-AUD-OPT06-1, ruled NEQ above, and it is open to the owner's decision.
fn foreground_ranking_pid() -> u32 {
    ranking_foreground_pid_for(foreground_window_pid())
}

/// WO-OPT06 (A1): the composition, factored over a pid so every row of the failure-path table is
/// testable against the shipped builder (`ranking_foreground_value_matches_the_shipped_identity_chain_on_reachable_pids`).
fn ranking_foreground_pid_for(pid: u32) -> u32 {
    ranking_foreground_pid(pid, foreground_process_openable(pid))
}

/// WO-OPT06 (A1): the outcome contract, pure so every row of the failure-path table is testable.
/// `0` is the shipped `Err`/absent-window value (no foreground preference).
fn ranking_foreground_pid(window_pid: u32, openable: bool) -> u32 {
    if window_pid == 0 || !openable {
        0
    } else {
        window_pid
    }
}

/// WO-OPT06: the window-thread pid of the foreground window, `0` when there is no foreground
/// window - the two shipped paths that already produced a default `ProcessInfo` (pid 0).
fn foreground_window_pid() -> u32 {
    unsafe {
        let window = GetForegroundWindow();
        if window.0.is_null() {
            return 0;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        pid
    }
}

/// WO-OPT06: the kept query - `OpenProcess` with the shipped access mask, closed on drop. The mask
/// is identical to `open_process_info`'s so this probe fails exactly where the shipped chain's
/// first query fails (access denied on an elevated/protected process, exited pid, invalid pid),
/// which is the row that decides whether ranking sees the pid or `0`.
fn foreground_process_openable(pid: u32) -> bool {
    unsafe {
        match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            Ok(handle) => {
                drop(ProcessHandle(handle));
                true
            }
            Err(_) => false,
        }
    }
}

fn process_info(pid: u32) -> Result<ProcessInfo, String> {
    if pid == 0 {
        return Ok(ProcessInfo::default());
    }
    open_process_info(pid).map(|(process, _)| process)
}

fn open_process_info(pid: u32) -> Result<(ProcessInfo, ProcessHandle), String> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
        .map_err(|e| format!("open process {pid}: {e}"))?;
        let handle = ProcessHandle(handle);
        let mut path = vec![0u16; 32_768];
        let mut len = path.len() as u32;
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path.as_mut_ptr()),
            &mut len,
        )
        .map_err(|e| format!("query process {pid}: {e}"))?;
        if len == 0 || len as usize > path.len() {
            return Err(format!("query process {pid}: invalid image path"));
        }
        path.truncate(len as usize);
        let path = PathBuf::from(String::from_utf16_lossy(&path));
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            return Err(format!("query process {pid}: empty image name"));
        }
        let image_path = path.to_string_lossy().into_owned();
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user)
            .map_err(|e| format!("query process {pid} creation time: {e}"))?;
        let creation_time = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        if creation_time == 0 {
            return Err(format!("query process {pid}: invalid creation time"));
        }
        Ok((
            ProcessInfo {
                pid,
                creation_time,
                name,
                image_path,
            },
            handle,
        ))
    }
}

fn find_named_process(name: &str) -> Result<ProcessInfo, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("enumerate processes: {e}"))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(MAX_PATH as usize);
                let process_name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if process_name.eq_ignore_ascii_case(name) {
                    found = Some(process_info(entry.th32ProcessID));
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found.transpose().map(Option::unwrap_or_default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EC-8 (P1 §2 EC-8, acceptance 2): a generation bump while the worker is idle clears the
    /// capture, and with it the publication-dedupe state, so the new generation republishes its
    /// own provider state instead of staying suppressed behind the previous generation's detail.
    /// PresentMon stays disabled: no child process is started and no ETW session is touched.
    #[test]
    fn presentmon_config_change_midflight_clears_capture_and_republishes_the_disabled_reason() {
        let config = Arc::new(RwLock::new(Config {
            presentmon_enabled: false,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        // WO-TEL-D2 (U-3): demand the presentmon readings so the worker reaches
        // `reconcile` (the shipped disabled-config path this test pins); demand
        // 0 would park the worker before the config gate.
        let consumers = Arc::new(AtomicU64::new(crate::telemetry::ALL_EXTENDED_CONSUMERS));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let thread = {
            let config = Arc::clone(&config);
            let generation = Arc::clone(&generation);
            let consumers = Arc::clone(&consumers);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || run(config, generation, consumers, shutdown, tx))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut published = None;
        while published.is_none() && Instant::now() < deadline {
            if let Ok(update) = rx.recv_timeout(Duration::from_millis(100)) {
                if update.generation == 1 {
                    published = Some(update);
                }
            }
        }
        let published = published.expect("the worker publishes generation-1 provider state");
        assert_eq!(published.detail, "disabled");
        generation.fetch_add(1, Ordering::SeqCst);
        let mut republished = None;
        while republished.is_none() && Instant::now() < deadline {
            if let Ok(update) = rx.recv_timeout(Duration::from_millis(100)) {
                if update.generation == 2 {
                    republished = Some(update);
                }
            }
        }
        let update = republished.expect(
            "a mid-flight generation bump clears the capture so the new generation republishes state",
        );
        assert_eq!(update.detail, "disabled");
        assert!(!update.available && !update.error);
        shutdown.store(true, Ordering::Relaxed);
        thread.join().unwrap();
    }

    /// EC-8 (P1 §2 EC-8, acceptance 2): the generation gate that runs immediately before a
    /// capture start refuses a request whose generation moved mid-flight, and a request whose
    /// config content moved while the generation counter did not. Presenting mode only, so no
    /// target enumeration, child process, or ETW session is involved.
    #[test]
    fn presentmon_config_change_midflight_refuses_the_capture_start_gate() {
        let config = Config {
            presentmon_enabled: true,
            presentmon_target_mode: "presenting".into(),
            ..Config::default()
        };
        let shared = RwLock::new(config.clone());
        let generation = AtomicU64::new(2);
        let shutdown = AtomicBool::new(false);
        let target = ProcessInfo::default();
        let key = capture_key(&config, &target);
        assert!(
            confirm_start_request(&shared, &generation, &shutdown, &config, 2, &target, &key)
                .is_ok(),
            "an unchanged generation and config must be accepted"
        );
        let error =
            confirm_start_request(&shared, &generation, &shutdown, &config, 1, &target, &key)
                .unwrap_err();
        assert!(
            error.contains("changed"),
            "a generation that moved mid-flight must refuse the start: {error}"
        );
        let mut moved = config.clone();
        moved.presentmon_interval += Duration::from_millis(1);
        *shared.write().unwrap() = moved.clone();
        let error =
            confirm_start_request(&shared, &generation, &shutdown, &moved, 2, &target, &key)
                .unwrap_err();
        assert!(
            error.contains("changed"),
            "a config change with a stable generation counter must refuse the start: {error}"
        );
    }

    #[test]
    #[ignore = "reads the optional current-user NVIDIA App catalog"]
    fn r6_real_catalog_probe() {
        let path = nvidia_catalog_path().expect("catalog path unavailable");
        let loaded = load_catalog_with(|| read_catalog_attempt(&path));
        let CatalogState::Available(paths) = loaded.state else {
            panic!("catalog unavailable reason={}", loaded.reason.code());
        };
        println!("catalog available qualified={}", paths.len());
    }

    #[test]
    fn parser_handles_legacy_headers_quotes_and_bad_values() {
        let mut parser = Parser::default();
        let at = SystemTime::UNIX_EPOCH;
        assert!(parser
            .parse_line("noise from startup", at)
            .unwrap()
            .is_none());
        assert!(parser
            .parse_line(
                "Application,ProcessID,MsBetweenPresents,MsBetweenDisplayChange,Extra",
                at,
            )
            .unwrap()
            .is_none());
        let frame = parser
            .parse_line("\"game, one.exe\",42,16.5,20.0,x", at)
            .unwrap()
            .unwrap();
        assert_eq!(frame.application, "game, one.exe");
        assert_eq!(frame.pid, 42);
        assert_eq!(frame.frame_ms, 16.5);
        assert!(parser
            .parse_line("game.exe,42,NA,20.0,x", at)
            .unwrap()
            .is_none());
        assert!(parser.parse_line("game.exe,42,NaN,20.0,x", at).is_err());

        let mut display_change = Parser::default();
        display_change
            .parse_line("ProcessID,MsBetweenDisplayChange,Application", at)
            .unwrap();
        assert_eq!(
            display_change
                .parse_line("7,20.0,other.exe", at)
                .unwrap()
                .unwrap()
                .frame_ms,
            20.0
        );
    }

    #[test]
    fn parser_prefers_v2_frame_time_without_displayed_time_fallback() {
        let at = SystemTime::UNIX_EPOCH;
        let mut parser = Parser::default();
        parser
            .parse_line(
                "Application,ProcessID,FrameTime,MsBetweenPresents,DisplayedTime,GPUTime,GPUBusy,GPUWait",
                at,
            )
            .unwrap();
        let frame = parser
            .parse_line("game.exe,42,12.5,16.0,17.0,900,800,700", at)
            .unwrap()
            .unwrap();
        assert_eq!(frame.frame_ms, 12.5);
        assert!(parser
            .parse_line("game.exe,42,NA,16.0,17.0,1,2,3", at)
            .unwrap()
            .is_none());

        let mut displayed_only = Parser::default();
        assert!(displayed_only
            .parse_line("Application,ProcessID,DisplayedTime", at)
            .unwrap_err()
            .contains("expected FrameTime for --v2_metrics"));
    }

    #[test]
    fn parser_accepts_optional_workload_metrics_and_missing_values() {
        let at = SystemTime::UNIX_EPOCH;
        let mut parser = Parser::default();
        parser
            .parse_line(
                "Application,ProcessID,FrameTime,CPUStartTime,CPUBusy,GPUBusy",
                at,
            )
            .unwrap();
        let frame = parser
            .parse_line("game.exe,42,8.33,1234.5,8.0,2.5", at)
            .unwrap()
            .unwrap();
        assert_eq!(frame.cpu_start_ms, Some(1234.5));
        assert_eq!(frame.cpu_busy_ms, Some(8.0));
        assert_eq!(frame.gpu_busy_ms, Some(2.5));

        let missing = parser
            .parse_line("game.exe,42,8.33,,NA,unavailable", at)
            .unwrap()
            .unwrap();
        assert!(
            missing.cpu_start_ms.is_none()
                && missing.cpu_busy_ms.is_none()
                && missing.gpu_busy_ms.is_none()
        );
        let negative = parser
            .parse_line("game.exe,42,8.33,-1,-2,-3", at)
            .unwrap()
            .unwrap();
        assert!(
            negative.cpu_start_ms.is_none()
                && negative.cpu_busy_ms.is_none()
                && negative.gpu_busy_ms.is_none()
        );
        assert!(parser.parse_line("game.exe,42,8.33,NaN,1,1", at).is_err());
        assert!(parser.parse_line("game.exe,42,8.33,1,inf,1", at).is_err());

        let mut renamed = Parser::default();
        renamed
            .parse_line(
                "Application,ProcessID,FrameTime,CPUStartTimeInMs,MsCPUBusy,MsGPUBusy",
                at,
            )
            .unwrap();
        let frame = renamed
            .parse_line("game.exe,42,8.33,2345,7,3", at)
            .unwrap()
            .unwrap();
        assert_eq!(
            (frame.cpu_start_ms, frame.cpu_busy_ms, frame.gpu_busy_ms),
            (Some(2345.0), Some(7.0), Some(3.0))
        );
    }

    /// WO-OPT08 (A1): the 23-column header and the rows below are VERBATIM from the pinned
    /// capture fixture
    /// `artifacts/phase3/operational-soak/opt/fixtures-pinned/presentmon-pinned-live2.csv`
    /// (80,341 B, SHA256-16 `FC228FCDD5D2553E`), whose SEGMENT 1/2 rows are byte-equal to the
    /// live capture `fixtures-live/presentmon-live2.csv` (2,376,062 B, SHA256-16
    /// `2F9F04C03AFBEE15`, 7,237 data rows, every row 23 fields, no quote character in the
    /// file). The emitted column count is therefore the MEASURED 23, never an assumed one.
    /// shortcut: the rows are embedded, the untracked artifact is not opened by this test, so a
    /// fixture edit cannot fail it; the pinned SHA-16 above is the anchor.
    const PINNED_HEADER: &str = "Application,ProcessID,SwapChainAddress,PresentRuntime,SyncInterval,PresentFlags,AllowsTearing,PresentMode,CPUStartTime,FrameTime,CPUBusy,CPUWait,GPULatency,GPUTime,GPUBusy,GPUWait,DisplayLatency,DisplayedTime,AnimationError,AnimationTime,MsFlipDelay,AllInputToPhotonLatency,ClickToPhotonLatency";
    const PINNED_ROW_A: &str = "dwm.exe,2888,0x100000000,Other,-1,0,0,Hardware: Legacy Flip,762.9536,903.0195,903.0195,0.0000,0.0219,903.4377,1.5059,901.9318,905.3810,32.0586,NA,762.9536,NA,NA,NA";
    const PINNED_ROW_B: &str = "dwm.exe,2888,0x100000000,Other,-1,0,0,Hardware: Legacy Flip,1665.9731,32.0668,32.0668,0.0000,0.4401,31.6522,0.9821,30.6701,34.4201,967.9036,870.9609,1665.9731,NA,NA,NA";
    const PINNED_ROW_C: &str = "dwm.exe,2888,0x100000000,Other,-1,0,0,Hardware: Legacy Flip,1698.0399,968.9269,968.9269,0.0000,816.0883,152.8613,1.1246,151.7367,970.2569,50.0256,-935.8368,1698.0399,NA,NA,NA";
    const PINNED_ROW_D: &str = "chrome.exe,27124,0x21DA824ED20,DXGI,0,0,0,Composed: Flip,2483.6828,26.5273,26.3925,0.1348,26.4675,1.2838,1.2838,0.0000,34.8653,206.9345,NA,2483.6828,NA,NA,NA";
    const PINNED_ROW_QUOTED: &str = r#""Game, Inc. [""beta""]",2888,0x100000000,Other,-1,0,0,Hardware: Legacy Flip,762.9536,903.0195,903.0195,0.0000,0.0219,903.4377,1.5059,901.9318,905.3810,32.0586,NA,762.9536,NA,NA,NA"#;
    const PINNED_ROW_NON_NUMERIC: &str = "dwm.exe,2888,0x100000000,Other,-1,0,0,Hardware: Legacy Flip,762.9536,not-a-number,903.0195,0.0000,0.0219,903.4377,1.5059,901.9318,905.3810,32.0586,NA,762.9536,NA,NA,NA";

    /// WO-OPT08 (A1/A3): the per-row CSV path counted on the lane's `alloc_count` allocator.
    /// Asserts are the counter's non-vacuity (a known 1,024 B block), the linearity of the
    /// per-row cost (1/2/4 of the SAME row) and the row bound this unit ships against; the
    /// numbers are PRINTS, because std's layout and growth law is not pinned (the F-AUD-OPT04-1
    /// rule). No CPU is measured and no CPU claim is made - these are counts.
    #[test]
    fn row_split_allocation_cost_is_measured_on_pinned_fixture_rows() {
        use crate::backends::alloc_count;

        let measure = |run: &mut dyn FnMut()| -> alloc_count::Counts {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            run();
            alloc_count::subtract(alloc_count::snapshot(), before)
        };

        // Non-vacuity of the counter: one known 1,024 B block is one over-256-byte allocation.
        let control = measure(&mut || {
            std::hint::black_box(vec![0u8; 1_024]);
        });
        assert_eq!(
            control.4, 1,
            "the counter sees the known 1,024 B block: {control:?}"
        );
        assert_eq!(
            control.3, 1,
            "the known block is the over-256-byte class: {control:?}"
        );

        // The header is accepted OUTSIDE every window, so the window measures the ROW path.
        let mut parser = Parser::default();
        parser
            .parse_line(PINNED_HEADER, SystemTime::UNIX_EPOCH)
            .unwrap();

        let row1 = measure(&mut || {
            std::hint::black_box(
                parser
                    .parse_line(PINNED_ROW_A, SystemTime::UNIX_EPOCH)
                    .unwrap(),
            );
        });
        let row2 = measure(&mut || {
            for _ in 0..2 {
                std::hint::black_box(
                    parser
                        .parse_line(PINNED_ROW_A, SystemTime::UNIX_EPOCH)
                        .unwrap(),
                );
            }
        });
        let row4 = measure(&mut || {
            for _ in 0..4 {
                std::hint::black_box(
                    parser
                        .parse_line(PINNED_ROW_A, SystemTime::UNIX_EPOCH)
                        .unwrap(),
                );
            }
        });
        let a = measure(&mut || {
            std::hint::black_box(
                parser
                    .parse_line(PINNED_ROW_A, SystemTime::UNIX_EPOCH)
                    .unwrap(),
            );
        });
        let b = measure(&mut || {
            std::hint::black_box(
                parser
                    .parse_line(PINNED_ROW_B, SystemTime::UNIX_EPOCH)
                    .unwrap(),
            );
        });
        let c = measure(&mut || {
            std::hint::black_box(
                parser
                    .parse_line(PINNED_ROW_C, SystemTime::UNIX_EPOCH)
                    .unwrap(),
            );
        });
        let d = measure(&mut || {
            std::hint::black_box(
                parser
                    .parse_line(PINNED_ROW_D, SystemTime::UNIX_EPOCH)
                    .unwrap(),
            );
        });
        let quoted = measure(&mut || {
            std::hint::black_box(
                parser
                    .parse_line(PINNED_ROW_QUOTED, SystemTime::UNIX_EPOCH)
                    .unwrap(),
            );
        });
        let batch = measure(&mut || {
            for _ in 0..20 {
                for row in [PINNED_ROW_A, PINNED_ROW_B, PINNED_ROW_C, PINNED_ROW_D] {
                    std::hint::black_box(parser.parse_line(row, SystemTime::UNIX_EPOCH).unwrap());
                }
            }
        });
        println!(
            "WO-OPT08 A1 row-split (pixel, key, metadata<=256, over-256, total): control1024={control:?} row1={row1:?} row2={row2:?} row4={row4:?} rowA={a:?} rowB={b:?} rowC={c:?} rowD={d:?} quoted={quoted:?} batch80={batch:?} foreign={}",
            alloc_count::foreign()
        );

        // Scaling: the per-row cost is linear, so 2 rows is exactly 2x and 4 rows exactly 4x.
        assert_eq!(
            row1.4,
            row2.4 - row1.4,
            "the 2nd row costs the same as the 1st"
        );
        assert_eq!(
            row2.4,
            row4.4 - row2.4,
            "two more rows cost exactly 2x one row"
        );
        assert!(row1.4 > 0, "the row path allocates at all (non-vacuity)");
        // The batch is the sum of its rows: the drain's per-row cost is stateless.
        assert_eq!(
            batch.4,
            20 * (a.4 + b.4 + c.4 + d.4),
            "the 80-row batch is exactly 20 x (A+B+C+D) allocations"
        );
        // The bound this unit ships against: 23 columns split without owning every field.
        assert!(
            a.4 <= 12,
            "one 23-column row costs at most 12 allocations, measured: {a:?}"
        );
        assert!(
            quoted.4 <= a.4 + 8,
            "the escaped/quoted row costs at most 8 more than a plain row, measured: {quoted:?} vs {a:?}"
        );
    }

    /// WO-OPT08 (A3): the shipped CSV outcomes over the pinned fixture matrix - real rows,
    /// quoted commas with escaped quotes, aliases in a non-production header order, optional and
    /// missing columns, whitespace, malformed rows (unterminated quote, too few columns,
    /// non-numeric metric, repeated header, trailing separator) and the no-frame-time-column
    /// error. Every row here is a PINNED SHIPPED OUTCOME; the test drives production
    /// `Parser::parse_line` only.
    #[test]
    fn parser_replays_the_pinned_fixture_matrix_with_identical_outcomes() {
        let at = SystemTime::UNIX_EPOCH;
        let mut parser = Parser::default();
        assert!(parser.parse_line(PINNED_HEADER, at).unwrap().is_none());

        let a = parser.parse_line(PINNED_ROW_A, at).unwrap().unwrap();
        assert_eq!(a.application, "dwm.exe");
        assert_eq!(a.pid, 2888);
        assert_eq!(a.frame_ms, 903.0195);
        assert_eq!(a.cpu_start_ms, Some(762.9536));
        assert_eq!(a.cpu_busy_ms, Some(903.0195));
        assert_eq!(a.gpu_busy_ms, Some(1.5059));
        assert_eq!(a.at, at);
        let b = parser.parse_line(PINNED_ROW_B, at).unwrap().unwrap();
        assert_eq!(
            (b.frame_ms, b.cpu_start_ms, b.cpu_busy_ms, b.gpu_busy_ms),
            (32.0668, Some(1665.9731), Some(32.0668), Some(0.9821))
        );
        let c = parser.parse_line(PINNED_ROW_C, at).unwrap().unwrap();
        assert_eq!(
            (c.frame_ms, c.cpu_start_ms, c.cpu_busy_ms, c.gpu_busy_ms),
            (968.9269, Some(1698.0399), Some(968.9269), Some(1.1246))
        );
        let d = parser.parse_line(PINNED_ROW_D, at).unwrap().unwrap();
        assert_eq!(d.application, "chrome.exe");
        assert_eq!(
            (d.pid, d.frame_ms, d.cpu_busy_ms, d.gpu_busy_ms),
            (27124, 26.5273, Some(26.3925), Some(1.2838))
        );

        // SEGMENT 5: an embedded comma AND escaped quotes inside the Application field.
        let quoted = parser.parse_line(PINNED_ROW_QUOTED, at).unwrap().unwrap();
        assert_eq!(quoted.application, "Game, Inc. [\"beta\"]");
        assert_eq!(quoted.pid, 2888);
        assert_eq!(quoted.frame_ms, 903.0195);
        assert_eq!(quoted.gpu_busy_ms, Some(1.5059));

        // SEGMENT 6: the malformed tail keeps the shipped outcomes.
        assert_eq!(
            parser
                .parse_line(r#""unterminated, 1, 2, 3"#, at)
                .unwrap_err(),
            "unterminated quoted PresentMon CSV field"
        );
        assert!(parser
            .parse_line("a,b", at)
            .unwrap_err()
            .contains("PresentMon ProcessID"));
        assert!(parser
            .parse_line(PINNED_ROW_NON_NUMERIC, at)
            .unwrap_err()
            .contains("PresentMon frame metric"));
        // A repeated header row is a data row once the header is accepted, as shipped.
        assert!(parser
            .parse_line(PINNED_HEADER, at)
            .unwrap_err()
            .contains("PresentMon ProcessID"));
        // A trailing separator adds one empty field and changes nothing else.
        let trailing = parser
            .parse_line(&format!("{PINNED_ROW_A},"), at)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                trailing.application.as_str(),
                trailing.pid,
                trailing.frame_ms,
                trailing.gpu_busy_ms
            ),
            ("dwm.exe", 2888, 903.0195, Some(1.5059))
        );

        // Aliases: the header order is NOT the production order, and names arrive in any case.
        let mut alias = Parser::default();
        alias
            .parse_line(
                "GPUBusy,ProcessID,FrameTime,Application,CPUBusy,CPUStartTime",
                at,
            )
            .unwrap();
        let frame = alias
            .parse_line("1.25,77,8.5,game.exe,9.5,100.5", at)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                frame.application.as_str(),
                frame.pid,
                frame.frame_ms,
                frame.cpu_start_ms,
                frame.cpu_busy_ms,
                frame.gpu_busy_ms
            ),
            ("game.exe", 77, 8.5, Some(100.5), Some(9.5), Some(1.25))
        );
        // Mixed case and header names carrying whitespace: the shipped gate at :1093 needs a
        // field equal to "ProcessID" ignoring CASE only (an untrimmed " processid " header is
        // NOT accepted), while the map keys and the row values ARE trimmed.
        let mut mixed_case = Parser::default();
        mixed_case
            .parse_line(
                " APPLICATION ,PROCESSID, CpuStartTimeInMs ,MsCPUBusy,MsGPUBusy, FrameTime",
                at,
            )
            .unwrap();
        let frame = mixed_case
            .parse_line(" game.exe , 78 , 1234.5 , 7 , 3 , 8.33 ", at)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                frame.application.as_str(),
                frame.pid,
                frame.frame_ms,
                frame.cpu_start_ms,
                frame.cpu_busy_ms,
                frame.gpu_busy_ms
            ),
            ("game.exe", 78, 8.33, Some(1234.5), Some(7.0), Some(3.0))
        );
        // MsBetweenPresents wins over MsBetweenDisplayChange when FrameTime is absent.
        let mut ladder = Parser::default();
        ladder
            .parse_line(
                "ProcessID,MsBetweenDisplayChange,MsBetweenPresents,Application",
                at,
            )
            .unwrap();
        assert_eq!(
            ladder
                .parse_line("7,20.0,11.5,other.exe", at)
                .unwrap()
                .unwrap()
                .frame_ms,
            11.5
        );

        // Optional/missing columns: the shipped empty-string/None outcomes.
        let mut sparse = Parser::default();
        sparse.parse_line("ProcessID,FrameTime", at).unwrap();
        let frame = sparse.parse_line("42,16.5", at).unwrap().unwrap();
        assert_eq!(frame.application, "");
        assert!(
            frame.cpu_start_ms.is_none()
                && frame.cpu_busy_ms.is_none()
                && frame.gpu_busy_ms.is_none()
        );
        // The shipped header gate: an UNTRIMMED " processid " name is not accepted, so the
        // stream stays headerless and every row produces no frame.
        let mut gate = Parser::default();
        assert!(gate
            .parse_line(" APPLICATION , processid , FrameTime", at)
            .unwrap()
            .is_none());
        assert!(gate.parse_line("game.exe,42,8.5", at).unwrap().is_none());
        // A header with no frame-time column keeps the shipped error row.
        let mut no_metric = Parser::default();
        assert!(no_metric
            .parse_line("Application,ProcessID,DisplayedTime", at)
            .unwrap_err()
            .contains("expected FrameTime for --v2_metrics"));
        // A line over the shipped 64 KiB bound is the shipped error, before any split.
        let mut oversized = Parser::default();
        assert!(oversized
            .parse_line(&"x".repeat(MAX_LINE_BYTES + 1), at)
            .unwrap_err()
            .contains("exceeds 64 KiB"));
    }

    /// WO-OPT08 (A3): the pinned rows through the shipped drain - the statistics see the
    /// DRAIN's timestamp, not the parser's clock read (the clock-overwrite ruling), and the
    /// sample-loss flag and the per-tick bound keep firing.
    #[test]
    fn pinned_fixture_rows_keep_the_drain_timestamp_and_sample_loss() {
        let (line_tx, line_rx) = mpsc::sync_channel(LINE_QUEUE_CAPACITY);
        line_tx.send(PINNED_HEADER.into()).unwrap();
        for row in [PINNED_ROW_A, PINNED_ROW_B, PINNED_ROW_NON_NUMERIC] {
            line_tx.send(row.into()).unwrap();
        }
        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.line_overflow = Some(Arc::new(AtomicBool::new(false)));
        capture.target = ProcessInfo {
            pid: 2888,
            creation_time: 1,
            name: "dwm.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(60)));
        let (updates, _rx) = mpsc::channel();
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        // A `now` 5 s in the future: only the drain's stamp can land in the statistics.
        let now = Instant::now() + Duration::from_secs(5);
        assert!(capture
            .drain(&cfg, 1, &updates, now)
            .unwrap_err()
            .contains("PresentMon frame metric"));
        let frames = &capture.stats.as_ref().unwrap().frames;
        assert_eq!(
            frames.len(),
            2,
            "the two valid rows are kept, the bad one is not"
        );
        assert_eq!(
            frames.back(),
            Some(&(now, 32.0668)),
            "the drain's stamp wins"
        );
        assert_eq!(
            frames.front(),
            Some(&(now, 903.0195)),
            "the drain's stamp wins"
        );
        assert_eq!(capture.last_frame, Some(now));

        // Sample-loss detection: a set overflow flag is reported over drained rows.
        let (line_tx, line_rx) = mpsc::sync_channel(LINE_QUEUE_CAPACITY);
        line_tx.send(PINNED_HEADER.into()).unwrap();
        line_tx.send(PINNED_ROW_A.into()).unwrap();
        capture.lines = Some(line_rx);
        capture.parser = Parser::default();
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(60)));
        capture.line_overflow = Some(Arc::new(AtomicBool::new(true)));
        assert_eq!(
            capture
                .drain(&cfg, 1, &updates, Instant::now())
                .unwrap_err(),
            "PresentMon output overflowed; statistics reset"
        );
    }

    #[test]
    fn child_stderr_capture_is_bounded_sanitized_and_preserves_exit_context() {
        let mut child = Command::new("cmd.exe")
            .args([
                "/D",
                "/C",
                "echo failed to start trace session access denied 1>&2 & exit /B 5",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let output = Arc::new(std::sync::Mutex::new(BoundedDiagnostic::default()));
        let reader_output = Arc::clone(&output);
        let reader = std::thread::spawn(move || read_diagnostic(stderr, reader_output));
        let status = child.wait().unwrap();
        reader.join().unwrap();
        assert_eq!(status.code(), Some(5));
        // EC-6: the bounded observation site classifies the real exit and names the code.
        assert_eq!(
            wait_child_bounded(&mut child, Duration::ZERO),
            Ok(ChildExit::Failed(5))
        );
        let reason = ChildExit::Failed(5).capture_end_reason("game.exe");
        assert!(reason.contains("exited_failed_5"), "{reason}");
        let detail = with_diagnostic(reason, diagnostic_text(&output.lock().unwrap()));
        assert!(detail.contains("access denied"));
        // privacy: the classification adds class + code only, never the child's argv or path.
        for leak in ["cmd.exe", "exit /B", "echo", "\\", "/C"] {
            assert!(
                !detail.contains(leak),
                "child detail leaked {leak:?}: {detail}"
            );
        }

        let output = Arc::new(std::sync::Mutex::new(BoundedDiagnostic::default()));
        read_diagnostic(
            std::io::Cursor::new(vec![b'x'; MAX_STDERR_BYTES + 1]),
            Arc::clone(&output),
        );
        let output = output.lock().unwrap();
        assert_eq!(output.bytes.len(), MAX_STDERR_BYTES);
        assert!(output.truncated);
        assert!(diagnostic_text(&output).ends_with("[truncated]"));
    }

    #[test]
    fn session_names_are_unique_and_presentmon_args_are_exact() {
        let first = new_session_name(4242).unwrap();
        assert_ne!(first, new_session_name(4242).unwrap());
        #[rustfmt::skip]
        assert!(first.starts_with("LCDSirPlus-4242-") && first.len() == 48 && first[16..].bytes().all(|byte| byte.is_ascii_hexdigit()));
        for mode in ["foreground", "process_name"] {
            #[rustfmt::skip]
            assert_eq!(
                presentmon_args((mode != "presenting").then_some(4242), &first),
                ["--process_id", "4242", "--output_stdout", "--no_console_stats", "--terminate_on_proc_exit", "--session_name", &first, "--v2_metrics", "--exclude_dropped"].map(OsString::from).to_vec()
            );
        }
        let targetless = presentmon_args(None, &first);
        assert_eq!(
            targetless,
            [
                "--output_stdout",
                "--v2_metrics",
                "--exclude_dropped",
                "--session_name",
                &first,
                "--no_console_stats"
            ]
            .map(OsString::from)
            .to_vec()
        );
        for omitted in ["--process_id", "--process_name", "--terminate_on_proc_exit"] {
            assert!(!targetless.contains(&OsString::from(omitted)));
        }
        assert!(trace_stop_succeeded(ERROR_MORE_DATA.0));
        assert!(!trace_stop_succeeded(5));
        stop_etw_session(&first).unwrap();
        stop_etw_session(&first).unwrap();
    }

    #[test]
    fn stats_compute_lows_stutters_session_and_window() {
        let observed = Instant::now();
        let mut stats = Stats::new(30.0, Duration::from_secs(5));
        for i in 0..100 {
            stats
                .add(&Frame {
                    at: SystemTime::UNIX_EPOCH + Duration::from_millis(i),
                    observed_at: observed + Duration::from_millis(i),
                    frame_ms: (i + 1) as f64,
                    ..Default::default()
                })
                .unwrap();
        }
        let game = stats.model(
            SystemTime::UNIX_EPOCH + Duration::from_millis(100),
            observed + Duration::from_millis(100),
            Duration::from_secs(5),
            "game.exe",
        );
        assert!(game.active);
        assert_eq!(game.game_name, "game");
        assert_eq!(game.stutters, 71);
        assert!((game.one_percent.value - 10.0).abs() < 1e-9);
        assert!((game.point_one_low.value - 10.0).abs() < 1e-9);
        assert_eq!(game.session_start, Some(SystemTime::UNIX_EPOCH));
    }

    #[test]
    fn stutters_saturate_and_new_stats_reset_the_session() {
        let observed = Instant::now();
        let first_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let frame = Frame {
            at: first_at,
            observed_at: observed,
            frame_ms: 30.0,
            ..Default::default()
        };
        let mut stats = Stats::new(30.0, Duration::from_secs(5));
        stats.stutters = i32::MAX - 1;
        stats.add(&frame).unwrap();
        stats.add(&frame).unwrap();
        assert_eq!(stats.stutters, i32::MAX);
        assert_eq!(stats.session_start, Some(first_at));

        let reset = Stats::new(30.0, Duration::from_secs(5));
        assert_eq!(reset.stutters, 0);
        assert_eq!(reset.session_start, None);
    }

    #[test]
    fn capture_key_resets_session_for_runtime_changes_and_target() {
        let mut cfg = Config::default();
        let first = ProcessInfo {
            pid: 1,
            creation_time: 10,
            name: "one.exe".into(),
            image_path: String::new(),
        };
        let second = ProcessInfo {
            pid: 2,
            ..first.clone()
        };
        assert_eq!(capture_key(&cfg, &first), capture_key(&cfg, &second));
        let persistent = Config {
            presentmon_persist: true,
            ..cfg.clone()
        };
        assert_ne!(capture_key(&cfg, &first), capture_key(&persistent, &first));
        cfg.presentmon_target_mode = "foreground".into();
        assert_ne!(capture_key(&cfg, &first), capture_key(&cfg, &second));
        let mut changed = cfg.clone();
        changed.stutter_threshold_ms += 1.0;
        assert_ne!(capture_key(&cfg, &first), capture_key(&changed, &first));
    }

    #[test]
    fn targetless_header_without_rows_does_not_timeout() {
        let now = Instant::now();
        let started = Some(now - STARTUP_TIMEOUT - Duration::from_secs(1));
        assert!(!startup_timed_out(true, None, started, true, now));
    }

    #[test]
    fn targetless_without_header_times_out() {
        let now = Instant::now();
        let started = Some(now - STARTUP_TIMEOUT - Duration::from_secs(1));
        assert!(startup_timed_out(true, None, started, false, now));
    }

    #[test]
    fn target_exclusions_are_case_insensitive_and_exact() {
        let exclusions = vec!["DWM.EXE".to_string(), "lcdsirplus.exe".to_string()];
        assert!(excluded("dwm.exe", &exclusions));
        assert!(!excluded("mydwm.exe", &exclusions));
    }

    #[test]
    fn missing_workload_uses_deterministic_activity_and_foreground_fallback() {
        let now = Instant::now();
        let exclusions = vec!["explorer.exe".into(), "overlay.exe".into()];
        let mut activity = ActivityTracker::default();
        {
            let mut record = |pid, application: &str, at, count| {
                if candidate_allowed(pid, application, &exclusions, 999, "LCDSirPlus.exe") {
                    for _ in 0..count {
                        activity.record_test(
                            &Frame {
                                pid,
                                application: application.into(),
                                observed_at: at,
                                frame_ms: 16.0,
                                ..Default::default()
                            },
                            pid as u64,
                        );
                    }
                }
            };

            record(1, "explorer.exe", now, 100);
            record(2, "overlay.exe", now, 100);
            record(3, "<unknown>", now, 100);
            record(999, "renamed.exe", now, 100);
            record(4, "LCDSirPlus.exe", now, 100);
            for tick in 0..4 {
                let at = now + Duration::from_millis(tick * 50);
                record(42, "NewUnknownGame.exe", at, 12);
                record(7, "Browser.exe", at, 6);
            }
        }

        assert_eq!(activity.candidates.len(), 2);
        assert_eq!(
            activity.strongest(now + Duration::from_millis(200), 7).pid,
            7
        );
        let strongest = activity.strongest(now + Duration::from_millis(200), 0);
        assert_eq!(strongest.pid, 42);
        assert_eq!(strongest.name, "NewUnknownGame.exe");

        let sticky = activity.select(now + Duration::from_secs(1), 7, &strongest, Some(now));
        assert_eq!(sticky, strongest);
        let expiry = now + ACTIVITY_EXPIRY + Duration::from_millis(1);
        activity.record_test(
            &Frame {
                pid: 8,
                application: "Replacement.exe".into(),
                observed_at: expiry,
                frame_ms: 16.0,
                ..Default::default()
            },
            8,
        );
        let reselection = activity.select(expiry, 7, &strongest, Some(now));
        assert_eq!(reselection.name, "Replacement.exe");

        let tie_at = now + Duration::from_secs(20);
        activity.record_test(
            &Frame {
                pid: 20,
                application: "Beta.exe".into(),
                observed_at: tie_at,
                frame_ms: 16.0,
                ..Default::default()
            },
            20,
        );
        activity.record_test(
            &Frame {
                pid: 30,
                application: "Alpha.exe".into(),
                observed_at: tie_at,
                frame_ms: 16.0,
                ..Default::default()
            },
            30,
        );
        assert_eq!(activity.strongest(tie_at, 0).name, "Alpha.exe");

        let mut bounded = ActivityTracker::default();
        for pid in 1..=(MAX_CANDIDATES as u32 + 10) {
            bounded.record_test(
                &Frame {
                    pid,
                    application: format!("candidate-{pid}.exe"),
                    observed_at: tie_at,
                    frame_ms: 16.0,
                    ..Default::default()
                },
                pid as u64,
            );
        }
        assert_eq!(bounded.candidates.len(), MAX_CANDIDATES);
        let repeated = bounded.candidates.keys().next().unwrap().clone();
        for tick in 1..=(MAX_ACTIVITY_BUCKETS + 10) {
            bounded.record_test(
                &Frame {
                    pid: repeated.pid,
                    application: repeated.application.clone(),
                    observed_at: tie_at + Duration::from_millis(tick as u64),
                    frame_ms: 16.0,
                    ..Default::default()
                },
                repeated.creation_time,
            );
        }
        assert_eq!(
            bounded.candidates[&repeated].buckets.len(),
            MAX_ACTIVITY_BUCKETS
        );

        let cfg = Config::default();
        let mut capture = Capture::default();
        capture.target = strongest;
        capture.stats = Some(Stats::new(1.0, cfg.presentmon_window));
        capture
            .stats
            .as_mut()
            .unwrap()
            .add(&Frame {
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
                observed_at: now,
                frame_ms: 16.0,
                ..Default::default()
            })
            .unwrap();
        capture.select_presenter(
            ProcessInfo {
                pid: 7,
                creation_time: 7,
                name: "Browser.exe".into(),
                image_path: String::new(),
            },
            &cfg,
        );
        let reset = capture.stats.as_ref().unwrap();
        assert!(reset.frames.is_empty() && reset.session_start.is_none() && reset.stutters == 0);
    }

    fn record_workload_second(
        activity: &mut ActivityTracker,
        process: &ProcessInfo,
        observed_at: Instant,
        second: u64,
        fps: u32,
        gpu_occupancy: f64,
        cpu_duty: f64,
    ) {
        let frame_ms = 1000.0 / fps as f64;
        for frame in 0..fps {
            activity.record_test_process(
                &Frame {
                    pid: process.pid,
                    application: process.name.clone(),
                    observed_at,
                    frame_ms,
                    cpu_start_ms: Some(second as f64 * 1000.0 + frame as f64 * frame_ms),
                    cpu_busy_ms: Some(frame_ms * cpu_duty),
                    gpu_busy_ms: Some(1000.0 * gpu_occupancy / fps as f64),
                    ..Default::default()
                },
                process,
            );
        }
    }

    fn record_cpu_workload_second(
        activity: &mut ActivityTracker,
        process: &ProcessInfo,
        observed_at: Instant,
        second: u64,
        duty: f64,
        valid_rows: u32,
    ) {
        const FPS: u32 = 100;
        let frame_ms = 1000.0 / FPS as f64;
        for frame in 0..FPS {
            activity.record_test_process(
                &Frame {
                    pid: process.pid,
                    application: process.name.clone(),
                    observed_at,
                    frame_ms,
                    cpu_start_ms: Some(second as f64 * 1000.0 + frame as f64 * frame_ms),
                    cpu_busy_ms: (frame < valid_rows).then_some(frame_ms * duty),
                    ..Default::default()
                },
                process,
            );
        }
    }

    fn catalog_record(
        paths: &[&str],
        creative: bool,
        ops: bool,
        fingerprint: bool,
        manual: bool,
    ) -> String {
        let paths = paths
            .iter()
            .map(|path| format!(r#""{}""#, path.replace('\\', r"\\")))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"DetectedFiles":[{paths}],"IsCreativeApplication":{creative},"IsOpsSupported":{ops},"IsFingerprintDetected":{fingerprint},"IsManuallyAdded":{manual}}}"#
        )
    }

    fn catalog_document(records: &[String]) -> Vec<u8> {
        format!(r#"{{"Applications":[{}]}}"#, records.join(",")).into_bytes()
    }

    #[test]
    fn r5_catalog_requires_exact_eligible_full_path_and_tolerates_stale_fingerprint() {
        let stale_path = r"C:\CatalogFixture\StaleFlag.exe";
        let verified_path = r"C:\CatalogFixture\Verified.exe";
        let records = vec![
            catalog_record(&[stale_path], false, true, false, false),
            catalog_record(&[verified_path], false, true, true, false),
            catalog_record(&[verified_path], false, true, true, false),
            catalog_record(
                &[r"C:\CatalogFixture\Creative.exe"],
                true,
                true,
                true,
                false,
            ),
            catalog_record(
                &[r"C:\CatalogFixture\OpsUnsupported.exe"],
                false,
                false,
                true,
                false,
            ),
            catalog_record(&[r"C:\CatalogFixture\Manual.exe"], false, true, true, true),
            catalog_record(
                &[
                    "Relative.exe",
                    r"\\server\share\Remote.exe",
                    r"\\?\C:\CatalogFixture\Namespaced.exe",
                    r"C:\CatalogFixture\..\Parent.exe",
                    r"C:\CatalogFixture\Alternate.exe:stream",
                ],
                false,
                true,
                false,
                false,
            ),
        ];
        let paths = parse_catalog(&catalog_document(&records)).unwrap();
        assert_eq!(
            paths,
            vec![
                normalize_catalog_path(stale_path).unwrap(),
                normalize_catalog_path(verified_path).unwrap(),
            ]
        );
        let catalog = CatalogState::Available(paths);
        let process = |path: &str| ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Synthetic.exe".into(),
            image_path: path.into(),
        };
        assert_eq!(
            catalog.classify(&process(r"c:\CATALOGfixture\STALEFLAG.EXE")),
            CatalogClassification::Game
        );
        assert_eq!(
            catalog.classify(&process(verified_path)),
            CatalogClassification::Game
        );
        assert_eq!(
            catalog.classify(&process(r"C:\Other\StaleFlag.exe")),
            CatalogClassification::NotGame
        );
        for rejected in [
            r"C:\CatalogFixture\Creative.exe",
            r"C:\CatalogFixture\OpsUnsupported.exe",
            r"C:\CatalogFixture\Manual.exe",
        ] {
            assert_eq!(
                catalog.classify(&process(rejected)),
                CatalogClassification::NotGame
            );
        }
        assert_eq!(
            catalog.classify(&process(r"C:\CatalogFixture\Absent.exe")),
            CatalogClassification::NotGame
        );
        for rejected in [
            "Relative.exe",
            r"\\server\share\Remote.exe",
            r"\\?\C:\CatalogFixture\Namespaced.exe",
            r"C:\CatalogFixture\..\Parent.exe",
            r"C:\CatalogFixture\Alternate.exe:stream",
            r"C:\CatalogFixture\unsafe.\Trailing.exe",
        ] {
            assert!(normalize_catalog_path(rejected).is_none(), "{rejected}");
        }
    }

    #[test]
    fn r5_catalog_schema_and_bounds_fail_to_unavailable() {
        let metadata = CatalogMetadata {
            volume: 1,
            index: 2,
            size: 3,
            modified: 4,
        };
        let unavailable = |bytes: Vec<u8>| {
            matches!(
                load_catalog_with(|| Ok((metadata, bytes.clone(), metadata))).state,
                CatalogState::Unavailable
            )
        };
        for malformed in [
            b"{".to_vec(),
            b"{\"Applications\":[".to_vec(),
            b"[]".to_vec(),
            b"{\"Applications\":{}}".to_vec(),
        ] {
            assert!(unavailable(malformed));
        }
        assert!(unavailable(vec![b' '; MAX_CATALOG_BYTES + 1]));

        let applications = std::iter::repeat_n("null", MAX_CATALOG_APPLICATIONS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(unavailable(
            format!(r#"{{"Applications":[{applications}]}}"#).into_bytes()
        ));
        let paths = vec![r"C:\Games\Game.exe"; MAX_CATALOG_PATHS + 1];
        assert!(unavailable(catalog_document(&[catalog_record(
            &paths, false, true, true, false,
        )])));

        let missing = load_catalog_with(|| Err(CatalogUnavailableReason::FileUnavailable));
        assert!(matches!(missing.state, CatalogState::Unavailable));
        assert!(missing.metadata.is_none());
    }

    #[test]
    fn r6_real_schema_wrapper_skips_invalid_records_without_disabling_catalog() {
        let valid = catalog_record(&[r"C:\Games\Qualified.exe"], false, true, true, false);
        let wrapped = format!(r#"{{"SanitizedApplication":{valid}}}"#);
        let missing_fields = r#"{"SanitizedApplication":{"DetectedFiles":[]}}"#.to_string();
        let wrong_type = r#"{"SanitizedApplication":{"DetectedFiles":[],"IsCreativeApplication":false,"IsOpsSupported":"true","IsFingerprintDetected":true,"IsManuallyAdded":false}}"#.to_string();
        let non_object = "null".to_string();
        let paths = parse_catalog(&catalog_document(&[
            missing_fields,
            wrong_type,
            non_object,
            wrapped,
        ]))
        .unwrap();
        assert_eq!(
            paths,
            vec![normalize_catalog_path(r"C:\Games\Qualified.exe").unwrap()]
        );

        let nonqualifying = catalog_record(&[], true, true, true, false);
        let zero = parse_catalog(&catalog_document(&[
            r#"{"SanitizedApplication":{"DetectedFiles":[]}}"#.to_string(),
            r#"{"First":{"DetectedFiles":[]},"Second":{"IsOpsSupported":true}}"#.to_string(),
            "null".to_string(),
            format!(r#"{{"SanitizedApplication":{nonqualifying}}}"#),
        ]))
        .unwrap();
        assert!(zero.is_empty());

        assert_eq!(
            CatalogDiagnostic::Available(1).message(),
            "PresentMon catalog available qualified=1"
        );
        assert_eq!(
            CatalogDiagnostic::Unavailable(CatalogUnavailableReason::RootShape).message(),
            "PresentMon catalog unavailable reason=root_shape"
        );
    }

    #[test]
    fn r7_catalog_rejects_ambiguous_and_unsupported_record_only_inputs() {
        let qualified = catalog_record(&[r"C:\Games\Qualified.exe"], false, true, true, false);
        let nonqualifying = catalog_record(&[], true, true, true, false);
        let wrapped = |record: &str| format!(r#"{{"SanitizedApplication":{record}}}"#);
        let direct_and_nested = |direct: &str, nested: &str| {
            format!(
                r#"{{{},"SanitizedApplication":{nested}}}"#,
                &direct[1..direct.len() - 1]
            )
        };
        let direct_conflict = direct_and_nested(&qualified, &nonqualifying);
        let nested_conflict = direct_and_nested(&nonqualifying, &qualified);
        let missing = r#"{"SanitizedApplication":{"DetectedFiles":[]}}"#.to_string();
        let wrong_type = r#"{"SanitizedApplication":{"DetectedFiles":[],"IsCreativeApplication":false,"IsOpsSupported":"true","IsFingerprintDetected":true,"IsManuallyAdded":false}}"#.to_string();
        let missing_fingerprint = r#"{"DetectedFiles":[],"IsCreativeApplication":false,"IsOpsSupported":true,"IsManuallyAdded":false}"#.to_string();
        let wrong_fingerprint = r#"{"DetectedFiles":[],"IsCreativeApplication":false,"IsOpsSupported":true,"IsFingerprintDetected":"false","IsManuallyAdded":false}"#.to_string();
        let duplicate = r#"{"DetectedFiles":[],"IsCreativeApplication":false,"IsOpsSupported":true,"IsOpsSupported":true,"IsFingerprintDetected":true,"IsManuallyAdded":false}"#.to_string();
        let unsupported = [
            ("unknown", r#"{"Other":true}"#.to_string()),
            ("null", "null".to_string()),
            ("missing", missing.clone()),
            ("wrong_type", wrong_type.clone()),
            ("missing_fingerprint", missing_fingerprint.clone()),
            ("wrong_fingerprint", wrong_fingerprint.clone()),
            ("ambiguous_direct", direct_conflict.clone()),
            ("ambiguous_nested", nested_conflict.clone()),
            ("duplicate", duplicate.clone()),
        ];
        for (name, record) in unsupported {
            assert_eq!(
                parse_catalog(&catalog_document(&[record])),
                Err(CatalogUnavailableReason::RecordShape),
                "{name}"
            );
        }

        let malformed = vec![
            r#"{"Other":true}"#.to_string(),
            "null".to_string(),
            missing,
            wrong_type,
            missing_fingerprint,
            wrong_fingerprint,
            direct_conflict,
            nested_conflict,
            duplicate,
        ];
        let mut zero_records = malformed.clone();
        zero_records.push(wrapped(&nonqualifying));
        assert!(parse_catalog(&catalog_document(&zero_records))
            .unwrap()
            .is_empty());

        let mut qualified_records = malformed;
        qualified_records.push(wrapped(&qualified));
        assert_eq!(
            parse_catalog(&catalog_document(&qualified_records)).unwrap(),
            vec![normalize_catalog_path(r"C:\Games\Qualified.exe").unwrap()]
        );
    }

    #[test]
    fn r5_catalog_metadata_race_retries_once() {
        let first = CatalogMetadata {
            volume: 1,
            index: 1,
            size: 1,
            modified: 1,
        };
        let second = CatalogMetadata {
            modified: 2,
            ..first
        };
        let bytes = catalog_document(&[catalog_record(
            &[r"C:\Games\Game.exe"],
            false,
            true,
            true,
            false,
        )]);
        let mut calls = 0;
        let settled = load_catalog_with(|| {
            calls += 1;
            if calls == 1 {
                Ok((first, bytes.clone(), second))
            } else {
                Ok((second, bytes.clone(), second))
            }
        });
        assert_eq!(calls, 2);
        assert!(matches!(settled.state, CatalogState::Available(_)));
        assert_eq!(settled.metadata, Some(second));

        calls = 0;
        let changing = load_catalog_with(|| {
            calls += 1;
            Ok((first, bytes.clone(), second))
        });
        assert_eq!(calls, 2);
        assert!(matches!(changing.state, CatalogState::Unavailable));
        assert!(changing.metadata.is_none());
        assert_eq!(changing.reason, CatalogUnavailableReason::MetadataChanged);
    }

    #[test]
    fn r5_selector_uses_stale_fingerprint_catalog_and_unavailable_fallback() {
        let cfg = Config::default();
        let base = Instant::now();
        let catalog_app = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "CatalogApp.exe".into(),
            image_path: r"C:\CatalogFixture\CatalogApp.exe".into(),
        };
        let desktop = ProcessInfo {
            pid: 2,
            creation_time: 2,
            name: "Desktop.exe".into(),
            image_path: r"C:\DesktopFixture\Desktop.exe".into(),
        };
        let busier_desktop = ProcessInfo {
            pid: 3,
            creation_time: 3,
            name: "BusierDesktop.exe".into(),
            image_path: r"C:\DesktopFixture\BusierDesktop.exe".into(),
        };
        let populate = |capture: &mut Capture, processes: &[(&ProcessInfo, f64)]| {
            for second in 0..=3 {
                for (process, duty) in processes {
                    record_cpu_workload_second(
                        &mut capture.activity,
                        process,
                        base + Duration::from_secs(second),
                        second,
                        *duty,
                        100,
                    );
                }
            }
        };

        let mut valid = Capture::default();
        populate(
            &mut valid,
            &[
                (&catalog_app, 0.40),
                (&desktop, 0.90),
                (&busier_desktop, 0.95),
            ],
        );
        valid.catalog.state = CatalogState::Available(
            parse_catalog(&catalog_document(&[catalog_record(
                &[&catalog_app.image_path],
                false,
                true,
                false,
                false,
            )]))
            .unwrap(),
        );
        valid.pick_presenter(&cfg, base + Duration::from_secs(3), || desktop.pid);
        assert_eq!(valid.target, catalog_app);

        let mut no_game = Capture::default();
        populate(&mut no_game, &[(&desktop, 0.90), (&busier_desktop, 0.95)]);
        no_game.catalog.state = CatalogState::Available(Vec::new());
        no_game.pick_presenter(&cfg, base + Duration::from_secs(3), || desktop.pid);
        assert_eq!(no_game.target, ProcessInfo::default());

        let mut unavailable = Capture::default();
        populate(
            &mut unavailable,
            &[
                (&catalog_app, 0.40),
                (&desktop, 0.90),
                (&busier_desktop, 0.95),
            ],
        );
        let metadata = CatalogMetadata {
            volume: 1,
            index: 2,
            size: 3,
            modified: 4,
        };
        unavailable.catalog.state =
            load_catalog_with(|| Ok((metadata, b"{".to_vec(), metadata))).state;
        unavailable.pick_presenter(&cfg, base + Duration::from_secs(3), || 0);
        assert_eq!(unavailable.target, busier_desktop);

        let persistent_cfg = Config {
            presentmon_persist: true,
            ..cfg.clone()
        };
        let mut persistent = Capture::default();
        populate(
            &mut persistent,
            &[
                (&catalog_app, 0.40),
                (&desktop, 0.90),
                (&busier_desktop, 0.95),
            ],
        );
        persistent.catalog.state =
            CatalogState::Available(vec![
                normalize_catalog_path(&catalog_app.image_path).unwrap()
            ]);
        persistent.pick_presenter(&persistent_cfg, base + Duration::from_secs(3), || 0);
        assert_eq!(persistent.target, busier_desktop);

        let targeted_cfg = Config {
            presentmon_target_mode: "foreground".into(),
            presentmon_persist: true,
            ..cfg
        };
        assert!(matches!(
            selection_catalog(&targeted_cfg, &persistent.catalog.state),
            CatalogState::Available(_)
        ));
    }

    #[test]
    fn r19_persist_controls_drain_admission_without_bypassing_identity_or_exclusions() {
        let game = ProcessInfo {
            pid: 10,
            creation_time: 10,
            name: "Game.exe".into(),
            image_path: r"C:\Games\Game.exe".into(),
        };
        let desktop = ProcessInfo {
            pid: 20,
            creation_time: 20,
            name: "Desktop.exe".into(),
            image_path: r"C:\Tools\Desktop.exe".into(),
        };
        let admitted = |cfg: &Config, process: &ProcessInfo, frame_name: &str| {
            let (line_tx, line_rx) = mpsc::sync_channel(4);
            line_tx
                .send("Application,ProcessID,FrameTime".into())
                .unwrap();
            line_tx
                .send(format!("{frame_name},{},16.0", process.pid))
                .unwrap();
            let mut capture = Capture::default();
            capture.lines = Some(line_rx);
            capture.catalog.state =
                CatalogState::Available(vec![normalize_catalog_path(&game.image_path).unwrap()]);
            let (updates, _rx) = mpsc::channel();
            capture
                .drain_with(cfg, 1, &updates, Instant::now(), &mut |_| true, &mut |_| {
                    Ok((process.clone(), None))
                })
                .unwrap();
            capture.activity.contains(process)
        };

        let gated = Config::default();
        assert!(admitted(&gated, &game, &game.name));
        assert!(!admitted(&gated, &desktop, &desktop.name));

        let persistent = Config {
            presentmon_persist: true,
            ..gated.clone()
        };
        assert!(admitted(&persistent, &desktop, &desktop.name));
        assert!(!admitted(&persistent, &desktop, "Wrong.exe"));

        let excluded = Config {
            presentmon_exclude: vec![desktop.name.clone()],
            ..persistent
        };
        assert!(!admitted(&excluded, &desktop, &desktop.name));
    }

    #[test]
    fn r5_catalog_and_process_diagnostics_do_not_expose_private_values() {
        let private_path = r"C:\PrivateFixture\SecretGame.exe";
        let private_name = "Private Catalog Display Name";
        let bytes = format!(
            r#"{{"Applications":[{{"DetectedFiles":["C:\\PrivateFixture\\SecretGame.exe"],"IsCreativeApplication":false,"IsOpsSupported":true,"IsFingerprintDetected":false,"IsManuallyAdded":false,"DisplayName":"{private_name}"}}]}}"#
        );
        assert!(parse_catalog(bytes.as_bytes()).is_ok());
        let process = ProcessInfo {
            pid: 1,
            creation_time: 2,
            name: "SecretGame.exe".into(),
            image_path: private_path.into(),
        };
        let visible = format!(
            "{:?}|{:?}|{}",
            process,
            CandidateKey::new(&process),
            process.name
        );
        assert!(!visible.contains(private_path));
        assert!(!visible.contains(private_name));

        let mut capture = Capture::default();
        capture.catalog.state =
            CatalogState::Available(vec![normalize_catalog_path(private_path).unwrap()]);
        capture.catalog.metadata = Some(CatalogMetadata {
            volume: 1,
            index: 2,
            size: 3,
            modified: 4,
        });
        capture.catalog.checked_at = Some(Instant::now());
        capture.stop_with(|_| Ok(ChildExit::Normal), |_| Ok(()));
        assert!(matches!(capture.catalog.state, CatalogState::Unavailable));
        assert!(capture.catalog.metadata.is_none() && capture.catalog.checked_at.is_none());
    }

    #[test]
    fn r4_global_margin_challenger_is_not_masked_by_comparable_foreground() {
        let cfg = Config::default();
        let base = Instant::now();
        let current = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Current.exe".into(),
            image_path: String::new(),
        };
        let best = ProcessInfo {
            pid: 2,
            creation_time: 2,
            name: "Best.exe".into(),
            image_path: String::new(),
        };
        let foreground = ProcessInfo {
            pid: 3,
            creation_time: 3,
            name: "Foreground.exe".into(),
            image_path: String::new(),
        };
        let mut capture = Capture::default();
        capture.target = current.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_cpu_workload_second(
                &mut capture.activity,
                &current,
                observed_at,
                second,
                0.60,
                100,
            );
            record_cpu_workload_second(
                &mut capture.activity,
                &best,
                observed_at,
                second,
                0.80,
                100,
            );
            record_cpu_workload_second(
                &mut capture.activity,
                &foreground,
                observed_at,
                second,
                0.66,
                100,
            );
        }
        for (process, expected) in [(&current, 0.60), (&best, 0.80), (&foreground, 0.66)] {
            assert!(
                (capture.activity.workload_score(process).unwrap().value - expected).abs() < 1e-9
            );
        }

        capture.pick_presenter(&cfg, base + Duration::from_secs(3), || foreground.pid);
        let challenger = capture.challenger.as_ref().unwrap();
        assert_eq!(challenger.key, CandidateKey::new(&best));
        assert_eq!(challenger.wins, 1);
        assert_eq!(capture.target, current);

        for process in [&current, &best, &foreground] {
            let duty = if process == &current {
                0.60
            } else if process == &best {
                0.80
            } else {
                0.66
            };
            record_cpu_workload_second(
                &mut capture.activity,
                process,
                base + Duration::from_secs(4),
                4,
                duty,
                100,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(4), || foreground.pid);
        assert_eq!(capture.target, best);
        assert!(capture.challenger.is_none());
    }

    #[test]
    fn r4_nonconsecutive_workload_window_resets_challenger_streak() {
        let cfg = Config::default();
        let base = Instant::now();
        let current = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Current.exe".into(),
            image_path: String::new(),
        };
        let challenger = ProcessInfo {
            pid: 2,
            creation_time: 2,
            name: "Challenger.exe".into(),
            image_path: String::new(),
        };
        let mut capture = Capture::default();
        capture.target = current.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_cpu_workload_second(
                &mut capture.activity,
                &current,
                observed_at,
                second,
                0.60,
                100,
            );
            record_cpu_workload_second(
                &mut capture.activity,
                &challenger,
                observed_at,
                second,
                0.80,
                100,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(3), || 0);
        assert_eq!(capture.challenger.as_ref().unwrap().wins, 1);

        for process in [&current, &challenger] {
            let duty = if process == &current { 0.60 } else { 0.80 };
            record_cpu_workload_second(
                &mut capture.activity,
                process,
                base + Duration::from_secs(5),
                5,
                duty,
                100,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(5), || 0);
        assert!(capture.challenger.is_none());

        for process in [&current, &challenger] {
            let duty = if process == &current { 0.60 } else { 0.80 };
            record_cpu_workload_second(
                &mut capture.activity,
                process,
                base + Duration::from_secs(5),
                4,
                duty,
                100,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(5), || 0);
        assert_eq!(capture.challenger.as_ref().unwrap().wins, 1);
        assert_eq!(capture.target, current);

        for process in [&current, &challenger] {
            let duty = if process == &current { 0.60 } else { 0.80 };
            record_cpu_workload_second(
                &mut capture.activity,
                process,
                base + Duration::from_secs(6),
                6,
                duty,
                100,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(6), || 0);
        assert_eq!(capture.target, challenger);
    }

    #[test]
    fn r4_workload_component_validity_boundary_is_inclusive_at_95_percent() {
        let base = Instant::now();
        let invalid = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Invalid.exe".into(),
            image_path: String::new(),
        };
        let valid = ProcessInfo {
            pid: 2,
            creation_time: 2,
            name: "Valid.exe".into(),
            image_path: String::new(),
        };
        let mut activity = ActivityTracker::default();
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_cpu_workload_second(&mut activity, &invalid, observed_at, second, 1.0, 94);
            record_cpu_workload_second(&mut activity, &valid, observed_at, second, 1.0, 95);
        }

        assert!(activity.workload_score(&invalid).is_none());
        assert!((activity.workload_score(&valid).unwrap().value - 0.95).abs() < 1e-9);
    }

    #[test]
    fn r4_latest_trace_bucket_is_excluded_until_the_next_bucket_arrives() {
        let base = Instant::now();
        let process = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Renderer.exe".into(),
            image_path: String::new(),
        };
        let mut activity = ActivityTracker::default();
        for (second, duty) in [0.10, 0.20, 0.30, 0.90].into_iter().enumerate() {
            record_cpu_workload_second(
                &mut activity,
                &process,
                base + Duration::from_secs(second as u64),
                second as u64,
                duty,
                100,
            );
        }
        let partial = activity.workload_score(&process).unwrap();
        assert_eq!(partial.window, 2);
        assert!((partial.value - 0.20).abs() < 1e-9);

        record_cpu_workload_second(
            &mut activity,
            &process,
            base + Duration::from_secs(4),
            4,
            0.40,
            100,
        );
        let completed = activity.workload_score(&process).unwrap();
        assert_eq!(completed.window, 3);
        assert!((completed.value - 0.30).abs() < 1e-9);
    }

    #[test]
    fn workload_game_beats_higher_rate_ui_after_two_completed_windows() {
        let cfg = Config::default();
        let base = Instant::now();
        let game = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "UnknownRenderer.exe".into(),
            image_path: String::new(),
        };
        let ui = ProcessInfo {
            pid: 7,
            creation_time: 2,
            name: "OpenCode.exe".into(),
            image_path: String::new(),
        };
        let mut capture = Capture::default();
        capture.target = ui.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        let old_start = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        capture
            .stats
            .as_mut()
            .unwrap()
            .add(&Frame {
                at: old_start,
                observed_at: base,
                frame_ms: 1000.0 / 280.0,
                ..Default::default()
            })
            .unwrap();
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_workload_second(
                &mut capture.activity,
                &game,
                observed_at,
                second,
                120,
                0.2021,
                0.9918,
            );
            record_workload_second(
                &mut capture.activity,
                &ui,
                observed_at,
                second,
                280,
                0.0174,
                0.0395,
            );
        }
        let game_score = capture.activity.workload_score(&game).unwrap().value;
        let ui_score = capture.activity.workload_score(&ui).unwrap().value;
        let expected_game = 0.65 * (0.2021 / (0.2021 + 0.05)) + 0.35 * 0.9918;
        let expected_ui = 0.65 * (0.0174 / (0.0174 + 0.05)) + 0.35 * 0.0395;
        assert!((game_score - expected_game).abs() < 1e-9);
        assert!((ui_score - expected_ui).abs() < 1e-9);

        capture.pick_presenter(&cfg, base + Duration::from_secs(3), || ui.pid);
        assert_eq!(capture.target, ui);
        assert_eq!(capture.challenger.as_ref().unwrap().wins, 1);
        capture.pick_presenter(&cfg, base + Duration::from_secs(3), || ui.pid);
        assert_eq!(capture.challenger.as_ref().unwrap().wins, 1);

        for process in [&game, &ui] {
            let (fps, gpu, cpu) = if process == &game {
                (120, 0.2021, 0.9918)
            } else {
                (280, 0.0174, 0.0395)
            };
            record_workload_second(
                &mut capture.activity,
                process,
                base + Duration::from_secs(4),
                4,
                fps,
                gpu,
                cpu,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(4), || ui.pid);
        assert_eq!(capture.target, game);
        let stats = capture.stats.as_ref().unwrap();
        assert!(stats.frames.is_empty() && stats.session_start.is_none());
        assert_ne!(stats.session_start, Some(old_start));
        assert!(capture.challenger.is_none());
    }

    #[test]
    fn mature_foreground_only_seeds_when_comparable_and_cannot_break_stickiness() {
        let cfg = Config::default();
        let base = Instant::now();
        let game = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "Background.exe".into(),
            image_path: String::new(),
        };
        let low_ui = ProcessInfo {
            pid: 7,
            creation_time: 2,
            name: "ForegroundUI.exe".into(),
            image_path: String::new(),
        };
        let comparable_foreground = ProcessInfo {
            pid: 8,
            creation_time: 3,
            name: "ForegroundWork.exe".into(),
            image_path: String::new(),
        };
        let mut activity = ActivityTracker::default();
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_workload_second(&mut activity, &game, observed_at, second, 120, 0.20, 0.90);
            record_workload_second(&mut activity, &low_ui, observed_at, second, 280, 0.01, 0.04);
            record_workload_second(
                &mut activity,
                &comparable_foreground,
                observed_at,
                second,
                120,
                0.18,
                0.90,
            );
        }

        let now = base + Duration::from_secs(3);
        let mut selected = Capture::default();
        selected.target = game.clone();
        selected.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        selected.activity = activity;
        selected.pick_presenter(&cfg, now, || low_ui.pid);
        assert_eq!(selected.target, game);
        assert!(selected.challenger.is_none());

        selected.select_presenter(ProcessInfo::default(), &cfg);
        selected.pick_presenter(&cfg, now, || comparable_foreground.pid);
        assert_eq!(selected.target, comparable_foreground);
    }

    /// WO-OPT06 (A1): the shipped ranking expression is
    /// `foreground_process().map(|p| p.pid).unwrap_or(0)`, so ranking saw `0` (no preference) on
    /// every failure path of the full identity chain. This is the outcome contract that the
    /// ranking-only relaxation must keep, stated per row. The rows whose shipped `Err` happens
    /// AFTER `OpenProcess` succeeded (image-path query, image name, creation-time query) are the
    /// rows whose query this unit drops. Their outcome contract is `0` on a pid that is not a
    /// candidate, which is the case these rows produce at ENTRY. The CONTINUATION case - a pid
    /// that passed identity once, later denies the image query while still running and is a
    /// current candidate - is NOT covered by the dropped queries; it is registered as
    /// F-AUD-OPT06-1 (P2) in the `foreground_ranking_pid` header, measured 0 of ~417 pids today,
    /// and closed by the follow-up unit's current-candidate flag. The eligibility row of
    /// `ranking_decision_keeps_the_shipped_foreground_eligibility_and_tie_breaks` proves the
    /// entry-shaped case: such a pid is not an activity candidate, so `0` and the pid give the
    /// same decision.
    #[test]
    fn ranking_foreground_pid_contract_covers_every_shipped_failure_path() {
        let rows = [
            (
                "no foreground window (GetForegroundWindow null)",
                0u32,
                true,
                0u32,
            ),
            ("no foreground window, chain would also fail", 0, false, 0),
            ("window-thread pid 0 (GetWindowThreadProcessId)", 0, true, 0),
            (
                "OpenProcess access denied: elevated/protected game process",
                4242,
                false,
                0,
            ),
            (
                "OpenProcess on an exited pid (pid died between reads)",
                4243,
                false,
                0,
            ),
            ("OpenProcess on an invalid pid", 4244, false, 0),
            (
                "shipped chain Ok: image, name and creation time all valid",
                4245,
                true,
                4245,
            ),
        ];
        // F-AUD-OPT06-A-4: the row print used to carry a hand-modeled `shipped=` column and a
        // literal `EQUAL=True`. The contract rows are compared to literal expectations by the
        // assert above; the REAL shipped-vs-new comparison, computed from the shipped builder,
        // lives in `ranking_foreground_value_matches_the_shipped_identity_chain_on_reachable_pids`.
        for (label, window_pid, openable, expected) in rows {
            assert_eq!(
                ranking_foreground_pid(window_pid, openable),
                expected,
                "A1 row: {label}"
            );
            println!(
                "A1-ROW {label} window_pid={window_pid} openable={openable} contract={expected}"
            );
        }
    }

    /// WO-OPT06 (A1, the trap guard): for every pid this machine can reach, the new chain must
    /// hand ranking the SAME value the shipped full-identity chain handed it. `process_info` is the
    /// shipped builder `foreground_process()` uses, so its `Ok`/`Err` shape is the shipped
    /// outcome. Any row where the two disagree is a behavior change, so this test is the STOP
    /// condition of the unit, not a style check.
    #[test]
    fn ranking_foreground_value_matches_the_shipped_identity_chain_on_reachable_pids() {
        let mut mismatches = 0usize;
        for pid in [std::process::id(), 0, 4, 0x0011_0000, u32::MAX] {
            let shipped = process_info(pid).map(|process| process.pid).unwrap_or(0);
            let openable = foreground_process_openable(pid);
            let new = ranking_foreground_pid_for(pid);
            if shipped != new {
                mismatches += 1;
            }
            println!(
                "A1-EQUIV pid={pid} shipped={shipped} new={new} openable={openable} shipped-chain={} MATCH={}",
                if shipped == 0 && pid != 0 { "Err" } else { "Ok" },
                shipped == new
            );
        }
        assert_eq!(
            mismatches, 0,
            "the ranking-only relaxation must return 0 exactly where the shipped chain returned Err"
        );
    }

    /// WO-OPT06 (A2): the ranking decision on the pure layer, over foreground states a live
    /// desktop cannot produce on demand. Every row compares the shipped value with the value the
    /// new chain produces for that row, so the eligibility and tie-break semantics are pinned
    /// without a game, an elevated process or a real presenter.
    #[test]
    fn ranking_decision_keeps_the_shipped_foreground_eligibility_and_tie_breaks() {
        let base = Instant::now();
        let game = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "Background.exe".into(),
            image_path: String::new(),
        };
        let low_ui = ProcessInfo {
            pid: 7,
            creation_time: 2,
            name: "ForegroundUI.exe".into(),
            image_path: String::new(),
        };
        let comparable = ProcessInfo {
            pid: 8,
            creation_time: 3,
            name: "ForegroundWork.exe".into(),
            image_path: String::new(),
        };
        let mut activity = ActivityTracker::default();
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_workload_second(&mut activity, &game, observed_at, second, 120, 0.20, 0.90);
            record_workload_second(&mut activity, &low_ui, observed_at, second, 280, 0.01, 0.04);
            record_workload_second(
                &mut activity,
                &comparable,
                observed_at,
                second,
                120,
                0.18,
                0.90,
            );
        }
        let now = base + Duration::from_secs(3);
        let catalog = CatalogState::Unavailable;
        let none = ProcessInfo::default();
        let decide = |pid: u32, current: &ProcessInfo| -> ProcessInfo {
            ranking_decision(&activity, now, ForegroundInfo { pid }, current, &catalog)
                .map(|candidate| candidate.process)
                .expect("the fixture always ranks a candidate")
        };
        // foreground absent (the shipped no-window / pid-0 / OpenProcess-denied rows all land here).
        assert_eq!(
            decide(0, &none),
            game,
            "absent foreground takes the global best"
        );
        // foreground = this process, which is not in the activity set.
        assert_eq!(
            decide(std::process::id(), &none),
            game,
            "a foreground pid outside the activity set must not change the pick"
        );
        // foreground = a present candidate whose workload is far below the best.
        assert_eq!(
            decide(low_ui.pid, &none),
            game,
            "a present foreground below the switch margin does not win"
        );
        // foreground = a present candidate within the margin: the shipped preference.
        assert_eq!(
            decide(comparable.pid, &none),
            comparable,
            "a comparable foreground candidate wins, the shipped preference"
        );
        // stickiness: the current presenter keeps a within-margin seat against the global best.
        assert_eq!(
            decide(low_ui.pid, &comparable),
            comparable,
            "the current presenter is not displaced by a weak foreground"
        );
        assert_eq!(
            decide(comparable.pid, &low_ui),
            game,
            "the global best displaces a weak current presenter"
        );

        // The no-workload (activity-only) branch and its tie-breaks, where the foreground input
        // is the whole decision: Beta/Alpha tie on score and the shipped order picks Alpha.
        let tie_at = base + Duration::from_secs(20);
        let mut plain = ActivityTracker::default();
        for (pid, application) in [(20u32, "Beta.exe"), (30u32, "Alpha.exe")] {
            plain.record_test(
                &Frame {
                    pid,
                    application: application.into(),
                    observed_at: tie_at,
                    frame_ms: 16.0,
                    ..Default::default()
                },
                pid as u64,
            );
        }
        let decide_activity = |pid: u32| -> ProcessInfo {
            ranking_decision(&plain, tie_at, ForegroundInfo { pid }, &none, &catalog)
                .map(|candidate| candidate.process)
                .expect("the tie fixture always ranks a candidate")
        };
        assert_eq!(
            decide_activity(0).name,
            "Alpha.exe",
            "the shipped tie-break order"
        );
        assert_eq!(
            decide_activity(20).name,
            "Beta.exe",
            "the foreground pid is the tie-break preference, so a 0-vs-pid difference is observable"
        );
        assert_eq!(
            decide_activity(999).name,
            "Alpha.exe",
            "a foreground pid outside the activity set yields the shipped 0 outcome"
        );
        println!(
            "A2-MATRIX workload-best={} foreground-preference={} weak-foreground={} self-outside-set=Alpha.exe tie-foreground=Beta.exe tie-outside-set=Alpha.exe",
            game.name, comparable.name, low_ui.name
        );
    }

    /// WO-OPT07 (A1): F-AUD-OPT06-1 pinned as (NEQ), over exactly the state dimensions
    /// `ranking_decision` reads. The divergent row is a pid that passed the identity chain at ENTRY
    /// (`ActivityTracker::record` resolves through `open_process_info`, :781), is a CURRENT
    /// candidate kept by `validate_processes` (handle LIVENESS only, :855-862), is presenting, and
    /// LATER denies the image query while running: shipped ranking saw `0` (no foreground
    /// preference), this tree's ranking input sees the pid. The branches that read the value, each
    /// with the code path that produces it:
    ///  - workload branch at :1064, reached with `current.pid == 0` (producer:
    ///    `prepare_presenting` :2087-2088 calls `pick_presenter` while `self.target` is the
    ///    default - the shipped "waiting for an active frame presenter" state) AND with
    ///    `current.pid != 0` and no workload score for the current target (producer: `reconcile`
    ///    :2029-2030 -> `pick_presenter` :2097, guarded at :1052): `0` takes the global best and
    ///    the pid takes the within-margin foreground candidate. FLIP.
    ///  - activity branch, no workload score in the window (producer: the shipped CSV carries no
    ///    valid CPU/GPU occupancy for the buckets, the shape
    ///    `missing_workload_uses_deterministic_activity_and_foreground_fallback` pins): the
    ///    foreground is read at :1074. FLIP.
    ///  - stickiness branch, `current.pid != 0` WITH a workload score (:1051-1063 returns before
    ///    the foreground is ever consulted): both values give the same winner. NO FLIP.
    ///  - foreground pid outside the candidate set: `required_pid` filtering (:977, :1012) yields
    ///    nothing, so both values give the same winner. NO FLIP - and that is the entire domain of
    ///    the round-1 "tracked current-candidate flag" proposal: outcome-equal to `0` for a
    ///    non-candidate, and TRUE (so it keeps the pid) for the divergent row, which is a candidate
    ///    by premise. The flag is a NO-OP; it is not implemented.
    #[test]
    fn ranking_foreground_divergent_row_flips_the_presenting_choice() {
        let base = Instant::now();
        // The shipped fixture vocabulary: the global-best presenter and a foreground presenter
        // inside SWITCH_MARGIN of it (0.835 vs 0.824 on unit 58's workload formula).
        let game = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "BestPresenter.exe".into(),
            image_path: String::new(),
        };
        let near = ProcessInfo {
            pid: 8,
            creation_time: 2,
            name: "NearMargin.exe".into(),
            image_path: String::new(),
        };
        let mut activity = ActivityTracker::default();
        for second in 0..=3 {
            let observed_at = base + Duration::from_secs(second);
            record_workload_second(&mut activity, &game, observed_at, second, 120, 0.20, 0.90);
            record_workload_second(&mut activity, &near, observed_at, second, 120, 0.18, 0.90);
        }
        let now = base + Duration::from_secs(3);
        let catalog = CatalogState::Unavailable;
        let none = ProcessInfo::default();
        let decide = |pid: u32, current: &ProcessInfo| -> ProcessInfo {
            ranking_decision(&activity, now, ForegroundInfo { pid }, current, &catalog)
                .map(|candidate| candidate.process)
                .expect("the fixture always ranks a candidate")
        };
        assert_eq!(
            activity.candidates.len(),
            2,
            "both presenters are CURRENT candidates, the divergent row's premise (b)"
        );
        let workload_with_0 = decide(0, &none);
        let workload_with_pid = decide(near.pid, &none);
        assert_eq!(
            workload_with_0, game,
            "the shipped row-4 outcome: foreground 0 takes the global best workload"
        );
        assert_eq!(
            workload_with_pid, near,
            "this tree's outcome: the within-margin foreground pid keeps the shipped preference"
        );
        assert_ne!(
            workload_with_0, workload_with_pid,
            "A1 (NEQ): the workload-branch presenting choice differs between 0 and the pid"
        );
        // A foreground pid outside the candidate set: already outcome-equal to 0, the whole domain
        // of the dropped tracked-flag proposal.
        assert_eq!(
            decide(std::process::id(), &none),
            workload_with_0,
            "a foreground pid that is not a current candidate is outcome-equal to the shipped 0"
        );
        // The stickiness branch never consults the foreground value, so it cannot diverge.
        assert_eq!(
            decide(0, &near),
            decide(game.pid, &near),
            "a current presenter with a workload score is decided without the foreground input"
        );
        // The activity-only branch (no workload score) and its tie-breaks.
        let tie_at = base + Duration::from_secs(20);
        let mut plain = ActivityTracker::default();
        for (pid, application) in [(20u32, "Beta.exe"), (30u32, "Alpha.exe")] {
            plain.record_test(
                &Frame {
                    pid,
                    application: application.into(),
                    observed_at: tie_at,
                    frame_ms: 16.0,
                    ..Default::default()
                },
                pid as u64,
            );
        }
        let decide_activity = |pid: u32| -> ProcessInfo {
            ranking_decision(&plain, tie_at, ForegroundInfo { pid }, &none, &catalog)
                .map(|candidate| candidate.process)
                .expect("the tie fixture always ranks a candidate")
        };
        let activity_with_0 = decide_activity(0);
        let activity_with_pid = decide_activity(20);
        assert_eq!(
            activity_with_0.name, "Alpha.exe",
            "the shipped tie-break order at foreground 0"
        );
        assert_eq!(
            activity_with_pid.name, "Beta.exe",
            "the foreground pid is the activity-branch preference"
        );
        assert_ne!(
            activity_with_0.name, activity_with_pid.name,
            "A1 (NEQ): the activity-branch presenting choice differs between 0 and the pid"
        );
        println!(
            "A1-NEQ workload fg0={} fgpid={} noncandidate={} sticky0={} stickypid={} activity fg0={} fgpid={} candidates={}",
            workload_with_0.name,
            workload_with_pid.name,
            decide(std::process::id(), &none).name,
            decide(0, &near).name,
            decide(game.pid, &near).name,
            activity_with_0.name,
            activity_with_pid.name,
            activity.candidates.len()
        );
    }

    /// WO-OPT06 (A2): an exited/reused pid between the two reads. Ranking filters by pid only, as
    /// shipped, so the winner is always a TRACKED identity (pid + creation time) and never a bare
    /// pid: the downstream candidate/target/challenger identity validation - untouched by this
    /// unit - is what rejects a stale instance.
    #[test]
    fn ranking_decision_never_adopts_an_untracked_pid_identity() {
        let base = Instant::now();
        let old = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "Exited.exe".into(),
            image_path: String::new(),
        };
        let reused = ProcessInfo {
            pid: 42,
            creation_time: 9,
            name: "Reused.exe".into(),
            image_path: String::new(),
        };
        let mut activity = ActivityTracker::default();
        for (process, count) in [(&old, 4u32), (&reused, 6u32)] {
            for tick in 0..count {
                activity.record_test(
                    &Frame {
                        pid: process.pid,
                        application: process.name.clone(),
                        observed_at: base + Duration::from_millis(tick as u64 * 10),
                        frame_ms: 16.0,
                        ..Default::default()
                    },
                    process.creation_time,
                );
            }
        }
        assert_eq!(activity.candidates.len(), 2, "both instances are tracked");
        let winner = ranking_decision(
            &activity,
            base,
            ForegroundInfo { pid: 42 },
            &ProcessInfo::default(),
            &CatalogState::Unavailable,
        )
        .map(|candidate| candidate.process)
        .expect("the reused pid is tracked");
        assert_ne!(
            winner.creation_time, 0,
            "ranking hands on a tracked identity"
        );
        assert!(
            winner == old || winner == reused,
            "the winner is one of the tracked instances, never a bare pid"
        );
        println!(
            "A2-REUSED pid=42 tracked={} winner={} winner-creation_time={}",
            activity.candidates.len(),
            winner.name,
            winner.creation_time
        );
    }

    /// A3 base-shape fixture (test-harness-fixture): the shipped identity builder's blocks,
    /// measured on the same pid, so the byte totals of the ranking path's base side are MEASURED
    /// decomposition prints and not std-layout asserts.
    fn base_ranking_identity_bytes(pid: u32) -> Option<(usize, usize, usize, usize)> {
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
            .ok()?;
            let handle = ProcessHandle(handle);
            let mut path = vec![0u16; 32_768];
            let mut len = path.len() as u32;
            QueryFullProcessImageNameW(
                handle.0,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(path.as_mut_ptr()),
                &mut len,
            )
            .ok()?;
            if len == 0 || len as usize > path.len() {
                return None;
            }
            path.truncate(len as usize);
            let text = String::from_utf16_lossy(&path);
            let name = Path::new(&text)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_string();
            let image_path = text.to_string();
            Some((
                std::mem::size_of::<u16>() * 32_768,
                text.capacity(),
                name.capacity(),
                image_path.capacity(),
            ))
        }
    }

    /// WO-OPT06 (A3): the allocation census of the relaxation, on the lane's counting allocator.
    /// The asserts are the classes ranking discards (the 64 KiB buffer and the owned path/name
    /// strings) going to ZERO, the base side being non-zero, the scaling of the 64 KiB class with
    /// calls, and the counter's non-vacuity on a known single block. Byte totals are PRINTS: std's
    /// layout and growth law is not pinned (the unit-56 F-AUD-OPT04-1 rule). No CPU is measured and
    /// no CPU claim is made; the per-second figure is a count RATE at the shipped 50 ms cadence.
    #[test]
    fn ranking_foreground_identity_cost_is_measured_and_the_ranking_path_drops_it() {
        use crate::backends::alloc_count;

        let measure = |run: &mut dyn FnMut()| -> alloc_count::Counts {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            run();
            alloc_count::subtract(alloc_count::snapshot(), before)
        };

        // Non-vacuity of the counter: a known single block of 1,024 B (over the 256 B class).
        let known = measure(&mut || {
            std::hint::black_box(vec![0u8; 1_024]);
        });
        assert_eq!(known.4, 1, "the counting allocator sees the known block");
        assert_eq!(known.3, 1, "1,024 B is one over-256-byte block");
        // The shipped buffer's own shape: one block of exactly 65,536 B.
        let buffer = measure(&mut || {
            std::hint::black_box(vec![0u16; 32_768]);
        });
        let buffer_bytes = std::mem::size_of::<u16>() * 32_768;
        assert_eq!(buffer.4, 1, "the shipped 64 KiB buffer is one block");
        assert_eq!(
            buffer.3, 1,
            "the shipped 64 KiB buffer is the over-256-byte class"
        );
        assert_eq!(buffer_bytes, 65_536, "the shipped buffer is 64 KiB");

        // base: the identity builder the shipped ranking closure paid, on a pid whose chain
        // succeeds (the self pid), and the shipped expression as written on the live desktop.
        let self_pid = std::process::id();
        let base1 = measure(&mut || {
            std::hint::black_box(
                process_info(self_pid)
                    .map(|process| process.pid)
                    .unwrap_or(0),
            );
        });
        let base3 = measure(&mut || {
            for _ in 0..3 {
                std::hint::black_box(
                    process_info(self_pid)
                        .map(|process| process.pid)
                        .unwrap_or(0),
                );
            }
        });
        let base_foreground = measure(&mut || {
            std::hint::black_box(foreground_process().map(|process| process.pid).unwrap_or(0));
        });
        // changed: the ranking path of this unit.
        let changed1 = measure(&mut || {
            std::hint::black_box(foreground_ranking_pid());
        });
        let changed20 = measure(&mut || {
            for _ in 0..20 {
                std::hint::black_box(foreground_ranking_pid());
            }
        });

        assert!(base1.4 > 0, "the shipped shape allocates at all");
        assert!(
            base1.3 > 0,
            "the shipped shape pays the 64 KiB-class block ranking discards"
        );
        assert!(
            base1.2 > 0,
            "the shipped shape pays the owned path/name string blocks ranking discards"
        );
        assert_eq!(
            base3.3,
            base1.3 * 3,
            "the 64 KiB-class count scales with the number of shipped calls"
        );
        assert_eq!(
            changed1.3, 0,
            "the ranking path pays no 64 KiB buffer / over-256-byte block"
        );
        assert_eq!(
            changed1.2, 0,
            "the ranking path pays no owned image-path or name string block"
        );
        assert_eq!(changed1.4, 0, "the ranking path allocates nothing per call");
        assert_eq!(
            changed20.4, 0,
            "20 ranking iterations (the shipped 50 ms cadence) allocate nothing"
        );

        let bytes =
            base_ranking_identity_bytes(self_pid).expect("the self pid's identity is queryable");
        assert_eq!(
            bytes.0, 65_536,
            "the measured 64 KiB buffer of the shipped shape"
        );
        assert!(
            bytes.1 > 0 && bytes.2 > 0 && bytes.3 > 0,
            "the shipped shape's owned strings are sized"
        );
        let base_bytes = bytes.0 + bytes.1 + bytes.2 + bytes.3;
        println!(
            "A3-ALLOC base-per-call total={} gt256={} le256={} | base-3-calls total={} gt256={} le256={} | base-foreground-expression total={} gt256={} le256={} | changed-per-call total={} gt256={} le256={} | changed-20-calls total={} gt256={} le256={} | NONVACUOUS known-1024B-block={} SIXTYFOURKIB-BYTES={}",
            base1.4, base1.3, base1.2,
            base3.4, base3.3, base3.2,
            base_foreground.4, base_foreground.3, base_foreground.2,
            changed1.4, changed1.3, changed1.2,
            changed20.4, changed20.3, changed20.2,
            known.4,
            bytes.0
        );
        println!(
            "A3-BYTES base-per-call buffer={} utf16-name-string={} name-string={} image-path-string={} TOTAL={} B (measured decomposition, PRINT not assert) | changed-per-call=0 B",
            bytes.0, bytes.1, bytes.2, bytes.3, base_bytes
        );
        println!(
            "A3-RATE cadence=button_poll 50 ms (config.rs:279) iterations=20/s => shipped ranking turnover {} B/s and {} over-256-byte blocks/s; this unit's ranking path 0 B/s and 0 blocks/s. RATE of counts, NOT CPU.",
            base_bytes * 20,
            base1.3 * 20
        );
    }

    #[test]
    fn workload_switch_requires_margin_and_consecutive_completed_windows() {
        let cfg = Config::default();
        let base = Instant::now();
        let current = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Current.exe".into(),
            image_path: String::new(),
        };
        let close = ProcessInfo {
            pid: 2,
            creation_time: 2,
            name: "Close.exe".into(),
            image_path: String::new(),
        };
        let mut capture = Capture::default();
        capture.target = current.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        for second in 0..=4 {
            let observed_at = base + Duration::from_secs(second);
            record_workload_second(
                &mut capture.activity,
                &current,
                observed_at,
                second,
                60,
                0.10,
                0.20,
            );
            record_workload_second(
                &mut capture.activity,
                &close,
                observed_at,
                second,
                60,
                0.12,
                0.25,
            );
        }
        capture.pick_presenter(&cfg, base + Duration::from_secs(4), || 0);
        assert_eq!(capture.target, current);
        assert!(capture.challenger.is_none());
    }

    #[test]
    fn presenting_flow_post_validation_pid_reuse_resets_stats_and_session() {
        let cfg = Config::default();
        let now = Instant::now();
        let old = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        let replacement = ProcessInfo {
            creation_time: 2,
            ..old.clone()
        };
        let old_start = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let mut capture = Capture::default();
        capture.target = old.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        capture
            .stats
            .as_mut()
            .unwrap()
            .add(&Frame {
                at: old_start,
                observed_at: now,
                frame_ms: 40.0,
                ..Default::default()
            })
            .unwrap();
        capture.last_frame = Some(now);
        capture.activity.record_test(
            &Frame {
                pid: old.pid,
                application: old.name.clone(),
                observed_at: now,
                frame_ms: 40.0,
                ..Default::default()
            },
            old.creation_time,
        );

        capture.activity.validate_with(|_| true);
        let (updates, rx) = mpsc::channel();

        let (line_tx, line_rx) = mpsc::sync_channel(4);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,42,16.0".into()).unwrap();
        capture.lines = Some(line_rx);
        capture
            .drain_with(
                &cfg,
                1,
                &updates,
                now,
                &mut |activity| {
                    assert_eq!(activity.process, old);
                    false
                },
                &mut |_| Ok((replacement.clone(), None)),
            )
            .unwrap();
        assert_eq!(capture.target, ProcessInfo::default());
        assert!(capture.stats.is_none());
        assert!(!capture.activity.contains(&old));
        assert!(capture.activity.contains(&replacement));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        capture.pick_presenter(&cfg, now, || 0);
        assert_eq!(capture.target, replacement);
        assert!(capture.stats.as_ref().unwrap().frames.is_empty());

        line_tx.send("game.exe,42,16.0".into()).unwrap();
        capture
            .drain_with(
                &cfg,
                1,
                &updates,
                now + Duration::from_millis(50),
                &mut |_| true,
                &mut |_| unreachable!("cached creation identity must avoid another query"),
            )
            .unwrap();
        let stats = capture.stats.as_ref().unwrap();
        assert_eq!(stats.frames.len(), 1);
        assert_ne!(stats.session_start, Some(old_start));
    }

    #[test]
    fn presenting_flow_expires_before_resumed_rows_and_selects_other_candidate() {
        let cfg = Config::default();
        let now = Instant::now();
        let old = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        let other = ProcessInfo {
            pid: 7,
            creation_time: 2,
            name: "other.exe".into(),
            image_path: String::new(),
        };
        let old_frame = now - Duration::from_secs(11);
        let old_start = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let mut capture = Capture::default();
        capture.target = old.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        capture
            .stats
            .as_mut()
            .unwrap()
            .add(&Frame {
                at: old_start,
                observed_at: old_frame,
                frame_ms: 40.0,
                ..Default::default()
            })
            .unwrap();
        capture.last_frame = Some(old_frame);
        capture.activity.record_test(
            &Frame {
                pid: old.pid,
                application: old.name.clone(),
                observed_at: old_frame,
                frame_ms: 40.0,
                ..Default::default()
            },
            old.creation_time,
        );
        capture.activity.record_test(
            &Frame {
                pid: other.pid,
                application: other.name.clone(),
                observed_at: now,
                frame_ms: 10.0,
                ..Default::default()
            },
            other.creation_time,
        );
        let (updates, rx) = mpsc::channel();
        capture.prepare_presenting(&cfg, 1, &updates, now, || 0);
        assert_eq!(capture.target, other);
        assert!(rx.recv().unwrap().detail.contains("stale data expired"));

        let (line_tx, line_rx) = mpsc::sync_channel(4);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,42,40.0".into()).unwrap();
        line_tx.send("other.exe,7,10.0".into()).unwrap();
        capture.lines = Some(line_rx);
        capture
            .drain_with(&cfg, 1, &updates, now, &mut |_| true, &mut |pid| {
                assert_eq!(pid, old.pid);
                Ok((old.clone(), None))
            })
            .unwrap();
        let stats = capture.stats.as_ref().unwrap();
        assert_eq!(
            stats
                .frames
                .iter()
                .map(|(_, value)| *value)
                .collect::<Vec<_>>(),
            vec![10.0]
        );
        assert_ne!(stats.session_start, Some(old_start));
    }

    #[test]
    fn presenting_flow_exit_overflow_and_config_cleanup_clear_identity_state() {
        let cfg = Config::default();
        let now = Instant::now();
        let old = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        let other = ProcessInfo {
            pid: 7,
            creation_time: 2,
            name: "other.exe".into(),
            image_path: String::new(),
        };
        let mut capture = Capture::default();
        for process in [&old, &other] {
            capture.activity.record_test(
                &Frame {
                    pid: process.pid,
                    application: process.name.clone(),
                    observed_at: now,
                    frame_ms: 16.0,
                    ..Default::default()
                },
                process.creation_time,
            );
        }
        capture.target = old.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        capture.last_frame = Some(now);
        capture.challenger = Some(ChallengerState {
            key: CandidateKey::new(&other),
            window: 1,
            wins: 1,
        });
        capture
            .activity
            .validate_with(|activity| activity.process != old);
        let (updates, _rx) = mpsc::channel();
        capture.prepare_presenting(&cfg, 1, &updates, now, || 0);
        assert_eq!(capture.target, other);
        assert!(capture.stats.as_ref().unwrap().frames.is_empty());
        assert!(capture.challenger.is_none());

        capture.challenger = Some(ChallengerState {
            key: CandidateKey::new(&old),
            window: 2,
            wins: 1,
        });
        capture.stop_with(|_| Ok(ChildExit::Normal), |_| Ok(()));
        assert!(
            capture.target.pid == 0
                && capture.stats.is_none()
                && capture.activity.candidates.is_empty()
                && capture.activity.streams.is_empty()
                && capture.challenger.is_none()
        );

        capture.target = old.clone();
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        capture.activity.record_test(
            &Frame {
                pid: old.pid,
                application: old.name.clone(),
                observed_at: now,
                frame_ms: 16.0,
                ..Default::default()
            },
            old.creation_time,
        );
        capture.line_overflow = Some(Arc::new(AtomicBool::new(true)));
        capture.challenger = Some(ChallengerState {
            key: CandidateKey::new(&old),
            window: 3,
            wins: 1,
        });
        capture.last_target_poll = Some(Instant::now());
        let (updates, rx) = mpsc::channel();
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        assert!(rx.recv().unwrap().error);
        assert!(
            capture.target.pid == 0
                && capture.stats.is_none()
                && capture.activity.candidates.is_empty()
                && capture.activity.streams.is_empty()
                && capture.challenger.is_none()
        );
    }

    #[test]
    fn current_process_creation_identity_is_stable_and_live() {
        let (first, handle) = open_process_info(std::process::id()).unwrap();
        let second = process_info(std::process::id()).unwrap();
        assert_ne!(first.creation_time, 0);
        assert_eq!(first, second);
        assert!(handle.is_running());
    }

    #[test]
    fn output_reader_discards_oversize_lines_and_recovers() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 1];
        input.extend_from_slice(b"\nvalid\n");
        let (tx, rx) = mpsc::sync_channel(2);
        let dropped = Arc::new(AtomicBool::new(false));
        read_output(std::io::Cursor::new(input), tx, Arc::clone(&dropped));
        assert_eq!(rx.into_iter().collect::<Vec<_>>(), vec!["valid"]);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn output_reader_queues_and_drains_a_stalled_presentmon_burst() {
        const BURST_FRAMES: usize = 1_200;
        let header = "Application,ProcessID,FrameTime,MsBetweenPresents,DisplayedTime,GPUTime,GPUBusy,GPUWait\n";
        let row = "Palworld-Win64-Shipping.exe,4242,16.667,16.667,16.667,4.100,3.900,0.200\n";
        let input = format!("{header}{}", row.repeat(BURST_FRAMES));
        let (line_tx, line_rx) = mpsc::sync_channel(LINE_QUEUE_CAPACITY);
        let keep_connected = line_tx.clone();
        let overflow = Arc::new(AtomicBool::new(false));

        // Read the whole burst before consuming to model a short consumer stall.
        read_output(
            std::io::Cursor::new(input.into_bytes()),
            line_tx,
            Arc::clone(&overflow),
        );
        assert!(!overflow.load(Ordering::Acquire));

        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.line_overflow = Some(overflow);
        capture.target = ProcessInfo {
            pid: 4242,
            creation_time: 1,
            name: "Palworld-Win64-Shipping.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(600)));
        let (updates, _rx) = mpsc::channel();
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        for _ in 0..(BURST_FRAMES + 1).div_ceil(MAX_LINES_PER_TICK) {
            capture.drain(&cfg, 1, &updates, Instant::now()).unwrap();
        }
        assert_eq!(capture.stats.as_ref().unwrap().frames.len(), BURST_FRAMES);
        assert!(matches!(
            capture.lines.as_ref().unwrap().try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(keep_connected);
    }

    #[test]
    fn output_reader_is_bounded_and_reports_saturation_without_blocking() {
        let (tx, rx) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let reader_dropped = Arc::clone(&dropped);
        let reader = std::thread::spawn(move || {
            read_output(
                std::io::Cursor::new(b"one\ntwo\nthree\n"),
                tx,
                reader_dropped,
            )
        });
        reader.join().unwrap();
        assert_eq!(rx.try_recv().unwrap(), "one");
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn drain_is_tick_bounded_and_malformed_rows_fail_statistics() {
        let (line_tx, line_rx) = mpsc::sync_channel(LINE_QUEUE_CAPACITY);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        for _ in 0..MAX_LINES_PER_TICK {
            line_tx.send("game.exe,42,16.0".into()).unwrap();
        }
        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.line_overflow = Some(Arc::new(AtomicBool::new(false)));
        capture.target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(60)));
        let (updates, _rx) = mpsc::channel();
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        capture.drain(&cfg, 1, &updates, Instant::now()).unwrap();
        assert_eq!(
            capture.stats.as_ref().unwrap().frames.len(),
            MAX_LINES_PER_TICK - 1
        );
        assert!(capture.lines.as_ref().unwrap().try_recv().is_ok());

        let (line_tx, line_rx) = mpsc::sync_channel(2);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,not-a-pid,16.0".into()).unwrap();
        capture.lines = Some(line_rx);
        capture.parser = Parser::default();
        assert!(capture
            .drain(&cfg, 1, &updates, Instant::now())
            .unwrap_err()
            .contains("CSV invalid"));
    }

    #[test]
    fn drain_reports_disconnection_after_prior_frames() {
        let (line_tx, line_rx) = mpsc::sync_channel(2);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,42,16.0".into()).unwrap();
        drop(line_tx);
        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(60)));
        let (updates, _rx) = mpsc::channel();
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        let error = capture
            .drain(&cfg, 1, &updates, Instant::now())
            .unwrap_err();
        assert!(capture.last_frame.is_some());
        assert!(error.contains("output reader disconnected"));
    }

    #[test]
    fn output_reader_exits_when_shutdown_drops_its_mailbox() {
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        let dropped = Arc::new(AtomicBool::new(false));
        let reader =
            std::thread::spawn(move || read_output(std::io::Cursor::new(b"line\n"), tx, dropped));
        assert!(reader.join().is_ok());
    }

    #[test]
    fn stale_projection_retains_then_expires_metrics() {
        let frame_at = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
        let observed = Instant::now();
        let mut stats = Stats::new(30.0, Duration::from_secs(10));
        stats
            .add(&Frame {
                at: frame_at,
                observed_at: observed,
                frame_ms: 16.0,
                ..Default::default()
            })
            .unwrap();

        // WO-OPT05: the shipped `stale_projection` is now the `stale_phase` gate plus the
        // transition-gated `stale_game` builder. Same arms, same bounds, same values.
        let stale_arm = stale_phase(Duration::from_secs(6)).expect("6 s idle is stale");
        let stale = stale_game(
            &mut stats,
            frame_at + Duration::from_secs(6),
            observed + Duration::from_secs(6),
            Duration::from_secs(5),
            "game.exe",
            stale_arm,
        );
        assert!(stale.active && stale.fps.valid && stale.fps.stale);

        let expired_arm = stale_phase(Duration::from_secs(11)).expect("11 s idle is expired");
        let expired = stale_game(
            &mut stats,
            frame_at + Duration::from_secs(11),
            observed + Duration::from_secs(11),
            Duration::from_secs(5),
            "game.exe",
            expired_arm,
        );
        assert!(!expired.active && !expired.fps.valid);

        // The shipped bounds and the two verbatim detail strings, at the new seams.
        assert_eq!(stale_phase(Duration::from_secs(4)), None);
        assert_eq!(
            stale_phase(Duration::from_secs(5)),
            None,
            "idle == STALE_AFTER is fresh"
        );
        assert_eq!(
            stale_phase(Duration::from_secs(6)),
            Some(StalePhase::Stale),
            "idle in (5 s, 10 s] is the stale arm"
        );
        assert_eq!(
            stale_phase(Duration::from_secs(10)),
            Some(StalePhase::Stale),
            "idle == 2 * STALE_AFTER is still the stale arm"
        );
        assert_eq!(
            stale_phase(Duration::from_secs(11)),
            Some(StalePhase::Expired),
            "idle > 2 * STALE_AFTER is the expired arm"
        );
        assert_eq!(
            stale_detail(StalePhase::Stale),
            "capture stale: no frames for 5 seconds"
        );
        assert_eq!(
            stale_detail(StalePhase::Expired),
            "capture unavailable: stale data expired"
        );
        assert!(
            stale_publishes("", StalePhase::Stale),
            "an empty ledger publishes the stale detail"
        );
        assert!(
            !stale_publishes("capture stale: no frames for 5 seconds", StalePhase::Stale),
            "the shipped dedupe: an unchanged detail publishes nothing, so no model is built"
        );
        assert!(
            stale_publishes(
                "capture stale: no frames for 5 seconds",
                StalePhase::Expired
            ),
            "the stale -> expired edge is a transition"
        );
    }

    #[test]
    fn stale_and_expired_updates_are_not_republished_as_active_until_a_new_frame() {
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        let (line_tx, line_rx) = mpsc::sync_channel(4);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,42,16.0".into()).unwrap();
        let (updates, rx) = mpsc::channel();
        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        capture.drain(&cfg, 1, &updates, Instant::now()).unwrap();
        assert!(rx.recv().unwrap().available);

        let stale_at = Instant::now() - Duration::from_secs(6);
        capture.last_frame = Some(stale_at);
        capture.last_publish = Some(stale_at);
        capture.last_target_poll = Some(Instant::now());
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        let stale = rx.recv().unwrap();
        assert!(stale.detail.contains("capture stale") && stale.game.fps.stale);
        capture.last_publish = Some(stale_at);
        capture.last_target_poll = Some(Instant::now());
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        assert!(rx.try_recv().is_err());

        let expired_at = Instant::now() - Duration::from_secs(11);
        capture.last_frame = Some(expired_at);
        capture.last_publish = Some(expired_at);
        capture.last_target_poll = Some(Instant::now());
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        let expired = rx.recv().unwrap();
        assert!(expired.detail.contains("stale data expired") && !expired.game.active);
        capture.last_publish = Some(expired_at);
        capture.last_target_poll = Some(Instant::now());
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        assert!(rx.try_recv().is_err());

        line_tx.send("game.exe,42,16.0".into()).unwrap();
        capture.last_target_poll = Some(Instant::now());
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        assert!(rx.recv().unwrap().available);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn statistics_use_monotonic_windows_and_fail_closed_at_capacity() {
        let observed = Instant::now();
        let mut stats = Stats::new(30.0, Duration::from_secs(600));
        stats.frames = std::iter::repeat_n((observed, 16.0), MAX_WINDOW_FRAMES).collect();
        assert!(stats
            .add(&Frame {
                observed_at: observed,
                frame_ms: 16.0,
                ..Default::default()
            })
            .is_err());

        stats.frames.clear();
        stats
            .add(&Frame {
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
                observed_at: observed,
                frame_ms: 16.0,
                ..Default::default()
            })
            .unwrap();
        let game = stats.model(
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            observed + Duration::from_secs(1),
            Duration::from_secs(600),
            "game.exe",
        );
        assert!(
            game.active,
            "wall-clock rollback cannot erase a monotonic window"
        );
    }

    #[test]
    fn retry_cooldown_blocks_until_deadline() {
        let now = Instant::now();
        assert!(retry_pending(Some(now + RETRY_DELAY), now));
        assert!(!retry_pending(Some(now), now));
    }

    #[test]
    fn auto_location_is_colocated_only() {
        let app = Path::new(r"C:\LCDSirPlus\LCDSirPlus.exe");
        let (candidate, root) = colocated_location(app).unwrap();
        assert_eq!(candidate, Path::new(r"C:\LCDSirPlus\PresentMon.exe"));
        assert_eq!(root, Path::new(r"C:\LCDSirPlus"));
    }

    #[test]
    fn canonical_path_policy_accepts_drive_forms_and_rejects_namespaces() {
        assert_eq!(
            local_drive_letter(Path::new(r"C:\Apps\PresentMon.exe")),
            Some(b'C')
        );
        assert_eq!(
            local_drive_letter(Path::new(r"\\?\C:\Apps\PresentMon.exe")),
            Some(b'C')
        );
        assert!(same_drive_path(
            Path::new(r"C:\Apps\PresentMon.exe"),
            Path::new(r"\\?\C:\Apps\PresentMon.exe")
        ));
        for rejected in [
            r"\\server\share\PresentMon.exe",
            r"\\?\UNC\server\share\PresentMon.exe",
            r"\\.\C:\PresentMon.exe",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\PresentMon.exe",
        ] {
            assert_eq!(local_drive_letter(Path::new(rejected)), None, "{rejected}");
        }
    }

    #[test]
    fn current_local_canonical_executable_can_be_pinned() {
        let current = std::env::current_exe().unwrap().canonicalize().unwrap();
        require_fixed_local_drive(&current).unwrap();
        let pinned = pin_executable(&current).unwrap();
        assert!(same_drive_path(&pinned.path, &current));
        assert_ne!(pinned.identity.size, 0);
    }

    #[test]
    fn failed_cleanup_is_retained_and_gates_retry() {
        let mut capture = Capture::default();
        capture.owned_session = Some("LCDSirPlus-42-owned".into());
        capture.stop_with(
            |_| Ok(ChildExit::Normal),
            |_| Err("injected failure".into()),
        );
        #[rustfmt::skip]
        assert_eq!(capture.owned_session.as_deref(), Some("LCDSirPlus-42-owned"));
        capture.schedule_retry(Instant::now());
        assert!(retry_pending(capture.retry_after, Instant::now()));
        #[rustfmt::skip]
        capture.stop_with(|_| Ok(ChildExit::Normal), |name| { assert_eq!(name, "LCDSirPlus-42-owned"); Ok(()) });
    }

    #[test]
    fn unconfirmed_child_retains_capture_and_gates_new_start() {
        #[rustfmt::skip]
        let child = Command::new(std::env::current_exe().unwrap()).arg("--list").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let executable = pin_executable(&std::env::current_exe().unwrap()).unwrap();
        let mut capture = Capture::default();
        capture.child = Some(child);
        capture.owned_session = Some("LCDSirPlus-42-owned".into());
        capture.reader = Some(std::thread::spawn(|| {}));
        capture.target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        let cleanup_names: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        capture.stop_with(
            |_| {
                Err(format!(
                    "kill child: forced kill; child is still running: {}",
                    ChildExit::DidNotExitInTime.token()
                ))
            },
            |name| {
                cleanup_names.lock().unwrap().push(name.to_string());
                Ok(())
            },
        );
        assert!(capture.child.is_some() && capture.reader.is_some());
        // WO-U46I (#13) supersedes the shipped coupling "ETW cleanup must wait for a confirmed
        // child exit": the named ETW session is THIS instance's own registration (and the
        // presenting-mode child carries no `--terminate_on_proc_exit`), so a child that survives
        // the bounded confirm must not leave it registered. The cleanup now runs in the same arm
        // that retains the child, on the owned name only, as a SEPARATE reported outcome. The
        // child/reader/target retention and the EC-6 token claims below are unchanged.
        assert_eq!(
            cleanup_names.lock().unwrap().as_slice(),
            ["LCDSirPlus-42-owned".to_string()],
            "(#13) the owned session is cleaned up on the retained-child path, exactly once"
        );
        assert_eq!(
            capture.owned_session, None,
            "(#13) a completed cleanup clears the claim"
        );
        assert_eq!(capture.target.pid, 42);
        // EC-6 acceptance (2): a child that survives the bounded confirm keeps its existing class,
        // and that class is the stable did_not_exit_in_time token.
        assert!(capture.stop_diagnostic.contains("forced kill"));
        assert!(capture.stop_diagnostic.contains("child is still running"));
        assert!(capture
            .stop_diagnostic
            .contains(&ChildExit::DidNotExitInTime.token()));
        let error = capture
            .start(
                &executable,
                ProcessInfo {
                    pid: 7,
                    creation_time: 2,
                    name: "next.exe".into(),
                    image_path: String::new(),
                },
                &Config::default(),
                "next".into(),
                &|_, _| panic!("start validation must follow teardown gating"),
            )
            .unwrap_err();
        assert!(error.contains("teardown is still pending"));
    }

    #[test]
    fn start_refuses_a_changed_final_request_before_spawn() {
        let executable = pin_executable(&std::env::current_exe().unwrap()).unwrap();
        let target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        let checked = std::cell::Cell::new(false);
        let error = Capture::default()
            .start(
                &executable,
                target,
                &Config::default(),
                "expected".into(),
                &|_, _| {
                    checked.set(true);
                    Err("forced final request change".into())
                },
            )
            .unwrap_err();
        assert!(checked.get());
        assert!(error.contains("forced final request change"));
    }

    #[test]
    fn detached_reader_skips_diagnostic_lock_and_bounded_helpers_timeout() {
        let diagnostic = Arc::new(std::sync::Mutex::new(BoundedDiagnostic::default()));
        let guard = diagnostic.lock().unwrap();
        let reader = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(50)));
        assert_eq!(
            join_thread_bounded(reader, Instant::now()),
            Err("reader timed out and was detached")
        );
        let started = Instant::now();
        assert!(take_diagnostic(Some(Arc::clone(&diagnostic)), false).is_empty());
        assert!(started.elapsed() < Duration::from_millis(20));
        assert_eq!(
            wait_bounded(Duration::ZERO, || Ok(false)),
            Ok(false),
            "a bounded confirm that never observes an exit reports an unconfirmed loop"
        );
        assert_eq!(wait_bounded(Duration::ZERO, || Ok(true)), Ok(true));
        drop(guard);
    }

    fn synthetic_cmd_child(command: &str) -> Child {
        Command::new("cmd.exe")
            .args(["/D", "/C", command])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap()
    }

    fn synthetic_exit_status(code: u8) -> ExitStatus {
        let mut child = synthetic_cmd_child(&format!("exit /B {code}"));
        child.wait().unwrap()
    }

    fn synthetic_long_running_child() -> Child {
        synthetic_cmd_child("ping -n 20 127.0.0.1 >nul")
    }

    #[test]
    fn child_exit_classification_names_the_synthetic_exit_code() {
        // EC-6 acceptance (1): a child that exits with code X yields a reason that names X.
        let failing = synthetic_exit_status(101);
        assert_eq!(failing.code(), Some(101));
        let exit = ChildExit::classify(&failing);
        assert_eq!(exit, ChildExit::Failed(101));
        assert_eq!(exit.token(), "exited_failed_101");
        assert_eq!(exit.reason(), "presentmon child exited_failed_101");
        assert!(exit.capture_end_reason("game.exe").contains("101"));

        // EC-6 acceptance (4): the exit-code-0 path keeps its semantics, named as a normal exit.
        let clean = synthetic_exit_status(0);
        assert_eq!(clean.code(), Some(0));
        assert_eq!(ChildExit::classify(&clean), ChildExit::Normal);
        assert_eq!(ChildExit::Normal.token(), "exited_normal");
    }

    #[test]
    fn child_exit_tokens_are_stable_and_privacy_bounded() {
        let cases = [
            (ChildExit::Normal, "exited_normal"),
            (ChildExit::Failed(5), "exited_failed_5"),
            (ChildExit::Failed(NO_EXIT_CODE), "exited_failed_4294967295"),
            (ChildExit::DidNotExitInTime, "did_not_exit_in_time"),
        ];
        for (exit, token) in cases {
            assert_eq!(exit.token(), token);
            assert_eq!(exit.reason(), format!("presentmon child {token}"));
            // The classification text carries class + numeric code only: no child argv, path,
            // session name or target identity.
            for leak in [
                "\\",
                "/",
                ":",
                "cmd",
                "exe",
                "LCDSirPlus-",
                "--process_id",
                "game",
            ] {
                assert!(
                    !exit.reason().contains(leak),
                    "{} leaks {leak:?}",
                    exit.reason()
                );
            }
        }
        // A code-less death is named as a failure, never masked as a normal exit.
        assert_eq!(ChildExit::Failed(NO_EXIT_CODE), ChildExit::Failed(u32::MAX));
    }

    #[test]
    fn wait_child_bounded_names_did_not_exit_in_time_when_the_confirm_expires() {
        let mut child = synthetic_long_running_child();
        let error = wait_child_bounded(&mut child, Duration::from_millis(1))
            .expect_err("a running child cannot confirm its exit within 1 ms");
        assert!(
            error.contains("exit was not confirmed within 1 ms"),
            "{error}"
        );
        assert!(
            error.contains(&ChildExit::DidNotExitInTime.token()),
            "{error}"
        );
        for leak in ["cmd.exe", "ping", "\\", "/C"] {
            assert!(
                !error.contains(leak),
                "timeout reason leaked {leak:?}: {error}"
            );
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn stop_child_bounded_classifies_the_confirmed_exit_after_kill() {
        let mut child = synthetic_long_running_child();
        let exit = stop_child_bounded(&mut child, Duration::from_millis(STOP_TIMEOUT_MS.into()))
            .expect("the bounded confirm must confirm the kill");
        let status = child.wait().unwrap();
        assert_eq!(exit, ChildExit::classify(&status));
        assert!(!matches!(exit, ChildExit::DidNotExitInTime));
        assert!(!exit.token().contains('\\') && !exit.token().contains('/'));
    }

    #[test]
    fn capture_boundedly_stops_child_and_readers_before_cleanup() {
        #[rustfmt::skip]
        let child = Command::new(std::env::current_exe().unwrap()).arg("--list").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut capture = Capture::default();
        capture.child = Some(child);
        let delay = Duration::from_millis(20);
        capture.reader = Some(std::thread::spawn(move || std::thread::sleep(delay)));
        capture.owned_session = Some("LCDSirPlus-77-owned".into());
        let started = Instant::now();
        #[rustfmt::skip]
        capture.stop_with(|child| stop_child_bounded(child, Duration::from_millis(STOP_TIMEOUT_MS.into())), |name| { assert!((Duration::from_millis(20)..=Duration::from_millis(STOP_TIMEOUT_MS.into())).contains(&started.elapsed())); assert_eq!(name, "LCDSirPlus-77-owned"); Ok(()) });
    }

    #[test]
    fn pinned_executable_denies_writers_until_release() {
        let temp =
            std::env::temp_dir().join(format!("lcdsirplus-presentmon-pin-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let path = temp.join("PresentMon.exe");
        std::fs::write(&path, b"fixture").unwrap();
        let pinned = pin_executable(&path).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        drop(pinned);
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn acquired_cache_satisfies_bundled_and_custom_runtime_trust() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/PresentMon");
        let executable = root.join("PresentMon.exe");
        if !executable.exists() {
            return;
        }
        validate_auto_executable(&executable, &root).unwrap();
        validate_executable(&executable).unwrap();

        let temp = std::env::temp_dir().join(format!(
            "lcdsirplus-presentmon-tamper-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let tampered = temp.join("PresentMon.exe");
        std::fs::copy(&executable, &tampered).unwrap();
        let mut bytes = std::fs::read(&tampered).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&tampered, bytes).unwrap();
        assert!(validate_executable(&tampered).is_err());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn auto_validation_rejects_candidates_outside_trusted_root() {
        let temp =
            std::env::temp_dir().join(format!("lcdsirplus-presentmon-{}", std::process::id()));
        let root = temp.join("trusted");
        let outside = temp.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let candidate = outside.join("PresentMon.exe");
        std::fs::write(&candidate, b"test").unwrap();
        assert!(validate_auto_executable(&candidate, &root).is_err());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn owner_thread_stops_and_joins_after_shutdown() {
        let config = Arc::new(RwLock::new(Config {
            safe_mode: true,
            ..Config::default()
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(1));
        let (_updates, thread) = spawn(
            config,
            generation,
            // WO-TEL-D2 (U-3): demand 0 parks the worker in the new gate arm;
            // shutdown then joins it through the same lifecycle (zero child work).
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&shutdown),
        );
        shutdown.store(true, Ordering::Relaxed);
        assert!(thread.join().is_ok());
    }

    /// WO-TEL-D2 (S2, U-3) STRUCTURAL pin (TEL-B `include_str!` occurrence
    /// form; name/doc are honest: this inspects shipped source, the worker's
    /// real-thread child behavior needs hardware and stays in the ignored
    /// native tests). Claims: the worker loop reads the demand word exactly
    /// once; the demand gate precedes `capture.reconcile(`; the park arm stops
    /// a running child (`stop_and_report_cleanup`) and publishes the deduped
    /// unavailable reason, parked on the worker's own deadline (WO-OPT02C re-baselines
    /// the shipped 4x 50 ms needle to a 4x `park_timeout(` needle); the child-spawn machinery
    /// (`resolve_executable`, `.spawn()`, the `TARGET_POLL` selection) occurs
    /// ZERO times inside the `run` slice — it lives behind the gate, inside
    /// reconcile, which the demand-0 path never reaches.
    #[test]
    fn presentmon_run_gate_stops_target_poll_and_child_spawn_at_zero_demand() {
        let source = include_str!("presentmon.rs").replace("\r\n", "\n");
        let start = source.find("fn run(\n").expect("the shipped worker exists");
        // Slice the WORKER LOOP BODY: from the shipped `fn run(` head to the
        // loop-end marker (the shutdown drain), so the slice is the loop, not
        // the following `impl Capture` block (where the gated machinery lives).
        let rest = &source[start..];
        let end = rest
            .find("\n    let mut stderr = String::new();")
            .expect("the shipped loop-end marker closes the worker loop");
        let run = &rest[..end];
        assert_eq!(
            run.matches("crate::telemetry::demanded(").count(),
            1,
            "the demand read exists exactly once in the worker loop"
        );
        assert_eq!(
            run.matches("consumers.load(Ordering::Relaxed)").count(),
            1,
            "the worker reads the published consumer word once per loop iteration"
        );
        let gate = run
            .find("if !crate::telemetry::demanded(")
            .expect("the demand gate");
        let reconcile = run.find("capture.reconcile(").expect("the reconcile call");
        assert!(
            gate < reconcile,
            "the gate precedes the reconcile path: demand 0 performs no target poll and no child-spawn retry"
        );
        assert!(
            run.contains("stop_and_report_cleanup(&tx, request_generation, Instant::now())"),
            "the park stops a running child once (shipped cleanup arm)"
        );
        assert!(
            run.contains("\"no display consumer\""),
            "the park publishes the deduped unavailable reason (health-fed availability predicate stays consistent)"
        );
        assert_eq!(
            run.matches("resolve_executable").count(),
            0,
            "the run loop never resolves the executable itself"
        );
        assert_eq!(
            run.matches(".spawn()").count(),
            0,
            "the run loop never spawns a child (the CreateProcess retry ladder lives behind the gate)"
        );
        assert_eq!(
            run.matches("capture.unavailable(").count(),
            1,
            "the park arm publishes the unavailable state through the dedupe exactly once per iteration"
        );
        assert_eq!(
            run.matches("std::thread::park_timeout(").count(),
            4,
            "WO-OPT02C S3 (the re-baselined 4x needle): generation cleanup, demand park (cleanup-pending), demand park (settled) and the main loop each park on the worker's own deadline"
        );
        assert_eq!(
            run.matches("std::thread::sleep(Duration::from_millis(50))")
                .count(),
            0,
            "WO-OPT02C S3: the 50 ms step is retired from every worker arm, so the shipped 150 ms re-arm bound is edge-driven, not a step size"
        );
    }

    /// WO-OPT02C (C3) fixture: the production QUIET shape of the worker, run on a real
    /// thread with no child and no ETW (the shipped config gate parks the capture path),
    /// so no PresentMon.exe and no LCDSirPlus instance is involved. `presentmon_enabled`
    /// false makes the demanded leg publish a DISTINCT deduped reason ("disabled"), which
    /// is the observable adoption signal on the worker's own channel.
    type DormancyWorker = (
        Arc<RwLock<Config>>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
        Arc<AtomicBool>,
        mpsc::Receiver<Update>,
        std::thread::JoinHandle<()>,
    );

    fn dormancy_worker(consumers: u64) -> DormancyWorker {
        let config = Arc::new(RwLock::new(Config {
            presentmon_enabled: false,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let consumers = Arc::new(AtomicU64::new(consumers));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker = {
            let config = Arc::clone(&config);
            let generation = Arc::clone(&generation);
            let consumers = Arc::clone(&consumers);
            let shutdown = Arc::clone(&shutdown);
            std::thread::Builder::new()
                .name("telemetry-presentmon-test".into())
                .spawn(move || run(config, generation, consumers, shutdown, tx))
                .expect("the presentmon worker under test")
        };
        (config, generation, consumers, shutdown, rx, worker)
    }

    /// WO-OPT02C (C3.1): the demand wake edge adopts a parked worker. The word is written
    /// and THEN unparked, exactly as `telemetry.rs:121-125` (`set_consumers` ->
    /// `wake_workers`, whose handle list carries this worker).
    /// RED ON BASE: the base arm sleeps the shipped 50 ms, so the worker iterates ~4 times
    /// inside the 200 ms dormancy window and the first assert below fails.
    /// WO-FLAKE-HARDEN (F-AUD-P3B-3): that first assert is a RATE bound now, not an exact-zero
    /// equality - `Thread::park_timeout` MAY RETURN SPURIOUSLY, so the exact-zero form demanded
    /// a guarantee the platform does not give (unit-54 audit r2 went red on it). The window, the
    /// 20 ms adoption budget and every shipped bound are UNCHANGED.
    #[test]
    fn the_presentmon_demand_edge_wakes_a_parked_worker() {
        let (_config, _generation, consumers, shutdown, rx, worker) = dormancy_worker(0);
        let id = worker.thread().id();
        let parked = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the zero-demand park publishes the deduped unavailable row");
        assert_eq!(parked.detail, "no display consumer");
        let at = Instant::now();
        let settled = worker_tick_count(id);
        std::thread::sleep(Duration::from_millis(200));
        let window = at.elapsed();
        // WO-FLAKE-HARDEN (F-AUD-P3B-3): the zero-iteration equality is replaced by a RATE
        // bound with a documented margin, normalized by the MEASURED window (under full-suite
        // load this lane's `sleep(200 ms)` was measured inflating past the worker's 1 s park
        // deadline, which is what made the exact-zero form, and a plain count bound, unstable).
        // Delivered rate: 1/s - the park is `worker_idle_deadline(&[])` = TARGET_POLL = 1 s
        // (the C3.2 census measures 1 iteration in 2 s). Spurious wakes: each costs ONE
        // iteration and then re-arms a 1 s park, and the host's floor is the 2 ms
        // `WORKER_MIN_PARK` inside the ~15.6 ms timer granularity, so ~1 wake/s is the
        // observed census. Bound = 2 iterations per measured second + 2 absolute slack = ~3x
        // the delivered rate. Discriminating power: the base arm sleeps 50 ms + the host
        // granularity => ~20 iterations/s (WO-OPT02C audit: 4 in 200 ms, ~40 in 2 s), i.e.
        // 10x the bound, so the row stays RED on base at any window length.
        let window_iters = worker_tick_count(id) - settled;
        let bound = (2 * (window.as_millis() / 1_000) + 2) as u64;
        assert!(
            window_iters <= bound,
            "a parked worker iterates at most 2/s + 2 slack inside the measured dormancy window ({window_iters} iterations in {window:?}, bound {bound}: the 1 s deadline park is 1/s and every extra iteration is a spurious wake that re-arms a 1 s park; base steps 50 ms => ~20/s)"
        );
        let started = Instant::now();
        consumers.store(
            crate::telemetry::extended_consumer_bit("presentmon"),
            Ordering::Relaxed,
        );
        worker.thread().unpark();
        let adopted = rx
            .recv_timeout(Duration::from_millis(20))
            .expect("the demand edge adopted the word inside 20 ms");
        let adopted_after = started.elapsed();
        assert_eq!(
            adopted.detail, "disabled",
            "the demanded worker re-enters reconcile and reaches the shipped config gate"
        );
        assert!(
            adopted_after < Duration::from_millis(20),
            "the wake drove adoption in {adopted_after:?} (budget 20 ms, inside the retired 50 ms step)"
        );
        assert!(
            worker_tick_count(id) > settled,
            "the wake drove the adoption iteration"
        );
        println!(
            "WO-OPT02C-WAKES demand edge: adoption {adopted_after:?} after the unpark (budget 20 ms, retired step 50 ms); the pre-edge 200 ms window held {} iterations",
            worker_tick_count(id) - settled - 1
        );
        shutdown.store(true, Ordering::Release);
        worker.thread().unpark();
        assert!(worker.join().is_ok());
    }

    /// WO-OPT02C (C3.2): the idle step is the worker's deadline, not the retired tick.
    /// RED ON BASE: the base loop is 20 wakes/s (~40 in the 2 s window); the assertion
    /// caps at 10.
    #[test]
    fn the_presentmon_worker_idle_step_is_the_deadline_not_the_tick() {
        let (_config, _generation, _consumers, shutdown, rx, worker) = dormancy_worker(0);
        let id = worker.thread().id();
        let parked = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the zero-demand park publishes the deduped unavailable row");
        assert_eq!(parked.detail, "no display consumer");
        let window = Duration::from_millis(2_000);
        let before = worker_tick_count(id);
        std::thread::sleep(window);
        let ticks = worker_tick_count(id) - before;
        println!(
            "WO-OPT02C-WAKES presentmon quiet shape (collection not due, no demand): {ticks} iterations in {window:?} = {:.1} wakes/s (base 50 ms step = 20/s; target ~1/s = TARGET_POLL)",
            ticks as f64 * 1000.0 / window.as_millis() as f64
        );
        shutdown.store(true, Ordering::Release);
        worker.thread().unpark();
        assert!(worker.join().is_ok());
        assert!(
            ticks >= 1,
            "the worker stays alive (it is dormant, not stopped)"
        );
        assert!(
            ticks <= 10,
            "the idle step is the deadline, not the retired 20/s tick: {ticks} iterations in {window:?} (base gives ~40; the shipped wake census number is this count / 2 s)"
        );
    }

    /// WO-OPT02C (C3.3): the shutdown wake edge exits a parked worker inside the shipped
    /// step budget. The teardown path is `telemetry.rs:88-96`: store -> `wake_workers` ->
    /// join, and that join (`telemetry.rs:93`) is UNBOUNDED, so this edge is what keeps
    /// teardown finite. Join budgets read at delivery: `telemetry.rs:93` unbounded;
    /// `presentmon.rs` `join_thread_bounded` = `STOP_TIMEOUT_MS` 2 s (reader threads, not
    /// this worker); the worker joins in the tests (`owner_thread_stops_and_joins_after_shutdown`,
    /// `presentmon_config_change_midflight_...`) are unbounded and assert no budget.
    /// PARITY GUARD: base is already fast here (a 50 ms sleep), so the 50 ms assert is
    /// GREEN on base too; the no-edge leg (3 workers, spurious-wake bound 1 while the measured
    /// window is inside the 1 s deadline park) is the discriminating row.
    #[test]
    fn the_presentmon_shutdown_edge_exits_a_parked_worker() {
        let (_config, _generation, _consumers, shutdown, rx, worker) = dormancy_worker(0);
        let parked = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the zero-demand park publishes the deduped unavailable row");
        assert_eq!(parked.detail, "no display consumer");
        // S13 P3 (F-AUD-OPT02C-3): the `elapsed()` read sits AFTER the join, so
        // `latency` is the JOIN latency the row claims, not the store + unpark cost.
        // The 50 ms budget is the shipped one; no budget moved.
        let started = Instant::now();
        shutdown.store(true, Ordering::Release);
        worker.thread().unpark();
        assert!(
            worker.join().is_ok(),
            "the worker exits on the shipped shutdown edge"
        );
        let latency = started.elapsed();
        assert!(
            latency < Duration::from_millis(50),
            "observed join latency {latency:?} must stay inside the retired 50 ms step, the last pinned teardown cadence (the `telemetry.rs:93` join itself has NO budget, so the unpark edge is the bound)"
        );
        // The edge is load-bearing: the same park with NO shutdown wake exits only at the
        // worker's deadline, so `wake_workers` is what pays for the long park.
        // WO-FLAKE-HARDEN (F-AUD-P3B-3): the old form asserted ONE worker was NOT finished
        // after 200 ms, i.e. ZERO spurious wakeups, which `park_timeout` does not promise (the
        // flag is already stored, so any wake ends the park). Rate form: 3 dormant workers, the
        // flag stored, NO edge posted, and the count that exited inside the MEASURED window is
        // bounded, not required to be zero. Discriminating power: the base arm sleeps 50 ms + the
        // host's ~15.6 ms granularity, so ALL 3 have exited inside any window shorter than the
        // 1 s deadline (WO-OPT02C audit: "base no-edge leg exits at 50 ms") - 3 > 1.
        let mut edgeless = Vec::new();
        for _ in 0..3 {
            edgeless.push(dormancy_worker(0));
        }
        for (_, _, _, _, parked_rx, _) in &edgeless {
            let parked2 = parked_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the zero-demand park publishes the deduped row");
            assert_eq!(parked2.detail, "no display consumer");
        }
        let at = Instant::now();
        for (_, _, _, shutdown2, _, _) in &edgeless {
            shutdown2.store(true, Ordering::Release);
        }
        std::thread::sleep(Duration::from_millis(200));
        let window = at.elapsed();
        let woke_without_edge = edgeless
            .iter()
            .filter(|(_, _, _, _, _, worker2)| worker2.is_finished())
            .count();
        // Reap BEFORE the assert, so a RED cannot leak a worker thread.
        for (_, _, _, _, _, worker2) in &edgeless {
            worker2.thread().unpark();
        }
        for (_, _, _, _, _, worker2) in edgeless {
            assert!(
                worker2.join().is_ok(),
                "the parked workers exit once the edge is posted"
            );
        }
        // The bound is a function of the MEASURED window: while the window is inside the 1 s
        // deadline park, ONE of the 3 may exit on a spurious wake, because each exit needs its own
        // wake. Measured census on this lane: the shipped single-worker form reddened once in four
        // full-suite runs (unit-54 audit, F-AUD-P3B-3) and the 3-worker form recorded 0 exits in
        // 30 worker-windows across the 10-round baseline (logs\stress-baseline-*.log), so the
        // per-window wake probability is small and bound 1 leaves one full event of slack. A
        // window that inflated PAST the 1 s deadline (this lane does that under full-suite load)
        // is not an observation of the park at all, because the deadline expires on its own, so
        // the row asserts nothing there and says so in the census print.
        // Discriminating power: the base arm sleeps 50 ms + the host's ~15.6 ms granularity, so
        // in any window shorter than the 1 s deadline ALL 3 have exited (WO-OPT02C audit: "base
        // no-edge leg exits at 50 ms") - 3 > 1, so the row stays RED on base.
        let allowed = if window < PRESENTMON_IDLE_BOUND { 1 } else { 3 };
        assert!(
            woke_without_edge <= allowed,
            "RED ON BASE: at most {allowed} of 3 parked workers may end a deadline park inside a measured window of {window:?} (bound 1 is the spurious-wake margin while the window is inside the 1 s park; base's 50 ms step ends all 3, which is why base needs no edge)"
        );
        println!(
            "WO-OPT02C-WAKES shutdown edge: parked worker joined in {latency:?} (parity bound 50 ms, the retired step; the telemetry.rs:93 join carries NO budget); no-edge leg measured_window={window:?} finished_without_edge={woke_without_edge}/3 bound={allowed} (0 proves the edge is what pays for the long park; allowed=3 only when load inflated the window past the 1 s deadline, which is not an observation of the park)"
        );
    }

    /// WO-OPT02C (C3.4): the shipped promise. GREEN ON BASE AND delivered: demand
    /// adoption stays inside the 150 ms upper bound (the app's 100 ms publish cadence
    /// plus the wake).
    #[test]
    fn the_presentmon_demand_edge_adopts_within_the_shipped_bound() {
        let (_config, _generation, consumers, shutdown, rx, worker) = dormancy_worker(0);
        let parked = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the zero-demand park publishes the deduped unavailable row");
        assert_eq!(parked.detail, "no display consumer");
        let started = Instant::now();
        consumers.store(
            crate::telemetry::extended_consumer_bit("presentmon"),
            Ordering::Relaxed,
        );
        worker.thread().unpark();
        let mut adopted = None;
        while adopted.is_none() && started.elapsed() <= Duration::from_millis(150) {
            if let Ok(update) = rx.recv_timeout(Duration::from_millis(150)) {
                adopted = Some(update);
            }
        }
        let adopted = adopted.expect("the demand is adopted inside the shipped 150 ms bound");
        let adopted_after = started.elapsed();
        assert_eq!(adopted.detail, "disabled");
        assert!(
            adopted_after <= Duration::from_millis(150),
            "re-arm happened in {adopted_after:?}, inside the shipped 150 ms upper bound"
        );
        shutdown.store(true, Ordering::Release);
        worker.thread().unpark();
        assert!(worker.join().is_ok());
    }

    /// WO-OPT02C (C3.5): the deadline math as exact `Duration`s - no clocks read, no
    /// re-implementation of the arm.
    #[test]
    fn worker_idle_deadline_is_the_earliest_gate_with_the_local_floor() {
        let now = Instant::now();
        let bound = PRESENTMON_IDLE_BOUND;
        assert_eq!(
            bound, TARGET_POLL,
            "the idle bound is NAMED from TARGET_POLL"
        );
        assert_eq!(WORKER_MIN_PARK, Duration::from_millis(2));
        // Nothing to wait for => the idle bound.
        assert_eq!(worker_idle_deadline(&[], now), bound);
        // Far gate => the bound (the cap is what the edges pay for).
        assert_eq!(
            worker_idle_deadline(&[Some(now + Duration::from_secs(30))], now),
            bound
        );
        // Near gate => exactly the remaining time.
        let near = now + Duration::from_millis(300);
        assert_eq!(
            worker_idle_deadline(&[Some(near)], now),
            Duration::from_millis(300)
        );
        // Past gate => the local floor, never negative, never zero.
        let past = now - Duration::from_millis(500);
        assert_eq!(worker_idle_deadline(&[Some(past)], now), WORKER_MIN_PARK);
        assert!(worker_idle_deadline(&[Some(past)], now) > Duration::ZERO);
        // The earliest gate wins, and a `None` gate contributes nothing.
        assert_eq!(
            worker_idle_deadline(&[Some(near), Some(past)], now),
            WORKER_MIN_PARK
        );
        assert_eq!(
            worker_idle_deadline(&[None, Some(near)], now),
            Duration::from_millis(300)
        );
        // The shipped shipped-cadence gates stay inside the bound: the 1 s target poll is
        // the bound itself, the 5 s / 10 s gates are capped to it.
        assert_eq!(
            worker_idle_deadline(&[Some(now + RETRY_DELAY), Some(now + CATALOG_REFRESH)], now),
            bound
        );
    }

    /// WO-OPT02C (C3.6): structural pins in repo form - the four arms park on the
    /// deadline, the retired step is absent, the 10 ms pumps stay (they are bounded child
    /// and reader polls, not idle waits), and the two wake edges keep their order.
    /// S14 (F-AUD-OPT02C-6): the pump needle is re-baselined to the shipped call form
    /// `capture.pump_step(PRESENTMON_PUMP_BOUND)`; every other assertion here - the 4x
    /// `park_timeout(` count, the 0x retired step, the branch containment and the three
    /// wake-edge order pins - is unchanged.
    #[test]
    fn presentmon_worker_arms_park_and_the_wake_edges_keep_their_order() {
        let source = include_str!("presentmon.rs").replace("\r\n", "\n");
        let start = source.find("fn run(\n").expect("the worker exists");
        let rest = &source[start..];
        let end = rest
            .find("\n    let mut stderr = String::new();")
            .expect("the loop-end marker closes the worker loop");
        let run = &rest[..end];
        assert_eq!(
            run.matches("std::thread::park_timeout(").count(),
            4,
            "all four retired 50 ms arms park"
        );
        assert_eq!(
            run.matches("std::thread::sleep(Duration::from_millis(50))")
                .count(),
            0,
            "no arm keeps the retired 50 ms step"
        );
        assert_eq!(
            run.matches("worker_idle_deadline(").count(),
            3,
            "the three timer-only arms build their park from the deadline helper"
        );
        assert!(
            run.contains("capture.pump_step(PRESENTMON_PUMP_BOUND)")
                && run.contains("capture.capture_running()"),
            "the tail arm's live-capture step is the data edge, with the park as its no-capture branch"
        );
        // C1.4 as shipped: the two 10 ms arms stay (bounded pumps on a child and on
        // reader threads, each with its own shipped deadline).
        assert!(
            source.contains("fn wait_bounded(")
                && source[..source.find("fn wait_bounded(").unwrap() + 700]
                    .contains("std::thread::sleep(Duration::from_millis(10))"),
            "the bounded child-exit pump keeps its 10 ms confirm cadence (no wake edge exists for a child process)"
        );
        assert!(
            source
                .find("fn join_thread_bounded(")
                .map(|at| &source[at..at + 400])
                .is_some_and(|body| body.contains("std::thread::sleep(Duration::from_millis(10))")),
            "the bounded reader-join pump keeps its 10 ms cadence (the deadline is STOP_TIMEOUT_MS and the readers own no edge to this worker)"
        );
        // The wake edges, pinned in the sliced sequence form.
        let runtime = include_str!("../telemetry.rs").replace("\r\n", "\n");
        let runtime = runtime
            .split("mod tests {")
            .next()
            .expect("the telemetry runtime has a production region");
        let publish = runtime
            .find("pub fn set_consumers(&self, consumers: u64)")
            .expect("the demand-publish site");
        let body = &runtime[publish..publish + 240];
        assert!(
            body.find("self.consumers.swap(consumers, Ordering::Relaxed)")
                .expect("the demand-word write")
                < body
                    .find("self.wake_workers()")
                    .expect("the demand wake edge"),
            "the demand site writes the word, then posts the wake"
        );
        let drop = runtime
            .find("fn drop(&mut self)")
            .expect("the teardown path");
        let body = &runtime[drop..drop + 320];
        assert!(
            body.find("self.shutdown.store(true, Ordering::Relaxed)")
                .expect("the shutdown store")
                < body.find("self.wake_workers()").expect("the shutdown wake")
                && body.find("self.wake_workers()").expect("the shutdown wake")
                    < body.find(".join()").expect("the worker join"),
            "shutdown stores, unparks, and only then joins, so no join blocks on a park"
        );
    }

    /// WO-U46I (#13) PRE-RED: the `stop_with` early-return arm (an unconfirmed child stop) restored
    /// the child handle and returned, skipping the ETW cleanup of the session THIS instance created.
    /// The child handle is a per-capture resource whose retry is owned by the retention ladder; the
    /// named ETW session is a system-wide registration, so leaving it registered on every unconfirmed
    /// stop is the defect. The fix attempts the cleanup in the same arm and reports the child-stop and
    /// session-cleanup outcomes as SEPARATE segments; ownership, retention and the retry ladder stay.
    /// Pre-fix: the cleanup seam is never called => RED at step (i).
    #[test]
    fn presentmon_child_stop_failure_still_cleans_the_owned_session() {
        use std::sync::atomic::AtomicUsize;
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut capture = Capture::default();
        capture.child = Some(child);
        capture.owned_session = Some("LCDSirPlus-99-owned".into());
        let calls = AtomicUsize::new(0);
        let names: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        capture.stop_with(
            |_| Err("injected child stop failure: child is still running".to_string()),
            |name| {
                calls.fetch_add(1, Ordering::Relaxed);
                names.lock().unwrap().push(name.to_string());
                Ok(())
            },
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "(i) the owned ETW session is cleaned up exactly once even when the child stop is unconfirmed"
        );
        assert_eq!(
            names.lock().unwrap().as_slice(),
            ["LCDSirPlus-99-owned".to_string()],
            "(ii) cleanup runs on THIS instance's owned session name, never an unrelated session"
        );
        assert!(
            capture.child.is_some(),
            "(iii) the child-handle retention on a stop failure is unchanged (the retry ladder owns it)"
        );
        assert_eq!(
            capture.owned_session, None,
            "(iv) a completed cleanup clears the ownership claim"
        );
        assert!(
            capture
                .stop_diagnostic
                .contains("teardown: injected child stop failure"),
            "(v) the child-stop outcome is named on its own: {}",
            capture.stop_diagnostic
        );
        assert!(
            !capture.stop_diagnostic.contains("ETW cleanup:"),
            "(v) a successful session cleanup adds no cleanup outcome: {}",
            capture.stop_diagnostic
        );
        // (vi) idempotence: the cleared session is never cleaned a second time.
        capture.stop_with(
            |_| Err("injected child stop failure again".to_string()),
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "(vi) a second stop does not repeat the completed cleanup"
        );
        assert!(
            capture.child.is_some(),
            "(vi) the child handle stays retained across repeats"
        );
        // (vii) a cleanup failure is reported as a SEPARATE outcome next to the child-stop outcome,
        // and the session claim is kept for the retry (the shipped `teardown_pending` rule).
        capture.owned_session = Some("LCDSirPlus-99-owned".into());
        capture.stop_with(
            |_| Err("child stop failed".to_string()),
            |_| Err("session stop refused".to_string()),
        );
        assert!(
            capture
                .stop_diagnostic
                .contains("teardown: child stop failed"),
            "(vii) the child-stop outcome: {}",
            capture.stop_diagnostic
        );
        assert!(
            capture
                .stop_diagnostic
                .contains("ETW cleanup: session stop refused"),
            "(vii) the session-cleanup outcome is reported separately: {}",
            capture.stop_diagnostic
        );
        assert_eq!(
            capture.owned_session.as_deref(),
            Some("LCDSirPlus-99-owned"),
            "(vii) a refused cleanup keeps the session claim so the retry re-attempts it"
        );
    }

    /// WO-U46I (#3a) PRE-RED, STRUCTURAL FORM (the runtime form needs the gate runner, which has
    /// no home pre-fix, and the WO bans real certificate hangs / real OS thread exhaustion): the
    /// capture start in the 50 ms worker loop must not run `WinVerifyTrustEx` synchronously. The
    /// shipped shape calls the unbounded verifier straight from `validate_executable`, so one
    /// revocation/OCSP stall blocks the worker (and the provider) with no bound. The fix routes the
    /// check through a bounded wrapper with a one-outstanding-thread gate. Pre-fix: the synchronous
    /// call site is present and the wrapper is absent => RED (the anchor panics on the missing
    /// runner, the same loud form the WO discloses).
    #[test]
    fn presentmon_verifier_call_site_is_bounded() {
        let source = include_str!("presentmon.rs").replace("\r\n", "\n");
        let body = |head: &str| -> String {
            let start = source
                .find(head)
                .unwrap_or_else(|| panic!("(#3a) the source carries `{head}`"));
            let end = source[start..]
                .find("\n}")
                .unwrap_or_else(|| panic!("`{head}` closes at column zero"))
                + start
                + 2;
            source[start..end].to_string()
        };
        let validate = body("fn validate_executable(path: &Path)");
        assert!(
            !validate.contains("verify_authenticode(&pinned.file, &pinned.path)"),
            "(#3a) the 50 ms worker path must not call the unbounded verifier synchronously"
        );
        assert!(
            validate.contains("verify_authenticode_bounded("),
            "(#3a) the Authenticode check runs through the bounded wrapper"
        );
        assert!(
            validate.contains("held_sha256(&pinned.file, pinned.identity.size)"),
            "(#3a) the SHA-256 and pinned-identity checks keep their shipped place"
        );
        let bounded = validate
            .find("verify_authenticode_bounded(")
            .unwrap_or(usize::MAX);
        let hashed = validate
            .find("held_sha256(&pinned.file")
            .unwrap_or(usize::MAX);
        assert!(
            bounded < hashed,
            "(#3a) the existing check ORDER is unchanged: signature, then pinned size + SHA-256"
        );
        let wrapper = body("fn verify_authenticode_bounded(");
        assert!(
            wrapper.contains("run_bounded_verifier("),
            "(#3a) the wrapper delegates to the gate runner that owns the bound"
        );
        let runner = body("fn run_bounded_verifier<");
        assert!(
            runner.contains("recv_timeout"),
            "(#3a) the worker waits a BOUNDED time for the verifier thread"
        );
        assert!(
            runner.contains("TryRecvError::Empty"),
            "(#3a) an in-flight verifier DECLINES a new attempt: at most one thread is outstanding"
        );
        assert!(
            runner.contains("*gate = None"),
            "(#3a) the gate is cleared by the verifier's COMPLETION, never by a timeout-ack"
        );
        assert!(
            !runner.contains(".expect("),
            "(#3a) the runner never panics on a spawn failure: it fails closed"
        );
    }

    /// WO-U46I (#3a) BEHAVIORAL + THREAD CENSUS, WO-47A DETERMINISTIC ORACLE: the gate runner facing
    /// a WEDGED verifier (the deterministic seam; the WO bans real certificate hangs). The first
    /// attempt times out and FAILS CLOSED with the shipped diagnostic; every later attempt while that
    /// verifier is in flight declines with `VERIFIER_BUSY`, and the census shows the outstanding
    /// verifier-thread count stays exactly ONE - no thread is stranded per timeout and nothing
    /// accumulates. When the wedged verifier returns, the gate is consumed by that COMPLETION (never
    /// by a timeout-ack) and the next attempt spawns exactly ONE new thread and ends cleared.
    ///
    /// WO-47A (FLAKE-2): the completion is reached by a HANDSHAKE observed under the gate, never by a
    /// yield census. Every decline asserts the gate CONTRACT (`VERIFIER_BUSY`, `entered == 1`, slot
    /// owned); the released verifier JOINS IN on a test-controlled completion channel and its result
    /// is then observed as the PAYLOAD the runner puts on the gate channel. How many declines precede
    /// a completion is scheduling and carries NO assertion - the registered red row (`declines=101`
    /// on a tree that ran green 5x, with `entered=1 gate_owned=true`, i.e. the property HELD) was
    /// exactly that count-as-oracle defect. Every wait here is the shipped `VERIFIER_BOUND` liveness
    /// budget and fails closed naming the census; no sleep, no cap, no suppression.
    #[test]
    fn presentmon_bounded_verifier_keeps_one_outstanding_thread() {
        use std::sync::atomic::AtomicUsize;
        let entered = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let mut gate: Option<mpsc::Receiver<Result<(), String>>> = None;
        let bound = Duration::from_millis(20);
        let counted = Arc::clone(&entered);
        let first = run_bounded_verifier(&mut gate, bound, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = release_rx.recv();
            // The body is done: report to the test, then return so the runner's own thread sends
            // this result on the GATE channel - the completion the gate carries.
            let _ = done_tx.send(());
            Ok(())
        });
        let detail = first.expect_err("(#3a) a wedged verifier makes the attempt fail closed");
        assert!(
            detail.contains("timed out"),
            "(#3a) the timeout is the recorded diagnostic: {detail}"
        );
        assert_eq!(
            detail.as_str(),
            VERIFIER_TIMEOUT,
            "(#3a) the fail-closed diagnostic is the shipped timeout constant"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            1,
            "(#3a) exactly one verifier thread entered"
        );
        assert!(
            gate.is_some(),
            "(#3a) the timeout keeps the in-flight thread COUNTED"
        );
        // WO-47A: a FIXED number of attempts, each asserting the GATE OUTCOME under the in-flight
        // verifier. The verifier is parked on the release, so this census is deterministic: every
        // attempt declines as in-flight and NO new thread enters.
        for attempt in 2..=6 {
            let declined = run_bounded_verifier(&mut gate, bound, || Ok(()))
                .expect_err("(#3a) an attempt declines while a verifier is in flight");
            assert_eq!(
                declined.as_str(),
                VERIFIER_BUSY,
                "(#3a) attempt {attempt} declines as in-flight, never with another outcome"
            );
            assert_eq!(
                entered.load(Ordering::SeqCst),
                1,
                "(#3a) census: NO new verifier thread per timeout (attempt {attempt})"
            );
        }
        let released = release_tx.send(());
        assert!(
            released.is_ok(),
            "(#3a) the release reaches the wedged verifier's own receiver: {released:?}"
        );
        // WO-47A THE HANDSHAKE, OBSERVED UNDER THE GATE (no yield count, no sleep, no cap):
        // (1) the released verifier joins in on its own completion channel;
        // (2) the runner's thread puts the verification result on the gate channel, and the test
        //     observes THAT completion as the payload of the slot it owns;
        // (3) the completion consumes the slot, exactly as the runner's contract does;
        // (4) the first attempt after the completion spawns exactly ONE new thread, and the runner
        //     clears the gate on that completion (the "reports results and clears the gate" leg).
        let joined = done_rx.recv_timeout(VERIFIER_BOUND);
        assert!(
            joined.is_ok(),
            "(#3a) the released verifier reports its body's completion to the test: {joined:?}"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            1,
            "(#3a) census: the in-flight verifier is still the ONLY thread after the join"
        );
        let slot = gate
            .take()
            .expect("(#3a) the in-flight verifier keeps the slot counted until its completion");
        let completion = slot.recv_timeout(VERIFIER_BOUND);
        assert_eq!(
            completion.expect("(#3a) the released verifier's result arrives on the gate channel"),
            Ok(()),
            "(#3a) the completion the gate carries IS the verifier's own result"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            1,
            "(#3a) census: consuming the completion adds no verifier thread (the decline count that
             preceded it is scheduling, never a bound on outstanding threads)"
        );
        let counted = Arc::clone(&entered);
        let outcome = run_bounded_verifier(&mut gate, VERIFIER_BOUND, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let outcome =
            outcome.expect("(#3a) the completion consumed the gate, so the next attempt spawns");
        assert!(
            outcome.is_ok(),
            "(#3a) the new verification is answered: {outcome:?}"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            2,
            "(#3a) exactly one new thread, for the new attempt"
        );
        assert!(
            gate.is_none(),
            "(#3a) a completed verification clears the gate"
        );
    }

    /// WO-U46I (#3a): a verifier that COMPLETES reports its own result and clears the gate, so a
    /// later attempt re-verifies (the discord precedent's latch, whose gate stays set forever
    /// after a timeout, is not replicated). A verifier thread that DIES is counted, fails closed,
    /// and its completion (the disconnected channel) releases the gate for the next attempt.
    #[test]
    fn presentmon_bounded_verifier_reports_results_and_clears_the_gate() {
        let mut gate: Option<mpsc::Receiver<Result<(), String>>> = None;
        let accepted = run_bounded_verifier(&mut gate, VERIFIER_BOUND, || Ok(()))
            .expect("(#3a) a verifier that completes is answered inside the bound");
        assert!(
            accepted.is_ok(),
            "(#3a) the verification result is the caller's outcome"
        );
        assert!(gate.is_none(), "(#3a) the gate is cleared by completion");
        let rejected = run_bounded_verifier(&mut gate, VERIFIER_BOUND, || {
            Err("PresentMon Authenticode signature is not valid: 0x800b0109".into())
        })
        .expect("(#3a) a rejecting verifier is answered inside the bound");
        assert_eq!(
            rejected.expect_err("(#3a) the signature failure reaches the capture start"),
            "PresentMon Authenticode signature is not valid: 0x800b0109",
            "(#3a) the shipped diagnostic text is preserved, not synthesized"
        );
        assert!(
            gate.is_none(),
            "(#3a) a completed rejection also clears the gate (re-verification stays possible)"
        );
        let dead = run_bounded_verifier(&mut gate, VERIFIER_BOUND, || {
            panic!("( #3a) injected verifier thread death: the runner must fail closed")
        });
        assert!(
            dead.is_err(),
            "(#3a) a verifier thread that dies fails the attempt closed"
        );
        assert!(
            gate.is_some(),
            "(#3a) the dead thread stays counted until its completion is observed"
        );
        let revived = run_bounded_verifier(&mut gate, VERIFIER_BOUND, || Ok(()))
            .expect("(#3a) the dead verifier's completion releases the gate for the next attempt");
        assert!(revived.is_ok(), "(#3a) the next attempt re-verifies");
        assert!(
            gate.is_none(),
            "(#3a) the gate ends cleared: no permanent latch"
        );
    }

    /// WO-OPT02C-PUMPCAP (S13) fixture: the shipped tail arm of `run` on a LIVE child
    /// and a LIVE line channel carrying zero lines - the capture-running shape the
    /// audit measured (F-AUD-OPT02C-1/-2). The production loop has no capture-injection
    /// seam, so this seam executes the arm's own expression (`idle_deadline` +
    /// `pump_step`), which IS the step the worker blocks in. The child is
    /// `cmd.exe /C ping` with `CREATE_NO_WINDOW` (no visible window), killed and waited
    /// by the test itself: no PresentMon, no ETW session, no LCDSirPlus instance.
    fn pumpcap_probe_capture(stale: bool) -> (mpsc::Sender<String>, Capture, Config) {
        let child = Command::new("cmd.exe")
            .args(["/C", "ping", "-n", "120", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("the hidden probe child");
        let (tx, rx) = mpsc::channel();
        let mut capture = Capture::default();
        capture.child = Some(child);
        capture.lines = Some(rx);
        let now = Instant::now();
        capture.last_target_poll = Some(now);
        capture.catalog.checked_at = Some(now);
        capture.started_at = Some(now);
        capture.last_frame = Some(if stale {
            now - STALE_AFTER - Duration::from_secs(1)
        } else {
            now
        });
        (tx, capture, Config::default())
    }

    /// WO-OPT02C-PUMPCAP (S13) fixture teardown: reap the probe child THIS test launched AND
    /// its descendant tree. The probe is `cmd.exe /C ping -n 120 127.0.0.1`, so killing only
    /// the DIRECT child (F-AUD-OPT02C2-2) left the `ping` grandchild orphaned and alive for
    /// its full 120 s. Every leg is scoped to this `Child`'s own pid, read from `Child::id()`
    /// (the pid the OS gave THIS test): never a name, never a pattern, so no process this test
    /// did not launch can be touched. The tree-kill runs FIRST - a dead parent is no longer
    /// enumerable - and its own report names each terminated descendant as
    /// `(child process of PID <probe pid>)`, which is the probe-attributed pid set; the
    /// hygiene leg then reads each one back with the read-only `tasklist /FI "PID eq <pid>"`
    /// filter and asserts it is gone, so NO probe-attributable `ping` survives the reap.
    /// (`tasklist` has no `PARENT_PID` filter and a name-only `ping` census cannot attribute
    /// a pid to this test in-process without a Toolhelp snapshot feature this crate does not
    /// enable; the raw `ping`/`cmd` name counts are censused per test window by
    /// WO-P3BUNDLE-scripts\Do-U54-Gate.ps1.)
    fn pumpcap_reap_capture(capture: &mut Capture) {
        if let Some(mut child) = capture.child.take() {
            let pid = child.id();
            let tree = pumpcap_probe_tool("taskkill.exe", &["/T", "/F", "/PID", &pid.to_string()]);
            let report = String::from_utf8_lossy(&tree.stdout).into_owned();
            let _ = child.kill();
            let _ = child.wait();
            drop(child);
            let attributed = format!("(child process of PID {pid})");
            let descendants: Vec<u32> = report
                .lines()
                .filter(|line| line.contains(&attributed))
                .filter_map(|line| {
                    line.split("PID ")
                        .nth(1)
                        .and_then(|tail| tail.split_whitespace().next())
                        .and_then(|digits| digits.parse().ok())
                })
                .collect();
            assert!(
                tree.status.success(),
                "the pid-scoped tree kill of the probe child pid {pid} succeeded (F-AUD-OPT02C2-2): {report}"
            );
            assert!(
                !descendants.is_empty(),
                "the tree kill enumerated the probe's OWN descendant (the `ping -n 120` grandchild) as a child of pid {pid}, so the tree is what was reaped: {report}"
            );
            for probe_pid in descendants.iter().chain(std::iter::once(&pid)) {
                assert!(
                    pumpcap_pid_gone(*probe_pid),
                    "no probe-attributable process survives the reap: pid {probe_pid} (probe child {pid} and its `ping` grandchild) is gone (F-AUD-OPT02C2-2)"
                );
            }
        }
        capture.lines = None;
    }

    /// Run a pid-scoped process tool - the tree kill, and the read-only pid filter probe -
    /// with no window. Every `pid` argument comes from `Child::id()` of the probe THIS test
    /// spawned, or from that tree kill's own report of the probe's descendants.
    fn pumpcap_probe_tool(program: &str, tool_args: &[&str]) -> std::process::Output {
        Command::new(program)
            .args(tool_args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("the pid-scoped process tool runs")
    }

    /// The read-only hygiene probe: `tasklist /FI "PID eq <pid>"` reports no matching task.
    fn pumpcap_pid_gone(pid: u32) -> bool {
        let probe = pumpcap_probe_tool("tasklist.exe", &["/FI", &format!("PID eq {pid}"), "/NH"]);
        probe.status.success()
            && String::from_utf8_lossy(&probe.stdout).contains("No tasks are running")
    }

    /// The capped pump step expression, in ONE place so the seam and the shipped arm
    /// cannot drift (S14: `run`'s running-capture branch pumps the plain
    /// `PRESENTMON_PUMP_BOUND`; the `min`/`max` here is algebraically that same constant,
    /// so the seam measures the shipped 50 ms step, and the structural pin of the shipped
    /// call form lives in the test below).
    fn pumpcap_capped_step(capture: &Capture, cfg: &Config) -> Duration {
        capture
            .idle_deadline(cfg, Instant::now())
            .min(PRESENTMON_PUMP_BOUND)
            .max(PRESENTMON_PUMP_BOUND)
    }

    /// WO-OPT02C-PUMPCAP S13.1 (F-AUD-OPT02C-1, P2, measured 1000 ms expression /
    /// 1011 ms observed at `8fc5a470`): in the capture-running / child-silent shape the
    /// pump step is capped to `PRESENTMON_PUMP_BOUND`, so a demand LOWERING posted in
    /// the middle of an in-flight pump is adopted inside the shipped 150 ms bound.
    /// RED ON THE FROZEN 02C TREE: the step is
    /// `recv_timeout(min(earliest gate, PRESENTMON_IDLE_BOUND = TARGET_POLL))` = 1000 ms,
    /// and `Thread::unpark` cannot cut a running `recv_timeout` (audit probe
    /// pump_iters=1 vs park_iters=2), so adoption lands at ~1.1 s.
    #[test]
    fn the_presentmon_pump_step_is_capped_while_the_child_is_silent() {
        let (_tx, mut capture, cfg) = pumpcap_probe_capture(false);
        let demand = Arc::new(AtomicU64::new(crate::telemetry::extended_consumer_bit(
            "presentmon",
        )));
        let start = Instant::now();
        let store_at = Arc::new(AtomicU64::new(u64::MAX));
        // The demand is lowered 5 ms into the first in-flight pump, exactly the shape of
        // `telemetry.rs:121-125` (`set_consumers` writes the word, then `wake_workers`
        // unparks): the word is visible while the pump is still blocking.
        let edge = Arc::clone(&demand);
        let stamp = Arc::clone(&store_at);
        let lower = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            edge.store(0, Ordering::Relaxed);
            stamp.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        });
        let window = Duration::from_millis(1_200);
        let mut iterations = 0u64;
        let mut worst = Duration::ZERO;
        let mut adopted_after = None;
        while start.elapsed() < window {
            let step = pumpcap_capped_step(&capture, &cfg);
            let at = Instant::now();
            capture.pump_step(step);
            worst = worst.max(at.elapsed());
            iterations += 1;
            // The worker reads the demand word once per iteration, at the top of the loop.
            if !crate::telemetry::demanded(demand.load(Ordering::Relaxed), "presentmon") {
                adopted_after =
                    Some(start.elapsed() - Duration::from_micros(store_at.load(Ordering::Relaxed)));
                break;
            }
        }
        pumpcap_reap_capture(&mut capture);
        lower.join().ok();
        let adopted_after = adopted_after.expect("the lowered demand is adopted inside the window");
        println!(
            "WO-OPT02C-PUMPCAP shape3 (capture running, child silent): worst_step {worst:?} (bound {PRESENTMON_PUMP_BOUND:?}), {iterations} iterations in {window:?}, demand lowering adopted in {adopted_after:?} (shipped bound 150 ms; frozen 02C measured 1011 ms)",
        );
        assert!(
            worst <= PRESENTMON_PUMP_BOUND + Duration::from_millis(20),
            "the pump step is capped: {worst:?} exceeds 50 ms + the ~15.6 ms host timer granularity (frozen 02C: 1011 ms, F-AUD-OPT02C-1)"
        );
        assert!(
            adopted_after <= Duration::from_millis(150),
            "a demand lowering posted in-flight is adopted inside the shipped 150 ms bound: {adopted_after:?} (frozen 02C: ~1.1 s, F-AUD-OPT02C-1)"
        );
        assert!(iterations >= 1, "the shape is live: the pump ran");
    }

    /// WO-OPT02C-PUMPCAP S13.2 (F-AUD-OPT02C-2, P2, measured ~500 iterations/s at
    /// `8fc5a470`): a running capture whose staleness gates are permanently due must not
    /// spin the worker. This is the unit-50-class anti-spin census for the shape the
    /// wake census never looked at. RED ON THE FROZEN 02C TREE: the deadline set
    /// collapses to `WORKER_MIN_PARK` (2 ms) => ~500 iterations/s, 25x the retired 20/s,
    /// each iteration calling `reconcile`.
    #[test]
    fn a_stale_running_capture_does_not_spin_the_worker() {
        let (_tx, mut capture, cfg) = pumpcap_probe_capture(true);
        let window = Duration::from_millis(800);
        let start = Instant::now();
        let mut iterations = 0u64;
        let mut worst = Duration::ZERO;
        while start.elapsed() < window {
            let step = pumpcap_capped_step(&capture, &cfg);
            let at = Instant::now();
            capture.pump_step(step);
            worst = worst.max(at.elapsed());
            iterations += 1;
        }
        pumpcap_reap_capture(&mut capture);
        let elapsed = start.elapsed();
        let per_second = iterations as f64 * 1000.0 / elapsed.as_millis() as f64;
        println!(
            "WO-OPT02C-PUMPCAP shape4 (capture running, past STALE_AFTER): {iterations} iterations in {elapsed:?} = {per_second:.1} iterations/s (worst step {worst:?}); frozen 02C ~500/s, the retired base arm 20/s",
        );
        assert!(
            per_second <= 25.0,
            "a stale running capture does not spin the worker: {per_second:.1} iterations/s over {elapsed:?} (retired base 20/s; the 2 ms `WORKER_MIN_PARK` floor is right for a DUE gate and wrong for a stale capture; frozen 02C ~500/s, F-AUD-OPT02C-2)"
        );
    }

    /// WO-OPT02C-PUMPCAP S13.3: the cap must not starve the shipped `line_overflow`
    /// reset routine - the pump is how a queued line reaches the drain and ages the
    /// bounded queue. Structural pin of the capped call in the sliced `run` body (repo
    /// form), plus the behavioral leg: one live line handed to the capped pump arrives
    /// in `pump_line`, the input of the shipped `drain_with` the 5 drain tests pin.
    #[test]
    fn the_presentmon_pump_cap_keeps_the_line_overflow_reset_reachable() {
        assert_eq!(
            PRESENTMON_PUMP_BOUND,
            Duration::from_millis(50),
            "the pump bound is the shipped 50 ms step: never slower (the `line_overflow` reset must stay reachable, and 20/s is the retired cadence), and the cap that ends the 1000 ms in-flight pump"
        );
        let source = include_str!("presentmon.rs").replace("\r\n", "\n");
        let start = source.find("fn run(\n").expect("the worker exists");
        let rest = &source[start..];
        let end = rest
            .find("\n    let mut stderr = String::new();")
            .expect("the loop-end marker closes the worker loop");
        let run = &rest[..end];
        let at = run
            .find("capture.pump_step(")
            .expect("the tail arm pumps the live capture");
        let call = &run[at..at + 90];
        assert!(
            call.contains("capture.pump_step(PRESENTMON_PUMP_BOUND)"),
            "S14: the pump call IS the pump bound, the plain form of the cap and the floor: {call}"
        );
        assert!(
            run[..at].contains("if capture.capture_running()"),
            "the capped pump is the running-capture arm; the no-capture arm keeps the deadline park"
        );
        let (tx, mut capture, cfg) = pumpcap_probe_capture(true);
        tx.send("probe-capped-pump-line".to_string())
            .expect("the line is queued on the worker's data edge");
        let step = pumpcap_capped_step(&capture, &cfg);
        let at = Instant::now();
        capture.pump_step(step);
        let took = at.elapsed();
        pumpcap_reap_capture(&mut capture);
        assert_eq!(
            capture.pump_line.as_deref(),
            Some("probe-capped-pump-line"),
            "the capped pump hands the queued line to the shipped drain, so the `line_overflow` reset stays reachable"
        );
        assert!(
            took <= PRESENTMON_PUMP_BOUND + Duration::from_millis(20),
            "a queued line is taken on the capped step, not after it: {took:?}"
        );
    }
    // ==== WO-OPT05 (unit 57): the A1 shape census and the A3 numerical-equality matrix ====

    use crate::history::{mean, percentile_high};

    /// The shipped shape's two `Vec` collect sites, counted here (test-only) because the
    /// changed production path has none: a production counter with no site would be a
    /// dead-code warning, and a counter that can only read 0 is vacuous. Per-thread, the
    /// `WORKER_TICKS` shape, so the parallel suite cannot contaminate a census read.
    static TEST_VEC_COLLECTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, u64>>,
    > = std::sync::OnceLock::new();

    fn test_vec_collect_map(
    ) -> &'static std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, u64>> {
        TEST_VEC_COLLECTS.get_or_init(std::sync::Mutex::default)
    }

    fn test_vec_collect_tick() {
        let map = test_vec_collect_map();
        let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *map.entry(std::thread::current().id()).or_insert(0) += 1;
    }

    fn test_vec_collect_count() -> u64 {
        let map = test_vec_collect_map();
        let map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(&std::thread::current().id()).copied().unwrap_or(0)
    }

    fn test_vec_collect_reset() {
        let map = test_vec_collect_map();
        let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(std::thread::current().id(), 0);
    }

    /// The unit-56 `Stats::model` body, kept VERBATIM as the numerical oracle the changed
    /// path is compared against (A3) and as the base side of the A1 shape census. The two
    /// collect ticks mark its two window vectors; nothing else was touched.
    /// test-harness-fixture: never reached by production code.
    fn base_model_shape(
        stats: &Stats,
        now: SystemTime,
        observed_now: Instant,
        window: Duration,
        process: &str,
    ) -> GameStats {
        let cutoff = observed_now.checked_sub(window);
        test_vec_collect_tick();
        let frame_times: Vec<f64> = stats
            .frames
            .iter()
            .filter(|(at, _)| cutoff.is_none_or(|cutoff| *at >= cutoff))
            .map(|(_, value)| *value)
            .collect();
        if frame_times.is_empty() {
            return GameStats::default();
        }
        let recent_cutoff = observed_now.checked_sub(Duration::from_secs(1));
        test_vec_collect_tick();
        let recent: Vec<f64> = stats
            .frames
            .iter()
            .filter(|(at, _)| recent_cutoff.is_none_or(|cutoff| *at >= cutoff))
            .map(|(_, value)| *value)
            .collect();
        let current = if recent.is_empty() {
            *frame_times.last().unwrap()
        } else {
            mean(&recent)
        };
        let one = percentile_high(&frame_times, 0.01);
        let point_one = percentile_high(&frame_times, 0.001);
        GameStats {
            active: true,
            process_name: process.to_string(),
            game_name: Path::new(process)
                .file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or(process)
                .to_string(),
            fps: Metric::valid(1000.0 / current, now),
            one_percent: Metric::valid(1000.0 / one, now),
            point_one_low: Metric::valid(1000.0 / point_one, now),
            frame_time_ms: Metric::valid(current, now),
            stutters: stats.stutters,
            session_start: stats.session_start,
        }
    }

    /// One measured shape of the model path: builds, stale builds, sorts, clones, collects.
    fn shape_counts() -> (u64, u64, u64, u64, u64) {
        let (builds, stale_builds) = model_shape_counts();
        let (sorts, clones) = crate::history::model_census_counts();
        (
            builds,
            stale_builds,
            sorts,
            clones,
            test_vec_collect_count(),
        )
    }

    fn shape_reset() {
        model_shape_reset();
        crate::history::model_census_reset();
        test_vec_collect_reset();
    }

    /// Every published field compared by BIT PATTERN, so `NaN`/`+-inf` are equal to
    /// themselves and a tail mean that moved into the NaN class is caught, not tolerated.
    fn assert_game_stats_bits(base: &GameStats, changed: &GameStats, case: &str) {
        let metric = |m: &Metric| (m.value.to_bits(), m.valid, m.stale, m.updated);
        assert_eq!(changed.active, base.active, "{case}: active");
        assert_eq!(
            changed.process_name, base.process_name,
            "{case}: process_name"
        );
        assert_eq!(changed.game_name, base.game_name, "{case}: game_name");
        for (name, b, c) in [
            ("fps", &base.fps, &changed.fps),
            ("one_percent", &base.one_percent, &changed.one_percent),
            ("point_one_low", &base.point_one_low, &changed.point_one_low),
            ("frame_time_ms", &base.frame_time_ms, &changed.frame_time_ms),
        ] {
            assert_eq!(metric(b), metric(c), "{case}: {name} bit pattern");
        }
        assert_eq!(changed.stutters, base.stutters, "{case}: stutters");
        assert_eq!(
            changed.session_start, base.session_start,
            "{case}: session_start"
        );
    }

    /// A frame stamp `offset_ms` away from `observed` (negative = older).
    fn observed_at(observed: Instant, offset_ms: i64) -> Instant {
        let magnitude = Duration::from_millis(offset_ms.unsigned_abs());
        if offset_ms >= 0 {
            observed.checked_add(magnitude)
        } else {
            observed.checked_sub(magnitude)
        }
        .expect("fixture offset is inside the monotonic clock")
    }

    /// A `Stats` whose frames, stutter count and session start are the fixture's, loaded by
    /// direct field assignment (the shape the shipped capacity test uses).
    fn fixture_stats(window: Duration, stutters: i32, frames: &[(i64, f64)]) -> Stats {
        let observed = Instant::now();
        let mut stats = Stats::new(30.0, window);
        stats.stutters = stutters;
        stats.session_start = Some(SystemTime::UNIX_EPOCH);
        stats.frames = frames
            .iter()
            .map(|(offset, value)| (observed_at(observed, *offset), *value))
            .collect();
        stats
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// `count` frames one millisecond apart, the newest at offset -1 ms.
    fn spread(count: usize, value_at: impl Fn(usize) -> f64) -> Vec<(i64, f64)> {
        (0..count)
            .map(|index| (index as i64 - count as i64, value_at(index)))
            .collect()
    }

    /// The reconciliation stamp: one tick after the newest stored frame.
    fn now_after(stats: &Stats, gap: Duration) -> Instant {
        stats
            .frames
            .back()
            .map(|(at, _)| *at)
            .unwrap_or(Instant::now())
            + gap
    }

    /// One A3 case: `(name, window, stutters, frames as (offset ms, frame_ms))`.
    type MatrixCase = (&'static str, Duration, i32, Vec<(i64, f64)>);

    fn a3_matrix() -> Vec<MatrixCase> {
        vec![
            ("empty-window", secs(60), 0, vec![]),
            ("one-frame", secs(60), 0, vec![(0, 16.0)]),
            (
                "all-duplicate",
                secs(60),
                3,
                vec![
                    (-500, 16.0),
                    (-400, 16.0),
                    (-300, 16.0),
                    (-200, 16.0),
                    (-100, 16.0),
                    (0, 16.0),
                ],
            ),
            (
                "recent-empty-window-not",
                secs(60),
                0,
                vec![(-30_000, 16.0), (-29_000, 20.0), (-28_000, 30.0)],
            ),
            (
                "window-cutoff-exact",
                secs(60),
                0,
                vec![(-59_999, 42.0), (-59_998, 20.0), (0, 10.0)],
            ),
            (
                "window-cutoff-one-tick-off",
                secs(60),
                0,
                vec![(-60_000, 42.0), (-59_998, 20.0), (0, 10.0)],
            ),
            (
                "recent-cutoff-exact",
                secs(60),
                0,
                vec![(-999, 24.0), (0, 16.0)],
            ),
            (
                "recent-cutoff-one-tick-off",
                secs(60),
                0,
                vec![(-1_000, 24.0), (0, 16.0)],
            ),
            ("len1", secs(60), 0, vec![(0, 7.0)]),
            ("len2", secs(60), 0, vec![(-100, 3.0), (0, 9.0)]),
            (
                "len3",
                secs(60),
                0,
                vec![(-200, 5.0), (-100, 1.0), (0, 3.0)],
            ),
            (
                "len4",
                secs(60),
                1,
                vec![(-300, 4.0), (-200, 2.0), (-100, 8.0), (0, 6.0)],
            ),
            (
                "len5",
                secs(60),
                5,
                vec![(-400, 5.0), (-300, 3.0), (-200, 9.0), (-100, 1.0), (0, 7.0)],
            ),
            ("len99", secs(60), 0, spread(99, |index| (index + 1) as f64)),
            (
                "len100",
                secs(60),
                0,
                spread(100, |index| (index + 1) as f64),
            ),
            (
                "len101",
                secs(60),
                0,
                spread(101, |index| (index + 1) as f64),
            ),
            (
                "len1000",
                secs(60),
                0,
                spread(1_000, |index| (index % 97) as f64),
            ),
            (
                "len1001",
                secs(60),
                0,
                spread(1_001, |index| (index % 89) as f64),
            ),
            (
                "len1001-ascending",
                secs(60),
                0,
                spread(1_001, |index| index as f64),
            ),
            (
                "unsorted-descending",
                secs(60),
                0,
                spread(50, |index| (50 - index) as f64),
            ),
            (
                "unsorted-sawtooth",
                secs(60),
                2,
                spread(60, |index| {
                    if index % 3 == 0 {
                        1.0
                    } else {
                        40.0 + index as f64
                    }
                }),
            ),
            (
                "nan-first",
                secs(60),
                0,
                vec![(-500, f64::NAN), (-400, 16.0), (0, 16.0)],
            ),
            (
                "nan-last",
                secs(60),
                0,
                vec![(-500, 16.0), (-400, 16.0), (0, f64::NAN)],
            ),
            (
                "nan-middle",
                secs(60),
                0,
                vec![(-500, 16.0), (-300, f64::NAN), (0, 16.0)],
            ),
            ("nan-only", secs(60), 0, spread(40, |_| f64::NAN)),
            (
                "nan-early-1-percent-tail",
                secs(60),
                0,
                spread(100, |index| if index == 0 { f64::NAN } else { 16.0 }),
            ),
            (
                "nan-early-01-percent-tail",
                secs(60),
                0,
                spread(1_000, |index| if index < 3 { f64::NAN } else { 16.0 }),
            ),
            (
                "nan-duplicate-run",
                secs(60),
                0,
                spread(200, |index| {
                    if index < 2 {
                        f64::NAN
                    } else if index % 2 == 0 {
                        16.0
                    } else {
                        33.0
                    }
                }),
            ),
            (
                "nan-at-every-tenth",
                secs(60),
                0,
                spread(300, |index| if index % 10 == 0 { f64::NAN } else { 16.0 }),
            ),
            (
                "inf-and-nan",
                secs(60),
                0,
                vec![
                    (-900, f64::INFINITY),
                    (-800, f64::NEG_INFINITY),
                    (-700, f64::NAN),
                    (0, 16.0),
                ],
            ),
            (
                "positive-infinity",
                secs(60),
                0,
                vec![(-500, f64::INFINITY), (-400, 16.0), (0, 16.0)],
            ),
            (
                "negative-infinity",
                secs(60),
                0,
                vec![(-500, f64::NEG_INFINITY), (-400, 16.0), (0, 16.0)],
            ),
            (
                "zero-frame-times",
                secs(60),
                0,
                vec![(-500, 0.0), (-400, 0.0), (0, 0.0)],
            ),
            (
                "window-shorter-than-recent",
                Duration::from_millis(500),
                0,
                vec![(-2_000, 7.0), (-1_000, 11.0), (-400, 5.0), (-100, 9.0)],
            ),
            (
                "window-one-ms",
                Duration::from_millis(1),
                0,
                spread(20, |index| (index * 3) as f64),
            ),
            (
                "non-monotonic-stamps",
                secs(60),
                0,
                vec![(-100, 5.0), (-2_000, 9.0), (0, 7.0), (-500, 12.0)],
            ),
            (
                "empty-recent-fallback",
                secs(60),
                0,
                vec![(-30_000, 42.0), (-29_000, 7.0), (-28_000, 3.0)],
            ),
            (
                "stutters-passthrough",
                secs(60),
                i32::MAX,
                spread(10, |index| (index + 1) as f64),
            ),
            (
                "default-window-occupancy-60fps",
                secs(60),
                12,
                (0..3_600usize)
                    .map(|index| {
                        (
                            -((3_600 - index) as i64) * 17,
                            if index % 4 == 0 { 16.7 } else { 33.3 },
                        )
                    })
                    .collect(),
            ),
            ("old-frames-plus-recent", secs(60), 0, {
                let mut frames: Vec<(i64, f64)> = (0..100usize)
                    .map(|index| ((index * 500) as i64 - 60_000, (101 + index) as f64))
                    .collect();
                frames.extend((1..=100i64).map(|index| (-index, index as f64)));
                frames
            }),
        ]
    }

    /// A3: the changed model is BIT-IDENTICAL to the shipped shape on every case. The
    /// reconciliation stamp is swept over three gaps so the `recent`-empty fallback and the
    /// recent/window cutoffs are crossed inside each fixture.
    #[test]
    fn stats_model_is_bit_identical_to_the_shipped_shape() {
        let now_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let matrix = a3_matrix();
        let mut equal_rows = 0usize;
        for (name, window, stutters, frames) in &matrix {
            for (gap_label, gap) in [
                ("gap-1ms", Duration::from_millis(1)),
                ("gap-5s", Duration::from_secs(5)),
                ("gap-2s", Duration::from_secs(2)),
            ] {
                let mut stats = fixture_stats(*window, *stutters, frames);
                let observed_now = now_after(&stats, gap);
                let base = base_model_shape(&stats, now_wall, observed_now, *window, "game.exe");
                let changed = stats.model(now_wall, observed_now, *window, "game.exe");
                assert_game_stats_bits(&base, &changed, &format!("{name} {gap_label}"));
                equal_rows += 1;
                // F-AUD-OPT06-A-4: the literal `BIT-EQUAL=True` claim is gone; the
                // `assert_game_stats_bits` above is the evidence.
                println!(
                    "A3-CASE {name} {gap_label} frames={} window={:?} base=(fps={:.6},1%={:.6},0.1%={:.6},ft={:.6},active={}) changed=(fps={:.6},1%={:.6},0.1%={:.6},ft={:.6},active={})",
                    frames.len(),
                    window,
                    base.fps.value,
                    base.one_percent.value,
                    base.point_one_low.value,
                    base.frame_time_ms.value,
                    base.active,
                    changed.fps.value,
                    changed.one_percent.value,
                    changed.point_one_low.value,
                    changed.frame_time_ms.value,
                    changed.active,
                );
            }
        }
        assert_eq!(equal_rows, matrix.len() * 3, "every A3 case ran");
    }

    /// A1: the model-shape census. Every number is MEASURED by the counting seams
    /// (`MODEL_BUILDS`/`STALE_BUILDS` at production `cfg(test)` sites,
    /// `MODEL_SORTS`/`MODEL_CLONES` in `history.rs`, `TEST_VEC_COLLECTS` for the shipped
    /// shape's two vectors) and by the lane's `alloc_count` counting allocator. The base
    /// side is the same measurement over `base_model_shape`, the verbatim unit-56 body.
    /// No CPU is measured and no CPU claim is made from these rows.
    #[test]
    fn model_shape_census_reports_sorts_clones_and_allocations() {
        use crate::backends::alloc_count;

        let measure = |run: &mut dyn FnMut()| -> alloc_count::Counts {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            run();
            alloc_count::subtract(alloc_count::snapshot(), before)
        };

        // The default 60 s window at 60/120/240/360 FPS is ORDINARY OCCUPANCY; 600 s is
        // the LEGAL MAXIMUM; 600 s at 1000 FPS is the cap's own basis; 602,048 frames is
        // the SOURCE CAP (`MAX_WINDOW_FRAMES`). No cap row is a claim about observed load.
        let sizes: Vec<(&str, usize, Duration, &str)> = vec![
            ("1-frame", 1, secs(60), "ordinary occupancy (minimum)"),
            ("60s@60fps", 3_600, secs(60), "ordinary occupancy"),
            ("60s@120fps", 7_200, secs(60), "ordinary occupancy"),
            ("60s@240fps", 14_400, secs(60), "ordinary occupancy"),
            ("60s@360fps", 21_600, secs(60), "ordinary occupancy"),
            (
                "600s@60fps",
                36_000,
                secs(600),
                "legal-max window, ordinary density",
            ),
            (
                "600s@1000fps",
                600_000,
                secs(600),
                "legal-max window at 1000 FPS, the cap's basis (cap-adjacent, not observed)",
            ),
            (
                "MAX_WINDOW_FRAMES",
                MAX_WINDOW_FRAMES,
                secs(600),
                "SOURCE CAP 602,048 frames (not an observed load)",
            ),
        ];
        let now_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for (label, frames, window, status) in sizes {
            let period_ns = (window.as_nanos() as u64) / (frames as u64);
            let observed = Instant::now();
            let mut stats = Stats::new(30.0, window);
            stats.frames = (0..frames)
                .map(|index| {
                    (
                        observed - Duration::from_nanos(period_ns * (frames as u64 - index as u64)),
                        16.0,
                    )
                })
                .collect();
            // Warm-up: the scratch capacity is paid once, then excluded (A2.2).
            let _ = stats.model(now_wall, observed, window, "game.exe");

            shape_reset();
            let base_allocs = measure(&mut || {
                let _ = base_model_shape(&stats, now_wall, observed, window, "game.exe");
            });
            let base = shape_counts();

            shape_reset();
            let changed_allocs = measure(&mut || {
                let _ = stats.model(now_wall, observed, window, "game.exe");
            });
            let changed = shape_counts();

            // Attribution: std's STABLE sort (the shipped comparator and sort kind that
            // A2.1 must keep) allocates its own merge scratch. A bare `tail_means` over the
            // same length names that block, so the model's over-256-byte count is measured
            // against it, not assumed away.
            shape_reset();
            let mut sort_buffer = vec![16.0f64; frames];
            let sort_probe = measure(&mut || {
                let _ = crate::history::tail_means(&mut sort_buffer);
            });

            println!(
                "A1-MODEL {label} frames={frames} window={window:?} status={status} SHAPED-BASELINE model-calls=1 sorts={} clones={} vec-collects={} vec-allocs={} alloc-total={} alloc-gt256={} alloc-le256={}",
                base.2,
                base.3,
                base.4,
                base.3 + base.4,
                base_allocs.4,
                base_allocs.3,
                base_allocs.2
            );
            println!(
                "A1-MODEL {label} frames={frames} window={window:?} status={status} THIS-TREE builds={} sorts={} clones={} vec-collects={} vec-allocs={} alloc-total={} alloc-gt256={} alloc-le256={}",
                changed.0,
                changed.2,
                changed.3,
                changed.4,
                changed.3 + changed.4,
                changed_allocs.4,
                changed_allocs.3,
                changed_allocs.2
            );
            println!(
                "A1-SORTSCRATCH {label} frames={frames} bare-tail_means alloc-total={} alloc-gt256={} alloc-le256={} (std stable sort, the shipped comparator and sort kind)",
                sort_probe.4, sort_probe.3, sort_probe.2
            );
            println!(
                "A1-SCRATCH {label} frames={frames} window={window:?} retained-scratch={} B ({} f64 slots, private memory, not CPU)",
                stats.scratch.capacity() * size_of::<f64>(),
                stats.scratch.capacity()
            );

            // The base side of the census (the shipped shape, measured on this tree).
            assert_eq!(
                base.2, 2,
                "the shipped shape sorts the window twice ({label})"
            );
            assert_eq!(
                base.3, 2,
                "the shipped shape clones the window twice ({label})"
            );
            assert_eq!(base.4, 2, "the shipped shape builds two vectors ({label})");
            if frames >= 100 {
                assert!(
                    base_allocs.3 >= 3,
                    "the shipped shape allocates over-256-byte blocks ({label})"
                );
            }
            assert!(
                base_allocs.4 >= changed_allocs.4 + 3,
                "the shipped shape pays at least three more allocations: the window vector, \
                 the 1 s vector and the two sort clones ({label})"
            );
            // The A3 counter claims, measured on this tree.
            assert_eq!(changed.0, 1, "one model call ({label})");
            assert_eq!(changed.2, 1, "A3 counter: sorts per model == 1 ({label})");
            assert_eq!(
                changed.3, 0,
                "A3 counter: to_vec clones per model == 0 ({label})"
            );
            assert_eq!(
                changed.4, 0,
                "A3 counter: window/1 s vectors per model == 0 ({label})"
            );
            assert_eq!(
                changed_allocs.2, 2,
                "a warm model call allocates exactly the two GameStats name strings ({label})"
            );
            assert_eq!(
                changed_allocs.3, sort_probe.3,
                "a warm model call pays no over-256-byte allocation beyond std's stable-sort scratch ({label})"
            );
            assert!(
                changed_allocs.4 < base_allocs.4,
                "the warm model call allocates fewer blocks than the shipped shape ({label})"
            );
        }
    }

    /// The counting seams are NOT vacuous: a known tick of each is seen by the reader, so a
    /// census row that reads 0 means "the site did not run", not "the counter is dead".
    #[test]
    fn model_shape_counters_are_not_vacuous() {
        shape_reset();
        model_build_tick();
        stale_build_tick();
        test_vec_collect_tick();
        crate::history::model_sort_tick();
        crate::history::model_clone_tick();
        let counts = shape_counts();
        println!(
            "A1-NONVACUOUS known-ticks builds={} stale={} sorts={} clones={} collects={}",
            counts.0, counts.1, counts.2, counts.3, counts.4
        );
        assert_eq!(
            counts,
            (1, 1, 1, 1, 1),
            "each counter must see exactly one known tick"
        );
        shape_reset();
        assert_eq!(
            shape_counts(),
            (0, 0, 0, 0, 0),
            "the reset clears every counter"
        );
    }

    /// A1: the stale-window census over the CURRENT cadence. It drives the shipped
    /// reconciliation loop, so it measures the shipped and the changed shape on the same
    /// seam; the cadence is read from `idle_deadline` (the unit-52 shape), not assumed.
    #[test]
    fn stale_window_census_counts_reconciliations_and_discarded_models() {
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        let (line_tx, line_rx) = mpsc::sync_channel(4);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,42,16.0".into()).unwrap();
        let (updates, rx) = mpsc::channel();
        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        capture
            .drain(&cfg, 1, &updates, Instant::now())
            .expect("the fresh frame drains");
        assert!(rx.recv().unwrap().available, "the fresh frame publishes");

        // The CURRENT cadence, measured from the shipped deadline set.
        let mut probe = Capture::default();
        probe.last_frame = Some(Instant::now() - Duration::from_secs(6));
        println!(
            "A1-CADENCE stale-idle-6s running-capture-pump-step={:?} no-capture-idle-deadline={:?} (the handoff's nominal 50 ms loop is HISTORICAL: unit 52 parks on last_frame+STALE_AFTER and +2*STALE_AFTER)",
            PRESENTMON_PUMP_BOUND,
            probe.idle_deadline(&cfg, Instant::now())
        );

        shape_reset();
        // The shipped stale window: idle 6.00..9.85 s at the 50 ms pump cadence = 78
        // reconciliations, the first of which is the fresh -> stale transition. The bound
        // stays strictly inside (5 s, 10 s]: `reconcile` stamps its own `now`, so the
        // effective idle drifts by microseconds and a nominal 10.000 s row would cross
        // into the expired arm.
        let steps = 78usize;
        let mut publications = 0usize;
        for index in 0..steps {
            let idle = Duration::from_secs(6) + Duration::from_millis(50 * index as u64);
            let last = Instant::now() - idle;
            capture.last_frame = Some(last);
            capture.last_publish = Some(last);
            capture.last_target_poll = Some(Instant::now());
            capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
            if rx.try_recv().is_ok() {
                publications += 1;
            }
        }
        let stale_window = shape_counts();
        println!(
            "A1-STALE window=idle-6.00-to-9.85s cadence=50ms reconciliations={steps} THIS-TREE model-builds={} stale-model-builds={} publications={} discarded={} (the shipped shape at HEAD, same seam and cadence, is measured in the u57-census-base log)",
            stale_window.0,
            stale_window.1,
            publications,
            stale_window.1.saturating_sub(publications as u64)
        );
        assert_eq!(publications, 1, "the stale detail publishes exactly once");
        assert_eq!(
            stale_window.1, 1,
            "A3 counter: exactly one model per fresh -> stale transition"
        );
        assert_eq!(
            stale_window.1 - publications as u64,
            0,
            "A3 counter: stale reconciliations that build a model to discard == 0"
        );

        // The stale -> unavailable(expired) edge: idle 11..15 s, same cadence.
        shape_reset();
        let mut expired_publications = 0usize;
        for index in 0..steps {
            let idle = Duration::from_secs(11) + Duration::from_millis(50 * index as u64);
            let last = Instant::now() - idle;
            capture.last_frame = Some(last);
            capture.last_publish = Some(last);
            capture.last_target_poll = Some(Instant::now());
            capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
            if rx.try_recv().is_ok() {
                expired_publications += 1;
            }
        }
        let expired_window = shape_counts();
        println!(
            "A1-STALE window=idle-11.00-to-14.85s cadence=50ms reconciliations={steps} THIS-TREE model-builds={} stale-model-builds={} publications={} discarded={} (the expired arm builds no model in either shape)",
            expired_window.0,
            expired_window.1,
            expired_publications,
            expired_window.1.saturating_sub(expired_publications as u64)
        );
        assert_eq!(
            expired_publications, 1,
            "the expired detail publishes exactly once"
        );
        assert_eq!(
            expired_window.1, 0,
            "A3 counter: the expired arm builds no model, so it discards none"
        );
    }

    /// A2.3 + A3: the shipped publication sequence end to end, on the seams the shipped
    /// tests use. `fresh -> stale -> unavailable(expired) -> fresh`, both detail strings
    /// verbatim, the `metric.stale = metric.valid` folding, and the `last_unavailable`
    /// bookkeeping including the shipped frame-site clear.
    #[test]
    fn stale_publication_sequence_is_the_shipped_shape_and_builds_no_discarded_model() {
        let cfg = Config {
            presentmon_target_mode: "foreground".into(),
            ..Config::default()
        };
        let (line_tx, line_rx) = mpsc::sync_channel(4);
        line_tx
            .send("Application,ProcessID,FrameTime".into())
            .unwrap();
        line_tx.send("game.exe,42,16.0".into()).unwrap();
        let (updates, rx) = mpsc::channel();
        let mut capture = Capture::default();
        capture.lines = Some(line_rx);
        capture.target = ProcessInfo {
            pid: 42,
            creation_time: 1,
            name: "game.exe".into(),
            image_path: String::new(),
        };
        capture.stats = Some(Stats::new(30.0, cfg.presentmon_window));
        shape_reset();
        capture
            .drain(&cfg, 1, &updates, Instant::now())
            .expect("the fresh frame drains");
        let fresh = rx.recv().unwrap();
        assert!(
            fresh.available && !fresh.error && fresh.game.fps.valid && !fresh.game.fps.stale,
            "the fresh publish is valid and not stale"
        );
        assert_eq!(fresh.detail, "game.exe");

        // fresh -> stale: 78 iterations at the 50 ms cadence, one publication.
        let stale_at = Instant::now() - Duration::from_secs(6);
        let mut stale_update = None;
        for index in 0..78 {
            let last = stale_at - Duration::from_millis(50 * index);
            capture.last_frame = Some(last);
            capture.last_publish = Some(last);
            capture.last_target_poll = Some(Instant::now());
            capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
            while let Ok(update) = rx.try_recv() {
                assert!(
                    stale_update.is_none(),
                    "the stale detail is published once, at the transition"
                );
                stale_update = Some(update);
            }
        }
        let stale = stale_update.expect("the fresh -> stale edge published");
        assert_eq!(stale.detail, "capture stale: no frames for 5 seconds");
        assert!(stale.game.active && stale.game.fps.valid && stale.game.fps.stale);
        assert!(stale.game.one_percent.stale && stale.game.point_one_low.stale);
        assert!(stale.game.frame_time_ms.stale);
        assert!(stale.game.one_percent.valid && stale.game.point_one_low.valid);
        let mid = shape_counts();
        println!(
            "A2-3 STALE-SEQUENCE fresh->stale model-builds={} stale-model-builds={} publications=1",
            mid.0, mid.1
        );
        assert_eq!(mid.1, 1, "the 78 stale iterations built ONE model");

        // stale -> unavailable(expired): 78 iterations, one publication, no model at all.
        shape_reset();
        let expired_at = Instant::now() - Duration::from_secs(11);
        let mut expired_update = None;
        for index in 0..78 {
            let last = expired_at - Duration::from_millis(50 * index);
            capture.last_frame = Some(last);
            capture.last_publish = Some(last);
            capture.last_target_poll = Some(Instant::now());
            capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
            while let Ok(update) = rx.try_recv() {
                assert!(
                    expired_update.is_none(),
                    "the expired detail is published once, at the transition"
                );
                expired_update = Some(update);
            }
        }
        let expired = expired_update.expect("the stale -> expired edge published");
        assert_eq!(expired.detail, "capture unavailable: stale data expired");
        assert!(!expired.game.active && !expired.game.fps.valid);
        let after_expired = shape_counts();
        println!(
            "A2-3 STALE-SEQUENCE stale->expired model-builds={} stale-model-builds={} publications=1",
            after_expired.0, after_expired.1
        );
        assert_eq!(after_expired.1, 0, "the expired arm built no model");

        // The fresh edge clears the ledger at the shipped site and republishes.
        line_tx.send("game.exe,42,16.0".into()).unwrap();
        capture.last_target_poll = Some(Instant::now());
        capture.reconcile(&cfg, 1, &updates, &|_, _| unreachable!());
        let fresh_again = rx.recv().unwrap();
        assert!(fresh_again.available, "a new frame returns to fresh");
        assert_eq!(fresh_again.detail, "game.exe");
        assert!(rx.try_recv().is_err(), "the fresh edge publishes once");
        assert!(
            capture.last_unavailable.is_empty(),
            "the shipped frame site clears the unavailable ledger"
        );
        println!(
            "A2-3 STALE-SEQUENCE fresh->stale->expired->fresh published=3 total-model-builds={} stale-model-builds={}",
            after_expired.0, after_expired.1
        );
    }

    /// A2.2: a warm `model` call allocates NOTHING on the window path, and no process-name
    /// clone was added. Measured with the lane's counting allocator; what remains is the
    /// two owned `GameStats` name strings (which the shipped shape also pays) and std's own
    /// stable-sort merge scratch, named by the `tail_means` attribution probe.
    #[test]
    fn warm_model_call_allocates_no_window_vector_and_no_name_clone() {
        use crate::backends::alloc_count;
        let window = Config::default().presentmon_window;
        let frames = 3_600usize;
        let period_ns = (window.as_nanos() as u64) / (frames as u64);
        let observed = Instant::now();
        let mut stats = Stats::new(30.0, window);
        stats.frames = (0..frames)
            .map(|index| {
                (
                    observed - Duration::from_nanos(period_ns * (frames as u64 - index as u64)),
                    16.0,
                )
            })
            .collect();
        let now_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let _ = stats.model(now_wall, observed, window, "game.exe");
        let warm = {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            let _ = stats.model(now_wall, observed, window, "game.exe");
            alloc_count::subtract(alloc_count::snapshot(), before)
        };
        let base_warm = {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            let _ = base_model_shape(&stats, now_wall, observed, window, "game.exe");
            alloc_count::subtract(alloc_count::snapshot(), before)
        };
        let mut sort_buffer = vec![16.0f64; frames];
        let sort_probe = {
            let _window = alloc_count::window();
            alloc_count::watch_thread();
            alloc_count::reset();
            let before = alloc_count::snapshot();
            let _ = crate::history::tail_means(&mut sort_buffer);
            alloc_count::subtract(alloc_count::snapshot(), before)
        };
        println!(
            "A2-2-ALLOC default-window frames=3600 window={:?} SHAPED-BASELINE total={} gt256={} le256={} | THIS-TREE total={} gt256={} le256={} | SORT-PROBE total={} gt256={} le256={}",
            window,
            base_warm.4,
            base_warm.3,
            base_warm.2,
            warm.4,
            warm.3,
            warm.2,
            sort_probe.4,
            sort_probe.3,
            sort_probe.2
        );
        println!(
            "A2-2-SCRATCH default-window frames=3600 retained-scratch={} B ({} f64 slots)",
            stats.scratch.capacity() * size_of::<f64>(),
            stats.scratch.capacity()
        );
        assert_eq!(
            warm.2, 2,
            "a warm model call allocates exactly the two GameStats name strings, so no \
             process-name clone was added to this path"
        );
        assert_eq!(
            warm.3, sort_probe.3,
            "a warm model call pays no over-256-byte allocation beyond std's stable-sort scratch"
        );
        assert_eq!(
            warm.4,
            sort_probe.4 + 2,
            "a warm model call is the sort's own scratch plus the two name strings"
        );
        assert!(
            base_warm.3 >= 3 && base_warm.4 > warm.4,
            "the shipped shape allocated the window/clone blocks this unit removed"
        );
    }

    /// A2.1/A3 counters, stated as the acceptance assertions for this tree.
    #[test]
    fn model_shape_is_one_sorting_no_clone_and_no_scratch_vector() {
        let window = Config::default().presentmon_window;
        let observed = Instant::now();
        let mut stats = Stats::new(30.0, window);
        stats.frames = (0..3_600usize)
            .map(|index| {
                (
                    observed - Duration::from_nanos(index as u64 * 16_666_667),
                    16.0,
                )
            })
            .rev()
            .collect();
        let now_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let _ = stats.model(now_wall, observed, window, "game.exe");
        shape_reset();
        let _ = stats.model(now_wall, observed, window, "game.exe");
        let counts = shape_counts();
        println!(
            "A1-AFTER model-per-call builds={} sorts={} clones={} vec-collects={} vec-allocs={}",
            counts.0,
            counts.2,
            counts.3,
            counts.4,
            counts.3 + counts.4
        );
        assert_eq!(counts.0, 1, "one model call");
        assert_eq!(counts.2, 1, "A3 counter: sorts per model == 1");
        assert_eq!(counts.3, 0, "A3 counter: to_vec clones per model == 0");
        assert_eq!(counts.4, 0, "A3 counter: window/1 s vectors per model == 0");
        assert!(
            stats.scratch.capacity() >= 3_600,
            "the scratch keeps its capacity after a warm call (retained memory)"
        );
        println!(
            "A1-AFTER retained-scratch={} B at the default 60 s window",
            stats.scratch.capacity() * size_of::<f64>()
        );
    }

    /// A3: the tail means pinned BY ABSOLUTE VALUE to the shipped definition, so a
    /// mutation of the SHARED arithmetic (a `floor` where the shipped code says `ceil`, a
    /// dropped `clamp`, a whole-window mean for the 1 s mean, a dropped last-window
    /// fallback) goes RED even where the base fixture and the changed model share the code.
    /// The shipped arithmetic: `n = ceil(len * p)` clamped to `[1, len]`, the mean over the
    /// highest `n` of a STABLE ordering, `fps = 1000 / mean`. Every `want` is hand-derived.
    #[test]
    fn model_tail_means_match_the_shipped_arithmetic_by_value() {
        let now_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(1.0);
        let check = |name: &str, got: f64, want: f64| {
            assert!(
                close(got, want),
                "{name}: the model published {got}, the shipped arithmetic gives {want}"
            );
            // F-AUD-OPT06-A-4: this print used to claim `MATCH=True` as a literal. The assert
            // above is the evidence; a print must not dress up as one.
            println!("A3-VALUE {name} model={:.12} shipped={:.12}", got, want);
        };
        let model_at = |stats: &mut Stats, gap: Duration| -> GameStats {
            let observed_now = now_after(stats, gap);
            let window = stats.window;
            stats.model(now_wall, observed_now, window, "game.exe")
        };

        // len 100, values 1..=100, all inside the 1 s recent window.
        // 1%: n = ceil(100 * 0.01) = 1 -> tail {100} -> 1000/100.
        // 0.1%: n = ceil(100 * 0.001) = 1 -> tail {100} -> 1000/100.
        // recent mean = 50.5 -> fps = 1000/50.5.
        let mut stats = fixture_stats(secs(60), 0, &spread(100, |i| (i + 1) as f64));
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("len100-1percent", game.one_percent.value, 10.0);
        check("len100-01percent", game.point_one_low.value, 10.0);
        check("len100-fps", game.fps.value, 1000.0 / 50.5);
        check("len100-frametime", game.frame_time_ms.value, 50.5);

        // len 101, values 1..=101. 1%: n = ceil(1.01) = 2 (a `floor` lands on 1) ->
        // tail {100,101} mean 100.5. 0.1%: n = ceil(0.101) = 1 -> 1000/101.
        let mut stats = fixture_stats(secs(60), 0, &spread(101, |i| (i + 1) as f64));
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("len101-1percent", game.one_percent.value, 1000.0 / 100.5);
        check("len101-01percent", game.point_one_low.value, 1000.0 / 101.0);
        check("len101-fps", game.fps.value, 1000.0 / 51.0);

        // len 1001, values 0..=1000, offsets -1001..-1 ms: the 1 s recent window drops the
        // oldest frame (the boundary the shipped filter pins) and keeps 1..=1000.
        // 1%: n = ceil(10.01) = 11 (a `floor` lands on 10) -> tail 990..=1000 mean 995.
        // 0.1%: n = ceil(1.001) = 2 (a `floor` lands on 1) -> tail {999,1000} mean 999.5.
        let mut stats = fixture_stats(secs(60), 0, &spread(1_001, |i| i as f64));
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("len1001-1percent", game.one_percent.value, 1000.0 / 995.0);
        check(
            "len1001-01percent",
            game.point_one_low.value,
            1000.0 / 999.5,
        );
        check("len1001-fps", game.fps.value, 1000.0 / 500.5);

        // The clamp, by value: len 1 -> n = ceil(0.01) = 1 -> the only frame.
        let mut stats = fixture_stats(secs(60), 0, &[(0, 25.0)]);
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("len1-1percent", game.one_percent.value, 40.0);
        check("len1-01percent", game.point_one_low.value, 40.0);
        check("len1-fps", game.fps.value, 40.0);

        // The stable-ordering requirement, by value: a NaN at the head of 100 equal frames
        // stays OUT of the 1% tail (n = 1), so the 1% low is exactly 1000/16. An unstable
        // sort may permute the NaN into the tail and publish NaN.
        let mut stats = fixture_stats(
            secs(60),
            0,
            &spread(100, |i| if i == 0 { f64::NAN } else { 16.0 }),
        );
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("nan-head-100-1percent", game.one_percent.value, 62.5);
        check("nan-head-100-01percent", game.point_one_low.value, 62.5);
        let mut stats = fixture_stats(
            secs(60),
            0,
            &spread(1_000, |i| if i < 3 { f64::NAN } else { 16.0 }),
        );
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("nan-head-1000-01percent", game.point_one_low.value, 62.5);

        // The 1 s mean is the RECENT set, not the whole window: the last 1 s is 1..=100
        // (mean 50.5) while the whole 60 s window tops out at 200,199.
        let mut frames: Vec<(i64, f64)> = (0..100usize)
            .map(|index| ((index * 500) as i64 - 60_000, (101 + index) as f64))
            .collect();
        frames.extend((1..=100i64).map(|index| (-index, index as f64)));
        let mut stats = fixture_stats(secs(60), 0, &frames);
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("recent-not-whole-window-fps", game.fps.value, 1000.0 / 50.5);
        check(
            "whole-window-1percent",
            game.one_percent.value,
            1000.0 / 199.5,
        );

        // The empty-recent fallback is the LAST frame time OF THE WINDOW in arrival order
        // (not the sorted tail): with a 5 s gap the recent set is empty, the newest
        // in-window frame is 3.0 and the sorted tail is 42.0.
        let mut stats = fixture_stats(
            secs(60),
            0,
            &[(-30_000, 42.0), (-29_000, 7.0), (-28_000, 3.0)],
        );
        let game = model_at(&mut stats, Duration::from_secs(5));
        check(
            "empty-recent-fallback-frametime",
            game.frame_time_ms.value,
            3.0,
        );
        check("empty-recent-fallback-fps", game.fps.value, 1000.0 / 3.0);
        check(
            "empty-recent-tail-1percent",
            game.one_percent.value,
            1000.0 / 42.0,
        );
        check(
            "empty-recent-tail-01percent",
            game.point_one_low.value,
            1000.0 / 42.0,
        );

        // The two filters stay INDEPENDENT: with a window shorter than 1 s the recent set
        // legitimately includes frames outside the window (the shipped shape).
        // window 500 ms, observed_now = -99 ms: window = {-400, -100} = {5, 9};
        // recent (>= -1099 ms) = {-1000, -400, -100} = {11, 5, 9} -> mean 25/3.
        let mut stats = fixture_stats(
            Duration::from_millis(500),
            0,
            &[(-2_000, 7.0), (-1_000, 11.0), (-400, 5.0), (-100, 9.0)],
        );
        let game = model_at(&mut stats, Duration::from_millis(1));
        check(
            "independent-filters-frametime",
            game.frame_time_ms.value,
            25.0 / 3.0,
        );
        check(
            "independent-filters-1percent",
            game.one_percent.value,
            1000.0 / 9.0,
        );

        // The window cutoff boundary, by value: the frame exactly at `now - window` is IN,
        // one tick earlier is OUT, and that changes the published 1% low.
        let mut stats = fixture_stats(secs(60), 0, &[(-59_999, 42.0), (0, 10.0)]);
        let game = model_at(&mut stats, Duration::from_millis(1));
        check(
            "cutoff-exact-in-window-1percent",
            game.one_percent.value,
            1000.0 / 42.0,
        );
        check("cutoff-exact-fps", game.fps.value, 100.0);
        let mut stats = fixture_stats(secs(60), 0, &[(-60_000, 42.0), (0, 10.0)]);
        let game = model_at(&mut stats, Duration::from_millis(1));
        check(
            "cutoff-out-of-window-1percent",
            game.one_percent.value,
            100.0,
        );
        check(
            "cutoff-out-of-window-frametime",
            game.frame_time_ms.value,
            10.0,
        );

        // The 1 s recent boundary, by value: observed_now = newest + 1 ms, so the recent
        // cutoff is -999 ms: a frame at -999 is exactly in, -1000 is one tick out.
        let mut stats = fixture_stats(secs(60), 0, &[(-999, 42.0), (-100, 8.0), (0, 10.0)]);
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("recent-exact-in-frametime", game.frame_time_ms.value, 20.0);
        let mut stats = fixture_stats(secs(60), 0, &[(-1_000, 42.0), (-100, 8.0), (0, 10.0)]);
        let game = model_at(&mut stats, Duration::from_millis(1));
        check("recent-out-frametime", game.frame_time_ms.value, 9.0);
    }
}
