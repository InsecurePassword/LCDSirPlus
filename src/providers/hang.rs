//! Query-only hung-window detection and disposable test harness.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, BOOL, ERROR_ACCESS_DENIED, ERROR_SUCCESS, ERROR_TIMEOUT, FILETIME,
    HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetAncestor,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    PeekMessageW, RegisterClassW, SendMessageTimeoutW, ShowWindow, TranslateMessage, CW_USEDEFAULT,
    GA_ROOT, MSG, PM_REMOVE, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SW_SHOW, WM_NULL, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

use crate::config::Config;
use crate::input::Event;
use crate::model::HungTarget;

const TICK: Duration = Duration::from_millis(50);
const POLL_BUDGET: Duration = Duration::from_secs(5);
const MAX_WINDOWS: usize = 4096;
const MAX_TITLE_CHARS: usize = 256;

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub targets: Vec<HungTarget>,
    pub available: bool,
    pub error: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    hwnd: usize,
    pid: u32,
    creation_time: u64,
    image_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Policy {
    enabled: bool,
    safe_mode: bool,
    interval: Duration,
    timeout: Duration,
    failures: i32,
    minimum: Duration,
    ignore: Vec<String>,
    button: i32,
    hold: Duration,
}

impl From<&Config> for Policy {
    fn from(cfg: &Config) -> Self {
        Self {
            enabled: cfg.hang_enabled,
            safe_mode: cfg.safe_mode,
            interval: cfg.hang_probe_interval,
            timeout: cfg.hang_probe_timeout,
            failures: cfg.hang_failures,
            minimum: cfg.hang_minimum,
            ignore: cfg.hang_ignore.clone(),
            button: cfg.hang_button,
            hold: cfg.hang_hold,
        }
    }
}

#[derive(Clone, Debug)]
struct Press {
    source: &'static str,
    started: Instant,
    backward: bool,
    bound: Option<(usize, HungTarget, Policy)>,
    canceled: bool,
}

#[derive(Clone, Debug)]
pub struct Audit {
    pub target: HungTarget,
    pub outcome: &'static str,
}

#[derive(Clone, Debug)]
pub enum HoldCommand {
    None,
    Cycle { index: usize, backward: bool },
    Terminate(HungTarget),
    Audit(Audit),
}

#[derive(Default)]
pub struct HoldState {
    presses: [Option<Press>; 4],
    pub hung_index: usize,
    pub hung_detail: bool,
}

impl HoldState {
    pub fn reconcile(
        &mut self,
        cfg: &Config,
        targets: &[HungTarget],
        provider_available: bool,
    ) -> Option<Audit> {
        let policy = Policy::from(cfg);
        for press in self.presses.iter_mut().flatten() {
            let Some((index, target, bound_policy)) = &press.bound else {
                continue;
            };
            if !press.canceled
                && (!provider_available
                    || !policy.active()
                    || &policy != bound_policy
                    || targets.get(*index) != Some(target))
            {
                press.canceled = true;
                return Some(Audit {
                    target: target.clone(),
                    outcome: "canceled-continuity",
                });
            }
        }
        None
    }

    pub fn cancel_reload(&mut self) -> Option<Audit> {
        for press in self.presses.iter_mut().flatten() {
            if let Some((_, target, _)) = &press.bound {
                if !press.canceled {
                    press.canceled = true;
                    return Some(Audit {
                        target: target.clone(),
                        outcome: "canceled-config-reload",
                    });
                }
            }
        }
        None
    }

