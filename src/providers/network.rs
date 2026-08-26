//! Optional bounded ICMP/TCP quality probe.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
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
static DNS_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

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
        let outcome = probe(&current);
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

fn probe(policy: &Policy) -> Outcome {
    match policy.method.as_str() {
        "tcp" => tcp_probe(&policy.target, policy.timeout),
        "auto" => match icmp_probe(auto_host(&policy.target), policy.timeout) {
            success @ Outcome::Success(_) => success,
            _ => tcp_probe(&auto_tcp_target(&policy.target), policy.timeout),
        },
        _ => icmp_probe(auto_host(&policy.target), policy.timeout),
    }
}

fn tcp_probe(target: &str, timeout: Duration) -> Outcome {
    let Ok(addresses) = resolve_bounded(target.to_string(), timeout) else {
        return Outcome::Error("network target resolution failed");
    };
    let Some(address) = addresses.into_iter().next() else {
        return Outcome::Error("network target has no address");
    };
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

fn icmp_probe(target: &str, timeout: Duration) -> Outcome {
    let query = if target.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
        format!("[{target}]:0")
    } else {
        format!("{target}:0")
    };
    let Ok(addresses) = resolve_bounded(query, timeout) else {
        return Outcome::Error("network target resolution failed");
    };
    let Some(IpAddr::V4(address)) = addresses
        .into_iter()
        .map(|address| address.ip())
        .find(|address| address.is_ipv4())
    else {
        return Outcome::Error("ICMP requires an IPv4 target");
    };
    let Ok(handle) = (unsafe { IcmpCreateFile() }) else {
        return Outcome::Error("ICMP provider unavailable");
    };
    let payload = b"LCDForge";
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

fn resolve_bounded(query: String, timeout: Duration) -> Result<Vec<SocketAddr>, ()> {
    if DNS_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return Err(());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = query.to_socket_addrs().map(|addresses| {
            let mut addresses: Vec<_> = addresses.collect();
            addresses.sort();
            addresses.dedup();
            addresses
        });
        DNS_IN_FLIGHT.store(false, Ordering::Release);
        let _ = tx.send(result);
    });
    rx.recv_timeout(timeout).map_err(|_| ())?.map_err(|_| ())
}

fn auto_host(target: &str) -> &str {
    if let Some(rest) = target.strip_prefix('[') {
        return rest.split_once(']').map(|(host, _)| host).unwrap_or(target);
    }
    if target.matches(':').count() == 1 {
        return target
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(target);
    }
    target
}

fn auto_tcp_target(target: &str) -> String {
    if target.parse::<SocketAddr>().is_ok()
        || (target.matches(':').count() == 1
            && target
                .rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok()))
    {
        target.to_string()
    } else if target.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
        format!("[{target}]:443")
    } else {
        format!("{target}:443")
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
    fn auto_target_uses_one_endpoint_and_default_https_fallback() {
        assert_eq!(auto_host("example.test:8443"), "example.test");
        assert_eq!(auto_tcp_target("example.test"), "example.test:443");
        assert_eq!(auto_tcp_target("example.test:8443"), "example.test:8443");
        assert_eq!(auto_tcp_target("::1"), "[::1]:443");
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
        assert!(matches!(
            tcp_probe(&address.to_string(), Duration::from_secs(1)),
            Outcome::Success(_)
        ));
        drop(owner.join().unwrap());
    }
}
