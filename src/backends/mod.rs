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
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
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
    Message::State(disconnect_state(error))
}

/// The `Disconnected` state of the single construction point, so a failure that
/// never reached a worker publishes the SAME row shape the worker publishes for
/// a transport loss (WO-U46G item 5: the `Backend::spawn` failure sites in
/// app.rs). No new state, label or journal vocabulary exists on this path.
pub fn disconnect_state(error: &LossError) -> BackendState {
    BackendState::Disconnected {
        cause: error.cause,
        reason: error.to_string(),
    }
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

/// WO-OPT02A (finding O1): the real wake edge for one inbound message.
///
/// Before this unit the app loop polled every inbound channel on a fixed 10 ms
/// sleep, so an idle process woke 100 times a second with no work, and a
/// published message waited up to 10 ms for its next poll. A publisher that
/// forwards and then posts a capacity-1 token makes the publish itself the
/// wake: `try_send` never blocks and never grows (the single token coalesces N
/// publishes into one edge), and the app loop's own `try_recv` sweep drains
/// every message channel after clearing the token, so a coalesced edge can
/// never drop a message - the clear-then-read order is what makes that true.
///
/// `Send` is a trait (`Publish`) so the worker bodies keep their existing
/// `&msg_tx` call shape: production passes a `WakeSender`, the worker-level
/// tests keep passing a bare `Sender<Message>` (no wake token exists there),
/// and every publish site stays one line.
pub struct WakeSender<T> {
    tx: Sender<T>,
    wake: SyncSender<()>,
}

impl<T> WakeSender<T> {
    pub fn new(tx: Sender<T>, wake: SyncSender<()>) -> Self {
        Self { tx, wake }
    }

    /// Forward the message, then post the wake edge. Both are best-effort by
    /// design: a closed message channel (the app is exiting) makes the token
    /// pointless, and a full token channel already promises a wake.
    pub fn send(&self, message: T) {
        let _ = self.tx.send(message);
        let _ = self.wake.try_send(());
    }
}

/// What the worker bodies publish through: the production `WakeSender` plus the
/// bare `Sender` the worker tests build, so no worker publish site needs a
/// branch on which one it owns.
pub trait Publish<T> {
    fn send(&self, message: T);
}

impl<T> Publish<T> for Sender<T> {
    fn send(&self, message: T) {
        let _ = Sender::send(self, message);
    }
}

impl<T> Publish<T> for WakeSender<T> {
    fn send(&self, message: T) {
        WakeSender::send(self, message);
    }
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

/// EC-2 test seam: bounded event ledger for the buffer-exclusivity proof.
/// Events: (`new`|`fill`|`read`|`release`, queue address, buffer address,
/// first content byte). The producer records `new`/`fill` at spare pop and
/// fill, the worker records `read` at the transform site and `release` at the
/// spare push, so all three recorders share one mutex and the per-buffer
/// interleaving (fill only after release, no read between) is decidable.
/// Compiles out of every non-test build.
#[cfg(test)]
static EXCL_EVENTS: Mutex<Vec<(&'static str, usize, usize, u8)>> = Mutex::new(Vec::new());

/// The exclusivity test registers its queue address here, so the ledger sees
/// only that queue and cannot be diluted by the parallel suite.
#[cfg(test)]
static WATCH_EXCL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Exclusivity tests take the ledger in use (compare_exchange) for their whole
/// lifetime, so a retired queue's teardown releases can never interleave with
/// the next test whose queue lands on the recycled address.
#[cfg(test)]
static EXCL_IN_USE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Take the ledger for one exclusivity scenario: reset gate, watch and events.
/// Panics if another exclusivity scenario is live (they must not overlap:
/// the allocator recycles queue and buffer addresses between tests).
#[cfg(test)]
fn excl_begin(queue_addr: usize) {
    assert!(
        EXCL_IN_USE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok(),
        "two exclusivity scenarios raced for the ledger"
    );
    EXCL_EVENTS.lock().unwrap().clear();
    WATCH_EXCL.store(queue_addr, Ordering::SeqCst);
}

#[cfg(test)]
fn excl_end() {
    WATCH_EXCL.store(0, Ordering::SeqCst);
    EXCL_IN_USE.store(false, Ordering::SeqCst);
}

#[cfg(test)]
fn excl_record(kind: &'static str, queue: &CommandQueue, pixels: &Pixels) {
    let queue_addr = std::ptr::from_ref(queue) as usize;
    if queue_addr != WATCH_EXCL.load(Ordering::SeqCst) {
        return;
    }
    let mut events = EXCL_EVENTS.lock().unwrap();
    if events.len() < 8192 {
        // The buffer key is the HEAP address of the pixel allocation (stable
        // for the buffer's life), not the stack address of the Box handle,
        // which moves with every owner.
        events.push((
            kind,
            queue_addr,
            std::ptr::from_ref(&**pixels) as usize,
            pixels[0],
        ));
    }
}

struct CommandQueue {
    latest: Mutex<Option<Pixels>>,
    /// EC-2 steady-state reuse: the frame-buffer free list. The producer fills
    /// a recycled `Box` popped here (the allocation survives; only its bytes are
    /// rewritten); the worker pushes a buffer here once it has finished reading
    /// it (after transform, pack and device submit). A buffer in the replay
    /// store is never in this list — the replay owner holds it exclusively
    /// while held. After warm-up (the first two frames) the frame path pops
    /// more often than it allocates, so steady ticks allocate zero pixel
    /// buffers; latest-wins overwrites recycle the displaced slot buffer here.
    spare: Mutex<Vec<Pixels>>,
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
    fn submit(&self, pixels: &FrameBytes) {
        {
            let mut last = self.last_accepted.lock().unwrap();
            if last.as_ref() == Some(pixels) {
                // Intended display retention, not an error path: no state
                // message, no slot write, no buffer rotation and no wake
                // happen here (EC-2: a skipped frame never pops or fills a
                // buffer, so the ping-pong rotation cannot disturb newest-wins).
                return;
            }
            *last = Some(*pixels);
        }
        let mut buffer = match self.spare.lock().unwrap().pop() {
            Some(buffer) => buffer,
            None => {
                let fresh = Box::new([0u8; crate::model::WIDTH * crate::model::HEIGHT]);
                #[cfg(test)]
                excl_record("new", self, &fresh);
                fresh
            }
        };
        buffer.copy_from_slice(pixels);
        #[cfg(test)]
        excl_record("fill", self, &buffer);
        let displaced = self.latest.lock().unwrap().replace(buffer);
        if let Some(old) = displaced {
            // Latest-wins overwrite: the displaced buffer was never taken, so
            // the producer still owns it and recycles it, not drops it.
            self.release(old);
        }
        #[cfg(test)]
        self.wake_sends.fetch_add(1, Ordering::SeqCst);
        let _ = self.wake.try_send(());
    }

    /// EC-2 "buffer free" signal: the worker calls this once it has finished
    /// reading the buffer (transform, key pack and device submit are done), so
    /// the producer only refills a buffer after the worker released it.
    fn release(&self, pixels: Pixels) {
        #[cfg(test)]
        excl_record("release", self, &pixels);
        self.spare.lock().unwrap().push(pixels);
    }

    #[cfg(test)]
    fn record_read(&self, pixels: &Pixels) {
        excl_record("read", self, pixels);
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

/// WO-U46G item 5 (audit finding #18): the named loss for a backend worker that
/// could not be spawned. The frozen site was
/// `.expect("spawn backend thread")` (`src/backends/mod.rs:414` on 12ff06d), so
/// an OS thread-creation failure aborted the process. `ProbeFail` is the
/// existing cause the SDK owner-spawn site already uses for the same failure
/// (`src/backends/sdk.rs:1730`), so no new loss cause exists; the OS error rides
/// in the detail. No ownership is claimed by `Backend::spawn` (the worker
/// claims on its own thread), so the rollback is the channels dropping.
fn spawn_failure_loss(error: std::io::Error) -> LossError {
    LossError::new(
        LossCause::ProbeFail,
        format!("spawn backend worker thread: {error}"),
    )
}

// WO-U46G item 5: the test-only injection seam, THREAD-LOCAL so a parallel
// sibling's `Backend::spawn` can never consume a token. Production never reads
// it: the WO bans real OS thread exhaustion, and the failure path keeps the
// production `std::io::Error` shape. A doc comment on a macro call is unused,
// so this note is a plain comment.
#[cfg(test)]
thread_local! {
    // The injection tokens, consumed one per `Backend::spawn` call.
    static SPAWN_FAILURE_TOKENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn inject_backend_spawn_failures(tokens: usize) {
    SPAWN_FAILURE_TOKENS.with(|cell| cell.set(tokens));
}

#[cfg(test)]
fn take_spawn_failure_token() -> bool {
    SPAWN_FAILURE_TOKENS.with(|cell| {
        let left = cell.get();
        if left == 0 {
            return false;
        }
        cell.set(left - 1);
        true
    })
}

impl Backend {
    /// Spawn the backend worker. `kind` comes from resolved configuration
    /// (`auto` resolves to Hid here; the worker falls back to Virtual when
    /// HID discovery cannot find a device).
    ///
    /// WO-U46G item 5: a spawn failure is a `Result` with the named loss, never
    /// a panic. The callers (app.rs startup and reload) publish the established
    /// `Disconnected` row; the reconnect ladder owns the re-attempts, so one
    /// failure here starts no restart storm.
    ///
    /// WO-OPT02A (finding O1): `app_wake` is the app loop's capacity-1 token
    /// sender. The worker publishes every message through a `WakeSender` built
    /// from it, so a state change, a button batch and a health row are each an
    /// immediate app-loop wake. The caller keeps its own sender (it is cloned
    /// per spawn, and `reconfigure` supplies the clone for the new worker).
    pub fn spawn(
        kind: BackendKind,
        cfg: &Config,
        app_wake: SyncSender<()>,
    ) -> Result<Backend, LossError> {
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        let commands = Arc::new(CommandQueue {
            latest: Mutex::new(None),
            spare: Mutex::new(Vec::with_capacity(4)),
            last_accepted: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            reconnect: AtomicBool::new(false),
            wake: wake_tx,
            #[cfg(test)]
            wake_sends: AtomicUsize::new(0),
        });
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let publisher = WakeSender::new(msg_tx, app_wake);
        let reconnect = cfg.logitech_reconnect;
        let reconnect_max = cfg.logitech_reconnect_max;
        let button_poll = cfg.logitech_button_poll;
        let debounce = cfg.logitech_button_debounce;
        let orientation = cfg.logitech_orientation.clone();
        let invert = cfg.logitech_invert;
        let friendly_name = cfg.logitech_friendly_name.clone();
        let worker_commands = Arc::clone(&commands);
        #[cfg(test)]
        if take_spawn_failure_token() {
            // The worker never exists: the channels built above drop here.
            // OS code 8 (ERROR_NOT_ENOUGH_MEMORY) is the shape a real
            // thread-creation failure carries.
            return Err(spawn_failure_loss(std::io::Error::from_raw_os_error(8)));
        }
        let thread = std::thread::Builder::new()
            .name("lcdsirplus-backend".into())
            .spawn(move || {
                worker(
                    kind,
                    worker_commands,
                    wake_rx,
                    publisher,
                    reconnect,
                    reconnect_max,
                    button_poll,
                    debounce,
                    orientation,
                    invert,
                    friendly_name,
                )
            })
            .map_err(spawn_failure_loss)?;
        Ok(Backend {
            commands,
            msg_rx,
            thread: Some(thread),
        })
    }

    pub fn submit(&self, frame: &crate::render::Frame) {
        self.commands.submit(&frame.pixels);
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

    /// WO-U46G item 5: the rebuild is a `Result`. `stop` runs first (the old
    /// worker is joined, its channels closed), so on a spawn failure `self` is
    /// the stopped backend: the app keeps its loop, publishes the named loss
    /// row, submits drop into the shutdown queue, and no thread exists to
    /// restart - the ladder owns re-attempts, so there is no restart storm.
    pub fn reconfigure(
        &mut self,
        kind: BackendKind,
        cfg: &Config,
        app_wake: SyncSender<()>,
    ) -> Result<(), LossError> {
        self.stop();
        *self = Self::spawn(kind, cfg, app_wake)?;
        Ok(())
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
    msg_tx: impl Publish<Message>,
    reconnect: Duration,
    reconnect_max: Duration,
    button_poll: Duration,
    debounce: Duration,
    orientation: String,
    invert: bool,
    friendly_name: String,
) {
    msg_tx.send(Message::State(BackendState::Discovering));
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
            msg_tx.send(Message::State(BackendState::ShutDown));
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
                // EC-2: the payload bytes ride the device-session's reusable
                // buffer inside `submit_frame`; the returned key Vec is the
                // disclosed per-accepted-frame `last_sent` residual.
                device.submit_frame(frame)?;
                Ok(frame.logitech_bytes())
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
/// EC-2 scenario: the device consumes frames with zero recording allocations,
/// so the counting allocator sees only the frame path (queue slot + keys).
#[cfg(test)]
const SCRIPT_QUIET: &str = "scripted-quiet-ticks";
/// EC-2 scenario: the device holds each frame ~15 ms while consuming it, so a
/// producer that refills before the release signal lands its write inside the
/// worker's read window and the exclusivity ledger catches it.
#[cfg(test)]
const SCRIPT_EXCL: &str = "scripted-excl-window";

/// Scripted device history: `(scenario, event, session, device key)`. Events:
/// `open`, `write`, `submit-fail`, `poll-fail`, `close-blank`. The `write` key is
/// the packed report the device received, so a test can name the exact frame.
#[cfg(test)]
type ScriptEvent = (&'static str, &'static str, usize, Vec<u8>);
#[cfg(test)]
type ScriptEventView = (&'static str, usize, Vec<u8>);
#[cfg(test)]
static SCRIPT_EVENTS: Mutex<Vec<ScriptEvent>> = Mutex::new(Vec::new());
/// EC-2 counters: device-side frame consumption per scripted scenario, so the
/// counter and exclusivity tests can wait for their own worker progress.
#[cfg(test)]
static QUIET_READS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static EXCL_READS: AtomicUsize = AtomicUsize::new(0);

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
        if self.scenario == SCRIPT_QUIET {
            // Register this worker thread with the counting harness so the
            // frame-path allocations it performs are inside the measured
            // window; foreign parallel tests are not.
            alloc_count::watch_thread();
            QUIET_READS.fetch_add(1, Ordering::SeqCst);
            return Ok(Vec::new());
        }
        if self.scenario == SCRIPT_EXCL {
            EXCL_READS.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(15));
            return Ok(Vec::new());
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
    // WO-U46G item 3 (audit finding #6) supersedes WO-U46B's refusal for a mode
    // that cannot own the SDK: the circuit guards SDK calls only, so the branch
    // is entered only by a mode that may own an SDK device. An explicit `hid`
    // selection is the backend the user chose, and with the audit #1 wedge
    // (circuit open, episode live, window empty) the flag was permanent after a
    // transient SDK hang: `hid` was then refused forever while the G13 was
    // reachable, panel blank until the process restarted. The 46b core property
    // stands: the resolver stays inside this gate, so `hid`/`virtual` can never
    // receive an SDK device, and `hid` still refuses the device while LCore
    // owns it - `select_physical_kind` answers OwnerConflict below and
    // `src/backends/hid.rs:359-370` re-checks ownership after the exclusive
    // open. SDK/auto keep the 45a shape byte-identically, and `virtual` never
    // reaches this function in production (`worker()` routes it to
    // `virtual_worker`).
    if sdk::circuit_open() && sdk_permits_ownership(kind) {
        // F-REC-1 (WO-U45A-REC-RECOVERY): the ladder pass is the recovery
        // tick clock - one tick per pass, so fresh-open attempts can never
        // run faster than the configured reconnect cadence. The resolver
        // authorizes at most one fresh open per episode window and the first
        // successful call through the fresh owner closes the circuit; a
        // refusal publishes the byte-identical circuit-open row, so the
        // 11/18 ladder shape is unchanged.
        if let Some(device) = sdk::try_circuit_recovery(friendly_name) {
            return Ok(PhysicalDevice::Sdk(device));
        }
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

/// Modes that may own an SDK device: `auto` (arbitration picks SDK when a
/// trusted LCore is present) and `sdk` (explicit). `hid` and `virtual` never
/// do, so the circuit-open recovery resolver is not consulted for them.
fn sdk_permits_ownership(kind: BackendKind) -> bool {
    matches!(kind, BackendKind::Auto | BackendKind::Sdk)
}

/// RC-6 candidate probe used while the ladder is terminal: reuse the open
/// path's own arbitration to answer "is there anything to open". A running
/// trusted LCore is an SDK candidate; otherwise a G13 HID discovery candidate.
/// Read-only — it never opens a device, so it cannot start a connection.
fn device_candidates_present(kind: BackendKind) -> bool {
    let owner_present = sdk::lcore_present().unwrap_or(true);
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
    let present =
        sdk::lcore_present().map_err(|reason| LossError::new(LossCause::ProbeFail, reason))?;
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
        if let Some(stale) = replay.take() {
            // The replay store's buffer was never taken by a device: the
            // worker is done with it, so it returns to reuse (EC-2).
            commands.release(stale);
        }
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
    msg_tx: &impl Publish<Message>,
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
        let reconnect_requested = commands.reconnect.swap(false, Ordering::AcqRel);
        if reconnect_requested {
            backoff = reconnect;
            msg_tx.send(Message::ReconnectStarted);
        }
        if terminal {
            // RC-6: the ladder is terminal. Device-open attempts are skipped;
            // shutdown, frame-submit, and reconnect wakes stay live, and each
            // wake observes the existing reconnect deadline. Ordinary frame wakes
            // must not drive process/HID discovery at the button polling rate.
            if !reconnect_requested && sleep_until_wake(commands, wake_rx, backoff) {
                break 'outer;
            }
            if reconnect_requested || last_probe.is_none_or(|when| when.elapsed() >= reconnect) {
                last_probe = Some(Instant::now());
                if probe(kind) {
                    terminal = false;
                    absent_passes = 0;
                    msg_tx.send(Message::State(BackendState::Discovering));
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
                    msg_tx.send(Message::State(BackendState::Absent {
                        reason: error.to_string(),
                    }));
                } else {
                    msg_tx.send(disconnect(&error));
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
                msg_tx.send(Message::State(BackendState::Connected {
                    kind: active_kind,
                }));
                msg_tx.send(Message::Buttons(
                    tracker.disconnect(Instant::now(), active_kind.name()),
                ));
                let mut last_sent: Option<Vec<u8>> = None;
                let mut last_health: Option<Instant> = None;
                backoff = reconnect;

                loop {
                    if commands.is_shutdown() {
                        if let Err(error) = device.close(true) {
                            msg_tx.send(disconnect(&shutdown_close_error(&error)));
                        }
                        break 'outer;
                    }
                    if commands.reconnect.load(Ordering::Acquire) {
                        if replay.is_none() {
                            replay = current.take();
                        }
                        let _ = device.close(false);
                        msg_tx.send(Message::State(BackendState::Discovering));
                        msg_tx.send(Message::Buttons(
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
                            msg_tx.send(Message::State(BackendState::Discovering));
                            msg_tx.send(Message::Buttons(
                                tracker.disconnect(Instant::now(), active_kind.name()),
                            ));
                            continue 'outer;
                        }
                        Err(error) => {
                            if replay.is_none() {
                                replay = current.take();
                            }
                            let _ = device.close(false);
                            msg_tx.send(disconnect(&error));
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
                        #[cfg(test)]
                        commands.record_read(&pixels);
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
                                    if let Some(old) = current.replace(pixels) {
                                        // Transform, key pack and device submit
                                        // are done: the old frame's buffer is
                                        // free for producer reuse (EC-2).
                                        commands.release(old);
                                    }
                                }
                                Err(mut error) => {
                                    replay = Some(pixels);
                                    if let Err(close_error) = device.close(true) {
                                        error = append_close_failure(error, &close_error);
                                    }
                                    // EC-1: the frame never became visible and the
                                    // close blanked the panel: drop the cached content.
                                    commands.invalidate_identity();
                                    msg_tx.send(disconnect(&error));
                                    msg_tx.send(Message::Buttons(
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
                            if let Some(old) = current.replace(pixels) {
                                // Key-equal frame: the buffer was read for
                                // transform and pack, never submitted; now free.
                                commands.release(old);
                            }
                        }
                    }

                    let poll = device.poll(button_poll.max(Duration::from_millis(10)));
                    let completed_at = Instant::now();
                    if last_sent.is_some()
                        && poll.is_ok()
                        && health_report_due(last_health, completed_at)
                    {
                        msg_tx.send(Message::PhysicalHealthy(active_kind));
                        last_health = Some(completed_at);
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
                                msg_tx.send(Message::Buttons(events));
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
                            msg_tx.send(disconnect(&error));
                            msg_tx.send(Message::Buttons(
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

/// A successful poll is progress even without new buttons or a changed frame.
/// Coalesce health to one message per second; no new thread, timer or queue.
fn health_report_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
}

/// WO-OPT02A (finding O1, A3): the backoff park. It sleeps the FULL `duration`
/// and wakes on a real edge: `CommandQueue::shutdown` (`:415-418`) and
/// `request_reconnect` (`:424-430`) each post the capacity-1 token, and a
/// frame-submit wake re-checks both flags, so shutdown and reconnect are
/// answered at the edge, not at the next poll. The `min(remaining, 100 ms)`
/// clamp this park used to carry is therefore deleted: it made every long
/// ladder sleep a 10 Hz poll (measured: a 60 s backoff = 600 wakes) for a
/// responsiveness the wake edge already provides. `remaining` is never zero
/// here (the `is_zero` guard returns first), so the park never degenerates to a
/// spin.
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
        match wake_rx.recv_timeout(remaining) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
}

/// Terminal-state wait: the same shutdown/reconnect wake edges as
/// `sleep_interruptible` (WO-OPT02A A3: the wake edge, not a 100 ms poll
/// clamp, is what answers them), but returns at the first wake so the terminal
/// worker can re-probe for a hotplugged candidate instead of sleeping out the
/// backoff.
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
        match wake_rx.recv_timeout(remaining) {
            Ok(()) => return false,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
}

fn virtual_fallback_loop(
    commands: &CommandQueue,
    wake_rx: &Receiver<()>,
    msg_tx: &impl Publish<Message>,
) {
    msg_tx.send(Message::State(BackendState::Connected {
        kind: BackendKind::Virtual,
    }));
    while !commands.is_shutdown() {
        if let Some(frame) = commands.take_latest() {
            // EC-2: the virtual device reads nothing, so a taken frame is free
            // for producer reuse the moment it leaves the slot.
            commands.release(frame);
        }
        if wake_rx.recv_timeout(Duration::from_millis(100)).is_err() && commands.is_shutdown() {
            return;
        }
    }
}

fn virtual_worker(
    commands: Arc<CommandQueue>,
    wake_rx: Receiver<()>,
    msg_tx: impl Publish<Message>,
) {
    msg_tx.send(Message::State(BackendState::Connected {
        kind: BackendKind::Virtual,
    }));
    virtual_fallback_loop(&commands, &wake_rx, &msg_tx);
    msg_tx.send(Message::State(BackendState::ShutDown));
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

/// EC-2 acceptance (1) harness: a counting `GlobalAlloc` that passes through to
/// `System` and buckets every allocation (`alloc`, `alloc_zeroed`, `realloc`)
/// by layout length. Frame-path classes: 6880 = pixel buffers (queue-slot
/// `Box` + SDK session payload) and 992 = the HID report key Vec; lengths up
/// to 256 are the fixed per-send metadata (mpsc reply-channel nodes); the rest
/// is "other". Counters are plain statics, so the instrumentation allocates
/// nothing. `GlobalAlloc` is an `unsafe` trait (the only reason the harness
/// mentions `unsafe`); it compiles for `cfg(test)` only — the production frame
/// path and its zero-unsafe rule are untouched.
///
/// WO-QA-01 (known issue F-1): the counters measure ONE armed window at a time.
/// [`window`] arms a window for the calling thread and returns a `Watch` guard;
/// [`watch_thread`] attaches a frame-path worker thread to the window that is
/// armed right now. Only threads attached to the armed cohort are counted — every
/// other allocation in the process lands in the foreign bucket — and dropping the
/// guard releases the cohort's slots (release-clear). The pre-fix harness counted
/// every thread that had EVER registered: the 8 `WATCHED` slots were never cleared
/// (`WO-B-03-FIX-AUDIT` F-1 — one libtest worker that ran an alloc test stayed
/// watched for the whole process, so each window measured the idle-path
/// allocations of every other test's worker; `WO-A1-06-FIX-AUDIT` N-C measured
/// `(0, 0, 1, 0, 1)` on the skipped-tick assert). The single-window claim also
/// serializes the alloc tests instead of letting parallel tests share global
/// counters, which is what made the family load-sensitive.
#[cfg(test)]
pub mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    pub const PIXELS: usize = crate::model::WIDTH * crate::model::HEIGHT;
    pub const HID_REPORT: usize = super::g13::G13_OUTPUT_REPORT_LENGTH;

    static N_PIXEL: AtomicUsize = AtomicUsize::new(0);
    static N_KEY: AtomicUsize = AtomicUsize::new(0);
    static N_METADATA: AtomicUsize = AtomicUsize::new(0);
    static N_OTHER: AtomicUsize = AtomicUsize::new(0);
    static N_TOTAL: AtomicUsize = AtomicUsize::new(0);
    static N_FOREIGN: AtomicUsize = AtomicUsize::new(0);

    /// Threads attached to the armed window: `WATCHED[i]` is the thread id
    /// (0 = free slot), `COHORT[i]` the cohort of the window that registered it.
    /// A cohort is one armed window; [`Watch`]'s `drop` returns the cohort's slots
    /// to the free pool, so a thread that once ran an alloc test is NOT watched
    /// for the rest of the process (the pre-fix leak F-1 names).
    static WATCHED: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
    static COHORT: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
    /// The cohort id of the one armed window (0 = none armed), and the allocator
    /// of cohort ids. Cohort ids are monotonic so a stale slot can never join a
    /// later window.
    static ARMED: AtomicUsize = AtomicUsize::new(0);
    static NEXT_COHORT: AtomicUsize = AtomicUsize::new(1);

    fn thread_id() -> usize {
        // SAFETY (test harness only): GetCurrentThreadId reads this thread's
        // own id; no allocation, so it is safe to call inside the allocator.
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() as usize }
    }

    /// Attach the calling thread to `cohort`; true when a slot holds it. Claiming
    /// the slot and stamping its cohort are two atomic steps: in the gap the
    /// thread is uncounted (foreign), never miscounted into another cohort.
    fn attach(cohort: usize) -> bool {
        let id = thread_id();
        for (slot, owned) in WATCHED.iter().zip(COHORT.iter()) {
            if slot.load(Ordering::SeqCst) == id && owned.load(Ordering::SeqCst) == cohort {
                return true;
            }
        }
        for (slot, owned) in WATCHED.iter().zip(COHORT.iter()) {
            if slot
                .compare_exchange(0, id, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                owned.store(cohort, Ordering::SeqCst);
                return true;
            }
        }
        false
    }

    /// The thread filter (acceptance (1)): the calling thread is inside the armed
    /// window, i.e. it is the arming test thread or a worker it registered.
    fn in_window() -> bool {
        let cohort = ARMED.load(Ordering::SeqCst);
        if cohort == 0 {
            return false;
        }
        let id = thread_id();
        WATCHED.iter().zip(COHORT.iter()).any(|(slot, owned)| {
            slot.load(Ordering::SeqCst) == id && owned.load(Ordering::SeqCst) == cohort
        })
    }

    /// Register the calling thread with the window armed right now (the scripted
    /// frame-path worker and the SDK recording owner call this on their first
    /// execute). A no-op while no window is armed: registering then would occupy a
    /// slot of a window that never measures it, which is the leak vector.
    pub fn watch_thread() {
        let cohort = ARMED.load(Ordering::SeqCst);
        if cohort != 0 {
            attach(cohort);
        }
    }

    /// A guarded measurement window. Dropping it is the release-clear: every slot
    /// attached to the cohort goes back to the pool and the counters are zeroed.
    pub struct Watch {
        cohort: usize,
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            for (slot, owned) in WATCHED.iter().zip(COHORT.iter()) {
                if owned.load(Ordering::SeqCst) == self.cohort {
                    owned.store(0, Ordering::SeqCst);
                    slot.store(0, Ordering::SeqCst);
                }
            }
            ARMED.store(0, Ordering::SeqCst);
            reset();
        }
    }

    /// Arm a measurement window on the calling thread and zero the counters.
    /// One window is armed per process; a second concurrent arm yields until the
    /// first guard drops, and a 10 s armed window panics loudly (no silent hang).
    /// shortcut: one window per process, so the alloc tests serialize on their
    /// ~ms windows; upgrade to per-cohort counters if a third window appears.
    pub fn window() -> Watch {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let cohort = NEXT_COHORT.load(Ordering::SeqCst);
            if ARMED
                .compare_exchange(0, cohort, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                NEXT_COHORT.fetch_add(1, Ordering::SeqCst);
                assert!(
                    attach(cohort),
                    "the armed window's own thread found no free WATCHED slot"
                );
                reset();
                return Watch { cohort };
            }
            assert!(
                Instant::now() < deadline,
                "an alloc measurement window stayed armed for 10 s"
            );
            std::thread::yield_now();
        }
    }

    pub fn foreign() -> usize {
        N_FOREIGN.load(Ordering::SeqCst)
    }

    /// (pixel 6880, key 992, metadata <=256, other, total)
    pub type Counts = (usize, usize, usize, usize, usize);

    fn bump(len: usize) {
        // The thread filter: only a thread attached to the armed cohort is counted.
        // Probe (i) of WO-QA-01 acceptance (1) removes this gate and the
        // `alloc_window_ignores_allocations_of_other_threads_under_load` test fails,
        // so the window's measurement scope is load-bearing.
        if !in_window() {
            N_FOREIGN.fetch_add(1, Ordering::Relaxed);
            return;
        }
        N_TOTAL.fetch_add(1, Ordering::Relaxed);
        match len {
            PIXELS => N_PIXEL.fetch_add(1, Ordering::Relaxed),
            HID_REPORT => N_KEY.fetch_add(1, Ordering::Relaxed),
            0..=256 => N_METADATA.fetch_add(1, Ordering::Relaxed),
            _ => N_OTHER.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn reset() {
        for counter in [
            &N_PIXEL,
            &N_KEY,
            &N_METADATA,
            &N_OTHER,
            &N_TOTAL,
            &N_FOREIGN,
        ] {
            counter.store(0, Ordering::SeqCst);
        }
    }

    pub fn snapshot() -> Counts {
        (
            N_PIXEL.load(Ordering::SeqCst),
            N_KEY.load(Ordering::SeqCst),
            N_METADATA.load(Ordering::SeqCst),
            N_OTHER.load(Ordering::SeqCst),
            N_TOTAL.load(Ordering::SeqCst),
        )
    }

    pub fn subtract(after: Counts, before: Counts) -> Counts {
        (
            after.0 - before.0,
            after.1 - before.1,
            after.2 - before.2,
            after.3 - before.3,
            after.4 - before.4,
        )
    }

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump(layout.size());
            System.alloc(layout)
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            bump(layout.size());
            System.alloc_zeroed(layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            bump(new_size);
            System.realloc(ptr, layout, new_size)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            System.dealloc(ptr, layout)
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;
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
                spare: Mutex::new(Vec::with_capacity(4)),
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

    /// WO-OPT02A (finding O1): the app-side wake sender the `Backend::spawn`
    /// tests supply. Production owns the real token pair in `app::run`; here the
    /// receiver is dropped, which is exactly the state a closed app loop leaves,
    /// and `WakeSender::send` is best-effort by design, so the worker keeps
    /// publishing and the tests observe the message channel as before.
    fn app_wake() -> SyncSender<()> {
        std::sync::mpsc::sync_channel(1).0
    }

    #[test]
    fn command_queue_retains_only_latest_frame_and_shutdown_is_reliable() {
        let (queue, wake) = command_queue();
        queue.submit(&pixels(1));
        queue.submit(&pixels(2));
        queue.submit(&pixels(3));
        assert_eq!(wake.try_recv(), Ok(()));
        assert!(wake.try_recv().is_err(), "wake channel is bounded to one");
        assert_eq!(queue.take_latest().unwrap()[0], 3);
        assert!(queue.take_latest().is_none());

        queue.submit(&pixels(4));
        queue.shutdown();
        assert!(
            queue.is_shutdown(),
            "shutdown does not depend on wake capacity"
        );
    }

    #[test]
    fn backoff_retains_one_newest_frame_for_one_reconnect_replay() {
        let (queue, wake) = command_queue();
        queue.submit(&pixels(1));
        queue.submit(&pixels(2));
        queue.submit(&pixels(3));
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
        backend.commands.submit(&pixels(7));
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

    /// WO-OPT02A (finding O1, A3): the sibling the reconnect test above never
    /// had. A SHUTDOWN issued while a worker parks a 120 s backoff is answered
    /// inside the same 150 ms budget the reconnect sibling uses, on the wake
    /// edge (`CommandQueue::shutdown` posts the capacity-1 token), not on a
    /// poll. The first assert is the guard that removing the
    /// `min(remaining, 100 ms)` clamp did NOT shorten the ladder (the park is
    /// the full backoff); the second is the shutdown edge the clamp used to be
    /// the only guarantee of.
    #[test]
    fn shutdown_wakes_a_worker_in_long_backoff_within_the_reconnect_budget() {
        let (commands, wake) = command_queue();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let shutdown = sleep_interruptible(&worker_commands, &wake, Duration::from_secs(120));
            done_tx.send(shutdown).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "a 120 s backoff stays a 120 s backoff after the clamp removal"
        );
        commands.shutdown();
        let result = done_rx.recv_timeout(Duration::from_millis(150));
        assert_eq!(
            result.ok(),
            Some(true),
            "shutdown returns within the 150 ms budget, on the wake edge"
        );
        thread.join().unwrap();
    }

    // EC-1 seams: the identity gate is exercised on the queue's own wake
    // counter, so a suppressed frame is observable as a zero, with no device
    // path involved.
    #[test]
    fn identical_frame_content_skips_enqueue_and_wake() {
        let (queue, wake) = command_queue();
        queue.submit(&pixels(1));
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

        queue.submit(&pixels(1));
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
        queue.submit(&changed);
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
        queue.submit(&pixels(5));
        assert_eq!(
            queue.take_latest().unwrap()[0],
            5,
            "the worker takes the frame"
        );
        queue.submit(&pixels(5));
        assert_eq!(queue.wake_sends.load(Ordering::SeqCst), 1);
        assert!(
            queue.take_latest().is_none(),
            "the repeat stays suppressed until the connection is rebuilt"
        );

        queue.request_reconnect();
        assert!(queue.reconnect.load(Ordering::Acquire));
        assert_eq!(wake.try_recv(), Ok(()), "the reconnect wake stays live");
        queue.submit(&pixels(5));
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
        queue.submit(&pixels(6));
        queue.submit(&pixels(6));
        assert_eq!(
            queue.wake_sends.load(Ordering::SeqCst),
            1,
            "the repeat is suppressed while the panel shows it"
        );

        queue.invalidate_identity();
        queue.submit(&pixels(6));
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
        } else if friendly_name == SCRIPT_QUIET {
            SCRIPT_QUIET
        } else if friendly_name == SCRIPT_EXCL {
            SCRIPT_EXCL
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
        commands.submit(&pixels(value));
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
        commands.submit(&pixels(6));
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
        commands.submit(&pixels(7));
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
        let mut backend = Backend::spawn(BackendKind::Virtual, &base, app_wake())
            .expect("the virtual backend spawns (WO-U46G item 5: the signature row)");
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
        backend
            .reconfigure(BackendKind::Virtual, &rotated, app_wake())
            .expect("the virtual backend reconfigures (WO-U46G item 5: the signature row)");
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
        queue.submit(&pixels(1));
        assert_eq!(wake.try_recv(), Ok(()));
        assert_eq!(
            queue.take_latest().unwrap()[0],
            1,
            "the worker takes the frame"
        );

        queue.submit(&pixels(1));
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
        queue.submit(&pixels(2));
        queue.submit(&pixels(3));
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
        commands.submit(&pixels(9));
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
        commands.submit(&pixels(10));
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

    // EC-2 seams: the counting allocator (alloc_count) measures the frame path,
    // and the exclusivity ledger (EXCL_EVENTS) decides, per buffer address,
    // whether the producer only ever refills buffers the worker released.
    fn uniform(value: u8) -> [u8; crate::model::WIDTH * crate::model::HEIGHT] {
        [value; crate::model::WIDTH * crate::model::HEIGHT]
    }

    fn wait_for_reads(counter: &AtomicUsize, want: usize) {
        let budget = Instant::now() + Duration::from_secs(10);
        while counter.load(Ordering::SeqCst) < want {
            assert!(
                Instant::now() < budget,
                "the scripted device never consumed frame {want}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn steady_frame_path_allocates_no_pixel_buffers_after_warm_up() {
        // Acceptance (1) at the queue-slot boundary: after the two warm-up
        // frames, 8 NEW-content ticks allocate zero 6880-byte pixel buffers
        // (Box ping-pong), and 8 SKIPPED (identical, EC-1-suppressed) ticks
        // allocate nothing at all. The disclosed residuals on new-content
        // ticks are the per-accepted-frame 992-byte HID key Vec (EC-1
        // last_sent), the per-send test-instrumentation pushes, and fixed
        // per-send metadata; every one of these is counted, not assumed.
        // WO-QA-01 (F-1): the measurement window is armed on this test thread and
        // released by the guard's drop; the scripted worker joins this window at its
        // first quiet submit, so the window measures producer + worker and nobody else.
        let _window = crate::backends::alloc_count::window();
        crate::backends::alloc_count::watch_thread();
        let (commands, wake) = command_queue();
        let (tx, _rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let base = Duration::from_millis(1);
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
                SCRIPT_QUIET.to_string(),
                open_scripted,
                probe_never,
            );
        });

        crate::backends::alloc_count::reset();
        let warm = crate::backends::alloc_count::snapshot();
        commands.submit(&uniform(1));
        wait_for_reads(&QUIET_READS, 1);
        commands.submit(&uniform(2));
        wait_for_reads(&QUIET_READS, 2);
        let warm =
            crate::backends::alloc_count::subtract(crate::backends::alloc_count::snapshot(), warm);

        let mut new = crate::backends::alloc_count::Counts::default();
        for tick in 0..8u8 {
            let before = crate::backends::alloc_count::snapshot();
            commands.submit(&uniform(3 + tick));
            wait_for_reads(&QUIET_READS, 3 + tick as usize);
            new = crate::backends::alloc_count::subtract(
                crate::backends::alloc_count::snapshot(),
                before,
            );
            assert_eq!(
                new.0, 0,
                "steady new-content tick {}: zero 6880-byte frame-path allocations (pixel class), measured: {new:?}",
                3 + tick
            );
            assert!(
                new.1 >= 1,
                "tick {}: the disclosed 992-byte key Vec residual must be visible, not faked away",
                3 + tick
            );
        }
        // One settle window before measuring skips: std's bounded channel
        // materializes a per-thread blocking node on the worker's first idle
        // timed recv. That one-shot is counted in `settle` below and
        // disclosed; the eight measured skip windows stay allocation-free.
        let settle = crate::backends::alloc_count::snapshot();
        std::thread::sleep(Duration::from_millis(6));
        let settle = crate::backends::alloc_count::subtract(
            crate::backends::alloc_count::snapshot(),
            settle,
        );
        let mut skipped = (0usize, 0usize, 0usize, 0usize, 0usize);
        for _ in 0..8 {
            let before = crate::backends::alloc_count::snapshot();
            commands.submit(&uniform(10));
            std::thread::sleep(Duration::from_millis(3));
            let d = crate::backends::alloc_count::subtract(
                crate::backends::alloc_count::snapshot(),
                before,
            );
            assert_eq!(
                d.4, 0,
                "steady skipped tick: zero heap allocations at all, measured: {d:?}"
            );
            skipped = (
                skipped.0 + d.0,
                skipped.1 + d.1,
                skipped.2 + d.2,
                skipped.3 + d.3,
                skipped.4 + d.4,
            );
        }
        println!(
            "WO-A1-02 EC-2 queue-slot measured (pixel 6880, key 992, metadata <=256, other, total): warm2={warm:?} newtick8(last)={new:?} settle6ms={settle:?} skipped8sum={skipped:?} foreign_parallel={}",
            crate::backends::alloc_count::foreign()
        );
        assert_eq!(commands.wake_sends.load(Ordering::SeqCst), 10);

        commands.shutdown();
        thread.join().unwrap();
    }

    #[test]
    fn alloc_window_ignores_allocations_of_other_threads_under_load() {
        // WO-QA-01 (A), acceptance (1): the armed window measures its OWN threads
        // only. Four threads allocate every measured class (6880 pixel, 992 key,
        // <=256 metadata) continuously across the armed window; every one of them
        // must land in the foreign bucket and the window must read exactly zero.
        // Probe (i) — deleting the thread filter in `bump` — fails this test, so the
        // filter is load-bearing, not decorative. The load threads are spawned
        // BEFORE the window is armed, so even the spawn bookkeeping allocations
        // (metadata class, the EC-2-disclosed instrumentation pattern) are outside
        // the measured window; the armed thread itself allocates nothing inside it.
        // WO-QA-05 (WO-QA-04-AUDIT R6, ranked option 1 = SELF-MEASURED ORACLE): the span
        // is event-proven, not wall-clock. Each load thread publishes the allocations it
        // has LANDED (one report per allocation site) and the window closes its
        // observation when every thread has self-reported a full round of the three
        // classes, bounded by the WO-QA-01 10 s loud-panic budget. The liveness witness is
        // the actor's own report: "foreign allocations happened" is proven by the threads
        // that made them, not by a clock. The 30 ms sleep this replaces was not a liveness
        // test but a volume race: `window()` RESETS the counters (`reset()` inside
        // `alloc_count::window`, infra mod.rs:1266), so the old form compared the whole
        // pre-arm foreign history (measured 1,116,691) with a post-arm count restarted at
        // zero (measured 984,854) - the red row in receipt §2. Both baselines below are
        // read in the window's own epoch, and each SeqCst report is published AFTER that
        // thread's `N_FOREIGN` bump (the bump runs first in program order), so observing n
        // report increments beyond a baseline PROVES the foreign counter grew by at least
        // n in the same span. No assertion was deleted and no sleep was inflated.
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static STOP: AtomicBool = AtomicBool::new(false);
        // One self-report slot per load thread: the allocations it has landed.
        static LANDED: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
        RUNNING.store(0, Ordering::SeqCst);
        STOP.store(false, Ordering::SeqCst);
        for report in &LANDED {
            report.store(0, Ordering::SeqCst);
        }
        let mut handles = Vec::new();
        for report in LANDED.iter() {
            handles.push(std::thread::spawn(move || {
                RUNNING.fetch_add(1, Ordering::SeqCst);
                while !STOP.load(Ordering::SeqCst) {
                    let pixel = vec![7u8; crate::backends::alloc_count::PIXELS];
                    report.fetch_add(1, Ordering::SeqCst);
                    let key = vec![9u8; crate::backends::alloc_count::HID_REPORT];
                    report.fetch_add(1, Ordering::SeqCst);
                    // A Box, not a vec!: the point is the 64-byte heap allocation in the
                    // metadata class, and clippy's array suggestion would move it to the
                    // stack and un-measure the class.
                    let metadata: Box<[u8; 64]> = Box::new([1u8; 64]);
                    report.fetch_add(1, Ordering::SeqCst);
                    std::hint::black_box(pixel[0] + key[0] + metadata[0]);
                }
            }));
        }
        while RUNNING.load(Ordering::SeqCst) < 4 {
            std::thread::sleep(Duration::from_millis(1));
        }
        let _window = crate::backends::alloc_count::window();
        let before = crate::backends::alloc_count::snapshot();
        // The window is armed, so the counters are in THIS window's epoch (the arm zeroed
        // them): the foreign baseline is read after the arm and is comparable with the
        // after-sample, which is what makes the liveness assertion provable.
        let foreign_before = crate::backends::alloc_count::foreign();
        let witness_base: [usize; 4] = std::array::from_fn(|id| LANDED[id].load(Ordering::SeqCst));
        let rounds = || -> usize {
            LANDED
                .iter()
                .zip(witness_base.iter())
                .map(|(report, base)| report.load(Ordering::SeqCst) - *base)
                .min()
                .unwrap_or(0)
        };
        // The event-proven span: every load thread must land a full round (all three
        // measured classes) beyond its baseline. It closes as soon as the witness is
        // satisfied - typically well under a millisecond - and 10 s of total starvation
        // panics loudly with every counter value, so a starved cohort is never a green.
        const ROUNDS: usize = 3;
        let span = Instant::now();
        let deadline = span + Duration::from_secs(10);
        while rounds() < ROUNDS {
            assert!(
                Instant::now() < deadline,
                "the load threads landed no full allocation round within the 10 s budget: self_report_min_rounds={} base={witness_base:?} reports={:?} foreign_before={foreign_before} span_ms={}",
                rounds(),
                LANDED.iter().map(|report| report.load(Ordering::SeqCst)).collect::<Vec<usize>>(),
                span.elapsed().as_millis(),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let span_ms = span.elapsed().as_millis();
        let d = crate::backends::alloc_count::subtract(
            crate::backends::alloc_count::snapshot(),
            before,
        );
        STOP.store(true, Ordering::SeqCst);
        for handle in handles {
            handle.join().unwrap();
        }
        let foreign_after = crate::backends::alloc_count::foreign();
        // The actor witness summed over the four load threads: allocations PROVEN to have
        // landed inside the window's span, each one counted by `bump` as foreign.
        let self_report: usize = LANDED
            .iter()
            .zip(witness_base.iter())
            .map(|(report, base)| report.load(Ordering::SeqCst) - *base)
            .sum();
        assert_eq!(
            d.4, 0,
            "the armed window measured allocations made by other threads: {d:?}"
        );
        assert_eq!(
            d.0, 0,
            "the 6880-byte class is not contaminated by cross-thread loads: {d:?}"
        );
        assert!(
            self_report >= ROUNDS * 4,
            "the load threads' self-reports must prove a full allocation round each: self_report={self_report} min={} span_ms={span_ms}",
            ROUNDS * 4
        );
        assert!(
            foreign_after > foreign_before,
            "the load threads' allocations must be counted as foreign, not lost: before={foreign_before} after={foreign_after} self_report={self_report} span_ms={span_ms}"
        );
        println!(
            "WO-QA-05-ALLOC-WINDOW before={before:?} d={d:?} foreign_before={foreign_before} foreign_after={foreign_after} self_report={self_report} span_ms={span_ms}"
        );
    }

    #[test]
    fn alloc_window_slots_are_released_when_the_watch_closes() {
        // WO-QA-01 (A), acceptance (1)(ii): closing a window releases its WATCHED
        // slots, so slots cannot leak across runs. Twelve windows (each with one
        // registered worker thread) exceed the 8-slot pool; a thirteenth window on a
        // fresh thread must still register and measure its own 6880-byte allocation.
        // Probe (ii) — deleting the slot release from `Watch::drop` — exhausts the
        // pool and this test fails, so the release-clear is load-bearing.
        for _ in 0..12 {
            let window = crate::backends::alloc_count::window();
            std::thread::spawn(|| {
                crate::backends::alloc_count::watch_thread();
                std::hint::black_box(vec![7u8; crate::backends::alloc_count::PIXELS]);
            })
            .join()
            .unwrap();
            drop(window);
        }
        let measured = std::thread::spawn(|| {
            let _window = crate::backends::alloc_count::window();
            let before = crate::backends::alloc_count::snapshot();
            std::hint::black_box(vec![7u8; crate::backends::alloc_count::PIXELS]);
            crate::backends::alloc_count::subtract(crate::backends::alloc_count::snapshot(), before)
        })
        .join()
        .unwrap();
        assert_eq!(
            measured.0, 1,
            "the 13th window still measures its own pixel-class allocation, so the 12 earlier slots were released, not leaked: {measured:?}"
        );
        assert_eq!(
            measured.4, 1,
            "the released-slot window measures exactly that one allocation: {measured:?}"
        );
    }

    #[test]
    fn queue_slot_buffer_exclusivity_producer_writes_only_released_buffers() {
        // Acceptance (3): the scripted device holds each frame ~15 ms while
        // consuming it; the producer submits every ~3 ms, so any
        // refill-before-release write lands inside the worker's read window
        // and breaks the per-buffer state machine below. Mirrors the
        // owner_serializes_submit_poll_close_and_shutdown shape: scripted
        // device + counters, exclusivity asserted from measured events.
        EXCL_READS.store(0, Ordering::SeqCst);
        let (commands, wake) = command_queue();
        let queue_addr = std::ptr::from_ref(&*commands) as usize;
        excl_begin(queue_addr);
        let (tx, _rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let base = Duration::from_millis(1);
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
                SCRIPT_EXCL.to_string(),
                open_scripted,
                probe_never,
            );
        });

        for value in 1..=60u8 {
            commands.submit(&uniform(value));
            // WO-QA-02 driven cadence: the producer advances one scripted-device
            // read per submit through the pre-existing progress seam
            // (`wait_for_reads` + `EXCL_READS`), so the scenario carries no fixed
            // 3 ms/15 ms producer-consumer ratio to lose under CPU
            // oversubscription; the 60 driven reads make the read/recycle counts
            // arithmetic, and the assertions below are untouched.
            wait_for_reads(&EXCL_READS, value as usize);
        }
        commands.shutdown();
        thread.join().unwrap();
        excl_end();

        let events: Vec<_> = EXCL_EVENTS
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.1 == queue_addr)
            .map(|event| (event.0, event.2, event.3))
            .collect();
        let mut state: std::collections::HashMap<usize, (&str, u8)> =
            std::collections::HashMap::new();
        let mut reuse_fills = 0usize;
        let mut reads = 0usize;
        let mut violations: Vec<String> = Vec::new();
        for (kind, buffer, content) in events.clone() {
            match kind {
                "new" => {
                    assert!(
                        state.insert(buffer, (kind, 0)).is_none(),
                        "the same buffer address was boxed twice in one queue"
                    );
                }
                "fill" => {
                    let prev = state.get(&buffer).map(|entry| entry.0);
                    match prev {
                        Some("new") => {}
                        Some("release") => reuse_fills += 1,
                        other => {
                            let history: Vec<_> = events
                                .iter()
                                .filter(|event| event.1 == buffer)
                                .map(|event| (event.0, event.2))
                                .collect();
                            violations.push(format!(
                                "producer refilled a buffer after {other:?} (history: {history:?})"
                            ));
                        }
                    }
                    state.insert(buffer, ("fill", content));
                }
                "read" => {
                    reads += 1;
                    match state.get(&buffer).copied() {
                        Some(entry) if entry.0 == "fill" || entry.0 == "read" => {
                            assert_eq!(
                                entry.1, content,
                                "the worker read content {content} after a fill of {}: producer and worker overlapped on one buffer",
                                entry.1
                            );
                        }
                        other => panic!("read of a buffer in state {other:?}"),
                    }
                    state.insert(buffer, ("read", content));
                }
                "release" => {
                    let prev = state.get(&buffer).map(|entry| entry.0);
                    match prev {
                        Some("fill") | Some("read") => {}
                        Some("release") => {
                            panic!("double release: the same buffer entered the spare list twice")
                        }
                        Some("new") => panic!("release of a buffer the producer never filled"),
                        _ => panic!("release of an unknown buffer"),
                    }
                    state.insert(buffer, ("release", 0));
                }
                other => unreachable!("unknown exclusivity event {other}"),
            }
        }
        assert!(
            violations.is_empty(),
            "EC-2 exclusivity violated: {}",
            violations.join("; ")
        );
        assert!(
            reads >= 10,
            "the worker consumed {reads} frames; the scenario must exercise the read path"
        );
        assert!(
            reuse_fills >= 4,
            "the ping-pong must recycle released buffers; fills-after-release counted {reuse_fills}"
        );
        println!("WO-A1-02 EC-2 exclusivity measured: reads={reads} reuse_fills={reuse_fills} buffers={}", state.len());
    }
    #[test]
    fn connection_health_continues_after_first_frame_without_per_tick_messages() {
        let start = Instant::now();
        assert!(health_report_due(None, start));
        assert!(!health_report_due(
            Some(start),
            start + Duration::from_millis(999)
        ));
        assert!(health_report_due(
            Some(start),
            start + Duration::from_secs(1)
        ));
        assert!(!health_report_due(
            Some(start + Duration::from_secs(1)),
            start + Duration::from_millis(1100)
        ));
        assert!(health_report_due(
            Some(start + Duration::from_secs(1)),
            start + Duration::from_secs(2)
        ));
    }

    #[test]
    fn physical_worker_reports_progress_after_the_initial_health_message() {
        fn open_health(_: BackendKind, _: &str) -> Result<PhysicalDevice, LossError> {
            Ok(PhysicalDevice::Scripted(ScriptedDevice {
                scenario: "scripted-health-progress",
                session: 1,
            }))
        }
        let (commands, wake) = command_queue();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_commands = Arc::clone(&commands);
        let worker = std::thread::spawn(move || {
            let interval = Duration::from_millis(10);
            physical_worker(
                BackendKind::Hid,
                &worker_commands,
                &wake,
                &tx,
                interval,
                interval,
                interval,
                interval,
                "normal".into(),
                false,
                "health-progress".into(),
                open_health,
                probe_never,
            );
        });
        commands.submit(&pixels(1));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut health = 0;
        while health < 2 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(Message::PhysicalHealthy(BackendKind::Hid)) => health += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        commands.shutdown();
        worker.join().unwrap();
        assert_eq!(
            health, 2,
            "a healthy established worker must keep reporting completed I/O progress"
        );
    }

    #[test]
    fn the_circuit_open_ladder_keeps_its_row_shape_and_never_terminalizes() {
        // F-REC-1 evidence shape (the 11/18 natural incident): every ladder
        // pass publishes the circuit-open disconnect row, and CircuitOpen is
        // not DeviceAbsent, so the ladder never enters the Absent terminal.
        // The WO-U45A resolver tick rides this same ladder, so the recovery
        // window stays live at the capped cadence and the row shape the
        // incident recorded is byte-pinned here.
        static CIRCUIT_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        fn open_circuit_open(
            _kind: BackendKind,
            _friendly_name: &str,
        ) -> Result<PhysicalDevice, LossError> {
            CIRCUIT_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
            Err(LossError::new(
                LossCause::CircuitOpen,
                "physical backend circuit is open after an unreturning SDK owner".into(),
            ))
        }
        let base = Duration::from_millis(1);
        CIRCUIT_ATTEMPTS.store(0, Ordering::SeqCst);
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
                base * 16,
                base,
                base,
                "normal".into(),
                false,
                String::new(),
                open_circuit_open,
                probe_never,
            )
        });
        let rows = 8usize;
        let budget = Instant::now() + Duration::from_secs(10);
        for row in 0..rows {
            let left = budget.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the circuit-open rows stopped at {row}");
            match rx
                .recv_timeout(left)
                .expect("a circuit-open row per ladder pass")
            {
                Message::State(BackendState::Disconnected { cause, reason }) => {
                    assert_eq!(cause, LossCause::CircuitOpen, "row {row}");
                    assert_eq!(
                        reason,
                        "circuit-open: physical backend circuit is open after an unreturning SDK owner",
                        "row {row} is byte-identical to the 11/18 ladder rows"
                    );
                }
                Message::State(BackendState::Absent { reason }) => {
                    panic!("row {row}: the circuit-open ladder terminalized: {reason}")
                }
                _ => panic!("row {row}: expected the circuit-open disconnect publication"),
            }
        }
        // Bounded settle window (500 ms): the circuit-open ladder publishes
        // rows at the capped cadence FOREVER by design - exactly the 11/18
        // shape - so an unbounded drain never closes. Assert: no Absent
        // publication (CircuitOpen passes never accumulate the
        // zero-candidate count), and the rows keep coming.
        let drain_deadline = Instant::now() + Duration::from_millis(500);
        let mut extra_absent = 0usize;
        let mut extra_rows = 0usize;
        loop {
            let left = drain_deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(Message::State(BackendState::Absent { .. })) => extra_absent += 1,
                Ok(Message::State(BackendState::Disconnected { .. })) => extra_rows += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert_eq!(
            extra_absent, 0,
            "the circuit-open ladder never terminalizes"
        );
        assert!(
            extra_rows > 0,
            "the ladder keeps publishing rows at the capped cadence (the 11/18 window)"
        );
        assert!(
            CIRCUIT_ATTEMPTS.load(Ordering::SeqCst) >= rows,
            "opens keep running on the ladder, so the recovery window stays live"
        );
        commands.shutdown();
        thread.join().unwrap();
    }

    #[test]
    fn the_worker_publishes_connected_and_health_when_a_circuit_open_episode_reopens() {
        // The WO-U45A hunk in `open_physical` returns `Ok` on a fresh
        // session from a ladder pass that began with the refusal shape. The
        // recovery publish is the worker's EXISTING machinery: `Connected`,
        // then `PhysicalHealthy` (consumed at app.rs by recovery.healthy,
        // which clears the loss labels on real health only). No new message,
        // state, log label or journal vocabulary exists on this path.
        static REOPEN_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn open_refuse_then_reopen(
            _kind: BackendKind,
            _friendly_name: &str,
        ) -> Result<PhysicalDevice, LossError> {
            if REOPEN_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(LossError::new(
                    LossCause::CircuitOpen,
                    "physical backend circuit is open after an unreturning SDK owner".into(),
                ));
            }
            Ok(PhysicalDevice::Scripted(ScriptedDevice {
                scenario: "scripted-circuit-reopen",
                session: 1,
            }))
        }
        let base = Duration::from_millis(1);
        REOPEN_CALLS.store(0, Ordering::SeqCst);
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
                String::new(),
                open_refuse_then_reopen,
                probe_never,
            )
        });
        let budget = Instant::now() + Duration::from_secs(10);
        let remaining = || budget.saturating_duration_since(Instant::now());
        // (1) The refusal publishes the circuit-open row (unchanged shape).
        loop {
            let left = remaining();
            assert!(
                !left.is_zero(),
                "the circuit-open refusal row never arrived"
            );
            if let Ok(Message::State(BackendState::Disconnected { cause, reason })) =
                rx.recv_timeout(left)
            {
                assert_eq!(cause, LossCause::CircuitOpen);
                assert!(reason.starts_with("circuit-open: "), "{reason}");
                break;
            }
        }
        // (2) The reopen publishes Connected: the existing recovery surface.
        let mut connected = false;
        while !connected {
            let left = remaining();
            assert!(
                !left.is_zero(),
                "the reopened session never published Connected"
            );
            if let Ok(Message::State(BackendState::Connected { kind })) = rx.recv_timeout(left) {
                assert_eq!(
                    kind,
                    BackendKind::Hid,
                    "the scripted fresh session reports its kind"
                );
                connected = true;
            }
        }
        // (3) Real health rides the existing cadence: a frame submit makes
        // the session write; the completed poll reports the health message.
        commands.submit(&pixels(3));
        let mut health = 0usize;
        while health < 1 {
            let left = remaining();
            assert!(
                !left.is_zero(),
                "the reopened session published no health message"
            );
            if let Ok(Message::PhysicalHealthy(_)) = rx.recv_timeout(left) {
                health += 1;
            }
        }
        commands.shutdown();
        thread.join().unwrap();
    }

    // ---- WO-U46B: backend-mode enforcement in the circuit-open recovery arm ----
    // The finding: `open_physical` consults `sdk::try_circuit_recovery` (which
    // opens an SDK device) BEFORE it honors `kind`, so a `sdk` -> `hid` config
    // hot-reload taken while the process-global circuit is open lets an
    // explicit `hid` selection receive an SDK device.
    //
    // The circuit flag is a private process-global static (`sdk.rs:73`
    // `CIRCUIT_OPEN`, cleared only by `sdk.rs:344` `circuit_closed_on_health`),
    // and `sdk.rs` is out of scope, so no test in this module can force it.
    // The pre-red form mirrors the indicted arm statement-for-statement (the
    // WO-U46A pre-red idiom) with the resolver's outcome injected through the
    // LCore column, and the wiring is pinned on the production region itself
    // with the `include_str!` census (`sdk.rs:3731` precedent).
    const CIRCUIT_OPEN_DETAIL: &str =
        "physical backend circuit is open after an unreturning SDK owner";
    const CIRCUIT_OPEN_ROW: &str =
        "circuit-open: physical backend circuit is open after an unreturning SDK owner";
    const SDK_MODE_ABSENT_ROW: &str =
        "device-absent: Logitech SDK mode requires a running trusted LCore.exe";
    const HID_CONFLICT_ROW: &str =
        "owner-conflict: direct HID is refused while competing process LCore.exe is running";
    const VIRTUAL_NOT_PHYSICAL_ROW: &str = "probe-fail: virtual is not a physical backend";

    /// The device class (the `Some` half) plus the published disconnect row
    /// (empty when the cell opens a device) that `open_physical` yields for one
    /// matrix cell: the mode gate decides the circuit-open arm, the existing
    /// arbitration decides the circuit-closed arm.
    fn recovery_arm_cell(
        kind: BackendKind,
        circuit_open: bool,
        lcore_present: bool,
    ) -> (Option<&'static str>, String) {
        // WO-U46G item 3 (audit #6, supersession of the 46b model's shape): the
        // circuit guards SDK calls only, so the arm is entered by a mode that
        // MAY own an SDK device. `hid` (and the never-dispatched `virtual`)
        // take the NORMAL arbitration, where the existing LCore-ownership guard
        // (`hid.rs:359-370` post-open twin) keeps HID off a device LCore owns.
        if circuit_open && sdk_permits_ownership(kind) {
            // The gated arm: the 45a contract (`sdk.rs:362`
            // `try_circuit_recovery`) yields a fresh SDK device exactly when a
            // trusted, responsive LCore authorizes the one open per episode
            // window; every other pass publishes the byte-identical refusal row.
            if lcore_present {
                return (Some("sdk"), String::new());
            }
            return (
                None,
                published(&LossError::new(
                    LossCause::CircuitOpen,
                    CIRCUIT_OPEN_DETAIL.into(),
                ))
                .1,
            );
        }
        match select_physical_kind(kind, lcore_present) {
            Ok(BackendKind::Sdk) => (Some("sdk"), String::new()),
            Ok(BackendKind::Hid) => (Some("hid"), String::new()),
            Ok(other) => (None, format!("unexpected arbitration to {}", other.name())),
            Err(error) => (None, published(&error).1),
        }
    }

    /// One matrix row: label, mode, circuit state, LCore state, expected
    /// device class (None = no device) and the expected published row
    /// (empty = the cell opens a device, so it publishes no refusal).
    type MatrixRow = (
        &'static str,
        BackendKind,
        bool,
        bool,
        Option<&'static str>,
        &'static str,
    );

    #[test]
    fn the_backend_mode_gates_the_circuit_open_recovery_arm_for_every_mode_and_lcore_state() {
        // Table: mode x circuit x LCore -> expected device class + published
        // row. `virtual` never reaches `open_physical` in production (worker()
        // routes it to `virtual_worker`, pinned by
        // a_virtual_selection_never_reaches_the_physical_open_path), so its
        // open-circuit cells pin the gate's decision as defense in depth.
        let table: [MatrixRow; 16] = [
            (
                "auto/open/lcore",
                BackendKind::Auto,
                true,
                true,
                Some("sdk"),
                "",
            ),
            (
                "auto/open/no-lcore",
                BackendKind::Auto,
                true,
                false,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "auto/closed/lcore",
                BackendKind::Auto,
                false,
                true,
                Some("sdk"),
                "",
            ),
            (
                "auto/closed/no-lcore",
                BackendKind::Auto,
                false,
                false,
                Some("hid"),
                "",
            ),
            (
                "sdk/open/lcore",
                BackendKind::Sdk,
                true,
                true,
                Some("sdk"),
                "",
            ),
            (
                "sdk/open/no-lcore",
                BackendKind::Sdk,
                true,
                false,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "sdk/closed/lcore",
                BackendKind::Sdk,
                false,
                true,
                Some("sdk"),
                "",
            ),
            (
                "sdk/closed/no-lcore",
                BackendKind::Sdk,
                false,
                false,
                None,
                SDK_MODE_ABSENT_ROW,
            ),
            // WO-U46G item 3 supersession (audit #6): the two `hid` open-circuit
            // cells were `None, CIRCUIT_OPEN_ROW` - an SDK-scoped circuit flag
            // refused the backend the user explicitly selected. They now reach
            // the HID arbitration: the OwnerConflict row while LCore owns the
            // device, the normal HID open when no LCore is present.
            (
                "hid/open/lcore",
                BackendKind::Hid,
                true,
                true,
                None,
                HID_CONFLICT_ROW,
            ),
            (
                "hid/open/no-lcore",
                BackendKind::Hid,
                true,
                false,
                Some("hid"),
                "",
            ),
            (
                "hid/closed/lcore",
                BackendKind::Hid,
                false,
                true,
                None,
                HID_CONFLICT_ROW,
            ),
            (
                "hid/closed/no-lcore",
                BackendKind::Hid,
                false,
                false,
                Some("hid"),
                "",
            ),
            // WO-U46G item 3: the gate is mode-scoped, so the `virtual`
            // open-circuit cells match the circuit-closed ones ("virtual is not
            // a physical backend"). `worker()` routes `virtual` to
            // `virtual_worker` BEFORE the ladder, pinned by
            // a_virtual_selection_never_reaches_the_physical_open_path, so this
            // stays defense in depth, exactly as 46b labelled it.
            (
                "virtual/open/lcore",
                BackendKind::Virtual,
                true,
                true,
                None,
                VIRTUAL_NOT_PHYSICAL_ROW,
            ),
            (
                "virtual/open/no-lcore",
                BackendKind::Virtual,
                true,
                false,
                None,
                VIRTUAL_NOT_PHYSICAL_ROW,
            ),
            (
                "virtual/closed/lcore",
                BackendKind::Virtual,
                false,
                true,
                None,
                VIRTUAL_NOT_PHYSICAL_ROW,
            ),
            (
                "virtual/closed/no-lcore",
                BackendKind::Virtual,
                false,
                false,
                None,
                VIRTUAL_NOT_PHYSICAL_ROW,
            ),
        ];
        for (label, mode, circuit_open, lcore_present, want_device, want_row) in table {
            let (device, row) = recovery_arm_cell(mode, circuit_open, lcore_present);
            assert_eq!(device, want_device, "cell {label}: device class");
            assert_eq!(row, want_row, "cell {label}: published row");
        }
        for mode in [BackendKind::Hid, BackendKind::Virtual] {
            for lcore_present in [true, false] {
                assert_ne!(
                    recovery_arm_cell(mode, true, lcore_present).0,
                    Some("sdk"),
                    "a {mode:?} selection under an open circuit never receives the resolver's SDK device"
                );
            }
        }
    }

    #[test]
    fn the_circuit_open_recovery_arm_consults_the_backend_mode_before_the_sdk_resolver() {
        // FROZEN RED on the pre-fix tree (`u46b-prefix-reds.log`): `left: 0,
        // right: 1` - the arm had no mode gate, so the resolver's SDK device
        // was reachable for any configured mode.
        let source = include_str!("./mod.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("the test module marker");
        let arm = production
            .split("fn open_physical")
            .nth(1)
            .and_then(|rest| rest.split("fn select_physical_kind").next())
            .expect("the open_physical production body");
        // rustfmt wraps at the column boundary, so the census joins every
        // whitespace run onto one line (the sdk.rs:3748 normalization).
        let flat: String = production
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" .", ".");
        let arm_flat: String = arm
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" .", ".");
        assert_eq!(
            arm_flat.matches("sdk::circuit_open()").count(),
            1,
            "the arm is gated on the process-global circuit flag"
        );
        assert_eq!(
            arm_flat
                .matches("sdk::try_circuit_recovery(friendly_name)")
                .count(),
            1,
            "the arm consults the fresh-open resolver exactly once"
        );
        assert_eq!(
            arm_flat.matches("PhysicalDevice::Sdk(device)").count(),
            1,
            "the resolver's device is the only SDK construction in the arm"
        );
        assert_eq!(
            arm_flat.matches(CIRCUIT_OPEN_DETAIL).count(),
            1,
            "the circuit-open refusal row stays the arm's only published row"
        );
        assert_eq!(
            arm_flat.matches("sdk_permits_ownership(kind)").count(),
            1,
            "the arm asks the configured mode before the resolver may open an SDK device"
        );
        let gate = arm_flat
            .find("sdk_permits_ownership(kind)")
            .expect("the mode gate lives in the circuit-open arm");
        let resolver = arm_flat
            .find("sdk::try_circuit_recovery(friendly_name)")
            .expect("the resolver call lives in the circuit-open arm");
        assert!(
            gate < resolver,
            "the mode gate precedes the resolver, so a refused mode cannot reach an SDK device"
        );
        assert_eq!(
            flat.matches("fn sdk_permits_ownership(kind: BackendKind) -> bool")
                .count(),
            1,
            "the gate is one production function over the existing BackendKind"
        );
    }

    #[test]
    fn an_explicit_hid_ladder_under_an_open_circuit_publishes_the_circuit_open_row_never_a_connected_sdk(
    ) {
        // The same cell at the ladder: a `kind = hid` pass whose open yields the
        // gated circuit-open refusal publishes the 11/18 row byte-identically
        // and never a `Connected{Sdk}`. The open is the RC-6 injected seam, so
        // no hardware, LCore or SDK state is touched.
        static HID_CIRCUIT_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        fn open_hid_circuit_open(
            kind: BackendKind,
            _friendly_name: &str,
        ) -> Result<PhysicalDevice, LossError> {
            assert_eq!(
                kind,
                BackendKind::Hid,
                "the ladder passes the configured mode down the open path"
            );
            assert!(
                !sdk_permits_ownership(kind),
                "the gate refuses SDK ownership for an explicit hid selection"
            );
            HID_CIRCUIT_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
            Err(LossError::new(
                LossCause::CircuitOpen,
                CIRCUIT_OPEN_DETAIL.into(),
            ))
        }
        let base = Duration::from_millis(1);
        HID_CIRCUIT_ATTEMPTS.store(0, Ordering::SeqCst);
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
                base * 16,
                base,
                base,
                "normal".into(),
                false,
                String::new(),
                open_hid_circuit_open,
                probe_never,
            )
        });
        let budget = Instant::now() + Duration::from_secs(10);
        let left = || budget.saturating_duration_since(Instant::now());
        let mut rows = 0usize;
        while rows < 2 {
            assert!(
                !left().is_zero(),
                "the hid circuit-open rows stopped at {rows}"
            );
            if let Ok(Message::State(BackendState::Disconnected { cause, reason })) =
                rx.recv_timeout(left())
            {
                assert_eq!(cause, LossCause::CircuitOpen, "row {rows}");
                assert_eq!(
                    reason, CIRCUIT_OPEN_ROW,
                    "row {rows} is byte-identical to the 11/18 ladder rows"
                );
                rows += 1;
            }
        }
        // A bounded drain: an explicit hid selection publishes no Connected (it
        // never gets a device) and the circuit-open pass never feeds the Absent
        // terminal, so the recovery window stays live at the capped cadence.
        let drain = Instant::now() + Duration::from_millis(300);
        let mut connected = 0usize;
        let mut absent = 0usize;
        loop {
            let left = drain.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(Message::State(BackendState::Connected { .. })) => connected += 1,
                Ok(Message::State(BackendState::Absent { .. })) => absent += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert_eq!(
            connected, 0,
            "an explicit hid selection never receives the resolver's SDK device"
        );
        assert_eq!(absent, 0, "the circuit-open ladder never terminalizes");
        assert!(
            HID_CIRCUIT_ATTEMPTS.load(Ordering::SeqCst) >= 2,
            "the ladder keeps opening, so a later `sdk` reload can still recover"
        );
        commands.shutdown();
        thread.join().unwrap();
    }

    #[test]
    fn a_virtual_selection_never_reaches_the_physical_open_path() {
        // The `virtual` matrix cells pin the gate as defense in depth: `worker()`
        // routes `virtual` to `virtual_worker`, so only the three physical modes
        // ever reach `physical_worker` and its `open_physical` arm.
        let source = include_str!("./mod.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("the test module marker");
        let dispatch = production
            .split("fn worker(")
            .nth(1)
            .and_then(|rest| rest.split("fn transform_pixels").next())
            .expect("the worker dispatch body");
        let flat: String = dispatch
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" .", ".");
        assert_eq!(
            flat.matches("BackendKind::Virtual => virtual_worker")
                .count(),
            1,
            "virtual has its own worker, so it never opens a physical device"
        );
        assert_eq!(
            flat.matches("BackendKind::Auto | BackendKind::Sdk | BackendKind::Hid =>")
                .count(),
            1,
            "the physical arm is the only route to the recovery branch"
        );
    }

    // WO-U46G-RECOVERY-RECLAIM regression set (audit findings #6 and #18). The
    // 46b matrix stays the model of the arm; the arm ITSELF is pinned on the
    // production region by the `include_str!` census below (46b's accepted
    // methodology), because `open_physical` is the ladder's real open path and
    // the circuit flag has no production setter a test may fake.

    #[test]
    fn the_circuit_open_arm_is_mode_scoped_so_hid_reaches_the_normal_arbitration() {
        // Pre-fix RED (`u46g-prefix-reds.log`): the needle is ABSENT -
        // `open_physical` entered the circuit branch on the flag alone
        // (`src/backends/mod.rs:718`), so EVERY mode fell into the refusal at
        // `src/backends/mod.rs:736` and the normal arbitration at
        // `src/backends/mod.rs:741` was unreachable while the circuit stayed
        // open. With the audit #1 wedge that meant an explicit `hid` selection
        // was refused by an SDK-scoped flag: panel blank to process restart.
        let source = include_str!("./mod.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("the test module marker");
        let arm = production
            .split("fn open_physical")
            .nth(1)
            .and_then(|rest| rest.split("fn select_physical_kind").next())
            .expect("the open_physical production body");
        let arm_flat: String = arm
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" .", ".");
        // `split`-count, not `matches`: the needle carries `(` and `)`, which
        // `str::matches` would read as regex groups (the 46b needles it shares
        // this census with are plain text, so they are unaffected).
        assert_eq!(
            arm_flat
                .split("sdk::circuit_open() && sdk_permits_ownership(kind)")
                .count()
                - 1,
            1,
            "the circuit branch is entered only by a mode that may own an SDK device"
        );
        assert_eq!(
            arm_flat.matches(CIRCUIT_OPEN_DETAIL).count(),
            1,
            "the SDK/auto refusal row is byte-identical and stays the arm's only published row"
        );
        // The arbitration a `hid` selection reaches, on the production rule: the
        // LCore-ownership guard answers OwnerConflict while LCore owns the
        // device (the 46b core property, preserved), and a normal HID open when
        // no LCore is present.
        assert_eq!(
            select_physical_kind(BackendKind::Hid, true)
                .err()
                .unwrap()
                .cause,
            LossCause::OwnerConflict,
            "HID stays off a device LCore owns"
        );
        assert_eq!(
            select_physical_kind(BackendKind::Hid, false).ok(),
            Some(BackendKind::Hid),
            "an explicit hid selection with no LCore reaches its own open"
        );
        // The cells the supersession changes, and the SDK/auto cells it must
        // not touch.
        let table: [(&str, BackendKind, bool, Option<&'static str>, &str); 5] = [
            (
                "hid/open/lcore",
                BackendKind::Hid,
                true,
                None,
                HID_CONFLICT_ROW,
            ),
            (
                "hid/open/no-lcore",
                BackendKind::Hid,
                false,
                Some("hid"),
                "",
            ),
            ("auto/open/lcore", BackendKind::Auto, true, Some("sdk"), ""),
            (
                "auto/open/no-lcore",
                BackendKind::Auto,
                false,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "sdk/open/no-lcore",
                BackendKind::Sdk,
                false,
                None,
                CIRCUIT_OPEN_ROW,
            ),
        ];
        for (label, mode, lcore_present, want_device, want_row) in table {
            let (device, row) = recovery_arm_cell(mode, true, lcore_present);
            assert_eq!(device, want_device, "cell {label}: device class");
            assert_eq!(row, want_row, "cell {label}: published row");
        }
    }

    /// The arm under `RECOVERY_BLOCKED` (the trust class, where 45a forbids
    /// every fresh open): the resolver returns None at
    /// `src/backends/sdk.rs:365`, so a permitting mode publishes the refusal
    /// row and a non-permitting mode never sees the block state at all.
    fn blocked_arm_cell(kind: BackendKind, lcore_present: bool) -> (Option<&'static str>, String) {
        if sdk_permits_ownership(kind) {
            return (
                None,
                published(&LossError::new(
                    LossCause::CircuitOpen,
                    CIRCUIT_OPEN_DETAIL.into(),
                ))
                .1,
            );
        }
        match select_physical_kind(kind, lcore_present) {
            Ok(BackendKind::Sdk) => (Some("sdk"), String::new()),
            Ok(BackendKind::Hid) => (Some("hid"), String::new()),
            Ok(other) => (None, format!("unexpected arbitration to {}", other.name())),
            Err(error) => (None, published(&error).1),
        }
    }

    #[test]
    fn the_recovery_block_is_scoped_to_the_modes_the_circuit_guards() {
        // Coverage row: `hid/blocked` is unaffected by RECOVERY_BLOCKED, and
        // `auto`/`sdk` under a block keep the refusal row byte-identical.
        let table: [(&str, BackendKind, bool, Option<&'static str>, &str); 6] = [
            (
                "auto/blocked/lcore",
                BackendKind::Auto,
                true,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "auto/blocked/no-lcore",
                BackendKind::Auto,
                false,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "sdk/blocked/lcore",
                BackendKind::Sdk,
                true,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "sdk/blocked/no-lcore",
                BackendKind::Sdk,
                false,
                None,
                CIRCUIT_OPEN_ROW,
            ),
            (
                "hid/blocked/lcore",
                BackendKind::Hid,
                true,
                None,
                HID_CONFLICT_ROW,
            ),
            (
                "hid/blocked/no-lcore",
                BackendKind::Hid,
                false,
                Some("hid"),
                "",
            ),
        ];
        for (label, mode, lcore_present, want_device, want_row) in table {
            let (device, row) = blocked_arm_cell(mode, lcore_present);
            assert_eq!(device, want_device, "cell {label}: device class");
            assert_eq!(row, want_row, "cell {label}: published row");
        }
    }

    #[test]
    fn a_backend_thread_spawn_failure_is_a_named_loss_never_a_panic() {
        // WO-U46G item 5 (audit #18). Pre-fix form, disclosed: the production
        // site was `.expect("spawn backend thread")`
        // (`src/backends/mod.rs:414`), so an OS thread-creation failure aborted
        // the process. Reproducing that behaviorally needs real OS thread
        // exhaustion, which this WO bans, so the PRE-FIX RED is this census row
        // (`u46g-prefix-reds.log`); the behavior is pinned post-fix by
        // a_backend_spawn_failure_yields_the_named_loss_and_the_disconnected_row.
        let source = include_str!("./mod.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("the test module marker");
        let spawn = production
            .split("pub fn spawn(")
            .nth(1)
            .and_then(|rest| rest.split("pub fn submit(").next())
            .expect("the Backend::spawn production body");
        assert_eq!(
            spawn.matches(".expect(\"spawn backend thread\")").count(),
            0,
            "the backend worker spawn has no panic site"
        );
        assert_eq!(
            spawn.matches("spawn_failure_loss").count(),
            2,
            "the real io::Error and the injected failure route through the one named-loss site"
        );
    }

    #[test]
    fn a_backend_spawn_failure_yields_the_named_loss_and_the_disconnected_row() {
        // WO-U46G item 5 behavior, on the thread-local `cfg(test)` injection (no
        // real OS exhaustion): `Backend::spawn` fails as a named loss, the state
        // app.rs publishes for it is the established `Disconnected` row built by
        // the worker's own construction point, and the injection is one token,
        // so the next real spawn succeeds and joins cleanly.
        let cfg = Config {
            logitech_button_poll: Duration::from_millis(1),
            ..Default::default()
        };
        inject_backend_spawn_failures(1);
        let error = match Backend::spawn(BackendKind::Virtual, &cfg, app_wake()) {
            Ok(_) => panic!("the injected spawn failure must yield the named loss"),
            Err(error) => error,
        };
        assert_eq!(error.cause, LossCause::ProbeFail, "{error}");
        assert!(
            error.detail.contains("spawn backend worker thread"),
            "{error}"
        );
        assert_eq!(
            disconnect_state(&error),
            BackendState::Disconnected {
                cause: LossCause::ProbeFail,
                reason: error.to_string(),
            },
            "the state app.rs publishes is the established loss-cause row, built by the worker's own construction point"
        );
        let backend = Backend::spawn(BackendKind::Virtual, &cfg, app_wake())
            .expect("the injection is one token, so the next spawn succeeds");
        backend.shutdown();
    }
}