    pub fn progress(&self, now: Instant, cfg: &Config) -> f64 {
        self.presses
            .iter()
            .flatten()
            .find(|press| press.bound.is_some() && !press.canceled)
            .map(|press| {
                now.saturating_duration_since(press.started).as_secs_f64()
                    / cfg.hang_hold.as_secs_f64()
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    pub fn event(
        &mut self,
        event: Event,
        cfg: &Config,
        targets: &[HungTarget],
        provider_available: bool,
    ) -> HoldCommand {
        if event.index >= self.presses.len() {
            return HoldCommand::None;
        }
        let button = cfg.hang_button.saturating_sub(1) as usize;
        if let Some((owner, press)) = self.presses.iter().enumerate().find_map(|(index, press)| {
            press
                .as_ref()
                .filter(|p| p.bound.is_some())
                .map(|p| (index, p))
        }) {
            if event.index != owner || press.source != event.source {
                return HoldCommand::None;
            }
        }
        if event.down {
            if let Some(existing) = &self.presses[event.index] {
                if existing.source != event.source || existing.canceled {
                    return HoldCommand::None;
                }
            }
            let bound = if event.index == button
                && cfg.hang_enabled
                && !cfg.safe_mode
                && provider_available
            {
                (!targets.is_empty()).then(|| {
                    let index = self.hung_index % targets.len();
                    (index, targets[index].clone(), Policy::from(cfg))
                })
            } else {
                None
            };
            self.presses[event.index] = Some(Press {
                source: event.source,
                started: event.at,
                backward: event.backward,
                bound,
                canceled: false,
            });
            return HoldCommand::None;
        }

        let Some(press) = self.presses[event.index].take() else {
            return HoldCommand::None;
        };
        if press.source != event.source {
            self.presses[event.index] = Some(press);
            return HoldCommand::None;
        }
        let Some((index, target, policy)) = press.bound else {
            if event.canceled || event.at < press.started {
                return HoldCommand::None;
            }
            if event.index == button && !cfg.safe_mode && !targets.is_empty() {
                return HoldCommand::None;
            }
            return HoldCommand::Cycle {
                index: event.index,
                backward: press.backward || event.backward,
            };
        };
        if event.canceled || press.canceled {
            return HoldCommand::Audit(Audit {
                target,
                outcome: if event.canceled {
                    "canceled-device-loss"
                } else {
                    "canceled-continuity"
                },
            });
        }
        if !provider_available
            || !policy.active()
            || Policy::from(cfg) != policy
            || targets.get(index) != Some(&target)
        {
            return HoldCommand::Audit(Audit {
                target,
                outcome: "refused-release-policy-or-target",
            });
        }
        let duration = event.at.saturating_duration_since(press.started);
        if duration > maximum_press_duration(cfg.hang_hold) {
            return HoldCommand::Audit(Audit {
                target,
                outcome: "refused-stale-release",
            });
        }
        if duration < cfg.hang_hold {
            if self.hung_detail && targets.len() > 1 {
                self.hung_index = (index + 1) % targets.len();
                self.hung_detail = false;
            } else {
                self.hung_detail = !self.hung_detail;
            }
            return HoldCommand::Audit(Audit {
                target,
                outcome: "canceled-early-release",
            });
        }
        HoldCommand::Terminate(target)
    }
}

fn maximum_press_duration(hold: Duration) -> Duration {
    (hold.saturating_mul(2)).clamp(Duration::from_secs(3), Duration::from_secs(15))
}

impl Policy {
    fn active(&self) -> bool {
        self.enabled && !self.safe_mode
    }
}

impl From<&HungTarget> for Identity {
    fn from(target: &HungTarget) -> Self {
        Self {
            hwnd: target.hwnd,
            pid: target.pid,
            creation_time: target.creation_time,
            image_path: target.image_path.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct Observation {
    target: HungTarget,
    responsive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeResult {
    Responsive,
    Timeout,
    Indeterminate,
}

#[derive(Clone, Debug)]
struct Record {
    target: HungTarget,
    first_failure: Duration,
    failures: i32,
}

#[derive(Default)]
struct Tracker {
    records: HashMap<Identity, Record>,
}

#[derive(Default)]
struct Cadence {
    last_attempt: Option<Duration>,
}

impl Cadence {
    fn poll_due(&mut self, policy: &Policy, now: Duration) -> bool {
        if !policy.active() {
            self.last_attempt = None;
            return false;
        }
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_sub(last) < policy.interval)
        {
            return false;
        }
        self.last_attempt = Some(now);
        true
    }
}

fn reset_on_policy_change(
    current: &mut Option<Policy>,
    next: &Policy,
    tracker: &mut Tracker,
    cadence: &mut Cadence,
) -> bool {
    let changed = current.as_ref().is_some_and(|old| old != next);
    if changed {
        tracker.clear();
        cadence.last_attempt = None;
    }
    *current = Some(next.clone());
    changed
}

fn poll_policy_current(started: &Policy, latest: &Policy) -> bool {
    latest.active() && latest == started
}

impl Tracker {
    fn clear(&mut self) {
        self.records.clear();
    }

    fn update(
        &mut self,
        now: Duration,
        observations: Vec<Observation>,
        failures_required: i32,
        minimum: Duration,
    ) -> Vec<HungTarget> {
        let previous = std::mem::take(&mut self.records);
        for observation in observations {
            if observation.responsive {
                continue;
            }
            let identity = Identity::from(&observation.target);
            let mut record = previous.get(&identity).cloned().unwrap_or(Record {
                target: observation.target.clone(),
                first_failure: now,
                failures: 0,
            });
            record.target = observation.target;
            record.failures += 1;
            self.records.insert(identity, record);
        }

        let mut confirmed: Vec<&Record> = self
            .records
            .values()
            .filter(|record| {
                record.failures >= failures_required
                    && now.saturating_sub(record.first_failure) >= minimum
            })
            .collect();
        confirmed.sort_by(|a, b| {
            a.first_failure
                .cmp(&b.first_failure)
                .then_with(|| a.target.image_path.cmp(&b.target.image_path))
                .then_with(|| a.target.pid.cmp(&b.target.pid))
                .then_with(|| a.target.hwnd.cmp(&b.target.hwnd))
        });
        confirmed
            .into_iter()
            .map(|record| record.target.clone())
            .collect()
    }
}

pub fn spawn(
    config: Arc<RwLock<Config>>,
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-hang".into())
        .spawn(move || worker(config, shutdown, tx))
        .expect("hung detector thread");
    (rx, thread)
}

fn worker(config: Arc<RwLock<Config>>, shutdown: Arc<AtomicBool>, tx: mpsc::Sender<Update>) {
    let started = Instant::now();
    let mut tracker = Tracker::default();
    let mut cadence = Cadence::default();
    let mut current_policy = None;
    let mut disabled_published = false;
    let mut published_available = None;
    let mut published_targets = None;
    while !shutdown.load(Ordering::Relaxed) {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
        let policy = Policy::from(&cfg);
        if reset_on_policy_change(&mut current_policy, &policy, &mut tracker, &mut cadence) {
            log_worker_transition(
                &mut published_available,
                &mut published_targets,
                policy.active(),
                0,
            );
            if tx
                .send(Update {
                    targets: Vec::new(),
                    available: policy.active(),
                    error: None,
                })
                .is_err()
            {
                return;
            }
            disabled_published = !policy.active();
            continue;
        }
        if !policy.active() {
            tracker.clear();
            cadence.poll_due(&policy, started.elapsed());
            if !disabled_published && {
                log_worker_transition(&mut published_available, &mut published_targets, false, 0);
                tx.send(Update {
                    targets: Vec::new(),
                    available: false,
                    error: None,
                })
                .is_err()
            } {
                return;
            }
            disabled_published = true;
            std::thread::sleep(TICK);
            continue;
        }
        disabled_published = false;
        if !cadence.poll_due(&policy, started.elapsed()) {
            std::thread::sleep(TICK);
            continue;
        }
        let result = enumerate_and_probe(policy.timeout, &policy.ignore, None);
        let latest = config.read().unwrap_or_else(|e| e.into_inner());
        let latest_policy = Policy::from(&*latest);
        if !poll_policy_current(&policy, &latest_policy) {
            reset_on_policy_change(
                &mut current_policy,
                &latest_policy,
                &mut tracker,
                &mut cadence,
            );
            disabled_published = !latest_policy.active();
            log_worker_transition(
                &mut published_available,
                &mut published_targets,
                latest_policy.active(),
                0,
            );
            if tx
                .send(Update {
                    targets: Vec::new(),
                    available: latest_policy.active(),
                    error: None,
                })
                .is_err()
            {
                return;
            }
            continue;
        }
        let update = match result {
            Ok(observations) => Update {
                targets: tracker.update(
                    started.elapsed(),
                    observations,
                    policy.failures,
                    policy.minimum,
                ),
                available: true,
                error: None,
            },
            Err(error) => {
                tracker.clear();
                Update {
                    targets: Vec::new(),
                    available: false,
                    error: Some(error),
                }
            }
        };
        log_worker_transition(
            &mut published_available,
            &mut published_targets,
            update.available,
            update.targets.len(),
        );
        if tx.send(update).is_err() {
            return;
        }
    }
}

fn log_worker_transition(
    available: &mut Option<bool>,
    targets: &mut Option<usize>,
    next_available: bool,
    next_targets: usize,
) {
    if *available != Some(next_available) {
        crate::log_info!("hung-window detector available={next_available}");
        *available = Some(next_available);
    }
    if *targets != Some(next_targets) {
        crate::log_info!("hung-window detector confirmed_targets={next_targets}");
        *targets = Some(next_targets);
    }
}

struct EnumerationContext {
    ignored: HashSet<String>,
    windows_dir: String,
    self_pid: u32,
    only_pid: Option<u32>,
    timeout: Duration,
    deadline: Instant,
    observations: Vec<Observation>,
    visited: usize,
    stopped: bool,
    limit_exceeded: bool,
}

fn enumerate_and_probe(
    timeout: Duration,
    ignore: &[String],
    only_pid: Option<u32>,
) -> Result<Vec<Observation>, &'static str> {
    let windows_dir = windows_directory()?;
    let mut ignored = ignored_names(ignore);
    ignored.insert("lcdsirplus.exe".into());
    let mut context = EnumerationContext {
        ignored,
        windows_dir,
        self_pid: unsafe { GetCurrentProcessId() },
        only_pid,
        timeout,
        deadline: Instant::now() + POLL_BUDGET,
        observations: Vec::new(),
        visited: 0,
        stopped: false,
        limit_exceeded: false,
    };
    let result = unsafe {
        EnumWindows(
            Some(enumerate_callback),
            LPARAM((&mut context as *mut EnumerationContext) as isize),
        )
    };
    if result.is_err() && !context.stopped {
        return Err("window enumeration failed");
    }
    if Instant::now() >= context.deadline {
        return Err("hung-window poll budget exceeded");
    }
    if context.limit_exceeded {
        return Err("hung-window enumeration limit exceeded");
    }
    Ok(context.observations)
}

unsafe extern "system" fn enumerate_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = &mut *(lparam.0 as *mut EnumerationContext);
    if Instant::now() >= context.deadline {
        context.stopped = true;
        return BOOL(0);
    }
    context.visited += 1;
    if context.visited > MAX_WINDOWS {
        context.stopped = true;
        context.limit_exceeded = true;
        return BOOL(0);
    }
    let Some(target) = describe_candidate(
        hwnd,
        context.self_pid,
        context.only_pid,
        &context.ignored,
        &context.windows_dir,
    ) else {
        return BOOL(1);
    };
    let remaining = context.deadline.saturating_duration_since(Instant::now());
    let probe_timeout = context.timeout.min(remaining).max(Duration::from_millis(1));
    match probe(hwnd, probe_timeout) {
        ProbeResult::Responsive => context.observations.push(Observation {
            target,
            responsive: true,
        }),
        ProbeResult::Timeout if identity_still_matches(hwnd, &target) => {
            context.observations.push(Observation {
                target,
                responsive: false,
            });
        }
        ProbeResult::Timeout | ProbeResult::Indeterminate => {}
    }
    BOOL(1)
}

unsafe fn describe_candidate(
    hwnd: HWND,
    self_pid: u32,
    only_pid: Option<u32>,
    ignored: &HashSet<String>,
    windows_dir: &str,
) -> Option<HungTarget> {
    if !IsWindowVisible(hwnd).as_bool() {
        return None;
    }
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 || length > 32767 {
        return None;
    }
    let mut title_buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut title_buffer);
    if copied <= 0 {
        return None;
    }
    let title = safe_display_text(&String::from_utf16_lossy(&title_buffer[..copied as usize]));
    if title.is_empty() {
        return None;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == self_pid || only_pid.is_some_and(|expected| expected != pid) {
        return None;
    }
    let (creation_time, image_path, process_name) = query_identity(pid).ok()?;
    if excluded(&process_name, &image_path, ignored, windows_dir) {
        return None;
    }
    Some(HungTarget {
        hwnd: hwnd.0 as usize,
        pid,
        creation_time,
        image_path,
        process_name,
        title,
    })
}

unsafe fn probe(hwnd: HWND, timeout: Duration) -> ProbeResult {
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    SetLastError(ERROR_SUCCESS);
    let result = SendMessageTimeoutW(
        hwnd,
        WM_NULL,
        WPARAM(0),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
        timeout_ms,
        None,
    );
    classify_probe_result(result.0 != 0, GetLastError())
}

fn classify_probe_result(succeeded: bool, error: WIN32_ERROR) -> ProbeResult {
    if succeeded {
        ProbeResult::Responsive
    } else if error == ERROR_TIMEOUT {
        ProbeResult::Timeout
    } else {
        ProbeResult::Indeterminate
    }
}

unsafe fn identity_still_matches(hwnd: HWND, expected: &HungTarget) -> bool {
    if !IsWindow(hwnd).as_bool() {
        return false;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let Ok((creation_time, image_path, _)) = query_identity(pid) else {
        return false;
    };
    identity_matches(expected, hwnd.0 as usize, pid, creation_time, &image_path)
}

fn identity_matches(
    expected: &HungTarget,
    hwnd: usize,
    pid: u32,
    creation_time: u64,
    image_path: &str,
) -> bool {
    expected.hwnd == hwnd
        && expected.pid == pid
        && expected.creation_time == creation_time
        && expected.image_path == normalize_path(image_path)
}

fn query_identity(pid: u32) -> Result<(u64, String, String), ()> {
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|_| ())?;
    let result = query_identity_handle(handle);
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    result
}

fn query_identity_handle(handle: HANDLE) -> Result<(u64, String, String), ()> {
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
        .map_err(|_| ())?;
    }
    if length == 0 || length as usize > buffer.len() {
        return Err(());
    }
    let image_path = normalize_path(&String::from_utf16_lossy(&buffer[..length as usize]));
    let process_name = safe_display_text(
        Path::new(&image_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""),
    );
    if image_path.is_empty() || process_name.is_empty() {
        return Err(());
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)
            .map_err(|_| ())?;
    }
    let creation_time = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    if creation_time == 0 {
        return Err(());
    }
    Ok((creation_time, image_path, process_name))
}

fn windows_directory() -> Result<String, &'static str> {
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("authoritative Windows directory unavailable");
    }
    let path = normalize_path(&String::from_utf16_lossy(&buffer[..length]));
    if path.is_empty() {
        Err("authoritative Windows directory unavailable")
    } else {
        Ok(path)
    }
}

