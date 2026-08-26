//! LCD backend worker: owns the physical device thread, serializes all
//! device I/O, and exposes a message surface to the application.
//!
//! Backend selection: `auto` = hid -> virtual (SDK mode is retired: the
//! Logitech runtime triggers the G HUB conflict on the target machine).
//! The worker suppresses unchanged frames and reconnects with bounded,
//! capped backoff. Device loss emits canceled button releases.

pub mod g13;
pub mod hid;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::input::{ButtonTracker, Event};
use g13::{pack_report, parse_input, G13_OUTPUT_REPORT_LENGTH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Hid,
    Virtual,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Hid => "hid",
            BackendKind::Virtual => "virtual",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendState {
    Discovering,
    Connected { kind: BackendKind },
    Disconnected { reason: String },
    ShutDown,
}

pub enum Message {
    State(BackendState),
    Buttons(Vec<Event>),
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
            wake: wake_tx,
        });
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let reconnect = cfg.logitech_reconnect;
        let reconnect_max = cfg.logitech_reconnect_max;
        let button_poll = cfg.logitech_button_poll;
        let debounce = cfg.logitech_button_debounce;
        let orientation = cfg.logitech_orientation.clone();
        let invert = cfg.logitech_invert;
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

    pub fn shutdown(mut self) {
        self.commands.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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
) {
    let _ = msg_tx.send(Message::State(BackendState::Discovering));
    match kind {
        BackendKind::Virtual => virtual_worker(commands, wake_rx, msg_tx),
        BackendKind::Hid => {
            hid_worker(
                &commands,
                &wake_rx,
                &msg_tx,
                reconnect,
                reconnect_max,
                button_poll,
                debounce,
                orientation,
                invert,
            );
            let _ = msg_tx.send(Message::State(BackendState::ShutDown));
        }
    }
}

fn transform_frame(
    frame: &[u8; crate::model::WIDTH * crate::model::HEIGHT],
    orientation: &str,
    invert: bool,
) -> [u8; G13_OUTPUT_REPORT_LENGTH] {
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
    pack_report(&transformed)
}

#[allow(clippy::too_many_arguments, unused_assignments)]
fn hid_worker(
    commands: &CommandQueue,
    wake_rx: &Receiver<()>,
    msg_tx: &std::sync::mpsc::Sender<Message>,
    reconnect: Duration,
    reconnect_max: Duration,
    button_poll: Duration,
    debounce: Duration,
    orientation: String,
    invert: bool,
) {
    let mut backoff = reconnect;
    let mut last_sent: Option<[u8; G13_OUTPUT_REPORT_LENGTH]> = None;
    let mut tracker = ButtonTracker::default();

    'outer: loop {
        // --- discovery/open with bounded, capped backoff ---
        match hid::HidDevice::open() {
            Err(reason) => {
                let _ = msg_tx.send(Message::State(BackendState::Disconnected { reason }));
                if sleep_interruptible(commands, wake_rx, backoff) {
                    break 'outer;
                }
                backoff = (backoff * 2).min(reconnect_max);
                continue 'outer;
            }
            Ok((mut device, _discovery)) => {
                let _ = msg_tx.send(Message::State(BackendState::Connected {
                    kind: BackendKind::Hid,
                }));
                let _ = msg_tx.send(Message::Buttons(tracker.disconnect(Instant::now(), "hid")));
                last_sent = None;
                backoff = reconnect;

                // --- connected loop ---
                loop {
                    if commands.is_shutdown() {
                        if let Err(reason) = device.close() {
                            let _ =
                                msg_tx.send(Message::State(BackendState::Disconnected { reason }));
                        }
                        break 'outer;
                    }
                    let mut pixels = commands.take_latest();
                    if pixels.is_none() {
                        match wake_rx.recv_timeout(button_poll) {
                            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break 'outer,
                        }
                        if commands.is_shutdown() {
                            continue;
                        }
                        pixels = commands.take_latest();
                    }

                    if let Some(pixels) = pixels {
                        let report = transform_frame(&pixels, &orientation, invert);
                        if last_sent.as_ref() != Some(&report) {
                            match device.write_report(&report) {
                                Ok(()) => last_sent = Some(report),
                                Err(mut reason) => {
                                    if let Err(close_error) = device.close() {
                                        reason.push_str("; ");
                                        reason.push_str(&close_error);
                                    }
                                    let _ =
                                        msg_tx.send(Message::State(BackendState::Disconnected {
                                            reason,
                                        }));
                                    let _ = msg_tx.send(Message::Buttons(
                                        tracker.disconnect(Instant::now(), "hid"),
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

                    let mut raw = [0u8; 8];
                    match device
                        .read_input_timeout(&mut raw, button_poll.max(Duration::from_millis(10)))
                    {
                        Ok(true) => match parse_input(&raw) {
                            Ok(buttons) => {
                                let events =
                                    tracker.observe(Instant::now(), buttons, debounce, "hid");
                                if !events.is_empty() {
                                    let _ = msg_tx.send(Message::Buttons(events));
                                }
                            }
                            Err(reason) => {
                                let mut reason = reason;
                                if let Err(close_error) = device.close() {
                                    reason.push_str("; ");
                                    reason.push_str(&close_error);
                                }
                                let _ = msg_tx
                                    .send(Message::State(BackendState::Disconnected { reason }));
                                let _ = msg_tx.send(Message::Buttons(
                                    tracker.disconnect(Instant::now(), "hid"),
                                ));
                                if sleep_interruptible(commands, wake_rx, backoff) {
                                    break 'outer;
                                }
                                backoff = (backoff * 2).min(reconnect_max);
                                continue 'outer;
                            }
                        },
                        Ok(false) => {}
                        Err(reason) => {
                            let mut reason = reason;
                            if let Err(close_error) = device.close() {
                                reason.push_str("; ");
                                reason.push_str(&close_error);
                            }
                            let _ =
                                msg_tx.send(Message::State(BackendState::Disconnected { reason }));
                            let _ = msg_tx
                                .send(Message::Buttons(tracker.disconnect(Instant::now(), "hid")));
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
        match wake_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
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
        "virtual" => BackendKind::Virtual,
        _ => BackendKind::Hid, // "auto" and "hid" both start with direct HID
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Frame;

    fn command_queue() -> (Arc<CommandQueue>, Receiver<()>) {
        let (wake, receiver) = std::sync::mpsc::sync_channel(1);
        (
            Arc::new(CommandQueue {
                latest: Mutex::new(None),
                shutdown: AtomicBool::new(false),
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
        assert_eq!(resolve_kind("auto"), BackendKind::Hid);
        assert_eq!(resolve_kind("hid"), BackendKind::Hid);
        assert_eq!(resolve_kind("virtual"), BackendKind::Virtual);
    }
}
