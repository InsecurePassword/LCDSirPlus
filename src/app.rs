//! Application orchestration: config lifecycle, providers, render loop,
//! backend wiring, UI events, and hot reload.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::backends::{Backend, BackendKind, BackendState, LossCause, Message};
use crate::config::{Config, FileSignature};
use crate::hardware_test::{self, TestConfig, Transport};
use crate::logging::Level;
use crate::model::{
    DeviceStatus, DiscordState, Metric, MetricKey, Reading, ReadingsSnapshot, Snapshot,
};
use crate::providers::{ccd, clock, cpu, hang, memory};
use crate::render::renderer::{OverlayOptions, Renderer, View};
use crate::slots::Manager;
use crate::telemetry::PROBE_INDETERMINATE;
use crate::ui::{Ui, UiEvent};

pub struct RunOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub preview_always: bool,
    pub safe_mode: bool,
    pub diagnostic_dir: Option<std::path::PathBuf>,
    pub recovery_handoff: crate::recovery::Handoff,
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
    // WO-OPT01 (C1): the watcher is seeded with the signatures of the bytes the
    // STARTUP parse consumed, not with a fresh read of the disk, so a write that
    // lands between the startup parse and the watcher cannot be recorded as
    // applied unparsed.
    let watched_signatures = loaded.signatures.clone();
    let mut cfg = loaded.config;
    if opts.safe_mode {
        cfg.safe_mode = true;
    }
    if let Err(error) = init_logging(&cfg, opts.diagnostic_dir.as_deref()) {
        return Err((1, format!("logging initialization failed: {error}")));
    }
    crate::log_info!("LCDSirPlus {} starting", env!("CARGO_PKG_VERSION"));
    crate::log_info!("configuration: {}", cfg.path);
    let mut recovery =
        crate::recovery::Recovery::start(opts.recovery_handoff, cfg.logitech_reconnect_max);
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
    let mut backend = match Backend::spawn(backend_kind, &cfg) {
        Ok(backend) => backend,
        Err(error) => {
            // WO-U46G item 5: the worker thread is the transport's only driver,
            // so its absence is named on the established startup-failure outcome
            // (the RC-1 cause+detail row) and the process exits with the
            // startup-failure code the logging site uses - never a panic. No
            // ownership was claimed: `Backend::spawn` claims on its own thread.
            crate::log_error!("backend worker could not start: {error}");
            return Err((1, format!("backend worker could not start: {error}")));
        }
    };
    crate::log_info!("backend: {}", backend_kind.name());

    // UI (tray + preview per mode).
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let show_preview =
        opts.preview_always || (!cfg.start_minimized && cfg.preview_mode.as_str() == "always");
    let ui = Ui::spawn(cfg.preview_scale.max(1) as u32, show_preview, ui_tx);

    // Providers.
    let mut cpu_provider = cpu::CpuLoadProvider::new();
    let mut slots = Manager::new(&cfg, [0; 4]);
    let mut snapshot = Snapshot::default();
    let seed_modules = effective_slot_modules(&slots, &snapshot, &cfg);
    let mut discord_demanded = discord_display_demand(&seed_modules);
    let telemetry_runtime = crate::telemetry::spawn(
        &cfg,
        provider_consumer_bits(&seed_modules, &cfg, false)
            | provider_demand_bits(&slots, &seed_modules, &cfg),
    );
    let discord_runtime =
        crate::providers::discord::spawn(&discord_runtime_config(&cfg, discord_demanded));
    let mut telemetry = crate::telemetry::Update {
        generation: telemetry_runtime.generation(),
        ..Default::default()
    };
    let mut discord = DiscordState::default();
    let mut renderer = Renderer::new();
    let mut hang_hold = hang::HoldState::default();
    // EC-4: the startup bind marks the owner resolved; later binds happen only
    // when the slot configuration changes.
    let mut hang_owner = HangOwner::default();
    let _ = hang_owner.bind(&slots, &mut hang_hold);

    let mut watcher = ConfigWatcher::new(&watched_files, &watched_signatures);

    // WO-U46H #2: the shipped seed was `Instant::now() - cfg.telemetry_interval`.
    // `Instant - Duration` is a documented panic on underflow (release builds
    // abort, no dialog): on a clock based at boot time it fires when uptime <
    // interval. Measured on this host, Windows keeps ~292 years of headroom, so a
    // LEGAL interval (100..10000 / 25..5000 ms) cannot reach it here - the form is
    // a latent panic, so it is removed. ONE `now` is captured for both seeds and
    // the initial state is the explicit never-stamped marker, so the first
    // collection and the first render stay immediate - a fallback to `now` would
    // delay each of them by a full interval.
    let startup_now = Instant::now();
    let mut last_telemetry = cadence_initial_state(startup_now, cfg.telemetry_interval);
    let mut last_render = cadence_initial_state(startup_now, cfg.render_interval);
    // WO-U46H #4: the last reload-rejection identity (see `reload_rejection_is_new`).
    let mut last_reload_rejection: Option<String> = None;
    let mut running = true;
    let mut backend_state = BackendState::Discovering;
    // RC-5: the label last published to the panel/log surface, so a change is
    // named exactly once on the log.
    let mut published_status = DeviceStatus::Healthy;
    let process_epoch = Instant::now();
    // EC-3: seed the consumer set before the first tick so the panel's own
    // modules keep their providers at full cadence from the start.
    // WO-TEL-D2 (S1): the word gains the reading-keyed demand bits for the
    // previously never-gated metering providers (same publication path).
    telemetry_runtime.set_consumers(
        provider_consumer_bits(
            &seed_modules,
            &cfg,
            preview_surface_active(opts.preview_always, &cfg, &backend_state),
        ) | provider_demand_bits(&slots, &seed_modules, &cfg),
    );

    // SOAK-OBS2: the resource-line tracker is seeded at start, so the first
    // line lands at t~60 s (the interval), never at t=0.
    let mut last_soak_resources = Instant::now();

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
            // WO-U46H #9: `detect` returns the DETECTION-TIME signature snapshot,
            // which `install` records on success, so bytes that arrive after
            // detection can never be recorded as applied.
            if let Some(observed) = watcher.detect() {
                crate::log_info!("configuration changed on disk; reloading");
                // WO-OPT01R (F-AUD-OPT01-1): the graph this attempt consumes - root
                // plus every include, whether or not an error names it - so a
                // rejection can record all of it for the bounded full recheck.
                let mut consumed_graph: Vec<(std::path::PathBuf, FileSignature)> = Vec::new();
                match crate::parser::load_with_consumed(&config_path, &mut consumed_graph) {
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
                        if topology_changed {
                            topology = resolve_ccd_topology(&cfg);
                            crate::log_info!("CCD topology reloaded: {}", topology.detail);
                        }
                        let generation = telemetry_runtime.update_config(&cfg);
                        invalidate_telemetry_publication(&mut telemetry, &mut snapshot, generation);
                        renderer = Renderer::new();
                        slots.apply(&cfg);
                        discord_demanded = discord_display_demand(&effective_slot_modules(
                            &slots, &snapshot, &cfg,
                        ));
                        discord_runtime
                            .update_config(&discord_runtime_config(&cfg, discord_demanded));
                        hang_owner.invalidate();
                        if let Some(audit) = hang_owner.bind(&slots, &mut hang_hold) {
                            log_hang_audit(&audit);
                        }
                        watcher.install(&files, &observed, &loaded.signatures);
                        if backend_changed {
                            recovery.suspend();
                            match backend.reconfigure(
                                crate::backends::resolve_kind(&cfg.logitech_backend),
                                &cfg,
                            ) {
                                Ok(()) => backend_state = BackendState::Discovering,
                                Err(error) => {
                                    // WO-U46G item 5: the reload publishes the
                                    // established Disconnected state/row (the
                                    // worker's own construction point for a
                                    // transport loss) and names the cause on the
                                    // log. No device was claimed and no worker
                                    // exists, so nothing restarts here: the
                                    // reconnect ladder owns re-attempts.
                                    crate::log_error!("backend reload failed: {error}");
                                    backend_state = crate::backends::disconnect_state(&error);
                                }
                            }
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
                        // WO-U46H #4: a successful reload clears the suppression, so
                        // any later rejection logs again, even the identical one.
                        last_reload_rejection = None;
                        crate::log_info!("configuration reloaded");
                    }
                    Err(e) => {
                        // WO-U46H #4: the reload attempt keeps running every refresh
                        // tick (repair stays detected), so a permanently-invalid
                        // watched config would otherwise emit one ERROR row per tick
                        // and rotate the 2 MiB diagnostic ring away in minutes. The
                        // row is emitted once per rejection identity - the parse
                        // error's own `file:line: message`, i.e. the trigger plus the
                        // reason - and a changed reason is a new identity. Bounded
                        // state: one identity string, replaced in place.
                        let identity = e.to_string();
                        // WO-OPT01 (O7 + C1) + WO-OPT01R (F-AUD-OPT01-1): record
                        // the rejected CONTENT version, so an unchanged repeat is
                        // quiet (no re-parse, no row); watch the file the parse
                        // named, so a dependency that is absent now is detected
                        // when it appears; and record the full graph this attempt
                        // consumed, so the bounded full recheck re-verifies every
                        // file the parse actually read. Nothing from the rejected
                        // bytes is adopted: the applied baseline stays the last
                        // valid graph, so a repair is still detected.
                        watcher.record_rejection(&observed, &e.file, &consumed_graph);
                        if reload_rejection_is_new(&mut last_reload_rejection, &identity) {
                            crate::log_error!(
                                "configuration reload rejected: {} (last valid config stays active)",
                                e
                            );
                        }
                    }
                }
            }
        }

        let telemetry_elapsed = last_telemetry.map(|last| now.duration_since(last));
        if cadence_due(telemetry_elapsed, cfg.telemetry_interval) {
            last_telemetry = Some(now);
            snapshot = build_snapshot(&cfg, &topology, &mut cpu_provider, &telemetry, &discord);
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
            // WO-QA-03 (F-b) wiring: a stale publication drops its loss-cause label too.
            snapshot
                .providers
                .retain(|name, _| !name.starts_with(PROBE_INDETERMINATE));
        }

        if let Some(audit) = hang_owner.bind(&slots, &mut hang_hold) {
            log_hang_audit(&audit);
        }
        if let Some(audit) = hang_hold.reconcile(&cfg, published_hung, hang_provider_available) {
            log_hang_audit(&audit);
        }
        // RC-5: publish the device/recovery label on the display surface every
        // loop, so a rebuilt snapshot always carries the current state; the log
        // names each transition once. The frame is rendered and submitted as
        // before, so the preview-retained fallback never blanks the panel.
        let device_status = published_device_status(&backend_state, &recovery.display_status());
        if device_status != published_status {
            crate::log_warn!("device surface: {}", device_status.label());
            published_status = device_status.clone();
        }
        snapshot.device_status = device_status;

        let render_elapsed = last_render.map(|last| now.duration_since(last));
        if cadence_due(render_elapsed, cfg.render_interval) {
            last_render = Some(now);
            let slot_modules = effective_slot_modules(&slots, &snapshot, &cfg);
            let demanded = discord_display_demand(&slot_modules);
            if demanded != discord_demanded {
                discord_demanded = demanded;
                discord_runtime.update_config(&discord_runtime_config(&cfg, demanded));
                discord = DiscordState::default();
                snapshot.discord = DiscordState::default();
            }
            // EC-3: publish the consumer set alongside the modules the panel
            // shows, so a quiescent provider returns to full cadence on the
            // next scheduled tick when a consumer reappears.
            // F-2: the Arc<AtomicU64> behind set_consumers is stored/loaded
            // Relaxed on purpose: the bitmask is a self-contained word with no
            // payload published alongside it, so no Acquire/Release pairing is
            // required, and a one-tick-stale read only delays a cadence decision.
            telemetry_runtime.set_consumers(
                provider_consumer_bits(
                    &slot_modules,
                    &cfg,
                    preview_surface_active(opts.preview_always, &cfg, &backend_state),
                ) | provider_demand_bits(&slots, &slot_modules, &cfg),
            );
            let view = View {
                slot_modules,
                hung_index: hang_hold.hung_index,
                hung_detail: hang_hold.hung_detail,
                hung_hold: hang_hold.progress(now, &cfg),
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
                Message::State(state) => {
                    // RC-2: the drain is the controller's only view of worker
                    // liveness, so every published state is stamped here.
                    recovery.note_liveness(now, &state);
                    match state {
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
                        BackendState::Disconnected { cause, reason } => {
                            recovery.disconnected(now, cause);
                            crate::log_warn!("backend disconnected: {}", reason);
                            backend_state = BackendState::Disconnected { cause, reason };
                            if !opts.preview_always
                                && cfg.preview_mode == "auto"
                                && !cfg.start_minimized
                            {
                                ui.set_preview_auto_visible(true);
                            }
                        }
                        BackendState::Absent { reason } => {
                            // Ladder terminal: the preview policy stays consistent
                            // with `Disconnected`, so the panel is never blanked.
                            recovery.disconnected(now, LossCause::DeviceAbsent);
                            crate::log_warn!("backend absent: {}", reason);
                            backend_state = BackendState::Absent { reason };
                            if !opts.preview_always
                                && cfg.preview_mode == "auto"
                                && !cfg.start_minimized
                            {
                                ui.set_preview_auto_visible(true);
                            }
                        }
                        BackendState::Discovering => {
                            recovery.disconnected(now, LossCause::DeviceAbsent);
                            backend_state = BackendState::Discovering;
                        }
                        BackendState::ShutDown => backend_state = BackendState::ShutDown,
                    }
                }
                Message::Buttons(events) => {
                    for event in events {
                        let command =
                            hang_hold.event(event, &cfg, published_hung, hang_provider_available);
                        apply_hold_command(
                            command,
                            &mut hang_hold,
                            &mut slots,
                            &cfg,
                            &mut hang_owner,
                        );
                        if let Some(audit) = hang_owner.bind(&slots, &mut hang_hold) {
                            log_hang_audit(&audit);
                        }
                    }
                }
                Message::PhysicalHealthy(kind) => recovery.healthy(kind),
                Message::ReconnectStarted => recovery.reconnect_started(),
            }
        }

        // UI events (non-blocking drain).
        while let Ok(event) = ui_rx.try_recv() {
            match event {
                UiEvent::SlotCycle { slot, backward } => {
                    // WO-REMOVE-ALERTS: the preview cycle is the ordinary slot action;
                    // the retired acknowledgement detour owned no input any more.
                    let delta: i64 = if backward { -1 } else { 1 };
                    slots.cycle(slot, delta);
                    hang_owner.invalidate();
                    if let Some(audit) = hang_owner.bind(&slots, &mut hang_hold) {
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

        if running
            && recovery.tick(
                now,
                !cfg.safe_mode && cfg.logitech_backend != "virtual",
                || backend.request_reconnect(),
            )
        {
            running = false;
        }

        if let Some(audit) = hang_owner.bind(&slots, &mut hang_hold) {
            log_hang_audit(&audit);
        }
        if let Some(audit) = hang_hold.reconcile(&cfg, published_hung, hang_provider_available) {
            log_hang_audit(&audit);
        }
        let command = hang_hold.tick(now, &cfg, published_hung, hang_provider_available);
        apply_hold_command(command, &mut hang_hold, &mut slots, &cfg, &mut hang_owner);

        // SOAK-OBS2: one ASCII resource line every 60 s of loop time; the gate
        // mirrors the L202 `config_refresh` compare and never allocates per frame.
        if soak_resources_due(now.duration_since(last_soak_resources)) {
            last_soak_resources = now;
            let sample = soak_resources_sample(&snapshot, now, process_epoch);
            crate::log_info!("{}", soak_resources_line(&sample));
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    crate::log_info!("shutting down");
    drop(recovery);
    backend.shutdown();
    ui.shutdown();
    Ok(0)
}

/// SOAK-OBS2: one ASCII resource line per minute in the soak log.
///
/// The 60 s cadence is CODE-DERIVED, not a measured value: it is 6,000
/// iterations of the app loop's own 10 ms sleep, gated in the shape of the
/// existing `config_refresh` compare. It adds no config key, no thread, no
/// timer and no per-frame allocation.
const SOAK_RESOURCES_INTERVAL_SECS: u64 = 60;
const SOAK_RESOURCES_INTERVAL: Duration = Duration::from_secs(SOAK_RESOURCES_INTERVAL_SECS);

fn soak_resources_due(elapsed: Duration) -> bool {
    elapsed >= SOAK_RESOURCES_INTERVAL
}

/// One sample of the schema. The OS quantities are read once per line;
/// `sys_busy_pct` is only ever the last published reading (`None` = before the
/// first publish, rendered as `none`).
#[derive(Clone, Copy, Debug)]
struct SoakResourcesSample {
    pid: u32,
    uptime_s: u64,
    cpu_s: Option<f64>,
    priv_bytes: Option<u64>,
    ws_bytes: Option<u64>,
    handles: Option<u64>,
    sys_avail_mb: Option<u64>,
    sys_busy_pct: Option<f64>,
}

/// The schema line: pure (no I/O, no clock, no syscall), fixed key order.
fn soak_resources_line(sample: &SoakResourcesSample) -> String {
    let SoakResourcesSample {
        pid,
        uptime_s,
        cpu_s,
        priv_bytes,
        ws_bytes,
        handles,
        sys_avail_mb,
        sys_busy_pct,
    } = *sample;
    let busy = match sys_busy_pct {
        Some(value) => format!("{value:.1}"),
        None => "none".to_string(),
    };
    let cpu = cpu_s.map_or_else(|| "none".into(), |value| format!("{value:.2}"));
    let counter =
        |value: Option<u64>| value.map_or_else(|| "none".into(), |value| value.to_string());
    let (priv_bytes, ws_bytes, handles, sys_avail_mb) = (
        counter(priv_bytes),
        counter(ws_bytes),
        counter(handles),
        counter(sys_avail_mb),
    );
    format!("soak-resources: pid={pid} uptime_s={uptime_s} cpu_s={cpu} priv_bytes={priv_bytes} ws_bytes={ws_bytes} handles={handles} sys_avail_mb={sys_avail_mb} sys_busy_pct={busy}")
}

/// Reads the per-process counters and the system's free memory, plus the busy
/// percent already published in `snapshot`. A failed counter call yields `none`
/// for its field, never a fabricated zero. Consumers must skip invalid deltas.
fn soak_resources_sample(
    snapshot: &Snapshot,
    now: Instant,
    process_epoch: Instant,
) -> SoakResourcesSample {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount, GetProcessTimes,
    };

    let sys_busy_pct = snapshot
        .readings
        .lookup(MetricKey::CPUUtilization, "cpu-system")
        .filter(|reading| {
            reading.has_value
                && reading.availability == crate::model::Availability::Available
                && reading.freshness == crate::model::Freshness::Current
        })
        .map(|reading| reading.number)
        .filter(|value| value.is_finite());
    let pid = unsafe { GetCurrentProcessId() };
    let mut cpu_s = None;
    let mut handles = None;
    let mut priv_bytes = None;
    let mut ws_bytes = None;
    let mut sys_avail_mb = None;
    unsafe {
        let process = GetCurrentProcess();
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).is_ok() {
            let units =
                |time: &FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
            cpu_s = Some((units(&kernel) + units(&user)) as f64 / 1e7);
        }
        let mut count = 0u32;
        if GetProcessHandleCount(process, &mut count).is_ok() {
            handles = Some(u64::from(count));
        }
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        if K32GetProcessMemoryInfo(
            process,
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut _,
            counters.cb,
        )
        .as_bool()
        {
            priv_bytes = Some(counters.PrivateUsage as u64);
            ws_bytes = Some(counters.WorkingSetSize as u64);
        }
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut status).is_ok() {
            sys_avail_mb = Some(status.ullAvailPhys / 1_048_576);
        }
    }
    SoakResourcesSample {
        pid,
        uptime_s: now.duration_since(process_epoch).as_secs(),
        cpu_s,
        priv_bytes,
        ws_bytes,
        handles,
        sys_avail_mb,
        sys_busy_pct,
    }
}

fn proc_hang_owner(slots: &Manager) -> Option<usize> {
    (0..4).find(|&slot| slots.current(slot).eq_ignore_ascii_case("PROC_HANG"))
}

/// EC-4: the PROC_HANG owner is a pure function of the slot selections, so it is
/// resolved once per slot-configuration change (slot cycle/apply, config reload)
/// instead of at every loop iteration. `hang::HoldState::set_owner` emits its
/// `canceled-slot-owner` audit on a real owner change by cancelling the press
/// bound to the old owner, so this cache is not a second dedup layer: it only
/// removes the per-tick owner scan.
#[derive(Default)]
struct HangOwner {
    /// `None` = the slot selections changed since the last resolution.
    cached: Option<Option<usize>>,
    /// EC-4 seam: number of owner resolutions, i.e. of `proc_hang_owner` scans.
    /// Read only by the EC-4 tests; production never observes it.
    #[cfg(test)]
    resolutions: u64,
}

impl HangOwner {
    /// Mark the owner stale. Call after every mutation of the slot selections.
    fn invalidate(&mut self) {
        self.cached = None;
    }

    /// Bind the owner at a slot-owner site: scan and call `set_owner` only when
    /// the slot configuration changed since the last bind, so a steady tick does
    /// no scan and no `set_owner` call.
    fn bind(&mut self, slots: &Manager, hold: &mut hang::HoldState) -> Option<hang::Audit> {
        if self.cached.is_some() {
            return None;
        }
        let owner = proc_hang_owner(slots);
        self.cached = Some(owner);
        #[cfg(test)]
        {
            self.resolutions += 1;
        }
        hold.set_owner(owner)
    }
}

/// EC-3: the telemetry providers a displayed module reads. Names must match
/// `telemetry::GATED_PROVIDERS`; providers outside that list (controller, audio,
/// net, presentmon, hang, network probe) are never gated, so listing them is a
/// no-op. Unknown module names consume nothing: they render N/A.
fn module_provider_consumers(module: &str) -> &'static [&'static str] {
    match module.to_ascii_uppercase().as_str() {
        "HEADSET_BATTERY" => &["headset"],
        "CPU_TEMP" | "CPU_TEMP_GRAPH" => &["hwinfo", "lhm"],
        "GPU_TEMP" | "GPU_TEMP_GRAPH" => &["gpu", "lhm"],
        "THERMALS" => &["hwinfo", "lhm", "gpu"],
        "GPU_LOAD" | "GPU_LOAD_GRAPH" | "VRAM_USAGE" => &["gpu"],
        "POWER_LIMIT" => &["hwinfo", "lhm"],
        "CPU_GPU_POWER" => &["hwinfo", "lhm", "gpu"],
        "VRM_TEMP" | "CHIPSET_TEMP" | "MOTHERBOARD_TEMP" | "CPU_FAN" | "PUMP_RPM" => &["lhm"],
        "DISK_IO" | "DISK_IO_GRAPH" | "HARD_FAULTS" => &["performance"],
        "CONNECTIONS" => &["connections"],
        "SYSTEM_BATTERY" => &["system battery"],
        // The availability surface reads every gated provider.
        "PROVIDER_STATUS" => &crate::telemetry::GATED_PROVIDERS,
        _ => &[],
    }
}

/// EC-3 + WO-TEL-D3-B1 (L1, core tier): the gated providers the fixed main-display bars read.
/// Layout 1 (`main_display 1`, the shipped default in `config.rs`) is the `dashboard` default
/// arm in `src/render/renderer.rs` L215-232, which draws the `GPU ` and `VRAM` metric bars from
/// `MetricKey::GPUUtilization`/`VRAMUtilization`; layout 2 is `main_display_2` L270-287 with the
/// same two bars. Both are drawn in every frame of those layouts and only the native `gpu`
/// provider emits those keys, so layouts 1 and 2 consume `gpu` whatever the slots
/// show. Layout 3 (`main_display_3` L312-356) draws CPU/RAM/NET bars only.
///
/// WO-TEL-D3-B1 (L1) SUPERSEDES the accepted WO-A1-03 F-1 layout arm `3 => &[]` (prior content =
/// that arm + this doc's layout-3 sentence at `ec3df0d`; disposition =
/// `WO-TEL-D3-A-AUDIT.md` §C4 DG-1 **ACCEPTED as the directive violation** + §10 ratification
/// item 1, adopted by the manager into `WO-TEL-D3-B1.json` scope L1): GPU utilization and VRAM
/// are **CORE** readings (the user directive, `HANDOFF.md` §2A), so the `gpu` bit is demanded
/// under EVERY slot choice, layout 3 included. F-1's anti-starvation intent is preserved and
/// strengthened: the rule that kept a bar-less layout from starving a provider now keeps it from
/// starving a core reading. The 7-bit wire (`GATED_PROVIDERS`/`consumer_bit`/`ALL_CONSUMERS`),
/// `provider_consumer_bits` (the preview uses the same selected consumers), the loop gate
/// `telemetry.rs:1723`, the backoff and the park arm stay byte-identical; the `gpu` demand
/// VALUE is the whole change, so the GPU-bar protection for layouts 1/2 is untouched.
fn main_display_provider_consumers(_main_display: i32) -> &'static [&'static str] {
    &["gpu"]
}

