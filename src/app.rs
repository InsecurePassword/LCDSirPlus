//! Application orchestration: config lifecycle, providers, render loop,
//! backend wiring, UI events, and hot reload.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::backends::{Backend, BackendKind, BackendState, Message};
use crate::config::Config;
use crate::hardware_test::{self, TestConfig, Transport};
use crate::logging::Level;
use crate::model::{
    BottleneckDetector, BottleneckInputs, BottleneckThresholds, DiscordState, Freshness, Metric,
    MetricKey, Reading, ReadingsSnapshot, Snapshot,
};
use crate::providers::{ccd, clock, cpu, hang, memory};
use crate::render::renderer::{OverlayOptions, Renderer, View};
use crate::slots::Manager;
use crate::ui::{Ui, UiEvent};

pub struct RunOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub preview_always: bool,
    pub safe_mode: bool,
    pub diagnostic_dir: Option<std::path::PathBuf>,
}

pub struct ValidateOutcome {
    pub ok: bool,
    pub message: String,
    pub loaded: Option<crate::parser::LoadedConfig>,
}

/// Load + validate a configuration; used by `--validate-config` and startup.
pub fn validate_config(path: &std::path::Path) -> ValidateOutcome {
    match crate::parser::load(path) {
        Ok(loaded) => {
            let message = format!(
                "configuration valid ({} file(s), {} bytes primary)",
                loaded.files.len(),
                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
            );
            ValidateOutcome {
                ok: true,
                message,
                loaded: Some(loaded),
            }
        }
        Err(e) => ValidateOutcome {
            ok: false,
            message: e.to_string(),
            loaded: None,
        },
    }
}

