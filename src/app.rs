//! Application orchestration: config lifecycle, providers, render loop,
//! backend wiring, UI events, and hot reload.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::backends::{Backend, BackendKind, BackendState, Message};
use crate::config::Config;
use crate::hardware_test::{self, TestConfig, Transport};
use crate::input::Event;
use crate::logging::Level;
use crate::model::{DiscordState, Metric, MetricKey, Reading, ReadingsSnapshot, Snapshot};
use crate::providers::{ccd, clock, cpu, memory};
use crate::render::renderer::{OverlayOptions, Renderer, View};
use crate::slots::Manager;
use crate::ui::{Ui, UiEvent};

pub struct RunOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub preview_always: bool,
    pub safe_mode: bool,
}

pub struct ValidateOutcome {
    pub ok: bool,
    pub message: String,
    pub files: Vec<std::path::PathBuf>,
}

/// Load + validate a configuration; used by `--validate-config` and startup.
pub fn validate_config(path: &std::path::Path) -> ValidateOutcome {
    match crate::parser::load(path) {
        Ok(loaded) => ValidateOutcome {
            ok: true,
            message: format!(
                "configuration valid ({} file(s), {} bytes primary)",
                loaded.files.len(),
                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
            ),
            files: loaded.files,
        },
        Err(e) => ValidateOutcome {
            ok: false,
            message: e.to_string(),
            files: Vec::new(),
        },
    }
}

fn default_config_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let dir = exe.parent().unwrap_or(std::path::Path::new("."));
    dir.join("lcdforge.txt")
}

/// Public accessor for the CLI (`--validate-config` default resolution).
pub fn default_config_path_pub() -> std::path::PathBuf {
    default_config_path()
}

fn log_dir() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|base| std::path::PathBuf::from(base).join("LCDForge2"))
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn init_logging(cfg: &Config, diagnostic_dir: Option<&std::path::Path>) {
    let dir = diagnostic_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(log_dir);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("lcdforge.log");
    crate::logging::init(
        Level::parse(&cfg.log_level).unwrap_or(Level::Info),
        Some((path, cfg.log_max_bytes, cfg.log_backups)),
    );
}

/// Passive, read-only HID discovery: enumerates interfaces and reports the
/// G13 candidate or exact rejection reasons. Never acquires the device for
/// I/O beyond attribute queries; never writes a report.
pub fn run_hardware_discover() -> i32 {
    let discovery = crate::backends::hid::discover();
    println!("interfaces scanned: {}", discovery.scanned);
    if discovery.candidates.is_empty() {
        println!("G13 candidate: NONE");
    } else {
        for c in &discovery.candidates {
            println!(
                "G13 candidate: vid={:04x} pid={:04x} usage={:04x}:{:04x} reports={}/{}",
                c.vid, c.pid, c.usage_page, c.usage, c.input_report_length, c.output_report_length
            );
            println!("  path: {}", c.device_path);
        }
    }
    for r in &discovery.rejections {
        println!("rejected: {}", r.reason);
    }
    if discovery.candidates.is_empty() {
        3
    } else {
        0
    }
}