fn ignored_names(values: &[String]) -> HashSet<String> {
    let mut ignored: HashSet<String> = values
        .iter()
        .filter_map(|value| {
            Path::new(value.trim())
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_lowercase())
        })
        .collect();
    for name in [
        "system",
        "registry",
        "memory compression",
        "idle",
        "secure system",
    ] {
        ignored.insert(name.into());
    }
    ignored
}

fn excluded(name: &str, path: &str, ignored: &HashSet<String>, windows_dir: &str) -> bool {
    let name = name.to_lowercase();
    let path = normalize_path(path);
    let windows_dir = normalize_path(windows_dir);
    ignored.contains(&name)
        || path == windows_dir
        || path
            .strip_prefix(&windows_dir)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

fn normalize_path(value: &str) -> String {
    let replaced = value.trim().replace('/', "\\");
    replaced
        .strip_prefix("\\\\?\\")
        .unwrap_or(&replaced)
        .trim_end_matches('\\')
        .to_lowercase()
}

fn safe_display_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    Success,
    Refused,
    Recovered,
    IdentityMismatch,
    AccessDenied,
    TerminateFailure,
    WaitTimeout,
}

impl ActionOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Refused => "refused",
            Self::Recovered => "recovered",
            Self::IdentityMismatch => "identity-mismatch",
            Self::AccessDenied => "access-denied",
            Self::TerminateFailure => "terminate-failure",
            Self::WaitTimeout => "wait-timeout",
        }
    }
}

enum ActionWait {
    Signaled,
    Timeout,
    Failed,
}

trait ActionApi {
    type Process;

    fn window(&mut self, hwnd: usize) -> Result<HungTarget, ActionOutcome>;
    fn probe(&mut self, hwnd: usize, timeout: Duration) -> ProbeResult;
    fn open(&mut self, pid: u32) -> Result<Self::Process, ActionOutcome>;
    fn process_identity(&mut self, process: &Self::Process)
        -> Result<(u64, String), ActionOutcome>;
    fn terminate(&mut self, process: &Self::Process) -> Result<(), ActionOutcome>;
    fn wait(&mut self, process: &Self::Process, timeout: Duration) -> ActionWait;
}

struct OwnedProcess(HANDLE);

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

struct NativeActionApi;

impl ActionApi for NativeActionApi {
    type Process = OwnedProcess;

    fn window(&mut self, hwnd: usize) -> Result<HungTarget, ActionOutcome> {
        unsafe { action_window(HWND(hwnd as *mut _)) }
    }

    fn probe(&mut self, hwnd: usize, timeout: Duration) -> ProbeResult {
        unsafe { probe(HWND(hwnd as *mut _), timeout) }
    }

    fn open(&mut self, pid: u32) -> Result<Self::Process, ActionOutcome> {
        let rights = PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
        unsafe { OpenProcess(rights, false, pid) }
            .map(OwnedProcess)
            .map_err(|error| {
                if error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                    ActionOutcome::AccessDenied
                } else {
                    ActionOutcome::Refused
                }
            })
    }

    fn process_identity(
        &mut self,
        process: &Self::Process,
    ) -> Result<(u64, String), ActionOutcome> {
        query_identity_handle(process.0)
            .map(|(creation, path, _)| (creation, path))
            .map_err(|_| ActionOutcome::IdentityMismatch)
    }

    fn terminate(&mut self, process: &Self::Process) -> Result<(), ActionOutcome> {
        unsafe { TerminateProcess(process.0, 0x4c43_4453) }
            .map_err(|_| ActionOutcome::TerminateFailure)
    }

    fn wait(&mut self, process: &Self::Process, timeout: Duration) -> ActionWait {
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { WaitForSingleObject(process.0, millis) } {
            WAIT_OBJECT_0 => ActionWait::Signaled,
            WAIT_TIMEOUT => ActionWait::Timeout,
            _ => ActionWait::Failed,
        }
    }
}

