//! Optional bounded ICMP/TCP quality probe.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{GetLastError, ERROR_TIMEOUT};
use windows::Win32::NetworkManagement::IpHelper::{
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY,
};

use crate::config::Config;
use crate::model::Metric;

const TICK: Duration = Duration::from_millis(50);
const IP_SUCCESS: u32 = 0;
const IP_REQ_TIMED_OUT: u32 = 11010;

#[derive(Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    fn new(timeout: Duration) -> Self {
        Self(
            Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now),
        )
    }

    fn remaining(self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    fn remaining_at(self, now: Instant) -> Option<Duration> {
        self.0
            .checked_duration_since(now)
            .filter(|left| !left.is_zero())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub generation: u64,
    pub ping: Metric,
    pub jitter: Metric,
    pub loss: Metric,
    pub available: bool,
    pub error: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Policy {
    enabled: bool,
    safe_mode: bool,
    method: String,
    target: String,
    interval: Duration,
    timeout: Duration,
    window: usize,
}

impl From<&Config> for Policy {
    fn from(cfg: &Config) -> Self {
        Self {
            enabled: cfg.network_probe_enabled,
            safe_mode: cfg.safe_mode,
            method: cfg.network_probe_method.clone(),
            target: cfg.network_probe_target.clone(),
            interval: cfg.network_probe_interval,
            timeout: cfg.network_probe_timeout,
            window: cfg.network_probe_window.max(1) as usize,
        }
    }
}

impl Policy {
    fn active(&self) -> bool {
        self.enabled && !self.safe_mode
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Success(f64),
    Loss,
    Error(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Tcp,
    Icmp(Ipv4Addr),
    Auto(Ipv4Addr),
    Invalid,
}

fn route(method: &str, ip: IpAddr) -> Route {
    match (method, ip) {
        ("tcp", _) | ("auto", IpAddr::V6(_)) => Route::Tcp,
        ("auto", IpAddr::V4(ipv4)) => Route::Auto(ipv4),
        ("icmp", IpAddr::V4(ipv4)) => Route::Icmp(ipv4),
        _ => Route::Invalid,
    }
}

#[derive(Clone, Copy, Debug)]
struct Trial {
    latency: f64,
    ok: bool,
}

#[derive(Default)]
struct State {
    trials: VecDeque<Trial>,
    update: Update,
}

impl State {
    fn clear(&mut self) -> Update {
        self.trials.clear();
        self.update = Update::default();
        self.update.clone()
    }

    fn apply(&mut self, outcome: Outcome, window: usize, now: SystemTime) -> Update {
        match outcome {
            Outcome::Success(latency) => {
                self.trials.push_back(Trial { latency, ok: true });
                self.update.ping = Metric::valid(latency, now);
                self.update.available = true;
                self.update.error = None;
            }
            Outcome::Loss => {
                self.trials.push_back(Trial {
                    latency: 0.0,
                    ok: false,
                });
                self.update.ping = Metric::default();
                self.update.available = true;
                self.update.error = None;
            }
            Outcome::Error(error) => {
                self.update.available = false;
                self.update.error = Some(error);
                self.update.ping.stale |= self.update.ping.valid;
                self.update.jitter.stale |= self.update.jitter.valid;
                self.update.loss.stale |= self.update.loss.valid;
                return self.update.clone();
            }
        }
        while self.trials.len() > window.max(1) {
            self.trials.pop_front();
        }
        let successes: Vec<f64> = self
            .trials
            .iter()
            .filter(|trial| trial.ok)
            .map(|trial| trial.latency)
            .collect();
        self.update.loss = Metric::valid(
            self.trials.iter().filter(|trial| !trial.ok).count() as f64 * 100.0
                / self.trials.len() as f64,
            now,
        );
        self.update.jitter = if successes.len() > 1 {
            Metric::valid(crate::history::jitter(&successes), now)
        } else {
            Metric::default()
        };
        self.update.clone()
    }
}

pub fn spawn(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-network-quality".into())
        .spawn(move || worker(config, generation, shutdown, tx, probe))
        .expect("network quality thread");
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

fn worker(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<Update>,
    probe: fn(&Policy, &RwLock<Config>, &AtomicBool) -> Outcome,
) {
    let mut state = State::default();
    let mut policy = None;
    let mut last_attempt = None;
    let mut disabled_published = false;
    let mut current_generation = 0;
    while !shutdown.load(Ordering::Relaxed) {
        let (request_generation, cfg) = current_request(&config, &generation);
        if request_generation != current_generation {
            current_generation = request_generation;
            state.clear();
            policy = None;
            last_attempt = None;
            disabled_published = false;
            if tx
                .send(Update {
                    generation: request_generation,
                    ..Default::default()
                })
                .is_err()
            {
                return;
            }
        }
        let current = Policy::from(&cfg);
        if policy.as_ref().is_some_and(|old| old != &current) {
            state.clear();
            last_attempt = None;
            let mut update = state.update.clone();
            update.generation = request_generation;
            if tx.send(update).is_err() {
                return;
            }
        }
        policy = Some(current.clone());
        if !current.active() {
            if !disabled_published {
                let mut update = state.clear();
                update.generation = request_generation;
                if tx.send(update).is_err() {
                    return;
                }
            }
            disabled_published = true;
            std::thread::sleep(TICK);
            continue;
        }
        disabled_published = false;
        let now = Instant::now();
        if last_attempt.is_some_and(|last: Instant| now.duration_since(last) < current.interval) {
            std::thread::sleep(TICK);
            continue;
        }
        last_attempt = Some(now);
        let outcome = probe(&current, &config, &shutdown);
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let (latest_generation, latest_cfg) = current_request(&config, &generation);
        let latest = Policy::from(&latest_cfg);
        if latest_generation != request_generation || latest != current || !latest.active() {
            current_generation = latest_generation;
            state.clear();
            policy = Some(latest);
            last_attempt = None;
            let mut update = state.update.clone();
            update.generation = latest_generation;
            if tx.send(update).is_err() {
                return;
            }
            continue;
        }
        let mut update = state.apply(outcome, current.window, SystemTime::now());
        update.generation = request_generation;
        if tx.send(update).is_err() {
            return;
        }
    }
}

fn probe(policy: &Policy, config: &RwLock<Config>, shutdown: &AtomicBool) -> Outcome {
    let Ok((ip, port)) = crate::config::parse_network_target(&policy.target) else {
        return Outcome::Error("network target is invalid");
    };
    let deadline = Deadline::new(policy.timeout);
    let tcp_address = SocketAddr::new(ip, port.unwrap_or(443));
    let mut current = || {
        !shutdown.load(Ordering::Acquire)
            && Policy::from(&*config.read().unwrap_or_else(|e| e.into_inner())) == *policy
            && policy.active()
    };
    match route(&policy.method, ip) {
        Route::Tcp => tcp_probe(tcp_address, deadline, &mut current),
        Route::Auto(ipv4) => auto_probe(
            ipv4,
            tcp_address,
            deadline,
            &mut current,
            icmp_probe,
            tcp_probe,
        ),
        Route::Icmp(ipv4) => {
            if !current() {
                return Outcome::Error("network probe canceled");
            }
            deadline
                .remaining()
                .map(|remaining| icmp_probe(ipv4, remaining))
                .unwrap_or(Outcome::Loss)
        }
        Route::Invalid => Outcome::Error("network probe method or target is invalid"),
    }
}

fn auto_probe<C, I, T>(
    ipv4: Ipv4Addr,
    tcp_address: SocketAddr,
    deadline: Deadline,
    current: &mut C,
    icmp: I,
    tcp: T,
) -> Outcome
where
    C: FnMut() -> bool,
    I: FnOnce(Ipv4Addr, Duration) -> Outcome,
    T: FnOnce(SocketAddr, Deadline, &mut C) -> Outcome,
{
    if !current() {
        return Outcome::Error("network probe canceled");
    }
    let Some(remaining) = deadline.remaining() else {
        return Outcome::Loss;
    };
    let outcome = icmp(ipv4, remaining);
    if matches!(outcome, Outcome::Success(_)) {
        return outcome;
    }
    if !current() {
        return Outcome::Error("network probe canceled");
    }
    tcp(tcp_address, deadline, current)
}

fn tcp_probe<C>(address: SocketAddr, deadline: Deadline, current: &mut C) -> Outcome
where
    C: FnMut() -> bool,
{
    if !current() {
        return Outcome::Error("network probe canceled");
    }
    let Some(timeout) = deadline.remaining() else {
        return Outcome::Loss;
    };
    if !current() {
        return Outcome::Error("network probe canceled");
    }
    let started = Instant::now();
    match TcpStream::connect_timeout(&address, timeout) {
        Ok(stream) => {
            drop(stream);
            Outcome::Success(started.elapsed().as_secs_f64() * 1000.0)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::HostUnreachable
                    | std::io::ErrorKind::NetworkUnreachable
            ) =>
        {
            Outcome::Loss
        }
        Err(_) => Outcome::Error("TCP probe failed"),
    }
}

fn icmp_probe(address: Ipv4Addr, timeout: Duration) -> Outcome {
    if timeout < Duration::from_millis(1) {
        return Outcome::Loss;
    }
    let Ok(handle) = (unsafe { IcmpCreateFile() }) else {
        return Outcome::Error("ICMP provider unavailable");
    };
    let payload = b"LCDSirPlus";
    let reply_bytes = std::mem::size_of::<ICMP_ECHO_REPLY>() + payload.len() + 8;
    let mut reply = vec![0usize; reply_bytes.div_ceil(std::mem::size_of::<usize>())];
    let count = unsafe {
        IcmpSendEcho(
            handle,
            u32::from_le_bytes(address.octets()),
            payload.as_ptr() as *const _,
            payload.len() as u16,
            None,
            reply.as_mut_ptr() as *mut _,
            reply_bytes as u32,
            timeout.as_millis().clamp(1, u32::MAX as u128) as u32,
        )
    };
    let error = unsafe { GetLastError() };
    unsafe {
        let _ = IcmpCloseHandle(handle);
    }
    if count == 0 {
        return if error == ERROR_TIMEOUT || error.0 == IP_REQ_TIMED_OUT {
            Outcome::Loss
        } else {
            Outcome::Error("ICMP probe failed")
        };
    }
    let echo = unsafe { &*(reply.as_ptr() as *const ICMP_ECHO_REPLY) };
    if echo.Status == IP_SUCCESS {
        Outcome::Success(echo.RoundTripTime as f64)
    } else {
        Outcome::Loss
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn history_is_bounded_and_separates_loss_from_provider_errors() {
        let mut state = State::default();
        let at = SystemTime::UNIX_EPOCH;
        state.apply(Outcome::Success(10.0), 3, at);
        state.apply(Outcome::Success(16.0), 3, at);
        let update = state.apply(Outcome::Loss, 3, at);
        assert!((update.loss.value - 33.333333).abs() < 0.001);
        assert_eq!(update.jitter.value, 6.0);
        let update = state.apply(Outcome::Error("api"), 3, at);
        assert_eq!(state.trials.len(), 3);
        assert!(!update.available && update.loss.stale);
        state.apply(Outcome::Success(20.0), 3, at);
        assert_eq!(state.trials.len(), 3);
    }

    #[test]
    fn one_absolute_deadline_supplies_the_remaining_budget() {
        let start = Instant::now();
        let deadline = Deadline(start + Duration::from_millis(100));
        assert_eq!(
            deadline.remaining_at(start),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            deadline.remaining_at(start + Duration::from_millis(40)),
            Some(Duration::from_millis(60))
        );
        assert_eq!(
            deadline.remaining_at(start + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            deadline.remaining_at(start + Duration::from_millis(101)),
            None
        );
    }

    #[test]
    fn ipv6_auto_routes_directly_to_tcp_and_icmp_is_invalid() {
        let ipv6 = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);
        assert_eq!(route("auto", ipv6), Route::Tcp);
        assert_eq!(route("tcp", ipv6), Route::Tcp);
        assert_eq!(route("icmp", ipv6), Route::Invalid);
    }

    #[test]
    fn shutdown_before_auto_fallback_starts_no_tcp_io() {
        use std::cell::Cell;

        let checks = Cell::new(0);
        let tcp_calls = Cell::new(0);
        let mut current = || {
            let check = checks.get();
            checks.set(check + 1);
            check == 0
        };
        let outcome = auto_probe(
            Ipv4Addr::LOCALHOST,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
            Deadline::new(Duration::from_secs(1)),
            &mut current,
            |_, _| Outcome::Loss,
            |_, _, _| {
                tcp_calls.set(tcp_calls.get() + 1);
                Outcome::Success(1.0)
            },
        );
        assert_eq!(outcome, Outcome::Error("network probe canceled"));
        assert_eq!(tcp_calls.get(), 0);
    }

    #[test]
    fn disabled_state_clears_history_and_metrics() {
        let mut state = State::default();
        state.apply(Outcome::Success(1.0), 5, SystemTime::UNIX_EPOCH);
        let update = state.clear();
        assert!(state.trials.is_empty());
        assert!(!update.ping.valid && !update.available);
    }

    #[test]
    fn disabled_and_safe_mode_policies_cannot_probe() {
        let mut cfg = Config::default();
        assert!(!Policy::from(&cfg).active());
        cfg.network_probe_enabled = true;
        cfg.safe_mode = true;
        assert!(!Policy::from(&cfg).active());
        cfg.safe_mode = false;
        assert!(Policy::from(&cfg).active());
    }

    #[test]
    fn loopback_tcp_probe_is_bounded() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let owner = std::thread::spawn(move || listener.accept().unwrap());
        let mut current = || true;
        assert!(matches!(
            tcp_probe(address, Deadline::new(Duration::from_secs(1)), &mut current),
            Outcome::Success(_)
        ));
        drop(owner.join().unwrap());
    }

    #[test]
    fn canceled_tcp_probe_starts_no_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut current = || false;
        assert_eq!(
            tcp_probe(address, Deadline::new(Duration::from_secs(1)), &mut current),
            Outcome::Error("network probe canceled")
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    /// EC-8 (P1 §2 EC-8, acceptance 2): a config generation bump that lands while the worker
    /// is mid-cadence must clear the probe baseline and republish for the new generation —
    /// the same rule as the template `netif::failure_clears_baseline_and_recovery_requires_a_new_interval`.
    /// Loopback only: the target is a socket this test binds and never accepts on, so the probe
    /// is a local `connect` with no external traffic and no privileged path.
    #[test]
    fn network_config_change_midflight_clears_baseline_and_republishes_the_new_generation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(RwLock::new(Config {
            network_probe_enabled: true,
            network_probe_method: "tcp".into(),
            network_probe_target: format!("127.0.0.1:{port}"),
            network_probe_interval: Duration::from_secs(600),
            network_probe_timeout: Duration::from_millis(500),
            network_probe_window: 5,
            ..Config::default()
        }));
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let thread = {
            let config = Arc::clone(&config);
            let generation = Arc::clone(&generation);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || worker(config, generation, shutdown, tx, probe))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut baseline = None;
        while baseline.is_none() && Instant::now() < deadline {
            if let Ok(update) = rx.recv_timeout(Duration::from_millis(100)) {
                if update.generation == 1 && update.ping.valid {
                    baseline = Some(update);
                }
            }
        }
        assert!(
            baseline.is_some(),
            "the loopback probe must publish a generation-1 baseline first"
        );
        generation.fetch_add(1, Ordering::SeqCst);
        let mut cleared = None;
        while cleared.is_none() && Instant::now() < deadline {
            if let Ok(update) = rx.recv_timeout(Duration::from_millis(100)) {
                if update.generation == 2 {
                    cleared = Some(update);
                }
            }
        }
        let cleared = cleared.expect(
            "a mid-flight generation bump republishes a cleared update for the new generation",
        );
        assert!(
            !cleared.ping.valid && !cleared.jitter.valid && !cleared.loss.valid,
            "the prior baseline must be cleared, never carried onto the new generation"
        );
        assert!(!cleared.available && cleared.error.is_none());
        shutdown.store(true, Ordering::Relaxed);
        thread.join().unwrap();
    }

    // EC-8 F-3 gap-fill (WO-A1-08-AUDIT probe F: deleting the post-probe re-check left the
    // full suite green). These three tests drive the real worker loop with a SYNTHETIC slow
    // probe — a channel handshake in which the probe signals its entry, then blocks until the
    // test releases it — so a config change lands strictly INSIDE the probe call and the
    // post-probe re-check (current L264-277) sees it with zero wall-clock racing. One test per
    // arm of the L266 condition: generation bump, policy-content change, policy disabled.
    // No sleeps, no sockets, no DNS, no ICMP: the synthetic probe never touches the network,
    // and the production `probe()` is not used by these tests.

    struct SlowProbe {
        enter: Sender<()>,
        release: Mutex<Receiver<()>>,
    }

    static SLOW_GENERATION: OnceLock<SlowProbe> = OnceLock::new();
    static SLOW_CONTENT: OnceLock<SlowProbe> = OnceLock::new();
    static SLOW_DISABLED: OnceLock<SlowProbe> = OnceLock::new();

    const MIDPROBE_WAIT: Duration = Duration::from_secs(2);

    fn install_slow_probe(slot: &'static OnceLock<SlowProbe>) -> (Receiver<()>, Sender<()>) {
        let (enter_tx, enter_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        assert!(
            slot.set(SlowProbe {
                enter: enter_tx,
                release: Mutex::new(release_rx),
            })
            .is_ok(),
            "each slow-probe slot is installed exactly once"
        );
        (enter_rx, release_tx)
    }

    fn block_in_probe(slot: &OnceLock<SlowProbe>) -> Outcome {
        let probe = slot
            .get()
            .expect("the test installs the slow-probe control before spawning the worker");
        probe
            .enter
            .send(())
            .expect("the slow probe signals its entry to the test");
        probe
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("the test releases the slow probe (loud 5 s timeout, never a hang)");
        Outcome::Success(12.0)
    }

    fn slow_probe_generation(
        _policy: &Policy,
        _config: &RwLock<Config>,
        _shutdown: &AtomicBool,
    ) -> Outcome {
        block_in_probe(&SLOW_GENERATION)
    }

    fn slow_probe_content(
        _policy: &Policy,
        _config: &RwLock<Config>,
        _shutdown: &AtomicBool,
    ) -> Outcome {
        block_in_probe(&SLOW_CONTENT)
    }

    fn slow_probe_disabled(
        _policy: &Policy,
        _config: &RwLock<Config>,
        _shutdown: &AtomicBool,
    ) -> Outcome {
        block_in_probe(&SLOW_DISABLED)
    }

    fn midprobe_config(target: &str) -> Config {
        Config {
            network_probe_enabled: true,
            network_probe_method: "tcp".into(),
            network_probe_target: target.into(),
            network_probe_interval: Duration::from_secs(600),
            network_probe_timeout: Duration::from_millis(500),
            network_probe_window: 5,
            ..Config::default()
        }
    }

    struct MidProbe {
        rx: Receiver<Update>,
        shutdown: Arc<AtomicBool>,
        thread: std::thread::JoinHandle<()>,
    }

    fn spawn_midprobe_worker(
        config: Arc<RwLock<Config>>,
        generation: Arc<AtomicU64>,
        probe: fn(&Policy, &RwLock<Config>, &AtomicBool) -> Outcome,
    ) -> MidProbe {
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let thread = {
            let config = Arc::clone(&config);
            let generation = Arc::clone(&generation);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || worker(config, generation, shutdown, tx, probe))
        };
        MidProbe {
            rx,
            shutdown,
            thread,
        }
    }

    fn cleared_update(update: &Update) -> bool {
        !update.ping.valid
            && !update.jitter.valid
            && !update.loss.valid
            && !update.available
            && update.error.is_none()
    }

    /// L266 arm 1 (generation moved mid-probe): the worker drops the probe outcome, never
    /// publishes it on the superseded request_generation 1, republishes a cleared update for
    /// generation 2, and resets cadence state (immediate re-probe under a 600 s interval).
    #[test]
    fn generation_bump_midprobe_discards_the_stale_outcome_and_republishes_the_new_generation() {
        let (enter_rx, release_tx) = install_slow_probe(&SLOW_GENERATION);
        let generation = Arc::new(AtomicU64::new(1));
        let run = spawn_midprobe_worker(
            Arc::new(RwLock::new(midprobe_config("synthetic-fixed"))),
            Arc::clone(&generation),
            slow_probe_generation,
        );
        let startup = run
            .rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the loop head publishes the cleared generation-1 update");
        assert_eq!(startup.generation, 1);
        enter_rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the worker entered the probe");
        generation.store(2, Ordering::SeqCst);
        release_tx.send(()).expect("release #1");
        let mut superseded = vec![startup];
        let republished = loop {
            let update = run
                .rx
                .recv_timeout(MIDPROBE_WAIT)
                .expect("the post-probe re-check republishes for generation 2");
            if update.generation == 2 {
                break update;
            }
            superseded.push(update);
        };
        assert!(
            superseded
                .iter()
                .all(|update| !(update.generation == 1 && update.ping.valid)),
            "the superseded probe outcome must never be published on request_generation 1: {superseded:?}"
        );
        assert!(
            cleared_update(&republished),
            "the generation-2 republish is a cleared baseline: {republished:?}"
        );
        enter_rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("last_attempt reset: the worker re-probes immediately for generation 2 under a 600 s cadence");
        run.shutdown.store(true, Ordering::Release);
        release_tx.send(()).expect("release #2");
        drop(release_tx);
        run.thread
            .join()
            .expect("the worker joins after release #2");
    }

    /// L266 arm 2 (policy content moved mid-probe, same generation): the outcome is dropped,
    /// the first post-change update is the cleared republish on the current generation, and
    /// the re-probe under the new policy starts from a fresh baseline (loss of exactly 0 %,
    /// jitter not yet computable) with cadence reset.
    #[test]
    fn policy_content_change_midprobe_discards_the_stale_outcome_and_republishes_a_cleared_baseline(
    ) {
        let (enter_rx, release_tx) = install_slow_probe(&SLOW_CONTENT);
        let config = Arc::new(RwLock::new(midprobe_config("synthetic-one")));
        let generation = Arc::new(AtomicU64::new(1));
        let run = spawn_midprobe_worker(
            Arc::clone(&config),
            Arc::clone(&generation),
            slow_probe_content,
        );
        let startup = run
            .rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the loop head publishes the cleared generation-1 update");
        assert_eq!(startup.generation, 1);
        enter_rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the worker entered the probe");
        config.write().unwrap().network_probe_target = "synthetic-two".into();
        release_tx.send(()).expect("release #1");
        let republished = run
            .rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("a mid-probe policy-content change republishes a cleared update");
        assert_eq!(
            republished.generation, 1,
            "the republish carries the current generation"
        );
        assert!(
            cleared_update(&republished),
            "the first post-change update is the cleared baseline, never the stale probe outcome: {republished:?}"
        );
        enter_rx.recv_timeout(MIDPROBE_WAIT).expect(
            "last_attempt reset: immediate re-probe under the new policy despite a 600 s cadence",
        );
        release_tx.send(()).expect("release #2");
        let fresh = run
            .rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the re-probe publishes the new baseline");
        assert_eq!(fresh.generation, 1);
        assert!(
            fresh.available && fresh.ping.valid && (fresh.ping.value - 12.0).abs() < f64::EPSILON
        );
        assert!(
            fresh.loss.valid && fresh.loss.value == 0.0,
            "state.clear() left a fresh trial window: {fresh:?}"
        );
        assert!(
            !fresh.jitter.valid,
            "the cleared baseline has a single success, so jitter is not yet computable: {fresh:?}"
        );
        run.shutdown.store(true, Ordering::Release);
        run.thread
            .join()
            .expect("the worker exits within one TICK of shutdown");
    }

    /// L266 arm 3 (policy disabled mid-probe): the outcome is dropped, the block republishes
    /// a cleared update for the current generation, and the loop then publishes the disabled
    /// unavailable state; NO update ever carries the superseded outcome.
    /// Disclosed (see receipt §1.6 P-b3): a disabling flip always changes Policy content
    /// (enabled is a field), so this scenario fires through the content AND the !active()
    /// disjuncts together.
    #[test]
    fn policy_disabled_midprobe_discards_the_stale_outcome_and_republishes_the_disabled_state() {
        let (enter_rx, release_tx) = install_slow_probe(&SLOW_DISABLED);
        let config = Arc::new(RwLock::new(midprobe_config("synthetic-fixed")));
        let generation = Arc::new(AtomicU64::new(1));
        let run = spawn_midprobe_worker(
            Arc::clone(&config),
            Arc::clone(&generation),
            slow_probe_disabled,
        );
        let startup = run
            .rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the loop head publishes the cleared generation-1 update");
        assert_eq!(startup.generation, 1);
        enter_rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the worker entered the probe");
        config.write().unwrap().network_probe_enabled = false;
        release_tx.send(()).expect("release #1");
        let republished = run.rx.recv_timeout(MIDPROBE_WAIT).expect(
            "the mid-probe disable republishes a cleared update for the current generation",
        );
        assert_eq!(republished.generation, 1);
        assert!(
            cleared_update(&republished),
            "the first post-disable update is the cleared republish, never the stale outcome: {republished:?}"
        );
        let disabled = run
            .rx
            .recv_timeout(MIDPROBE_WAIT)
            .expect("the disabled policy publishes its unavailable state");
        assert_eq!(disabled.generation, 1);
        assert!(cleared_update(&disabled));
        let mut all = vec![startup, republished, disabled];
        while let Ok(update) = run.rx.recv_timeout(Duration::from_millis(250)) {
            all.push(update);
        }
        assert!(
            all.iter().all(|update| !update.ping.valid),
            "the superseded probe outcome is dropped on the disabled path: {all:?}"
        );
        run.shutdown.store(true, Ordering::Release);
        run.thread
            .join()
            .expect("the disabled worker exits within one TICK of shutdown");
    }
}