/// EC-3: which gated providers have a consumer right now. Union of the fixed
/// main-display bars and the displayed slot modules. The preview renders the
/// same dashboard and adds no independent demand. Computed with the slot modules, not
/// per tick. WO-REMOVE-ALERTS retired the third leg, the enabled alert
/// categories, with the feature: no non-hang alarm consumer can keep a provider
/// at full cadence, and no display consumer is lost with it (the temperature,
/// memory and headset readings ride their own module arms).
fn provider_consumer_bits(modules: &[String; 4], cfg: &Config, _preview_active: bool) -> u64 {
    // The preview draws these same selected readings. It is not a subscription
    // to every optional provider, even when device loss opens it automatically.
    let mut bits = 0u64;
    for provider in main_display_provider_consumers(cfg.main_display) {
        bits |= crate::telemetry::consumer_bit(provider);
    }
    for module in modules {
        for provider in module_provider_consumers(module) {
            bits |= crate::telemetry::consumer_bit(provider);
        }
    }
    bits
}

/// EC-3 consumer surface: the preview window shows the same slots, so treat it as
/// another presentation of those readings, not an all-provider subscription.
fn preview_surface_active(preview_always: bool, cfg: &Config, state: &BackendState) -> bool {
    preview_always || preview_for_backend(cfg, state)
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
        let inactive_dynamic =
            selected.eq_ignore_ascii_case("PROC_HANG") && snapshot.hung.is_empty();
        let inactive_presentmon = cfg.presentmon_deferred
            && presentmon_available(&selected).is_some_and(|available| !available);
        if !inactive_dynamic && !inactive_presentmon {
            return selected;
        }

        let mut excluded = vec!["PROC_HANG".to_string(), selected];
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

/// WO-TEL-D2 (S1): the reading-keyed demand registry for the metering providers
/// EC-3 never gated. Keys are the displayed-reading module names (the config
/// slot-list vocabulary); values are provider names in `telemetry.rs`
/// `EXTENDED_CONSUMER_PROVIDERS`, occupying the extended bit range of the SAME
/// published consumer word (bits above the shipped 7). Demand is the module the
/// panel effectively shows (`effective_slot_modules`, fall-through included: a
/// slot displaying a placeholder consumes nothing behind it; slot alternatives
/// reachable only by cycling are demand-on-switch). WO-REMOVE-ALERTS retired the
/// alert-category note with the feature: no non-hang alarm demand ever entered
/// this word, and the retired gated-word alert leg is gone with it.
fn module_extended_consumers(module: &str) -> &'static [&'static str] {
    match module.to_ascii_uppercase().as_str() {
        "AUDIO" | "MIC_STATUS" => &["audio"],
        "CONTROLLER_BATTERY" => &["controller"],
        "NET_IN" | "NET_OUT" | "NET_BOTH" | "NET_IN_GRAPH" | "NET_OUT_GRAPH" | "NET_GRAPH" => {
            &["net"]
        }
        // The presentmon keys include the graph slots: `renderer.rs:87` ->
        // `capture()` L117-118 stores `s.game.frame_time_ms`/`s.game.fps` into
        // the graph history every render, and `FPS_GRAPH`/`FRAME_TIME` consume
        // that history, so displaying them demands collection (U-3 structural
        // reader modelled as a display consumer of the published fields).
        "FPS_CURRENT" | "FPS_1LOW" | "FPS_01LOW" | "FPS_GRAPH" | "FRAME_TIME" | "SESSION_TIME"
        | "SESSION_SUMMARY" | "GAME_NAME" => &["presentmon"],
        _ => &[],
    }
}

/// WO-TEL-D2 (S1): the fixed-bar layout rule for the extended word. Layout 1
/// draws `CPU`/`RAM`/`GPU `/`VRAM` (`GPU `/`VRAM` come from the gated `gpu`
/// provider, consumed by the shipped `main_display_provider_consumers`);
/// layouts 2 and 3 also draw the `NET IN`/`NET OUT` bars
/// (`renderer.rs:289-311`, `renderer.rs:335-356`), which read the netif
/// metrics, so they consume `net`.
fn main_display_extended_consumers(main_display: i32) -> &'static [&'static str] {
    match main_display {
        1 => &[],
        _ => &["net"],
    }
}

/// WO-TEL-D2 (S1): the extended demand word, published on the existing path
/// (`app.rs` seed and render tick) beside the unchanged gated word. The preview
/// keeps consuming every GATED provider (`ALL_CONSUMERS` early-return is the
/// ratified U-9 preserve; a visible preview shows the slots, and the extended
/// word derives from those same displayed modules, so the preview consumes
/// every displayed reading through both words together). `PROVIDER_STATUS` keeps
/// the shipped gated-only mapping: health rows and the
/// presentmon availability predicate (`app.rs:745-763`) are fed by publication
/// (health + staleness stamps), not by fresh polls, per WO-TEL-D1-AUDIT
/// amendment 2.
///
/// WO-TEL-D2-FIX (F-AUD-1): the presentmon bit is the one derivation that does
/// NOT ride the fall-through display. The fall-through is driven by the health
/// row the park itself publishes (`providers/presentmon.rs:1455-1463` =
/// `stop_and_report_cleanup` + `unavailable("no display consumer")`), so
/// deriving this bit from `modules` self-locks the gate: with
/// `presentmon_deferred 1` (the `config.rs:299` default) a selected
/// `FPS_CURRENT`/`FRAME_TIME`/`GAME_NAME` module would never re-arm the 1 s
/// target poll, and a game launch would stay undetectable for the session. The
/// bit therefore rides the RAW current selection (`slots.current`), which keeps
/// the ratified rule that slot ALTERNATIVES are demand-on-select and keeps the
/// U-3 saving for every shape with no presentmon-backed module selected. Every
/// other provider's derivation below is byte-untouched.
fn provider_demand_bits(slots: &Manager, modules: &[String; 4], cfg: &Config) -> u64 {
    let mut bits = 0u64;
    for provider in main_display_extended_consumers(cfg.main_display) {
        bits |= crate::telemetry::extended_consumer_bit(provider);
    }
    for module in modules {
        for provider in module_extended_consumers(module) {
            bits |= crate::telemetry::extended_consumer_bit(provider);
        }
    }
    if cfg.presentmon_enabled && !cfg.safe_mode && cfg.presentmon_target_mode != "disabled" {
        for slot in 0..4 {
            if module_extended_consumers(&slots.current(slot)).contains(&"presentmon") {
                bits |= crate::telemetry::extended_consumer_bit("presentmon");
            }
        }
        // WO-U46C (PR review P2): the same guard also covers the PENDING
        // fallbacks of the chain walk, so a slot the fall-through is serving
        // cannot park the collection its own chain is waiting for.
        if pending_presentmon_fallback_demand(slots, modules) {
            bits |= crate::telemetry::extended_consumer_bit("presentmon");
        }
    }
    bits
}

/// WO-U46C (PR review P2, "dynamic fallback selection can leave PresentMon
/// permanently parked"): a PENDING, REACHABLE PresentMon-backed dynamic fallback
/// is a consumer the panel is awaiting, so it demands collection.
/// `effective_slot_modules` walks a slot's chain and displays the first module
/// that resolves (`app.rs:786-798`); every PresentMon-backed module the walk
/// skipped is a fallback the panel shows the moment FPS becomes current.
/// Deriving the bit from the displayed module and the raw selection alone
/// self-locks exactly the slot the fall-through is serving: with
/// `PROC_HANG FPS_CURRENT GPU_TEMP`, no hung target and no FPS, the panel shows
/// `GPU_TEMP` (no consumer) and `PROC_HANG` is selected (no consumer), so
/// nothing is collected, so FPS never arrives, so the slot never leaves
/// `GPU_TEMP`. The walk reproduces the shipped chain order (`slots.next_except`,
/// `app.rs:789`) and STOPS at the module the panel landed on, so a PresentMon
/// module that is only an ALTERNATIVE behind a displayable selection stays
/// demand-on-select (the ratified rule: no eager collection), a chain with no
/// PresentMon-backed module never sets the bit, and the demand releases - the
/// park resumes - as soon as no panel displays, selects or awaits one. The
/// caller keeps the shipped enablement guard, so `presentmon_enabled 0`,
/// `safe_mode` and `presentmon_target_mode disabled` never collect, and the
/// `effective_slot_modules` gating contract (`app.rs:752-759`) is untouched.
fn pending_presentmon_fallback_demand(slots: &Manager, modules: &[String; 4]) -> bool {
    for (slot, displayed) in modules.iter().enumerate() {
        let selected = slots.current(slot);
        if displayed.eq_ignore_ascii_case(&selected) {
            continue; // the panel shows its own selection: nothing is pending
        }
        let mut excluded = vec![selected.clone()];
        loop {
            let excluded_refs: Vec<_> = excluded.iter().map(String::as_str).collect();
            let Some(candidate) = slots.next_except(slot, &excluded_refs) else {
                break;
            };
            if candidate.eq_ignore_ascii_case(displayed) {
                break; // the chain ends where the panel landed
            }
            if module_extended_consumers(&candidate).contains(&"presentmon") {
                return true;
            }
            excluded.push(candidate);
        }
    }
    false
}

fn discord_display_demand(modules: &[String; 4]) -> bool {
    modules
        .iter()
        .any(|module| module.eq_ignore_ascii_case("DISCORD"))
}

