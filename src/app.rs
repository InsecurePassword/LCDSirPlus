//! Application orchestration: config lifecycle, providers, render loop,
//! backend wiring, UI events, and hot reload.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::backends::{Backend, BackendKind, BackendState, LossCause, Message};
use crate::config::Config;
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
    let mut backend = Backend::spawn(backend_kind, &cfg);
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

    let mut watcher = ConfigWatcher::new(&watched_files);

    let mut last_telemetry = Instant::now() - cfg.telemetry_interval;
    let mut last_render = Instant::now() - cfg.render_interval;
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
                        watcher.replace(&files);
                        if backend_changed {
                            recovery.suspend();
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

        if now.duration_since(last_render) >= cfg.render_interval {
            last_render = now;
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
