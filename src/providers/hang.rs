//! Query-only hung-window detection and disposable test harness.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{BOOL, FILETIME, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, EnumWindows, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, RegisterClassW, SendMessageTimeoutW, ShowWindow,
    CW_USEDEFAULT, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SW_SHOW, WM_NULL, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

use crate::config::Config;
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
    fn poll_due(&mut self, cfg: &Config, now: Duration) -> bool {
        if cfg.safe_mode || !cfg.hang_enabled {
            self.last_attempt = None;
            return false;
        }
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_sub(last) < cfg.hang_probe_interval)
        {
            return false;
        }
        self.last_attempt = Some(now);
        true
    }
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
    let mut disabled_published = false;
    while !shutdown.load(Ordering::Relaxed) {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
        if cfg.safe_mode || !cfg.hang_enabled {
            tracker.clear();
            cadence.poll_due(&cfg, started.elapsed());
            if !disabled_published
                && tx
                    .send(Update {
                        targets: Vec::new(),
                        available: false,
                        error: None,
                    })
                    .is_err()
            {
                return;
            }
            disabled_published = true;
            std::thread::sleep(TICK);
            continue;
        }
        disabled_published = false;
        if !cadence.poll_due(&cfg, started.elapsed()) {
            std::thread::sleep(TICK);
            continue;
        }
        let update = match enumerate_and_probe(cfg.hang_probe_timeout, &cfg.hang_ignore, None) {
            Ok(observations) => Update {
                targets: tracker.update(
                    started.elapsed(),
                    observations,
                    cfg.hang_failures,
                    cfg.hang_minimum,
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
        if tx.send(update).is_err() {
            return;
        }
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
    ignored.insert("lcdforge.exe".into());
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
    context.observations.push(Observation {
        target,
        responsive: probe(hwnd, probe_timeout),
    });
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

unsafe fn probe(hwnd: HWND, timeout: Duration) -> bool {
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    SendMessageTimeoutW(
        hwnd,
        WM_NULL,
        WPARAM(0),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
        timeout_ms,
        None,
    )
    .0 != 0
}

fn query_identity(pid: u32) -> Result<(u64, String, String), ()> {
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|_| ())?;
    let result = (|| {
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
    })();
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    result
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

unsafe extern "system" fn harness_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, message, wparam, lparam)
}

pub fn run_harness(duration: Duration) -> i32 {
    match create_harness_window(duration) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            1
        }
    }
}

fn create_harness_window(duration: Duration) -> Result<(), &'static str> {
    let pid = unsafe { GetCurrentProcessId() };
    let class = format!("LCDFORGE_HANG_HARNESS_{pid}\0");
    let title = format!("LCDForge Disposable Hang Harness {pid}\0");
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
    std::thread::sleep(duration);
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

fn smoke() -> Result<HungTarget, String> {
    let source = smoke_source()?;
    let root = std::env::temp_dir().join(format!(
        "lcdforge2-hang-smoke-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir(&root).map_err(|_| "temporary harness directory creation failed")?;
    let harness = root.join("LCDForgeHangHarness.exe");
    if let Err(error) = std::fs::copy(&source, &harness) {
        let _ = std::fs::remove_dir(&root);
        return Err(format!("temporary harness copy failed: {error}"));
    }
    let mut child = match std::process::Command::new(&harness)
        .args(["--hang-test-harness", "--duration-secs", "6"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&harness);
            let _ = std::fs::remove_dir(&root);
            return Err(format!("temporary harness start failed: {error}"));
        }
    };
    let pid = child.id();
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
    let exit_deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < exit_deadline => std::thread::sleep(TICK),
            Ok(None) => return Err("temporary harness did not exit within its bound".into()),
            Err(_) => return Err("temporary harness status unavailable".into()),
        }
    }
    std::fs::remove_file(&harness).map_err(|_| "temporary harness cleanup failed")?;
    std::fs::remove_dir(&root).map_err(|_| "temporary harness directory cleanup failed")?;
    detected.ok_or_else(|| {
        last_error
            .unwrap_or("purpose-built hung window was not confirmed")
            .into()
    })
}

fn smoke_source() -> Result<std::path::PathBuf, String> {
    let current = std::env::current_exe().map_err(|_| "current executable unavailable")?;
    let is_test_binary = current
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("lcdforge-"))
        && current
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("deps");
    if is_test_binary {
        let release_binary = current
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join("lcdforge.exe"))
            .ok_or("release harness executable unavailable")?;
        if release_binary.is_file() {
            return Ok(release_binary);
        }
        return Err("build the release executable before running the ignored harness smoke".into());
    }
    Ok(current)
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
        assert!(cadence.poll_due(&cfg, Duration::ZERO));
        assert!(!cadence.poll_due(&cfg, Duration::from_secs(5)));
        cfg.hang_probe_interval = Duration::from_secs(2);
        assert!(cadence.poll_due(&cfg, Duration::from_secs(5)));
        cfg.safe_mode = true;
        assert!(!cadence.poll_due(&cfg, Duration::from_secs(6)));
        cfg.safe_mode = false;
        assert!(cadence.poll_due(&cfg, Duration::from_secs(6)));
        cfg.hang_enabled = false;
        assert!(!cadence.poll_due(&cfg, Duration::from_secs(7)));
    }

    #[test]
    #[ignore = "starts a visible purpose-built Windows window for bounded native smoke"]
    fn disposable_harness_is_detected_and_cleans_up() {
        assert!(smoke().is_ok());
    }
}
