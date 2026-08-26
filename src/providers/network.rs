//! Optional bounded ICMP/TCP quality probe.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
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
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-network-quality".into())
        .spawn(move || worker(config, shutdown, tx))
        .expect("network quality thread");
    (rx, thread)
}

fn worker(config: Arc<RwLock<Config>>, shutdown: Arc<AtomicBool>, tx: mpsc::Sender<Update>) {
    let mut state = State::default();
    let mut policy = None;
    let mut last_attempt = None;
    let mut disabled_published = false;
    while !shutdown.load(Ordering::Relaxed) {
        let current = Policy::from(&*config.read().unwrap_or_else(|e| e.into_inner()));
        if policy.as_ref().is_some_and(|old| old != &current) {
            state.clear();
            last_attempt = None;
            if tx.send(state.update.clone()).is_err() {
                return;
            }
        }
        policy = Some(current.clone());
        if !current.active() {
            if !disabled_published && tx.send(state.clear()).is_err() {
                return;
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
        let latest = Policy::from(&*config.read().unwrap_or_else(|e| e.into_inner()));
        if latest != current || !latest.active() {
            state.clear();
            policy = Some(latest);
            last_attempt = None;
            if tx.send(state.update.clone()).is_err() {
                return;
            }
            continue;
        }
        if tx
            .send(state.apply(outcome, current.window, SystemTime::now()))
            .is_err()
        {
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
}
