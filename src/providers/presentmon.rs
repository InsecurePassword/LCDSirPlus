//! PresentMon console discovery, target selection, owned capture, and CSV statistics.

#![cfg(windows)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::Config;
use crate::history::{mean, percentile_high, Series};
use crate::model::{GameStats, Metric};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STALE_AFTER: Duration = Duration::from_secs(5);
const TARGET_POLL: Duration = Duration::from_secs(1);
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub game: GameStats,
    pub available: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
struct Frame {
    at: SystemTime,
    application: String,
    pid: u32,
    frame_ms: f64,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            at: SystemTime::UNIX_EPOCH,
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
            return Err("PresentMon CSV line exceeds 2 MiB".into());
        }
        let fields = csv_fields(line)?;
        if fields.is_empty() {
            return Ok(None);
        }
        if self.header.is_none() {
            if !fields.iter().any(|v| v.eq_ignore_ascii_case("ProcessID")) {
                return Ok(None);
            }
            self.header = Some(
                fields
                    .iter()
                    .enumerate()
                    .map(|(i, h)| (h.trim().to_ascii_lowercase(), i))
                    .collect(),
            );
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
        let text = get(&[
            "MsBetweenPresents",
            "MsBetweenDisplayChange",
            "DisplayedTime",
        ]);
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
    frames: Series,
    session_start: Option<SystemTime>,
    stutters: i32,
    threshold: f64,
}

impl Stats {
    fn new(threshold: f64) -> Self {
        Self {
            frames: Series::new(360_000),
            session_start: None,
            stutters: 0,
            threshold,
        }
    }

    fn add(&mut self, frame: &Frame) {
        self.session_start.get_or_insert(frame.at);
        self.frames.add(frame.at, frame.frame_ms);
        if frame.frame_ms >= self.threshold {
            self.stutters += 1;
        }
    }