unsafe fn action_window(hwnd: HWND) -> Result<HungTarget, ActionOutcome> {
    if !IsWindow(hwnd).as_bool()
        || !IsWindowVisible(hwnd).as_bool()
        || GetAncestor(hwnd, GA_ROOT) != hwnd
    {
        return Err(ActionOutcome::Refused);
    }
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 || length > 32767 {
        return Err(ActionOutcome::Refused);
    }
    let mut title_buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut title_buffer);
    if copied <= 0 {
        return Err(ActionOutcome::Refused);
    }
    let title = safe_display_text(&String::from_utf16_lossy(&title_buffer[..copied as usize]));
    if title.is_empty() {
        return Err(ActionOutcome::Refused);
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let (creation_time, image_path, process_name) =
        query_identity(pid).map_err(|_| ActionOutcome::Refused)?;
    Ok(HungTarget {
        hwnd: hwnd.0 as usize,
        pid,
        creation_time,
        image_path,
        process_name,
        title,
    })
}

pub fn terminate_bound_target(target: &HungTarget, cfg: &Config) -> ActionOutcome {
    terminate_bound_target_with(&mut NativeActionApi, target, cfg)
}

fn terminate_bound_target_with<A: ActionApi>(
    api: &mut A,
    target: &HungTarget,
    cfg: &Config,
) -> ActionOutcome {
    if cfg.safe_mode
        || !cfg.hang_enabled
        || target.hwnd == 0
        || target.pid == 0
        || target.creation_time == 0
        || target.image_path.is_empty()
        || target.process_name.is_empty()
    {
        return ActionOutcome::Refused;
    }
    let Ok(windows_dir) = windows_directory() else {
        return ActionOutcome::Refused;
    };
    let ignored = ignored_names(&cfg.hang_ignore);
    let self_pid = unsafe { GetCurrentProcessId() };
    let validate_window = |current: &HungTarget| {
        if current != target {
            ActionOutcome::IdentityMismatch
        } else if current.pid == self_pid
            || excluded(
                &current.process_name,
                &current.image_path,
                &ignored,
                &windows_dir,
            )
        {
            ActionOutcome::Refused
        } else {
            ActionOutcome::Success
        }
    };

    let current = match api.window(target.hwnd) {
        Ok(current) => current,
        Err(outcome) => return outcome,
    };
    if validate_window(&current) != ActionOutcome::Success {
        return validate_window(&current);
    }
    match api.probe(target.hwnd, cfg.hang_probe_timeout) {
        ProbeResult::Responsive => return ActionOutcome::Recovered,
        ProbeResult::Indeterminate => return ActionOutcome::Refused,
        ProbeResult::Timeout => {}
    }
    match api.window(target.hwnd) {
        Ok(current) if validate_window(&current) == ActionOutcome::Success => {}
        Ok(current) => return validate_window(&current),
        Err(outcome) => return outcome,
    }

    let process = match api.open(target.pid) {
        Ok(process) => process,
        Err(outcome) => return outcome,
    };
    if !api
        .process_identity(&process)
        .is_ok_and(|(creation, path)| {
            creation == target.creation_time && normalize_path(&path) == target.image_path
        })
    {
        return ActionOutcome::IdentityMismatch;
    }
    match api.window(target.hwnd) {
        Ok(current) if validate_window(&current) == ActionOutcome::Success => {}
        Ok(current) => return validate_window(&current),
        Err(outcome) => return outcome,
    }
    match api.probe(target.hwnd, cfg.hang_probe_timeout) {
        ProbeResult::Responsive => return ActionOutcome::Recovered,
        ProbeResult::Indeterminate => return ActionOutcome::Refused,
        ProbeResult::Timeout => {}
    }
    match api.window(target.hwnd) {
        Ok(current) if validate_window(&current) == ActionOutcome::Success => {}
        Ok(current) => return validate_window(&current),
        Err(outcome) => return outcome,
    }
    if let Err(outcome) = api.terminate(&process) {
        return outcome;
    }
    match api.wait(&process, Duration::from_secs(5)) {
        ActionWait::Signaled => ActionOutcome::Success,
        ActionWait::Timeout => ActionOutcome::WaitTimeout,
        ActionWait::Failed => ActionOutcome::TerminateFailure,
    }
}

unsafe extern "system" fn harness_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if message == WM_NULL {
        std::thread::sleep(Duration::from_secs(1));
        return windows::Win32::Foundation::LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

pub fn run_harness(duration: Duration) -> i32 {
    let current = match std::env::current_exe() {
        Ok(current) => current,
        Err(_) => {
            eprintln!("hang harness failed: current executable unavailable");
            return 1;
        }
    };
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("LCDSirPlus.exe"))
    {
        return run_owned_harness(&current, duration);
    }
    match create_harness_window(duration) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            1
        }
    }
}

fn run_owned_harness(source: &Path, duration: Duration) -> i32 {
    let mut cleanup = match spawn_renamed_harness(source, duration, false) {
        Ok(cleanup) => cleanup,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            return 1;
        }
    };
    let status = match wait_harness(&mut cleanup, duration + Duration::from_secs(2)) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            return 1;
        }
    };
    if let Err(error) = cleanup.remove_tree() {
        eprintln!("hang harness failed: temporary harness cleanup failed: {error}");
        return 1;
    }
    status.code().unwrap_or(1)
}

fn create_harness_window(duration: Duration) -> Result<(), &'static str> {
    let pid = unsafe { GetCurrentProcessId() };
    let class = format!("LCDSIRPLUS_HANG_HARNESS_{pid}\0");
    let title = format!("LCDSirPlus Disposable Hang Harness {pid}\0");
    let class_wide: Vec<u16> = class.encode_utf16().collect();
    let title_wide: Vec<u16> = title.encode_utf16().collect();
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|_| "module handle unavailable")?;
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(harness_wndproc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_wide.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&window_class) == 0 {
            return Err("window class registration failed");
        }
        let hwnd = CreateWindowExW(
            Default::default(),
            PCWSTR(class_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            160,
            None,
            None,
            instance,
            None,
        )
        .map_err(|_| "window creation failed")?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
    }
    println!("HANG-HARNESS READY pid={pid}");
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(TICK);
    }
    Ok(())
}

pub fn run_smoke() -> i32 {
    match smoke() {
        Ok(target) => {
            println!(
                "HANG-DETECTOR SMOKE OK pid={} process={}",
                target.pid, target.process_name
            );
            0
        }
        Err(error) => {
            eprintln!("HANG-DETECTOR SMOKE FAILED: {error}");
            1
        }
    }
}

struct HarnessCleanup {
    child: Option<Child>,
    root: PathBuf,
}

impl HarnessCleanup {
    fn remove_tree(&mut self) -> std::io::Result<()> {
        if self.root.as_os_str().is_empty() {
            return Ok(());
        }
        std::fs::remove_dir_all(&self.root)?;
        self.root.clear();
        Ok(())
    }
}

