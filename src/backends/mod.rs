//! LCD backend worker: owns the physical device thread, serializes all
//! device I/O, and exposes a message surface to the application.
//!
//! Backend selection is process-presence-driven: `auto` uses the SDK while
//! exact `LCore.exe` is running and direct HID while competing processes are absent.
//! The worker suppresses unchanged frames and reconnects with bounded,
//! capped backoff. Device loss emits canceled button releases.

pub mod g13;
pub mod hid;
pub mod sdk;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use crate::config::Config;
use crate::input::{ButtonTracker, Event};
#[cfg(test)]
use g13::G13_OUTPUT_REPORT_LENGTH;
use g13::{pack_report, parse_input};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Auto,
    Sdk,
    Hid,
    Virtual,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Auto => "auto",
            BackendKind::Sdk => "sdk",
            BackendKind::Hid => "hid",
            BackendKind::Virtual => "virtual",
        }
    }
}

/// Named loss cause behind a `Disconnected` publication (RC-1). The cause
/// classifies why the device connection was lost; recovery-side outcomes keep
/// their own journal event labels, so the two are never written alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossCause {
    DeviceAbsent,
    OpenFailed {
        code: u32,
    },
    CallTimeout,
    CircuitOpen,
    TransferIncomplete {
        code: u32,
    },
    OwnerConflict,
    ProbeFail,
    ShutdownRequested,
    /// RC-2 producer: the recovery controller, not a transport site. The worker
    /// claimed a physical connection and stopped publishing health while frames
    /// kept flowing, past the liveness deadline. One-shot per stage; the named
    /// cause replaces a thread kill (the original's measured weakness).
    WorkerStalled,
}

impl LossCause {
    /// All causes producers may emit; used by the label-whitelist test.
    #[cfg(test)]
    pub fn all() -> [LossCause; 9] {
        [
            LossCause::DeviceAbsent,
            LossCause::OpenFailed { code: 0 },
            LossCause::CallTimeout,
            LossCause::CircuitOpen,
            LossCause::TransferIncomplete { code: 0 },
            LossCause::OwnerConflict,
            LossCause::ProbeFail,
            LossCause::ShutdownRequested,
            LossCause::WorkerStalled,
        ]
    }

    /// Short journal-safe code (bounded by the 500-char detail clamp).
    pub fn short(self) -> String {
        match self {
            Self::DeviceAbsent => "device-absent".to_string(),
            Self::OpenFailed { code } => format!("open-failed({code:#010x})"),
            Self::CallTimeout => "call-timeout".to_string(),
            Self::CircuitOpen => "circuit-open".to_string(),
            Self::TransferIncomplete { code } => format!("transfer-incomplete({code})"),
            Self::OwnerConflict => "owner-conflict".to_string(),
            Self::ProbeFail => "probe-fail".to_string(),
            Self::ShutdownRequested => "shutdown-requested".to_string(),
            Self::WorkerStalled => "worker-stalled".to_string(),
        }
    }
}

/// Transport failure carrying the named cause plus its human-readable detail;
/// numeric OS codes ride alongside at their origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LossError {
    pub cause: LossCause,
    pub detail: String,
}

impl LossError {
    pub fn new(cause: LossCause, detail: String) -> Self {
        Self { cause, detail }
    }
}

impl std::fmt::Display for LossError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.cause.short(), self.detail)
    }
}

/// Single construction point for disconnect publications: the human-readable
/// reason is derived from cause + code + origin detail.
fn disconnect(error: &LossError) -> Message {
    Message::State(BackendState::Disconnected {
        cause: error.cause,
        reason: error.to_string(),
    })
}

/// Close failure on the shutdown exit: the loss is the requested shutdown.
fn shutdown_close_error(error: &LossError) -> LossError {
    LossError::new(
        LossCause::ShutdownRequested,
        format!("close on shutdown failed: {}", error.detail),
    )
}

/// Close failure after a submit/poll loss: keep the origin cause, record the
/// close outcome in the detail only.
fn append_close_failure(mut error: LossError, close_error: &LossError) -> LossError {
    error.detail.push_str("; close failed: ");
    error.detail.push_str(&close_error.detail);
    error
}

/// RC-6 ladder-terminal threshold: how many consecutive discovery passes that
/// yield zero device candidates stop the worker from attempting device opens.
///
/// Derived from the existing config pair, no new keys: `logitech_reconnect`
/// default 5 s and `logitech_reconnect_max` default 60 s (`config.rs`
/// L270-L271) give N = reconnect_max / reconnect = 60 / 5 = 12, so N base
/// intervals cover at least the full capped ladder. The real ladder sleeps
/// double, so 12 passes cover 5+10+20+40+8*60 = 555 s, past `reconnect_max`.
pub const ABSENT_TERMINATION_PASSES: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendState {
    Discovering,
    Connected {
        kind: BackendKind,
    },
    Disconnected {
        cause: LossCause,
        reason: String,
    },
    /// Ladder terminal after `ABSENT_TERMINATION_PASSES` zero-candidate
    /// discovery passes: paired with `LossCause::DeviceAbsent` as a named
    /// published state (not a masked failure). Device opens are skipped while
    /// frame submits, shutdown, and reconnect wakes stay live, and the preview
    /// policy matches `Disconnected` so the panel is never blanked.
    Absent {
        reason: String,
    },
    ShutDown,
}

pub enum Message {
    State(BackendState),
    Buttons(Vec<Event>),
    PhysicalHealthy(BackendKind),
    ReconnectStarted,
}

pub struct Backend {
    commands: Arc<CommandQueue>,
    pub msg_rx: Receiver<Message>,
    thread: Option<JoinHandle<()>>,
}

type Pixels = Box<[u8; crate::model::WIDTH * crate::model::HEIGHT]>;
/// A frame's whole pixel content by value, so the EC-1 identity cache is one
/// resident buffer inside the queue instead of a per-frame allocation.
type FrameBytes = [u8; crate::model::WIDTH * crate::model::HEIGHT];

struct CommandQueue {
    latest: Mutex<Option<Pixels>>,
    /// EC-1 identity cache: the content of the last frame accepted into
    /// `latest` since the last device-visible change. A submission carrying
    /// byte-identical content is skipped (no enqueue, no wake): the panel
    /// already shows that frame, so the pack + device write + queue work is
    /// what disappears. Identity is over the pre-transform pixels;
    /// orientation/invert are fixed for the life of a `Backend`, so identical
    /// input cannot produce different device output, and a config change
    /// spawns a fresh queue (empty cache). Invalidated on `request_reconnect`
    /// (spec: non-optional, the device may reset out of band), on the blanking
    /// device closes after a submit/poll loss, and structurally on
    /// reconfigure/stop, which spawn a brand-new queue.
    last_accepted: Mutex<Option<FrameBytes>>,
    shutdown: AtomicBool,
    reconnect: AtomicBool,
    wake: SyncSender<()>,
    /// EC-1 acceptance seam: counts the wake attempts issued by frame
    /// submission, per queue, so a suppressed frame shows up as a zero.
    #[cfg(test)]
    wake_sends: AtomicUsize,
}

impl CommandQueue {
    fn submit(&self, pixels: Pixels) {
        {
            let mut last = self.last_accepted.lock().unwrap();
            if last.as_ref() == Some(&*pixels) {
                // Intended display retention, not an error path: no state
                // message, no slot write and no wake happen here.
                return;
            }
            *last = Some(*pixels);
        }
        *self.latest.lock().unwrap() = Some(pixels);
        #[cfg(test)]
        self.wake_sends.fetch_add(1, Ordering::SeqCst);
        let _ = self.wake.try_send(());
    }

