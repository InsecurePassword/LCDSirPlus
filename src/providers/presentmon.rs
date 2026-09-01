//! PresentMon console discovery, target selection, owned capture, and CSV statistics.

#![cfg(windows)]

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
use std::process::{Child, Command, Stdio};
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
use crate::history::{mean, percentile_high};
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
        let Some(fingerprint) =
            catalog_field(fields, "IsFingerprintDetected").and_then(Json::as_bool)
        else {
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
            if !creative && ops && fingerprint && !manual {
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
            foreground_pid,
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
        foreground_pid: u32,
        current: &ProcessInfo,
        catalog: &CatalogState,
    ) -> Option<RankedCandidate> {
        self.prune(now);
        if let Some(best) = self.best_workload(now, 0, catalog) {
            if current.pid != 0 {
                if let Some(workload) = self.workload_score(current) {
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
            if let Some(foreground) = self.best_workload(now, foreground_pid, catalog) {
                if best.process == foreground.process
                    || best.workload?.value - foreground.workload?.value < SWITCH_MARGIN
                {
                    return Some(foreground);
                }
            }
            return Some(best);
        }
        self.best_activity(now, foreground_pid, catalog)
            .or_else(|| self.best_activity(now, 0, catalog))
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
            if !["FrameTime", "MsBetweenPresents", "MsBetweenDisplayChange"]
                .iter()
                .any(|name| header.contains_key(&name.to_ascii_lowercase()))
            {
                return Err(
                    "CSV header has no frame-time column (expected FrameTime for --v2_metrics)"
                        .into(),
                );
            }
            self.header = Some(header);
            return Ok(None);
        }
        let get = |names: &[&str]| -> &str {
            let header = self.header.as_ref().unwrap();
            names
                .iter()
                .find_map(|name| {
                    header
                        .get(&name.to_ascii_lowercase())
                        .and_then(|i| fields.get(*i))
                })
                .map(|v| v.trim())
                .unwrap_or("")
        };
        let pid = get(&["ProcessID"])
            .parse::<u32>()
            .map_err(|e| format!("PresentMon ProcessID: {e}"))?;
        let text = get(&["FrameTime", "MsBetweenPresents", "MsBetweenDisplayChange"]);
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
            get(&["CPUStartTime", "CPUStartTimeInMs"]),
            "CPUStartTime",
            u64::MAX as f64,
        )?;
        let cpu_busy_ms = optional_metric(get(&["CPUBusy", "MsCPUBusy"]), "CPUBusy", 10_000.0)?;
        let gpu_busy_ms = optional_metric(get(&["GPUBusy", "MsGPUBusy"]), "GPUBusy", 10_000.0)?;
        Ok(Some(Frame {
            at,
            observed_at: Instant::now(),
            application: get(&["Application"]).to_string(),
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

fn csv_fields(line: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.trim().chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(ch),
        }
    }
    if quoted {
        return Err("unterminated quoted PresentMon CSV field".into());
    }
    fields.push(field);
    Ok(fields)
}

struct Stats {
    frames: VecDeque<(Instant, f64)>,
    session_start: Option<SystemTime>,
    stutters: i32,
    threshold: f64,
    window: Duration,
}

impl Stats {
    fn new(threshold: f64, window: Duration) -> Self {
        Self {
            frames: VecDeque::new(),
            session_start: None,
            stutters: 0,
            threshold,
            window,
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

    fn model(
        &self,
        now: SystemTime,
        observed_now: Instant,
        window: Duration,
        process: &str,
    ) -> GameStats {
        let cutoff = observed_now.checked_sub(window);
        let frame_times: Vec<f64> = self
            .frames
            .iter()
            .filter(|(at, _)| cutoff.is_none_or(|cutoff| *at >= cutoff))
            .map(|(_, value)| *value)
            .collect();
        if frame_times.is_empty() {
            return GameStats::default();
        }
        let recent_cutoff = observed_now.checked_sub(Duration::from_secs(1));
        let recent: Vec<f64> = self
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
            stutters: self.stutters,
            session_start: self.session_start,
        }
    }
}

pub fn spawn(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-presentmon".into())
        .spawn(move || run(config, generation, shutdown, tx))
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

fn wait_bounded(
    timeout: Duration,
    mut exited: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if exited()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "exit was not confirmed within {} ms",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_child_bounded(child: &mut Child, timeout: Duration) -> Result<(), String> {
    wait_bounded(timeout, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("query child status: {error}"))
    })
}

fn stop_child_bounded(child: &mut Child, timeout: Duration) -> Result<(), String> {
    if child.try_wait().is_ok_and(|status| status.is_some()) {
        return Ok(());
    }
    if let Err(kill) = child.kill() {
        return match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(format!("kill child: {kill}; child is still running")),
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

fn run(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<Update>,
) {
    let mut capture = Capture::default();
    let mut current_generation = 0;
    while !shutdown.load(Ordering::Relaxed) {
        let (request_generation, cfg) = current_request(&config, &generation);
        if request_generation != current_generation {
            if capture.stop_and_report_cleanup(&tx, request_generation, Instant::now()) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            capture = Capture::default();
            current_generation = request_generation;
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
        std::thread::sleep(Duration::from_millis(50));
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
                        with_diagnostic(
                            format!(
                                "PresentMon exited ({status}) while capturing {}; the target may have exited",
                                target
                            ),
                            stderr,
                        ),
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
            self.prepare_presenting(cfg, generation, tx, now, || {
                foreground_process().map(|process| process.pid).unwrap_or(0)
            });
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
            if let (Some(last), Some(stats)) = (self.last_frame, self.stats.as_ref()) {
                let idle = now.duration_since(last);
                let Some((game, detail)) = stale_projection(
                    stats,
                    SystemTime::now(),
                    now,
                    cfg.presentmon_window,
                    &self.target.name,
                    idle,
                ) else {
                    return;
                };
                if self.last_unavailable != detail {
                    self.last_unavailable = detail.clone();
                    let _ = tx.send(Update {
                        generation,
                        game,
                        detail,
                        ..Default::default()
                    });
                }
            }
        }
        if presenting {
            self.pick_presenter(cfg, now, || {
                foreground_process().map(|process| process.pid).unwrap_or(0)
            });
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
        let stale = self
            .last_frame
            .zip(self.stats.as_ref())
            .and_then(|(last, stats)| {
                let idle = now.duration_since(last);
                stale_projection(
                    stats,
                    SystemTime::now(),
                    now,
                    cfg.presentmon_window,
                    &self.target.name,
                    idle,
                )
                .map(|projection| (idle, projection))
            });
        if let Some((idle, (game, detail))) = stale {
            if self.last_unavailable != detail {
                self.last_unavailable = detail.clone();
                let _ = tx.send(Update {
                    generation,
                    game,
                    detail,
                    ..Default::default()
                });
            }
            if idle > ACTIVITY_EXPIRY {
                let expired = self.target.clone();
                self.activity.remove(&expired);
                self.select_presenter(ProcessInfo::default(), cfg);
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
            foreground_pid(),
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
            let line = match lines.try_recv() {
                Ok(line) => line,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("PresentMon output reader disconnected".into());
                }
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
            let game = self.stats.as_ref().unwrap().model(
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
        stop_child: impl FnOnce(&mut Child) -> Result<(), String>,
        cleanup: impl FnOnce(&str) -> Result<(), String>,
    ) -> String {
        let mut shutdown = Vec::new();
        self.stop_diagnostic.clear();
        if let Some(mut child) = self.child.take() {
            if let Err(error) = stop_child(&mut child) {
                self.stop_diagnostic = format!("teardown: {error}");
                self.child = Some(child);
                return String::new();
            }
        }
        self.lines = None;
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

fn stale_projection(
    stats: &Stats,
    now: SystemTime,
    observed_now: Instant,
    window: Duration,
    process: &str,
    idle: Duration,
) -> Option<(GameStats, String)> {
    if idle <= STALE_AFTER {
        return None;
    }
    if idle > STALE_AFTER + STALE_AFTER {
        return Some((
            GameStats::default(),
            "capture unavailable: stale data expired".into(),
        ));
    }
    let mut game = stats.model(now, observed_now, window + STALE_AFTER, process);
    for metric in [
        &mut game.fps,
        &mut game.one_percent,
        &mut game.point_one_low,
        &mut game.frame_time_ms,
    ] {
        metric.stale = metric.valid;
    }
    Some((game, "capture stale: no frames for 5 seconds".into()))
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
    verify_authenticode(&pinned.file, &pinned.path)?;
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
        let detail = with_diagnostic(
            format!("PresentMon exited ({status})"),
            diagnostic_text(&output.lock().unwrap()),
        );
        assert_eq!(status.code(), Some(5));
        assert!(detail.contains("access denied"));

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
    fn r5_catalog_requires_exact_qualified_full_path() {
        let game_path = r"C:\PrivateFixture\EscapeFromTarkov.exe";
        let records = vec![
            catalog_record(&[game_path], false, true, true, false),
            catalog_record(&[game_path], false, true, true, false),
            catalog_record(
                &[r"C:\PrivateFixture\Creative.exe"],
                true,
                true,
                true,
                false,
            ),
            catalog_record(&[r"C:\PrivateFixture\NoOps.exe"], false, false, true, false),
            catalog_record(
                &[r"C:\PrivateFixture\NoFingerprint.exe"],
                false,
                true,
                false,
                false,
            ),
            catalog_record(&[r"C:\PrivateFixture\Manual.exe"], false, true, true, true),
            catalog_record(
                &[
                    "EscapeFromTarkov.exe",
                    r"\\server\share\EscapeFromTarkov.exe",
                    r"\\?\C:\PrivateFixture\EscapeFromTarkov.exe",
                    r"C:\PrivateFixture\..\EscapeFromTarkov.exe",
                    r"C:\PrivateFixture\EscapeFromTarkov.exe:stream",
                ],
                false,
                true,
                true,
                false,
            ),
        ];
        let paths = parse_catalog(&catalog_document(&records)).unwrap();
        assert_eq!(paths, vec![normalize_catalog_path(game_path).unwrap()]);
        let catalog = CatalogState::Available(paths);
        let process = |path: &str| ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "EscapeFromTarkov.exe".into(),
            image_path: path.into(),
        };
        assert_eq!(
            catalog.classify(&process(r"c:\PRIVATEfixture\ESCAPEFROMTARKOV.EXE")),
            CatalogClassification::Game
        );
        assert_eq!(
            catalog.classify(&process(r"C:\Other\EscapeFromTarkov.exe")),
            CatalogClassification::NotGame
        );
        for rejected in [
            r"C:\PrivateFixture\Creative.exe",
            r"C:\PrivateFixture\NoOps.exe",
            r"C:\PrivateFixture\NoFingerprint.exe",
            r"C:\PrivateFixture\Manual.exe",
        ] {
            assert_eq!(
                catalog.classify(&process(rejected)),
                CatalogClassification::NotGame
            );
        }
        assert_eq!(
            catalog.classify(&process(r"C:\PrivateFixture\Absent.exe")),
            CatalogClassification::NotGame
        );
        for rejected in [
            "EscapeFromTarkov.exe",
            r"\\server\share\EscapeFromTarkov.exe",
            r"\\?\C:\PrivateFixture\EscapeFromTarkov.exe",
            r"C:\PrivateFixture\..\EscapeFromTarkov.exe",
            r"C:\PrivateFixture\EscapeFromTarkov.exe:stream",
            r"C:\PrivateFixture\unsafe.\EscapeFromTarkov.exe",
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
        let duplicate = r#"{"DetectedFiles":[],"IsCreativeApplication":false,"IsOpsSupported":true,"IsOpsSupported":true,"IsFingerprintDetected":true,"IsManuallyAdded":false}"#.to_string();
        let unsupported = [
            ("unknown", r#"{"Other":true}"#.to_string()),
            ("null", "null".to_string()),
            ("missing", missing.clone()),
            ("wrong_type", wrong_type.clone()),
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
    fn r5_selector_uses_valid_catalog_and_unavailable_fallback() {
        let cfg = Config::default();
        let base = Instant::now();
        let game = ProcessInfo {
            pid: 1,
            creation_time: 1,
            name: "Game.exe".into(),
            image_path: r"C:\Games\Game.exe".into(),
        };
        let chrome = ProcessInfo {
            pid: 2,
            creation_time: 2,
            name: "chrome.exe".into(),
            image_path: r"C:\Program Files\Chrome\chrome.exe".into(),
        };
        let opencode = ProcessInfo {
            pid: 3,
            creation_time: 3,
            name: "OpenCode.exe".into(),
            image_path: r"C:\Tools\OpenCode.exe".into(),
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
            &[(&game, 0.40), (&chrome, 0.90), (&opencode, 0.95)],
        );
        valid.catalog.state =
            CatalogState::Available(vec![normalize_catalog_path(&game.image_path).unwrap()]);
        valid.pick_presenter(&cfg, base + Duration::from_secs(3), || chrome.pid);
        assert_eq!(valid.target, game);

        let mut no_game = Capture::default();
        populate(&mut no_game, &[(&chrome, 0.90), (&opencode, 0.95)]);
        no_game.catalog.state = CatalogState::Available(Vec::new());
        no_game.pick_presenter(&cfg, base + Duration::from_secs(3), || chrome.pid);
        assert_eq!(no_game.target, ProcessInfo::default());

        let mut unavailable = Capture::default();
        populate(
            &mut unavailable,
            &[(&game, 0.40), (&chrome, 0.90), (&opencode, 0.95)],
        );
        unavailable.catalog.state = CatalogState::Unavailable;
        unavailable.pick_presenter(&cfg, base + Duration::from_secs(3), || 0);
        assert_eq!(unavailable.target, opencode);

        let persistent_cfg = Config {
            presentmon_persist: true,
            ..cfg.clone()
        };
        let mut persistent = Capture::default();
        populate(
            &mut persistent,
            &[(&game, 0.40), (&chrome, 0.90), (&opencode, 0.95)],
        );
        persistent.catalog.state =
            CatalogState::Available(vec![normalize_catalog_path(&game.image_path).unwrap()]);
        persistent.pick_presenter(&persistent_cfg, base + Duration::from_secs(3), || 0);
        assert_eq!(persistent.target, opencode);

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
            r#"{{"Applications":[{{"DetectedFiles":["C:\\PrivateFixture\\SecretGame.exe"],"IsCreativeApplication":false,"IsOpsSupported":true,"IsFingerprintDetected":true,"IsManuallyAdded":false,"DisplayName":"{private_name}"}}]}}"#
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
        capture.stop_with(|_| Ok(()), |_| Ok(()));
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
        capture.stop_with(|_| Ok(()), |_| Ok(()));
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

        let (stale, _) = stale_projection(
            &stats,
            frame_at + Duration::from_secs(6),
            observed + Duration::from_secs(6),
            Duration::from_secs(5),
            "game.exe",
            Duration::from_secs(6),
        )
        .unwrap();
        assert!(stale.active && stale.fps.valid && stale.fps.stale);

        let (expired, _) = stale_projection(
            &stats,
            frame_at + Duration::from_secs(11),
            observed + Duration::from_secs(11),
            Duration::from_secs(5),
            "game.exe",
            Duration::from_secs(11),
        )
        .unwrap();
        assert!(!expired.active && !expired.fps.valid);
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
        capture.stop_with(|_| Ok(()), |_| Err("injected failure".into()));
        #[rustfmt::skip]
        assert_eq!(capture.owned_session.as_deref(), Some("LCDSirPlus-42-owned"));
        capture.schedule_retry(Instant::now());
        assert!(retry_pending(capture.retry_after, Instant::now()));
        #[rustfmt::skip]
        capture.stop_with(|_| Ok(()), |name| { assert_eq!(name, "LCDSirPlus-42-owned"); Ok(()) });
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
        capture.stop_with(
            |_| Err("forced kill/wait timeout".into()),
            |_| panic!("ETW cleanup must wait for confirmed child exit"),
        );
        assert!(capture.child.is_some() && capture.reader.is_some());
        assert_eq!(
            capture.owned_session.as_deref(),
            Some("LCDSirPlus-42-owned")
        );
        assert_eq!(capture.target.pid, 42);
        assert!(capture.stop_diagnostic.contains("forced kill/wait timeout"));
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
        assert!(wait_bounded(Duration::ZERO, || Ok(false))
            .unwrap_err()
            .contains("exit was not confirmed"));
        drop(guard);
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
        let (_updates, thread) = spawn(config, generation, Arc::clone(&shutdown));
        shutdown.store(true, Ordering::Relaxed);
        assert!(thread.join().is_ok());
    }
}
