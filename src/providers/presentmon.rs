//! PresentMon console discovery, target selection, owned capture, and CSV statistics.

#![cfg(windows)]

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND, HANDLE, MAX_PATH,
};
use windows::Win32::System::Com::CoCreateGuid;
use windows::Win32::System::Diagnostics::Etw::{
    ControlTraceW, CONTROLTRACE_HANDLE, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_PROPERTIES,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::Config;
use crate::history::{mean, percentile_high};
use crate::model::{GameStats, Metric};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STALE_AFTER: Duration = Duration::from_secs(5);
const TARGET_POLL: Duration = Duration::from_secs(1);
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
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
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            at: SystemTime::UNIX_EPOCH,
            observed_at: Instant::now(),
            application: String::new(),
            pid: 0,
            frame_ms: 0.0,
        }
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
        Ok(Some(Frame {
            at,
            observed_at: Instant::now(),
            application: get(&["Application"]).to_string(),
            pid,
            frame_ms,
        }))
    }
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
    let target = select_target(&config)
        .map_err(|error| format!("PresentMon final target selection failed: {error}"))?;
    if shutdown.load(Ordering::Acquire)
        || generation != expected_generation
        || live_generation.load(Ordering::SeqCst) != expected_generation
        || config != *expected_config
        || target.pid != expected_target.pid
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
            match select_target(cfg) {
                Ok(target) if target.pid != 0 => {
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
                    let target = self.target.name.clone();
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
        if let Err(error) = self.drain(cfg, generation, tx, now) {
            let stderr = self.stop();
            self.schedule_retry(now);
            self.failure(tx, generation, with_diagnostic(error, stderr));
            return;
        }
        if self.last_frame.is_none()
            && self
                .started_at
                .map(|started| now.duration_since(started) > STARTUP_TIMEOUT)
                .unwrap_or(false)
        {
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
        let session_name = new_session_name(target.pid)?;
        let args = presentmon_args(target.pid, &session_name);
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
        self.stats = Some(Stats::new(cfg.stutter_threshold_ms, cfg.presentmon_window));
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
        let Some(lines) = &self.lines else {
            return Ok(());
        };
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
            if frame.pid != self.target.pid
                || (!frame.application.is_empty()
                    && !frame.application.eq_ignore_ascii_case(&self.target.name))
            {
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
    format!(
        "{}\0{}\0{}\0{}\0{}",
        cfg.presentmon_path,
        target.pid,
        cfg.presentmon_interval.as_millis(),
        cfg.presentmon_window.as_millis(),
        cfg.stutter_threshold_ms
    )
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

fn presentmon_args(pid: u32, session_name: &str) -> [OsString; 9] {
    [
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
    if excluded(&target.name, &cfg.presentmon_exclude) {
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
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("open process {pid}: {e}"))?;
        let mut path = vec![0u16; 32_768];
        let mut len = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.map_err(|e| format!("query process {pid}: {e}"))?;
        path.truncate(len as usize);
        let path = PathBuf::from(String::from_utf16_lossy(&path));
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_string();
        Ok(ProcessInfo { pid, name })
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
        let mut found = ProcessInfo::default();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(MAX_PATH as usize);
                let process_name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if process_name.eq_ignore_ascii_case(name) {
                    found = process_info(entry.th32ProcessID).unwrap_or(ProcessInfo {
                        pid: entry.th32ProcessID,
                        name: process_name,
                    });
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        #[rustfmt::skip]
        assert_eq!(
            presentmon_args(4242, &first),
            ["--process_id", "4242", "--output_stdout", "--no_console_stats", "--terminate_on_proc_exit", "--session_name", &first, "--v2_metrics", "--exclude_dropped"].map(OsString::from)
        );
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
        let cfg = Config::default();
        let first = ProcessInfo {
            pid: 1,
            name: "one.exe".into(),
        };
        let second = ProcessInfo {
            pid: 2,
            ..first.clone()
        };
        assert_ne!(capture_key(&cfg, &first), capture_key(&cfg, &second));
        let mut changed = cfg.clone();
        changed.stutter_threshold_ms += 1.0;
        assert_ne!(capture_key(&cfg, &first), capture_key(&changed, &first));
    }

    #[test]
    fn target_exclusions_are_case_insensitive_and_exact() {
        let exclusions = vec!["DWM.EXE".to_string(), "lcdsirplus.exe".to_string()];
        assert!(excluded("dwm.exe", &exclusions));
        assert!(!excluded("mydwm.exe", &exclusions));
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
            name: "Palworld-Win64-Shipping.exe".into(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(600)));
        let (updates, _rx) = mpsc::channel();
        for _ in 0..(BURST_FRAMES + 1).div_ceil(MAX_LINES_PER_TICK) {
            capture
                .drain(&Config::default(), 1, &updates, Instant::now())
                .unwrap();
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
            name: "game.exe".into(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(60)));
        let (updates, _rx) = mpsc::channel();
        capture
            .drain(&Config::default(), 1, &updates, Instant::now())
            .unwrap();
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
            .drain(&Config::default(), 1, &updates, Instant::now())
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
            name: "game.exe".into(),
        };
        capture.stats = Some(Stats::new(30.0, Duration::from_secs(60)));
        let (updates, _rx) = mpsc::channel();
        let error = capture
            .drain(&Config::default(), 1, &updates, Instant::now())
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
        let cfg = Config::default();
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
            name: "game.exe".into(),
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
            name: "game.exe".into(),
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
                    name: "next.exe".into(),
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
            name: "game.exe".into(),
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