/// Normal application run. Returns a process exit code.
pub fn run(opts: RunOptions) -> i32 {
    let config_path = opts.config_path.clone().unwrap_or_else(default_config_path);
    let outcome = validate_config(&config_path);
    if !outcome.ok {
        eprintln!("configuration error: {}", outcome.message);
        return 2;
    }
    let mut cfg = crate::parser::load(&config_path).expect("checked").config;
    if opts.safe_mode {
        cfg.safe_mode = true;
    }
    init_logging(&cfg, None);
    crate::log_info!("LCDForge {} starting", env!("CARGO_PKG_VERSION"));
    crate::log_info!("configuration: {}", cfg.path);

    // CCD topology (native detection; Process Lasso retired).
    let topology = match (&cfg.ccd_source, &cfg.ccd_cache_processors) {
        _ if cfg.ccd_source == "manual" && cfg.ccd_cache_processors.is_some() => ccd::from_lists(
            cfg.ccd_cache_processors.as_deref().unwrap_or(&[]),
            cfg.ccd_frequency_processors.as_deref().unwrap_or(&[]),
        ),
        _ => ccd::detect(),
    };
    crate::log_info!("CCD topology: {}", topology.detail);

    // Backend.
    let backend_kind = crate::backends::resolve_kind(&cfg.logitech_backend);
    let backend = Backend::spawn(backend_kind, &cfg);
    crate::log_info!("backend: {}", backend_kind.name());

    // UI (tray + preview per mode).
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let show_preview = match cfg.preview_mode.as_str() {
        "always" => true,
        "never" => false,
        _ => opts.preview_always, // auto: preview only when forced/CLI
    };
    let ui = Ui::spawn(cfg.preview_scale.max(1) as u32, show_preview, ui_tx);

    // Providers.
    let mut cpu_provider = cpu::CpuLoadProvider::new();
    let telemetry_runtime = crate::telemetry::spawn(&cfg);
    let discord_runtime = crate::providers::discord::spawn(&cfg);
    let mut telemetry = crate::telemetry::Update::default();
    let mut discord = DiscordState::default();
    let mut renderer = Renderer::new();
    let slots = Manager::new(&cfg, [0; 4]);
    let mut slot_indexes: [usize; 4] = [0; 4];

    // Config watcher state: (mtime, len) of primary.
    let mut watcher = ConfigWatcher::new(&config_path);

    let mut last_telemetry = Instant::now() - cfg.telemetry_interval;
    let mut last_render = Instant::now() - cfg.render_interval;
    let mut snapshot = Snapshot::default();
    let mut running = true;

    while running {
        let now = Instant::now();

        if now.duration_since(last_telemetry) >= cfg.telemetry_interval {
            last_telemetry = now;
            while let Ok(update) = telemetry_runtime.updates.try_recv() {
                telemetry = update;
            }
            while let Ok(update) = discord_runtime.updates.try_recv() {
                discord = update;
            }
            snapshot = build_snapshot(
                &cfg,
                &topology,
                &mut cpu_provider,
                &telemetry,
                &discord,
                slot_indexes,
            );
        }

        if now.duration_since(last_render) >= cfg.render_interval {
            last_render = now;
            let view = View {
                slot_modules: [
                    slots.current(0),
                    slots.current(1),
                    slots.current(2),
                    slots.current(3),
                ],
                ..Default::default()
            };
            let frame = renderer.render(
                &snapshot,
                OverlayOptions {
                    discord_linger: cfg.discord_linger,
                    discord_max_speakers: cfg.discord_max_speakers.max(1) as usize,
                    discord_show_self: cfg.discord_show_self,
                    discord_show_channel: cfg.discord_show_channel,
                },
                &view,
            );
            ui.show_frame(&frame);
            backend.submit(&frame);
        }

        // Backend messages (non-blocking drain).
        while let Ok(message) = backend.msg_rx.try_recv() {
            match message {
                Message::State(state) => match state {
                    BackendState::Connected { kind } => {
                        crate::log_info!("backend connected: {}", kind.name())
                    }
                    BackendState::Disconnected { reason } => {
                        crate::log_warn!("backend disconnected: {}", reason)
                    }
                    BackendState::Discovering => {}
                    BackendState::ShutDown => {}
                },
                Message::Buttons(events) => {
                    for event in events {
                        handle_button(event, &slots, &mut slot_indexes, &cfg);
                    }
                }
            }
        }

        // UI events (non-blocking drain).
        while let Ok(event) = ui_rx.try_recv() {
            match event {
                UiEvent::SlotCycle { slot, backward } => {
                    let delta: i64 = if backward { -1 } else { 1 };
                    slots.cycle(slot, delta);
                    slot_indexes = slots.indexes();
                    crate::log_debug!("preview slot {} cycled", slot);
                }
                UiEvent::Exit => {
                    crate::log_info!("exit requested from tray");
                    running = false;
                }
            }
        }

        // Hot reload.
        if now.duration_since(watcher.last_check) >= cfg.config_refresh {
            watcher.last_check = now;
            if watcher.changed() {
                crate::log_info!("configuration changed on disk; reloading");
                match crate::parser::load(&config_path) {
                    Ok(loaded) => {
                        cfg = loaded.config;
                        if opts.safe_mode {
                            cfg.safe_mode = true;
                        }
                        telemetry_runtime.update_config(&cfg);
                        discord_runtime.update_config(&cfg);
                        slots.apply(&cfg);
                        crate::log_info!("configuration reloaded");
                    }
                    Err(e) => {
                        crate::log_error!(
                            "configuration reload rejected: {} (last valid config stays active)",
                            e
                        );
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    crate::log_info!("shutting down");
    backend.shutdown();
    ui.shutdown();
    0
}

/// Isolated hardware test: owns the device directly for honest transport
/// evidence. Never cycles production slots and never terminates processes.
pub fn run_hardware_test(
    config_path: Option<std::path::PathBuf>,
    backend: BackendKind,
    duration: Duration,
) -> i32 {
    let config_path = config_path.unwrap_or_else(default_config_path);
    let outcome = validate_config(&config_path);
    if !outcome.ok {
        eprintln!("configuration error: {}", outcome.message);
        return 2;
    }
    let cfg = crate::parser::load(&config_path).expect("checked").config;
    init_logging(&cfg, None);
    crate::log_info!(
        "hardware test: backend={} duration={:?}",
        backend.name(),
        duration
    );

    // Build the live dashboard frame for STEP 10.
    let mut cpu_provider = cpu::CpuLoadProvider::new();
    let topology = ccd::detect();
    let snapshot = build_snapshot(
        &cfg,
        &topology,
        &mut cpu_provider,
        &crate::telemetry::Update::default(),
        &DiscordState::default(),
        [0; 4],
    );
    let dashboard = crate::render::renderer::Renderer::new().render(
        &snapshot,
        OverlayOptions::default(),
        &View {
            slot_modules: [
                "PROVIDER_STATUS".into(),
                "PROVIDER_STATUS".into(),
                "PROVIDER_STATUS".into(),
                "PROVIDER_STATUS".into(),
            ],
            ..Default::default()
        },
    );

    match backend {
        BackendKind::Hid => {
            let (mut device, discovery) = match crate::backends::hid::HidDevice::open() {
                Ok(ok) => ok,
                Err(reason) => {
                    crate::log_error!("hardware test: {}", reason);
                    eprintln!("hardware test: {}", reason);
                    return 3;
                }
            };
            crate::log_info!(
                "hardware test: selected {} interfaces scanned={}",
                discovery.candidates[0].device_path,
                discovery.scanned
            );
            let test = TestConfig {
                duration,
                backend_name: "hid",
                dashboard: Some(dashboard),
                ..Default::default()
            };
            let (results, buttons) = hardware_test::run(&Transport::DirectHid(&device), &test);
            device.close();
            report_test_results(&results, &buttons)
        }
        BackendKind::Virtual => {
            let test = TestConfig {
                duration,
                backend_name: "virtual",
                dashboard: Some(dashboard),
                ..Default::default()
            };
            let (results, buttons) = hardware_test::run(&Transport::Virtual, &test);
            report_test_results(&results, &buttons)
        }
    }
}

fn report_test_results(
    results: &[hardware_test::StepResult],
    buttons: &[hardware_test::ButtonObservation],
) -> i32 {
    let mut all_ok = true;
    for step in results {
        match (&step.error, step.submissions) {
            (None, n) if n > 0 => {
                crate::log_info!("STEP {:02} {} submitted x{}", step.step, step.name, n);
                println!("STEP {:02} {} OK ({} submissions)", step.step, step.name, n);
            }
            _ => {
                all_ok = false;
                let reason = step
                    .error
                    .clone()
                    .unwrap_or_else(|| "no submissions".into());
                crate::log_error!("STEP {:02} {} FAILED: {}", step.step, step.name, reason);
                println!("STEP {:02} {} FAILED: {}", step.step, step.name, reason);
            }
        }
    }
    if buttons.is_empty() {
        crate::log_warn!(
            "hardware test: zero button events observed (physical presses unconfirmed)"
        );
        println!("buttons: zero events observed");
    } else {
        for b in buttons {
            crate::log_info!(
                "button {} {}",
                b.index + 1,
                if b.down { "down" } else { "up" }
            );
        }
        println!("buttons: {} transitions observed", buttons.len());
    }
    println!(
        "NOTE: software transport results are NOT physical display confirmation; a human observer must verify the G13."
    );
    if all_ok {
        0
    } else {
        4
    }
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    cfg: &Config,
    topology: &ccd::CcdTopology,
    cpu_provider: &mut cpu::CpuLoadProvider,
    telemetry: &crate::telemetry::Update,
    discord: &DiscordState,
    _slot_indexes: [usize; 4],
) -> Snapshot {
    let now = SystemTime::now();
    let clock = clock::read();
    let per_lp = cpu_provider.update();

    let (cpu_dual, cache_load, freq_load) = if topology.dual {
        (
            true,
            cpu::aggregate(&per_lp, topology.cache_mask),
            cpu::aggregate(&per_lp, topology.freq_mask),
        )
    } else {
        (false, cpu::aggregate(&per_lp, topology.cache_mask), 0.0)
    };
    let total_load = if per_lp.is_empty() {
        0.0
    } else {
        per_lp.iter().sum::<f64>() / per_lp.len() as f64
    };
    let mem = memory::memory_load_percent();

    let mut snapshot = Snapshot {
        now: Some(now),
        cpu_dual,
        date_text: clock.date,
        date_short_text: clock.date_short,
        time_text: clock.time,
        cpu_cache_load: Metric::valid(cache_load, now),
        cpu_freq_load: Metric::valid(freq_load, now),
        headset: telemetry.headset.clone(),
        controller: telemetry.controller.clone(),
        discord: discord.clone(),
        hung: telemetry.hung.clone(),
        ..Default::default()
    };

    // Canonical readings for the fixed bars.
    let mut readings = ReadingsSnapshot::default();
    if total_load.is_finite() {
        readings.metrics.push(Reading::current_percent(
            MetricKey::CPUUtilization,
            total_load,
            "cpu-system",
            now,
        ));
    }
    if mem.is_finite() {
        readings.metrics.push(Reading::current_percent(
            MetricKey::RAMUtilization,
            mem,
            "physical-memory",
            now,
        ));
    }
    readings
        .metrics
        .extend(telemetry.gpu_readings.iter().cloned());
    snapshot.readings = readings;

    apply_telemetry(&mut snapshot, telemetry, now);

    if cfg.safe_mode {
        snapshot.providers.insert("safe-mode".into(), true);
    }
    for (name, ok) in &telemetry.providers {
        snapshot.providers.insert(name.clone(), *ok);
    }
    snapshot.providers.insert(
        "Discord".into(),
        snapshot.discord.connected && snapshot.discord.authenticated,
    );
    snapshot
}

fn apply_telemetry(snapshot: &mut Snapshot, telemetry: &crate::telemetry::Update, now: SystemTime) {
    snapshot.cpu_temp = telemetry.cpu_temp;
    snapshot.gpu_temp = telemetry.gpu_temp;
    snapshot.game = telemetry.game.clone();

    // A sampled idle interval is valid telemetry, not unavailable data.
    snapshot.network_in = Metric::default();
    snapshot.network_out = Metric::default();
    if let Some(net) = telemetry.net {
        snapshot.network_in = Metric::valid(net.in_bps, now);
        snapshot.network_out = Metric::valid(net.out_bps, now);
    }

    // Audio endpoint state.
    snapshot.audio_volume = Metric::default();
    snapshot.microphone_known = false;
    snapshot.microphone_muted = false;
    if let Some(audio) = &telemetry.audio {
        snapshot.audio_volume = Metric::valid(audio.volume_percent, now);
        snapshot.microphone_known = audio.mic_known;
        snapshot.microphone_muted = audio.mic_muted;
    }
}

fn handle_button(event: Event, slots: &Manager, indexes: &mut [usize; 4], cfg: &Config) {
    if event.canceled {
        crate::log_debug!("button {} canceled (device loss)", event.index + 1);
        return;
    }
    if !event.down {
        return; // release edges: long-press logic arrives with Phase 4
    }
    if cfg.safe_mode {
        crate::log_info!(
            "safe-mode: button {} press observed, no slot cycling",
            event.index + 1
        );
        return;
    }
    let module = slots.cycle(event.index, 1);
    *indexes = slots.indexes();
    crate::log_debug!("button {} -> slot module {}", event.index + 1, module);
}

struct ConfigWatcher {
    path: std::path::PathBuf,
    last_mtime: Option<SystemTime>,
    last_len: u64,
    pub last_check: Instant,
}

impl ConfigWatcher {
    fn new(path: &std::path::Path) -> Self {
        let (mtime, len) = stat(path);
        ConfigWatcher {
            path: path.to_path_buf(),
            last_mtime: mtime,
            last_len: len,
            last_check: Instant::now(),
        }
    }

    fn changed(&mut self) -> bool {
        let (mtime, len) = stat(&self.path);
        if mtime != self.last_mtime || len != self.last_len {
            self.last_mtime = mtime;
            self.last_len = len;
            return true;
        }
        false
    }
}

fn stat(path: &std::path::Path) -> (Option<SystemTime>, u64) {
    match std::fs::metadata(path) {
        Ok(meta) => (meta.modified().ok(), meta.len()),
        Err(_) => (None, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_projects_temperature_states_and_idle_network() {
        let now = SystemTime::now();
        let mut update = crate::telemetry::Update {
            cpu_temp: Metric::valid(67.0, now),
            gpu_temp: Metric::valid(74.0, now),
            net: Some(crate::providers::netif::NetThroughput::default()),
            ..Default::default()
        };
        update.gpu_temp.stale = true;
        let mut snapshot = Snapshot::default();
        apply_telemetry(&mut snapshot, &update, now);
        assert!(snapshot.cpu_temp.valid && !snapshot.cpu_temp.stale);
        assert!(snapshot.gpu_temp.valid && snapshot.gpu_temp.stale);
        assert!(snapshot.network_in.valid && snapshot.network_out.valid);

        apply_telemetry(&mut snapshot, &crate::telemetry::Update::default(), now);
        assert!(!snapshot.cpu_temp.valid);
        assert!(!snapshot.gpu_temp.valid);
        assert!(!snapshot.network_in.valid);
        assert!(!snapshot.audio_volume.valid);
    }
}