pub fn log_dir() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|base| std::path::PathBuf::from(base).join("LCDSirPlus"))
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn init_logging(cfg: &Config, diagnostic_dir: Option<&std::path::Path>) -> Result<(), String> {
    let dir = diagnostic_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(log_dir);
    std::fs::create_dir_all(&dir).map_err(|_| "log directory could not be created".to_string())?;
    let path = dir.join("lcdsirplus.log");
    crate::logging::init(
        Level::parse(&cfg.log_level).unwrap_or(Level::Info),
        Some((path, cfg.log_max_bytes, cfg.log_backups)),
    )
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
            println!("{}", hardware_candidate_summary(c));
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

fn hardware_candidate_summary(c: &crate::backends::hid::CandidateInfo) -> String {
    format!(
        "G13 candidate: vid={:04x} pid={:04x} usage={:04x}:{:04x} reports={}/{}",
        c.vid, c.pid, c.usage_page, c.usage, c.input_report_length, c.output_report_length
    )
}

/// Normal application run. Returns a process exit code or a startup failure.
pub fn run(opts: RunOptions) -> Result<i32, (i32, String)> {
    let resolved = crate::runtime::resolve_config(opts.config_path.clone())
        .map_err(|error| (2, format!("configuration resolution failed: {error}")))?;
    let config_path = resolved.path;
    let outcome = validate_config(&config_path);
    let loaded = outcome
        .loaded
        .ok_or_else(|| (2, format!("configuration error: {}", outcome.message)))?;
    let watched_files = loaded.files.clone();
    let mut cfg = loaded.config;
    if opts.safe_mode {
        cfg.safe_mode = true;
    }
    if let Err(error) = init_logging(&cfg, opts.diagnostic_dir.as_deref()) {
        return Err((1, format!("logging initialization failed: {error}")));
    }
    crate::log_info!("LCDSirPlus {} starting", env!("CARGO_PKG_VERSION"));
    crate::log_info!("configuration: {}", cfg.path);
    let startup_executable = crate::runtime::canonical_executable().ok();
    let mut startup_synced = None;
    sync_startup_if_needed(
        &cfg,
        resolved.installed,
        startup_executable.as_deref(),
        &mut startup_synced,
    );

    // CCD topology (native detection; Process Lasso retired).
    let mut topology = resolve_ccd_topology(&cfg);
    crate::log_info!("CCD topology: {}", topology.detail);

    // Backend.
    let backend_kind = crate::backends::resolve_kind(&cfg.logitech_backend);
    let mut backend = Backend::spawn(backend_kind, &cfg);
    crate::log_info!("backend: {}", backend_kind.name());

    // UI (tray + preview per mode).
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let show_preview =
        opts.preview_always || (!cfg.start_minimized && cfg.preview_mode.as_str() == "always");
    let ui = Ui::spawn(cfg.preview_scale.max(1) as u32, show_preview, ui_tx);

    // Providers.
    let mut cpu_provider = cpu::CpuLoadProvider::new();
    let telemetry_runtime = crate::telemetry::spawn(&cfg);
    let discord_runtime = crate::providers::discord::spawn(&cfg);
    let mut telemetry = crate::telemetry::Update {
        generation: telemetry_runtime.generation(),
        ..Default::default()
    };
    let mut discord = DiscordState::default();
    let mut renderer = Renderer::new();
    let mut slots = Manager::new(&cfg, [0; 4]);
    let mut hang_hold = hang::HoldState::default();
    hang_hold.set_owner(proc_hang_owner(&slots));
    let mut alerts = crate::alerts::Manager::default();
    let mut bottleneck = BottleneckDetector::default();

    let mut watcher = ConfigWatcher::new(&watched_files);

    let mut last_telemetry = Instant::now() - cfg.telemetry_interval;
    let mut last_render = Instant::now() - cfg.render_interval;
    let mut snapshot = Snapshot::default();
    let mut running = true;
    let mut backend_state = BackendState::Discovering;
    let process_epoch = Instant::now();

    while running {
        let now = Instant::now();

        while let Ok(update) = telemetry_runtime.updates.try_recv() {
            if update.generation == telemetry_runtime.generation() {
                telemetry = update;
            }
        }
        while let Ok(update) = discord_runtime.updates.try_recv() {
            discord = update;
        }

        // Reload before input so a same-loop hold cannot use stale action policy.
        if now.duration_since(watcher.last_check) >= cfg.config_refresh {
            watcher.last_check = now;
            if watcher.changed() {
                crate::log_info!("configuration changed on disk; reloading");
                match crate::parser::load(&config_path) {
                    Ok(loaded) => {
                        if let Some(audit) = hang_hold.cancel_reload() {
                            log_hang_audit(&audit);
                        }
                        let backend_changed = backend_config_changed(&cfg, &loaded.config);
                        let topology_changed = ccd_config_changed(&cfg, &loaded.config);
                        let files = loaded.files.clone();
                        cfg = loaded.config;
                        if opts.safe_mode {
                            cfg.safe_mode = true;
                        }
                        alerts.clear_disabled_categories(&cfg);
                        if topology_changed {
                            topology = resolve_ccd_topology(&cfg);
                            crate::log_info!("CCD topology reloaded: {}", topology.detail);
                        }
                        let generation = telemetry_runtime.update_config(&cfg);
                        invalidate_telemetry_publication(&mut telemetry, &mut snapshot, generation);
                        bottleneck = BottleneckDetector::default();
                        renderer = Renderer::new();
                        discord_runtime.update_config(&cfg);
                        slots.apply(&cfg);
                        if let Some(audit) = hang_hold.set_owner(proc_hang_owner(&slots)) {
                            log_hang_audit(&audit);
                        }
                        watcher.replace(&files);
                        if backend_changed {
                            backend.reconfigure(
                                crate::backends::resolve_kind(&cfg.logitech_backend),
                                &cfg,
                            );
                            backend_state = BackendState::Discovering;
                        }
                        sync_startup_if_needed(
                            &cfg,
                            resolved.installed,
                            startup_executable.as_deref(),
                            &mut startup_synced,
                        );
                        if !opts.preview_always {
                            apply_preview_policy(&ui, &cfg, &backend_state);
                        }
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

        if now.duration_since(last_telemetry) >= cfg.telemetry_interval {
            last_telemetry = now;
            snapshot = build_snapshot(
                &cfg,
                &topology,
                &mut cpu_provider,
                &telemetry,
                &discord,
                &mut bottleneck,
            );
        }
        let telemetry_generation = telemetry_runtime.generation();
        let (published_hung, hang_provider_available) =
            current_hang_publication(&telemetry, telemetry_generation);
        snapshot.hung = if cfg.safe_mode || !cfg.hang_enabled || !hang_provider_available {
            Vec::new()
        } else {
            published_hung.to_vec()
        };
        if telemetry.generation != telemetry_generation {
            snapshot.providers.remove("Hung window detector");
        }

        if let Some(audit) = hang_hold.set_owner(proc_hang_owner(&slots)) {
            log_hang_audit(&audit);
        }
        if let Some(audit) = hang_hold.reconcile(&cfg, published_hung, hang_provider_available) {
            log_hang_audit(&audit);
        }
        alerts.evaluate(&mut snapshot, &cfg, now);

        if now.duration_since(last_render) >= cfg.render_interval {
            last_render = now;
            let view = View {
                slot_modules: effective_slot_modules(&slots, &snapshot, &cfg),
                hung_index: hang_hold.hung_index,
                hung_detail: hang_hold.hung_detail,
                hung_hold: hang_hold.progress(now, &cfg),
                ..Default::default()
            };
            let frame = renderer.render(
                &snapshot,
                OverlayOptions {
                    main_display: cfg.main_display,
                    discord_linger: cfg.discord_linger,
                    discord_max_speakers: cfg.discord_max_speakers.max(1) as usize,
                    discord_show_self: cfg.discord_show_self,
                    discord_show_channel: cfg.discord_show_channel,
                    disk_graph_ceiling_mbps: cfg.disk_graph_ceiling_mbps,
                    fps_graph_ceiling: cfg.fps_graph_ceiling,
                    cpu_temp_max_c: cfg.cpu_temp_max_c,
                    gpu_temp_max_c: cfg.gpu_temp_max_c,
                    warning: temperature_pane_warning(&cfg),
                    warning_phase: (process_epoch.elapsed().as_millis() / 100) % 2 == 1,
                    ..network_overlay_options(&cfg)
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
                        crate::log_info!("backend connected: {}", kind.name());
                        backend_state = BackendState::Connected { kind };
                        if !opts.preview_always
                            && cfg.preview_mode == "auto"
                            && !cfg.start_minimized
                        {
                            ui.set_preview_auto_visible(!matches!(
                                kind,
                                BackendKind::Hid | BackendKind::Sdk
                            ));
                        }
                    }
                    BackendState::Disconnected { reason } => {
                        crate::log_warn!("backend disconnected: {}", reason);
                        backend_state = BackendState::Disconnected { reason };
                        if !opts.preview_always
                            && cfg.preview_mode == "auto"
                            && !cfg.start_minimized
                        {
                            ui.set_preview_auto_visible(true);
                        }
                    }
                    BackendState::Discovering => backend_state = BackendState::Discovering,
                    BackendState::ShutDown => backend_state = BackendState::ShutDown,
                },
                Message::Buttons(events) => {
                    for event in events {
                        let command =
                            hang_hold.event(event, &cfg, published_hung, hang_provider_available);
                        apply_hold_command(command, &mut slots, &cfg, &mut alerts, &mut snapshot);
                        if let Some(audit) = hang_hold.set_owner(proc_hang_owner(&slots)) {
                            log_hang_audit(&audit);
                        }
                    }
                }
            }
        }

        // UI events (non-blocking drain).
        while let Ok(event) = ui_rx.try_recv() {
            match event {
                UiEvent::SlotCycle { slot, backward } => {
                    if slot == 3 && alerts.acknowledge_highest(&mut snapshot) {
                        crate::log_info!("acknowledged highest critical alert");
                        continue;
                    }
                    let delta: i64 = if backward { -1 } else { 1 };
                    slots.cycle(slot, delta);
                    if let Some(audit) = hang_hold.set_owner(proc_hang_owner(&slots)) {
                        log_hang_audit(&audit);
                    }
                    crate::log_debug!("preview slot {} cycled", slot);
                }
                UiEvent::Exit => {
                    crate::log_info!("exit requested from tray");
                    running = false;
                }
            }
        }

        if let Some(audit) = hang_hold.set_owner(proc_hang_owner(&slots)) {
            log_hang_audit(&audit);
        }
        if let Some(audit) = hang_hold.reconcile(&cfg, published_hung, hang_provider_available) {
            log_hang_audit(&audit);
        }
        let command = hang_hold.tick(now, &cfg, published_hung, hang_provider_available);
        apply_hold_command(command, &mut slots, &cfg, &mut alerts, &mut snapshot);

        std::thread::sleep(Duration::from_millis(10));
    }

    crate::log_info!("shutting down");
    backend.shutdown();
    ui.shutdown();
    Ok(0)
}

fn proc_hang_owner(slots: &Manager) -> Option<usize> {
    (0..4).find(|&slot| slots.current(slot).eq_ignore_ascii_case("PROC_HANG"))
}

fn effective_slot_modules(slots: &Manager, snapshot: &Snapshot, cfg: &Config) -> [String; 4] {
    let provider_current = !cfg.safe_mode
        && cfg.presentmon_enabled
        && cfg.presentmon_target_mode != "disabled"
        && snapshot.providers.get("presentmon") == Some(&true);
    let metric_current = |metric: crate::model::Metric| {
        provider_current && metric.valid && !metric.stale && metric.value.is_finite()
    };
    let presentmon_available = |module: &str| match module.to_ascii_uppercase().as_str() {
        "FPS_CURRENT" | "FPS_GRAPH" => Some(metric_current(snapshot.game.fps)),
        "FPS_1LOW" => Some(metric_current(snapshot.game.one_percent)),
        "FPS_01LOW" => Some(metric_current(snapshot.game.point_one_low)),
        "FRAME_TIME" => Some(metric_current(snapshot.game.frame_time_ms)),
        "SESSION_TIME" | "SESSION_SUMMARY" => {
            Some(provider_current && snapshot.game.active && snapshot.game.session_start.is_some())
        }
        "GAME_NAME" => Some(
            provider_current
                && snapshot.game.active
                && snapshot.game.session_start.is_some()
                && !snapshot.game.game_name.is_empty(),
        ),
        _ => None,
    };
    std::array::from_fn(|slot| {
        let selected = slots.current(slot);
        let inactive_dynamic = (selected.eq_ignore_ascii_case("PROC_HANG")
            && snapshot.hung.is_empty())
            || (selected.eq_ignore_ascii_case("BOTTLENECK")
                && snapshot.bottleneck.state == crate::model::BottleneckState::None
                && snapshot.bottleneck.freshness == Freshness::Current);
        let inactive_presentmon = cfg.presentmon_deferred
            && presentmon_available(&selected).is_some_and(|available| !available);
        if !inactive_dynamic && !inactive_presentmon {
            return selected;
        }

        let mut excluded = vec!["PROC_HANG".to_string(), "BOTTLENECK".to_string(), selected];
        loop {
            let excluded_refs: Vec<_> = excluded.iter().map(String::as_str).collect();
            let Some(candidate) = slots.next_except(slot, &excluded_refs) else {
                return "CLEAR".into();
            };
            if !cfg.presentmon_deferred
                || presentmon_available(&candidate).is_none_or(|available| available)
            {
                return candidate;
            }
            excluded.push(candidate);
        }
    })
}

fn resolve_ccd_topology(cfg: &Config) -> ccd::CcdTopology {
    match (&cfg.ccd_source, &cfg.ccd_cache_processors) {
        _ if cfg.ccd_source == "manual" && cfg.ccd_cache_processors.is_some() => ccd::from_lists(
            cfg.ccd_cache_processors.as_deref().unwrap_or(&[]),
            cfg.ccd_frequency_processors.as_deref().unwrap_or(&[]),
        ),
        _ => ccd::detect(),
    }
}

fn ccd_config_changed(old: &Config, new: &Config) -> bool {
    old.ccd_source != new.ccd_source
        || old.ccd_cache_processors != new.ccd_cache_processors
        || old.ccd_frequency_processors != new.ccd_frequency_processors
}

fn preview_for_backend(cfg: &Config, state: &BackendState) -> bool {
    match cfg.preview_mode.as_str() {
        "always" => true,
        "never" => false,
        _ => !matches!(
            state,
            BackendState::Connected {
                kind: BackendKind::Hid | BackendKind::Sdk
            }
        ),
    }
}

fn temperature_pane_warning(cfg: &Config) -> bool {
    cfg.temperature_warning_enabled && cfg.warning
}

fn network_overlay_options(cfg: &Config) -> OverlayOptions {
    OverlayOptions {
        network_graph_ceiling_download_mbps: cfg.network_graph_ceiling_download_mbps,
        network_graph_ceiling_upload_mbps: cfg.network_graph_ceiling_upload_mbps,
        ..Default::default()
    }
}

fn backend_config_changed(old: &Config, new: &Config) -> bool {
    old.logitech_backend != new.logitech_backend
        || old.logitech_reconnect != new.logitech_reconnect
        || old.logitech_reconnect_max != new.logitech_reconnect_max
        || old.logitech_button_poll != new.logitech_button_poll
        || old.logitech_button_debounce != new.logitech_button_debounce
        || old.logitech_friendly_name != new.logitech_friendly_name
        || old.logitech_orientation != new.logitech_orientation
        || old.logitech_invert != new.logitech_invert
}

fn apply_preview_policy(ui: &Ui, cfg: &Config, state: &BackendState) {
    match cfg.preview_mode.as_str() {
        "always" if !cfg.start_minimized => ui.set_preview_visible(true),
        "never" => ui.set_preview_visible(false),
        "auto" if !cfg.start_minimized => {
            ui.set_preview_auto_visible(preview_for_backend(cfg, state))
        }
        _ => {}
    }
}

fn sync_startup_if_needed(
    cfg: &Config,
    installed: bool,
    executable: Option<&std::path::Path>,
    synced: &mut Option<bool>,
) {
    sync_startup_if_needed_with(
        cfg,
        installed,
        executable,
        synced,
        crate::runtime::sync_startup,
    );
}

fn sync_startup_if_needed_with<F>(
    cfg: &Config,
    installed: bool,
    executable: Option<&std::path::Path>,
    synced: &mut Option<bool>,
    sync: F,
) where
    F: FnOnce(bool, &std::path::Path) -> Result<(), String>,
{
    if installed || cfg.safe_mode || *synced == Some(cfg.start_at_login) {
        return;
    }
    let Some(executable) = executable else {
        crate::log_warn!(
            "start-at-login synchronization skipped: canonical executable unavailable"
        );
        return;
    };
    match sync(cfg.start_at_login, executable) {
        Ok(()) => *synced = Some(cfg.start_at_login),
        Err(error) => crate::log_warn!(
            "start-at-login synchronization failed enabled={}: {}",
            cfg.start_at_login,
            error
        ),
    }
}

/// Isolated hardware test: owns the device directly for honest transport
/// evidence. Never cycles production slots and never terminates processes.
pub fn run_hardware_test(
    config_path: Option<std::path::PathBuf>,
    backend: BackendKind,
    duration: Duration,
    diagnostic_dir: Option<std::path::PathBuf>,
) -> i32 {
    let config_path = match crate::runtime::resolve_config(config_path) {
        Ok(resolved) => resolved.path,
        Err(error) => {
            eprintln!("configuration resolution failed: {error}");
            return 2;
        }
    };
    let outcome = validate_config(&config_path);
    let Some(loaded) = outcome.loaded else {
        eprintln!("configuration error: {}", outcome.message);
        return 2;
    };
    let cfg = loaded.config;
    if let Err(error) = init_logging(&cfg, diagnostic_dir.as_deref()) {
        eprintln!("logging initialization failed: {error}");
        return 1;
    }
    crate::log_info!(
        "hardware test: backend={} duration={:?}",
        backend.name(),
        duration
    );

    // Build the live dashboard frame for STEP 10.
    let mut cpu_provider = cpu::CpuLoadProvider::new();
    let topology = ccd::detect();
    let mut bottleneck = BottleneckDetector::default();
    let snapshot = build_snapshot(
        &cfg,
        &topology,
        &mut cpu_provider,
        &crate::telemetry::Update::default(),
        &DiscordState::default(),
        &mut bottleneck,
    );
    let dashboard = crate::render::renderer::Renderer::new().render(
        &snapshot,
        OverlayOptions {
            main_display: cfg.main_display,
            cpu_temp_max_c: cfg.cpu_temp_max_c,
            gpu_temp_max_c: cfg.gpu_temp_max_c,
            warning: temperature_pane_warning(&cfg),
            ..network_overlay_options(&cfg)
        },
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

    let backend = if backend == BackendKind::Auto {
        match crate::backends::sdk::lcore() {
            Ok(Some(_)) => BackendKind::Sdk,
            Ok(None) => BackendKind::Hid,
            Err(error) => {
                eprintln!(
                    "hardware test SDK selection failed while checking running LCore.exe: {error}"
                );
                return 3;
            }
        }
    } else {
        backend
    };

    match backend {
        BackendKind::Auto => unreachable!(),
        BackendKind::Sdk => {
            let owner = match crate::backends::sdk::lcore() {
                Ok(Some(owner)) => owner,
                Ok(None) => {
                    eprintln!("hardware test: SDK mode requires running LCore.exe");
                    return 3;
                }
                Err(error) => {
                    eprintln!(
                        "hardware test SDK selection failed while checking running LCore.exe: {error}"
                    );
                    return 3;
                }
            };
            let mut device =
                match crate::backends::sdk::SdkDevice::open(&owner, &cfg.logitech_friendly_name) {
                    Ok(device) => device,
                    Err(error) => {
                        eprintln!("hardware test SDK open failed: {error}");
                        return 3;
                    }
                };
            let test = TestConfig {
                duration,
                backend_name: "sdk",
                dashboard: Some(dashboard),
                ..Default::default()
            };
            let (mut results, buttons) = hardware_test::run(&Transport::DirectSdk(&device), &test);
            if let Err(error) = device.close(true) {
                eprintln!("hardware test SDK close failed: {error}");
                hardware_test::record_final_error(&mut results, "SDK shutdown", error);
            }
            report_test_results(&results, &buttons)
        }
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
            let (mut results, buttons) = hardware_test::run(&Transport::DirectHid(&device), &test);
            if let Err(error) = device.close() {
                crate::log_error!("hardware test close failed: {}", error);
                eprintln!("hardware test close failed: {error}");
                hardware_test::record_final_error(&mut results, "HID shutdown", error);
            }
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
                if b.canceled {
                    "canceled"
                } else if b.down {
                    "down"
                } else {
                    "up"
                }
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
    bottleneck: &mut BottleneckDetector,
) -> Snapshot {
    let now = SystemTime::now();
    let monotonic_now = Instant::now();
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
    let total_load = (!per_lp.is_empty()).then(|| per_lp.iter().sum::<f64>() / per_lp.len() as f64);
    let peak_load = per_lp.iter().copied().reduce(f64::max);
    let mem = memory::sample();

    let mut snapshot = Snapshot {
        now: Some(now),
        cpu_dual,
        date_text: clock.date,
        date_short_text: clock.date_short,
        time_text: clock.time,
        cpu_cache_load: Metric::valid(cache_load, now),
        cpu_freq_load: Metric::valid(freq_load, now),
        headset: if cfg.safe_mode {
            Default::default()
        } else {
            telemetry.headset.clone()
        },
        controller: if cfg.safe_mode {
            Default::default()
        } else {
            telemetry.controller.clone()
        },
        discord: if cfg.safe_mode {
            DiscordState::default()
        } else {
            discord.clone()
        },
        hung: if cfg.safe_mode {
            Vec::new()
        } else {
            telemetry.hung.clone()
        },
        system_battery: if cfg.safe_mode {
            Default::default()
        } else {
            telemetry.system_battery
        },
        ..Default::default()
    };

    // Canonical readings for the fixed bars.
    let mut readings = ReadingsSnapshot::default();
    if let Some(total_load) = total_load.filter(|load| load.is_finite()) {
        readings.metrics.push(Reading::current_percent(
            MetricKey::CPUUtilization,
            total_load,
            "cpu-system",
            now,
        ));
    }
    if let Some(peak_load) = peak_load.filter(|load| load.is_finite()) {
        readings.metrics.push(Reading::current_percent(
            MetricKey::CPUPeakUtilization,
            peak_load,
            "cpu-peak-logical-processor",
            now,
        ));
    }
    readings.metrics.extend(memory_readings(mem));
    if !cfg.safe_mode {
        readings
            .metrics
            .extend(telemetry.gpu_readings.iter().cloned());
        readings
            .metrics
            .extend(telemetry.extended_readings.iter().cloned());
    }
    snapshot.readings = readings;

    snapshot.bottleneck = bottleneck.update(
        BottleneckThresholds {
            cpu_percent: cfg.bottleneck_cpu_percent,
            gpu_percent: cfg.bottleneck_gpu_percent,
            memory_percent: cfg.bottleneck_memory_percent,
            disk_mbps: cfg.bottleneck_disk_mbps,
            sustain: cfg.bottleneck_sustain,
        },
        bottleneck_inputs(&snapshot.readings),
        monotonic_now,
    );

    if !cfg.safe_mode {
        apply_telemetry(&mut snapshot, telemetry, now);
    }

    if cfg.safe_mode {
        snapshot.providers.insert("safe-mode".into(), true);
    }
    if !cfg.safe_mode {
        for (name, ok) in &telemetry.providers {
            snapshot.providers.insert(name.clone(), *ok);
        }
        snapshot.providers.insert(
            "Discord".into(),
            snapshot.discord.connected && snapshot.discord.authenticated,
        );
    }
    if cfg.safe_mode || !cfg.hang_enabled || !hang_provider_available(telemetry) {
        snapshot.hung.clear();
    }
    snapshot
}

fn current_number(readings: &ReadingsSnapshot, key: MetricKey) -> Option<f64> {
    readings.lookup(key, "").and_then(|reading| {
        (reading.has_value && reading.freshness == Freshness::Current).then_some(reading.number)
    })
}

fn bottleneck_inputs(readings: &ReadingsSnapshot) -> BottleneckInputs {
    let disk = current_number(readings, MetricKey::DiskReadBytesPerSec)
        .zip(current_number(readings, MetricKey::DiskWriteBytesPerSec));
    BottleneckInputs {
        cpu_peak_percent: current_number(readings, MetricKey::CPUPeakUtilization),
        gpu_percent: current_number(readings, MetricKey::GPUUtilization),
        ram_percent: current_number(readings, MetricKey::RAMUtilization),
        vram_percent: current_number(readings, MetricKey::VRAMUtilization),
        disk_mbps: disk.map(|(read, write)| (read + write) / 1_000_000.0),
    }
}

fn memory_readings(sample: Result<memory::MemorySample, String>) -> Vec<Reading> {
    let Ok(sample) = sample else {
        return Vec::new();
    };
    vec![
        Reading::current_percent(
            MetricKey::RAMUtilization,
            sample.percent,
            "physical-memory",
            sample.sampled_at,
        ),
        Reading::current_bytes(
            MetricKey::RAMUsed,
            sample.used_bytes,
            "physical-memory",
            sample.sampled_at,
        ),
        Reading::current_bytes(
            MetricKey::RAMTotal,
            sample.total_bytes,
            "physical-memory",
            sample.sampled_at,
        ),
    ]
}

fn apply_telemetry(snapshot: &mut Snapshot, telemetry: &crate::telemetry::Update, now: SystemTime) {
    snapshot.cpu_temp = telemetry.cpu_temp;
    snapshot.gpu_temp = telemetry.gpu_temp;
    snapshot.game = telemetry.game.clone();
    snapshot.ping_ms = telemetry.ping_ms;
    snapshot.jitter_ms = telemetry.jitter_ms;
    snapshot.packet_loss = telemetry.packet_loss;

    // A sampled idle interval is valid telemetry, not unavailable data.
    snapshot.network_in = Metric::default();
    snapshot.network_out = Metric::default();
    if let Some(net) = telemetry.net {
        snapshot.network_in = Metric::valid(net.in_bps, net.sampled_at);
        snapshot.network_out = Metric::valid(net.out_bps, net.sampled_at);
    }

    // Audio endpoint state.
    snapshot.audio_volume = Metric::default();
    snapshot.microphone_known = false;
    snapshot.microphone_muted = false;
    if let Some(audio) = &telemetry.audio {
        snapshot.audio_volume = Metric::valid(
            audio.volume_percent,
            telemetry.audio_sampled_at.unwrap_or(now),
        );
        snapshot.microphone_known = audio.mic_known;
        snapshot.microphone_muted = audio.mic_muted;
    }
}

fn current_hang_publication(
    telemetry: &crate::telemetry::Update,
    generation: u64,
) -> (&[crate::model::HungTarget], bool) {
    if telemetry.generation != generation {
        return (&[], false);
    }
    let available = hang_provider_available(telemetry);
    (&telemetry.hung, available)
}

fn hang_provider_available(telemetry: &crate::telemetry::Update) -> bool {
    telemetry
        .providers
        .iter()
        .any(|(name, available)| name == "Hung window detector" && *available)
}

fn invalidate_telemetry_publication(
    telemetry: &mut crate::telemetry::Update,
    snapshot: &mut Snapshot,
    generation: u64,
) {
    *telemetry = crate::telemetry::Update {
        generation,
        ..Default::default()
    };
    *snapshot = Snapshot::default();
}

fn log_hang_audit(audit: &hang::Audit) {
    crate::log_warn!(
        "hung action pid={} process={} outcome={}",
        audit.target.pid,
        audit.target.process_name,
        audit.outcome
    );
}

fn apply_hold_command(
    command: hang::HoldCommand,
    slots: &mut Manager,
    cfg: &Config,
    alerts: &mut crate::alerts::Manager,
    snapshot: &mut Snapshot,
) {
    apply_hold_command_with(
        command,
        slots,
        cfg,
        alerts,
        snapshot,
        hang::terminate_bound_target,
    );
}

fn apply_hold_command_with<F>(
    command: hang::HoldCommand,
    slots: &mut Manager,
    cfg: &Config,
    alerts: &mut crate::alerts::Manager,
    snapshot: &mut Snapshot,
    terminate: F,
) where
    F: FnOnce(&crate::model::HungTarget, &Config) -> hang::ActionOutcome,
{
    match command {
        hang::HoldCommand::None => {}
        hang::HoldCommand::Cycle { index, backward } => {
            if index == 3 && alerts.acknowledge_highest(snapshot) {
                crate::log_info!("acknowledged highest critical alert");
                return;
            }
            let module = slots.cycle(index, if backward { -1 } else { 1 });
            crate::log_debug!("button {} -> slot module {}", index + 1, module);
        }
        hang::HoldCommand::Audit(audit) => log_hang_audit(&audit),
        hang::HoldCommand::Terminate(target) => {
            if cfg.safe_mode {
                log_hang_audit(&hang::Audit {
                    target,
                    outcome: "refused-safe-mode",
                });
                return;
            }
            log_hang_audit(&hang::Audit {
                target: target.clone(),
                outcome: "attempted",
            });
            let outcome = terminate(&target, cfg);
            log_hang_audit(&hang::Audit {
                target,
                outcome: outcome.label(),
            });
        }
    }
}

struct ConfigWatcher {
    files: Vec<(std::path::PathBuf, Option<FileSignature>)>,
    pub last_check: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileSignature {
    modified: Option<SystemTime>,
    len: u64,
    digest: String,
}

impl ConfigWatcher {
    fn new(paths: &[std::path::PathBuf]) -> Self {
        ConfigWatcher {
            files: paths
                .iter()
                .map(|path| (path.clone(), file_signature(path)))
                .collect(),
            last_check: Instant::now(),
        }
    }

    fn changed(&self) -> bool {
        self.files
            .iter()
            .any(|(path, signature)| file_signature(path) != *signature)
    }

    fn replace(&mut self, paths: &[std::path::PathBuf]) {
        self.files = paths
            .iter()
            .map(|path| (path.clone(), file_signature(path)))
            .collect();
    }
}

fn file_signature(path: &std::path::Path) -> Option<FileSignature> {
    let bytes = std::fs::read(path).ok()?;
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileSignature {
        modified: metadata.modified().ok(),
        len: metadata.len(),
        digest: crate::sha256::sha256_hex(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_overlay_options_preserve_directional_ceilings() {
        let cfg = Config {
            network_graph_ceiling_download_mbps: 1000.0,
            network_graph_ceiling_upload_mbps: 40.0,
            ..Config::default()
        };
        let options = network_overlay_options(&cfg);
        assert_eq!(options.network_graph_ceiling_download_mbps, 1000.0);
        assert_eq!(options.network_graph_ceiling_upload_mbps, 40.0);
    }

    #[test]
    fn hardware_discovery_summary_omits_raw_device_path() {
        let candidate = crate::backends::hid::CandidateInfo {
            device_path: r"\\?\hid#vid_046d&pid_c21c#private-instance".into(),
            vid: 0x046d,
            pid: 0xc21c,
            usage_page: 0xff00,
            usage: 0x0001,
            input_report_length: 9,
            output_report_length: 992,
        };
        let summary = hardware_candidate_summary(&candidate);
        assert_eq!(
            summary,
            "G13 candidate: vid=046d pid=c21c usage=ff00:0001 reports=9/992"
        );
        assert!(!summary.contains(&candidate.device_path));
    }

    #[test]
    fn memory_success_publishes_zero_and_bytes_while_failure_omits_all() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(7);
        let readings = memory_readings(Ok(memory::MemorySample {
            percent: 0.0,
            used_bytes: 0,
            total_bytes: 16,
            sampled_at: at,
        }));
        assert_eq!(readings.len(), 3);
        assert_eq!(readings[0].key, MetricKey::RAMUtilization);
        assert_eq!(readings[0].number, 0.0);
        assert_eq!(readings[1].key, MetricKey::RAMUsed);
        assert_eq!(readings[1].bytes, 0);
        assert_eq!(readings[2].key, MetricKey::RAMTotal);
        assert_eq!(readings[2].bytes, 16);
        assert!(readings
            .iter()
            .all(|reading| reading.sampled_at == Some(at)));
        assert!(memory_readings(Err("unavailable".into())).is_empty());
    }

    #[test]
    fn telemetry_projects_temperature_states_and_idle_network() {
        let now = SystemTime::now();
        let mut update = crate::telemetry::Update {
            cpu_temp: Metric::valid(67.0, now),
            gpu_temp: Metric::valid(74.0, now),
            ping_ms: Metric::valid(12.0, now),
            packet_loss: Metric::valid(0.0, now),
            net: Some(crate::providers::netif::NetThroughput::default()),
            ..Default::default()
        };
        update.gpu_temp.stale = true;
        let mut snapshot = Snapshot::default();
        apply_telemetry(&mut snapshot, &update, now);
        assert!(snapshot.cpu_temp.valid && !snapshot.cpu_temp.stale);
        assert!(snapshot.gpu_temp.valid && snapshot.gpu_temp.stale);
        assert_eq!(snapshot.ping_ms.value, 12.0);
        assert!(snapshot.packet_loss.valid);
        assert!(snapshot.network_in.valid && snapshot.network_out.valid);
        assert_eq!(snapshot.network_in.updated, Some(SystemTime::UNIX_EPOCH));

        apply_telemetry(&mut snapshot, &crate::telemetry::Update::default(), now);
        assert!(!snapshot.cpu_temp.valid);
        assert!(!snapshot.gpu_temp.valid);
        assert!(!snapshot.network_in.valid);
        assert!(!snapshot.audio_volume.valid);
    }

    #[test]
    fn bottleneck_inputs_require_native_core_data_and_use_peak_and_disk_sum() {
        let at = SystemTime::UNIX_EPOCH;
        let mut readings = ReadingsSnapshot {
            metrics: vec![
                Reading::current_percent(MetricKey::CPUPeakUtilization, 91.0, "cpu", at),
                Reading::current_percent(MetricKey::RAMUtilization, 50.0, "ram", at),
                Reading::current_number(
                    MetricKey::DiskReadBytesPerSec,
                    crate::model::ValueKind::ByteRate,
                    300_000_000.0,
                    "disk",
                    at,
                ),
                Reading::current_number(
                    MetricKey::DiskWriteBytesPerSec,
                    crate::model::ValueKind::ByteRate,
                    250_000_000.0,
                    "disk",
                    at,
                ),
            ],
            ..Default::default()
        };
        let input = bottleneck_inputs(&readings);
        assert_eq!(input.cpu_peak_percent, Some(91.0));
        assert_eq!(input.disk_mbps, Some(550.0));
        readings.metrics[2].freshness = Freshness::Stale;
        assert_eq!(bottleneck_inputs(&readings).disk_mbps, None);
    }

    #[test]
    fn preview_policy_tracks_physical_availability() {
        let mut cfg = Config::default();
        assert!(!preview_for_backend(
            &cfg,
            &BackendState::Connected {
                kind: BackendKind::Hid
            }
        ));
        assert!(!preview_for_backend(
            &cfg,
            &BackendState::Connected {
                kind: BackendKind::Sdk
            }
        ));
        assert!(preview_for_backend(
            &cfg,
            &BackendState::Connected {
                kind: BackendKind::Virtual
            }
        ));
        assert!(preview_for_backend(
            &cfg,
            &BackendState::Disconnected {
                reason: "test".into()
            }
        ));
        cfg.preview_mode = "never".into();
        assert!(!preview_for_backend(&cfg, &BackendState::Discovering));
        cfg.preview_mode = "always".into();
        assert!(preview_for_backend(&cfg, &BackendState::Discovering));
    }

    #[test]
    fn temperature_pane_warning_requires_both_legacy_and_category_switches() {
        let mut cfg = Config::default();
        assert!(temperature_pane_warning(&cfg));
        cfg.warning = false;
        assert!(!temperature_pane_warning(&cfg));
        cfg.warning = true;
        cfg.temperature_warning_enabled = false;
        assert!(!temperature_pane_warning(&cfg));
    }

    #[test]
    fn only_backend_settings_trigger_backend_reload() {
        let old = Config::default();
        let mut new = old.clone();
        new.date_format = "%Y".into();
        assert!(!backend_config_changed(&old, &new));
        new.logitech_backend = "sdk".into();
        assert!(backend_config_changed(&old, &new));
    }

    #[test]
    fn ccd_policy_change_replaces_manual_topology() {
        let old = Config::default();
        let new = Config {
            ccd_source: "manual".into(),
            ccd_cache_processors: Some(vec![0, 1]),
            ccd_frequency_processors: Some(vec![2, 3]),
            ..Config::default()
        };
        assert!(ccd_config_changed(&old, &new));
        let topology = resolve_ccd_topology(&new);
        assert!(topology.dual);
        assert_eq!(topology.cache_mask, 0b0011);
        assert_eq!(topology.freq_mask, 0b1100);
        let mut unrelated = new.clone();
        unrelated.preview_scale = 2;
        assert!(!ccd_config_changed(&new, &unrelated));
    }

    #[test]
    fn validation_loads_once_and_retains_files_for_the_watcher() {
        let dir = std::env::temp_dir().join(format!(
            "lcdsirplus-validation-test-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let primary = dir.join("main.txt");
        let include = dir.join("local.txt");
        std::fs::write(&primary, "include local.txt\n").unwrap();
        std::fs::write(&include, "preview_scale 2\n").unwrap();

        let before = crate::parser::test_load_calls();
        let valid = validate_config(&primary);
        assert_eq!(crate::parser::test_load_calls(), before + 1);
        assert!(valid.ok);
        assert_eq!(
            valid.message,
            "configuration valid (2 file(s), 18 bytes primary)"
        );
        let loaded = valid.loaded.unwrap();
        assert_eq!(loaded.files.len(), 2);
        let watcher = ConfigWatcher::new(&loaded.files);
        assert!(!watcher.changed());

        std::fs::write(&primary, "unknown_setting 1\n").unwrap();
        let before = crate::parser::test_load_calls();
        let invalid = validate_config(&primary);
        assert_eq!(crate::parser::test_load_calls(), before + 1);
        assert!(!invalid.ok && invalid.loaded.is_none());
        assert!(invalid
            .message
            .ends_with(":1: unknown key \"unknown_setting\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_watcher_tracks_includes_and_retries_until_success_commit() {
        let dir = std::env::temp_dir().join(format!(
            "lcdsirplus-watcher-test-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let primary = dir.join("main.txt");
        let include = dir.join("local.txt");
        std::fs::write(&primary, "include local.txt\n").unwrap();
        std::fs::write(&include, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files);
        assert!(!watcher.changed());

        std::fs::write(&include, "preview_scale 3\n").unwrap();
        assert!(watcher.changed(), "transitive include change is observed");
        assert!(
            watcher.changed(),
            "failed reload does not commit the changed baseline"
        );
        let reloaded = crate::parser::load(&primary).unwrap();
        watcher.replace(&reloaded.files);
        assert!(!watcher.changed());

        std::fs::remove_file(&include).unwrap();
        assert!(watcher.changed(), "missing include triggers reload");
        assert!(crate::parser::load(&primary).is_err());
        assert!(watcher.changed(), "missing include keeps triggering retry");
        std::fs::write(&include, "preview_scale 4\n").unwrap();
        assert!(watcher.changed(), "replacement include remains pending");
        let reloaded = crate::parser::load(&primary).unwrap();
        watcher.replace(&reloaded.files);
        assert!(!watcher.changed());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn safe_mode_snapshot_contains_only_native_cpu_and_ram_data() {
        let at = SystemTime::UNIX_EPOCH;
        let telemetry = crate::telemetry::Update {
            headset: crate::model::HeadsetBattery {
                present: true,
                ..Default::default()
            },
            gpu_readings: vec![Reading::current_percent(
                MetricKey::GPUUtilization,
                99.0,
                "gpu",
                at,
            )],
            extended_readings: vec![Reading::current_number(
                MetricKey::EstablishedConnections,
                crate::model::ValueKind::Count,
                5.0,
                "tcp",
                at,
            )],
            ..Default::default()
        };
        let cfg = Config {
            safe_mode: true,
            ccd_source: "manual".into(),
            ccd_cache_processors: Some(vec![0]),
            ..Config::default()
        };
        let mut cpu = cpu::CpuLoadProvider::new();
        let mut bottleneck = BottleneckDetector::default();
        let snapshot = build_snapshot(
            &cfg,
            &resolve_ccd_topology(&cfg),
            &mut cpu,
            &telemetry,
            &DiscordState {
                connected: true,
                ..Default::default()
            },
            &mut bottleneck,
        );
        assert!(!snapshot.headset.present);
        assert!(!snapshot.discord.connected);
        assert!(snapshot.readings.metrics.iter().all(|reading| matches!(
            reading.key,
            MetricKey::CPUUtilization
                | MetricKey::CPUPeakUtilization
                | MetricKey::RAMUtilization
                | MetricKey::RAMUsed
                | MetricKey::RAMTotal
        )));
    }

    #[test]
    fn safe_mode_never_attempts_startup_synchronization() {
        let cfg = Config {
            safe_mode: true,
            start_at_login: true,
            ..Config::default()
        };
        let mut synced = None;
        sync_startup_if_needed(&cfg, false, None, &mut synced);
        assert_eq!(synced, None);
    }

    #[test]
    fn installed_mode_suppresses_startup_while_portable_mode_keeps_syncing() {
        use std::cell::Cell;

        let cfg = Config {
            start_at_login: true,
            ..Config::default()
        };
        let executable = std::path::Path::new(r"C:\LCDSirPlus\LCDSirPlus.exe");
        let calls = Cell::new(0);
        let mut synced = None;
        sync_startup_if_needed_with(&cfg, true, Some(executable), &mut synced, |_, _| {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(calls.get(), 0);
        assert_eq!(synced, None);

        sync_startup_if_needed_with(
            &cfg,
            false,
            Some(executable),
            &mut synced,
            |enabled, path| {
                calls.set(calls.get() + 1);
                assert!(enabled);
                assert_eq!(path, executable);
                Ok(())
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(synced, Some(true));
    }

    #[test]
    fn safe_mode_cycles_acknowledges_and_never_calls_terminate() {
        use std::cell::Cell;

        let cfg = Config {
            safe_mode: true,
            ..Config::default()
        };
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut alerts = crate::alerts::Manager::default();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        alerts.evaluate(&mut snapshot, &cfg, Instant::now());
        let kills = Cell::new(0);

        apply_hold_command_with(
            hang::HoldCommand::Cycle {
                index: 0,
                backward: false,
            },
            &mut slots,
            &cfg,
            &mut alerts,
            &mut snapshot,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionOutcome::Success
            },
        );
        assert_eq!(slots.current(0), "CPU_TEMP");

        apply_hold_command_with(
            hang::HoldCommand::Cycle {
                index: 3,
                backward: false,
            },
            &mut slots,
            &cfg,
            &mut alerts,
            &mut snapshot,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionOutcome::Success
            },
        );
        assert!(snapshot.alerts[0].acknowledged);

        apply_hold_command_with(
            hang::HoldCommand::Terminate(crate::model::HungTarget {
                hwnd: 1,
                pid: 2,
                creation_time: 3,
                image_path: r"C:\Games\game.exe".into(),
                process_name: "game.exe".into(),
                title: "Game".into(),
            }),
            &mut slots,
            &cfg,
            &mut alerts,
            &mut snapshot,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionOutcome::Success
            },
        );
        assert_eq!(kills.get(), 0);
    }

    #[test]
    fn dynamic_modules_fall_through_without_changing_selection() {
        let cfg = Config::default();
        let slots = Manager::new(&cfg, [0; 4]);
        let mut snapshot = Snapshot::default();
        snapshot.bottleneck.state = crate::model::BottleneckState::None;
        snapshot.bottleneck.freshness = Freshness::Current;
        assert_eq!(slots.current(2), "PROC_HANG");
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[2],
            "DISK_IO"
        );
        assert_eq!(slots.current(2), "PROC_HANG");

        snapshot.hung.push(crate::model::HungTarget::default());
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[2],
            "PROC_HANG"
        );

        let mut dynamic = cfg;
        dynamic.slots[0] = vec!["BOTTLENECK".into(), "PROC_HANG".into()];
        dynamic.slots[2] = vec!["GPU_TEMP".into()];
        let slots = Manager::new(&dynamic, [0; 4]);
        snapshot.hung.clear();
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &dynamic)[0],
            "CLEAR"
        );
        assert_eq!(slots.current(0), "BOTTLENECK");
    }

    #[test]
    fn presentmon_deferred_resolves_without_changing_stored_selection() {
        let mut cfg = Config::default();
        cfg.slots[0] = ["PROC_HANG", "FPS_CURRENT", "GPU_TEMP"]
            .map(str::to_string)
            .to_vec();
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut snapshot = Snapshot::default();
        snapshot.providers.insert("presentmon".into(), true);
        snapshot.game.active = true;
        snapshot.game.fps = crate::model::Metric::valid(60.0, SystemTime::now());

        snapshot.hung.push(crate::model::HungTarget::default());
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[0],
            "PROC_HANG"
        );
        snapshot.hung.clear();
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[0],
            "FPS_CURRENT"
        );
        snapshot.game = Default::default();
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[0],
            "GPU_TEMP"
        );
        assert_eq!(slots.current(0), "PROC_HANG");

        assert_eq!(slots.cycle(0, 1), "FPS_CURRENT");
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[0],
            "GPU_TEMP"
        );
        assert_eq!(slots.current(0), "FPS_CURRENT");

        cfg.presentmon_deferred = false;
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[0],
            "FPS_CURRENT"
        );
    }

    #[test]
    fn presentmon_deferred_checks_every_panel_and_skips_dynamic_fallbacks() {
        let modules = [
            "FPS_CURRENT",
            "FPS_1LOW",
            "FPS_01LOW",
            "FRAME_TIME",
            "FPS_GRAPH",
            "SESSION_TIME",
            "SESSION_SUMMARY",
            "GAME_NAME",
        ];
        let mut current = Snapshot::default();
        current.providers.insert("presentmon".into(), true);
        current.game.active = true;
        current.game.session_start = Some(SystemTime::now());
        current.game.game_name = "Game".into();
        current.game.fps = crate::model::Metric::valid(60.0, SystemTime::now());
        current.game.one_percent = crate::model::Metric::valid(50.0, SystemTime::now());
        current.game.point_one_low = crate::model::Metric::valid(40.0, SystemTime::now());
        current.game.frame_time_ms = crate::model::Metric::valid(16.0, SystemTime::now());

        for module in modules {
            let mut cfg = Config::default();
            cfg.slots[0] = [module, "GPU_TEMP"].map(str::to_string).to_vec();
            let slots = Manager::new(&cfg, [0; 4]);
            assert_eq!(effective_slot_modules(&slots, &current, &cfg)[0], module);

            let mut unavailable = current.clone();
            match module {
                "FPS_CURRENT" | "FPS_GRAPH" => unavailable.game.fps.stale = true,
                "FPS_1LOW" => unavailable.game.one_percent.valid = false,
                "FPS_01LOW" => unavailable.game.point_one_low.value = f64::NAN,
                "FRAME_TIME" => unavailable.game.frame_time_ms.value = f64::INFINITY,
                "SESSION_TIME" | "SESSION_SUMMARY" => unavailable.game.session_start = None,
                "GAME_NAME" => unavailable.game.game_name.clear(),
                _ => unreachable!(),
            }
            assert_eq!(
                effective_slot_modules(&slots, &unavailable, &cfg)[0],
                "GPU_TEMP",
                "{module}"
            );

            unavailable = current.clone();
            unavailable.providers.remove("presentmon");
            assert_eq!(
                effective_slot_modules(&slots, &unavailable, &cfg)[0],
                "GPU_TEMP",
                "absent {module}"
            );
        }

        let mut cfg = Config::default();
        cfg.slots[0] = ["FPS_CURRENT", "PROC_HANG", "BOTTLENECK", "GPU_TEMP"]
            .map(str::to_string)
            .to_vec();
        let slots = Manager::new(&cfg, [0; 4]);
        let mut unavailable = Snapshot::default();
        unavailable.bottleneck.freshness = Freshness::Current;
        assert_eq!(
            effective_slot_modules(&slots, &unavailable, &cfg)[0],
            "GPU_TEMP"
        );

        cfg.slots[0].pop();
        let slots = Manager::new(&cfg, [0; 4]);
        assert_eq!(
            effective_slot_modules(&slots, &unavailable, &cfg)[0],
            "CLEAR"
        );

        cfg.slots[0] = ["FPS_CURRENT", "GPU_TEMP"].map(str::to_string).to_vec();
        let slots = Manager::new(&cfg, [0; 4]);
        cfg.presentmon_enabled = false;
        assert_eq!(
            effective_slot_modules(&slots, &current, &cfg)[0],
            "GPU_TEMP"
        );
        cfg.presentmon_enabled = true;
        cfg.safe_mode = true;
        assert_eq!(
            effective_slot_modules(&slots, &current, &cfg)[0],
            "GPU_TEMP"
        );
        cfg.safe_mode = false;
        cfg.presentmon_target_mode = "disabled".into();
        assert_eq!(
            effective_slot_modules(&slots, &current, &cfg)[0],
            "GPU_TEMP"
        );
    }

    #[test]
    fn stale_hang_generation_cannot_bind_during_reload_button_race() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let target = crate::model::HungTarget {
            hwnd: 1,
            pid: 2,
            creation_time: 3,
            image_path: r"C:\Games\game.exe".into(),
            process_name: "game.exe".into(),
            title: "Game".into(),
        };
        let stale = crate::telemetry::Update {
            generation: 1,
            hung: vec![target.clone()],
            providers: vec![("Hung window detector".into(), true)],
            ..Default::default()
        };
        let (targets, available) = current_hang_publication(&stale, 2);
        assert!(targets.is_empty() && !available);

        let started = Instant::now();
        let mut hold = hang::HoldState::default();
        hold.set_owner(Some(2));
        hold.event(
            crate::input::Event {
                index: 2,
                down: true,
                backward: false,
                canceled: false,
                at: started,
                source: "test",
            },
            &cfg,
            targets,
            available,
        );
        assert!(!matches!(
            hold.tick(started + cfg.hang_hold, &cfg, targets, available),
            hang::HoldCommand::Terminate(_)
        ));

        let current = crate::telemetry::Update {
            generation: 2,
            hung: vec![target.clone()],
            providers: vec![("Hung window detector".into(), true)],
            ..Default::default()
        };
        let (targets, available) = current_hang_publication(&current, 2);
        let mut hold = hang::HoldState::default();
        hold.set_owner(Some(2));
        hold.event(
            crate::input::Event {
                index: 2,
                down: true,
                backward: false,
                canceled: false,
                at: started,
                source: "test",
            },
            &cfg,
            targets,
            available,
        );
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, targets, available),
            hang::HoldCommand::Terminate(bound) if bound == target
        ));
        assert!(matches!(
            hold.event(
                crate::input::Event {
                    index: 2,
                    down: false,
                    backward: false,
                    canceled: false,
                    at: started + cfg.hang_hold,
                    source: "test",
                },
                &cfg,
                targets,
                available,
            ),
            hang::HoldCommand::None
        ));
    }

    #[test]
    fn selector_reload_immediately_invalidates_all_telemetry_publication() {
        let mut telemetry = crate::telemetry::Update {
            generation: 1,
            cpu_temp: Metric::valid(67.0, SystemTime::UNIX_EPOCH),
            gpu_temp: Metric::valid(74.0, SystemTime::UNIX_EPOCH),
            hung: vec![crate::model::HungTarget::default()],
            providers: vec![
                ("Hung window detector".into(), true),
                ("other".into(), true),
            ],
            ..Default::default()
        };
        let mut snapshot = Snapshot {
            cpu_temp: Metric::valid(67.0, SystemTime::UNIX_EPOCH),
            gpu_temp: Metric::valid(74.0, SystemTime::UNIX_EPOCH),
            hung: vec![crate::model::HungTarget::default()],
            providers: [
                ("Hung window detector".into(), true),
                ("other".into(), true),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        invalidate_telemetry_publication(&mut telemetry, &mut snapshot, 2);
        assert_eq!(telemetry.generation, 2);
        assert!(telemetry.hung.is_empty() && snapshot.hung.is_empty());
        assert!(!telemetry.cpu_temp.valid && !snapshot.cpu_temp.valid);
        assert!(!telemetry.gpu_temp.valid && !snapshot.gpu_temp.valid);
        assert!(telemetry.providers.is_empty() && snapshot.providers.is_empty());
    }

    #[test]
    fn reload_invalidation_preserves_acknowledged_alert_until_fresh_recovery() {
        let cfg = Config {
            critical_alert_linger: Duration::ZERO,
            telemetry_interval: Duration::from_secs(60),
            ..Config::default()
        };
        let now = Instant::now();
        let mut alerts = crate::alerts::Manager::default();
        let mut snapshot = Snapshot {
            gpu_temp: Metric::valid(91.0, SystemTime::UNIX_EPOCH),
            ..Default::default()
        };
        alerts.evaluate(&mut snapshot, &cfg, now);
        assert!(alerts.acknowledge_highest(&mut snapshot));

        let mut telemetry = crate::telemetry::Update::default();
        alerts.clear_disabled_categories(&cfg);
        invalidate_telemetry_publication(&mut telemetry, &mut snapshot, 1);
        alerts.evaluate(&mut snapshot, &cfg, now + cfg.telemetry_interval);
        assert_eq!(snapshot.alerts.len(), 1);
        assert!(snapshot.alerts[0].acknowledged);

        snapshot.gpu_temp = Metric::valid(40.0, SystemTime::UNIX_EPOCH);
        alerts.evaluate(
            &mut snapshot,
            &cfg,
            now + cfg.telemetry_interval + Duration::from_secs(1),
        );
        assert!(snapshot.alerts.is_empty());
    }

    #[test]
    fn safe_mode_reload_clears_old_provider_values_before_rebuild() {
        let mut telemetry = crate::telemetry::Update {
            generation: 1,
            cpu_temp: Metric::valid(67.0, SystemTime::UNIX_EPOCH),
            extended_readings: vec![Reading::current_number(
                MetricKey::CPUPower,
                crate::model::ValueKind::Watts,
                100.0,
                "old-hwinfo",
                SystemTime::UNIX_EPOCH,
            )],
            providers: vec![("hwinfo".into(), true)],
            ..Default::default()
        };
        let mut snapshot = Snapshot {
            cpu_temp: telemetry.cpu_temp,
            readings: ReadingsSnapshot {
                metrics: telemetry.extended_readings.clone(),
                ..Default::default()
            },
            providers: [("hwinfo".into(), true)].into_iter().collect(),
            ..Default::default()
        };
        invalidate_telemetry_publication(&mut telemetry, &mut snapshot, 2);
        assert_eq!(telemetry.generation, 2);
        assert!(!snapshot.cpu_temp.valid);
        assert!(snapshot.readings.metrics.is_empty());
        assert!(snapshot.providers.is_empty());
        assert!(telemetry.extended_readings.is_empty());
        assert!(telemetry.providers.is_empty());
    }
}