    /// EC-1: forget the cached visible content so the next submission is
    /// enqueued even when it is identical. Required wherever the panel stops
    /// showing the cached frame (a blanking close) or the connection is
    /// rebuilt (reconnect, reconfigure).
    fn invalidate_identity(&self) {
        *self.last_accepted.lock().unwrap() = None;
    }

    fn take_latest(&self) -> Option<Pixels> {
        self.latest.lock().unwrap().take()
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    fn request_reconnect(&self) {
        // The rebuild can reset the device out of band, so the cached frame is
        // no longer known-visible: the next frame goes out even if identical.
        self.invalidate_identity();
        self.reconnect.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

impl Backend {
    /// Spawn the backend worker. `kind` comes from resolved configuration
    /// (`auto` resolves to Hid here; the worker falls back to Virtual when
    /// HID discovery cannot find a device).
    pub fn spawn(kind: BackendKind, cfg: &Config) -> Backend {
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        let commands = Arc::new(CommandQueue {
            latest: Mutex::new(None),
            last_accepted: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            reconnect: AtomicBool::new(false),
            wake: wake_tx,
            #[cfg(test)]
            wake_sends: AtomicUsize::new(0),
        });
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let reconnect = cfg.logitech_reconnect;
        let reconnect_max = cfg.logitech_reconnect_max;
        let button_poll = cfg.logitech_button_poll;
        let debounce = cfg.logitech_button_debounce;
        let orientation = cfg.logitech_orientation.clone();
        let invert = cfg.logitech_invert;
        let friendly_name = cfg.logitech_friendly_name.clone();
        let worker_commands = Arc::clone(&commands);
        let thread = std::thread::Builder::new()
            .name("lcdsirplus-backend".into())
            .spawn(move || {
                worker(
                    kind,
                    worker_commands,
                    wake_rx,
                    msg_tx,
                    reconnect,
                    reconnect_max,
                    button_poll,
                    debounce,
                    orientation,
                    invert,
                    friendly_name,
                )
            })
            .expect("spawn backend thread");
        Backend {
            commands,
            msg_rx,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, frame: &crate::render::Frame) {
        let mut boxed = Box::new([0u8; crate::model::WIDTH * crate::model::HEIGHT]);
        boxed.copy_from_slice(&frame.pixels);
        self.commands.submit(boxed);
    }

    /// Wake backoff and reacquire the device, without resetting SDK quarantine.
    pub fn request_reconnect(&self) {
        self.commands.request_reconnect();
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.commands.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub fn reconfigure(&mut self, kind: BackendKind, cfg: &Config) {
        self.stop();
        *self = Self::spawn(kind, cfg);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.commands.shutdown();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    kind: BackendKind,
    commands: Arc<CommandQueue>,
    wake_rx: Receiver<()>,
    msg_tx: std::sync::mpsc::Sender<Message>,
    reconnect: Duration,
    reconnect_max: Duration,
    button_poll: Duration,
    debounce: Duration,
    orientation: String,
    invert: bool,
    friendly_name: String,
) {
    let _ = msg_tx.send(Message::State(BackendState::Discovering));
    match kind {
        BackendKind::Virtual => virtual_worker(commands, wake_rx, msg_tx),
        BackendKind::Auto | BackendKind::Sdk | BackendKind::Hid => {
            physical_worker(
                kind,
                &commands,
                &wake_rx,
                &msg_tx,
                reconnect,
                reconnect_max,
                button_poll,
                debounce,
                orientation,
                invert,
                friendly_name,
                open_physical,
                device_candidates_present,
            );
            let _ = msg_tx.send(Message::State(BackendState::ShutDown));
        }
    }
}

fn transform_pixels(
    frame: &[u8; crate::model::WIDTH * crate::model::HEIGHT],
    orientation: &str,
    invert: bool,
) -> crate::render::Frame {
    // Build a temporary logical view, apply orientation/invert, then pack.
    let mut transformed = crate::render::Frame::new();
    for y in 0..crate::model::HEIGHT {
        for x in 0..crate::model::WIDTH {
            let (sx, sy) = match orientation {
                "flip_x" => (crate::model::WIDTH - 1 - x, y),
                "flip_y" => (x, crate::model::HEIGHT - 1 - y),
                "rotate_180" => (crate::model::WIDTH - 1 - x, crate::model::HEIGHT - 1 - y),
                _ => (x, y),
            };
            let mut on = frame[sy * crate::model::WIDTH + sx] != 0;
            if invert {
                on = !on;
            }
            transformed.set(x as i32, y as i32, on);
        }
    }
    transformed
}

#[cfg(test)]
fn transform_frame(
    frame: &[u8; crate::model::WIDTH * crate::model::HEIGHT],
    orientation: &str,
    invert: bool,
) -> [u8; G13_OUTPUT_REPORT_LENGTH] {
    pack_report(&transform_pixels(frame, orientation, invert))
}

enum PhysicalDevice {
    Hid(hid::HidDevice),
    Sdk(sdk::SdkDevice),
    /// F-2 test seam: a scripted stand-in for a physical device, so the worker's
    /// two blanking device-loss sites and their EC-1 invalidations are testable
    /// without hardware. Compiles out of every non-test build.
    #[cfg(test)]
    Scripted(ScriptedDevice),
}

impl PhysicalDevice {
    fn kind(&self) -> BackendKind {
        match self {
            Self::Hid(_) => BackendKind::Hid,
            Self::Sdk(_) => BackendKind::Sdk,
            #[cfg(test)]
            Self::Scripted(_) => BackendKind::Hid,
        }
    }

    fn submit(&self, frame: &crate::render::Frame) -> Result<Vec<u8>, LossError> {
        match self {
            Self::Hid(device) => {
                let report = pack_report(frame);
                device.write_report(&report)?;
                Ok(report.to_vec())
            }
            Self::Sdk(device) => {
                let bytes = frame.logitech_bytes();
                device.submit(bytes.clone())?;
                Ok(bytes)
            }
            #[cfg(test)]
            Self::Scripted(device) => device.submit(frame),
        }
    }

    fn poll(&self, timeout: Duration) -> Result<Option<[bool; 4]>, LossError> {
        match self {
            Self::Hid(device) => {
                let mut raw = [0u8; 8];
                if device.read_input_timeout(&mut raw, timeout)? {
                    parse_input(&raw)
                        .map(Some)
                        .map_err(|reason| LossError::new(LossCause::ProbeFail, reason))
                } else {
                    Ok(None)
                }
            }
            Self::Sdk(device) => {
                let (connected, buttons) = device.poll()?;
                if connected {
                    Ok(Some(buttons))
                } else {
                    Err(LossError::new(
                        LossCause::DeviceAbsent,
                        "Logitech SDK reports the monochrome LCD disconnected".into(),
                    ))
                }
            }
            #[cfg(test)]
            Self::Scripted(device) => device.poll(timeout),
        }
    }

    fn close(&mut self, blank: bool) -> Result<(), LossError> {
        match self {
            Self::Hid(device) if blank => device.close(),
            Self::Hid(device) => {
                device.close_without_blank();
                Ok(())
            }
            Self::Sdk(device) => device.close(blank),
            #[cfg(test)]
            Self::Scripted(device) => device.close(blank),
        }
    }
}

/// F-2 test seam: a scripted physical device for the worker loss-site tests, in
/// the `ScriptedAliveness` fake style of `sdk.rs`. The scenario travels through
/// the existing `open(kind, friendly_name)` seam string, and every scripted
/// action is appended to `SCRIPT_EVENTS`, so each test reads the device-visible
/// history of its own scenario only (the suite runs tests in parallel).
#[cfg(test)]
struct ScriptedDevice {
    scenario: &'static str,
    session: usize,
}

/// F-2 scenario: session 1 fails the frame submit; every later call succeeds.
#[cfg(test)]
const SCRIPT_SUBMIT_LOSS: &str = "scripted-submit-loss";
/// F-2 scenario: session 1 writes a frame, then the button poll fails; every
/// later call succeeds.
#[cfg(test)]
const SCRIPT_POLL_LOSS: &str = "scripted-poll-loss";

/// Scripted device history: `(scenario, event, session, device key)`. Events:
/// `open`, `write`, `submit-fail`, `poll-fail`, `close-blank`. The `write` key is
/// the packed report the device received, so a test can name the exact frame.
#[cfg(test)]
type ScriptEvent = (&'static str, &'static str, usize, Vec<u8>);
#[cfg(test)]
type ScriptEventView = (&'static str, usize, Vec<u8>);
#[cfg(test)]
static SCRIPT_EVENTS: Mutex<Vec<ScriptEvent>> = Mutex::new(Vec::new());

#[cfg(test)]
impl ScriptedDevice {
    fn record(&self, event: &'static str, key: Vec<u8>) {
        SCRIPT_EVENTS
            .lock()
            .unwrap()
            .push((self.scenario, event, self.session, key));
    }

    fn submit(&self, frame: &crate::render::Frame) -> Result<Vec<u8>, LossError> {
        if self.scenario == SCRIPT_SUBMIT_LOSS && self.session == 1 {
            self.record("submit-fail", Vec::new());
            return Err(LossError::new(
                LossCause::TransferIncomplete { code: 991 },
                "injected submit loss".into(),
            ));
        }
        let report = pack_report(frame).to_vec();
        self.record("write", report.clone());
        Ok(report)
    }

    fn poll(&self, _timeout: Duration) -> Result<Option<[bool; 4]>, LossError> {
        if self.scenario == SCRIPT_POLL_LOSS && self.session == 1 {
            self.record("poll-fail", Vec::new());
            return Err(LossError::new(
                LossCause::CallTimeout,
                "injected poll loss".into(),
            ));
        }
        Ok(None)
    }

    fn close(&self, blank: bool) -> Result<(), LossError> {
        if blank {
            self.record("close-blank", Vec::new());
        }
        Ok(())
    }
}

/// The device-open path the ladder attempts, injectable so the RC-6 terminal
/// is testable without touching hardware.
type OpenPhysical = fn(BackendKind, &str) -> Result<PhysicalDevice, LossError>;
/// The candidate probe that ends the RC-6 terminal, injectable for the same
/// tests.
type ProbeCandidates = fn(BackendKind) -> bool;

fn open_physical(kind: BackendKind, friendly_name: &str) -> Result<PhysicalDevice, LossError> {
    if sdk::circuit_open() {
        return Err(LossError::new(
            LossCause::CircuitOpen,
            "physical backend circuit is open after an unreturning SDK owner".into(),
        ));
    }
    let owner = sdk::lcore().map_err(|reason| LossError::new(LossCause::ProbeFail, reason))?;
    match (select_physical_kind(kind, owner.is_some())?, owner) {
        (BackendKind::Sdk, Some(owner)) => {
            sdk::SdkDevice::open(&owner, friendly_name).map(PhysicalDevice::Sdk)
        }
        (BackendKind::Hid, None) => {
            hid::HidDevice::open().map(|(device, _)| PhysicalDevice::Hid(device))
        }
        _ => unreachable!(),
    }
}

fn select_physical_kind(kind: BackendKind, lcore_present: bool) -> Result<BackendKind, LossError> {
    match (kind, lcore_present) {
        (BackendKind::Auto, true) | (BackendKind::Sdk, true) => Ok(BackendKind::Sdk),
        (BackendKind::Auto, false) | (BackendKind::Hid, false) => Ok(BackendKind::Hid),
        (BackendKind::Sdk, false) => Err(LossError::new(
            LossCause::DeviceAbsent,
            "Logitech SDK mode requires a running trusted LCore.exe".into(),
        )),
        (BackendKind::Hid, true) => Err(LossError::new(
            LossCause::OwnerConflict,
            "direct HID is refused while competing process LCore.exe is running".into(),
        )),
        (BackendKind::Virtual, _) => Err(LossError::new(
            LossCause::ProbeFail,
            "virtual is not a physical backend".into(),
        )),
    }
}

/// RC-6 candidate probe used while the ladder is terminal: reuse the open
/// path's own arbitration to answer "is there anything to open". A running
/// trusted LCore is an SDK candidate; otherwise a G13 HID discovery candidate.
/// Read-only — it never opens a device, so it cannot start a connection.
fn device_candidates_present(kind: BackendKind) -> bool {
    let owner_present = sdk::lcore().ok().flatten().is_some();
    match select_physical_kind(kind, owner_present) {
        Ok(BackendKind::Sdk) => true,
        Ok(BackendKind::Hid) => !hid::discover().candidates.is_empty(),
        Ok(_) | Err(_) => false,
    }
}

fn ownership_changed(device: &PhysicalDevice) -> Result<bool, LossError> {
    // The scripted seam has no owner to enumerate: the real probe reads live
    // LCore.exe state, which would route the worker to the ownership site instead
    // of the device-loss site a scripted test is driving.
    #[cfg(test)]
    if let PhysicalDevice::Scripted(_) = device {
        return Ok(false);
    }
    let present = sdk::lcore()
        .map_err(|reason| LossError::new(LossCause::ProbeFail, reason))?
        .is_some();
    Ok(ownership_conflict(device.kind(), present))
}

fn ownership_conflict(kind: BackendKind, lcore_present: bool) -> bool {
    matches!(
        (kind, lcore_present),
        (BackendKind::Hid, true) | (BackendKind::Sdk, false)
    )
}

fn next_frame(commands: &CommandQueue, replay: &mut Option<Pixels>) -> Option<Pixels> {
    if let Some(newest) = commands.take_latest() {
        *replay = None;
        Some(newest)
    } else {
        replay.take()
    }
}

#[allow(clippy::too_many_arguments, unused_assignments)]
fn physical_worker(
    kind: BackendKind,
    commands: &CommandQueue,
    wake_rx: &Receiver<()>,
    msg_tx: &std::sync::mpsc::Sender<Message>,
    reconnect: Duration,
    reconnect_max: Duration,
    button_poll: Duration,
    debounce: Duration,
    orientation: String,
    invert: bool,
    friendly_name: String,
    open: OpenPhysical,
    probe: ProbeCandidates,
) {
    let mut backoff = reconnect;
    let mut replay: Option<Pixels> = None;
    let mut current: Option<Pixels> = None;
    let mut tracker = ButtonTracker::default();
    let mut absent_passes: usize = 0;
    let mut terminal = false;
    let mut last_probe: Option<Instant> = None;

    'outer: loop {
        if commands.reconnect.swap(false, Ordering::AcqRel) {
            backoff = reconnect;
            let _ = msg_tx.send(Message::ReconnectStarted);
        }
        if terminal {
            // RC-6: the ladder is terminal. Device-open attempts are skipped;
            // shutdown, frame-submit, and reconnect wakes stay live, and each
            // wake re-probes for a hotplugged candidate at most once per
            // `button_poll` — the worker's existing cadence key.
            if sleep_until_wake(commands, wake_rx, backoff) {
                break 'outer;
            }
            if last_probe.is_none_or(|when| when.elapsed() >= button_poll) {
                last_probe = Some(Instant::now());
                if probe(kind) {
                    terminal = false;
                    absent_passes = 0;
                    let _ = msg_tx.send(Message::State(BackendState::Discovering));
                } else {
                    continue 'outer;
                }
            } else {
                continue 'outer;
            }
        }
        match open(kind, &friendly_name) {
            Err(error) => {
                if error.cause == LossCause::DeviceAbsent {
                    absent_passes += 1;
                } else {
                    // The pass saw a candidate and failed for another reason:
                    // it is not a zero-candidate pass.
                    absent_passes = 0;
                }
                if !terminal && absent_passes >= ABSENT_TERMINATION_PASSES {
                    terminal = true;
                    last_probe = None;
                    let _ = msg_tx.send(Message::State(BackendState::Absent {
                        reason: error.to_string(),
                    }));
                } else {
                    let _ = msg_tx.send(disconnect(&error));
                }
                if sleep_interruptible(commands, wake_rx, backoff) {
                    break 'outer;
                }
                backoff = (backoff * 2).min(reconnect_max);
                continue 'outer;
            }
            Ok(mut device) => {
                absent_passes = 0;
                let active_kind = device.kind();
                let _ = msg_tx.send(Message::State(BackendState::Connected {
                    kind: active_kind,
                }));
                let _ = msg_tx.send(Message::Buttons(
                    tracker.disconnect(Instant::now(), active_kind.name()),
                ));
                let mut last_sent: Option<Vec<u8>> = None;
                let mut health_reported = false;
                backoff = reconnect;

                loop {
                    if commands.is_shutdown() {
                        if let Err(error) = device.close(true) {
                            let _ = msg_tx.send(disconnect(&shutdown_close_error(&error)));
                        }
                        break 'outer;
                    }
                    if commands.reconnect.load(Ordering::Acquire) {
                        if replay.is_none() {
                            replay = current.take();
                        }
                        let _ = device.close(false);
                        let _ = msg_tx.send(Message::State(BackendState::Discovering));
                        let _ = msg_tx.send(Message::Buttons(
                            tracker.disconnect(Instant::now(), active_kind.name()),
                        ));
                        continue 'outer;
                    }
                    match ownership_changed(&device) {
                        Ok(true) => {
                            if replay.is_none() {
                                replay = current.take();
                            }
                            let _ = device.close(false);
                            let _ = msg_tx.send(Message::State(BackendState::Discovering));
                            let _ = msg_tx.send(Message::Buttons(
                                tracker.disconnect(Instant::now(), active_kind.name()),
                            ));
                            continue 'outer;
                        }
                        Err(error) => {
                            if replay.is_none() {
                                replay = current.take();
                            }
                            let _ = device.close(false);
                            let _ = msg_tx.send(disconnect(&error));
                            continue 'outer;
                        }
                        Ok(false) => {}
                    }
                    let mut pixels = next_frame(commands, &mut replay);
                    if pixels.is_none() {
                        match wake_rx.recv_timeout(button_poll) {
                            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break 'outer,
                        }
                        if commands.is_shutdown() || commands.reconnect.load(Ordering::Acquire) {
                            continue;
                        }
                        pixels = next_frame(commands, &mut replay);
                    }

                    if let Some(pixels) = pixels {
                        let frame = transform_pixels(&pixels, &orientation, invert);
                        let key = match active_kind {
                            BackendKind::Hid => pack_report(&frame).to_vec(),
                            BackendKind::Sdk => frame.logitech_bytes(),
                            _ => unreachable!(),
                        };
                        if last_sent.as_ref() != Some(&key) {
                            match device.submit(&frame) {
                                Ok(sent) => {
                                    last_sent = Some(sent);
                                    current = Some(pixels);
                                }
                                Err(mut error) => {
                                    replay = Some(pixels);
                                    if let Err(close_error) = device.close(true) {
                                        error = append_close_failure(error, &close_error);
                                    }
                                    // EC-1: the frame never became visible and the
                                    // close blanked the panel: drop the cached content.
                                    commands.invalidate_identity();
                                    let _ = msg_tx.send(disconnect(&error));
                                    let _ = msg_tx.send(Message::Buttons(
                                        tracker.disconnect(Instant::now(), active_kind.name()),
                                    ));
                                    if sleep_interruptible(commands, wake_rx, backoff) {
                                        break 'outer;
                                    }
                                    backoff = (backoff * 2).min(reconnect_max);
                                    continue 'outer;
                                }
                            }
                        } else {
                            current = Some(pixels);
                        }
                    }

                    let poll = device.poll(button_poll.max(Duration::from_millis(10)));
                    if !health_reported && last_sent.is_some() && poll.is_ok() {
                        let _ = msg_tx.send(Message::PhysicalHealthy(active_kind));
                        health_reported = true;
                    }
                    match poll {
                        Ok(Some(buttons)) => {
                            let events = tracker.observe(
                                Instant::now(),
                                buttons,
                                debounce,
                                active_kind.name(),
                            );
                            if !events.is_empty() {
                                let _ = msg_tx.send(Message::Buttons(events));
                            }
                        }
                        Ok(None) => {}
                        Err(mut error) => {
                            if replay.is_none() {
                                replay = current.take();
                            }
                            if let Err(close_error) = device.close(true) {
                                error = append_close_failure(error, &close_error);
                            }
                            // EC-1: the blanking close erased the cached visible
                            // frame, so the next submitted frame goes out.
                            commands.invalidate_identity();
                            let _ = msg_tx.send(disconnect(&error));
                            let _ = msg_tx.send(Message::Buttons(
                                tracker.disconnect(Instant::now(), active_kind.name()),
                            ));
                            if sleep_interruptible(commands, wake_rx, backoff) {
                                break 'outer;
                            }
                            backoff = (backoff * 2).min(reconnect_max);
                            continue 'outer;
                        }
                    }
                }
            }
        }
    }
}

fn sleep_interruptible(
    commands: &CommandQueue,
    wake_rx: &Receiver<()>,
    duration: Duration,
) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        if commands.is_shutdown() {
            return true;
        }
        if commands.reconnect.load(Ordering::Acquire) {
            return false;
        }
        match wake_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
}

/// Terminal-state wait: keeps the same 100 ms shutdown responsiveness as
/// `sleep_interruptible`, but returns at the first wake so the terminal worker
/// can re-probe for a hotplugged candidate instead of sleeping out the backoff.
fn sleep_until_wake(commands: &CommandQueue, wake_rx: &Receiver<()>, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        if commands.is_shutdown() {
            return true;
        }
        if commands.reconnect.load(Ordering::Acquire) {
            return false;
        }
        match wake_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(()) => return false,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
}

fn virtual_fallback_loop(
    commands: &CommandQueue,
    wake_rx: &Receiver<()>,
    msg_tx: &std::sync::mpsc::Sender<Message>,
) {
    let _ = msg_tx.send(Message::State(BackendState::Connected {
        kind: BackendKind::Virtual,
    }));
    while !commands.is_shutdown() {
        let _ = commands.take_latest();
        if wake_rx.recv_timeout(Duration::from_millis(100)).is_err() && commands.is_shutdown() {
            return;
        }
    }
}

fn virtual_worker(
    commands: Arc<CommandQueue>,
    wake_rx: Receiver<()>,
    msg_tx: std::sync::mpsc::Sender<Message>,
) {
    let _ = msg_tx.send(Message::State(BackendState::Connected {
        kind: BackendKind::Virtual,
    }));
    virtual_fallback_loop(&commands, &wake_rx, &msg_tx);
    let _ = msg_tx.send(Message::State(BackendState::ShutDown));
}

/// Resolve configured backend selection to a concrete kind for startup.
pub fn resolve_kind(config_backend: &str) -> BackendKind {
    match config_backend {
        "auto" => BackendKind::Auto,
        "sdk" => BackendKind::Sdk,
        "hid" => BackendKind::Hid,
        "virtual" => BackendKind::Virtual,
        _ => BackendKind::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Frame;
    use std::sync::atomic::AtomicUsize;

    fn command_queue() -> (Arc<CommandQueue>, Receiver<()>) {
        let (wake, receiver) = std::sync::mpsc::sync_channel(1);
        (
            Arc::new(CommandQueue {
                latest: Mutex::new(None),
                last_accepted: Mutex::new(None),
                shutdown: AtomicBool::new(false),
                reconnect: AtomicBool::new(false),
                wake,
                wake_sends: AtomicUsize::new(0),
            }),
            receiver,
        )
    }

    fn pixels(value: u8) -> Pixels {
        Box::new([value; crate::model::WIDTH * crate::model::HEIGHT])
    }

    #[test]
    fn command_queue_retains_only_latest_frame_and_shutdown_is_reliable() {
        let (queue, wake) = command_queue();
        queue.submit(pixels(1));
        queue.submit(pixels(2));
        queue.submit(pixels(3));
        assert_eq!(wake.try_recv(), Ok(()));
        assert!(wake.try_recv().is_err(), "wake channel is bounded to one");
        assert_eq!(queue.take_latest().unwrap()[0], 3);
        assert!(queue.take_latest().is_none());

        queue.submit(pixels(4));
        queue.shutdown();
        assert!(
            queue.is_shutdown(),
            "shutdown does not depend on wake capacity"
        );
    }

    #[test]
    fn backoff_retains_one_newest_frame_for_one_reconnect_replay() {
        let (queue, wake) = command_queue();
        queue.submit(pixels(1));
        queue.submit(pixels(2));
        queue.submit(pixels(3));
        assert!(!sleep_interruptible(
            &queue,
            &wake,
            Duration::from_millis(1)
        ));
        let newest = queue.take_latest().unwrap();
        assert_eq!(newest[0], 3);
        let replay = transform_frame(&newest, "normal", false);
        assert!(queue.take_latest().is_none());
        assert_eq!(replay[32], 0xFF, "newest nonzero frame is replayed once");
    }

    #[test]
    fn reconnect_request_wakes_a_worker_in_long_backoff_without_losing_its_frame() {
        let (commands, wake) = command_queue();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let shutdown = sleep_interruptible(&worker_commands, &wake, Duration::from_secs(120));
            done_tx.send(shutdown).unwrap();
        });
        let backend = Backend {
            commands,
            msg_rx: std::sync::mpsc::channel().1,
            thread: Some(thread),
        };
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        backend.commands.submit(pixels(7));
        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "ordinary frame wakes must not bypass backoff"
        );
        backend.request_reconnect();
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        assert!(backend.commands.reconnect.load(Ordering::Acquire));
        assert_eq!(backend.commands.take_latest().unwrap()[0], 7);
        backend.shutdown();
        assert!(!result.unwrap(), "retry, not shutdown");
    }

    // EC-1 seams: the identity gate is exercised on the queue's own wake
    // counter, so a suppressed frame is observable as a zero, with no device
    // path involved.
    #[test]
    fn identical_frame_content_skips_enqueue_and_wake() {
        let (queue, wake) = command_queue();
        queue.submit(pixels(1));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            1,
            "the first submission enqueues and wakes"
        );
        assert_eq!(wake.try_recv(), Ok(()));
        assert_eq!(
            queue.take_latest().unwrap()[0],
            1,
            "the first submission sits in the single slot"
        );

        queue.submit(pixels(1));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            1,
            "an identical frame performs zero wake sends"
        );
        assert!(
            queue.take_latest().is_none(),
            "an identical frame performs zero enqueue"
        );
        assert!(wake.try_recv().is_err(), "no wake token was produced");

        let mut changed = pixels(1);
        changed[changed.len() - 1] = 2;
        queue.submit(changed);
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            2,
            "a last-byte difference enqueues and wakes"
        );
        let queued = queue.take_latest().unwrap();
        assert_eq!(
            queued[0], 1,
            "the changed frame is the cached one plus the last byte"
        );
        assert_eq!(queued[queued.len() - 1], 2);
    }