impl Drop for HarnessCleanup {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                // This handle is created only by a smoke; normal application code cannot reach it.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = self.remove_tree();
    }
}

fn spawn_renamed_harness(
    source: &Path,
    duration: Duration,
    quiet: bool,
) -> Result<HarnessCleanup, String> {
    let root = std::env::temp_dir().join(format!(
        "lcdsirplus-hang-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir(&root).map_err(|_| "temporary harness directory creation failed")?;
    let mut cleanup = HarnessCleanup { child: None, root };
    let harness = cleanup.root.join("LCDSirPlusHangHarness.exe");
    if let Err(error) = std::fs::copy(source, &harness) {
        return Err(format!("temporary harness copy failed: {error}"));
    }
    let mut command = std::process::Command::new(&harness);
    command.args([
        "--hang-test-harness",
        "--duration-secs",
        &duration.as_secs().to_string(),
    ]);
    if quiet {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    cleanup.child = Some(
        command
            .spawn()
            .map_err(|error| format!("temporary harness start failed: {error}"))?,
    );
    Ok(cleanup)
}

fn spawn_harness(duration_secs: u64) -> Result<HarnessCleanup, String> {
    spawn_renamed_harness(&smoke_source()?, Duration::from_secs(duration_secs), true)
}

fn detect_harness(pid: u32) -> Result<HungTarget, String> {
    let started = Instant::now();
    let mut tracker = Tracker::default();
    let mut detected = None;
    let mut last_error = None;
    while started.elapsed() < Duration::from_secs(5) {
        match enumerate_and_probe(Duration::from_millis(100), &[], Some(pid)) {
            Ok(observations) => {
                let targets = tracker.update(
                    started.elapsed(),
                    observations,
                    2,
                    Duration::from_millis(300),
                );
                if let Some(target) = targets.into_iter().next() {
                    detected = Some(target);
                    break;
                }
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    detected.ok_or_else(|| {
        last_error
            .unwrap_or("purpose-built hung window was not confirmed")
            .into()
    })
}

#[cfg(test)]
fn detect_harness_with_shipped_policy(pid: u32) -> Result<HungTarget, String> {
    let policy = Policy::from(&Config::default());
    let started = Instant::now();
    let mut tracker = Tracker::default();
    let mut cadence = Cadence::default();
    while started.elapsed() < Duration::from_secs(15) {
        if !cadence.poll_due(&policy, started.elapsed()) {
            std::thread::sleep(TICK);
            continue;
        }
        let observations =
            enumerate_and_probe(policy.timeout, &policy.ignore, None).map_err(str::to_string)?;
        if let Some(target) = tracker
            .update(
                started.elapsed(),
                observations,
                policy.failures,
                policy.minimum,
            )
            .into_iter()
            .find(|target| target.pid == pid)
        {
            return Ok(target);
        }
        std::thread::sleep(TICK);
    }
    Err("purpose-built hung window was not confirmed with shipped policy".into())
}

fn wait_harness(cleanup: &mut HarnessCleanup, timeout: Duration) -> Result<ExitStatus, String> {
    let exit_deadline = Instant::now() + timeout;
    loop {
        match cleanup.child.as_mut().unwrap().try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < exit_deadline => std::thread::sleep(TICK),
            Ok(None) => return Err("temporary harness did not exit within its bound".into()),
            Err(_) => return Err("temporary harness status unavailable".into()),
        }
    }
}

fn smoke() -> Result<HungTarget, String> {
    use std::io::BufRead;

    let source = smoke_source()?;
    if source.file_name().and_then(|name| name.to_str()) != Some("LCDSirPlus.exe") {
        return Err("public harness executable must be named LCDSirPlus.exe".into());
    }
    let owner = std::process::Command::new(&source)
        .args(["--hang-test-harness", "--duration-secs", "6"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("public harness start failed: {error}"))?;
    let mut owner = HarnessCleanup {
        child: Some(owner),
        root: PathBuf::new(),
    };
    let stdout = owner.child.as_mut().unwrap().stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = ready_tx.send(result);
    });
    let observed = (|| -> Result<(HungTarget, PathBuf), String> {
        let line = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "public harness child did not become ready")?
            .map_err(|_| "public harness output unavailable")?;
        let pid = line
            .trim()
            .strip_prefix("HANG-HARNESS READY pid=")
            .and_then(|pid| pid.parse::<u32>().ok())
            .ok_or("public harness did not report its renamed child")?;
        let target = detect_harness(pid)?;
        if !target
            .process_name
            .eq_ignore_ascii_case("LCDSirPlusHangHarness.exe")
        {
            return Err("public harness child retained the ignored production basename".into());
        }
        let owned_root = Path::new(&target.image_path)
            .parent()
            .ok_or("public harness child path unavailable")?
            .to_path_buf();
        Ok((target, owned_root))
    })();
    let status = wait_harness(&mut owner, Duration::from_secs(8))?;
    if !status.success() {
        return Err(format!("public harness owner exited with {status}"));
    }
    let (target, owned_root) = observed?;
    if owned_root.exists() {
        return Err("public harness owner did not clean its temporary tree".into());
    }
    Ok(target)
}

pub fn run_action_smoke(negative: bool) -> i32 {
    match action_smoke(negative) {
        Ok(pid) => {
            if negative {
                println!("HANG-ACTION NEGATIVE SMOKE OK survived_checks_pid={pid}");
            } else {
                println!("HANG-ACTION POSITIVE SMOKE OK killed_pid={pid}");
            }
            0
        }
        Err(error) => {
            eprintln!("HANG-ACTION SMOKE FAILED: {error}");
            1
        }
    }
}

fn action_smoke(negative: bool) -> Result<u32, String> {
    let mut cleanup = spawn_harness(20)?;
    let pid = cleanup.child.as_ref().unwrap().id();
    let target = detect_harness(pid)?;
    let cfg = Config {
        hang_hold: Duration::from_secs(1),
        hang_probe_timeout: Duration::from_millis(100),
        ..Config::default()
    };
    let started = Instant::now();
    let mut hold = HoldState::default();
    hold.event(
        Event {
            index: 2,
            down: true,
            backward: false,
            canceled: false,
            at: started,
            source: "action-smoke",
        },
        &cfg,
        std::slice::from_ref(&target),
        true,
    );

    if negative {
        let early = hold.event(
            Event {
                index: 2,
                down: false,
                backward: false,
                canceled: false,
                at: started + Duration::from_millis(100),
                source: "action-smoke",
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        );
        if !matches!(early, HoldCommand::Audit(_))
            || !matches!(cleanup.child.as_mut().unwrap().try_wait(), Ok(None))
        {
            return Err("early release did not leave owned child alive".into());
        }
        let restarted = started + Duration::from_secs(1);
        hold.event(
            Event {
                index: 2,
                down: true,
                backward: false,
                canceled: false,
                at: restarted,
                source: "action-smoke",
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        );
        let mut changed = target.clone();
        changed.creation_time += 1;
        if hold.reconcile(&cfg, &[changed], true).is_none() {
            return Err("wrong identity did not cancel hold".into());
        }
        let release = hold.event(
            Event {
                index: 2,
                down: false,
                backward: false,
                canceled: false,
                at: restarted + cfg.hang_hold,
                source: "action-smoke",
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        );
        if !matches!(release, HoldCommand::Audit(_))
            || !matches!(cleanup.child.as_mut().unwrap().try_wait(), Ok(None))
        {
            return Err("wrong identity release did not leave owned child alive".into());
        }
        return Ok(pid);
    }

    if hold
        .reconcile(&cfg, std::slice::from_ref(&target), true)
        .is_some()
    {
        return Err("valid action hold canceled unexpectedly".into());
    }
    let command = hold.event(
        Event {
            index: 2,
            down: false,
            backward: false,
            canceled: false,
            at: started + cfg.hang_hold,
            source: "action-smoke",
        },
        &cfg,
        std::slice::from_ref(&target),
        true,
    );
    let HoldCommand::Terminate(bound) = command else {
        return Err("valid release did not request termination".into());
    };
    let outcome = terminate_bound_target(&bound, &cfg);
    if outcome != ActionOutcome::Success {
        return Err(format!(
            "owned child termination outcome={}",
            outcome.label()
        ));
    }
    wait_harness(&mut cleanup, Duration::from_secs(1))?;
    cleanup
        .remove_tree()
        .map_err(|_| "temporary harness cleanup failed")?;
    Ok(pid)
}

fn smoke_source() -> Result<std::path::PathBuf, String> {
    let current = std::env::current_exe().map_err(|_| "current executable unavailable")?;
    if is_cargo_test_binary(&current) {
        let release_binary = current
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join("LCDSirPlus.exe"))
            .ok_or("release harness executable unavailable")?;
        if release_binary.is_file() {
            return Ok(release_binary);
        }
        return Err("build the release executable before running the ignored harness smoke".into());
    }
    Ok(current)
}

fn is_cargo_test_binary(current: &Path) -> bool {
    current
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().starts_with("lcdsirplus-"))
        && current
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("deps")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(hwnd: usize, pid: u32, creation: u64, path: &str) -> HungTarget {
        HungTarget {
            hwnd,
            pid,
            creation_time: creation,
            image_path: normalize_path(path),
            process_name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            title: format!("target {pid}"),
        }
    }

    fn failed(target: HungTarget) -> Observation {
        Observation {
            target,
            responsive: false,
        }
    }

    #[test]
    fn tracker_requires_failures_and_duration_then_resets_on_recovery() {
        let mut tracker = Tracker::default();
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        assert!(tracker
            .update(
                Duration::ZERO,
                vec![failed(item.clone())],
                2,
                Duration::from_secs(1)
            )
            .is_empty());
        assert!(tracker
            .update(
                Duration::from_secs(1),
                vec![failed(item.clone())],
                3,
                Duration::from_secs(1)
            )
            .is_empty());
        assert_eq!(
            tracker.update(
                Duration::from_secs(2),
                vec![failed(item.clone())],
                3,
                Duration::from_secs(1)
            ),
            vec![item.clone()]
        );
        assert!(tracker
            .update(
                Duration::from_secs(3),
                vec![Observation {
                    target: item,
                    responsive: true,
                }],
                3,
                Duration::from_secs(1),
            )
            .is_empty());
        assert!(tracker.records.is_empty());
    }

    #[test]
    fn tracker_removes_absent_and_resets_replaced_identity() {
        let mut tracker = Tracker::default();
        let old = target(1, 10, 20, r"C:\Games\game.exe");
        tracker.update(Duration::ZERO, vec![failed(old)], 2, Duration::ZERO);
        assert!(tracker
            .update(Duration::from_secs(1), Vec::new(), 2, Duration::ZERO)
            .is_empty());
        assert!(tracker.records.is_empty());

        let first = target(1, 10, 20, r"C:\Games\game.exe");
        let replacement = target(1, 10, 21, r"C:\Games\game.exe");
        tracker.update(Duration::ZERO, vec![failed(first)], 2, Duration::ZERO);
        assert!(tracker
            .update(
                Duration::from_secs(1),
                vec![failed(replacement)],
                2,
                Duration::ZERO
            )
            .is_empty());

        let moved = target(1, 10, 21, r"D:\Games\game.exe");
        assert!(tracker
            .update(
                Duration::from_secs(2),
                vec![failed(moved)],
                2,
                Duration::ZERO
            )
            .is_empty());
    }

    #[test]
    fn tracker_orders_multiple_targets_deterministically() {
        let mut tracker = Tracker::default();
        let b = target(2, 20, 30, r"C:\Games\b.exe");
        let a = target(1, 10, 20, r"C:\Games\a.exe");
        tracker.update(
            Duration::ZERO,
            vec![failed(b.clone()), failed(a.clone())],
            2,
            Duration::ZERO,
        );
        assert_eq!(
            tracker.update(
                Duration::from_secs(1),
                vec![failed(b), failed(a.clone())],
                2,
                Duration::ZERO,
            )[0],
            a
        );
    }

    #[test]
    fn ignore_and_windows_directory_policy_is_case_insensitive() {
        let ignored = ignored_names(&["MyGame.EXE".into()]);
        assert!(excluded(
            "mygame.exe",
            r"D:\Games\MyGame.exe",
            &ignored,
            r"C:\Windows"
        ));
        assert!(excluded(
            "tool.exe",
            r"C:\WINDOWS\System32\tool.exe",
            &ignored,
            r"C:\Windows"
        ));
        assert!(!excluded(
            "tool.exe",
            r"C:\WindowsOld\tool.exe",
            &ignored,
            r"C:\Windows"
        ));
    }

    #[test]
    fn path_and_display_text_normalization_is_bounded() {
        assert_eq!(
            normalize_path(r"\\?\C:/Games/Test.EXE\"),
            r"c:\games\test.exe"
        );
        assert_eq!(
            safe_display_text("  title\r\n with   space "),
            "title with space"
        );
        assert_eq!(
            safe_display_text(&"x".repeat(300)).chars().count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn probe_classification_counts_only_documented_timeout() {
        assert_eq!(
            classify_probe_result(false, ERROR_TIMEOUT),
            ProbeResult::Timeout
        );
        assert_eq!(
            classify_probe_result(false, ERROR_SUCCESS),
            ProbeResult::Indeterminate
        );
        assert_eq!(
            classify_probe_result(false, WIN32_ERROR(5)),
            ProbeResult::Indeterminate
        );
        assert_eq!(
            classify_probe_result(true, ERROR_TIMEOUT),
            ProbeResult::Responsive
        );
    }

    #[test]
    fn post_probe_identity_requires_every_captured_field() {
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        assert!(identity_matches(&expected, 1, 10, 20, r"C:\GAMES\GAME.EXE"));
        assert!(!identity_matches(
            &expected,
            2,
            10,
            20,
            &expected.image_path
        ));
        assert!(!identity_matches(
            &expected,
            1,
            11,
            20,
            &expected.image_path
        ));
        assert!(!identity_matches(
            &expected,
            1,
            10,
            21,
            &expected.image_path
        ));
        assert!(!identity_matches(
            &expected,
            1,
            10,
            20,
            r"D:\Games\game.exe"
        ));
    }

    #[test]
    fn disabled_policy_clears_tracker_and_cadence_reloads() {
        let mut tracker = Tracker::default();
        tracker.update(
            Duration::ZERO,
            vec![failed(target(1, 2, 3, r"C:\Games\game.exe"))],
            1,
            Duration::ZERO,
        );
        tracker.clear();
        assert!(tracker.records.is_empty());

        let mut cadence = Cadence::default();
        let mut cfg = Config {
            hang_probe_interval: Duration::from_secs(10),
            ..Config::default()
        };
        let mut policy = Policy::from(&cfg);
        assert!(cadence.poll_due(&policy, Duration::ZERO));
        assert!(!cadence.poll_due(&policy, Duration::from_secs(5)));
        cfg.hang_probe_interval = Duration::from_secs(2);
        policy = Policy::from(&cfg);
        assert!(cadence.poll_due(&policy, Duration::from_secs(5)));
        cfg.safe_mode = true;
        policy = Policy::from(&cfg);
        assert!(!cadence.poll_due(&policy, Duration::from_secs(6)));
        cfg.safe_mode = false;
        policy = Policy::from(&cfg);
        assert!(cadence.poll_due(&policy, Duration::from_secs(6)));
        cfg.hang_enabled = false;
        policy = Policy::from(&cfg);
        assert!(!cadence.poll_due(&policy, Duration::from_secs(7)));
    }

    #[test]
    fn changed_or_disabled_policy_discards_poll_and_resets_evidence() {
        let cfg = Config::default();
        let started = Policy::from(&cfg);
        assert!(poll_policy_current(&started, &started));

        let mut changed = started.clone();
        changed.timeout += Duration::from_millis(1);
        assert!(!poll_policy_current(&started, &changed));

        let mut disabled = started.clone();
        disabled.enabled = false;
        assert!(!poll_policy_current(&started, &disabled));

        let mut tracker = Tracker::default();
        tracker.update(
            Duration::ZERO,
            vec![failed(target(1, 2, 3, r"C:\Games\game.exe"))],
            1,
            Duration::ZERO,
        );
        let mut cadence = Cadence {
            last_attempt: Some(Duration::ZERO),
        };
        let mut current = Some(started);
        assert!(reset_on_policy_change(
            &mut current,
            &changed,
            &mut tracker,
            &mut cadence
        ));
        assert!(tracker.records.is_empty());
        assert_eq!(cadence.last_attempt, None);
    }

    #[test]
    fn policy_compares_every_runtime_probe_setting() {
        let base = Policy::from(&Config::default());
        let mut values = Vec::new();
        let mut value = base.clone();
        value.enabled = !value.enabled;
        values.push(value);
        let mut value = base.clone();
        value.safe_mode = !value.safe_mode;
        values.push(value);
        let mut value = base.clone();
        value.interval += Duration::from_millis(1);
        values.push(value);
        let mut value = base.clone();
        value.timeout += Duration::from_millis(1);
        values.push(value);
        let mut value = base.clone();
        value.failures += 1;
        values.push(value);
        let mut value = base.clone();
        value.minimum += Duration::from_millis(1);
        values.push(value);
        let mut value = base.clone();
        value.ignore.push("another.exe".into());
        values.push(value);
        let mut value = base.clone();
        value.button += 1;
        values.push(value);
        let mut value = base.clone();
        value.hold += Duration::from_millis(1);
        values.push(value);
        assert!(values.into_iter().all(|value| value != base));
    }

    fn button(index: usize, down: bool, at: Instant, source: &'static str) -> Event {
        Event {
            index,
            down,
            backward: false,
            canceled: false,
            at,
            source,
        }
    }

    #[test]
    fn hold_requires_target_at_down_and_only_release_can_act() {
        let cfg = Config::default();
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let started = Instant::now();
        let mut hold = HoldState::default();
        assert!(matches!(
            hold.event(button(2, true, started, "hid"), &cfg, &[], true),
            HoldCommand::None
        ));
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));

        hold.event(
            button(2, true, started, "hid"),
            &cfg,
            std::slice::from_ref(&item),
            true,
        );
        assert_eq!(hold.progress(started + cfg.hang_hold, &cfg), 1.0);
        assert!(hold.presses[2].is_some(), "100% must not trigger action");
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Terminate(bound) if bound == item
        ));
    }

    #[test]
    fn safe_mode_short_press_cycles_without_binding_a_target() {
        let cfg = Config {
            safe_mode: true,
            ..Config::default()
        };
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let started = Instant::now();
        let mut hold = HoldState::default();
        assert!(matches!(
            hold.event(
                button(2, true, started, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));
        assert!(hold.presses[2]
            .as_ref()
            .is_some_and(|press| press.bound.is_none()));
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Cycle {
                index: 2,
                backward: false
            }
        ));
    }

    #[test]
    fn early_release_preserves_go_detail_and_duplicate_down_restarts() {
        let cfg = Config::default();
        let items = vec![
            target(1, 10, 20, r"C:\Games\a.exe"),
            target(2, 11, 21, r"C:\Games\b.exe"),
        ];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.event(
                button(2, false, started + Duration::from_millis(100), "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::Audit(Audit {
                outcome: "canceled-early-release",
                ..
            })
        ));
        assert!(hold.hung_detail);

        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        let restarted = started + cfg.hang_hold;
        hold.event(button(2, true, restarted, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.event(
                button(2, false, restarted + Duration::from_millis(100), "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::Audit(Audit {
                outcome: "canceled-early-release",
                ..
            })
        ));
        assert_eq!(hold.hung_index, 1);
        assert!(!hold.hung_detail);
    }

    #[test]
    fn hold_owner_and_monotonic_cancellation_are_exact() {
        let cfg = Config::default();
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let items = vec![item.clone()];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        hold.event(
            button(2, true, started + cfg.hang_hold, "preview"),
            &cfg,
            &items,
            true,
        );
        hold.event(
            button(1, true, started + cfg.hang_hold, "hid"),
            &cfg,
            &items,
            true,
        );
        assert_eq!(hold.presses.iter().flatten().count(), 1);
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "preview"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::None
        ));
        assert!(hold.presses[2].is_some());

        assert!(hold.reconcile(&cfg, &[], true).is_some());
        assert!(hold.reconcile(&cfg, &items, true).is_none());
        hold.event(
            button(2, true, started + cfg.hang_hold, "hid"),
            &cfg,
            &items,
            true,
        );
        assert!(hold.presses[2].as_ref().unwrap().canceled);
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::Audit(_)
        ));
    }

    #[test]
    fn device_policy_provider_and_selection_changes_cancel_until_release() {
        let base = Config::default();
        let a = target(1, 10, 20, r"C:\Games\a.exe");
        let b = target(2, 11, 21, r"C:\Games\b.exe");
        let started = Instant::now();
        for mutation in 0..7 {
            let mut hold = HoldState::default();
            hold.event(
                button(2, true, started, "hid"),
                &base,
                std::slice::from_ref(&a),
                true,
            );
            let mut cfg = base.clone();
            let mut targets = vec![a.clone()];
            let mut available = true;
            match mutation {
                0 => cfg.safe_mode = true,
                1 => cfg.hang_enabled = false,
                2 => cfg.hang_button = 4,
                3 => cfg.hang_hold += Duration::from_millis(1),
                4 => cfg.hang_ignore.push("other.exe".into()),
                5 => available = false,
                _ => targets = vec![b.clone(), a.clone()],
            }
            assert!(hold.reconcile(&cfg, &targets, available).is_some());
            assert_eq!(hold.progress(started + base.hang_hold, &base), 0.0);
        }

        let mut hold = HoldState::default();
        hold.event(
            button(2, true, started, "hid"),
            &base,
            std::slice::from_ref(&a),
            true,
        );
        let mut canceled = button(2, false, started + base.hang_hold, "hid");
        canceled.canceled = true;
        assert!(matches!(
            hold.event(canceled, &base, std::slice::from_ref(&a), true),
            HoldCommand::Audit(Audit {
                outcome: "canceled-device-loss",
                ..
            })
        ));

        let mut hold = HoldState::default();
        hold.event(
            button(2, true, started, "hid"),
            &base,
            std::slice::from_ref(&a),
            true,
        );
        assert!(hold.cancel_reload().is_some());
        assert_eq!(hold.progress(started + base.hang_hold, &base), 0.0);
    }

    struct FakeActionApi {
        target: HungTarget,
        windows: Vec<Result<HungTarget, ActionOutcome>>,
        probes: Vec<ProbeResult>,
        open: Result<(), ActionOutcome>,
        identity: Result<(u64, String), ActionOutcome>,
        terminate: Result<(), ActionOutcome>,
        wait: ActionWait,
        terminate_calls: usize,
    }

    impl FakeActionApi {
        fn valid(target: HungTarget) -> Self {
            Self {
                identity: Ok((target.creation_time, target.image_path.clone())),
                target,
                windows: Vec::new(),
                probes: vec![ProbeResult::Timeout, ProbeResult::Timeout],
                open: Ok(()),
                terminate: Ok(()),
                wait: ActionWait::Signaled,
                terminate_calls: 0,
            }
        }
    }

    impl ActionApi for FakeActionApi {
        type Process = ();

        fn window(&mut self, _hwnd: usize) -> Result<HungTarget, ActionOutcome> {
            if self.windows.is_empty() {
                Ok(self.target.clone())
            } else {
                self.windows.remove(0)
            }
        }

        fn probe(&mut self, _hwnd: usize, _timeout: Duration) -> ProbeResult {
            self.probes.remove(0)
        }

        fn open(&mut self, _pid: u32) -> Result<Self::Process, ActionOutcome> {
            self.open
        }

        fn process_identity(
            &mut self,
            _process: &Self::Process,
        ) -> Result<(u64, String), ActionOutcome> {
            self.identity.clone()
        }

        fn terminate(&mut self, _process: &Self::Process) -> Result<(), ActionOutcome> {
            self.terminate_calls += 1;
            self.terminate
        }

        fn wait(&mut self, _process: &Self::Process, _timeout: Duration) -> ActionWait {
            match self.wait {
                ActionWait::Signaled => ActionWait::Signaled,
                ActionWait::Timeout => ActionWait::Timeout,
                ActionWait::Failed => ActionWait::Failed,
            }
        }
    }

    #[test]
    fn action_adversarial_checks_prevent_termination() {
        let cfg = Config::default();
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut cases = Vec::new();

        let mut api = FakeActionApi::valid(expected.clone());
        api.windows.push(Err(ActionOutcome::Refused));
        cases.push((api, ActionOutcome::Refused));
        for changed in [
            target(2, 10, 20, r"C:\Games\game.exe"),
            target(1, 11, 20, r"C:\Games\game.exe"),
            target(1, 10, 21, r"C:\Games\game.exe"),
            target(1, 10, 20, r"D:\Games\game.exe"),
        ] {
            let mut api = FakeActionApi::valid(expected.clone());
            api.windows.push(Ok(changed));
            cases.push((api, ActionOutcome::IdentityMismatch));
        }
        let mut api = FakeActionApi::valid(expected.clone());
        api.probes[0] = ProbeResult::Responsive;
        cases.push((api, ActionOutcome::Recovered));
        let mut api = FakeActionApi::valid(expected.clone());
        api.probes[0] = ProbeResult::Indeterminate;
        cases.push((api, ActionOutcome::Refused));
        let mut api = FakeActionApi::valid(expected.clone());
        api.open = Err(ActionOutcome::AccessDenied);
        cases.push((api, ActionOutcome::AccessDenied));
        let mut api = FakeActionApi::valid(expected.clone());
        api.identity = Ok((21, expected.image_path.clone()));
        cases.push((api, ActionOutcome::IdentityMismatch));
        let mut api = FakeActionApi::valid(expected.clone());
        api.identity = Ok((20, r"D:\Games\game.exe".into()));
        cases.push((api, ActionOutcome::IdentityMismatch));
        let mut api = FakeActionApi::valid(expected.clone());
        api.windows = vec![
            Ok(expected.clone()),
            Ok(expected.clone()),
            Ok(target(1, 11, 20, r"C:\Games\game.exe")),
        ];
        cases.push((api, ActionOutcome::IdentityMismatch));

        for (mut api, outcome) in cases {
            assert_eq!(
                terminate_bound_target_with(&mut api, &expected, &cfg),
                outcome
            );
            assert_eq!(api.terminate_calls, 0);
        }

        let mut ignored_cfg = cfg.clone();
        ignored_cfg.hang_ignore.push("game.exe".into());
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &ignored_cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);

        let mut safe_cfg = cfg.clone();
        safe_cfg.safe_mode = true;
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &safe_cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);

        let self_target = target(
            1,
            unsafe { GetCurrentProcessId() },
            20,
            r"C:\Games\self.exe",
        );
        let mut api = FakeActionApi::valid(self_target.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &self_target, &cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);

        let windows_target = target(
            1,
            10,
            20,
            &format!(r"{}\protected.exe", windows_directory().unwrap()),
        );
        let mut api = FakeActionApi::valid(windows_target.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &windows_target, &cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);
    }

    #[test]
    fn action_calls_terminate_once_only_after_all_checks() {
        let cfg = Config::default();
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::Success
        );
        assert_eq!(api.terminate_calls, 1);

        let mut api = FakeActionApi::valid(expected.clone());
        api.terminate = Err(ActionOutcome::TerminateFailure);
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::TerminateFailure
        );
        assert_eq!(api.terminate_calls, 1);

        let mut api = FakeActionApi::valid(expected.clone());
        api.wait = ActionWait::Timeout;
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::WaitTimeout
        );
        assert_eq!(api.terminate_calls, 1);
    }

    #[test]
    fn harness_cleanup_removes_only_its_owned_tree() {
        let root = std::env::temp_dir().join(format!(
            "lcdsirplus-hang-cleanup-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("owned"), b"test").unwrap();
        {
            let _cleanup = HarnessCleanup {
                child: None,
                root: root.clone(),
            };
        }
        assert!(!root.exists());
    }

    #[test]
    fn renamed_cargo_test_binary_is_recognized_case_insensitively() {
        assert!(is_cargo_test_binary(Path::new(
            r"C:\repo\target\release\deps\LCDSirPlus-0ffc5aa055826aa0.exe"
        )));
        assert!(!is_cargo_test_binary(Path::new(
            r"C:\repo\target\release\LCDSirPlus.exe"
        )));
    }

    #[test]
    #[ignore = "starts a visible purpose-built Windows window for bounded native smoke"]
    fn disposable_harness_is_detected_and_cleans_up() {
        smoke().unwrap();
    }

    #[test]
    #[ignore = "runs the pumping native timeout harness through the shipped unrestricted policy"]
    fn disposable_harness_pumps_until_shipped_policy_confirms_timeouts() {
        let mut cleanup = spawn_harness(20).unwrap();
        let pid = cleanup.child.as_ref().unwrap().id();
        detect_harness_with_shipped_policy(pid).unwrap();
        assert!(matches!(
            cleanup.child.as_mut().unwrap().try_wait(),
            Ok(None)
        ));
    }

    #[test]
    #[ignore = "terminates only its directly spawned identity-bound disposable child"]
    fn disposable_harness_action_positive_smoke() {
        assert!(action_smoke(false).is_ok());
    }

    #[test]
    #[ignore = "proves early/wrong-identity release leaves owned child alive until cleanup"]
    fn disposable_harness_action_negative_smoke() {
        assert!(action_smoke(true).is_ok());
    }
}