/// Reuse the existing enable/disable lifecycle; allocate a config only at a
/// selection edge or reload, never per tick. The user's stored config is unchanged.
fn discord_runtime_config(cfg: &Config, demanded: bool) -> Config {
    let mut effective = cfg.clone();
    effective.discord_enabled &= demanded;
    effective
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

/// RC-5: the single device state published on the human-facing surface.
/// Terminal recovery outcomes (c)/(d) outrank the transient loss labels, so a
/// spent allowance is never masked by a fresh disconnect line; (a)/(b) come
/// from the accepted `Absent` terminal and the RC-1 cause code. The transport
/// `reason` free text never reaches the label.
fn published_device_status(state: &BackendState, recovery: &DeviceStatus) -> DeviceStatus {
    match recovery {
        DeviceStatus::AllowanceConsumed | DeviceStatus::RecoveryFailed { .. } => recovery.clone(),
        _ => match state {
            BackendState::Absent { .. } => DeviceStatus::Absent,
            BackendState::Disconnected { cause, .. } => DeviceStatus::LinkFailed {
                code: cause.short(),
            },
            _ => DeviceStatus::Healthy,
        },
    }
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
    let snapshot = build_snapshot(
        &cfg,
        &topology,
        &mut cpu_provider,
        &crate::telemetry::Update::default(),
        &DiscordState::default(),
    );
    let dashboard = crate::render::renderer::Renderer::new().render(
        &snapshot,
        OverlayOptions {
            main_display: cfg.main_display,
            cpu_temp_max_c: cfg.cpu_temp_max_c,
            gpu_temp_max_c: cfg.gpu_temp_max_c,
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
                hardware_test::record_final_error(&mut results, "SDK shutdown", error.to_string());
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
                hardware_test::record_final_error(&mut results, "HID shutdown", error.to_string());
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

fn build_snapshot(
    cfg: &Config,
    topology: &ccd::CcdTopology,
    cpu_provider: &mut cpu::CpuLoadProvider,
    telemetry: &crate::telemetry::Update,
    discord: &DiscordState,
) -> Snapshot {
    let now = SystemTime::now();
    let clock = clock::read(&cfg.date_format);
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
        cpu_cache_load: cpu_split_metric(!per_lp.is_empty(), cache_load, now),
        cpu_freq_load: cpu_split_metric(!per_lp.is_empty(), freq_load, now),
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

/// F-EC5-a parity: the CCD Cache/Frequency split carries the same no-data
/// discipline as the CPU bar. With per-logical-processor samples the aggregate
/// value passes through untouched as `Metric::valid(value, now)`; with none
/// (Nt-query failure or decode rejection -> empty `per_lp`) the Metric is
/// `Metric::invalid()`, so the renderer draws its unavailable placeholder
/// instead of a current 0% bar. No new mechanism: `Metric::invalid()` is the
/// one the CPU-bar path already uses (`canonical_percent_metric`).
fn cpu_split_metric(has_data: bool, value: f64, now: SystemTime) -> Metric {
    if has_data {
        Metric::valid(value, now)
    } else {
        Metric::invalid()
    }
}

fn log_hang_audit(audit: &hang::Audit) {
    // TC-3 (P2 §2 TC-3): the line text (exit-code classification included) is
    // rendered by the provider, where the classification is produced.
    crate::log_warn!("{}", audit.line());
}

fn apply_hold_command(
    command: hang::HoldCommand,
    hold: &mut hang::HoldState,
    slots: &mut Manager,
    cfg: &Config,
    owner: &mut HangOwner,
) {
    if apply_hold_command_with(command, hold, slots, cfg, hang::terminate_bound_target) {
        owner.invalidate();
    }
}

fn apply_hold_command_with<F>(
    command: hang::HoldCommand,
    hold: &mut hang::HoldState,
    slots: &mut Manager,
    cfg: &Config,
    terminate: F,
) -> bool
where
    F: FnOnce(&crate::model::HungTarget, &Config) -> hang::ActionReport,
{
    match command {
        hang::HoldCommand::None => {}
        hang::HoldCommand::Cycle { index, backward } => {
            // WO-REMOVE-ALERTS: the retired acknowledgement detour is gone, so a release on
            // any button, slot 4 (index 3) included, is the ordinary slot cycle.
            let module = slots.cycle(index, if backward { -1 } else { 1 });
            crate::log_debug!("button {} -> slot module {}", index + 1, module);
            return index < 4;
        }
        hang::HoldCommand::Audit(audit) => log_hang_audit(&audit),
        hang::HoldCommand::Terminate(target) => {
            if cfg.safe_mode {
                log_hang_audit(&hang::Audit::raised(target, "refused-safe-mode"));
                return false;
            }
            log_hang_audit(&hang::Audit::raised(target.clone(), "attempted"));
            let report = terminate(&target, cfg);
            log_hang_audit(&hang::Audit {
                target: target.clone(),
                outcome: report.outcome.label(),
                exit: report.exit,
            });
            // TC-3 (P2 §2 TC-3): a `wait-timeout` is terminal for the published
            // identity this press acted on; the provider owns the rule, the app only
            // reports what its own action returned.
            hold.report_action(&target, report.outcome);
        }
    }
    false
}

/// WO-U46H #2: the cadence seed seam. The shipped seed `Instant::now() -
/// interval` panics when the monotonic clock's base is closer than the
/// interval, so the initial state is the explicit never-stamped marker `None`
/// and no subtraction happens at all. The parameters keep the shipped call
/// shape; nothing about the clock base may enter the decision.
fn cadence_initial_state(_now: Instant, _interval: Duration) -> Option<Instant> {
    None
}

/// WO-U46H #2: the cadence boundary in `Duration` space, so the seam test is
/// exact with no clock framework and no boot-time dependence: never stamped is
/// due at once (the first collection and the first render stay immediate), a
/// stamp is due once `elapsed >= interval`.
fn cadence_due(elapsed: Option<Duration>, interval: Duration) -> bool {
    elapsed.is_none_or(|elapsed| elapsed >= interval)
}

/// WO-U46H #4: one reload-rejection row per rejection identity - true (log the
/// row) when `identity` differs from the last logged one, and record it; the
/// caller clears the state on a successful reload. Bounded: a single
/// `Option<String>` replaced in place, so no failure collection grows.
fn reload_rejection_is_new(last: &mut Option<String>, identity: &str) -> bool {
    if last.as_deref() == Some(identity) {
        return false;
    }
    *last = Some(identity.to_string());
    true
}

/// WO-U46H #14: the bounded content recheck period, in refresh sweeps. Between
/// full rechecks a sweep is metadata-only, so a write that keeps BOTH size and
/// mtime is found by the recheck, never later.
const WATCHER_FULL_RECHECK_CHECKS: u32 = 16;

/// WO-U46H #9 + #14, WO-OPT01 (C1 + O7): the watched-file watcher.
///
/// Cost (#14): a refresh sweep calls `fs::metadata` per watched file while the
/// stored size and mtime still match - no read, no SHA-256 - instead of hashing
/// the whole include graph every tick on the render loop. A complete signature
/// (content digest) is taken when metadata moves, which is also what separates
/// a touch from an edit, on every `WATCHER_FULL_RECHECK_CHECKS`-th sweep, and
/// while any path is pending.
///
/// Detection latency: an ordinary edit is seen on the sweep after the write, so
/// <= one `config_refresh_ms` (the legal 100..60000). A metadata-invisible
/// write (same size AND same mtime) is seen at the next full recheck, so <=
/// `WATCHER_FULL_RECHECK_CHECKS * config_refresh_ms` (1.6 s at the 100 ms
/// floor, 16 min at the 60 s ceiling). That bound is also the repair guarantee
/// for the O7 quiescence below: an unchanged rejected version is deliberately
/// not reparsed, so a metadata-colliding repair lands at the next full recheck
/// and never later. An edit that lands after detection is applied on the next
/// sweep, because a newly adopted dependency is pending (C1).
///
/// Integrity (C1): `install` records the signature of the bytes the parse
/// consumed (`LoadedConfig::signatures`) and performs no read of its own, so a
/// version can never be recorded as applied unparsed and no post-parse rehash
/// exists. A dependency with no detection-time signature (new to the graph,
/// recreated, missing -> present) is recorded as PENDING, so the next sweep
/// takes a full content signature of every watched file.
///
/// Quiescence (O7): `rejected` is the content identity of the version the last
/// parse rejected, kept apart from the applied baseline in `files`. A sweep that
/// observes exactly that version returns `None`, so an unchanged rejected config
/// is never reparsed and never re-logs. A byte change (even with an identical
/// mtime and length, at the next full recheck), a graph change, a missing
/// dependency that appears, and a recurrence after a success all produce a
/// different observed version and are detected.
///
/// Consumed graph (WO-OPT01R, F-AUD-OPT01-1): `files` is the watched set - the
/// applied graph plus the paths a rejection NAMED - and a parse can consume more
/// than that (an intermediate include that no error named). `consumed` records
/// the signature of every path the last parse attempt consumed, and on the
/// periodic full recheck, while a rejection stands and the observed content is
/// the standing rejected version, the sweep compares that whole consumed graph
/// against those recorded signatures and reports a change for any difference.
/// that closes the quiet-and-lost shape for a repair performed in a consumed,
/// unwatched file while the named files stay byte-identical. Cost, measured by T9
/// (`a_standing_rejection_of_a_present_named_file_keeps_the_recheck_cadence`): a
/// stable graph under a standing rejection performs NO extra parse at all (T9 legA:
/// `fires=0 window_parses=0` over 64 sweeps, `uopt1r5-green-g2.log:8`), and a graph
/// that changes once per cycle performs at most one parse per
/// `WATCHER_FULL_RECHECK_CHECKS` sweeps (T9 legB: `fires=4 window_parses=4` over 64
/// sweeps against the bound 4 + 1, `uopt1r5-green-g2.log:9`). The content reads ride
/// the sweep cadence that already exists, and the accepted (no-rejection) path is
/// untouched. Residual, stated precisely: a file the parser never reached and no
/// error named is in neither set, so a change confined to such a file is picked up
/// when a parse next reaches it - that is, when any watched or consumed path
/// changes, which re-runs the parse and re-arms both sets (a change is never
/// adopted, so the named version keeps re-detecting). Until then the last valid
/// configuration stays active, and nothing unparsed is ever recorded as applied.
struct ConfigWatcher {
    files: Vec<(std::path::PathBuf, Option<FileSignature>)>,
    pending: Vec<std::path::PathBuf>,
    consumed: Vec<(std::path::PathBuf, FileSignature)>,
    rejected: Option<String>,
    checks: u32,
    pub last_check: Instant,
}

impl ConfigWatcher {
    /// Seed the watcher for a graph that was just parsed, from the signatures of
    /// the bytes that parse consumed (C1), so a write landing between the
    /// startup parse and the seeding cannot be recorded as applied unparsed.
    fn new(paths: &[std::path::PathBuf], parsed: &[(std::path::PathBuf, FileSignature)]) -> Self {
        ConfigWatcher {
            files: paths
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        parsed
                            .iter()
                            .find(|(seen, _)| seen == path)
                            .map(|(_, signature)| signature.clone()),
                    )
                })
                .collect(),
            pending: Vec::new(),
            consumed: parsed.to_vec(),
            rejected: None,
            checks: 0,
            last_check: Instant::now(),
        }
    }

    /// One refresh sweep: `Some(snapshot)` when a watched file's content differs
    /// from the recorded baseline AND the observed content is not the version
    /// this watcher last rejected, `None` when the graph is as recorded or is an
    /// unchanged repeat of a rejection. A metadata move with an unchanged digest
    /// is a touch, not a change, so its stamp is adopted and the next sweep is
    /// cheap again. A change is NOT adopted: a rejected reload keeps a repair
    /// detected.
    fn detect(&mut self) -> Option<Vec<(std::path::PathBuf, Option<FileSignature>)>> {
        let full = self.checks % WATCHER_FULL_RECHECK_CHECKS == WATCHER_FULL_RECHECK_CHECKS - 1
            // C1: a dependency adopted without a detection-time signature gets a
            // full content check on the very next sweep, not at the recheck.
            || !self.pending.is_empty();
        self.checks = self.checks.wrapping_add(1);
        let mut changed = false;
        let mut snapshot = Vec::with_capacity(self.files.len());
        for index in 0..self.files.len() {
            let path = self.files[index].0.clone();
            let stored = self.files[index].1.clone();
            let observed = sweep_signature(&path, stored.as_ref(), full);
            let is_change = match (&observed, &stored) {
                (None, None) => false,
                (Some(observed), Some(stored)) => observed.digest != stored.digest,
                _ => true,
            };
            if is_change {
                changed = true;
            } else {
                self.files[index].1 = observed.clone();
            }
            snapshot.push((path, observed));
        }
        self.pending.clear();
        // O7: the very bytes that were rejected a moment ago are a known
        // rejection, so this sweep performs no parse and emits no row. A sweep
        // with no change at all is the same case.
        let repeat =
            !changed || self.rejected.as_deref() == Some(observed_version(&snapshot).as_str());
        if repeat {
            // WO-OPT01R (F-AUD-OPT01-1): while a rejection stands, the periodic
            // full recheck re-verifies the WHOLE graph that parse attempt
            // consumed, not only the watched/named set, so a repair performed in
            // a consumed intermediate include that no error named re-fires
            // detection. Only a full sweep reads those paths: no extra work on the
            // other sweeps, and no read at all on the accepted path. The rate is
            // the one T9 measures - zero fires on a stable graph
            // (`uopt1r5-green-g2.log:8`), one fire per
            // `WATCHER_FULL_RECHECK_CHECKS`-sweep cycle when the graph changes each
            // cycle (`uopt1r5-green-g2.log:9`, 4 fires / 64 against bound 5).
            if full && self.consumed_changed() {
                return Some(snapshot);
            }
            return None;
        }
        Some(snapshot)
    }

    /// WO-OPT01R (F-AUD-OPT01-1): did any file the last parse attempt consume
    /// change since that attempt? `consumed` holds the signatures the parser
    /// produced from its own bytes, so the comparison baseline is recorded state,
    /// never a per-tick rehash. It is called only on a full-recheck sweep, and
    /// only while a rejection stands: the accepted path detects every content
    /// change through `files` already, and its cadence must stay unchanged.
    fn consumed_changed(&self) -> bool {
        if self.rejected.is_none() {
            return false;
        }
        self.consumed.iter().any(|(path, signature)| {
            file_signature(path).is_none_or(|observed| observed.digest != signature.digest)
        })
    }

    /// Record the reloaded include graph from the bytes the parse consumed; a
    /// dependency the detection sweep did not see becomes pending.
    fn install(
        &mut self,
        paths: &[std::path::PathBuf],
        snapshot: &[(std::path::PathBuf, Option<FileSignature>)],
        parsed: &[(std::path::PathBuf, FileSignature)],
    ) {
        let mut files = Vec::with_capacity(paths.len());
        let mut pending = Vec::new();
        for path in paths {
            // C1: the recorded signature is the one produced from the bytes this
            // parse consumed. Nothing is read here, so a write that lands after
            // the parse cannot be recorded as applied.
            let signature = parsed
                .iter()
                .find(|(seen, _)| seen == path)
                .map(|(_, signature)| signature.clone());
            if !matches!(
                snapshot.iter().find(|(seen, _)| seen == path),
                Some((_, Some(_)))
            ) {
                pending.push(path.clone());
            }
            files.push((path.clone(), signature));
        }
        self.files = files;
        self.pending = pending;
        self.checks = 0;
        // WO-OPT01R (F-AUD-OPT01-1): the applied graph is the consumed graph.
        self.consumed = parsed.to_vec();
        // O7: the success ends the rejection. The state is one String, cleared
        // in place, so any later rejection - even the identical one - is a new
        // observed version and is detected.
        self.rejected = None;
    }

    /// WO-OPT01 (O7 + C1): the parse rejected the observed version. Record its
    /// content identity, so an unchanged repeat stays quiet, and watch the file
    /// the parse named (`ParseError::file`) as a dependency that is currently
    /// unseen, so a file that is absent now is detected the moment it appears.
    /// WO-OPT01R5 (F-AUD-OPT01R-1): the named path is recorded with the signature
    /// the filesystem actually holds - its digest when the file EXISTS (the case T9
    /// measures), `None` only when it cannot be read - so the watched baseline and
    /// the content identity state the same thing and cannot oscillate.
    /// WO-OPT01R: `consumed` is the full graph that attempt read - root plus every
    /// include, named or not - recorded as the signatures the parser produced from
    /// its own bytes, so the bounded full recheck can re-verify all of it. Nothing
    /// from the rejected bytes is adopted: the applied baseline and the last valid
    /// configuration are untouched.
    fn record_rejection(
        &mut self,
        observed: &[(std::path::PathBuf, Option<FileSignature>)],
        failed: &str,
        consumed: &[(std::path::PathBuf, FileSignature)],
    ) {
        self.consumed = consumed.to_vec();
        let path = std::path::PathBuf::from(failed);
        // Bounded: the parser refuses a graph larger than MAX_CONFIG_FILES, so
        // watching more failed paths than that could never lead to a load.
        let added = self.files.len() < crate::config::MAX_CONFIG_FILES
            && !self.files.iter().any(|(seen, _)| *seen == path);
        // WO-OPT01R5 (F-AUD-OPT01R-1, T9): the named path may EXIST on disk - the
        // file the parse opened and failed on. Recording it as `absent` in the
        // watched baseline while a full sweep reports its DIGEST makes the pair
        // oscillate, and the watcher fires twice per recheck cycle. Record what is
        // really there: the sweep's own observation when it covers the path, else
        // one content read here, at rejection time (at most once per rejection,
        // never per sweep). An unreadable path still gets `None`, so the
        // absent-dependency cadence measured by T8 is unchanged, and the accepted
        // path performs no read at all.
        let mut version = observed.to_vec();
        if added {
            let signature = observed
                .iter()
                .find(|(seen, _)| *seen == path)
                .and_then(|(_, seen)| seen.clone())
                .or_else(|| sweep_signature(&path, None, true));
            self.files.push((path.clone(), signature.clone()));
            version.push((path, signature));
        }
        self.rejected = Some(observed_version(&version));
    }
}

/// WO-OPT01 (O7): the content identity of one observed graph - `path=digest` per
/// watched file plus an explicit `absent` marker for a file that could not be
/// read. It is the content signature, never a timestamp, so a rewrite that keeps
/// mtime and length still changes it. A byte change, a graph change and a
/// missing file that appears all produce a different key, so the rejection
/// dedup never hides a repair. Bounded: one String per watcher, replaced in
/// place, sized by the watched graph: at most `MAX_CONFIG_FILES` entries, plus the
/// one path a rejection names that is not already watched, so the observed ceiling
/// is `MAX_CONFIG_FILES + 1` = 65 (`MAX_CONFIG_FILES` = 64, `config.rs:16`).
fn observed_version(observed: &[(std::path::PathBuf, Option<FileSignature>)]) -> String {
    observed
        .iter()
        .map(|(path, signature)| match signature {
            Some(signature) => format!("{}={}", path.display(), signature.digest),
            None => format!("{}=absent", path.display()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The sweep signature of one path: the stored signature while its metadata
/// still matches and no full recheck is due, the complete content signature
/// otherwise.
///
/// WO-OPT01R (F-AUD-OPT01-2): a path with NO stored baseline is a file a parse
/// could not read (an absent dependency a rejection named). Its metadata can never
/// confirm, so before this unit that entry was read AND hashed on EVERY sweep of a
/// rejected window. It is now evaluated only at the full-recheck cadence, so the
/// absent -> present transition is detected within
/// `WATCHER_FULL_RECHECK_CHECKS * config_refresh_ms` (1.6 s at the 100 ms floor,
/// 16 min at the 60 s ceiling) - the same documented bound as a metadata-colliding
/// write. T8 measures it (32 sweeps of a rejected window over 2 watched paths:
/// 64 sweep reads before, 36 after; the None-baseline entry 32 -> 2), and the fix of
/// F-AUD-OPT01R-1 left those rows exactly as measured
/// (`uopt1r5-green-g2.log:22` `total_reads=36 baseline_reads=2 parses=0`).
/// WO-OPT01R5: a named path that EXISTS is recorded with its real digest at
/// rejection time, so it joins the metadata fast path like every other watched file
/// and its repair lands on the sweep AFTER the write, not at the recheck; `None`
/// stays the marker of a path that cannot be read, which is the only case this
/// paragraph describes.
fn sweep_signature(
    path: &std::path::Path,
    stored: Option<&FileSignature>,
    full: bool,
) -> Option<FileSignature> {
    if !full {
        let Some(stored) = stored else {
            // Known absent: report absent without touching the filesystem.
            return None;
        };
        if let Ok(metadata) = std::fs::metadata(path) {
            if stored.len == metadata.len() && metadata.modified().ok() == stored.modified {
                return Some(stored.clone());
            }
        }
    }
    #[cfg(test)]
    {
        if stored.is_none() {
            BASELINE_READS.set(BASELINE_READS.get() + 1);
        }
    }
    file_signature(path)
}

// WO-OPT01R (T8): the read census of the watcher's own content reads, the
// `#[cfg(test)]` seam in the shipped `parser::LOAD_CALLS` form. Production code
// is untouched; T8 bounds the sweep reads of a rejected window with it.
// `BASELINE_READS` attributes the reads of entries with NO stored baseline (the
// audited F-AUD-OPT01-2 entry) so its bound is measured, not asserted in prose.
#[cfg(test)]
thread_local! {
    static SWEEP_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BASELINE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn test_file_signature_calls() -> usize {
    SWEEP_READS.get()
}

#[cfg(test)]
pub fn test_baseline_read_calls() -> usize {
    BASELINE_READS.get()
}

/// The watcher's own signature of one path (its detection-time baseline): one
/// read of the bytes plus the metadata, signed by the shared producer
/// `FileSignature::of_bytes`. The signature `install` records as applied comes
/// from the PARSER's read (C1); this one is only the baseline that change
/// detection compares against.
fn file_signature(path: &std::path::Path) -> Option<FileSignature> {
    #[cfg(test)]
    SWEEP_READS.set(SWEEP_READS.get() + 1);
    FileSignature::of_bytes(path, &std::fs::read(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WO-REMOVE-ALERTS (S5, "no ordinary alerts"): extreme CPU/GPU temperature, RAM, VRAM and
    /// headset-battery samples still publish the correct ordinary readings, and the retired
    /// evaluator/episode/linger/flash/acknowledgement machinery cannot be observed because it
    /// is deleted: `Snapshot` has no `alerts` and no `bottleneck` member, and no render option
    /// can request a pane flash. A repeated identical sample publishes the same values, so no
    /// dormant state can reappear after a reload.
    #[test]
    fn extreme_readings_publish_ordinary_values_and_no_alarm_surface() {
        let at = SystemTime::UNIX_EPOCH;
        let telemetry = crate::telemetry::Update {
            generation: 1,
            cpu_temp: Metric::valid(150.0, at),
            gpu_temp: Metric::valid(120.0, at),
            headset: crate::model::HeadsetBattery {
                present: true,
                online: true,
                percent: 1,
                ..Default::default()
            },
            gpu_readings: vec![
                Reading::current_percent(MetricKey::GPUUtilization, 99.0, "gpu0", at),
                Reading::current_percent(MetricKey::VRAMUtilization, 100.0, "gpu0", at),
            ],
            extended_readings: vec![Reading::current_percent(
                MetricKey::RAMUtilization,
                100.0,
                "ext-ram",
                at,
            )],
            ..Default::default()
        };
        let cfg = Config {
            ccd_source: "manual".into(),
            ccd_cache_processors: Some(vec![0]),
            ccd_frequency_processors: Some(vec![1]),
            ..Config::default()
        };
        let topology = resolve_ccd_topology(&cfg);
        let mut cpu = cpu::CpuLoadProvider::new();
        let snapshot = build_snapshot(
            &cfg,
            &topology,
            &mut cpu,
            &telemetry,
            &DiscordState::default(),
        );
        assert!(snapshot.cpu_temp.valid && !snapshot.cpu_temp.stale);
        assert_eq!(snapshot.cpu_temp.value, 150.0);
        assert_eq!(snapshot.gpu_temp.value, 120.0);
        assert_eq!(
            snapshot
                .readings
                .lookup(MetricKey::RAMUtilization, "")
                .map(|reading| reading.number),
            snapshot
                .readings
                .lookup(MetricKey::RAMUtilization, "physical-memory")
                .map(|reading| reading.number),
            "the core RAM reading keeps its canonical first position"
        );
        assert_eq!(
            snapshot
                .readings
                .lookup(MetricKey::RAMUtilization, "ext-ram")
                .map(|reading| reading.number),
            Some(100.0)
        );
        assert_eq!(
            snapshot
                .readings
                .lookup(MetricKey::VRAMUtilization, "")
                .map(|reading| reading.number),
            Some(100.0)
        );
        assert!(snapshot.headset.present && snapshot.headset.online);
        assert_eq!(snapshot.headset.percent, 1);

        let mut cpu2 = cpu::CpuLoadProvider::new();
        let again = build_snapshot(
            &cfg,
            &topology,
            &mut cpu2,
            &telemetry,
            &DiscordState::default(),
        );
        assert_eq!(again.cpu_temp.value, snapshot.cpu_temp.value);
        assert_eq!(again.gpu_temp.value, snapshot.gpu_temp.value);
        assert_eq!(again.headset.percent, snapshot.headset.percent);
    }

    /// WO-REMOVE-ALERTS (S5, "no alert-only polling"): the demand word is derived from the
    /// layout rule, the displayed modules and the preview only. The retired alert category leg,
    /// the `BOTTLENECK` consumer arm and the `ALERTS` all-gated arm are gone, so a panel that
    /// shows no optional reading leaves every optional provider quiescent, and a retired module
    /// name consumes nothing.
    #[test]
    fn provider_consumer_bits_carries_no_alert_or_bottleneck_demand() {
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        let plain = Config {
            main_display: 3,
            headset_enabled: true,
            lhm_mode: "auto".into(),
            ..Config::default()
        };
        let gpu_bit = crate::telemetry::consumer_bit("gpu");
        assert_ne!(gpu_bit, 0, "gpu is a gated core provider");
        assert_eq!(
            provider_consumer_bits(&idle, &plain, false),
            gpu_bit,
            "an idle panel demands no optional provider: nothing polls for a deleted alarm"
        );
        let ghost = slots_showing(["BOTTLENECK", "ALERTS", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&ghost, &plain, false),
            gpu_bit,
            "the retired module names consume no provider at all"
        );
        assert!(
            !crate::config::valid_module("ALERTS") && !crate::config::valid_module("BOTTLENECK"),
            "the retired modules are not selectable, so no configuration can re-arm their demand"
        );
        assert_eq!(
            provider_demand_bits(&slots_current(&ghost), &ghost, &plain),
            provider_demand_bits(&slots_current(&idle), &idle, &plain),
            "the extended word ignores the retired names too"
        );
        assert_eq!(
            provider_consumer_bits(&idle, &plain, true),
            crate::telemetry::consumer_bit("gpu"),
            "a visible preview keeps the shipped prompt-consumer rule"
        );
    }

    /// WO-REMOVE-ALERTS (S5, "buttons and selected readings"): the shared routing helper is the
    /// production path (`run()` calls `apply_hold_command`, which forwards to
    /// `apply_hold_command_with`). The physical release on button 4 (index 3) performs the
    /// ordinary cycle, and the same input never reaches a terminator. The preview route is the
    /// `UiEvent::SlotCycle` arm in `run()`, which now calls the same `slots.cycle` with no
    /// acknowledgement branch (proven by the deleted branch in this order's diff).
    #[test]
    fn physical_cycle_on_slot_four_cycles_the_slot_and_never_terminates() {
        use std::cell::Cell;

        let cfg = Config::default();
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut hold = hang::HoldState::default();
        let kills = Cell::new(0);
        let before = slots.current(3);
        apply_hold_command_with(
            hang::HoldCommand::Cycle {
                index: 3,
                backward: false,
            },
            &mut hold,
            &mut slots,
            &cfg,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionReport::bare(hang::ActionOutcome::Success)
            },
        );
        assert_ne!(slots.current(3), before, "button 4 cycles slot 4 normally");
        assert_eq!(kills.get(), 0, "the cycle input never reaches a terminator");
        assert_eq!(
            slots.current(3),
            "POWER_LIMIT",
            "the migrated default slot 4 is THERMALS POWER_LIMIT"
        );

        let mut preview_slots = Manager::new(&cfg, [0; 4]);
        let first = preview_slots.current(3);
        preview_slots.cycle(3, 1);
        assert_eq!(
            preview_slots.current(3),
            slots.current(3),
            "the preview cycle route lands on the same module as the physical route"
        );
        preview_slots.cycle(3, -1);
        assert_eq!(preview_slots.current(3), first, "and it cycles back");
    }

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
                cause: LossCause::DeviceAbsent,
                reason: "test".into()
            }
        ));
        assert!(
            preview_for_backend(
                &cfg,
                &BackendState::Absent {
                    reason: "test".into()
                }
            ),
            "the ladder terminal keeps the preview visible like a disconnect"
        );
        cfg.preview_mode = "never".into();
        assert!(!preview_for_backend(&cfg, &BackendState::Discovering));
        cfg.preview_mode = "always".into();
        assert!(preview_for_backend(&cfg, &BackendState::Discovering));
    }

    /// RC-5 acceptance (1): four synthetic scenarios publish four distinct
    /// labels on the display surface. The transport `reason` free text carries
    /// a sentinel here; it must never reach any published label.
    #[test]
    fn four_device_states_publish_four_distinct_labels() {
        let free_text = "transport prose 0xDEADBEEF";
        let scenarios = [
            // (a) device absent: the RC-6 ladder terminal.
            published_device_status(
                &BackendState::Absent {
                    reason: free_text.into(),
                },
                &DeviceStatus::Healthy,
            ),
            // (b) device present, connection failed: RC-1 cause + code.
            published_device_status(
                &BackendState::Disconnected {
                    cause: LossCause::OpenFailed { code: 0x8007_0002 },
                    reason: free_text.into(),
                },
                &DeviceStatus::Healthy,
            ),
            // (c) recovery allowance consumed, published over the live loss.
            published_device_status(
                &BackendState::Disconnected {
                    cause: LossCause::CallTimeout,
                    reason: free_text.into(),
                },
                &DeviceStatus::AllowanceConsumed,
            ),
            // (d) recovery action failed, named by cause.
            published_device_status(
                &BackendState::Absent {
                    reason: free_text.into(),
                },
                &DeviceStatus::RecoveryFailed {
                    code: LossCause::WorkerStalled.short(),
                },
            ),
        ];
        let labels: Vec<String> = scenarios.iter().map(DeviceStatus::label).collect();
        assert_eq!(
            labels,
            vec![
                DeviceStatus::LABEL_ABSENT.to_string(),
                format!("{}open-failed(0x80070002)", DeviceStatus::PREFIX_LINK),
                DeviceStatus::LABEL_ALLOWANCE.to_string(),
                format!("{}worker-stalled", DeviceStatus::PREFIX_RECOVERY),
            ]
        );
        for (index, left) in labels.iter().enumerate() {
            for right in labels[index + 1..].iter() {
                assert_ne!(left, right, "the four states must publish distinctly");
            }
        }
        for label in &labels {
            assert!(
                !label.contains("0xDEADBEEF") && !label.contains("transport prose"),
                "free text reached the published label: {label}"
            );
        }
    }

    /// RC-5 acceptance (4): every published label is an enum-arm template plus
    /// an RC-1 cause code. The whole accepted cause vocabulary is walked, so no
    /// label can be a free-text concatenation path.
    #[test]
    fn published_device_labels_are_derived_from_cause_codes_only() {
        let free_text = "transport prose 0xDEADBEEF";
        let mut link_labels = std::collections::HashSet::new();
        for cause in LossCause::all() {
            let status = published_device_status(
                &BackendState::Disconnected {
                    cause,
                    reason: free_text.into(),
                },
                &DeviceStatus::Healthy,
            );
            let expected = format!("{}{}", DeviceStatus::PREFIX_LINK, cause.short());
            assert_eq!(status.label(), expected, "label is not code-derived");
            assert!(!status.label().contains("0xDEADBEEF"));
            link_labels.insert(status.label());

            let failed = published_device_status(
                &BackendState::Connected {
                    kind: BackendKind::Sdk,
                },
                &DeviceStatus::RecoveryFailed {
                    code: cause.short(),
                },
            );
            assert_eq!(
                failed.label(),
                format!("{}{}", DeviceStatus::PREFIX_RECOVERY, cause.short())
            );
        }
        assert_eq!(
            link_labels.len(),
            LossCause::all().len(),
            "each cause must name its own published label"
        );
        // The two non-cause states are fixed arm templates, never derived text.
        assert_eq!(
            published_device_status(
                &BackendState::Absent {
                    reason: free_text.into()
                },
                &DeviceStatus::Healthy,
            )
            .label(),
            DeviceStatus::LABEL_ABSENT
        );
        assert_eq!(
            published_device_status(&BackendState::Discovering, &DeviceStatus::AllowanceConsumed)
                .label(),
            DeviceStatus::LABEL_ALLOWANCE
        );
        assert_eq!(
            published_device_status(&BackendState::Discovering, &DeviceStatus::Healthy).label(),
            DeviceStatus::LABEL_HEALTHY
        );
    }

    /// RC-5: terminal recovery labels (c)/(d) outrank the transient loss labels
    /// and (b) is published while the allowance is still unspent.
    #[test]
    fn recovery_labels_outrank_the_transient_loss_labels() {
        let loss = BackendState::Disconnected {
            cause: LossCause::ProbeFail,
            reason: "probe failed".into(),
        };
        assert_eq!(
            published_device_status(&loss, &DeviceStatus::Healthy),
            DeviceStatus::LinkFailed {
                code: "probe-fail".to_string()
            },
            "the loss is named before any automatic action ends"
        );
        assert_eq!(
            published_device_status(&loss, &DeviceStatus::AllowanceConsumed),
            DeviceStatus::AllowanceConsumed
        );
        assert_eq!(
            published_device_status(
                &BackendState::Absent {
                    reason: "no candidates".into()
                },
                &DeviceStatus::RecoveryFailed {
                    code: "call-timeout".to_string()
                },
            ),
            DeviceStatus::RecoveryFailed {
                code: "call-timeout".to_string()
            }
        );
    }

    #[test]
    fn a_backend_spawn_failure_publishes_the_named_loss_row_on_the_established_surface() {
        // WO-U46G item 5: the startup/reload sites publish the SAME
        // `Disconnected` shape the worker publishes for a transport loss (one
        // construction point), so the panel and the log name the cause and no
        // device status is claimed.
        let error = crate::backends::LossError::new(
            LossCause::ProbeFail,
            "spawn backend worker thread: os error 8".into(),
        );
        let state = crate::backends::disconnect_state(&error);
        assert_eq!(
            state,
            BackendState::Disconnected {
                cause: LossCause::ProbeFail,
                reason: "probe-fail: spawn backend worker thread: os error 8".to_string(),
            },
            "the reload site's state is the established loss-cause row"
        );
        assert_eq!(
            published_device_status(&state, &DeviceStatus::Healthy),
            DeviceStatus::LinkFailed {
                code: "probe-fail".to_string()
            },
            "the failure surfaces as the named loss, never as a connected device"
        );
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
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "a fresh graph is as recorded");

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
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "a fresh graph is as recorded");

        // The write changes the include's size, so the detection is deterministic
        // on this filesystem: a same-size edit inside one file-timestamp tick is
        // metadata-invisible and is owned by the bounded full recheck (pinned by
        // `config_watcher_finds_a_same_size_edit_with_a_preserved_mtime_at_the_bounded_recheck`).
        std::fs::write(&include, "preview_scale 3 # transitive\n").unwrap();
        let observed = watcher
            .detect()
            .expect("transitive include change is observed");
        assert!(
            watcher.detect().is_some(),
            "failed reload does not commit the changed baseline"
        );
        let reloaded = crate::parser::load(&primary).unwrap();
        watcher.install(&reloaded.files, &observed, &reloaded.signatures);
        assert!(watcher.detect().is_none(), "the committed graph is quiet");

        std::fs::remove_file(&include).unwrap();
        assert!(
            watcher.detect().is_some(),
            "missing include triggers reload"
        );
        assert!(crate::parser::load(&primary).is_err());
        assert!(
            watcher.detect().is_some(),
            "missing include keeps triggering retry"
        );
        std::fs::write(&include, "preview_scale 4\n").unwrap();
        let observed = watcher
            .detect()
            .expect("replacement include remains pending");
        let reloaded = crate::parser::load(&primary).unwrap();
        watcher.install(&reloaded.files, &observed, &reloaded.signatures);
        assert!(watcher.detect().is_none(), "the replacement is committed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-U46H: a private temp dir per watcher test (the shipped fixture shape).
    fn watcher_fixture_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lcdsirplus-watcher-{}-{}-{:?}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// WO-U46H #9: the watcher must never record unparsed bytes as applied. The
    /// interleaving is detection (t0), a write, the parse (t1), the install (t3).
    /// The shipped `replace` signed the CURRENT disk at t3, so the late write was
    /// recorded as applied and never parsed (lost). 46h made `install` record the
    /// t0 snapshot, which keeps that write pending; WO-OPT01 (C1) moves the
    /// source to the bytes the parse consumed, so what is recorded as applied IS
    /// what was parsed and the t0/t3 gap closes in both directions.
    ///
    /// ADAPTED leg (disclosed): the 46h shape asserted a re-detect of the late
    /// write after the install, because 46h signed the root from the t0 read.
    /// Under the parsed-bytes rule that write is consumed BY the parse, so the
    /// same claims are made directly: the parsed value is the late value, and the
    /// recorded signature equals the digest of the bytes that are on disk. No
    /// shipped claim is dropped (T1/T2 own the late-write-to-a-dependency leg).
    #[test]
    fn config_watcher_records_the_detection_snapshot_so_a_late_write_is_never_applied_unparsed() {
        let dir = watcher_fixture_dir("interleaving");
        let primary = dir.join("main.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the baseline graph is quiet");

        // Each write changes the file size, so detection is deterministic: within
        // one NTFS file-timestamp tick (~15.6 ms) consecutive writes share an
        // mtime, and a same-size edit is then a metadata-invisible write, owned by
        // the bounded full recheck (pinned by
        // `config_watcher_finds_a_same_size_edit_with_a_preserved_mtime_at_the_bounded_recheck`).
        std::fs::write(&primary, "preview_scale 3 # the detected edit\n").unwrap();
        let observed = watcher.detect().expect("the first edit is detected");
        // The late write is the metadata-visible shape too: a different length and a
        // legal value, landing between the detection and the install.
        std::fs::write(&primary, "preview_scale 4 # the late write\n").unwrap();
        let parsed = crate::parser::load(&primary).unwrap();
        assert_eq!(
            parsed.config.preview_scale, 4,
            "the late write is parsed by the reload this install records"
        );
        watcher.install(&parsed.files, &observed, &parsed.signatures);

        // WO-OPT01 (C1): the applied signature is the digest of the parsed bytes,
        // which are the bytes on disk, so no unparsed version is recorded.
        let on_disk = std::fs::read(&primary).unwrap();
        assert_eq!(
            watcher
                .files
                .iter()
                .find(|(seen, _)| *seen == parsed.files[0])
                .and_then(|(_, signature)| signature.as_ref())
                .map(|signature| signature.digest.clone()),
            Some(crate::sha256::sha256_hex(&on_disk)),
            "the recorded signature is the digest of the bytes the parse consumed"
        );
        assert!(
            watcher.detect().is_none(),
            "the late write is applied by the parse that consumed it, so the graph goes quiet"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-U46H #14: the metadata fast path must not lose a write it cannot see.
    /// A same-size edit with a preserved mtime is exactly that case, and the
    /// bounded full recheck is what finds it: never before sweep
    /// `WATCHER_FULL_RECHECK_CHECKS`, at that sweep, once, then quiet.
    #[test]
    fn config_watcher_finds_a_same_size_edit_with_a_preserved_mtime_at_the_bounded_recheck() {
        let dir = watcher_fixture_dir("preserved-mtime");
        let primary = dir.join("main.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        let mtime = std::fs::metadata(&primary).unwrap().modified().unwrap();

        std::fs::write(&primary, "preview_scale 3\n").unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&primary)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        drop(file);
        let metadata = std::fs::metadata(&primary).unwrap();
        assert_eq!(metadata.modified().unwrap(), mtime, "mtime preserved");
        assert_eq!(metadata.len(), 16, "size preserved");

        let mut found = 0;
        for sweep in 1..=WATCHER_FULL_RECHECK_CHECKS {
            if let Some(observed) = watcher.detect() {
                assert_eq!(
                    sweep, WATCHER_FULL_RECHECK_CHECKS,
                    "the fast path reported the metadata-invisible edit early"
                );
                found += 1;
                let reloaded = crate::parser::load(&primary).unwrap();
                watcher.install(&reloaded.files, &observed, &reloaded.signatures);
            }
        }
        assert_eq!(
            found, 1,
            "the bounded full recheck finds the metadata-invisible edit"
        );
        assert!(
            watcher.detect().is_none(),
            "the applied edit goes quiet, so the fast path cannot re-fire on it"
        );
        assert_eq!(
            crate::parser::load(&primary).unwrap().config.preview_scale,
            3,
            "the panel gets the edited value"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-U46H #9: include-graph changes are re-signatured correctly. A new
    /// include has no detection-time signature and a recreated include was absent
    /// at detection, so both are signed from disk; the file detection observed
    /// keeps its detection-time signature.
    #[test]
    fn config_watcher_install_signs_added_and_recreated_includes() {
        let dir = watcher_fixture_dir("graph-changes");
        let primary = dir.join("main.txt");
        let include = dir.join("local.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);

        std::fs::write(&include, "preview_scale 3\n").unwrap();
        std::fs::write(&primary, "include local.txt\npreview_scale 2\n").unwrap();
        let observed = watcher
            .detect()
            .expect("the primary change that adds an include is detected");
        assert_eq!(
            observed.len(),
            1,
            "detection only knows the graph it recorded, before the parse"
        );
        let reloaded = crate::parser::load(&primary).unwrap();
        assert_eq!(reloaded.files.len(), 2);
        watcher.install(&reloaded.files, &observed, &reloaded.signatures);
        assert!(
            watcher.detect().is_none(),
            "the new include is signed, so the grown graph is quiet"
        );

        std::fs::remove_file(&include).unwrap();
        let observed = watcher
            .detect()
            .expect("the removed include is detected for a retry");
        assert!(
            crate::parser::load(&primary).is_err(),
            "and the load is rejected"
        );
        std::fs::write(&include, "preview_scale 4\n").unwrap();
        let reloaded = crate::parser::load(&primary).unwrap();
        watcher.install(&reloaded.files, &observed, &reloaded.signatures);
        assert!(
            watcher.detect().is_none(),
            "the recreated include is signed from disk, not installed as absent"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01 (C1, T1): the handoff's interleaving scenario on the dependency
    /// leg. The include is NEW to the graph, so detection has no signature for
    /// it. The order is detection (t0), the parse (t1, value 3), the late write to
    /// the include (t2, value 4), `install` (t3). Pre-fix `install` signed a path
    /// it never saw at detection from a read of the CURRENT disk at t3, so version
    /// 4 was recorded as applied, never parsed, and the graph went quiet with the
    /// panel showing 3 forever - the Step 0 probe recorded exactly that
    /// (`RED_EXIT[opt01_probe_late_write]=101`). Post-fix the recorded signature is
    /// the parser's digest of version 3 and the path is PENDING, so the next sweep
    /// flags the change and version 4 is parsed and applied.
    #[test]
    fn a_late_write_to_an_include_that_is_new_to_the_graph_is_never_recorded_as_applied() {
        let dir = watcher_fixture_dir("late-include");
        let primary = dir.join("main.txt");
        let include = dir.join("local.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the baseline graph is quiet");

        std::fs::write(&include, "preview_scale 3\n").unwrap();
        std::fs::write(&primary, "preview_scale 2\ninclude local.txt\n").unwrap();
        let observed = watcher
            .detect()
            .expect("the root edit that adds the include is detected");
        assert_eq!(
            observed.len(),
            1,
            "detection cannot know the include the parse is about to read"
        );
        let parsed = crate::parser::load(&primary).unwrap();
        assert_eq!(parsed.config.preview_scale, 3, "the parse result is 3");
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(
            parsed.signatures.len(),
            parsed.files.len(),
            "the parse signs every file whose bytes it consumed"
        );

        // The late write lands after the parse and before the install.
        std::fs::write(&include, "preview_scale 4 # late\n").unwrap();
        watcher.install(&parsed.files, &observed, &parsed.signatures);

        // C1: what is recorded is the parsed version 3, never the disk's 4, and
        // the dependency the detection sweep did not see is pending.
        let recorded = watcher
            .files
            .iter()
            .find(|(seen, _)| *seen == parsed.files[1])
            .and_then(|(_, signature)| signature.as_ref())
            .map(|signature| signature.digest.clone());
        assert_eq!(
            recorded,
            Some(parsed.signatures[1].1.digest.clone()),
            "the include is recorded with the digest of the bytes the parse consumed"
        );
        assert_ne!(
            recorded,
            Some(crate::sha256::sha256_hex(&std::fs::read(&include).unwrap())),
            "the late version on disk is NOT recorded as applied"
        );
        assert_eq!(
            watcher.pending,
            vec![parsed.files[1].clone()],
            "a dependency unseen at detection is pending, never markable applied"
        );

        let late = watcher
            .detect()
            .expect("the next watch cycle flags the late write");
        let reapplied = crate::parser::load(&primary).unwrap();
        assert_eq!(reapplied.config.preview_scale, 4, "the result becomes 4");
        watcher.install(&reapplied.files, &late, &reapplied.signatures);
        assert!(
            watcher.detect().is_none(),
            "the late version is applied once and the graph goes quiet"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01 (C1, T2): a dependency that is absent at detection time. Leg A:
    /// it is created after the detection and before the parse, so the parse
    /// consumes it - it is then applied EXACTLY once, with no re-parse, and it
    /// was pending (never markable applied) while unparsed. Leg B: it is absent
    /// at detection AND at the parse, so the reload is rejected: nothing about it
    /// may be recorded as applied, its appearance must be detected, and it is
    /// then applied exactly once.
    #[test]
    fn a_dependency_absent_at_detection_is_pending_and_applied_exactly_once() {
        let dir = watcher_fixture_dir("absent-at-detection");
        let primary = dir.join("main.txt");
        let include = dir.join("local.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the baseline graph is quiet");

        // Leg A: created after the detection, before the parse.
        std::fs::write(&primary, "preview_scale 2\ninclude local.txt\n").unwrap();
        let observed = watcher
            .detect()
            .expect("the root edit that adds the include is detected");
        assert_eq!(observed.len(), 1, "the include is unseen at detection time");
        std::fs::write(&include, "preview_scale 3\n").unwrap();
        let before = crate::parser::test_load_calls();
        let parsed = crate::parser::load(&primary).unwrap();
        assert_eq!(
            crate::parser::test_load_calls(),
            before + 1,
            "one parse produces the applied version"
        );
        assert_eq!(parsed.config.preview_scale, 3);
        assert!(
            watcher
                .files
                .iter()
                .all(|(seen, _)| *seen != parsed.files[1]),
            "the dependency is not in the applied graph before the install"
        );
        watcher.install(&parsed.files, &observed, &parsed.signatures);
        assert_eq!(
            watcher.pending,
            vec![parsed.files[1].clone()],
            "created after detection => pending"
        );
        assert!(
            watcher.detect().is_none(),
            "the parsed include is applied exactly once, with no second cycle"
        );
        assert_eq!(
            crate::parser::test_load_calls(),
            before + 1,
            "the applied version is never reparsed"
        );

        // Leg B: absent at detection and at the parse => rejected, so nothing of
        // the dependency is recorded as applied; its appearance is detected.
        std::fs::remove_file(&include).unwrap();
        std::fs::write(&primary, "preview_scale 2\ninclude local.txt\n# touched\n").unwrap();
        let observed = watcher.detect().expect("the root edit is detected again");
        let mut consumed = Vec::new();
        let error = crate::parser::load_with_consumed(&primary, &mut consumed)
            .expect_err("the missing include is rejected");
        assert!(
            error.to_string().contains("cannot read configuration"),
            "the rejection names the unreadable dependency: {error}"
        );
        watcher.record_rejection(&observed, &error.file, &consumed);
        assert_eq!(
            watcher
                .files
                .iter()
                .find(|(seen, _)| *seen == parsed.files[1])
                .and_then(|(_, signature)| signature.as_ref())
                .map(|signature| signature.digest.clone()),
            Some(parsed.signatures[1].1.digest.clone()),
            "the rejected load records nothing new for the dependency: its entry is still \
             the last APPLIED version, never the version that failed to load: {error}"
        );
        let before = crate::parser::test_load_calls();
        for _ in 0..4 {
            assert!(
                watcher.detect().is_none(),
                "an unchanged rejection (still-missing include) is quiet"
            );
        }
        std::fs::write(&include, "preview_scale 5\n").unwrap();
        // WO-OPT01R (F-AUD-OPT01-2): a path with no recorded baseline is evaluated
        // at the full-recheck cadence, so the appearance is detected within the
        // documented bound (`WATCHER_FULL_RECHECK_CHECKS` sweeps), not on a
        // per-sweep read.
        let mut observed = None;
        for sweep in 1..=WATCHER_FULL_RECHECK_CHECKS {
            if let Some(found) = watcher.detect() {
                observed = Some(found);
                break;
            }
            assert!(
                sweep < WATCHER_FULL_RECHECK_CHECKS,
                "the missing include appearing is detected within the bound (sweep {sweep})"
            );
        }
        let observed = observed.expect("the missing include appearing is detected");
        let parsed = crate::parser::load(&primary).unwrap();
        assert_eq!(parsed.config.preview_scale, 5);
        watcher.install(&parsed.files, &observed, &parsed.signatures);
        assert_eq!(
            crate::parser::test_load_calls() - before,
            1,
            "the recreated include is parsed exactly once"
        );
        assert!(
            watcher.detect().is_none(),
            "the recreated include is applied and the graph goes quiet"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01 (O7, T3): the quiescence regression. >= 20 sweeps on an unchanged
    /// rejected config: the parser is invoked ONCE and the INFO row and the
    /// rejection row are each emitted ONCE, because the observed content is the
    /// version the watcher already rejected. Pre-fix the whole window is a storm:
    /// one INFO row and one full graph parse per sweep (the shipped ERROR-row
    /// dedup never covered the INFO row or the parse).
    #[test]
    fn an_unchanged_rejected_config_is_never_reparsed_and_never_relogs() {
        let dir = watcher_fixture_dir("rejection-quiescence");
        let primary = dir.join("main.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the applied baseline is quiet");

        std::fs::write(&primary, "preview_mode sometimes\n").unwrap();
        let mut info_rows = 0;
        let mut rejection_rows = 0;
        let mut last: Option<String> = None;
        let before = crate::parser::test_load_calls();
        for sweep in 1..=24 {
            let observed = match watcher.detect() {
                Some(observed) => observed,
                None => continue,
            };
            // src\app.rs:248: the INFO row is gated on exactly this `Some`.
            info_rows += 1;
            let mut consumed = Vec::new();
            match crate::parser::load_with_consumed(&primary, &mut consumed) {
                Ok(_) => unreachable!("the config stays invalid for the window (sweep {sweep})"),
                Err(error) => {
                    watcher.record_rejection(&observed, &error.file, &consumed);
                    let identity = error.to_string();
                    if reload_rejection_is_new(&mut last, &identity) {
                        rejection_rows += 1;
                    }
                }
            }
        }
        assert_eq!(
            crate::parser::test_load_calls() - before,
            1,
            "24 sweeps on an unchanged rejected version perform one parse"
        );
        assert_eq!(
            info_rows, 1,
            "one INFO row for the first rejection, none for an unchanged repeat"
        );
        assert_eq!(
            rejection_rows, 1,
            "one rejection row for an unchanged rejected version"
        );
        assert!(
            last.as_deref()
                .unwrap_or_default()
                .ends_with(":0: preview_mode must be auto, always, or never"),
            "the identity keeps the shipped {{file}}:{{line}}: {{message}} text: {last:?}"
        );

        // The suppression is not permanent: a repair is detected, reported,
        // parsed once, and goes quiet.
        std::fs::write(&primary, "preview_mode auto\n").unwrap();
        let observed = watcher
            .detect()
            .expect("the repair is detected after the quiescence");
        info_rows += 1;
        let repaired = crate::parser::load(&primary).expect("the repair parses");
        watcher.install(&repaired.files, &observed, &repaired.signatures);
        // src\app.rs:314 clears the rejection-identity suppression on success; the
        // recurrence legs are pinned by T5.
        assert_eq!(info_rows, 2, "the repair is reported once");
        assert_eq!(
            crate::parser::test_load_calls() - before,
            2,
            "one parse for the repair, and the rejected window stays at one"
        );
        assert!(watcher.detect().is_none(), "the repair goes quiet");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01 (O7, T4): a repair whose bytes keep BOTH the mtime and the length
    /// the watcher has stored - the metadata collision. The fast path cannot see
    /// it, so the shipped bounded content recheck is the guarantee: the repair is
    /// applied within `WATCHER_FULL_RECHECK_CHECKS` sweeps, exactly once, and the
    /// unchanged rejected window in between performs no extra parse.
    #[test]
    fn a_repair_with_identical_mtime_and_length_is_applied_within_the_bounded_recheck() {
        let dir = watcher_fixture_dir("identical-metadata-repair");
        let primary = dir.join("main.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        let baseline = std::fs::metadata(&primary).unwrap();
        let mtime = baseline.modified().unwrap();
        let len = baseline.len();
        assert_eq!(len, 16, "the applied baseline is 16 bytes");

        // Reject: a different length, so the first detection is deterministic.
        std::fs::write(&primary, "preview_mode sometimes\n").unwrap();
        let observed = watcher.detect().expect("the invalid edit is detected");
        let mut consumed = Vec::new();
        let error = crate::parser::load_with_consumed(&primary, &mut consumed)
            .expect_err("the config is rejected");
        watcher.record_rejection(&observed, &error.file, &consumed);
        let before = crate::parser::test_load_calls();

        // Repair: same length as the stored baseline, mtime restored to it.
        std::fs::write(&primary, "preview_scale 3\n").unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&primary)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        drop(file);
        let metadata = std::fs::metadata(&primary).unwrap();
        assert_eq!(metadata.modified().unwrap(), mtime, "mtime preserved");
        assert_eq!(metadata.len(), len, "length preserved");

        let mut applied_at = 0u32;
        let mut values = Vec::new();
        for sweep in 1..=WATCHER_FULL_RECHECK_CHECKS {
            if let Some(observed) = watcher.detect() {
                let repaired = crate::parser::load(&primary).expect("the repair parses");
                watcher.install(&repaired.files, &observed, &repaired.signatures);
                applied_at = sweep;
                values.push(repaired.config.preview_scale);
            }
        }
        assert_eq!(values, vec![3], "the repair is applied exactly once");
        assert!(
            applied_at > 0 && applied_at <= WATCHER_FULL_RECHECK_CHECKS,
            "the bounded recheck is the guarantee: applied at sweep {applied_at} of {WATCHER_FULL_RECHECK_CHECKS}"
        );
        assert_eq!(
            crate::parser::test_load_calls() - before,
            1,
            "the unchanged rejected window performs no parse; only the repair does"
        );
        assert!(watcher.detect().is_none(), "the repair goes quiet");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01 (O7, T5): rejection state is per observed CONTENT version.
    /// reject (cause A) -> unchanged repeats quiet -> repair (success resets) ->
    /// reject again with the identical cause (logs again) -> repair -> reject with
    /// a DIFFERENT cause (logs again) -> unchanged repeats quiet -> the missing
    /// include appears (detected, parsed, applied).
    #[test]
    fn a_recurrence_after_a_repair_and_a_different_cause_are_logged_again() {
        let dir = watcher_fixture_dir("recurrence");
        let primary = dir.join("main.txt");
        let gone = dir.join("gone.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        let mut info_rows = 0;
        let mut rejection_rows = 0;
        let mut last: Option<String> = None;

        let reject = |watcher: &mut ConfigWatcher,
                      last: &mut Option<String>,
                      info_rows: &mut i32,
                      rejection_rows: &mut i32| {
            let observed = watcher.detect().expect("the change is detected");
            *info_rows += 1;
            let mut consumed = Vec::new();
            let error = crate::parser::load_with_consumed(&primary, &mut consumed)
                .expect_err("the reload is rejected");
            watcher.record_rejection(&observed, &error.file, &consumed);
            let identity = error.to_string();
            if reload_rejection_is_new(last, &identity) {
                *rejection_rows += 1;
            }
            identity
        };

        // 1. cause A, then unchanged repeats.
        std::fs::write(&primary, "preview_mode sometimes\n").unwrap();
        let cause_a = reject(&mut watcher, &mut last, &mut info_rows, &mut rejection_rows);
        for _ in 0..8 {
            assert!(
                watcher.detect().is_none(),
                "an unchanged rejected version is quiet (O7)"
            );
        }
        assert_eq!(rejection_rows, 1, "the first rejection logs once");

        // 2. repair: the success resets the state.
        std::fs::write(&primary, "preview_mode auto\n").unwrap();
        let observed = watcher.detect().expect("the repair is detected");
        info_rows += 1;
        let repaired = crate::parser::load(&primary).expect("the repair parses");
        watcher.install(&repaired.files, &observed, &repaired.signatures);
        last = None;

        // 3. the identical cause recurs after the repair: logged again.
        std::fs::write(&primary, "preview_mode sometimes\n").unwrap();
        let again = reject(&mut watcher, &mut last, &mut info_rows, &mut rejection_rows);
        assert_eq!(again, cause_a, "the recurrence is the identical identity");
        assert_eq!(rejection_rows, 2, "a recurrence after a success logs again");

        // 4. a repair, then a DIFFERENT cause: logged again. Each repair has its
        // own length, so every detection here is metadata-visible and the window
        // needs no recheck to make progress.
        std::fs::write(&primary, "preview_mode auto\npreview_scale 5\n").unwrap();
        let observed = watcher.detect().expect("the second repair is detected");
        info_rows += 1;
        let repaired = crate::parser::load(&primary).expect("the second repair parses");
        watcher.install(&repaired.files, &observed, &repaired.signatures);
        last = None;
        std::fs::write(&primary, "preview_mode auto\ninclude gone.txt\n").unwrap();
        let cause_b = reject(&mut watcher, &mut last, &mut info_rows, &mut rejection_rows);
        assert_ne!(
            cause_b, cause_a,
            "the second rejection has a different cause"
        );
        assert_eq!(
            rejection_rows, 3,
            "a different rejection cause after a repair logs again"
        );

        // 5. the new rejection is quiet while unchanged, and the missing include
        // appearing is detected and applied.
        for _ in 0..6 {
            assert!(
                watcher.detect().is_none(),
                "the unchanged cause-B rejection is quiet (O7)"
            );
        }
        std::fs::write(&gone, "preview_scale 7\n").unwrap();
        // WO-OPT01R (F-AUD-OPT01-2): the absent entry is evaluated at the
        // full-recheck cadence, so the appearance lands within the bound.
        let mut observed = None;
        for sweep in 1..=WATCHER_FULL_RECHECK_CHECKS {
            if let Some(found) = watcher.detect() {
                observed = Some(found);
                break;
            }
            assert!(
                sweep < WATCHER_FULL_RECHECK_CHECKS,
                "the missing include appearing is detected within the bound (sweep {sweep})"
            );
        }
        let observed = observed.expect("the missing include appearing is detected");
        info_rows += 1;
        let applied = crate::parser::load(&primary).expect("the graph now loads");
        watcher.install(&applied.files, &observed, &applied.signatures);
        assert_eq!(applied.config.preview_scale, 7, "the include is applied");
        assert_eq!(info_rows, 6, "one INFO row per detected version change");
        assert!(watcher.detect().is_none(), "the applied graph is quiet");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01 (C1.3, T6): the whole include graph is observed and every shipped
    /// limit, rule and identity text is pinned at the values this head ships.
    #[test]
    fn the_include_graph_limits_cycles_and_identity_text_are_pinned() {
        let dir = watcher_fixture_dir("graph-limits");
        let pin = |path: &std::path::Path, line: usize, message: &str| {
            let error = crate::parser::load(path).expect_err("the pinned case is rejected");
            assert_eq!(
                error.to_string(),
                format!("{}:{}: {}", error.file, error.line, error.message),
                "the identity keeps the shipped {{file}}:{{line}}: {{message}} form"
            );
            assert_eq!(error.line, line, "pinned line for {message}");
            assert_eq!(error.message, message, "pinned message text");
        };

        // Depth: MAX_INCLUDES is 8, so the 10-file chain (depth 9) is refused at
        // the file that would be read at that depth.
        assert_eq!(crate::config::MAX_INCLUDES, 8);
        let deep = dir.join("deep0.txt");
        std::fs::write(&deep, "preview_scale 2\ninclude deep1.txt\n").unwrap();
        for index in 1..=9 {
            let path = dir.join(format!("deep{index}.txt"));
            let body = if index == 9 {
                "preview_scale 9\n".to_string()
            } else {
                format!("include deep{}.txt\n", index + 1)
            };
            std::fs::write(&path, body).unwrap();
        }
        let error = crate::parser::load(&deep).expect_err("the deep chain is refused");
        assert_eq!(error.message, "include depth exceeds 8");
        assert_eq!(error.line, 0);
        assert!(
            error.file.ends_with("deep9.txt"),
            "the file at the illegal depth is named: {}",
            error.file
        );

        // File count: MAX_CONFIG_FILES is 64.
        assert_eq!(crate::config::MAX_CONFIG_FILES, 64);
        let many = dir.join("many.txt");
        let mut body = String::new();
        for index in 0..64 {
            let name = dir.join(format!("leaf{index}.txt"));
            std::fs::write(&name, format!("# leaf {index}\n")).unwrap();
            body.push_str(&format!("include leaf{index}.txt\n"));
        }
        std::fs::write(&many, format!("preview_scale 2\n{body}")).unwrap();
        pin(&many, 0, "config file count exceeds limit of 64");

        // Include directives: MAX_CONFIG_INCLUDE_DIRECTIVES is 64, so the 65th
        // directive is refused (all 65 name one file, so the file-count rule
        // cannot fire first).
        assert_eq!(crate::config::MAX_CONFIG_INCLUDE_DIRECTIVES, 64);
        let one = dir.join("one.txt");
        std::fs::write(&one, "# shared include\n").unwrap();
        let directives = dir.join("directives.txt");
        std::fs::write(
            &directives,
            format!(
                "preview_scale 2\n{}include one.txt\n",
                "include one.txt\n".repeat(64)
            ),
        )
        .unwrap();
        pin(
            &directives,
            66,
            "config include directive count exceeds limit of 64",
        );

        // Aggregate bytes: MAX_CONFIG_AGGREGATE_BYTES is 8 MiB.
        assert_eq!(crate::config::MAX_CONFIG_AGGREGATE_BYTES, 8 * 1024 * 1024);
        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'#'; 8 * 1024 * 1024]).unwrap();
        let aggregate = dir.join("aggregate.txt");
        std::fs::write(&aggregate, "preview_scale 2\ninclude big.txt\n").unwrap();
        pin(
            &aggregate,
            0,
            "config input exceeds aggregate limit of 8388608 bytes",
        );

        // Cycle: the path that closes the cycle is named.
        let cycle = dir.join("cycle.txt");
        let cyclic = dir.join("cyclic.txt");
        std::fs::write(&cycle, "preview_scale 2\ninclude cyclic.txt\n").unwrap();
        std::fs::write(&cyclic, "include cycle.txt\n").unwrap();
        let error = crate::parser::load(&cycle).expect_err("the cycle is refused");
        assert!(
            error.message.starts_with("include cycle involving ")
                && error.message.ends_with("cycle.txt"),
            "the shipped cycle text names the file that closes the cycle: {}",
            error.message
        );
        assert_eq!(error.line, 0);

        // Duplicate key: per file, naming the first definition.
        let duplicate = dir.join("duplicate.txt");
        std::fs::write(&duplicate, "preview_scale 2\npreview_scale 3\n").unwrap();
        pin(
            &duplicate,
            2,
            "duplicate key \"preview_scale\"; first defined at line 1",
        );

        // Include path rules keep their text.
        let traversal = dir.join("traversal.txt");
        std::fs::write(&traversal, "include ..\\escape.txt\n").unwrap();
        pin(
            &traversal,
            1,
            "include path must not contain parent traversal",
        );
        let absolute = dir.join("absolute.txt");
        std::fs::write(&absolute, "include C:\\Windows\\evil.txt\n").unwrap();
        pin(&absolute, 1, "include path must be relative");

        // C1: the signature table covers the whole graph, one entry per consumed
        // file, from the bytes on disk; a duplicate include is consumed once.
        let root = dir.join("root.txt");
        let alpha = dir.join("alpha.txt");
        let beta = dir.join("beta.txt");
        std::fs::write(&alpha, "preview_scale 3\n").unwrap();
        std::fs::write(&beta, "preview_mode auto\n").unwrap();
        std::fs::write(
            &root,
            "preview_scale 2\ninclude alpha.txt\ninclude beta.txt\ninclude alpha.txt\n",
        )
        .unwrap();
        let loaded = crate::parser::load(&root).unwrap();
        assert_eq!(
            loaded.files.len(),
            3,
            "the duplicate include is consumed once"
        );
        assert_eq!(
            loaded.signatures.len(),
            loaded.files.len(),
            "one signature per consumed file, in graph order"
        );
        for (path, signature) in &loaded.signatures {
            assert_eq!(
                signature.digest,
                crate::sha256::sha256_hex(&std::fs::read(path).unwrap()),
                "the signature is the digest of the bytes the parser read from {path:?}"
            );
            assert_eq!(signature.len, std::fs::metadata(path).unwrap().len());
        }
        assert_eq!(loaded.config.preview_scale, 3, "the include position wins");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01R (T7, the audited F-AUD-OPT01-1 gap): a 3-level graph
    /// `main.txt -> mid.txt -> leaf.txt` where the rejection is caused inside the
    /// LEAF, so the leaf is the file the error NAMES (and is watched) while the
    /// intermediate `mid.txt` is consumed by the parse but never watched. A repair
    /// applied to `mid.txt` while the named root and the named leaf stay
    /// byte-identical was invisible on the frozen OPT-01 state machine
    /// (`detections=0 of 32`, the pre-repair RED rows). The bounded full recheck
    /// now re-verifies the whole graph the rejected parse consumed, so the repair
    /// is detected within `WATCHER_FULL_RECHECK_CHECKS + 1` sweeps (WO-OPT01R5
    /// tightened the old `* 2` = 32 per F-AUD-OPT01R-2; the derivation is at the
    /// legA loop), the repaired value becomes applied, and the quiescence legs keep
    /// their counts. legB (the named leaf deleted) is the leg the audit found
    /// already detected, kept as a pin. legD (WO-OPT01R5) measures the shipped
    /// consumed-graph claim that the audit left unmeasured: a repair in a file that
    /// is in NEITHER set - never read by the rejected parse, never named by its
    /// error - stays quiet until a parse reaches it, and is adopted on the sweep
    /// that reaches it (measured row: `UOPT1R-T7 legD_...`, T9 carries the cadence
    /// rows for the standing-rejection parse rate).
    #[test]
    fn a_repair_in_a_consumed_intermediate_include_is_detected_within_the_bounded_recheck() {
        let dir = watcher_fixture_dir("consumed-intermediate");
        let primary = dir.join("main.txt");
        let mid = dir.join("mid.txt");
        let leaf = dir.join("leaf.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        std::fs::write(&mid, "include leaf.txt\n").unwrap();
        std::fs::write(&leaf, "preview_scale 5\npreview_scale 6\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the baseline graph is quiet");

        std::fs::write(&primary, "preview_scale 2\ninclude mid.txt\n").unwrap();
        let mut last: Option<String> = None;
        let mut info_rows = 0;
        let mut rejection_rows = 0;
        let mut parses = 0;
        for _ in 0..3 {
            if let Some(observed) = watcher.detect() {
                info_rows += 1;
                let before = crate::parser::test_load_calls();
                let mut consumed = Vec::new();
                let error = crate::parser::load_with_consumed(&primary, &mut consumed)
                    .expect_err("the duplicate key in the leaf is rejected");
                parses += crate::parser::test_load_calls() - before;
                assert!(
                    error.to_string().contains("duplicate key"),
                    "the rejection is the leaf's duplicate key: {error}"
                );
                assert_eq!(
                    consumed.len(),
                    3,
                    "the attempt consumed root + mid + leaf, and reports all of them"
                );
                watcher.record_rejection(&observed, &error.file, &consumed);
                if reload_rejection_is_new(&mut last, &error.to_string()) {
                    rejection_rows += 1;
                }
            }
        }
        assert!(
            info_rows <= 2 && parses <= 2,
            "the rejected window fires at most the leaf-transition re-fire (info={info_rows} parses={parses})"
        );
        assert_eq!(
            rejection_rows, 1,
            "one rejection row for an unchanged rejection"
        );
        assert!(
            watcher.files.iter().any(|(p, _)| p.ends_with("leaf.txt")),
            "the named leaf is watched"
        );
        assert!(
            watcher.files.iter().all(|(p, _)| !p.ends_with("mid.txt")),
            "the consumed-but-not-named intermediate is NOT in the watched graph"
        );
        assert_eq!(
            watcher
                .consumed
                .iter()
                .filter(|(p, _)| p.ends_with("mid.txt"))
                .count(),
            1,
            "the intermediate IS in the recorded consumed graph"
        );
        for _ in 0..8 {
            assert!(
                watcher.detect().is_none(),
                "the unchanged rejection stays quiet (O7)"
            );
        }

        // legB: the named leaf disappears - the leg the audit found detected.
        std::fs::remove_file(&leaf).unwrap();
        let mut detected_b = 0;
        for _ in 0..WATCHER_FULL_RECHECK_CHECKS {
            if let Some(observed) = watcher.detect() {
                detected_b += 1;
                let mut consumed = Vec::new();
                let error = crate::parser::load_with_consumed(&primary, &mut consumed)
                    .expect_err("the missing leaf is rejected");
                assert!(
                    error.to_string().contains("cannot read configuration"),
                    "{error}"
                );
                watcher.record_rejection(&observed, &error.file, &consumed);
            }
        }
        assert_eq!(detected_b, 1, "legB: the leaf deletion is detected once");

        // legA: the repair lands in the consumed intermediate. The named root and
        // the (deleted) named leaf are untouched, so the watched sweep sees no
        // change and the content identity stays the standing rejected version.
        //
        // Bound derivation (WO-OPT01R5, F-AUD-OPT01R-2): the ONLY path that reports
        // this change is the periodic full recheck - `detect` returns through
        // `if full && self.consumed_changed()`, and `full` is
        // `checks % WATCHER_FULL_RECHECK_CHECKS == WATCHER_FULL_RECHECK_CHECKS - 1`
        // (evaluated before the counter is bumped). A repair that lands right AFTER
        // a full sweep must wait one whole period for the next one, so the landing
        // sweep is at most `WATCHER_FULL_RECHECK_CHECKS`, and the loop bound carries
        // + 1 for that firing sweep itself: 16 + 1 = 17. The shipped latency claim
        // is the same period (`WATCHER_FULL_RECHECK_CHECKS * config_refresh_ms`).
        // The old `* 2` = 32 accepted a cadence twice the documented one. Measured on the
        // staged `checks % 32 == 31` mutant (`WO-OPT01R-AUDIT.md` A2, lines 376-402): legA
        // stays GREEN (`detections=1 of 17 at_sweep=4`, identical to the delivered row
        // `uopt1r5-green-g2.log:14`), because the leg enters its loop at `checks=28` and 31
        // satisfies `checks % 16 == 15` as well; T7's RED on that mutant comes from legD
        // (`app.rs:3666`, `fired2=0`) and the cadence is carried by T9 legB (`fires=2` vs the
        // 4 of `uopt1r5-green-g2.log:9`). The bound's own job (detection within 17) is
        // falsified at the worst-case phase, a repair landing right after a full sweep
        // (`fires=0 at_sweep=0` RED, the same A2 rows). Measured landing:
        // sweep 4 (`uopt1r5-green-g2.log:14`), 13 sweeps of margin.
        std::fs::write(&mid, "preview_scale 3\n").unwrap();
        let mut detected_a = 0;
        let mut at_sweep = 0;
        let mut applied_value = 0;
        let parses_before_a = crate::parser::test_load_calls();
        for sweep in 1..=WATCHER_FULL_RECHECK_CHECKS + 1 {
            if let Some(observed) = watcher.detect() {
                detected_a += 1;
                at_sweep = sweep;
                let applied = crate::parser::load(&primary).expect("the repaired graph loads");
                applied_value = applied.config.preview_scale;
                watcher.install(&applied.files, &observed, &applied.signatures);
            }
        }
        let audit_row = format!(
            "legA_repair_in_intermediate_named_leaf_present detections={} of {} => {}",
            detected_a,
            WATCHER_FULL_RECHECK_CHECKS + 1,
            if detected_a == 0 {
                "NOT DETECTED (quiet-and-lost shape)"
            } else {
                "detected"
            }
        );
        println!("UOPT1R-T7 {audit_row} at_sweep={at_sweep} applied_value={applied_value}");
        assert!(
            detected_a >= 1,
            "{audit_row}: a repair in a consumed intermediate must be detected within the bound"
        );
        assert_eq!(detected_a, 1, "legA: the repair is detected exactly once");
        assert!(
            at_sweep <= WATCHER_FULL_RECHECK_CHECKS + 1,
            "legA: detection lands within the derived bound (sweep {at_sweep}, bound {})",
            WATCHER_FULL_RECHECK_CHECKS + 1
        );
        assert_eq!(applied_value, 3, "legA: the repaired value becomes applied");
        assert_eq!(
            crate::parser::test_load_calls() - parses_before_a,
            1,
            "legA: exactly one parse in the whole {}-sweep window - the quiescence stays intact",
            WATCHER_FULL_RECHECK_CHECKS + 1
        );
        assert!(
            watcher.detect().is_none(),
            "legA: the repaired graph is quiet afterwards"
        );

        // legD (WO-OPT01R5, R5-3): the consumed-graph claim the audit left unmeasured
        // (`app.rs:1704-1709`, the residual sentence) - a file that is in NEITHER the watched
        // set nor the consumed set of the standing rejection, i.e. one the rejected parse never
        // read and never named. Fresh sub-graph so the leg is independent of the
        // legs above:
        //   e_main -> e_a -> e_b -> e_c(duplicate key)   (attempt 1 reads all four)
        //   e_main -> e_a(duplicate key)                 (attempt 2 reads two)
        // After attempt 2, `e_b` is watched by nothing and consumed by nothing.
        let e_main = dir.join("e_main.txt");
        let e_a = dir.join("e_a.txt");
        let e_b = dir.join("e_b.txt");
        let e_c = dir.join("e_c.txt");
        std::fs::write(&e_c, "preview_scale 5\npreview_scale 6\n").unwrap();
        std::fs::write(&e_b, "include e_c.txt\n").unwrap();
        std::fs::write(&e_a, "include e_b.txt\npreview_mode sometimes\n").unwrap();
        std::fs::write(&e_main, "preview_scale 1\n").unwrap();
        let seeded = crate::parser::load(&e_main).unwrap();
        let mut w = ConfigWatcher::new(&seeded.files, &seeded.signatures);
        assert!(w.detect().is_none(), "legD: the seed graph is quiet");
        let mut last_d: Option<String> = None;
        let mut info_d = 0u32;
        let mut rows_d = 0u32;
        let mut applied_d: Vec<i32> = Vec::new();

        // One bounded sweep window of the shipped production shape: detect -> parse
        // -> install (success) / record_rejection + row dedup (failure). A nested fn
        // takes the counters by reference, so the leg rows can be printed between
        // windows without borrowing them for the whole leg.
        fn leg_fire(
            w: &mut ConfigWatcher,
            main: &std::path::Path,
            sweeps: u32,
            last: &mut Option<String>,
            info: &mut u32,
            rows: &mut u32,
        ) -> (u32, Vec<i32>) {
            let mut fired = 0;
            let mut got = Vec::new();
            for _ in 0..sweeps {
                if let Some(observed) = w.detect() {
                    fired += 1;
                    *info += 1;
                    let mut consumed = Vec::new();
                    match crate::parser::load_with_consumed(main, &mut consumed) {
                        Ok(loaded) => {
                            got.push(loaded.config.preview_scale);
                            w.install(&loaded.files, &observed, &loaded.signatures);
                        }
                        Err(error) => {
                            w.record_rejection(&observed, &error.file, &consumed);
                            if reload_rejection_is_new(last, &error.to_string()) {
                                *rows += 1;
                            }
                        }
                    }
                }
            }
            (fired, got)
        }

        // Attempt 1: the graph reaches e_c, whose duplicate key is rejected, so
        // e_c is the NAMED (present) file and e_a/e_b are consumed but unwatched.
        std::fs::write(&e_main, "preview_scale 1\ninclude e_a.txt\n").unwrap();
        let (fired1, _) = leg_fire(&mut w, &e_main, 1, &mut last_d, &mut info_d, &mut rows_d);
        assert_eq!(
            fired1, 1,
            "legD: the root edit is detected on the next sweep"
        );
        assert_eq!(
            w.consumed.len(),
            4,
            "legD: attempt 1 consumed e_main + e_a + e_b + e_c"
        );
        // Attempt 2 shrinks the graph: e_a now fails on its own duplicate key, so
        // e_a becomes the NAMED (present) file and stops including e_b. e_b leaves
        // the consumed set and was never watched: the invisible shape, with a
        // present named path standing in rejection. Detection rides the full
        // recheck (e_a is consumed, not watched, at this moment).
        std::fs::write(&e_a, "preview_scale 5\npreview_scale 6\n").unwrap();
        let (fired2, _) = leg_fire(
            &mut w,
            &e_main,
            WATCHER_FULL_RECHECK_CHECKS + 1,
            &mut last_d,
            &mut info_d,
            &mut rows_d,
        );
        assert_eq!(
            fired2, 1,
            "legD: the shrinking of the consumed graph is detected once within the bound"
        );
        assert!(
            w.files.iter().any(|(p, _)| p.ends_with("e_a.txt")),
            "legD: the newly named e_a is watched"
        );
        assert!(
            w.consumed.iter().all(|(p, _)| !p.ends_with("e_b.txt")),
            "legD: e_b is in NO set - not watched, not consumed"
        );
        assert!(
            w.files.iter().all(|(p, _)| !p.ends_with("e_b.txt")),
            "legD: e_b is not watched"
        );

        // The invisible repair: e_b becomes valid content. Nothing is adopted and
        // nothing fires while no watched/consumed path moves (the documented
        // residual), and the standing rejection stays quiescent (F-AUD-OPT01R-1).
        std::fs::write(&e_b, "preview_scale 9\n").unwrap();
        let (fired3, got3) = leg_fire(&mut w, &e_main, 20, &mut last_d, &mut info_d, &mut rows_d);
        println!(
            "UOPT1R-T7 legD_invisible_repair_quiet fires={fired3} adopted={got3:?} reject_rows={rows_d} info_rows={info_d} window=20"
        );
        assert_eq!(
            fired3, 0,
            "legD: a repair confined to a file in neither set performs no fire (20 sweeps)"
        );
        assert!(
            got3.is_empty(),
            "legD: nothing is adopted by the invisible repair"
        );

        // The parse next reaches it: e_a includes e_b again and is valid, so the
        // firing sweep's parse consumes the repaired e_b and adopts its value.
        let applied_before = applied_d.len();
        std::fs::write(&e_a, "include e_b.txt\n").unwrap();
        let (fired4, got4) = leg_fire(
            &mut w,
            &e_main,
            WATCHER_FULL_RECHECK_CHECKS + 1,
            &mut last_d,
            &mut info_d,
            &mut rows_d,
        );
        applied_d.extend(got4);
        println!(
            "UOPT1R-T7 legD_adoption fires={fired4} adopted={applied_d:?} consumed_e_b={} reject_rows={rows_d} info_rows={info_d}",
            w.consumed
                .iter()
                .filter(|(p, _)| p.ends_with("e_b.txt"))
                .count()
        );
        assert_eq!(
            fired4, 1,
            "legD: the change that re-runs the parse is detected once within the bound"
        );
        assert_eq!(
            applied_d,
            vec![9],
            "legD: the invisible repair is adopted by the parse that next reaches it"
        );
        assert_eq!(
            w.consumed
                .iter()
                .filter(|(p, _)| p.ends_with("e_b.txt"))
                .count(),
            1,
            "legD: the firing sweep's parse consumed the repaired file (consumed-set seam)"
        );
        assert_eq!(
            w.consumed
                .iter()
                .find(|(p, _)| p.ends_with("e_b.txt"))
                .map(|(_, s)| s.digest.clone()),
            file_signature(&e_b).map(|s| s.digest),
            "legD: the recorded consumed signature is the digest of the repaired bytes"
        );
        assert_eq!(
            applied_d.len(),
            applied_before + 1,
            "legD: exactly one adoption over the whole leg"
        );
        let quiet = 16;
        for sweep in 1..=quiet {
            assert!(
                w.detect().is_none(),
                "legD: the post-adoption window is quiet (sweep {sweep} of {quiet})"
            );
        }
        println!(
            "UOPT1R-T7 legD_post_adoption_quiet sweeps={quiet} reject_rows={rows_d} info_rows={info_d} applied={applied_d:?}"
        );
        assert_eq!(
            rows_d, 2,
            "legD: two distinct rejection causes, one row each"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01R5 (T9, the blocking F-AUD-OPT01R-1 regression): a standing
    /// rejection whose NAMED path EXISTS on disk - a file the parse opened and
    /// failed on, not an absent dependency - must keep the documented recheck
    /// cadence. `record_rejection` used to store that path as `absent`,
    /// `(path, None)`, in BOTH the watched baseline and the rejected identity,
    /// while a full sweep observes its digest; the digest/absent pair then
    /// oscillated and the watcher fired TWICE per
    /// `WATCHER_FULL_RECHECK_CHECKS`-sweep cycle (9 parses / 64 sweeps, the
    /// `audit6` row of `WO-OPT01R-AUDIT.md:91`). legA is the stable graph: after
    /// the one seed parse the whole 64-sweep window must perform ZERO fires. legB
    /// changes the consumed graph once per cycle, the case the shipped allowance
    /// covers; its bound arithmetic is at the leg. Integrity legs: nothing is
    /// adopted, exactly ONE rejection row, no ERROR growth.
    #[test]
    fn a_standing_rejection_of_a_present_named_file_keeps_the_recheck_cadence() {
        let dir = watcher_fixture_dir("present-named-rejection");
        let primary = dir.join("main.txt");
        let mid = dir.join("mid.txt");
        let leaf = dir.join("leaf.txt");
        std::fs::write(&leaf, "preview_scale 5\npreview_scale 6\n").unwrap();
        std::fs::write(&mid, "include leaf.txt\n").unwrap();
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the baseline graph is quiet");

        let mut last: Option<String> = None;
        let mut info_rows = 0u32;
        let mut rejection_rows = 0u32;
        // The watched baseline of the rejection: `(path, digest)` per watched entry.
        // `applied=[]` is asserted as "this table never moves", i.e. nothing from a
        // rejected version is recorded as applied (the shipped integrity rule).
        fn watched_baselines(w: &ConfigWatcher) -> Vec<(std::path::PathBuf, Option<String>)> {
            w.files
                .iter()
                .map(|(path, signature)| {
                    (path.clone(), signature.as_ref().map(|s| s.digest.clone()))
                })
                .collect()
        }
        let parses_start = crate::parser::test_load_calls();

        // The seed rejection: the parse names `leaf.txt`, which EXISTS, and consumes
        // root + mid + leaf.
        std::fs::write(&primary, "preview_scale 2\ninclude mid.txt\n").unwrap();
        let observed = watcher.detect().expect("the root edit is detected");
        info_rows += 1;
        let mut consumed = Vec::new();
        let error = crate::parser::load_with_consumed(&primary, &mut consumed)
            .expect_err("the duplicate key in the PRESENT leaf is rejected");
        assert!(
            error.to_string().contains("duplicate key"),
            "the rejection is content, not a missing file: {error}"
        );
        assert_eq!(
            consumed.len(),
            3,
            "the attempt consumed root + mid + leaf and reports all of them"
        );
        watcher.record_rejection(&observed, &error.file, &consumed);
        if reload_rejection_is_new(&mut last, &error.to_string()) {
            rejection_rows += 1;
        }
        let baseline_start = watched_baselines(&watcher);

        // legA: 64 sweeps, nothing touched. The graph is a permanent rejection, so
        // the only permitted work is the metadata sweep; the parse must not fire.
        let identity_before = watcher.rejected.clone();
        let parses_before_a = crate::parser::test_load_calls();
        let mut fires_a = 0u32;
        for sweep in 1..=64 {
            if let Some(observed) = watcher.detect() {
                fires_a += 1;
                info_rows += 1;
                let mut consumed = Vec::new();
                let error = crate::parser::load_with_consumed(&primary, &mut consumed)
                    .expect_err(&format!("the config stays invalid (legA sweep {sweep})"));
                watcher.record_rejection(&observed, &error.file, &consumed);
                if reload_rejection_is_new(&mut last, &error.to_string()) {
                    rejection_rows += 1;
                }
            }
        }
        let parses_a = crate::parser::test_load_calls() - parses_before_a;
        let parses_pre = crate::parser::test_load_calls() - parses_start;
        println!(
            "UOPT1R-T9 legA_stable_named_present window_sweeps=64 fires={fires_a} window_parses={parses_a} parses_pre={parses_pre} identity_stable={} baseline_stable={} applied=[] reject_rows={rejection_rows} info_rows={info_rows}",
            identity_before == watcher.rejected,
            watched_baselines(&watcher) == baseline_start
        );
        assert_eq!(
            fires_a, 0,
            "legA: a stable graph under a standing rejection of a PRESENT named file \
             fires the parse ZERO times in 64 sweeps (F-AUD-OPT01R-1: the digest/absent \
             oscillation fired 8 times per 64 sweeps, 9 parses counting the seed)"
        );
        assert_eq!(
            parses_a, 0,
            "legA: the fires and the parses are the same count"
        );

        // The fix's structural claim, stated at the seam: the watched baseline of
        // the named path is the signature the filesystem holds, and the rejected
        // identity says the same thing, so the pair cannot oscillate.
        let named = std::path::PathBuf::from(&error.file);
        let baseline = watcher
            .files
            .iter()
            .find(|(seen, _)| *seen == named)
            .and_then(|(_, signature)| signature.clone())
            .expect("the named path is watched WITH a signature, not as absent");
        assert_eq!(
            Some(&baseline.digest),
            consumed
                .iter()
                .find(|(seen, _)| *seen == named)
                .map(|(_, signature)| &signature.digest),
            "the watched baseline of the named path is the digest of its real bytes"
        );
        assert_eq!(
            watcher.rejected, identity_before,
            "a quiet 64-sweep window never moves the rejected identity"
        );
        assert_eq!(
            rejection_rows, 1,
            "one rejection row for the whole standing rejection (no ERROR storm)"
        );

        // legB: the graph changes once per cycle - the case the shipped allowance
        // covers, one extra parse per `WATCHER_FULL_RECHECK_CHECKS` sweeps.
        // Bound arithmetic: 64 sweeps / 16 sweeps per cycle = 4 cycles, + 1 for a
        // change that lands just after a full sweep and is reported by the next one
        // => bound 5 parses over the window. Measured post-fix: 4 (one per cycle,
        // each at that cycle's full recheck).
        let bound_b = 64 / WATCHER_FULL_RECHECK_CHECKS as usize + 1;
        let parses_before_b = crate::parser::test_load_calls();
        let mut fires_b = 0u32;
        for sweep in 1..=64 {
            if (sweep - 1) % WATCHER_FULL_RECHECK_CHECKS as usize == 0 {
                std::fs::write(&mid, format!("preview_scale {}\ninclude leaf.txt\n", sweep))
                    .unwrap();
            }
            if let Some(observed) = watcher.detect() {
                fires_b += 1;
                info_rows += 1;
                let mut consumed = Vec::new();
                let error = crate::parser::load_with_consumed(&primary, &mut consumed)
                    .expect_err(&format!("the config stays invalid (legB sweep {sweep})"));
                watcher.record_rejection(&observed, &error.file, &consumed);
                if reload_rejection_is_new(&mut last, &error.to_string()) {
                    rejection_rows += 1;
                }
            }
        }
        let parses_b = crate::parser::test_load_calls() - parses_before_b;
        println!(
            "UOPT1R-T9 legB_per_cycle_change window_sweeps=64 changes=4 fires={fires_b} window_parses={parses_b} bound={bound_b} baseline_stable={} applied=[] reject_rows={rejection_rows} info_rows={info_rows} consumed={}",
            watched_baselines(&watcher) == baseline_start,
            watcher.consumed.len()
        );
        assert_eq!(
            fires_b, 4,
            "legB: every per-cycle change is detected, once per cycle, at the full recheck"
        );
        assert!(
            parses_b <= bound_b,
            "legB: the sustained rate stays at the documented one parse per \
             WATCHER_FULL_RECHECK_CHECKS sweeps (parses={parses_b}, bound={bound_b})"
        );
        assert_eq!(
            watched_baselines(&watcher),
            baseline_start,
            "detection integrity: 128 sweeps of the standing rejection adopt nothing \
             (applied=[]), so the watched baselines are byte-for-byte the seed table"
        );
        assert_eq!(
            rejection_rows, 1,
            "128 sweeps of the same standing rejection emit exactly one rejection row"
        );
        assert_eq!(
            info_rows,
            fires_a + fires_b + 1,
            "one INFO row per fire, counting the seed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-OPT01R (T8, the audited F-AUD-OPT01-2 nit): a rejected window must not
    /// pay a read+hash per sweep for an entry with no baseline. The count comes
    /// from the `#[cfg(test)]` read census (`test_file_signature_calls`, the
    /// shipped `parser::LOAD_CALLS` form), so the bound is measured, not asserted
    /// in prose: over 32 sweeps of a rejected window the reads are bounded by the
    /// full-recheck cadence, and the window still performs no parse.
    #[test]
    fn a_rejected_window_performs_bounded_sweep_reads() {
        let dir = watcher_fixture_dir("sweep-read-bound");
        let primary = dir.join("main.txt");
        std::fs::write(&primary, "preview_scale 2\n").unwrap();
        let loaded = crate::parser::load(&primary).unwrap();
        let mut watcher = ConfigWatcher::new(&loaded.files, &loaded.signatures);
        assert!(watcher.detect().is_none(), "the baseline graph is quiet");

        // A rejection whose named dependency does not exist: the watcher records
        // it with a None baseline, and it stays absent for the whole window.
        std::fs::write(&primary, "preview_mode auto\ninclude gone.txt\n").unwrap();
        let observed = watcher.detect().expect("the root edit is detected");
        let mut consumed = Vec::new();
        let error = crate::parser::load_with_consumed(&primary, &mut consumed)
            .expect_err("the missing include is rejected");
        assert!(
            error.to_string().contains("cannot read configuration"),
            "{error}"
        );
        watcher.record_rejection(&observed, &error.file, &consumed);
        assert!(
            watcher.files.iter().any(|(p, _)| p.ends_with("gone.txt")),
            "the named dependency is watched"
        );

        let reads_before = test_file_signature_calls();
        let baseline_before = test_baseline_read_calls();
        let parses_before = crate::parser::test_load_calls();
        for sweep in 1..=32 {
            assert!(
                watcher.detect().is_none(),
                "the unchanged rejection stays quiet (sweep {sweep})"
            );
        }
        let reads = test_file_signature_calls() - reads_before;
        let baseline_reads = test_baseline_read_calls() - baseline_before;
        let parses = crate::parser::test_load_calls() - parses_before;
        let full_sweeps = 2usize; // the 32 sweeps contain checks 15 and 31
        let watched = watcher.files.len();
        let consumed_n = watcher.consumed.len();
        let baseline_entries = watcher.files.iter().filter(|(_, s)| s.is_none()).count();
        let bound_baseline = full_sweeps * baseline_entries;
        // The window's whole read budget: one content signature per sweep for the
        // changed, NAMED file (the shipped rule that a rejected change is never
        // adopted, which is what makes a watched repair land on the next sweep),
        // plus the baseline-less entries and the consumed graph at the recheck
        // cadence only.
        let bound_total = 32 + full_sweeps * (baseline_entries + consumed_n);
        println!(
            "UOPT1R-T8 total_reads={reads} baseline_reads={baseline_reads} (watched={watched} baseline_entries={baseline_entries} consumed={consumed_n} full_sweeps={full_sweeps} bound_baseline={bound_baseline} bound_total={bound_total} parses={parses})"
        );
        assert_eq!(parses, 0, "a quiet rejected window performs no parse");
        assert!(
            baseline_reads <= bound_baseline,
            "the audited None-baseline entry is read+hashed only at the full-recheck cadence: \
             baseline_reads={baseline_reads} bound={bound_baseline} (total_reads={reads})"
        );
        assert!(
            reads <= bound_total,
            "the rejected window's total sweep reads stay bounded: reads={reads} bound={bound_total}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WO-U46H #2: the seed seam performs NO arithmetic on the clock. Measured on
    /// this host (`rustc -O` probe): `Instant::now().checked_sub` succeeds for a
    /// 100-year interval and fails at 292 years, so the shipped
    /// `Instant::now() - interval` form cannot underflow for the LEGAL intervals
    /// (telemetry 100..10000 ms, render 25..5000 ms) on Windows - it is a latent
    /// panic form, and on a clock whose base is the boot time the same expression
    /// panics whenever uptime < interval. The seam must not panic for ANY
    /// interval, so the boundary cell uses one the platform itself refuses to
    /// subtract, and the first collection/render must stay immediate. No clock
    /// framework, no boot-time dependence: the precondition is asserted.
    #[test]
    fn startup_cadence_seed_is_immediate_for_an_interval_beyond_any_uptime() {
        let now = Instant::now();
        let beyond_any_uptime = Duration::from_secs(60 * 60 * 24 * 365 * 1_000);
        assert!(
            now.checked_sub(beyond_any_uptime).is_none(),
            "the platform itself refuses this subtraction, so the shipped seed form panics here"
        );
        let seed = cadence_initial_state(now, beyond_any_uptime);
        assert!(
            cadence_due(seed.map(|last| now.duration_since(last)), beyond_any_uptime),
            "the first telemetry collection/render must fire immediately"
        );
        assert!(
            !cadence_due(Some(Duration::ZERO), beyond_any_uptime),
            "the seed is the explicit due state, not a `now` stamp that delays the first tick by a full interval"
        );
        assert!(
            !cadence_due(
                Some(beyond_any_uptime - Duration::from_nanos(1)),
                beyond_any_uptime
            ),
            "one nanosecond short is not due"
        );
        assert!(
            cadence_due(Some(beyond_any_uptime), beyond_any_uptime),
            "exactly the interval is due"
        );
        for interval in [
            Duration::from_millis(25),
            Duration::from_millis(100),
            Duration::from_millis(300),
            Duration::from_secs(10),
        ] {
            let seed = cadence_initial_state(now, interval);
            assert!(
                cadence_due(seed.map(|last| now.duration_since(last)), interval),
                "{interval:?}: the first tick must not wait"
            );
        }
    }

    /// WO-U46H #4: the reload attempt continues on every refresh tick (a repair
    /// must stay detected), so the ERROR row is deduplicated by rejection
    /// identity: identical repeat, changed failure, repair, recurrence.
    #[test]
    fn reload_rejections_log_once_per_identity_and_a_repair_resets_the_suppression() {
        let missing = "C:\\cfg\\lcdsirplus.txt:0: cannot read configuration: The system cannot find the file specified. (os error 2)";
        let invalid = "C:\\cfg\\lcdsirplus.txt:0: preview_mode must be auto, always, or never";
        let mut last: Option<String> = None;
        let mut rows = 0;
        for _ in 0..10 {
            if reload_rejection_is_new(&mut last, missing) {
                rows += 1;
            }
        }
        assert_eq!(
            rows, 1,
            "ten identical rejections at the 100 ms floor log exactly one row"
        );
        for _ in 0..10 {
            if reload_rejection_is_new(&mut last, invalid) {
                rows += 1;
            }
        }
        assert_eq!(rows, 2, "a changed rejection identity is a new row");
        assert_eq!(
            last.as_deref(),
            Some(invalid),
            "the state is one identity, replaced in place: it never grows"
        );
        last = None; // the successful reload path clears the suppression
        for _ in 0..10 {
            if reload_rejection_is_new(&mut last, missing) {
                rows += 1;
            }
        }
        assert_eq!(rows, 3, "after a repair the recurrence is reported again");
    }

    /// WO-U46H #10 ruling evidence: a repeated module in one slot is
    /// observably the single-module list. Cycling, the availability walk, the
    /// displayed panel, the demand words and validation all agree with the
    /// ordinary case, so the duplicate is not a distinct defect.
    #[test]
    fn a_repeated_slot_module_is_observably_the_single_module_list() {
        let shape = |modules: &[&str]| {
            let mut cfg = Config::default();
            cfg.slots[0] = modules.iter().map(|m| m.to_string()).collect();
            cfg.presentmon_enabled = true;
            cfg.presentmon_deferred = true;
            cfg.safe_mode = false;
            cfg
        };
        let unavailable = Snapshot::default();
        let duplicate = shape(&["FPS_CURRENT", "FPS_CURRENT"]);
        let single = shape(&["FPS_CURRENT"]);
        assert!(
            crate::config::validate(&duplicate).is_ok(),
            "the shipped contract: only PROC_HANG carries a uniqueness rule"
        );

        let duplicate_slots = Manager::new(&duplicate, [0; 4]);
        let single_slots = Manager::new(&single, [0; 4]);
        assert_eq!(
            effective_slot_modules(&duplicate_slots, &unavailable, &duplicate)[0],
            effective_slot_modules(&single_slots, &unavailable, &single)[0],
            "the unavailable duplicate and the unavailable single module land on the same panel"
        );
        assert_eq!(
            effective_slot_modules(&duplicate_slots, &unavailable, &duplicate)[0],
            "CLEAR"
        );
        let duplicate_shown = effective_slot_modules(&duplicate_slots, &unavailable, &duplicate);
        let single_shown = effective_slot_modules(&single_slots, &unavailable, &single);
        assert_eq!(
            provider_demand_bits(&duplicate_slots, &duplicate_shown, &duplicate),
            provider_demand_bits(&single_slots, &single_shown, &single),
            "the demand words are identical, so the duplicate cannot park or wake a provider"
        );
        assert_eq!(
            provider_consumer_bits(&duplicate_shown, &duplicate, false),
            provider_consumer_bits(&single_shown, &single, false),
            "the consumer words are identical"
        );

        // Cycling: a duplicate press lands on the same module, exactly as the
        // legal one-option list does; the selection index is invisible.
        let mut duplicate_cycler = Manager::new(&duplicate, [0; 4]);
        let mut single_cycler = Manager::new(&single, [0; 4]);
        for _ in 0..4 {
            assert_eq!(
                duplicate_cycler.cycle(0, 1),
                single_cycler.cycle(0, 1),
                "every press shows what the single-module list shows"
            );
        }
        // A reload of a duplicate list preserves the same displayed module.
        let before = duplicate_cycler.current(0);
        duplicate_cycler.apply(&duplicate);
        assert_eq!(
            duplicate_cycler.current(0),
            before,
            "reload preserves the module"
        );
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
        let snapshot = build_snapshot(
            &cfg,
            &resolve_ccd_topology(&cfg),
            &mut cpu,
            &telemetry,
            &DiscordState {
                connected: true,
                ..Default::default()
            },
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

    /// WO-REMOVE-ALERTS: with the acknowledgement detour retired, the physical release on
    /// button 4 (index 3) performs the ordinary slot cycle, and safe mode still refuses every
    /// terminate (the RIP safeguard is untouched).
    #[test]
    fn safe_mode_cycles_slot_four_normally_and_never_calls_terminate() {
        use std::cell::Cell;

        let cfg = Config {
            safe_mode: true,
            ..Config::default()
        };
        let mut slots = Manager::new(&cfg, [0; 4]);
        let kills = Cell::new(0);
        let mut hold = hang::HoldState::default();

        apply_hold_command_with(
            hang::HoldCommand::Cycle {
                index: 0,
                backward: false,
            },
            &mut hold,
            &mut slots,
            &cfg,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionReport::bare(hang::ActionOutcome::Success)
            },
        );
        assert_eq!(slots.current(0), "CPU_TEMP");

        apply_hold_command_with(
            hang::HoldCommand::Cycle {
                index: 3,
                backward: false,
            },
            &mut hold,
            &mut slots,
            &cfg,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionReport::bare(hang::ActionOutcome::Success)
            },
        );
        assert_eq!(
            slots.current(3),
            "POWER_LIMIT",
            "slot 4 cycles its own configured list, with no acknowledgement detour"
        );

        apply_hold_command_with(
            hang::HoldCommand::Terminate(crate::model::HungTarget {
                hwnd: 1,
                pid: 2,
                creation_time: 3,
                image_path: r"C:\Games\game.exe".into(),
                process_name: "game.exe".into(),
                title: "Game".into(),
                ..Default::default()
            }),
            &mut hold,
            &mut slots,
            &cfg,
            |_, _| {
                kills.set(kills.get() + 1);
                hang::ActionReport::bare(hang::ActionOutcome::Success)
            },
        );
        assert_eq!(kills.get(), 0);
    }

    #[test]
    fn dynamic_modules_fall_through_without_changing_selection() {
        let cfg = Config::default();
        let slots = Manager::new(&cfg, [0; 4]);
        let mut snapshot = Snapshot::default();
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
        dynamic.slots[0] = vec!["PROC_HANG".into(), "GPU_TEMP".into()];
        dynamic.slots[2] = vec!["GPU_TEMP".into()];
        let slots = Manager::new(&dynamic, [0; 4]);
        snapshot.hung.clear();
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &dynamic)[0],
            "GPU_TEMP",
            "the retired BOTTLENECK term is gone; PROC_HANG alone drives the fall-through"
        );
        assert_eq!(slots.current(0), "PROC_HANG", "selection is never mutated");

        let mut sole_hang = dynamic.clone();
        sole_hang.slots[0] = vec!["PROC_HANG".into()];
        let slots = Manager::new(&sole_hang, [0; 4]);
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &sole_hang)[0],
            "CLEAR",
            "an all-unavailable list keeps the internal CLEAR fallback"
        );
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

    /// WO-TEL-D2 (S2) + WO-U46C SUPERSESSION (the single permitted existing-test
    /// semantic change, declared in `WO-U46C-RECEIPT.md`): the shipped claims
    /// below are BYTE-IDENTICAL (every panel is checked, the dynamic fallback is
    /// skipped by the DISPLAY, and the config gates keep the module unavailable).
    /// What is superseded is the semantics the name encoded: a dynamic fallback
    /// the deferral skips is not merely ignored, it is a PENDING, REACHABLE
    /// consumer, so it DEMANDS the `presentmon` collection. The two demand rows
    /// added below are the new half of that claim; the last one (the WO's
    /// regression chain) is the defect cell and is RED without the chain leg.
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
            // WO-U46C (new half of the superseded claim): the panel shows the
            // fallback while the reading is unavailable, and the PresentMon
            // module the deferral is awaiting keeps the collection demanded, so
            // the fall-through is a transition and not a parking place.
            assert_ne!(
                provider_demand_bits(
                    &slots,
                    &effective_slot_modules(&slots, &unavailable, &cfg),
                    &cfg
                ) & crate::telemetry::extended_consumer_bit("presentmon"),
                0,
                "{module} unavailable: the deferred reading is awaited, so presentmon stays demanded"
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
        cfg.slots[0] = ["FPS_CURRENT", "PROC_HANG", "GPU_TEMP"]
            .map(str::to_string)
            .to_vec();
        let slots = Manager::new(&cfg, [0; 4]);
        let unavailable = Snapshot::default();
        assert_eq!(
            effective_slot_modules(&slots, &unavailable, &cfg)[0],
            "GPU_TEMP",
            "the deferral skip-list keeps PROC_HANG, the sole dynamic module, out"
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

        // WO-U46C THE DEFECT CELL, the WO's verbatim regression chain
        // `PROC_HANG FPS_CURRENT GPU_TEMP` with no hung target, no FPS and the
        // provider row the park publishes: the DISPLAY half is the shipped
        // priority, the DEMAND half is what the "skips dynamic fallbacks"
        // semantics used to deny. This cell is RED without the chain leg.
        let mut pending_cfg = Config::default();
        pending_cfg.slots[0] = ["PROC_HANG", "FPS_CURRENT", "GPU_TEMP"]
            .map(str::to_string)
            .to_vec();
        // The other panels are pinned PresentMon-free, so this cell measures the
        // chain leg alone (the default slot 3 selection demands by the raw leg).
        for slot in 1..4 {
            pending_cfg.slots[slot] = vec!["CLOCK".to_string()];
        }
        let pending = Manager::new(&pending_cfg, [0; 4]);
        let pending_shown = effective_slot_modules(&pending, &unavailable, &pending_cfg);
        assert_eq!(
            pending_shown[0], "GPU_TEMP",
            "the shipped display priority is unchanged for the regression chain"
        );
        assert_ne!(
            provider_demand_bits(&pending, &pending_shown, &pending_cfg)
                & crate::telemetry::extended_consumer_bit("presentmon"),
            0,
            "WO-U46C: the skipped FPS_CURRENT fallback is pending and reachable, so the slot the fall-through serves keeps presentmon demanded instead of parking it forever"
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
            // TC-1: the current publication states its generation and publish time.
            publish_generation: 1,
            publish_time: Some(Instant::now()),
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
                stamp: 0,
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
                stamp: 0,
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
                    stamp: 0,
                },
                &cfg,
                targets,
                available,
            ),
            hang::HoldCommand::None
        ));
    }

    /// WO-QA-03 (TC-4 follow-up F-b): the app-side half of the chain — the loss-cause
    /// label is display-only. A token entry on the provider-health record never stands in
    /// for the provider's availability and never carries a target, so the panel gains the
    /// named cause while the termination chain sees exactly what it saw before.
    #[test]
    fn the_loss_cause_label_carries_no_availability_and_no_target() {
        let loss = crate::telemetry::Update {
            generation: 1,
            providers: vec![
                ("Hung window detector".into(), false),
                ("probe-indeterminate-3".into(), true),
            ],
            ..Default::default()
        };
        let (targets, available) = current_hang_publication(&loss, 1);
        assert!(targets.is_empty(), "the loss cause publishes no target");
        assert!(
            !available,
            "the token entry is not the provider's availability"
        );
        let (targets, available) = current_hang_publication(&loss, 2);
        assert!(
            targets.is_empty() && !available,
            "a stale publication drops the provider and its label together"
        );

        let healthy = crate::telemetry::Update {
            generation: 1,
            providers: vec![
                ("Hung window detector".into(), true),
                ("probe-indeterminate-3".into(), true),
            ],
            ..Default::default()
        };
        let (targets, available) = current_hang_publication(&healthy, 1);
        assert!(
            targets.is_empty() && available,
            "an indeterminate cycle is available with zero targets"
        );
    }

    fn ec4_target(pid: u32) -> crate::model::HungTarget {
        crate::model::HungTarget {
            hwnd: 1,
            pid,
            creation_time: 3,
            image_path: r"C:\Games\game.exe".into(),
            process_name: "game.exe".into(),
            title: "Game".into(),
            // TC-1: the EC-4 parity scripts pin owner/audit ordering over 4.2 s of
            // synthetic tick time, not target freshness, so the publication stamp
            // stays ahead of that whole script (interval 2 s + tolerance 5 s = 7 s).
            publish_generation: 1,
            publish_time: Some(Instant::now() + Duration::from_secs(3600)),
        }
    }

    fn ec4_press(index: usize, down: bool, at: Instant) -> crate::input::Event {
        crate::input::Event {
            index,
            down,
            backward: false,
            canceled: false,
            at,
            source: "test",
            stamp: 0,
        }
    }

    fn ec4_cfg() -> Config {
        Config {
            hang_enabled: true,
            ..Config::default()
        }
    }

    /// EC-4 acceptance (1): a steady slot configuration resolves the owner once.
    /// The render-tick sites neither scan nor reach `set_owner`, so no audit fires.
    #[test]
    fn ec4_owner_cache_resolves_once_across_render_ticks() {
        let cfg = ec4_cfg();
        let slots = Manager::new(&cfg, [0; 4]);
        assert_eq!(proc_hang_owner(&slots), Some(2), "default owner is slot 2");
        let mut cache = HangOwner::default();
        let mut hold = hang::HoldState::default();
        let mut audits = 0;
        for _ in 0..100 {
            if cache.bind(&slots, &mut hold).is_some() {
                audits += 1;
            }
        }
        assert_eq!(audits, 0, "no set_owner audit across 100 unchanged ticks");
        assert_eq!(
            cache.resolutions, 1,
            "the O(slots) owner scan runs once, not at every tick site"
        );
    }

    /// EC-4 acceptance (2): moving PROC_HANG between slots yields exactly one
    /// `canceled-slot-owner` audit, by cycle and by reload, and nothing while the
    /// slots stay put.
    #[test]
    fn ec4_owner_cache_slot_move_emits_exactly_one_canceled_owner_audit() {
        let cfg = ec4_cfg();
        let target = ec4_target(2);
        let targets = std::slice::from_ref(&target);
        let started = Instant::now();
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut cache = HangOwner::default();
        let mut hold = hang::HoldState::default();
        assert!(cache.bind(&slots, &mut hold).is_none(), "startup bind");
        hold.event(ec4_press(2, true, started), &cfg, targets, true);
        // M4: cycling slot 2 moves PROC_HANG away, so the owner disappears.
        slots.cycle(2, 1);
        cache.invalidate();
        let audit = cache
            .bind(&slots, &mut hold)
            .expect("one canceled-slot-owner audit on the cycle move");
        assert_eq!(audit.outcome, "canceled-slot-owner");
        assert_eq!(audit.target.pid, 2);
        for _ in 0..50 {
            assert!(
                cache.bind(&slots, &mut hold).is_none(),
                "unchanged slots emit nothing"
            );
        }
        assert_eq!(cache.resolutions, 2, "startup + one cycle scan");
        // The press is retired, then PROC_HANG cycles back onto slot 2.
        assert!(matches!(
            hold.event(
                ec4_press(2, false, started + cfg.hang_hold),
                &cfg,
                targets,
                true
            ),
            hang::HoldCommand::Audit(_)
        ));
        slots.cycle(2, -1);
        cache.invalidate();
        assert!(
            cache.bind(&slots, &mut hold).is_none(),
            "owner restored with no press to cancel"
        );
        // M2: a reload that moves PROC_HANG from slot 2 to slot 0 cancels the live
        // press with its own audit; the owner site adds nothing, because
        // `cancel_reload` already cancelled the press (pre-EC-4 identical).
        hold.event(ec4_press(2, true, started), &cfg, targets, true);
        assert!(
            hold.cancel_reload()
                .is_some_and(|audit| audit.outcome == "canceled-config-reload"),
            "reload cancels the live press"
        );
        let mut moved = ec4_cfg();
        moved.slots[0] = vec!["PROC_HANG".into(), "FPS_CURRENT".into()];
        moved.slots[2] = vec!["DISK_IO".into(), "GPU_TEMP".into()];
        slots.apply(&moved);
        cache.invalidate();
        assert!(
            cache.bind(&slots, &mut hold).is_none(),
            "the reload owner site adds no audit"
        );
        assert_eq!(proc_hang_owner(&slots), Some(0), "owner moved to slot 0");
        assert_eq!(
            cache.resolutions, 4,
            "startup + cycle + restore + reload scans"
        );
        // The cancelled press retires, a new press binds on the reloaded owner,
        // and moving PROC_HANG off slot 0 cancels it exactly once (M4).
        assert!(matches!(
            hold.event(
                ec4_press(2, false, started + cfg.hang_hold),
                &cfg,
                targets,
                true
            ),
            hang::HoldCommand::Audit(_)
        ));
        hold.event(
            ec4_press(0, true, started + cfg.hang_hold),
            &moved,
            targets,
            true,
        );
        slots.cycle(0, 1);
        cache.invalidate();
        let audit = cache
            .bind(&slots, &mut hold)
            .expect("exactly one canceled-slot-owner audit on the move");
        assert_eq!(audit.outcome, "canceled-slot-owner");
        assert_eq!(audit.target.pid, 2);
        for _ in 0..50 {
            assert!(
                cache.bind(&slots, &mut hold).is_none(),
                "unchanged slots emit nothing"
            );
        }
        assert_eq!(cache.resolutions, 5, "one scan per configuration change");
    }

    /// EC-4 acceptance (3): the config-reload path keeps its semantics, and the
    /// EC-3 consumer input keeps reading the live slot modules.
    #[test]
    fn ec4_owner_cache_config_reload_semantics_preserved() {
        let cfg = ec4_cfg();
        let target = ec4_target(7);
        let targets = std::slice::from_ref(&target);
        let started = Instant::now();
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut cache = HangOwner::default();
        let mut hold = hang::HoldState::default();
        assert!(cache.bind(&slots, &mut hold).is_none(), "startup bind");
        hold.event(ec4_press(2, true, started), &cfg, targets, true);
        // Reload with an unchanged slot list: the reload audit only, no owner audit.
        assert!(
            hold.cancel_reload()
                .is_some_and(|audit| audit.outcome == "canceled-config-reload"),
            "reload cancels the live press"
        );
        slots.apply(&cfg);
        cache.invalidate();
        assert!(
            cache.bind(&slots, &mut hold).is_none(),
            "an unchanged owner must not emit an owner audit"
        );
        assert_eq!(cache.resolutions, 2, "startup + the reload scan");
        // EC-3: the consumer-bit input is still the slot modules, not the cache.
        let snapshot = Snapshot::default();
        assert_eq!(
            effective_slot_modules(&slots, &snapshot, &cfg)[2],
            "DISK_IO",
            "the dynamic fall-through is unchanged by the owner cache"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Tick(u64),
        CycleUi {
            slot: usize,
            delta: i64,
        },
        Button {
            index: usize,
            down: bool,
            at_ms: u64,
        },
        SetSlotModules {
            slot: usize,
            modules: &'static [&'static str],
        },
        SafeMode(bool),
        Reload,
    }

    fn ec4_audit_line(audit: &hang::Audit) -> String {
        format!("{}:{}", audit.outcome, audit.target.pid)
    }

    /// One visit to a slot-owner site: count it, run the driver's owner action,
    /// and record the audit it emits (if any).
    fn ec4_site<SITE>(
        out: &mut Vec<String>,
        sites: &mut u64,
        site: &mut SITE,
        slots: &mut Manager,
        hold: &mut hang::HoldState,
    ) where
        SITE: FnMut(&Manager, &mut hang::HoldState) -> Option<hang::Audit>,
    {
        *sites += 1;
        if let Some(audit) = site(slots, hold) {
            out.push(ec4_audit_line(&audit));
        }
    }

    fn ec4_record_command(
        out: &mut Vec<String>,
        command: hang::HoldCommand,
        hold: &mut hang::HoldState,
        slots: &mut Manager,
        cfg: &Config,
    ) {
        match &command {
            hang::HoldCommand::None => {}
            hang::HoldCommand::Cycle { index, backward } => {
                out.push(format!("cycle:{index}:{backward}"))
            }
            hang::HoldCommand::Terminate(target) => out.push(format!("terminate:{}", target.pid)),
            hang::HoldCommand::Audit(audit) => {
                out.push(format!("cmd-audit:{}", ec4_audit_line(audit)))
            }
        }
        // The injectable terminator keeps the probe away from any real process.
        apply_hold_command_with(command, hold, slots, cfg, |_, _| {
            hang::ActionReport::bare(hang::ActionOutcome::Success)
        });
    }

    /// EC-4 parity driver: replays the run loop's slot-owner site order for a
    /// script. `SITE` runs at every former `hang_hold.set_owner(proc_hang_owner
    /// (&slots))` call and `CHANGED` right after every slot-selection mutation, so
    /// the legacy (rescan at every site) and EC-4 (cached) drivers differ only by
    /// the cache. The site order mirrors `run()`: startup bind; per iteration the
    /// top-of-loop site + `reconcile`, the end-of-loop site + `reconcile` +
    /// `tick` command; per button event the command, the site; per UI cycle the
    /// cycle + the site (WO-REMOVE-ALERTS: the retired acknowledgement branch
    /// cycled nothing and reached no site, so every UI cycle now cycles and
    /// reaches its site); per reload `cancel_reload`, `slots.apply`, the site.
    fn ec4_drive_sites<CHANGED, SITE>(
        script: &[Step],
        cfg: &mut Config,
        slots: &mut Manager,
        hold: &mut hang::HoldState,
        targets: &[crate::model::HungTarget],
        mut changed: CHANGED,
        mut site: SITE,
    ) -> (Vec<String>, u64)
    where
        CHANGED: FnMut(),
        SITE: FnMut(&Manager, &mut hang::HoldState) -> Option<hang::Audit>,
    {
        let mut out = Vec::new();
        let mut sites = 0;
        let started = Instant::now();
        // app.rs startup bind.
        ec4_site(&mut out, &mut sites, &mut site, slots, hold);
        for step in script {
            match *step {
                Step::Tick(i) => {
                    let now = started + Duration::from_millis(100 * i);
                    // app.rs top-of-loop site, then reconcile.
                    ec4_site(&mut out, &mut sites, &mut site, slots, hold);
                    if let Some(audit) = hold.reconcile(cfg, targets, true) {
                        out.push(ec4_audit_line(&audit));
                    }
                    // app.rs end-of-loop site, reconcile, then the tick command.
                    ec4_site(&mut out, &mut sites, &mut site, slots, hold);
                    if let Some(audit) = hold.reconcile(cfg, targets, true) {
                        out.push(ec4_audit_line(&audit));
                    }
                    let command = hold.tick(now, cfg, targets, true);
                    ec4_record_command(&mut out, command, hold, slots, cfg);
                    // The command can cycle a slot (app.rs end-of-loop apply).
                    changed();
                }
                Step::CycleUi { slot, delta } => {
                    slots.cycle(slot, delta);
                    changed();
                    ec4_site(&mut out, &mut sites, &mut site, slots, hold);
                }
                Step::Button { index, down, at_ms } => {
                    let command = hold.event(
                        ec4_press(index, down, started + Duration::from_millis(at_ms)),
                        cfg,
                        targets,
                        true,
                    );
                    ec4_record_command(&mut out, command, hold, slots, cfg);
                    changed();
                    ec4_site(&mut out, &mut sites, &mut site, slots, hold);
                }
                Step::SetSlotModules { slot, modules } => {
                    cfg.slots[slot] = modules.iter().map(|module| module.to_string()).collect();
                }
                Step::SafeMode(on) => cfg.safe_mode = on,
                Step::Reload => {
                    if let Some(audit) = hold.cancel_reload() {
                        out.push(ec4_audit_line(&audit));
                    }
                    slots.apply(cfg);
                    changed();
                    ec4_site(&mut out, &mut sites, &mut site, slots, hold);
                }
            }
        }
        (out, sites)
    }

    /// EC-4 acceptance (4): audit-fire parity. One scripted sequence (render
    /// ticks, slot cycles, presses/releases, config reload, safe-mode toggles,
    /// slot-4 UI cycle, terminate window) is replayed through the pre-EC-4 site order
    /// (owner rescanned at every site) and the EC-4 order (owner resolved once per
    /// slot-configuration change); the recorded audit stream must be identical,
    /// and the EC-4 run must scan far fewer times than it visits sites.
    #[test]
    fn ec4_owner_audit_fire_parity_with_pre_cache_sites() {
        const HANG_ON_ZERO: [&str; 2] = ["PROC_HANG", "FPS_CURRENT"];
        const HANG_OFF_TWO: [&str; 2] = ["DISK_IO", "GPU_TEMP"];
        let script = [
            Step::Tick(1),
            Step::Tick(2),
            Step::Button {
                index: 1,
                down: true,
                at_ms: 250,
            },
            // A press that is not bound to the owner releases as a slot cycle (M3).
            Step::Button {
                index: 1,
                down: false,
                at_ms: 280,
            },
            Step::Button {
                index: 2,
                down: true,
                at_ms: 300,
            },
            Step::Tick(3),
            Step::Tick(4),
            Step::CycleUi { slot: 2, delta: 1 },
            Step::Tick(5),
            Step::CycleUi { slot: 2, delta: -1 },
            Step::Button {
                index: 2,
                down: false,
                at_ms: 900,
            },
            Step::Button {
                index: 2,
                down: true,
                at_ms: 1000,
            },
            Step::SetSlotModules {
                slot: 0,
                modules: &HANG_ON_ZERO,
            },
            Step::SetSlotModules {
                slot: 2,
                modules: &HANG_OFF_TWO,
            },
            Step::Reload,
            Step::Button {
                index: 2,
                down: false,
                at_ms: 1100,
            },
            Step::Tick(11),
            Step::Button {
                index: 0,
                down: true,
                at_ms: 1200,
            },
            Step::SafeMode(true),
            Step::Tick(13),
            Step::SafeMode(false),
            Step::Tick(14),
            Step::CycleUi { slot: 3, delta: 1 },
            Step::Tick(15),
            Step::Button {
                index: 0,
                down: false,
                at_ms: 1600,
            },
            Step::Button {
                index: 0,
                down: true,
                at_ms: 1700,
            },
            Step::Tick(20),
            Step::Tick(38),
            Step::Button {
                index: 0,
                down: false,
                at_ms: 3900,
            },
            Step::Tick(40),
            Step::CycleUi { slot: 0, delta: 1 },
            Step::Tick(41),
            Step::CycleUi { slot: 0, delta: -1 },
            Step::Tick(42),
        ];
        let target = ec4_target(2);
        let targets = std::slice::from_ref(&target);

        // Pre-EC-4 shape: every site rescans the owner and calls set_owner.
        let mut legacy_cfg = ec4_cfg();
        let mut legacy_slots = Manager::new(&legacy_cfg, [0; 4]);
        let mut legacy_hold = hang::HoldState::default();
        let mut legacy_mutations = 0;
        let (legacy, legacy_sites) = ec4_drive_sites(
            &script,
            &mut legacy_cfg,
            &mut legacy_slots,
            &mut legacy_hold,
            targets,
            || legacy_mutations += 1,
            |slots, hold| hold.set_owner(proc_hang_owner(slots)),
        );

        // EC-4 shape: the owner is resolved once per slot-configuration change.
        let mut cfg = ec4_cfg();
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut hold = hang::HoldState::default();
        let mut cached_mutations = 0;
        // The mutation hook marks the cache stale; the owner site consumes that
        // mark, exactly as the run loop pairs invalidate() with bind().
        let cache = std::cell::RefCell::new(HangOwner::default());
        let stale = std::cell::Cell::new(false);
        let (cached, cached_sites) = ec4_drive_sites(
            &script,
            &mut cfg,
            &mut slots,
            &mut hold,
            targets,
            || {
                cached_mutations += 1;
                stale.set(true);
            },
            |slots, hold| {
                if stale.get() {
                    stale.set(false);
                    cache.borrow_mut().invalidate();
                }
                cache.borrow_mut().bind(slots, hold)
            },
        );

        assert_eq!(cached_sites, legacy_sites, "same site order");
        assert_eq!(
            cached, legacy,
            "audit-fire parity: {cached:?} vs {legacy:?}"
        );
        assert_eq!(cached_mutations, legacy_mutations, "same mutations");
        // The script must be load-bearing, not an empty equality.
        assert!(
            legacy
                .iter()
                .any(|line| line.starts_with("canceled-slot-owner")),
            "owner moves must happen: {legacy:?}"
        );
        assert!(
            legacy
                .iter()
                .any(|line| line.starts_with("canceled-config-reload")),
            "the reload must cancel a live press: {legacy:?}"
        );
        assert!(
            legacy.iter().any(|line| line.starts_with("cycle:")),
            "a slot cycle command must run: {legacy:?}"
        );
        assert!(
            legacy.iter().any(|line| line.starts_with("terminate:")),
            "the hold must reach the terminate window: {legacy:?}"
        );
        assert!(
            legacy.iter().any(|line| line.starts_with("cmd-audit:")),
            "release audits must fire: {legacy:?}"
        );
        // EC-4 scan win on this script: 21 owner scans cover the 30 mutation
        // hooks (consecutive mutations between two sites coalesce) plus the
        // startup bind, while the pre-EC-4 shape scans at all 45 owner sites.
        // WO-REMOVE-ALERTS: the slot-4 UI cycle now reaches its own site and
        // mutation hook (the retired acknowledgement branch skipped both), so the
        // site/hook counts each gained one; the scan count, the audit stream, and
        // the parity are unchanged.
        // The steady-tick case is pinned by
        // ec4_owner_cache_resolves_once_across_render_ticks.
        assert_eq!(
            cache.borrow().resolutions,
            21,
            "one scan per slot-configuration change, never per tick site"
        );
        assert!(
            cache.borrow().resolutions < cached_sites,
            "{} scans over {cached_sites} owner sites",
            cache.borrow().resolutions
        );
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

    fn slots_showing(modules: [&str; 4]) -> [String; 4] {
        modules.map(String::from)
    }

    /// WO-TEL-D2-FIX: the RAW current selection behind a displayed-module set
    /// (every slot selects exactly the module the panel shows). The demand tests
    /// pass it beside the displayed array, so each shipped assertion keeps
    /// pinning exactly the derivation it pinned before.
    fn slots_current(modules: &[String; 4]) -> Manager {
        let mut cfg = Config::default();
        for (slot, module) in modules.iter().enumerate() {
            cfg.slots[slot] = vec![module.clone()];
        }
        Manager::new(&cfg, [0; 4])
    }

    #[test]
    fn consumer_set_covers_slots_and_the_preview_surface() {
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        // main_display 3 draws no fixed GPU/VRAM bar, so this table pins the
        // slot/preview contribution alone; the fixed-bar rule is covered
        // by `fixed_main_display_gpu_bars_keep_the_gpu_provider_consumed`.
        // WO-TEL-D3-B1 (L1) PIN LOCKSTEP, DECLARED (not silent, no deletion): the layout-3
        // baseline rows here asserted "every gated provider quiescent" and single-provider
        // equalities. `gpu` is a CORE reading now, so the layout rule contributes the `gpu`
        // bit in every row of this `main_display 3` fixture; the slot/preview legs are
        // byte-identical claims and stay exactly as shipped.
        // WO-REMOVE-ALERTS: the alert-category legs (`temperature_alerts`, `headset_alerts`)
        // are retired with the feature — no non-hang alarm can hold a provider at full
        // cadence any more — and the `safe_mode` DG-4 row is retaken from a plain config.
        let quiescent = Config {
            main_display: 3,
            headset_enabled: false,
            ..Config::default()
        };
        let gpu_bit = crate::telemetry::consumer_bit("gpu");
        assert_eq!(
            provider_consumer_bits(&idle, &quiescent, false),
            gpu_bit,
            "no slot or snapshot consumer => every OPTIONAL provider quiescent; the core gpu demand rides the layout rule (WO-TEL-D3-B1 L1)"
        );

        let disk = slots_showing(["CLOCK", "DISK_IO", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&disk, &quiescent, false),
            crate::telemetry::consumer_bit("performance") | gpu_bit,
            "DISK_IO consumes the performance provider (the core gpu demand is the L1 lockstep term)"
        );
        let temps = slots_showing(["CPU_TEMP", "GPU_TEMP", "THERMALS", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&temps, &quiescent, false),
            crate::telemetry::consumer_bit("hwinfo")
                | crate::telemetry::consumer_bit("lhm")
                | crate::telemetry::consumer_bit("gpu"),
            "temperature modules consume their sensor providers"
        );
        let status = slots_showing(["PROVIDER_STATUS", "CLOCK", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&status, &quiescent, false),
            crate::telemetry::ALL_CONSUMERS,
            "the availability surface consumes every gated provider"
        );
        assert_eq!(
            provider_consumer_bits(&idle, &quiescent, true),
            gpu_bit,
            "preview mirrors selected readings without activating hidden providers"
        );
        let safe = Config {
            main_display: 3,
            safe_mode: true,
            ..Config::default()
        };
        assert_eq!(
            provider_consumer_bits(&idle, &safe, false),
            gpu_bit,
            "L1 lockstep, DECLARED: the gated word is layout/slot driven, so it carries the core gpu demand even under `safe_mode`; the provider parks because `enabled(cfg, \"gpu\")` is false under safe mode (`telemetry.rs:369-371`), and the shipped DG-4 exclusion row stays true (accepted: `safe_mode` stops gpu by design)"
        );
        assert!(
            preview_surface_active(
                false,
                &Config::default(),
                &crate::backends::BackendState::Discovering
            ),
            "the auto policy shows the preview before a device is connected"
        );
    }

    #[test]
    fn fixed_main_display_gpu_bars_keep_the_gpu_provider_consumed() {
        // F-1: the reachable quiescent panel - layout 1 (the shipped default),
        // idle slots, headset off, and a
        // connected panel so the auto preview hides. The fixed GPU/VRAM bars
        // (renderer.rs layout 1 = `dashboard` default arm L215-232, layout 2 =
        // `main_display_2` L270-287) read `MetricKey::GPUUtilization`/
        // `VRAMUtilization` every frame, and only the native gpu provider emits
        // those keys, so the panel the user is watching is a gpu consumer and the
        // gate must not apply. Layout 3 (`main_display_3` L312-356) draws only
        // CPU/RAM/NET bars; WO-TEL-D3-B1 (L1) keeps the `gpu` bit set there too,
        // because GPU utilization and VRAM are CORE readings (see the supersession
        // note on the layout-3 leg below), so no layout starves a core reading.
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        let quiescent = |main_display| Config {
            main_display,
            headset_enabled: false,
            ..Config::default()
        };
        let gpu_bit = crate::telemetry::consumer_bit("gpu");
        assert_ne!(gpu_bit, 0, "gpu is a gated provider");
        assert!(
            !preview_surface_active(
                false,
                &quiescent(1),
                &BackendState::Connected {
                    kind: BackendKind::Hid
                }
            ),
            "a connected panel hides the auto preview: the F-1 scenario is reachable"
        );
        for main_display in [1, 2] {
            let cfg = quiescent(main_display);
            let bits = provider_consumer_bits(&idle, &cfg, false);
            assert_eq!(
                bits, gpu_bit,
                "layout {main_display} consumes gpu through its fixed bars and nothing else"
            );
            assert_eq!(
                crate::telemetry::sampling_interval(&cfg, bits, "gpu", cfg.telemetry_interval),
                cfg.telemetry_interval,
                "the visible GPU/VRAM bars keep the configured gpu cadence"
            );
        }
        // WO-TEL-D3-B1 (L1) SUPERSESSION of the WO-A1-03 F-1 layout-3 leg, DECLARED here and in
        // the receipt (prior content of these two asserts, quoted verbatim at `ec3df0d`:
        // `assert_eq!(bits & gpu_bit, 0, "layout 3 draws no GPU/VRAM bar, so gpu stays gated");`
        // and `assert!(sampling_interval(&cfg, bits, "gpu", cfg.telemetry_interval)
        // > cfg.telemetry_interval, "layout 3 keeps the EC-3 quiescence saving");`).
        // Rationale: `WO-TEL-D3-A-AUDIT.md` §C4 DG-1 ruled the clear a directive violation
        // (GPU utilization + VRAM stop), §10 ratified LAZY-1 into D3-B1. The layout-1/2 legs
        // above stay byte-identical, and the "no over-coverage" concern is answered by the P2
        // leg: the core demand keeps gpu at the CONFIGURED cadence, never faster.
        let cfg = quiescent(3);
        let bits = provider_consumer_bits(&idle, &cfg, false);
        assert_eq!(
            bits, gpu_bit,
            "layout 3 keeps the CORE gpu demand (GPU utilization + VRAM are core readings); the optional providers stay quiescent"
        );
        assert_eq!(
            crate::telemetry::sampling_interval(&cfg, bits, "gpu", cfg.telemetry_interval),
            cfg.telemetry_interval,
            "the core demand samples at the configured cadence: core-always never accelerates the gpu loop (P2)"
        );
    }

    /// WO-TEL-D3-B1 (L1, core tier) RED-FIRST falsifier of DG-1. GPU utilization and VRAM are
    /// CORE readings (user directive + `HANDOFF.md` §2A), so the `gpu` consumer bit is set by
    /// the layout rule for EVERY layout, including layout 3, which draws no GPU/VRAM bar. At
    /// `ec3df0d` the rule was `3 => &[]` (the accepted WO-A1-03 F-1 anti-starvation shape), so
    /// a layout-3 panel cleared the bit and the gpu loop parked (`telemetry.rs:1723` gate =>
    /// `1740-1743` park), stopping BOTH core readings = the ratified directive violation.
    /// This test fails at that baseline and passes on the always-demanded rule.
    ///
    /// The cadence leg is the P2 preservation proof: core-always demand must NOT accelerate
    /// the gpu sampler - a demanded provider samples at exactly `telemetry_interval`.
    #[test]
    fn core_gpu_and_vram_are_demanded_under_every_layout() {
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        let core_off = |main_display| Config {
            main_display,
            headset_enabled: false,
            ..Config::default()
        };
        let gpu_bit = crate::telemetry::consumer_bit("gpu");
        assert_ne!(gpu_bit, 0, "gpu is a gated provider");
        for main_display in [1, 2, 3] {
            let cfg = core_off(main_display);
            let bits = provider_consumer_bits(&idle, &cfg, false);
            assert_ne!(
                bits & gpu_bit,
                0,
                "layout {main_display} keeps the gpu provider DEMANDED: GPU utilization and VRAM are core readings, sampled under every slot choice"
            );
            assert_eq!(
                crate::telemetry::sampling_interval(&cfg, bits, "gpu", cfg.telemetry_interval),
                cfg.telemetry_interval,
                "layout {main_display} samples gpu at the configured cadence - core-always never accelerates it"
            );
            let word = bits | provider_demand_bits(&slots_current(&idle), &idle, &cfg);
            assert_ne!(
                word & gpu_bit,
                0,
                "the word the app PUBLISHES (gated | demand, `app.rs:186-192`/`328-333`) carries the core gpu demand under layout {main_display}"
            );
        }
        // The optional-off shape: every optional provider disabled, preview hidden (a connected
        // panel), gpu capability present. The core demand must survive a fully-lazy config.
        let lazy_off = Config {
            main_display: 3,
            gpu_provider: "auto".into(),
            presentmon_enabled: false,
            controller_enabled: false,
            audio_enabled: false,
            discord_enabled: false,
            network_probe_enabled: false,
            lhm_mode: "off".into(),
            headset_enabled: false,
            ..Config::default()
        };
        let bits = provider_consumer_bits(&idle, &lazy_off, false);
        assert_ne!(
            bits & gpu_bit,
            0,
            "a config with every OPTIONAL provider off keeps the gpu core demand"
        );
        assert_eq!(
            provider_demand_bits(&slots_current(&idle), &idle, &lazy_off),
            crate::telemetry::extended_consumer_bit("net"),
            "L1 touches the gated word only: the extended (reading-keyed) demand word keeps the shipped layout rule (net bars), so the presentmon/audio/controller/netif parks stay byte-identical"
        );
        assert!(
            !preview_surface_active(
                false,
                &lazy_off,
                &BackendState::Connected {
                    kind: BackendKind::Sdk
                }
            ),
            "the proof shape is reachable with the preview hidden (the demand is not preview-carried)"
        );
    }

    /// F-EC5-a fix (WO-A1-06-FIX), the audit-recommended assertion shape: an
    /// empty `per_lp` (Nt-query/decode failure) makes the Cache/Frequency split
    /// invalid, never a current 0% bar.
    #[test]
    fn ec5_split_is_invalid_when_per_lp_is_empty() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(11);
        let cache = cpu_split_metric(false, cpu::aggregate(&[], 0xffff), now);
        let freq = cpu_split_metric(false, cpu::aggregate(&[], 0xf0f0), now);
        assert!(!cache.valid, "empty per_lp must not publish a Cache value");
        assert!(
            !freq.valid,
            "empty per_lp must not publish a Frequency value"
        );
        assert!(
            !cache.stale && !freq.stale,
            "no data is unavailable, not stale"
        );
        assert_eq!(cache.updated, None);
        assert_eq!(freq.updated, None);
    }

    /// Normal (non-empty `per_lp`) data must not move at all: the aggregate
    /// result passes through unclamped as a current valid Metric, and the first
    /// sample's genuine zero delta stays valid.
    #[test]
    fn ec5_split_keeps_the_aggregate_values_on_normal_per_lp() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(11);
        let per_lp = [10.0, 20.0, 30.0, 40.0];
        let cache = cpu_split_metric(true, cpu::aggregate(&per_lp, 0b0011), now);
        let freq = cpu_split_metric(true, cpu::aggregate(&per_lp, 0b1100), now);
        assert!(cache.valid && freq.valid, "sampled data stays valid");
        assert!(
            !cache.stale && !freq.stale,
            "sampled data is current, not stale"
        );
        assert_eq!(cache.updated, Some(now));
        assert_eq!(freq.updated, Some(now));
        assert_eq!(cache.value, 15.0, "mean of the cache-mask members");
        assert_eq!(freq.value, 35.0, "mean of the frequency-mask members");
        let over = cpu_split_metric(true, cpu::aggregate(&[120.0, 140.0], 0b0011), now);
        assert_eq!(
            over.value, 130.0,
            "a normal reading passes through unclamped"
        );
    }

    /// Snapshot-level parity through the existing `build_snapshot` test infra:
    /// the split is valid exactly when the provider produced per-LP data.
    /// `update()` emptiness is host-deterministic (it depends only on
    /// `nt_query()`, the Nt status and `decode_samples`), so a fresh provider
    /// predicts the emptiness of the sample the snapshot builder walked.
    #[test]
    fn ec5_snapshot_split_validity_tracks_per_lp_data_presence() {
        let cfg = Config {
            safe_mode: true,
            ccd_source: "manual".into(),
            ccd_cache_processors: Some(vec![0]),
            ccd_frequency_processors: Some(vec![1]),
            ..Config::default()
        };
        let topology = resolve_ccd_topology(&cfg);
        assert!(topology.dual, "manual lists build the dual split");
        let mut cpu = cpu::CpuLoadProvider::new();
        let snapshot = build_snapshot(
            &cfg,
            &topology,
            &mut cpu,
            &crate::telemetry::Update::default(),
            &DiscordState::default(),
        );
        let has_data = !cpu::CpuLoadProvider::new().update().is_empty();
        assert_eq!(
            snapshot.cpu_cache_load.valid, has_data,
            "the Cache split's validity must follow per_lp presence"
        );
        assert_eq!(
            snapshot.cpu_freq_load.valid, has_data,
            "the Frequency split's validity must follow per_lp presence"
        );
        if has_data {
            assert_eq!(
                snapshot.cpu_cache_load.value, 0.0,
                "a first sample is a real zero delta, so it stays valid"
            );
        }
    }

    /// The schema line, byte-exact, on both `sys_busy_pct` paths.
    #[test]
    fn soak_resources_line_renders_exact_schema_with_none_busy() {
        let sample = SoakResourcesSample {
            pid: 4242,
            uptime_s: 3600,
            cpu_s: Some(12.5),
            priv_bytes: Some(1_048_576),
            ws_bytes: Some(2_097_152),
            handles: Some(99),
            sys_avail_mb: Some(8_192),
            sys_busy_pct: Some(37.5),
        };
        assert_eq!(
            soak_resources_line(&sample),
            "soak-resources: pid=4242 uptime_s=3600 cpu_s=12.50 priv_bytes=1048576 ws_bytes=2097152 handles=99 sys_avail_mb=8192 sys_busy_pct=37.5"
        );
        assert_eq!(
            soak_resources_line(&SoakResourcesSample {
                sys_busy_pct: None,
                ..sample
            }),
            "soak-resources: pid=4242 uptime_s=3600 cpu_s=12.50 priv_bytes=1048576 ws_bytes=2097152 handles=99 sys_avail_mb=8192 sys_busy_pct=none"
        );
    }

    /// Behavioral: the gate predicate fires at the 60 s boundary and never
    /// earlier. The loop wiring (the tracker seeded at start + the single gate
    /// site) is structural and pinned by
    /// `soak_resources_source_pin_of_60s_constant_and_single_emit_site`: the
    /// app loop has no time seam, it uses `Instant`, not an injected clock.
    #[test]
    fn soak_resources_cadence_gate_fires_at_60s_never_earlier() {
        for below in [
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_millis(59_999),
            Duration::from_nanos(59_999_999_999),
        ] {
            assert!(!soak_resources_due(below), "must not fire at {below:?}");
        }
        for at_or_above in [
            Duration::from_secs(60),
            Duration::from_millis(60_001),
            Duration::from_secs(3_600),
        ] {
            assert!(
                soak_resources_due(at_or_above),
                "must fire at {at_or_above:?}"
            );
        }
    }

    /// Structural pin, the TEL-B `include_str!` occurrence-count form, sliced at
    /// `mod tests` so the pin's own quoted literals cannot self-count. It pins:
    /// the 60 s constant exactly once, one cadence predicate, one schema
    /// formatter body, one key-order format string, and exactly one emit call
    /// site. Renaming a schema key, changing 60, duplicating the formatter or
    /// removing/duplicating the emit call breaks an assert.
    #[test]
    fn soak_resources_source_pin_of_60s_constant_and_single_emit_site() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let production = source.split("mod tests {").next().unwrap_or_default();
        assert_eq!(
            production
                .matches("const SOAK_RESOURCES_INTERVAL_SECS: u64 = 60;")
                .count(),
            1,
            "the 60 s constant exists exactly once"
        );
        assert_eq!(
            production.matches("fn soak_resources_due(").count(),
            1,
            "one cadence predicate"
        );
        assert_eq!(
            production
                .matches("fn soak_resources_line(sample: &SoakResourcesSample)")
                .count(),
            1,
            "one schema formatter body"
        );
        assert_eq!(
            production
                .matches("soak-resources: pid={pid} uptime_s={uptime_s} cpu_s=")
                .count(),
            1,
            "the key order lives in exactly one format string"
        );
        assert_eq!(
            production
                .matches("crate::log_info!(\"{}\", soak_resources_line(&sample));")
                .count(),
            1,
            "exactly one emit call site in the loop"
        );
    }

    /// WO-TEL-D2 (S1): the reading-keyed demand registry for the metering
    /// providers. Demand is keyed by the DISPLAYED reading (the effective slot
    /// module), never by a configured slot alternative. WO-REMOVE-ALERTS: the
    /// retired alert categories never entered this word (they read gated
    /// providers only), and their gated-word leg is deleted with the feature.
    #[test]
    fn reading_demand_maps_displayed_modules_to_extended_providers() {
        let quiescent = Config {
            main_display: 1,
            ..Config::default()
        };
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&idle), &idle, &quiescent),
            0,
            "layout 1 with no metering module displayed => every metering provider parks"
        );
        let audio = slots_showing(["CLOCK", "AUDIO", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&audio), &audio, &quiescent),
            crate::telemetry::extended_consumer_bit("audio"),
            "the volume reading demands the audio endpoint read"
        );
        let mic = slots_showing(["MIC_STATUS", "CLEAR", "CLEAR", "CLEAR"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&mic), &mic, &quiescent),
            crate::telemetry::extended_consumer_bit("audio"),
            "the mic reading shares the ONE audio collection op (P3 sharing, same bit)"
        );
        let pad = slots_showing(["CONTROLLER_BATTERY", "CLEAR", "CLEAR", "CLEAR"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&pad), &pad, &quiescent),
            crate::telemetry::extended_consumer_bit("controller"),
            "the controller reading demands xinput on select (demand-on-switch)"
        );
        assert_eq!(
            provider_demand_bits(&slots_current(&idle), &idle, &quiescent),
            0,
            "the same lists parked at index 0 park again (the registry reads the selection, not the config lists)"
        );
    }

    /// WO-TEL-D2 (S1, U-11): the netif demand rule — displayed NET_* modules
    /// demand netif; layouts 2/3 draw the NET IN/OUT bars and demand netif
    /// unconditionally; layout 1 with no NET module does not.
    #[test]
    fn reading_demand_maps_net_bar_layouts_and_net_modules_to_netif() {
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        let net_bit = crate::telemetry::extended_consumer_bit("net");
        assert_eq!(
            provider_demand_bits(
                &slots_current(&idle),
                &idle,
                &Config {
                    main_display: 1,
                    ..Config::default()
                }
            ),
            0,
            "layout 1 draws no NET bar"
        );
        assert_eq!(
            provider_demand_bits(
                &slots_current(&idle),
                &idle,
                &Config {
                    main_display: 2,
                    ..Config::default()
                }
            ),
            net_bit,
            "layout 2 draws the NET IN/OUT bars (renderer.rs:289-311)"
        );
        assert_eq!(
            provider_demand_bits(
                &slots_current(&idle),
                &idle,
                &Config {
                    main_display: 3,
                    ..Config::default()
                }
            ),
            net_bit,
            "layout 3 draws the NET IN/OUT bars (renderer.rs:335-356)"
        );
        let nets = slots_showing(["CLOCK", "NET_BOTH", "NET_GRAPH", "CLOCK"]);
        assert_eq!(
            provider_demand_bits(
                &slots_current(&nets),
                &nets,
                &Config {
                    main_display: 1,
                    ..Config::default()
                }
            ),
            net_bit,
            "the NET_* modules demand netif exactly once (dual display = shared op)"
        );
    }

    /// WO-TEL-D2 (S2, U-3): presentmon demand = displayed game readings,
    /// graph slots included (the `capture()` history consumer); the live-soak
    /// shape (FPS_CURRENT fallen through by `presentmon_deferred`) demands
    /// nothing, so zero-demand = no target poll and no child retries.
    #[test]
    fn reading_demand_maps_presentmon_backed_modules_including_graph_slots() {
        let pm_bit = crate::telemetry::extended_consumer_bit("presentmon");
        let quiescent = Config {
            main_display: 1,
            ..Config::default()
        };
        let game = slots_showing(["FPS_CURRENT", "FRAME_TIME", "GAME_NAME", "CLOCK"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&game), &game, &quiescent),
            pm_bit,
            "the game readings demand presentmon once (frame time feeds the graph surface too)"
        );
        let graphs = slots_showing(["FPS_GRAPH", "SESSION_SUMMARY", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&graphs), &graphs, &quiescent),
            pm_bit,
            "the FPS graph slot (the renderer graph-history consumer) and the session readings demand presentmon"
        );
        // The live-soak display state after the fall-throughs: slot 1 shows
        // NET_BOTH (not FPS_CURRENT), slot 2 DISK_IO, slot 3 THERMALS, slot 0
        // HEADSET_BATTERY => the extended word demands netif only.
        let fallen = slots_showing(["HEADSET_BATTERY", "NET_BOTH", "DISK_IO", "THERMALS"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&fallen), &fallen, &quiescent),
            crate::telemetry::extended_consumer_bit("net"),
            "the fallen-through display demands netif only: presentmon, audio and xinput park at the live soak"
        );
        let cleared = slots_showing(["CLEAR", "CLEAR", "CLEAR", "CLEAR"]);
        assert_eq!(
            provider_demand_bits(&slots_current(&cleared), &cleared, &quiescent),
            0,
            "no displayed reading => no demand"
        );
    }

    /// WO-TEL-D2 (S1, U-9 preserve): the preview keeps the shipped gated
    /// `ALL_CONSUMERS` early return; the published word is `gated | demand`,
    /// and every shipped gated fixture equality survives the union.
    #[test]
    fn preview_and_gated_word_keep_their_shipped_shape_under_the_extended_word() {
        let idle = slots_showing(["CLOCK", "CLEAR", "CLOCK", "CLOCK"]);
        let quiescent3 = Config {
            main_display: 3,
            headset_enabled: false,
            ..Config::default()
        };
        assert_eq!(
            provider_consumer_bits(&idle, &quiescent3, true),
            crate::telemetry::consumer_bit("gpu"),
            "preview mirrors the selected readings, without hidden provider demand"
        );
        assert_eq!(
            provider_demand_bits(&slots_current(&idle), &idle, &quiescent3),
            crate::telemetry::extended_consumer_bit("net"),
            "layout 3's preview shows the NET bars, so the extended word demands netif"
        );
        let quiescent1 = Config {
            main_display: 1,
            ..quiescent3
        };
        assert_eq!(
            provider_demand_bits(&slots_current(&idle), &idle, &quiescent1),
            0,
            "a layout 1 idle preview adds no optional metering demand"
        );
        let disk = slots_showing(["CLOCK", "DISK_IO", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&disk, &quiescent1, false)
                | provider_demand_bits(&slots_current(&disk), &disk, &quiescent1),
            crate::telemetry::consumer_bit("performance") | crate::telemetry::consumer_bit("gpu"),
            "the combined publish word keeps the gated fixture shape: DISK_IO demands performance, layout 1's fixed bars demand gpu (the shipped bare equality lives in the gated function, pinned by the EC-3 tests)"
        );
        let temps = slots_showing(["CPU_TEMP", "GPU_TEMP", "THERMALS", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&temps, &quiescent1, false)
                | provider_demand_bits(&slots_current(&temps), &temps, &quiescent1),
            crate::telemetry::consumer_bit("hwinfo")
                | crate::telemetry::consumer_bit("lhm")
                | crate::telemetry::consumer_bit("gpu"),
            "temperature displays demand only gated providers: the union adds no extended bits"
        );
    }

    /// WO-TEL-D2-FIX (F-AUD-1, C2 gate): the presentmon demand bit derives from
    /// the RAW current selection, not from the fall-through display. The
    /// fall-through is driven by the health row the park itself publishes
    /// (`providers/presentmon.rs:1455-1463`: `stop_and_report_cleanup` +
    /// `unavailable("no display consumer")` => `providers["presentmon"] = false`),
    /// so a demand bit taken from the fallen-through list self-locks: with
    /// `presentmon_deferred 1` (the `config.rs:299` default, and the live-soak
    /// config) a selected `FPS_CURRENT` could never re-arm the 1 s target poll,
    /// and a game launch would stay undetectable for the whole session.
    /// The DISPLAY fall-through stays byte-identical (asserted below), every
    /// other provider's derivation is untouched, and slot ALTERNATIVES stay
    /// demand-on-select: only `slots.current` demands.
    #[test]
    fn presentmon_demand_from_raw_selection_rearms_while_parked() {
        // The live-soak shape: layout 1 (no NET bar), the soak's presentmon
        // config, slot 2 (index 1) SELECTING `FPS_CURRENT` with a `NET_BOTH`
        // alternative.
        let cfg = Config {
            main_display: 1,
            ..Config::default()
        };
        assert!(
            cfg.presentmon_deferred,
            "config.rs:299 default = deferred 1"
        );
        assert!(cfg.presentmon_enabled, "config.rs:294 default = enabled");
        assert_eq!(cfg.presentmon_target_mode, "presenting", "config.rs:298");
        let mut soak = Config::default();
        // WO-U46C DECLARED FIXTURE LOCKSTEP (fixture lines added, every shipped
        // assertion below byte-identical): WO-U46C makes a slot that the
        // fall-through is serving demand the PresentMon module its chain skipped,
        // and the untouched DEFAULT slot 3 (`PROC_HANG FPS_1LOW ...`) is exactly
        // that shape, so it would hold the bit for its own reason. The shipped
        // claim below is about slot 2's own chain, so the other panels are pinned
        // to a PresentMon-free reading here; no assertion is weakened.
        soak.slots[0] = vec!["CLOCK".to_string()];
        soak.slots[2] = vec!["CLOCK".to_string()];
        soak.slots[3] = vec!["CLOCK".to_string()];
        soak.slots[1] = vec!["FPS_CURRENT".to_string(), "NET_BOTH".to_string()];
        let slots = Manager::new(&soak, [0; 4]);
        assert_eq!(slots.current(1), "FPS_CURRENT", "the RAW current selection");
        let mut snapshot = Snapshot::default();
        snapshot.providers.insert("presentmon".to_string(), false);
        let shown = effective_slot_modules(&slots, &snapshot, &cfg);
        assert_eq!(
            shown[1], "NET_BOTH",
            "the fall-through DISPLAY is unchanged: the panel keeps its shipped fallback"
        );
        let pm_bit = crate::telemetry::extended_consumer_bit("presentmon");
        let net_bit = crate::telemetry::extended_consumer_bit("net");
        let bits = provider_demand_bits(&slots, &shown, &cfg);
        assert_ne!(
            bits & pm_bit,
            0,
            "F-AUD-1 RE-ARM: a SELECTED FPS module keeps presentmon demanded while the park publishes health=false, so the worker re-enters reconcile and a game launch is detected"
        );
        assert_ne!(
            bits & net_bit,
            0,
            "the fallen-through display still demands netif: every other derivation byte-untouched"
        );
        assert_eq!(
            bits & crate::telemetry::extended_consumer_bit("audio"),
            0,
            "audio demand stays display-derived (layout 1 draws no NET bar either)"
        );
        assert_eq!(
            bits & crate::telemetry::extended_consumer_bit("controller"),
            0,
            "controller demand stays display-derived"
        );
        // Slot ALTERNATIVES stay demand-on-select: cycling slot 2 onto the
        // `NET_BOTH` alternative releases the presentmon demand again.
        let mut cycled = Manager::new(&soak, [0; 4]);
        cycled.cycle(1, 1);
        assert_eq!(
            cycled.current(1),
            "NET_BOTH",
            "the alternative, now selected"
        );
        let shown2 = effective_slot_modules(&cycled, &snapshot, &cfg);
        assert_eq!(
            provider_demand_bits(&cycled, &shown2, &cfg) & pm_bit,
            0,
            "demand-on-select: an FPS alternative that is NOT the current selection demands no presentmon"
        );
        // The shipped enablement guard keeps its meaning: presentmon off => no demand.
        let off = Config {
            main_display: 1,
            presentmon_enabled: false,
            ..Config::default()
        };
        assert_eq!(
            provider_demand_bits(
                &slots,
                &effective_slot_modules(&slots, &snapshot, &off),
                &off,
            ) & pm_bit,
            0,
            "presentmon_enabled 0 => no demand bit (the enablement guard, not the selection)"
        );
        // BOUND leg (the shipped activation-resume bound, asserted numerically on
        // the production source): the demand word reaches the worker through the
        // app's render gate (<= 100 ms, `app.rs:318`/`app.rs:328`), the worker
        // reads the published word exactly once per loop iteration and sleeps the
        // shipped 50 ms, and the 1 s `TARGET_POLL` gate is already due after the
        // park (`last_target_poll` ages, the park never stamps it) => re-arm
        // within render 100 ms + worker 50 ms = 150 ms, target poll on that tick.
        let source = include_str!("providers/presentmon.rs").replace("\r\n", "\n");
        let start = source.find("fn run(\n").expect("the shipped worker exists");
        let rest = &source[start..];
        let end = rest
            .find("\n    let mut stderr = String::new();")
            .expect("the shipped loop-end marker closes the worker loop");
        let run = &rest[..end];
        assert_eq!(
            run.matches("crate::telemetry::demanded(").count(),
            1,
            "the worker reads the demand word exactly once per loop iteration (the re-arm input)"
        );
        assert_eq!(
            run.matches("consumers.load(Ordering::Relaxed)").count(),
            1,
            "one published-word read per iteration (no extra wakeups)"
        );
        let gate = run
            .find("if !crate::telemetry::demanded(")
            .expect("the shipped demand gate");
        let reconcile = run.find("capture.reconcile(").expect("the reconcile call");
        assert!(
            gate < reconcile,
            "a set demand bit re-enters reconcile on the next loop tick (the park never reaches it)"
        );
        assert_eq!(
            run.matches("std::thread::sleep(Duration::from_millis(50))")
                .count(),
            4,
            "every worker path sleeps the shipped 50 ms (the 50 ms half of the re-arm bound)"
        );
        assert!(
            source.contains("const TARGET_POLL: Duration = Duration::from_secs(1);"),
            "the 1 s target-poll gate is due on re-entry (the park never stamps `last_target_poll`)"
        );
        assert!(
            cfg.render_interval <= Duration::from_millis(100),
            "the app publishes the demand word at most every 100 ms => re-arm bound = 150 ms"
        );
    }

    /// WO-U46C (PR review P2, "dynamic fallback selection can leave PresentMon
    /// permanently parked"): the complete transition on the regression slot
    /// `PROC_HANG FPS_CURRENT GPU_TEMP`. With no hung target and no FPS reading
    /// the panel displays the `GPU_TEMP` fallback, and the `FPS_CURRENT` module
    /// the deferral skipped is a PENDING, REACHABLE PresentMon-backed fallback:
    /// the panel will show it the moment FPS is current, so it must demand the
    /// `presentmon` collection. Step 3 is the pre-fix RED cell
    /// (`u46c-prefix-reds.log`); steps 4-6 then prove the demand arms collection,
    /// the display transitions, and the demand releases (park resumes) when no
    /// panel displays/selects/awaits a PresentMon-backed module.
    #[test]
    fn a_pending_presentmon_backed_dynamic_fallback_demands_collection_and_releases() {
        let mut cfg = Config {
            main_display: 1,
            ..Config::default()
        };
        assert!(cfg.presentmon_deferred, "the shipped default is deferred 1");
        assert!(cfg.presentmon_enabled, "the shipped default is enabled");
        assert_eq!(cfg.presentmon_target_mode, "presenting");
        // The regression chain on slot 1; the other panels carry no PresentMon
        // reading, so every row below measures this one slot.
        cfg.slots[0] = ["PROC_HANG", "FPS_CURRENT", "GPU_TEMP"]
            .map(str::to_string)
            .to_vec();
        for slot in 1..4 {
            cfg.slots[slot] = vec!["CLOCK".to_string()];
        }
        let slots = Manager::new(&cfg, [0; 4]);
        let pm_bit = crate::telemetry::extended_consumer_bit("presentmon");
        assert_ne!(pm_bit, 0, "presentmon carries an extended demand bit");

        // (1) no hung target, no FPS reading, and the health row the park publishes.
        let mut snapshot = Snapshot::default();
        snapshot.providers.insert("presentmon".to_string(), false);
        assert!(snapshot.hung.is_empty(), "step 1: no hung target");
        assert!(!snapshot.game.fps.valid, "step 1: no FPS reading");
        assert_eq!(slots.current(0), "PROC_HANG", "the RAW selection");

        // (2) the fallback priority is preserved: GPU_TEMP is what the panel shows.
        let shown = effective_slot_modules(&slots, &snapshot, &cfg);
        assert_eq!(
            shown[0], "GPU_TEMP",
            "step 2: the deferral walks past the unavailable FPS_CURRENT onto the next configured module"
        );

        // (3) PRE-FIX RED: the pending, reachable fallback demands collection.
        assert_ne!(
            provider_demand_bits(&slots, &shown, &cfg) & pm_bit,
            0,
            "step 3: a PENDING, REACHABLE PresentMon-backed fallback demands collection, so the park arm re-arms and FPS can become current"
        );

        // (4) the demand arms collection: the provider is present with a fresh valid metric.
        snapshot.providers.insert("presentmon".to_string(), true);
        snapshot.game.active = true;
        snapshot.game.fps = crate::model::Metric::valid(60.0, SystemTime::now());

        // (5) the display transitions to the FPS module.
        let shown = effective_slot_modules(&slots, &snapshot, &cfg);
        assert_eq!(
            shown[0], "FPS_CURRENT",
            "step 5: the pending fallback resolves, so FPS_CURRENT is displayed"
        );
        assert_ne!(
            provider_demand_bits(&slots, &shown, &cfg) & pm_bit,
            0,
            "the displayed FPS reading keeps the shipped display-derived demand"
        );

        // (6a) cycle-away: the slot selects GPU_TEMP, so nothing is pending.
        let mut cycled = Manager::new(&cfg, [0; 4]);
        cycled.cycle(0, 1);
        cycled.cycle(0, 1);
        assert_eq!(
            cycled.current(0),
            "GPU_TEMP",
            "step 6: the raw selection is the fallback itself"
        );
        let shown = effective_slot_modules(&cycled, &snapshot, &cfg);
        assert_eq!(shown[0], "GPU_TEMP", "the panel shows its own selection");
        assert_eq!(
            provider_demand_bits(&cycled, &shown, &cfg) & pm_bit,
            0,
            "step 6a: no panel displays/selects/awaits a PresentMon-backed module => the demand releases and the park resumes"
        );

        // (6b) removal: the chain holds no PresentMon-backed fallback at all.
        cfg.slots[0] = ["PROC_HANG", "GPU_TEMP"].map(str::to_string).to_vec();
        let trimmed = Manager::new(&cfg, [0; 4]);
        assert_eq!(
            trimmed.current(0),
            "PROC_HANG",
            "the dynamic module, selected"
        );
        let shown = effective_slot_modules(&trimmed, &snapshot, &cfg);
        assert_eq!(
            shown[0], "GPU_TEMP",
            "the hang fall-through lands on the only surviving module"
        );
        assert_eq!(
            provider_demand_bits(&trimmed, &shown, &cfg) & pm_bit,
            0,
            "step 6b: a chain with no PresentMon-backed fallback demands nothing (no eager collection)"
        );
    }

    /// WO-U46C (the negative cells): the pending-fallback demand stays inside the
    /// shipped enablement guard and inside reachability. `presentmon_enabled 0`,
    /// `presentmon_target_mode disabled` and `safe_mode` never set the bit; a
    /// chain with no PresentMon-backed module never sets it; and a PresentMon
    /// module that is only an ALTERNATIVE of a selection the panel can display -
    /// never skipped by a fall-through - stays demand-on-select, so collection
    /// stays parked (the deferred-mode semantics, no eager provider).
    #[test]
    fn a_pending_presentmon_fallback_never_demands_under_the_gates_or_without_a_fallback() {
        fn regression_word(cfg: &Config) -> (u64, [String; 4]) {
            let mut cfg = cfg.clone();
            cfg.slots[0] = ["PROC_HANG", "FPS_CURRENT", "GPU_TEMP"]
                .map(str::to_string)
                .to_vec();
            for slot in 1..4 {
                cfg.slots[slot] = vec!["CLOCK".to_string()];
            }
            let slots = Manager::new(&cfg, [0; 4]);
            let snapshot = Snapshot::default();
            let shown = effective_slot_modules(&slots, &snapshot, &cfg);
            (provider_demand_bits(&slots, &shown, &cfg), shown)
        }
        let pm_bit = crate::telemetry::extended_consumer_bit("presentmon");
        let base = Config {
            main_display: 1,
            ..Config::default()
        };

        let (bits, shown) = regression_word(&base);
        assert_eq!(shown[0], "GPU_TEMP", "the regression display shape");
        assert_ne!(
            bits & pm_bit,
            0,
            "control: enabled + deferred => the pending fallback demands collection"
        );

        let off = Config {
            main_display: 1,
            presentmon_enabled: false,
            ..Config::default()
        };
        let (bits, shown) = regression_word(&off);
        assert_eq!(
            shown[0], "GPU_TEMP",
            "presentmon off => the same display shape"
        );
        assert_eq!(
            bits & pm_bit,
            0,
            "presentmon_enabled 0 => the bit is NEVER set: no target poll, no child retries"
        );

        let disabled = Config {
            main_display: 1,
            presentmon_target_mode: "disabled".into(),
            ..Config::default()
        };
        let (bits, _) = regression_word(&disabled);
        assert_eq!(
            bits & pm_bit,
            0,
            "presentmon_target_mode disabled => the bit is NEVER set"
        );

        let safe = Config {
            main_display: 1,
            safe_mode: true,
            ..Config::default()
        };
        let (bits, _) = regression_word(&safe);
        assert_eq!(bits & pm_bit, 0, "safe_mode => the bit is NEVER set");

        // A chain with no PresentMon-backed module: the hang fall-through has
        // nothing to await, so nothing is collected.
        let mut plain = base.clone();
        plain.slots[0] = ["PROC_HANG", "GPU_TEMP"].map(str::to_string).to_vec();
        for slot in 1..4 {
            plain.slots[slot] = vec!["CLOCK".to_string()];
        }
        let plain_slots = Manager::new(&plain, [0; 4]);
        let plain_shown = effective_slot_modules(&plain_slots, &Snapshot::default(), &plain);
        assert_eq!(
            plain_shown[0], "GPU_TEMP",
            "the dynamic fall-through still serves the panel"
        );
        assert_eq!(
            provider_demand_bits(&plain_slots, &plain_shown, &plain),
            0,
            "no PresentMon-backed fallback in any chain => the extended word stays empty (no eager demand)"
        );

        // Demand-on-select preserved: GPU_TEMP is the selection and displays, so
        // the FPS_CURRENT behind it is an alternative, never a skipped fallback.
        let mut alternative = base.clone();
        alternative.slots[0] = ["GPU_TEMP", "FPS_CURRENT"].map(str::to_string).to_vec();
        for slot in 1..4 {
            alternative.slots[slot] = vec!["CLOCK".to_string()];
        }
        let alt_slots = Manager::new(&alternative, [0; 4]);
        let alt_snapshot = Snapshot::default();
        let alt_shown = effective_slot_modules(&alt_slots, &alt_snapshot, &alternative);
        assert_eq!(alt_slots.current(0), "GPU_TEMP", "the selection");
        assert_eq!(
            alt_shown[0], "GPU_TEMP",
            "the selection resolves, so no fall-through runs"
        );
        assert_eq!(
            provider_demand_bits(&alt_slots, &alt_shown, &alternative) & pm_bit,
            0,
            "demand-on-select: an FPS alternative that is not the selection and was never skipped demands no presentmon"
        );
    }

    #[test]
    fn idle_production_dispatch_keeps_hang_owner_cached_until_a_real_cycle() {
        let cfg = Config::default();
        let mut slots = Manager::new(&cfg, [0; 4]);
        let mut hold = hang::HoldState::default();
        let mut owner = HangOwner::default();
        owner.bind(&slots, &mut hold);
        for _ in 0..1000 {
            apply_hold_command(
                hang::HoldCommand::None,
                &mut hold,
                &mut slots,
                &cfg,
                &mut owner,
            );
            owner.bind(&slots, &mut hold);
        }
        assert_eq!(owner.resolutions, 1);
        apply_hold_command(
            hang::HoldCommand::Cycle {
                index: 2,
                backward: false,
            },
            &mut hold,
            &mut slots,
            &cfg,
            &mut owner,
        );
        owner.bind(&slots, &mut hold);
        assert_eq!(owner.resolutions, 2);
    }

    #[test]
    fn preview_and_device_loss_never_demand_undisplayed_optional_readings() {
        let cfg = Config::default();
        let modules = slots_showing(["CLOCK", "CPU_LOAD", "RAM_USAGE", "CLOCK"]);
        for state in [
            BackendState::Discovering,
            BackendState::Connected {
                kind: BackendKind::Sdk,
            },
        ] {
            assert_eq!(
                provider_consumer_bits(&modules, &cfg, preview_surface_active(false, &cfg, &state)),
                crate::telemetry::consumer_bit("gpu"),
            );
        }
        let displayed = slots_showing(["HEADSET_BATTERY", "CLOCK", "CLOCK", "CLOCK"]);
        assert_eq!(
            provider_consumer_bits(&displayed, &cfg, true),
            crate::telemetry::consumer_bit("gpu") | crate::telemetry::consumer_bit("headset"),
        );
    }

    #[test]
    fn discord_has_demand_only_while_selected_without_rewriting_user_preferences() {
        let cfg = Config {
            discord_enabled: true,
            discord_client_id: "123456789".into(),
            ..Default::default()
        };
        let mut hidden = slots_showing(["CLOCK", "CPU_LOAD", "RAM_USAGE", "CLOCK"]);
        assert!(!discord_display_demand(&hidden));
        assert!(!discord_runtime_config(&cfg, discord_display_demand(&hidden)).discord_enabled);
        hidden[2] = "DISCORD".into();
        assert!(discord_runtime_config(&cfg, discord_display_demand(&hidden)).discord_enabled);
        assert!(cfg.discord_enabled);
        let disabled = Config {
            discord_enabled: false,
            ..cfg.clone()
        };
        assert!(!discord_runtime_config(&disabled, true).discord_enabled);
    }

    #[test]
    fn failed_soak_counters_are_unavailable_not_measured_zero() {
        let failed = SoakResourcesSample {
            pid: 1,
            uptime_s: 60,
            cpu_s: None,
            priv_bytes: None,
            ws_bytes: None,
            handles: None,
            sys_avail_mb: None,
            sys_busy_pct: None,
        };
        assert_eq!(soak_resources_line(&failed), "soak-resources: pid=1 uptime_s=60 cpu_s=none priv_bytes=none ws_bytes=none handles=none sys_avail_mb=none sys_busy_pct=none");
        let measured = SoakResourcesSample {
            cpu_s: Some(0.0),
            priv_bytes: Some(0),
            ..failed
        };
        assert!(soak_resources_line(&measured).contains("cpu_s=0.00 priv_bytes=0 "));
    }
}