    #[test]
    fn reconnect_request_invalidates_the_identity_cache_so_the_identical_frame_is_sent() {
        let (queue, wake) = command_queue();
        queue.submit(pixels(5));
        assert_eq!(
            queue.take_latest().unwrap()[0],
            5,
            "the worker takes the frame"
        );
        queue.submit(pixels(5));
        assert_eq!(queue.wake_sends.load(Ordering::SeqCst), 1);
        assert!(
            queue.take_latest().is_none(),
            "the repeat stays suppressed until the connection is rebuilt"
        );

        queue.request_reconnect();
        assert!(queue.reconnect.load(Ordering::Acquire));
        assert_eq!(wake.try_recv(), Ok(()), "the reconnect wake stays live");
        queue.submit(pixels(5));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            2,
            "after request_reconnect the identical frame enqueues and wakes"
        );
        assert_eq!(
            queue.take_latest().unwrap()[0],
            5,
            "the identical frame is sent again after the reconnect"
        );
    }

    #[test]
    fn a_blanking_device_loss_sends_the_identical_frame() {
        // The worker calls this invalidation on both blanking closes: after a
        // device submit loss and after a poll loss (`device.close(true)`), so
        // content the panel lost is never withheld by the cache.
        let (queue, wake) = command_queue();
        queue.submit(pixels(6));
        queue.submit(pixels(6));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            1,
            "the repeat is suppressed while the panel shows it"
        );

        queue.invalidate_identity();
        queue.submit(pixels(6));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            2,
            "the invalidated cache sends the identical frame"
        );
        assert_eq!(queue.take_latest().unwrap()[0], 6);
        assert_eq!(wake.try_recv(), Ok(()));
    }

    // F-2 seams: the worker's two blanking device-loss sites call
    // `invalidate_identity` right after the blanking `close(true)` and before the
    // disconnect publication. The scripted device injects those losses, so each
    // site's call is load-bearing in its own test: removing one fails that test.
    fn open_scripted(_kind: BackendKind, friendly_name: &str) -> Result<PhysicalDevice, LossError> {
        let scenario = if friendly_name == SCRIPT_SUBMIT_LOSS {
            SCRIPT_SUBMIT_LOSS
        } else if friendly_name == SCRIPT_POLL_LOSS {
            SCRIPT_POLL_LOSS
        } else {
            panic!("unknown scripted scenario: {friendly_name}");
        };
        let session = {
            let mut events = SCRIPT_EVENTS.lock().unwrap();
            let session = events
                .iter()
                .filter(|event| event.0 == scenario && event.1 == "open")
                .count()
                + 1;
            events.push((scenario, "open", session, Vec::new()));
            session
        };
        Ok(PhysicalDevice::Scripted(ScriptedDevice {
            scenario,
            session,
        }))
    }

    fn script_events(scenario: &str) -> Vec<ScriptEventView> {
        SCRIPT_EVENTS
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.0 == scenario)
            .map(|event| (event.1, event.2, event.3.clone()))
            .collect()
    }

    /// Drain the publication stream for a bounded settle window, counting the
    /// health reports: each new device session reports at most one.
    fn settle(rx: std::sync::mpsc::Receiver<Message>) -> usize {
        let mut healthy = 0;
        while let Ok(message) = rx.recv_timeout(Duration::from_millis(300)) {
            if matches!(message, Message::PhysicalHealthy(_)) {
                healthy += 1;
            }
        }
        healthy
    }

    /// Start the worker on a scripted device, send one frame, and wait for the
    /// injected loss to publish its disconnect. The disconnect is sent by the same
    /// worker thread that ran the site's `invalidate_identity`, so observing it
    /// proves the invalidation already happened when the test resubmits.
    fn run_scripted_loss(
        scenario: &'static str,
        value: u8,
        want: LossCause,
    ) -> (
        Arc<CommandQueue>,
        std::sync::mpsc::Receiver<Message>,
        JoinHandle<()>,
    ) {
        let base = Duration::from_millis(1);
        let (commands, wake) = command_queue();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let thread = std::thread::spawn(move || {
            physical_worker(
                BackendKind::Hid,
                &worker_commands,
                &wake,
                &tx,
                base,
                base,
                base,
                base,
                "normal".into(),
                false,
                scenario.to_string(),
                open_scripted,
                probe_never,
            );
        });
        commands.submit(pixels(value));
        let budget = Instant::now() + Duration::from_secs(10);
        let (cause, reason) = loop {
            let left = budget.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "{scenario}: the injected loss never published"
            );
            if let Ok(Message::State(BackendState::Disconnected { cause, reason })) =
                rx.recv_timeout(left)
            {
                break (cause, reason);
            }
        };
        assert_eq!(cause, want, "{scenario}: the loss site names its own cause");
        assert!(reason.contains("injected "), "{scenario}: {reason}");
        (commands, rx, thread)
    }

    #[test]
    fn a_submit_device_loss_invalidates_identity_so_the_identical_frame_returns_after_reopen() {
        // Loss site 1 (baseline mod.rs:734): a failed frame submit blanked the
        // panel and the frame never became visible, so the cache must forget it
        // and the app's next identical frame must reach the reopened device.
        let (commands, rx, thread) = run_scripted_loss(
            SCRIPT_SUBMIT_LOSS,
            6,
            LossCause::TransferIncomplete { code: 991 },
        );
        commands.submit(pixels(6));
        assert_eq!(
            commands.wake_sends.load(Ordering::SeqCst),
            2,
            "the invalidated cache enqueues and wakes the identical frame"
        );

        let key = transform_frame(&pixels(6), "normal", false).to_vec();
        let want_events: Vec<ScriptEventView> = vec![
            ("open", 1, Vec::new()),
            ("submit-fail", 1, Vec::new()),
            ("close-blank", 1, Vec::new()),
            ("open", 2, Vec::new()),
            ("write", 2, key),
        ];
        assert_eq!(settle(rx), 1, "the reopened session reports health once");
        assert_eq!(
            script_events(SCRIPT_SUBMIT_LOSS),
            want_events,
            "the identical frame is written exactly once on the reopened device"
        );

        commands.shutdown();
        thread.join().unwrap();
        assert_eq!(
            script_events(SCRIPT_SUBMIT_LOSS).len(),
            want_events.len() + 1,
            "the shutdown close ran on the live session"
        );
    }

    #[test]
    fn a_poll_device_loss_invalidates_identity_so_the_identical_frame_returns_after_reopen() {
        // Loss site 2 (baseline mod.rs:778): a failed button poll blanked the panel
        // after the frame was visible, so the cache must forget it and the app's
        // next identical frame must be enqueued for the reopened device.
        let (commands, rx, thread) = run_scripted_loss(SCRIPT_POLL_LOSS, 7, LossCause::CallTimeout);
        commands.submit(pixels(7));
        assert_eq!(
            commands.wake_sends.load(Ordering::SeqCst),
            2,
            "the invalidated cache enqueues and wakes the identical frame"
        );

        let key = transform_frame(&pixels(7), "normal", false).to_vec();
        let want_events: Vec<ScriptEventView> = vec![
            ("open", 1, Vec::new()),
            ("write", 1, key.clone()),
            ("poll-fail", 1, Vec::new()),
            ("close-blank", 1, Vec::new()),
            ("open", 2, Vec::new()),
            ("write", 2, key),
        ];
        assert_eq!(settle(rx), 1, "the reopened session reports health once");
        assert_eq!(
            script_events(SCRIPT_POLL_LOSS),
            want_events,
            "the identical frame is written exactly once on the reopened device"
        );

        commands.shutdown();
        thread.join().unwrap();
        assert_eq!(
            script_events(SCRIPT_POLL_LOSS).len(),
            want_events.len() + 1,
            "the shutdown close ran on the live session"
        );
    }

    #[test]
    fn reconfigure_spawns_a_fresh_identity_cache_which_sends_the_identical_frame() {
        // Acceptance (5): an orientation/invert change goes through
        // `Backend::reconfigure`, which stops the worker and spawns a brand-new
        // `CommandQueue`, so the identity cache starts empty and the first
        // frame after the change is enqueued and wakes the worker.
        let base = Config {
            logitech_button_poll: Duration::from_millis(1),
            ..Default::default()
        };
        let rotated = Config {
            logitech_orientation: "rotate_180".into(),
            logitech_invert: true,
            ..Default::default()
        };
        let mut backend = Backend::spawn(BackendKind::Virtual, &base);
        let mut frame = Frame::new();
        frame.set(3, 4, true);
        assert_ne!(
            transform_frame(&frame.pixels, "normal", false),
            transform_frame(&frame.pixels, "rotate_180", true),
            "the config change is visible device work"
        );

        backend.submit(&frame);
        backend.submit(&frame);
        assert_eq!(
            backend.commands.wake_sends.load(Ordering::SeqCst),
            1,
            "within one backend the identical repeat is suppressed"
        );

        let before = Arc::clone(&backend.commands);
        backend.reconfigure(BackendKind::Virtual, &rotated);
        // F-1: pin the fresh-queue intent. The send counter is per queue, so a
        // reused queue carrying a stale cache would read the same 1; the queue
        // identity is what makes the test sensitive to the mechanism.
        assert!(
            !Arc::ptr_eq(&before, &backend.commands),
            "reconfigure replaces the queue, so the identity cache starts empty"
        );
        backend.submit(&frame);
        assert_eq!(
            backend.commands.wake_sends.load(Ordering::SeqCst),
            1,
            "the reconfigured backend's fresh cache sends the identical frame exactly once"
        );
        assert_eq!(
            before.wake_sends.load(Ordering::SeqCst),
            1,
            "the retired queue sent it exactly once and takes nothing after reconfigure"
        );
        backend.shutdown();
    }

    #[test]
    fn a_stale_identity_cache_reuse_reads_the_same_send_counter_as_a_fresh_queue() {
        // F-1: why acceptance-(5) asserts queue identity and not only the send
        // counter. This queue stands for a `reconfigure` that reuses the retired
        // queue: its cache is stale, so the identical frame is suppressed and the
        // counter reads 1 — the same number the fresh queue reads when it sends
        // the frame. Only `!Arc::ptr_eq` separates the two cases.
        let (queue, wake) = command_queue();
        queue.submit(pixels(1));
        assert_eq!(wake.try_recv(), Ok(()));
        assert_eq!(
            queue.take_latest().unwrap()[0],
            1,
            "the worker takes the frame"
        );

        queue.submit(pixels(1));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            1,
            "a stale-cache reuse reads the same one send a fresh queue reports"
        );
        assert!(
            queue.take_latest().is_none(),
            "the stale cache withholds the frame a fresh queue would send"
        );
        assert!(wake.try_recv().is_err(), "and produces no second wake");
    }

    #[test]
    fn transform_identity_matches_direct_pack() {
        let mut f = Frame::new();
        f.set(3, 4, true);
        f.set(159, 42, true);
        let direct = pack_report(&f);
        let via_transform = transform_frame(&f.pixels, "normal", false);
        assert_eq!(direct.as_slice(), via_transform.as_slice());
    }

    #[test]
    fn transform_flip_x_mirrors() {
        let mut f = Frame::new();
        f.set(0, 0, true);
        let flipped = transform_frame(&f.pixels, "flip_x", false);
        let mut expected = Frame::new();
        expected.set(159, 0, true);
        assert_eq!(pack_report(&expected).as_slice(), flipped.as_slice());
    }

    #[test]
    fn transform_invert_flips_all_bits() {
        let f = Frame::new();
        let inverted = transform_frame(&f.pixels, "normal", true);
        assert!(inverted[32] == 0xFF, "blank frame inverted becomes all-on");
        assert_eq!(inverted[0], 0x03, "report ID preserved");
    }

    #[test]
    fn resolve_kind_maps_config() {
        assert_eq!(resolve_kind("auto"), BackendKind::Auto);
        assert_eq!(resolve_kind("sdk"), BackendKind::Sdk);
        assert_eq!(resolve_kind("hid"), BackendKind::Hid);
        assert_eq!(resolve_kind("virtual"), BackendKind::Virtual);
    }

    #[test]
    fn arbitration_never_cross_falls_back() {
        assert_eq!(
            select_physical_kind(BackendKind::Auto, true).unwrap(),
            BackendKind::Sdk
        );
        assert_eq!(
            select_physical_kind(BackendKind::Auto, false).unwrap(),
            BackendKind::Hid
        );
        assert_eq!(
            select_physical_kind(BackendKind::Sdk, false)
                .unwrap_err()
                .cause,
            LossCause::DeviceAbsent,
            "sdk mode without LCore.exe names the absent service"
        );
        assert_eq!(
            select_physical_kind(BackendKind::Hid, true)
                .unwrap_err()
                .cause,
            LossCause::OwnerConflict,
            "hid refusal under a competing owner names the conflict"
        );
    }

    #[test]
    fn ownership_transition_matrix_closes_hid_and_never_falls_back_from_sdk() {
        assert!(!ownership_conflict(BackendKind::Hid, false));
        assert!(ownership_conflict(BackendKind::Hid, true));
        assert!(!ownership_conflict(BackendKind::Sdk, true));
        assert!(ownership_conflict(BackendKind::Sdk, false));
        assert!(select_physical_kind(BackendKind::Auto, true).is_ok());
        assert!(select_physical_kind(BackendKind::Hid, true).is_err());
    }

    #[test]
    fn newest_pending_frame_replaces_stale_transition_replay() {
        let (queue, _) = command_queue();
        let mut replay = Some(pixels(1));
        queue.submit(pixels(2));
        queue.submit(pixels(3));
        assert_eq!(next_frame(&queue, &mut replay).unwrap()[0], 3);
        assert!(replay.is_none());
        assert!(next_frame(&queue, &mut replay).is_none());
    }

    fn published(error: &LossError) -> (LossCause, String) {
        match disconnect(error) {
            Message::State(BackendState::Disconnected { cause, reason }) => (cause, reason),
            _ => panic!("disconnect producers publish state messages"),
        }
    }

    #[test]
    fn open_fail_producer_publishes_open_failed_with_the_origin_code() {
        let (cause, reason) = published(&LossError::new(
            LossCause::OpenFailed { code: 0x8007_0002 },
            "open failed: device not present".into(),
        ));
        assert_eq!(cause, LossCause::OpenFailed { code: 0x8007_0002 });
        assert_eq!(
            reason,
            "open-failed(0x80070002): open failed: device not present"
        );
    }

    #[test]
    fn ownership_probe_fail_producer_publishes_probe_fail() {
        let (cause, reason) = published(&LossError::new(
            LossCause::ProbeFail,
            "enumerate LCore.exe processes: injected".into(),
        ));
        assert_eq!(cause, LossCause::ProbeFail);
        assert!(reason.starts_with("probe-fail: "), "{reason}");
    }

    #[test]
    fn submit_fail_producer_keeps_transfer_cause_and_appends_close_detail() {
        let origin = LossError::new(
            LossCause::TransferIncomplete { code: 991 },
            "HID write completed 991 of 992 bytes".into(),
        );
        let close = LossError::new(
            LossCause::ProbeFail,
            "HID blank on close: handle denied".into(),
        );
        let (cause, reason) = published(&append_close_failure(origin, &close));
        assert_eq!(cause, LossCause::TransferIncomplete { code: 991 });
        assert!(reason.contains("transfer-incomplete(991)"), "{reason}");
        assert!(reason.contains("close failed:"), "{reason}");
    }

    #[test]
    fn poll_fail_producer_publishes_call_timeout() {
        let (cause, reason) = published(&LossError::new(
            LossCause::CallTimeout,
            "Logitech SDK native call timed out; owner quarantined and circuit opened".into(),
        ));
        assert_eq!(cause, LossCause::CallTimeout);
        assert!(reason.starts_with("call-timeout: "), "{reason}");
    }

    #[test]
    fn shutdown_close_producer_publishes_shutdown_requested() {
        let origin = LossError::new(
            LossCause::TransferIncomplete { code: 0 },
            "LogiLcdMonoSetBackground blank returned false".into(),
        );
        let (cause, reason) = published(&shutdown_close_error(&origin));
        assert_eq!(cause, LossCause::ShutdownRequested);
        assert!(reason.starts_with("shutdown-requested: "), "{reason}");
    }

    #[test]
    fn circuit_open_producer_publishes_circuit_open() {
        let (cause, _) = published(&LossError::new(
            LossCause::CircuitOpen,
            "physical backend circuit is open after an unreturning SDK owner".into(),
        ));
        assert_eq!(cause, LossCause::CircuitOpen);
    }

    #[test]
    fn device_absent_producer_publishes_device_absent() {
        let (cause, reason) = published(&LossError::new(
            LossCause::DeviceAbsent,
            "HID discovery: no G13 collection matched".into(),
        ));
        assert_eq!(cause, LossCause::DeviceAbsent);
        assert!(reason.starts_with("device-absent: "), "{reason}");
    }

    #[test]
    fn every_named_loss_cause_is_distinct_and_short() {
        let labels: Vec<_> = LossCause::all().iter().map(|c| c.short()).collect();
        let unique: std::collections::HashSet<_> = labels
            .iter()
            .map(|l| l.split('(').next().unwrap())
            .collect();
        assert_eq!(
            unique.len(),
            9,
            "the cause vocabulary carries nine distinct names"
        );
        for label in &labels {
            assert!(label.len() <= 30, "journal codes stay short: {label}");
            assert!(
                !label.contains("healthy") && !label.contains("finished"),
                "{label}"
            );
        }
        assert!(
            labels.contains(&"worker-stalled".to_string()),
            "RC-2 names the stalled worker, it is never swallowed"
        );
    }

    // RC-6 seams: the ladder terminal is exercised against injected open/probe
    // functions, so no hardware path is touched.
    static OPEN_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    static CANDIDATES: AtomicBool = AtomicBool::new(false);

    fn open_absent(_kind: BackendKind, _friendly_name: &str) -> Result<PhysicalDevice, LossError> {
        OPEN_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        Err(LossError::new(
            LossCause::DeviceAbsent,
            "injected discovery: no G13 candidates".into(),
        ))
    }

    fn probe_candidates(_kind: BackendKind) -> bool {
        CANDIDATES.load(Ordering::SeqCst)
    }

    #[test]
    fn absent_ladder_terminal_covers_the_reconnect_cap_at_defaults() {
        // 5 s base / 60 s cap (config.rs L270-L271): N = 60 / 5 = 12 passes.
        assert_eq!(ABSENT_TERMINATION_PASSES, 12);
        assert!(
            Duration::from_secs(5) * ABSENT_TERMINATION_PASSES as u32 >= Duration::from_secs(60),
            "N base intervals cover at least reconnect_max"
        );
    }

    #[test]
    fn ladder_terminal_publishes_absent_once_then_a_candidate_reopens() {
        let base = Duration::from_millis(1);
        OPEN_ATTEMPTS.store(0, Ordering::SeqCst);
        CANDIDATES.store(false, Ordering::SeqCst);
        let (commands, wake) = command_queue();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let thread = std::thread::spawn(move || {
            physical_worker(
                BackendKind::Auto,
                &worker_commands,
                &wake,
                &tx,
                base,
                base,
                base,
                base,
                "normal".into(),
                false,
                String::new(),
                open_absent,
                probe_candidates,
            );
        });
        let budget = Instant::now() + Duration::from_secs(10);
        let remaining = || budget.saturating_duration_since(Instant::now());

        // (1) After N zero-candidate passes, Absent is published and opens stop.
        let mut absent = 0usize;
        while absent == 0 {
            let left = remaining();
            assert!(!left.is_zero(), "Absent was never published");
            if let Ok(Message::State(BackendState::Absent { reason })) = rx.recv_timeout(left) {
                assert!(reason.starts_with("device-absent: "), "{reason}");
                absent += 1;
            }
        }
        let attempts = OPEN_ATTEMPTS.load(Ordering::SeqCst);
        assert_eq!(
            attempts, ABSENT_TERMINATION_PASSES,
            "the ladder stops opening at the terminal"
        );

        // (2) Submitting while Absent keeps preview behavior: the frame stays
        // newest for replay, nothing panics, and the state is not republished.
        commands.submit(pixels(9));
        let mut extra_absent = 0usize;
        let mut extra_disconnects = 0usize;
        while let Ok(message) = rx.recv_timeout(Duration::from_millis(300)) {
            match message {
                Message::State(BackendState::Absent { .. }) => extra_absent += 1,
                Message::State(BackendState::Disconnected { .. }) => extra_disconnects += 1,
                _ => {}
            }
        }
        assert_eq!(absent + extra_absent, 1, "Absent is published exactly once");
        assert_eq!(
            extra_disconnects, 0,
            "the terminal stops the repeating disconnect publication"
        );
        assert_eq!(
            OPEN_ATTEMPTS.load(Ordering::SeqCst),
            attempts,
            "no device opens while Absent"
        );
        assert_eq!(
            commands.take_latest().unwrap()[0],
            9,
            "the newest submitted frame is retained for preview and replay"
        );

        // (3) A candidate appearance resumes opens within one wake.
        CANDIDATES.store(true, Ordering::SeqCst);
        commands.submit(pixels(10));
        let mut discovering = false;
        let reopened = loop {
            let left = remaining();
            assert!(!left.is_zero(), "no wake after a candidate appeared");
            if let Ok(message) = rx.recv_timeout(left) {
                match message {
                    Message::State(BackendState::Discovering) => discovering = true,
                    Message::State(BackendState::Disconnected { .. }) if discovering => break true,
                    _ => {}
                }
            }
        };
        assert!(reopened, "the candidate wake returns the ladder to opens");
        assert!(
            OPEN_ATTEMPTS.load(Ordering::SeqCst) > attempts,
            "the ladder opened again after the candidate"
        );

        // The shutdown wake path stayed live through the terminal state.
        commands.shutdown();
        thread.join().unwrap();
    }

    // F-1 seams: a ladder in which one pass fails for a NON-Absent cause. The
    // counter is keyed on `LossCause::DeviceAbsent` only, so that pass must
    // reset the consecutive count instead of advancing the terminal.
    const NON_ABSENT_INTERLEAVE_PASS: usize = ABSENT_TERMINATION_PASSES;
    const INTERLEAVED_NON_ABSENT_CODE: u32 = 0x8007_0002;
    static INTERLEAVED_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

    fn open_interleaved(
        _kind: BackendKind,
        _friendly_name: &str,
    ) -> Result<PhysicalDevice, LossError> {
        let pass = INTERLEAVED_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
        let cause = if pass == NON_ABSENT_INTERLEAVE_PASS {
            LossCause::OpenFailed {
                code: INTERLEAVED_NON_ABSENT_CODE,
            }
        } else {
            LossCause::DeviceAbsent
        };
        Err(LossError::new(cause, format!("injected pass {pass}")))
    }

    fn probe_never(_kind: BackendKind) -> bool {
        false
    }

    #[test]
    fn ladder_counting_ignores_non_absent_cause_passes_and_publishes_absent_once() {
        let base = Duration::from_millis(1);
        INTERLEAVED_ATTEMPTS.store(0, Ordering::SeqCst);
        let (commands, wake) = command_queue();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let thread = std::thread::spawn(move || {
            physical_worker(
                BackendKind::Auto,
                &worker_commands,
                &wake,
                &tx,
                base,
                base,
                base,
                base,
                "normal".into(),
                false,
                String::new(),
                open_interleaved,
                probe_never,
            );
        });
        let budget = Instant::now() + Duration::from_secs(10);
        let remaining = || budget.saturating_duration_since(Instant::now());
        // Passes that stay pre-terminal: the 11 DeviceAbsent passes before the
        // interleaved failure, the interleaved failure itself, and the 11
        // DeviceAbsent passes that re-accumulate without reaching 12.
        let pre_terminal_passes = NON_ABSENT_INTERLEAVE_PASS - 1 + ABSENT_TERMINATION_PASSES;
        let terminal_pass = NON_ABSENT_INTERLEAVE_PASS + ABSENT_TERMINATION_PASSES;

        // (1) Every one of those passes publishes a plain disconnect with its
        // own cause, and no Absent appears: the non-Absent pass reset the
        // consecutive count rather than advancing it.
        for pass in 1..=pre_terminal_passes {
            let left = remaining();
            assert!(!left.is_zero(), "no publication after pass {pass}");
            match rx
                .recv_timeout(left)
                .expect("a publication per failed open pass")
            {
                Message::State(BackendState::Disconnected { cause, reason }) => {
                    let want = if pass == NON_ABSENT_INTERLEAVE_PASS {
                        LossCause::OpenFailed {
                            code: INTERLEAVED_NON_ABSENT_CODE,
                        }
                    } else {
                        LossCause::DeviceAbsent
                    };
                    assert_eq!(cause, want, "pass {pass} is keyed by its own cause");
                    assert!(
                        reason.contains(&format!("injected pass {pass}")),
                        "pass {pass}: {reason}"
                    );
                }
                Message::State(BackendState::Absent { reason }) => {
                    panic!("pass {pass}: Absent published before 12 consecutive DeviceAbsent passes: {reason}")
                }
                _ => panic!("pass {pass}: expected a disconnect publication"),
            }
        }

        // (2) Absent arrives on the 12th CONSECUTIVE DeviceAbsent pass.
        let mut absent = 0usize;
        while absent == 0 {
            let left = remaining();
            assert!(!left.is_zero(), "Absent was never published");
            if let Ok(Message::State(BackendState::Absent { reason })) = rx.recv_timeout(left) {
                assert!(
                    reason.starts_with("device-absent: injected pass "),
                    "{reason}"
                );
                absent += 1;
            }
        }
        let attempts = INTERLEAVED_ATTEMPTS.load(Ordering::SeqCst);
        assert_eq!(
            attempts, terminal_pass,
            "Absent published on pass {terminal_pass}: 12 consecutive DeviceAbsent passes after the reset"
        );

        // (3) Published exactly once, and the terminal keeps device opens off.
        let mut extra_absent = 0usize;
        let mut extra_disconnects = 0usize;
        while let Ok(message) = rx.recv_timeout(Duration::from_millis(300)) {
            match message {
                Message::State(BackendState::Absent { .. }) => extra_absent += 1,
                Message::State(BackendState::Disconnected { .. }) => extra_disconnects += 1,
                _ => {}
            }
        }
        assert_eq!(absent + extra_absent, 1, "Absent is published exactly once");
        assert_eq!(
            extra_disconnects, 0,
            "the terminal stops the repeating disconnect publication"
        );
        assert_eq!(
            INTERLEAVED_ATTEMPTS.load(Ordering::SeqCst),
            attempts,
            "no device opens while Absent"
        );

        commands.shutdown();
        thread.join().unwrap();
    }
}
