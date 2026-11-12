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
    OpenFailed { code: u32 },
    CallTimeout,
    CircuitOpen,
    TransferIncomplete { code: u32 },
    OwnerConflict,
    ProbeFail,
    ShutdownRequested,
}

impl LossCause {
    /// All causes producers may emit; used by the label-whitelist test.
    #[cfg(test)]
    pub fn all() -> [LossCause; 8] {
        [
            LossCause::DeviceAbsent,
            LossCause::OpenFailed { code: 0 },
            LossCause::CallTimeout,
            LossCause::CircuitOpen,
            LossCause::TransferIncomplete { code: 0 },
            LossCause::OwnerConflict,
            LossCause::ProbeFail,
            LossCause::ShutdownRequested,
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

struct CommandQueue {
    latest: Mutex<Option<Pixels>>,
    shutdown: AtomicBool,
    reconnect: AtomicBool,
    wake: SyncSender<()>,
}

impl CommandQueue {
    fn submit(&self, pixels: Pixels) {
        *self.latest.lock().unwrap() = Some(pixels);
        let _ = self.wake.try_send(());
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
            shutdown: AtomicBool::new(false),
            reconnect: AtomicBool::new(false),
            wake: wake_tx,
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
}

impl PhysicalDevice {
    fn kind(&self) -> BackendKind {
        match self {
            Self::Hid(_) => BackendKind::Hid,
            Self::Sdk(_) => BackendKind::Sdk,
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
        }
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
                shutdown: AtomicBool::new(false),
                reconnect: AtomicBool::new(false),
                wake,
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
            8,
            "the cause vocabulary carries eight distinct names"
        );
        for label in &labels {
            assert!(label.len() <= 30, "journal codes stay short: {label}");
            assert!(
                !label.contains("healthy") && !label.contains("finished"),
                "{label}"
            );
        }
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