    fn model(&self, now: SystemTime, window: Duration, process: &str) -> GameStats {
        let cutoff = now.checked_sub(window).unwrap_or(SystemTime::UNIX_EPOCH);
        let frame_times = self.frames.values_since(cutoff);
        if frame_times.is_empty() {
            return GameStats::default();
        }
        let recent = self
            .frames
            .values_since(now.checked_sub(Duration::from_secs(1)).unwrap_or(cutoff));
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
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-presentmon".into())
        .spawn(move || run(config, shutdown, tx))
        .expect("PresentMon telemetry thread");
    (rx, thread)
}

fn run(config: Arc<RwLock<Config>>, shutdown: Arc<AtomicBool>, tx: mpsc::Sender<Update>) {
    let mut capture = Capture::default();
    while !shutdown.load(Ordering::Relaxed) {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
        capture.reconcile(&cfg, &tx);
        std::thread::sleep(Duration::from_millis(50));
    }
    capture.stop();
}

#[derive(Default)]
struct Capture {
    child: Option<Child>,
    lines: Option<mpsc::Receiver<String>>,
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
}

impl Capture {
    fn reconcile(&mut self, cfg: &Config, tx: &mpsc::Sender<Update>) {
        if cfg.safe_mode || !cfg.presentmon_enabled || cfg.presentmon_target_mode == "disabled" {
            self.stop();
            self.unavailable(tx, "disabled".into());
            return;
        }
        let now = Instant::now();
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
                            self.unavailable(tx, "PresentMon restart cooldown".into());
                            return;
                        }
                        self.stop();
                        match resolve_executable(&cfg.presentmon_path)
                            .and_then(|path| self.start(&path, target.clone(), cfg, key))
                        {
                            Ok(()) => {
                                self.last_unavailable.clear();
                                let _ = tx.send(Update {
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
                                self.unavailable(tx, e);
                                return;
                            }
                        }
                    }
                }
                Ok(_) | Err(_) => {
                    self.stop();
                    self.unavailable(tx, "waiting for target process".into());
                    return;
                }
            }
        }

        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.stop();
                    self.schedule_retry(now);
                    self.unavailable(tx, format!("PresentMon exited ({status})"));
                    return;
                }
                Err(e) => {
                    self.stop();
                    self.schedule_retry(now);
                    self.unavailable(tx, format!("PresentMon process status failed: {e}"));
                    return;
                }
                Ok(None) => {}
            }
        }
        self.drain(cfg, tx, now);
        if self.last_frame.is_none()
            && self
                .started_at
                .map(|started| now.duration_since(started) > STARTUP_TIMEOUT)
                .unwrap_or(false)
        {
            self.stop();
            self.schedule_retry(now);
            self.unavailable(tx, "PresentMon produced no frames for 45 seconds".into());
            return;
        }
        if let (Some(last), Some(stats)) = (self.last_frame, self.stats.as_ref()) {
            let idle = now.duration_since(last);
            let Some((game, detail)) = stale_projection(
                stats,
                SystemTime::now(),
                cfg.presentmon_window,
                &self.target.name,
                idle,
            ) else {
                return;
            };
            if self.last_unavailable != detail {
                self.last_unavailable = detail.clone();
                let _ = tx.send(Update {
                    game,
                    detail,
                    ..Default::default()
                });
            }
        }
    }

    fn start(
        &mut self,
        path: &Path,
        target: ProcessInfo,
        cfg: &Config,
        key: String,
    ) -> Result<(), String> {
        let mut child = Command::new(path)
            .args([
                "--process_id",
                &target.pid.to_string(),
                "--output_stdout",
                "--no_console_stats",
                "--terminate_on_proc_exit",
                "--session_name",
                &format!("LCDSirPlus-{}", target.pid),
                "--stop_existing_session",
                "--v2_metrics",
                "--exclude_dropped",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start PresentMon {}: {e}", path.display()))?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("PresentMon stdout pipe unavailable".into());
        };
        let (line_tx, line_rx) = mpsc::channel();
        if let Err(e) = std::thread::Builder::new()
            .name("presentmon-output".into())
            .spawn(move || read_output(BufReader::new(stdout), line_tx))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("start PresentMon output reader: {e}"));
        }
        self.child = Some(child);
        self.lines = Some(line_rx);
        self.target = target;
        self.key = key;
        self.parser = Parser::default();
        self.stats = Some(Stats::new(cfg.stutter_threshold_ms));
        self.last_frame = None;
        self.last_publish = None;
        self.started_at = Some(Instant::now());
        self.retry_after = None;
        Ok(())
    }

    fn drain(&mut self, cfg: &Config, tx: &mpsc::Sender<Update>, now: Instant) {
        let Some(lines) = &self.lines else { return };
        while let Ok(line) = lines.try_recv() {
            let Ok(Some(frame)) = self.parser.parse_line(&line, SystemTime::now()) else {
                continue;
            };
            if frame.pid != self.target.pid
                || (!frame.application.is_empty()
                    && !frame.application.eq_ignore_ascii_case(&self.target.name))
            {
                continue;
            }
            self.stats.as_mut().unwrap().add(&frame);
            self.last_frame = Some(now);
            self.last_unavailable.clear();
        }
        if self.last_frame.is_some()
            && self
                .last_publish
                .map(|last| now.duration_since(last) >= cfg.presentmon_interval)
                .unwrap_or(true)
        {
            self.last_publish = Some(now);
            let game = self.stats.as_ref().unwrap().model(
                SystemTime::now(),
                cfg.presentmon_window,
                &self.target.name,
            );
            let _ = tx.send(Update {
                available: game.active,
                detail: self.target.name.clone(),
                game,
            });
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.lines = None;
        self.target = ProcessInfo::default();
        self.key.clear();
        self.stats = None;
        self.last_frame = None;
        self.last_publish = None;
        self.started_at = None;
    }

    fn unavailable(&mut self, tx: &mpsc::Sender<Update>, detail: String) {
        if self.last_unavailable != detail {
            self.last_unavailable = detail.clone();
            let _ = tx.send(Update {
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
    let mut game = stats.model(now, window + STALE_AFTER, process);
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

fn read_output(mut reader: impl BufRead, tx: mpsc::Sender<String>) {
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let Ok(buffer) = reader.fill_buf() else {
            return;
        };
        if buffer.is_empty() {
            if !overflow && !line.is_empty() {
                if let Ok(text) = String::from_utf8(line) {
                    let _ = tx.send(text);
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
        }
        let complete = buffer.get(end.wrapping_sub(1)) == Some(&b'\n');
        reader.consume(end);
        if complete {
            if !overflow {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                if let Ok(text) = String::from_utf8(std::mem::take(&mut line)) {
                    if tx.send(text).is_err() {
                        return;
                    }
                }
            }
            line.clear();
            overflow = false;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
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

fn resolve_executable(setting: &str) -> Result<PathBuf, String> {
    if !setting.trim().is_empty() && !setting.eq_ignore_ascii_case("auto") {
        return validate_executable(Path::new(setting));
    }

    if let Some((candidate, root)) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(colocated_location)
    {
        if let Ok(path) = validate_auto_executable(&candidate, &root) {
            return Ok(path);
        }
    }
    Err("PresentMon console executable not found beside LCDSirPlus".into())
}

fn colocated_location(current: &Path) -> Option<(PathBuf, PathBuf)> {
    current
        .parent()
        .map(|dir| (dir.join("PresentMon.exe"), dir.to_path_buf()))
}

fn validate_auto_executable(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("PresentMon trusted root {}: {e}", root.display()))?;
    let executable = validate_executable(path)?;
    if !executable.starts_with(&root) {
        return Err("PresentMon candidate escapes its trusted root".into());
    }
    Ok(executable)
}

fn validate_executable(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("PresentMon path {}: {e}", path.display()))?;
    if canonical.to_string_lossy().starts_with("\\\\") || !canonical.is_file() {
        return Err("PresentMon must be a local regular file".into());
    }
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
    Ok(canonical)
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
        Ok(ProcessInfo { pid, name, path })
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
                        path: PathBuf::new(),
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
    fn parser_handles_headers_quotes_versions_and_bad_values() {
        let mut parser = Parser::default();
        let at = SystemTime::UNIX_EPOCH;
        assert!(parser
            .parse_line("noise from startup", at)
            .unwrap()
            .is_none());
        assert!(parser
            .parse_line("Application,ProcessID,MsBetweenPresents,Extra", at)
            .unwrap()
            .is_none());
        let frame = parser
            .parse_line("\"game, one.exe\",42,16.5,x", at)
            .unwrap()
            .unwrap();
        assert_eq!(frame.application, "game, one.exe");
        assert_eq!(frame.pid, 42);
        assert_eq!(frame.frame_ms, 16.5);
        assert!(parser.parse_line("game.exe,42,NA,x", at).unwrap().is_none());
        assert!(parser.parse_line("game.exe,42,NaN,x", at).is_err());

        let mut alternate = Parser::default();
        alternate
            .parse_line("ProcessID,DisplayedTime,Application", at)
            .unwrap();
        assert_eq!(
            alternate
                .parse_line("7,20.0,other.exe", at)
                .unwrap()
                .unwrap()
                .frame_ms,
            20.0
        );
    }

    #[test]
    fn stats_compute_lows_stutters_session_and_window() {
        let mut stats = Stats::new(30.0);
        for i in 0..100 {
            stats.add(&Frame {
                at: SystemTime::UNIX_EPOCH + Duration::from_millis(i),
                frame_ms: (i + 1) as f64,
                ..Default::default()
            });
        }
        let game = stats.model(
            SystemTime::UNIX_EPOCH + Duration::from_millis(100),
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
    fn capture_key_resets_session_for_runtime_changes_and_target() {
        let cfg = Config::default();
        let first = ProcessInfo {
            pid: 1,
            name: "one.exe".into(),
            ..Default::default()
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
        let (tx, rx) = mpsc::channel();
        read_output(std::io::Cursor::new(input), tx);
        assert_eq!(rx.into_iter().collect::<Vec<_>>(), vec!["valid"]);
    }

    #[test]
    fn stale_projection_retains_then_expires_metrics() {
        let frame_at = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
        let mut stats = Stats::new(30.0);
        stats.add(&Frame {
            at: frame_at,
            frame_ms: 16.0,
            ..Default::default()
        });

        let (stale, _) = stale_projection(
            &stats,
            frame_at + Duration::from_secs(6),
            Duration::from_secs(5),
            "game.exe",
            Duration::from_secs(6),
        )
        .unwrap();
        assert!(stale.active && stale.fps.valid && stale.fps.stale);

        let (expired, _) = stale_projection(
            &stats,
            frame_at + Duration::from_secs(11),
            Duration::from_secs(5),
            "game.exe",
            Duration::from_secs(11),
        )
        .unwrap();
        assert!(!expired.active && !expired.fps.valid);
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
        let (_updates, thread) = spawn(config, Arc::clone(&shutdown));
        shutdown.store(true, Ordering::Relaxed);
        assert!(thread.join().is_ok());
    }
}
