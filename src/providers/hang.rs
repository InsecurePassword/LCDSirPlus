//! Query-only hung-window detection and disposable test harness.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, BOOL, ERROR_ACCESS_DENIED, ERROR_SUCCESS, ERROR_TIMEOUT, FILETIME,
    HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetExitCodeProcess, GetProcessTimes, OpenProcess,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetAncestor,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    PeekMessageW, RegisterClassW, SendMessageTimeoutW, ShowWindow, TranslateMessage, CW_USEDEFAULT,
    GA_ROOT, MSG, PM_REMOVE, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SW_SHOW, WM_NULL, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

use crate::config::Config;
use crate::input::Event;
use crate::model::HungTarget;

const TICK: Duration = Duration::from_millis(50);
const POLL_BUDGET: Duration = Duration::from_secs(5);
const MAX_WINDOWS: usize = 4096;
const MAX_TITLE_CHARS: usize = 256;

/// TC-1 (P2 §2 TC-1): the bound a press-bind places on the publication that
/// carries a bound target is `bound_policy.interval + PUBLISH_AGE_TOLERANCE`,
/// measured on the monotonic clock. The interval is the probe interval in force
/// at bind time; this is the ONE stated tolerance.
///
/// Derivation — the tolerance is one publish cycle:
/// * `Cadence::poll_due` starts a publish no earlier than `hang_probe_interval_ms`
///   after the previous publish *start*, so a cadence step costs one interval.
/// * Producing the publication costs up to the bounded enumeration/probe budget
///   `POLL_BUDGET` (5 s: deadline in `enumerate_and_probe`, per-window clamp and
///   budget-exhaustion abort in `enumerate_callback`), and the publish stamp is
///   taken when the publish completes, so a legitimate publication lags the
///   schedule by exactly that budget in the worst case.
/// * One publish cycle = interval (cadence) + `POLL_BUDGET` (worst-case
///   production of one cycle), hence `PUBLISH_AGE_TOLERANCE = POLL_BUDGET`.
///
/// Consequence (P2 tradeoffs): a publication older than one whole cycle means the
/// detector skipped a cycle (stall, or a publish that exhausted its budget), so
/// the kill identity is no longer bounded; the press is refused with the audited
/// `canceled-continuity` outcome, never silently. `hang_probe_interval_ms` spans
/// 250..60000, so the bound is nearly free at short intervals and loose in
/// absolute time at 60000 (65 s), which is what the opt-in config range implies.
const PUBLISH_AGE_TOLERANCE: Duration = POLL_BUDGET;

/// TC-3 (WO-B-03-FIX): the capacity of [`HoldState::terminal`], the set of timed-out
/// published identities.
///
/// Derivation from the existing enumerate/publication caps (no new budget invented):
/// * A mark is pushed only for the target a press bound, and the press binds
///   `targets[hung_index % targets.len()]` (`event`), i.e. an entry of the
///   publication that is current at bind time.
/// * The gates then require presence in that same publication
///   (`target_in_current_generation`), so a live mark can only name an identity the
///   detector published.
/// * One publish cycle enumerates at most `MAX_WINDOWS` windows and ABORTS the whole
///   publication past that limit (`enumerate_callback` sets `limit_exceeded`, and
///   `enumerate_and_probe` turns it into `Err`, which publishes ZERO targets), and
///   one window contributes at most one observation, whose identity is one `Tracker`
///   record. So a publication carries at most `MAX_WINDOWS` distinct identities.
///
/// Hence `MAX_TERMINAL_MARKS = MAX_WINDOWS`: one mark per identity a publication can
/// carry. The production path cannot reach it (each mark costs one press-hold of
/// 1..10 s plus the 5 s bounded completion wait), and the generation prune in
/// [`HoldState::prune_terminal`] keeps the set far below it in practice. Eviction
/// when full is OLDEST-first, stated on `report_action` and pinned by
/// `the_terminal_set_evicts_the_oldest_mark_at_capacity`.
const MAX_TERMINAL_MARKS: usize = MAX_WINDOWS;

/// TC-4 (P2 §2 TC-4): the named publication token for a probe result the detector
/// cannot evaluate — `ProbeResult::Indeterminate`, the class `classify_probe_result`
/// gives every failed `WM_NULL` probe that is NOT a documented `ERROR_TIMEOUT`.
///
/// The token is the single formatting site [`probe_indeterminate_reason`] in the
/// RC-1/EC-6 named-cause discipline (the RC-1 precedent is
/// `LossCause::TransferIncomplete` -> `format!("transfer-incomplete({code})")`,
/// `src/backends/mod.rs` L95), with the count as payload: `probe-indeterminate-<N>`.
///
/// Driver (ACCEPTED b54-16 loss-vs-failure separation + B53-10/B53-11): the shipped
/// app's single detector (`IsHungAppWindow`) gives ONE undifferentiated signal, and the
/// counterpart absorbed this class silently (`Timeout | Indeterminate => {}`). The token
/// separates "probes lost their ability to evaluate" from "no timeout observed".
///
/// Distinctness (anti-masking): no token = the cycle saw no not-evaluable probe;
/// hard enumeration/budget/limit failures keep their own `Update::error` classes;
/// a token NEVER turns into a target (see [`record_probe_result`]).
const PROBE_INDETERMINATE: &str = "probe-indeterminate";

/// TC-4: the ONE site that renders the token, so the vocabulary cannot drift.
/// `None` = the publication has nothing of this class to report.
fn probe_indeterminate_reason(count: usize) -> Option<String> {
    (count != 0).then(|| format!("{PROBE_INDETERMINATE}-{count}"))
}

/// TC-4: one publish cycle's probe bookkeeping, built fresh inside
/// [`enumerate_and_probe`]. Because the record is created per cycle, a cycle's
/// not-evaluable count cannot leak into the next publication; the worker-side reset
/// rule is stated again in [`publish_indeterminate`].
///
/// Bounded by the existing caps, no new budget: one window is probed at most once per
/// cycle (`enumerate_callback` runs once per top-level window) and one cycle visits at
/// most `MAX_WINDOWS` windows before `limit_exceeded` aborts the whole publication
/// (`enumerate_callback` L1007-1011 + `enumerate_and_probe` L994-995 => ZERO targets),
/// so `indeterminate <= MAX_WINDOWS` (4096) always — the same derivation family as
/// `MAX_TERMINAL_MARKS`.
#[derive(Clone, Debug, Default)]
struct ProbeCycle {
    /// Eligibility input for the `Tracker`: only documented timeouts with a confirmed
    /// identity, exactly as before TC-4.
    observations: Vec<Observation>,
    /// The per-window-per-cycle count of `Indeterminate` probes (information only).
    indeterminate: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub generation: u64,
    pub targets: Vec<HungTarget>,
    pub available: bool,
    pub error: Option<&'static str>,
    /// TC-4 (P2 §2 TC-4): how many probes of the cycle that produced this publication
    /// returned `Indeterminate`. Purely additive: these probes never enter the
    /// eligibility math, so this number is never a target and never a failure, and a
    /// non-zero count publishes [`Update::availability_reason`] beside ZERO targets.
    /// The field is the machine-readable half of the signal; `0` is the default.
    pub probe_indeterminate: usize,
}

impl Update {
    /// TC-4: the named availability reason this publication carries, i.e.
    /// `probe-indeterminate-<N>` when the cycle lost probes it could not evaluate, and
    /// `None` when it did not — which is how "no timeout observed" stays distinguishable
    /// from "N not-evaluable probes".
    pub fn availability_reason(&self) -> Option<String> {
        probe_indeterminate_reason(self.probe_indeterminate)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    hwnd: usize,
    pid: u32,
    creation_time: u64,
    image_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Policy {
    enabled: bool,
    safe_mode: bool,
    interval: Duration,
    timeout: Duration,
    failures: i32,
    minimum: Duration,
    ignore: Vec<String>,
    button: i32,
    hold: Duration,
}

impl From<&Config> for Policy {
    fn from(cfg: &Config) -> Self {
        Self {
            enabled: cfg.hang_enabled,
            safe_mode: cfg.safe_mode,
            interval: cfg.hang_probe_interval,
            timeout: cfg.hang_probe_timeout,
            failures: cfg.hang_failures,
            minimum: cfg.hang_minimum,
            ignore: cfg.hang_ignore.clone(),
            button: cfg.hang_button,
            hold: cfg.hang_hold,
        }
    }
}

#[derive(Clone, Debug)]
struct Press {
    source: &'static str,
    started: Instant,
    /// EC-7: the stamp of the transition that bound this press, so a re-emitted
    /// transition of the same physical press is not bound as a second press
    /// (0 = unstamped, the pre-EC-7 shape, whose behavior is unchanged).
    stamp: u64,
    backward: bool,
    bound: Option<(usize, HungTarget, Policy)>,
    canceled: bool,
    resolved: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Audit {
    pub target: HungTarget,
    pub outcome: &'static str,
    /// TC-3 (P2 §2 TC-3): the exit-code classification the bounded completion wait
    /// produced for this action; `ExitInfo::None` on every non-action audit line.
    pub exit: ExitInfo,
}

impl Audit {
    /// An audit line with no completion classification (the pre-TC-3 shape).
    pub(crate) fn raised(target: HungTarget, outcome: &'static str) -> Self {
        Self {
            target,
            outcome,
            exit: ExitInfo::None,
        }
    }

    /// The audit line text `app.rs` logs verbatim, so the TC-3 classification is
    /// asserted where it is produced (P2 §2 TC-3 acceptance (1)).
    pub fn line(&self) -> String {
        format!(
            "hung action pid={} process={} outcome={}{}",
            self.target.pid,
            self.target.process_name,
            self.outcome,
            self.exit.suffix()
        )
    }
}

/// TC-3 (P2 §2 TC-3): what the bounded wait learned about the target exit AFTER a
/// terminate. Deliberately NOT an `ActionOutcome` class: the shipped outcome classes
/// (§3 items 5-8) stay exactly as they are, and this is extra named information on
/// the same audit line (anti-masking, per the TC-3 ban scan).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExitInfo {
    /// No completion classification applies (refusal, failed open, pre-terminate exit).
    #[default]
    None,
    /// Signaled completion and the exit code was read off the handle this action
    /// already opened (`PROCESS_QUERY_LIMITED_INFORMATION`; rights set unchanged).
    Captured(u32),
    /// Signaled completion, exit-code read failed: named in the audit line, never silent.
    Unavailable,
}

impl ExitInfo {
    /// Label suffix in the existing audit label style (`exit=0x1234`,
    /// `exit-code-unavailable`); empty when there is nothing to name.
    fn suffix(self) -> String {
        match self {
            Self::None => String::new(),
            Self::Captured(code) => format!(" exit={code:#x}"),
            Self::Unavailable => " exit-code-unavailable".to_owned(),
        }
    }
}

/// TC-3 (P2 §2 TC-3): what one guarded action produced — the shipped outcome class
/// plus the completion classification that rides the same audit line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionReport {
    pub outcome: ActionOutcome,
    pub exit: ExitInfo,
}

impl ActionReport {
    /// A report with no completion classification (the injected test terminators).
    #[cfg(test)]
    pub fn bare(outcome: ActionOutcome) -> Self {
        Self {
            outcome,
            exit: ExitInfo::None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum HoldCommand {
    None,
    Cycle { index: usize, backward: bool },
    Terminate(HungTarget),
    Audit(Audit),
}

#[derive(Default)]
pub struct HoldState {
    presses: [Option<Press>; 4],
    owner: Option<usize>,
    owner_resolved: bool,
    pub hung_index: usize,
    pub hung_detail: bool,
    /// TC-3 (P2 §2 TC-3, WO-B-03-FIX): the published identities (quadruples) whose last
    /// action reached `wait-timeout`, each with the publish generation that publication
    /// carried. A SET, not a slot: every timed-out identity of a generation stays
    /// terminal, so a second `wait-timeout` cannot evict an earlier identity's mark and
    /// re-open its terminate (the WO-B-03-AUDIT §3 gap). Bounded by
    /// [`MAX_TERMINAL_MARKS`] and pruned of generations the detector has republished
    /// ([`HoldState::prune_terminal`]); empty = no terminal action recorded, so every
    /// identity is bindable as before.
    terminal: Vec<(Identity, u64)>,
}

impl HoldState {
    pub fn set_owner(&mut self, owner: Option<usize>) -> Option<Audit> {
        self.owner_resolved = true;
        self.owner = owner;
        for (index, press) in self.presses.iter_mut().enumerate() {
            let Some(press) = press else { continue };
            let Some((_, target, _)) = &press.bound else {
                continue;
            };
            if !press.canceled && !press.resolved && Some(index) != owner {
                press.canceled = true;
                return Some(Audit::raised(target.clone(), "canceled-slot-owner"));
            }
        }
        None
    }

    pub fn reconcile(
        &mut self,
        cfg: &Config,
        targets: &[HungTarget],
        provider_available: bool,
    ) -> Option<Audit> {
        let policy = Policy::from(cfg);
        self.prune_terminal(targets);
        for (button, press) in self
            .presses
            .iter_mut()
            .enumerate()
            .filter_map(|(i, p)| p.as_mut().map(|p| (i, p)))
        {
            let Some((index, target, bound_policy)) = &press.bound else {
                continue;
            };
            if !press.canceled
                && !press.resolved
                && (!provider_available
                    || !policy.active()
                    || &policy != bound_policy
                    || (self.owner_resolved && self.owner != Some(button))
                    || targets.get(*index) != Some(target)
                    || !target_in_current_generation(target, targets)
                    || !publication_within_bind_bound(
                        target,
                        bound_policy,
                        targets,
                        Instant::now(),
                    )
                    || target_is_terminal(&self.terminal, target))
            {
                press.canceled = true;
                return Some(Audit::raised(target.clone(), "canceled-continuity"));
            }
        }
        None
    }

    pub fn cancel_reload(&mut self) -> Option<Audit> {
        for press in self.presses.iter_mut().flatten() {
            if let Some((_, target, _)) = &press.bound {
                if !press.canceled && !press.resolved {
                    press.canceled = true;
                    return Some(Audit::raised(target.clone(), "canceled-config-reload"));
                }
            }
        }
        None
    }

    pub fn progress(&self, now: Instant, cfg: &Config) -> f64 {
        self.presses
            .iter()
            .flatten()
            .find(|press| press.bound.is_some() && !press.canceled && !press.resolved)
            .map(|press| {
                now.saturating_duration_since(press.started).as_secs_f64()
                    / cfg.hang_hold.as_secs_f64()
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    pub fn event(
        &mut self,
        event: Event,
        cfg: &Config,
        targets: &[HungTarget],
        provider_available: bool,
    ) -> HoldCommand {
        if event.index >= self.presses.len() {
            return HoldCommand::None;
        }
        let button = if self.owner_resolved {
            self.owner
        } else {
            Some(cfg.hang_button.saturating_sub(1) as usize)
        };
        if let Some((owner, press)) = self.presses.iter().enumerate().find_map(|(index, press)| {
            press
                .as_ref()
                .filter(|p| p.bound.is_some())
                .map(|p| (index, p))
        }) {
            if event.index != owner || press.source != event.source {
                return HoldCommand::None;
            }
        }
        if event.down {
            if let Some(existing) = &self.presses[event.index] {
                // EC-7: a re-emitted transition of the press that is already bound
                // carries that press's own stamp, so it is one physical press and is
                // not re-bound (which would re-arm the hold budget). Unstamped
                // events (0) keep the shipped duplicate-down behavior.
                if existing.source != event.source
                    || existing.canceled
                    || existing.resolved
                    || (event.stamp != 0 && existing.stamp == event.stamp)
                {
                    return HoldCommand::None;
                }
            }
            let bound = if Some(event.index) == button
                && cfg.hang_enabled
                && !cfg.safe_mode
                && provider_available
            {
                (!targets.is_empty()).then(|| {
                    let index = self.hung_index % targets.len();
                    (index, targets[index].clone(), Policy::from(cfg))
                })
            } else {
                None
            };
            self.presses[event.index] = Some(Press {
                source: event.source,
                started: event.at,
                stamp: event.stamp,
                backward: event.backward,
                bound,
                canceled: false,
                resolved: false,
            });
            return HoldCommand::None;
        }

        let Some(press) = self.presses[event.index].take() else {
            return HoldCommand::None;
        };
        if press.source != event.source {
            self.presses[event.index] = Some(press);
            return HoldCommand::None;
        }
        if press.resolved {
            return HoldCommand::None;
        }
        let Some((index, target, policy)) = press.bound else {
            if event.canceled || event.at < press.started {
                return HoldCommand::None;
            }
            if Some(event.index) == button && !cfg.safe_mode && !targets.is_empty() {
                return HoldCommand::None;
            }
            return HoldCommand::Cycle {
                index: event.index,
                backward: press.backward || event.backward,
            };
        };
        if event.canceled || press.canceled {
            return HoldCommand::Audit(Audit::raised(
                target,
                if event.canceled {
                    "canceled-device-loss"
                } else {
                    "canceled-continuity"
                },
            ));
        }
        if !provider_available
            || !policy.active()
            || Policy::from(cfg) != policy
            || targets.get(index) != Some(&target)
        {
            return HoldCommand::Audit(Audit::raised(target, "refused-release-policy-or-target"));
        }
        let duration = event.at.saturating_duration_since(press.started);
        if duration >= cfg.hang_hold {
            return HoldCommand::None;
        }
        if self.hung_detail && targets.len() > 1 {
            self.hung_index = (index + 1) % targets.len();
            self.hung_detail = false;
        } else {
            self.hung_detail = !self.hung_detail;
        }
        HoldCommand::Audit(Audit::raised(target, "canceled-early-release"))
    }

    pub fn tick(
        &mut self,
        now: Instant,
        cfg: &Config,
        targets: &[HungTarget],
        provider_available: bool,
    ) -> HoldCommand {
        let policy = Policy::from(cfg);
        self.prune_terminal(targets);
        let owner = if self.owner_resolved {
            self.owner
        } else {
            Some(cfg.hang_button.saturating_sub(1) as usize)
        };
        let Some((button, press)) = self.presses.iter_mut().enumerate().find(|(_, press)| {
            press
                .as_ref()
                .is_some_and(|press| press.bound.is_some() && !press.canceled && !press.resolved)
        }) else {
            return HoldCommand::None;
        };
        let press = press.as_mut().unwrap();
        let (index, target, bound_policy) = press.bound.as_ref().unwrap();
        if !provider_available
            || !policy.active()
            || &policy != bound_policy
            || owner != Some(button)
            || targets.get(*index) != Some(target)
            || !target_in_current_generation(target, targets)
            || !publication_within_bind_bound(target, bound_policy, targets, now)
            || target_is_terminal(&self.terminal, target)
        {
            press.canceled = true;
            return HoldCommand::Audit(Audit::raised(target.clone(), "canceled-continuity"));
        }
        let duration = now.saturating_duration_since(press.started);
        if duration < cfg.hang_hold {
            return HoldCommand::None;
        }
        press.resolved = true;
        if duration > maximum_press_duration(cfg.hang_hold) {
            return HoldCommand::Audit(Audit::raised(target.clone(), "refused-stale-hold"));
        }
        HoldCommand::Terminate(target.clone())
    }

    /// TC-3 (P2 §2 TC-3): `wait-timeout` is a TERMINAL outcome for the published
    /// identity the action acted on. The identity is marked action-attempted together
    /// with the publish generation that publication carried, so a later press can only
    /// bind an identity a probe cycle RE-PUBLISHED at a strictly newer generation —
    /// no repeat terminate on a stale identity, and no permanent lockout of the
    /// feature (the re-publish re-arm is the only re-arm, §3 item 10; the mark rides
    /// the existing `canceled-continuity` refusal, so the press is audited, never silent).
    ///
    /// WO-B-03-FIX: the marks are a SET, so marking a second identity of the same
    /// publication keeps every earlier identity's mark (the one-slot field let a second
    /// `wait-timeout` evict the first mark, which re-opened a stale-identity terminate
    /// in a legal long-interval config — WO-B-03-AUDIT §3). Single-identity semantics are
    /// unchanged: one identity, one mark, refused until a re-publish.
    pub fn report_action(&mut self, target: &HungTarget, outcome: ActionOutcome) {
        if outcome != ActionOutcome::WaitTimeout {
            return;
        }
        let mark = (Identity::from(target), target.publish_generation);
        if self.terminal.contains(&mark) {
            return;
        }
        // Stated eviction policy: at capacity the OLDEST mark is dropped (it carries the
        // lowest publish generation, so it is the first entry `prune_terminal` discards
        // anyway), and the mark this action just earned is always kept — the set never
        // trades the identity that just timed out for a stale one.
        if self.terminal.len() >= MAX_TERMINAL_MARKS {
            self.terminal.remove(0);
        }
        self.terminal.push(mark);
    }

    /// TC-3 (WO-B-03-FIX): the mark-set half of the re-arm rule. A publication at a
    /// newer generation IS the re-publish the spec requires to re-arm (`event` binds an
    /// entry of the current publication, and `target_in_current_generation` refuses a
    /// bind older than it), so marks of generations the detector has already replaced
    /// are inert: dropping them at every gate pass is what keeps the set bounded within
    /// one publication. The app feeds each publication to `HoldState` at the two gate
    /// sites (`reconcile`/`tick`, once per run-loop iteration), which is where this runs.
    /// An unstamped or empty publication carries no stated generation, so nothing is
    /// pruned — the refusal-safe direction, matching `target_in_current_generation`.
    fn prune_terminal(&mut self, targets: &[HungTarget]) {
        let current = targets
            .iter()
            .map(|target| target.publish_generation)
            .max()
            .unwrap_or(0);
        if current != 0 {
            self.terminal
                .retain(|(_, generation)| *generation >= current);
        }
    }
}

/// TC-3: the terminal-marking half of the bind-side refusal (see
/// [`HoldState::report_action`]). A free function so the press-side gates in
/// `reconcile`/`tick` read the `terminal` field alongside the pressed target.
/// WO-B-03-FIX: the bound target is terminal when ANY stored mark names its
/// quadruple with a generation at least as new as the one the press bound — the same
/// five clauses the one-slot field had, applied to every identity of the set.
fn target_is_terminal(terminal: &[(Identity, u64)], bound: &HungTarget) -> bool {
    terminal.iter().any(|(identity, generation)| {
        bound.publish_generation <= *generation
            && bound.hwnd == identity.hwnd
            && bound.pid == identity.pid
            && bound.creation_time == identity.creation_time
            && bound.image_path == identity.image_path
    })
}

fn maximum_press_duration(hold: Duration) -> Duration {
    (hold.saturating_mul(2)).clamp(Duration::from_secs(3), Duration::from_secs(15))
}

impl Policy {
    fn active(&self) -> bool {
        self.enabled && !self.safe_mode
    }
}

/// TC-1 stamping: every target of a publication carries the detector's monotonic
/// publish generation and the monotonic time of that publish. Driver
/// B53-06/B53-17: the shipped app zeroes its target global every pass, so its
/// kill identity is at most one render tick old; the counterpart bounds the
/// identity to the newest publication instead, and the two gates below measure it.
fn stamp_publish(targets: Vec<HungTarget>, generation: u64) -> Vec<HungTarget> {
    let published_at = Instant::now();
    targets
        .into_iter()
        .map(|mut target| {
            target.publish_generation = generation;
            target.publish_time = Some(published_at);
            target
        })
        .collect()
}

/// TC-1 action bound: the bound identity must be present in the CURRENT publish
/// generation, and that generation must be *stated* (non-zero, stamped by the
/// detector) and never older than the generation the press bound. Presence keeps
/// the pre-TC-1 identity meaning, so this clause can only add a refusal.
fn target_in_current_generation(bound: &HungTarget, targets: &[HungTarget]) -> bool {
    bound.publish_generation != 0
        && targets.iter().any(|live| {
            live == bound
                && live.publish_generation != 0
                && live.publish_generation >= bound.publish_generation
        })
}

/// TC-1 press-bind bound: the publication that carries the bound identity is at
/// most `interval + PUBLISH_AGE_TOLERANCE` old, where `interval` is the probe
/// interval in force at bind time (`bound_policy`).
fn publication_within_bind_bound(
    bound: &HungTarget,
    bound_policy: &Policy,
    targets: &[HungTarget],
    now: Instant,
) -> bool {
    targets.iter().any(|live| {
        live == bound
            && live.publish_time.is_some_and(|published_at| {
                now.saturating_duration_since(published_at)
                    <= bound_policy.interval + PUBLISH_AGE_TOLERANCE
            })
    })
}

impl From<&HungTarget> for Identity {
    fn from(target: &HungTarget) -> Self {
        Self {
            hwnd: target.hwnd,
            pid: target.pid,
            creation_time: target.creation_time,
            image_path: target.image_path.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct Observation {
    target: HungTarget,
    responsive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeResult {
    Responsive,
    Timeout,
    Indeterminate,
}

#[derive(Clone, Debug)]
struct Record {
    target: HungTarget,
    first_failure: Duration,
    failures: i32,
}

#[derive(Default)]
struct Tracker {
    records: HashMap<Identity, Record>,
}

#[derive(Default)]
struct Cadence {
    last_attempt: Option<Duration>,
}

impl Cadence {
    fn poll_due(&mut self, policy: &Policy, now: Duration) -> bool {
        if !policy.active() {
            self.last_attempt = None;
            return false;
        }
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_sub(last) < policy.interval)
        {
            return false;
        }
        self.last_attempt = Some(now);
        true
    }
}

fn reset_on_policy_change(
    current: &mut Option<Policy>,
    next: &Policy,
    tracker: &mut Tracker,
    cadence: &mut Cadence,
) -> bool {
    let changed = current.as_ref().is_some_and(|old| old != next);
    if changed {
        tracker.clear();
        cadence.last_attempt = None;
    }
    *current = Some(next.clone());
    changed
}

fn poll_policy_current(started: &Policy, latest: &Policy) -> bool {
    latest.active() && latest == started
}

impl Tracker {
    fn clear(&mut self) {
        self.records.clear();
    }

    fn update(
        &mut self,
        now: Duration,
        observations: Vec<Observation>,
        failures_required: i32,
        minimum: Duration,
    ) -> Vec<HungTarget> {
        let previous = std::mem::take(&mut self.records);
        for observation in observations {
            if observation.responsive {
                continue;
            }
            let identity = Identity::from(&observation.target);
            let mut record = previous.get(&identity).cloned().unwrap_or(Record {
                target: observation.target.clone(),
                first_failure: now,
                failures: 0,
            });
            record.target = observation.target;
            record.failures += 1;
            self.records.insert(identity, record);
        }

        let mut confirmed: Vec<&Record> = self
            .records
            .values()
            .filter(|record| {
                record.failures >= failures_required
                    && now.saturating_sub(record.first_failure) >= minimum
            })
            .collect();
        confirmed.sort_by(|a, b| {
            a.first_failure
                .cmp(&b.first_failure)
                .then_with(|| a.target.image_path.cmp(&b.target.image_path))
                .then_with(|| a.target.pid.cmp(&b.target.pid))
                .then_with(|| a.target.hwnd.cmp(&b.target.hwnd))
        });
        confirmed
            .into_iter()
            .map(|record| record.target.clone())
            .collect()
    }
}

pub fn spawn(
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<Update>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-hang".into())
        .spawn(move || worker(config, generation, shutdown, tx))
        .expect("hung detector thread");
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
) {
    let started = Instant::now();
    let mut tracker = Tracker::default();
    let mut cadence = Cadence::default();
    let mut current_policy = None;
    let mut disabled_published = false;
    let mut published_available = None;
    let mut published_targets = None;
    // TC-4: the not-evaluable count of the CURRENTLY published cycle. Written only
    // through `publish_indeterminate`, which replaces it per cycle (never accumulates).
    let mut published_indeterminate = 0usize;
    let mut current_generation = 0;
    let mut publish_generation = 0u64;
    while !shutdown.load(Ordering::Relaxed) {
        let (request_generation, cfg) = current_request(&config, &generation);
        if request_generation != current_generation {
            current_generation = request_generation;
            tracker.clear();
            cadence.last_attempt = None;
            current_policy = None;
            disabled_published = false;
            published_available = None;
            published_targets = None;
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
        let policy = Policy::from(&cfg);
        if reset_on_policy_change(&mut current_policy, &policy, &mut tracker, &mut cadence) {
            log_worker_transition(
                &mut published_available,
                &mut published_targets,
                policy.active(),
                0,
            );
            if tx
                .send(Update {
                    generation: request_generation,
                    targets: Vec::new(),
                    available: policy.active(),
                    error: None,
                    ..Default::default()
                })
                .is_err()
            {
                return;
            }
            disabled_published = !policy.active();
            continue;
        }
        if !policy.active() {
            tracker.clear();
            cadence.poll_due(&policy, started.elapsed());
            if !disabled_published && {
                log_worker_transition(&mut published_available, &mut published_targets, false, 0);
                tx.send(Update {
                    generation: request_generation,
                    targets: Vec::new(),
                    available: false,
                    error: None,
                    ..Default::default()
                })
                .is_err()
            } {
                return;
            }
            disabled_published = true;
            std::thread::sleep(TICK);
            continue;
        }
        disabled_published = false;
        if !cadence.poll_due(&policy, started.elapsed()) {
            std::thread::sleep(TICK);
            continue;
        }
        let result = enumerate_and_probe(policy.timeout, &policy.ignore, None);
        let (latest_generation, latest) = current_request(&config, &generation);
        let latest_policy = Policy::from(&latest);
        if latest_generation != request_generation || !poll_policy_current(&policy, &latest_policy)
        {
            current_generation = latest_generation;
            tracker.clear();
            cadence.last_attempt = None;
            current_policy = Some(latest_policy.clone());
            disabled_published = !latest_policy.active();
            log_worker_transition(
                &mut published_available,
                &mut published_targets,
                latest_policy.active(),
                0,
            );
            if tx
                .send(Update {
                    generation: latest_generation,
                    targets: Vec::new(),
                    available: latest_policy.active(),
                    error: None,
                    ..Default::default()
                })
                .is_err()
            {
                return;
            }
            continue;
        }
        let update = match result {
            Ok(cycle) => {
                publish_generation += 1;
                let probe_indeterminate =
                    publish_indeterminate(&cycle, &mut published_indeterminate);
                Update {
                    generation: request_generation,
                    targets: stamp_publish(
                        tracker.update(
                            started.elapsed(),
                            cycle.observations,
                            policy.failures,
                            policy.minimum,
                        ),
                        publish_generation,
                    ),
                    available: true,
                    error: None,
                    probe_indeterminate,
                }
            }
            Err(error) => {
                tracker.clear();
                // An aborted cycle (budget, enumeration, limit) has no publication, so
                // the same reset rule publishes NO count — a stale number cannot ride on
                // the zero-target failure update either.
                let probe_indeterminate =
                    publish_indeterminate(&ProbeCycle::default(), &mut published_indeterminate);
                Update {
                    generation: request_generation,
                    targets: Vec::new(),
                    available: false,
                    error: Some(error),
                    probe_indeterminate,
                }
            }
        };
        log_worker_transition(
            &mut published_available,
            &mut published_targets,
            update.available,
            update.targets.len(),
        );
        if let Some(reason) = update.availability_reason() {
            // TC-4 (P2 §2 TC-4): the named loss cause is audited once per publish
            // cycle — bounded and never silent, the same discipline as the TC-2
            // acceptance-refusal log in `enumerate_and_probe`.
            crate::log_warn!("hung-window detector {reason}");
        }
        if tx.send(update).is_err() {
            return;
        }
    }
}

fn log_worker_transition(
    available: &mut Option<bool>,
    targets: &mut Option<usize>,
    next_available: bool,
    next_targets: usize,
) {
    if *available != Some(next_available) {
        crate::log_info!("hung-window detector available={next_available}");
        *available = Some(next_available);
    }
    if *targets != Some(next_targets) {
        crate::log_info!("hung-window detector confirmed_targets={next_targets}");
        *targets = Some(next_targets);
    }
}

struct EnumerationContext {
    ignored: HashSet<String>,
    windows_dir: String,
    self_pid: u32,
    /// TC-2: the session of this process, computed once per publish cycle; `Err` =
    /// the session query failed, which refuses every candidate fail-closed.
    self_session: Result<u32, ()>,
    only_pid: Option<u32>,
    timeout: Duration,
    deadline: Instant,
    /// TC-4: this cycle's probe bookkeeping — the eligible observations AND the
    /// per-window-per-cycle count of not-evaluable probes. Fresh per cycle.
    cycle: ProbeCycle,
    visited: usize,
    stopped: bool,
    limit_exceeded: bool,
    refusals: AcceptanceCounters,
}

fn enumerate_and_probe(
    timeout: Duration,
    ignore: &[String],
    only_pid: Option<u32>,
) -> Result<ProbeCycle, &'static str> {
    let windows_dir = windows_directory()?;
    let mut ignored = ignored_names(ignore);
    ignored.insert("lcdsirplus.exe".into());
    let self_pid = unsafe { GetCurrentProcessId() };
    let mut context = EnumerationContext {
        ignored,
        windows_dir,
        self_pid,
        self_session: session_of(self_pid),
        only_pid,
        timeout,
        deadline: Instant::now() + POLL_BUDGET,
        cycle: ProbeCycle::default(),
        visited: 0,
        stopped: false,
        limit_exceeded: false,
        refusals: AcceptanceCounters::default(),
    };
    let result = unsafe {
        EnumWindows(
            Some(enumerate_callback),
            LPARAM((&mut context as *mut EnumerationContext) as isize),
        )
    };
    if context.refusals.cross_session != 0 {
        // TC-2: acceptance-side refusals are audited once per cycle (bounded, never
        // silent), using the same named vocabulary as the action outcomes.
        crate::log_warn!(
            "hung-window detector {}={}",
            ActionOutcome::CrossSession.label(),
            context.refusals.cross_session
        );
    }
    if context.refusals.session_unknown != 0 {
        crate::log_warn!(
            "hung-window detector {}={}",
            ActionOutcome::SessionUnknown.label(),
            context.refusals.session_unknown
        );
    }
    if result.is_err() && !context.stopped {
        return Err("window enumeration failed");
    }
    if Instant::now() >= context.deadline {
        return Err("hung-window poll budget exceeded");
    }
    if context.limit_exceeded {
        return Err("hung-window enumeration limit exceeded");
    }
    Ok(context.cycle)
}

unsafe extern "system" fn enumerate_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = &mut *(lparam.0 as *mut EnumerationContext);
    if Instant::now() >= context.deadline {
        context.stopped = true;
        return BOOL(0);
    }
    context.visited += 1;
    if context.visited > MAX_WINDOWS {
        context.stopped = true;
        context.limit_exceeded = true;
        return BOOL(0);
    }
    let Some(target) = describe_candidate(
        hwnd,
        context.self_pid,
        context.self_session,
        context.only_pid,
        &context.ignored,
        &context.windows_dir,
        &mut context.refusals,
    ) else {
        return BOOL(1);
    };
    let remaining = context.deadline.saturating_duration_since(Instant::now());
    let probe_timeout = context.timeout.min(remaining).max(Duration::from_millis(1));
    let result = probe(hwnd, probe_timeout);
    // The post-probe identity cross-check runs ONLY for a documented timeout, exactly
    // as the old guard arm did (short-circuit), so no extra Win32 call is added on the
    // responsive path and the eligibility semantics cannot change.
    let identity_confirmed =
        result == ProbeResult::Timeout && identity_still_matches(hwnd, &target);
    record_probe_result(result, identity_confirmed, target, &mut context.cycle);
    BOOL(1)
}

unsafe fn describe_candidate(
    hwnd: HWND,
    self_pid: u32,
    self_session: Result<u32, ()>,
    only_pid: Option<u32>,
    ignored: &HashSet<String>,
    windows_dir: &str,
    refusals: &mut AcceptanceCounters,
) -> Option<HungTarget> {
    if !IsWindowVisible(hwnd).as_bool() {
        return None;
    }
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 || length > 32767 {
        return None;
    }
    let mut title_buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut title_buffer);
    if copied <= 0 {
        return None;
    }
    let title = safe_display_text(&String::from_utf16_lossy(&title_buffer[..copied as usize]));
    if title.is_empty() {
        return None;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == self_pid || only_pid.is_some_and(|expected| expected != pid) {
        return None;
    }
    // TC-2 (P2 §2 TC-2): session identity is checked immediately after the pid floor
    // (which stays the first gate, §3 item 1), so a cross-session candidate never enters
    // the publication and a failed session query is refused fail-closed.
    match session_gate(session_of(pid), self_session) {
        SessionGate::Same => {}
        SessionGate::Cross => {
            refusals.cross_session += 1;
            return None;
        }
        SessionGate::Unknown => {
            refusals.session_unknown += 1;
            return None;
        }
    }
    let (creation_time, image_path, process_name) = query_identity(pid).ok()?;
    if excluded(&process_name, &image_path, ignored, windows_dir) {
        return None;
    }
    Some(HungTarget {
        hwnd: hwnd.0 as usize,
        pid,
        creation_time,
        image_path,
        process_name,
        title,
        ..Default::default()
    })
}

/// TC-2 (P2 §2 TC-2): session of a process id, mirroring the `src/backends/sdk.rs`
/// restart-path pair check. No handle is opened and no rights are requested.
fn session_of(pid: u32) -> Result<u32, ()> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .map(|_| session)
        .map_err(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionGate {
    /// Target shares this process's session: candidate stays eligible.
    Same,
    /// Foreign session: non-actionable (P2 TC-2 tradeoff), audited by name.
    Cross,
    /// A `ProcessIdToSessionId` query failed: refused fail-closed, never proceed.
    Unknown,
}

fn session_gate(target: Result<u32, ()>, current: Result<u32, ()>) -> SessionGate {
    // One precondition per edge: target query ok? current query ok? sessions equal?
    let Ok(target) = target else {
        return SessionGate::Unknown;
    };
    let Ok(current) = current else {
        return SessionGate::Unknown;
    };
    if target == current {
        SessionGate::Same
    } else {
        SessionGate::Cross
    }
}

/// Per-cycle acceptance refusals, reported once by `enumerate_and_probe`.
#[derive(Clone, Copy, Debug, Default)]
struct AcceptanceCounters {
    cross_session: usize,
    session_unknown: usize,
}

unsafe fn probe(hwnd: HWND, timeout: Duration) -> ProbeResult {
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    SetLastError(ERROR_SUCCESS);
    let result = SendMessageTimeoutW(
        hwnd,
        WM_NULL,
        WPARAM(0),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
        timeout_ms,
        None,
    );
    classify_probe_result(result.0 != 0, GetLastError())
}

fn classify_probe_result(succeeded: bool, error: WIN32_ERROR) -> ProbeResult {
    if succeeded {
        ProbeResult::Responsive
    } else if error == ERROR_TIMEOUT {
        ProbeResult::Timeout
    } else {
        ProbeResult::Indeterminate
    }
}

/// TC-4 (P2 §2 TC-4): the disposition of ONE probe result inside the current publish
/// cycle. The eligibility arms keep their exact pre-TC-4 semantics — the class pinned by
/// `probe_classification_counts_only_documented_timeout`:
/// * `Responsive` -> a responsive observation (as before);
/// * `Timeout` ONLY with a confirmed identity (`identity_confirmed`, the same
///   `identity_still_matches` cross-check the guard arm ran) -> the ONLY path that can
///   make a target;
/// * `Timeout` without it -> nothing (as before);
/// * `Indeterminate` -> NOTHING for eligibility: it never becomes an `Observation`, so it
///   never reaches the `Tracker`, the failure counters, or a published target. It is
///   counted here, per window per cycle, which is the whole TC-4 change: the old
///   absorption site `Timeout | Indeterminate => {}` becomes a named loss cause
///   (b54-16 loss-vs-failure, B53-10/B53-11 single undifferentiated detector).
///
/// Bounded: at most one result per visited window, and one cycle visits at most
/// `MAX_WINDOWS` windows before the publication is aborted, so
/// `cycle.indeterminate <= MAX_WINDOWS` (derivation on [`ProbeCycle`]).
fn record_probe_result(
    result: ProbeResult,
    identity_confirmed: bool,
    target: HungTarget,
    cycle: &mut ProbeCycle,
) {
    match result {
        ProbeResult::Responsive => cycle.observations.push(Observation {
            target,
            responsive: true,
        }),
        ProbeResult::Timeout if identity_confirmed => cycle.observations.push(Observation {
            target,
            responsive: false,
        }),
        ProbeResult::Timeout => {}
        ProbeResult::Indeterminate => cycle.indeterminate += 1,
    }
}

/// TC-4: publish one cycle's not-evaluable count. The count of the published cycle
/// REPLACES the previous one (assignment, never accumulation), so a stale count can
/// never leak into a new publication — including the zero-target publications of an
/// aborted cycle, which pass a default [`ProbeCycle`].
///
/// Derivation stays the [`ProbeCycle`] bound: the value assigned here is at most
/// `MAX_WINDOWS` per publication, and the worker holds exactly one such number.
fn publish_indeterminate(cycle: &ProbeCycle, published: &mut usize) -> usize {
    *published = cycle.indeterminate;
    *published
}

unsafe fn identity_still_matches(hwnd: HWND, expected: &HungTarget) -> bool {
    if !IsWindow(hwnd).as_bool() {
        return false;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let Ok((creation_time, image_path, _)) = query_identity(pid) else {
        return false;
    };
    identity_matches(expected, hwnd.0 as usize, pid, creation_time, &image_path)
}

fn identity_matches(
    expected: &HungTarget,
    hwnd: usize,
    pid: u32,
    creation_time: u64,
    image_path: &str,
) -> bool {
    expected.hwnd == hwnd
        && expected.pid == pid
        && expected.creation_time == creation_time
        && expected.image_path == normalize_path(image_path)
}

fn query_identity(pid: u32) -> Result<(u64, String, String), ()> {
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|_| ())?;
    let result = query_identity_handle(handle);
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    result
}

fn query_identity_handle(handle: HANDLE) -> Result<(u64, String, String), ()> {
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
        .map_err(|_| ())?;
    }
    if length == 0 || length as usize > buffer.len() {
        return Err(());
    }
    let image_path = normalize_path(&String::from_utf16_lossy(&buffer[..length as usize]));
    let process_name = safe_display_text(
        Path::new(&image_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""),
    );
    if image_path.is_empty() || process_name.is_empty() {
        return Err(());
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)
            .map_err(|_| ())?;
    }
    let creation_time = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    if creation_time == 0 {
        return Err(());
    }
    Ok((creation_time, image_path, process_name))
}

fn windows_directory() -> Result<String, &'static str> {
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("authoritative Windows directory unavailable");
    }
    let path = normalize_path(&String::from_utf16_lossy(&buffer[..length]));
    if path.is_empty() {
        Err("authoritative Windows directory unavailable")
    } else {
        Ok(path)
    }
}

fn ignored_names(values: &[String]) -> HashSet<String> {
    let mut ignored: HashSet<String> = values
        .iter()
        .filter_map(|value| {
            Path::new(value.trim())
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_lowercase())
        })
        .collect();
    for name in [
        "system",
        "registry",
        "memory compression",
        "idle",
        "secure system",
    ] {
        ignored.insert(name.into());
    }
    ignored
}

fn excluded(name: &str, path: &str, ignored: &HashSet<String>, windows_dir: &str) -> bool {
    let name = name.to_lowercase();
    let path = normalize_path(path);
    let windows_dir = normalize_path(windows_dir);
    ignored.contains(&name)
        || path == windows_dir
        || path
            .strip_prefix(&windows_dir)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

fn normalize_path(value: &str) -> String {
    let replaced = value.trim().replace('/', "\\");
    replaced
        .strip_prefix("\\\\?\\")
        .unwrap_or(&replaced)
        .trim_end_matches('\\')
        .to_lowercase()
}

fn safe_display_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    Success,
    Refused,
    Recovered,
    IdentityMismatch,
    AccessDenied,
    /// TC-2 (P2 §2 TC-2): the target lives in another session; named refusal raised
    /// before the target is opened, so the `AccessDenied` class stays reachable.
    CrossSession,
    /// TC-2 (P2 §2 TC-2): a session query failed; refused fail-closed, never proceed.
    SessionUnknown,
    TerminateFailure,
    WaitTimeout,
}

impl ActionOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Refused => "refused",
            Self::Recovered => "recovered",
            Self::IdentityMismatch => "identity-mismatch",
            Self::AccessDenied => "access-denied",
            Self::CrossSession => "refused-cross-session",
            Self::SessionUnknown => "refused-session-unknown",
            Self::TerminateFailure => "terminate-failure",
            Self::WaitTimeout => "wait-timeout",
        }
    }
}

enum ActionWait {
    Signaled,
    Timeout,
    Failed,
}

trait ActionApi {
    type Process;

    fn window(&mut self, hwnd: usize) -> Result<HungTarget, ActionOutcome>;
    fn probe(&mut self, hwnd: usize, timeout: Duration) -> ProbeResult;
    fn open(&mut self, pid: u32) -> Result<Self::Process, ActionOutcome>;
    fn session(&mut self, pid: u32) -> Result<u32, ActionOutcome>;
    fn process_identity(&mut self, process: &Self::Process)
        -> Result<(u64, String), ActionOutcome>;
    fn terminate(&mut self, process: &Self::Process) -> Result<(), ActionOutcome>;
    fn wait(&mut self, process: &Self::Process, timeout: Duration) -> ActionWait;
    /// TC-3 (P2 §2 TC-3): read the target's exit code from the handle the action
    /// ALREADY opened (rights set unchanged), after a signaled completion. Its failure
    /// is a named classification, never an outcome class, so `Err(())` carries no class.
    fn exit_code(&mut self, process: &Self::Process) -> Result<u32, ()>;
}

struct OwnedProcess(HANDLE);

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

struct NativeActionApi;

impl ActionApi for NativeActionApi {
    type Process = OwnedProcess;

    fn window(&mut self, hwnd: usize) -> Result<HungTarget, ActionOutcome> {
        unsafe { action_window(HWND(hwnd as *mut _)) }
    }

    fn probe(&mut self, hwnd: usize, timeout: Duration) -> ProbeResult {
        unsafe { probe(HWND(hwnd as *mut _), timeout) }
    }

    fn open(&mut self, pid: u32) -> Result<Self::Process, ActionOutcome> {
        let rights = PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
        unsafe { OpenProcess(rights, false, pid) }
            .map(OwnedProcess)
            .map_err(|error| {
                if error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                    ActionOutcome::AccessDenied
                } else {
                    ActionOutcome::Refused
                }
            })
    }

    fn session(&mut self, pid: u32) -> Result<u32, ActionOutcome> {
        session_of(pid).map_err(|_| ActionOutcome::SessionUnknown)
    }

    fn process_identity(
        &mut self,
        process: &Self::Process,
    ) -> Result<(u64, String), ActionOutcome> {
        query_identity_handle(process.0)
            .map(|(creation, path, _)| (creation, path))
            .map_err(|_| ActionOutcome::IdentityMismatch)
    }

    fn terminate(&mut self, process: &Self::Process) -> Result<(), ActionOutcome> {
        unsafe { TerminateProcess(process.0, 0x4c43_4453) }
            .map_err(|_| ActionOutcome::TerminateFailure)
    }

    fn wait(&mut self, process: &Self::Process, timeout: Duration) -> ActionWait {
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { WaitForSingleObject(process.0, millis) } {
            WAIT_OBJECT_0 => ActionWait::Signaled,
            WAIT_TIMEOUT => ActionWait::Timeout,
            _ => ActionWait::Failed,
        }
    }

    fn exit_code(&mut self, process: &Self::Process) -> Result<u32, ()> {
        // TC-3 (P2 §2 TC-3), read-only on the existing handle — the
        // `src/recovery.rs` GetExitCodeProcess precedent (`Win32_System_Threading`,
        // already a declared feature, so the Cargo manifest is untouched).
        let mut code = 0;
        unsafe { GetExitCodeProcess(process.0, &mut code) }
            .map(|()| code)
            .map_err(|_| ())
    }
}

unsafe fn action_window(hwnd: HWND) -> Result<HungTarget, ActionOutcome> {
    if !IsWindow(hwnd).as_bool()
        || !IsWindowVisible(hwnd).as_bool()
        || GetAncestor(hwnd, GA_ROOT) != hwnd
    {
        return Err(ActionOutcome::Refused);
    }
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 || length > 32767 {
        return Err(ActionOutcome::Refused);
    }
    let mut title_buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut title_buffer);
    if copied <= 0 {
        return Err(ActionOutcome::Refused);
    }
    let title = safe_display_text(&String::from_utf16_lossy(&title_buffer[..copied as usize]));
    if title.is_empty() {
        return Err(ActionOutcome::Refused);
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let (creation_time, image_path, process_name) =
        query_identity(pid).map_err(|_| ActionOutcome::Refused)?;
    Ok(HungTarget {
        hwnd: hwnd.0 as usize,
        pid,
        creation_time,
        image_path,
        process_name,
        title,
        ..Default::default()
    })
}

pub fn terminate_bound_target(target: &HungTarget, cfg: &Config) -> ActionReport {
    terminate_action_with(&mut NativeActionApi, target, cfg)
}

/// TC-3 (P2 §2 TC-3): the full completion report for one guarded action.
fn terminate_action_with<A: ActionApi>(
    api: &mut A,
    target: &HungTarget,
    cfg: &Config,
) -> ActionReport {
    let mut exit = ExitInfo::None;
    let outcome = terminate_outcome_with(api, target, cfg, &mut exit);
    ActionReport { outcome, exit }
}

/// The pre-TC-3 entry point kept verbatim for the shipped outcome-only tests: the
/// outcome class is untouched by the completion classification.
#[cfg(test)]
fn terminate_bound_target_with<A: ActionApi>(
    api: &mut A,
    target: &HungTarget,
    cfg: &Config,
) -> ActionOutcome {
    terminate_outcome_with(api, target, cfg, &mut ExitInfo::None)
}

fn terminate_outcome_with<A: ActionApi>(
    api: &mut A,
    target: &HungTarget,
    cfg: &Config,
    exit: &mut ExitInfo,
) -> ActionOutcome {
    if cfg.safe_mode
        || !cfg.hang_enabled
        || target.hwnd == 0
        || target.pid == 0
        || target.creation_time == 0
        || target.image_path.is_empty()
        || target.process_name.is_empty()
    {
        return ActionOutcome::Refused;
    }
    let Ok(windows_dir) = windows_directory() else {
        return ActionOutcome::Refused;
    };
    let ignored = ignored_names(&cfg.hang_ignore);
    let self_pid = unsafe { GetCurrentProcessId() };
    let validate_window = |current: &HungTarget| {
        if current != target {
            ActionOutcome::IdentityMismatch
        } else if current.pid == self_pid
            || excluded(
                &current.process_name,
                &current.image_path,
                &ignored,
                &windows_dir,
            )
        {
            ActionOutcome::Refused
        } else {
            ActionOutcome::Success
        }
    };

    let current = match api.window(target.hwnd) {
        Ok(current) => current,
        Err(outcome) => return outcome,
    };
    if validate_window(&current) != ActionOutcome::Success {
        return validate_window(&current);
    }
    match api.probe(target.hwnd, cfg.hang_probe_timeout) {
        ProbeResult::Responsive => return ActionOutcome::Recovered,
        ProbeResult::Indeterminate => return ActionOutcome::Refused,
        ProbeResult::Timeout => {}
    }
    match api.window(target.hwnd) {
        Ok(current) if validate_window(&current) == ActionOutcome::Success => {}
        Ok(current) => return validate_window(&current),
        Err(outcome) => return outcome,
    }

    // TC-2 (P2 §2 TC-2): the session pair (the `src/backends/sdk.rs` restart precedent) is
    // resolved BEFORE the target is opened, so a foreign-session refusal can never be
    // absorbed by the open site's AccessDenied class (§3 item 6); each refusal edge below
    // carries exactly one precondition.
    let current_session = match api.session(self_pid) {
        Ok(session) => session,
        Err(outcome) => return outcome,
    };
    let target_session = match api.session(target.pid) {
        Ok(session) => session,
        Err(outcome) => return outcome,
    };
    if target_session != current_session {
        return ActionOutcome::CrossSession;
    }

    let process = match api.open(target.pid) {
        Ok(process) => process,
        Err(outcome) => return outcome,
    };
    if !api
        .process_identity(&process)
        .is_ok_and(|(creation, path)| {
            creation == target.creation_time && normalize_path(&path) == target.image_path
        })
    {
        return ActionOutcome::IdentityMismatch;
    }
    match api.window(target.hwnd) {
        Ok(current) if validate_window(&current) == ActionOutcome::Success => {}
        Ok(current) => return validate_window(&current),
        Err(outcome) => return outcome,
    }
    match api.probe(target.hwnd, cfg.hang_probe_timeout) {
        ProbeResult::Responsive => return ActionOutcome::Recovered,
        ProbeResult::Indeterminate => return ActionOutcome::Refused,
        ProbeResult::Timeout => {}
    }
    match api.window(target.hwnd) {
        Ok(current) if validate_window(&current) == ActionOutcome::Success => {}
        Ok(current) => return validate_window(&current),
        Err(outcome) => return outcome,
    }
    if let Err(outcome) = api.terminate(&process) {
        return outcome;
    }
    match api.wait(&process, Duration::from_secs(5)) {
        ActionWait::Signaled => {
            // TC-3 (P2 §2 TC-3): classification continues past `Signaled` — the exit
            // code is read off the handle this action already opened, and a read
            // failure is NAMED (`exit-code-unavailable`) while the `Success` class
            // stays exactly as shipped. The bounded wait above remains the only
            // completion mechanism (§3 item 8).
            *exit = match api.exit_code(&process) {
                Ok(code) => ExitInfo::Captured(code),
                Err(()) => ExitInfo::Unavailable,
            };
            ActionOutcome::Success
        }
        ActionWait::Timeout => ActionOutcome::WaitTimeout,
        ActionWait::Failed => ActionOutcome::TerminateFailure,
    }
}

unsafe extern "system" fn harness_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if message == WM_NULL {
        std::thread::sleep(Duration::from_secs(1));
        return windows::Win32::Foundation::LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

pub fn run_harness(duration: Duration) -> i32 {
    let current = match std::env::current_exe() {
        Ok(current) => current,
        Err(_) => {
            eprintln!("hang harness failed: current executable unavailable");
            return 1;
        }
    };
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("LCDSirPlus.exe"))
    {
        return run_owned_harness(&current, duration);
    }
    match create_harness_window(duration) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            1
        }
    }
}

fn run_owned_harness(source: &Path, duration: Duration) -> i32 {
    let mut cleanup = match spawn_renamed_harness(source, duration, false) {
        Ok(cleanup) => cleanup,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            return 1;
        }
    };
    let status = match wait_harness(&mut cleanup, duration + Duration::from_secs(2)) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("hang harness failed: {error}");
            return 1;
        }
    };
    if let Err(error) = cleanup.remove_tree() {
        eprintln!("hang harness failed: temporary harness cleanup failed: {error}");
        return 1;
    }
    status.code().unwrap_or(1)
}

fn create_harness_window(duration: Duration) -> Result<(), &'static str> {
    let pid = unsafe { GetCurrentProcessId() };
    let class = format!("LCDSIRPLUS_HANG_HARNESS_{pid}\0");
    let title = format!("LCDSirPlus Disposable Hang Harness {pid}\0");
    let class_wide: Vec<u16> = class.encode_utf16().collect();
    let title_wide: Vec<u16> = title.encode_utf16().collect();
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|_| "module handle unavailable")?;
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(harness_wndproc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_wide.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&window_class) == 0 {
            return Err("window class registration failed");
        }
        let hwnd = CreateWindowExW(
            Default::default(),
            PCWSTR(class_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            160,
            None,
            None,
            instance,
            None,
        )
        .map_err(|_| "window creation failed")?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
    }
    println!("HANG-HARNESS READY pid={pid}");
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(TICK);
    }
    Ok(())
}

pub fn run_smoke() -> i32 {
    match smoke() {
        Ok(target) => {
            println!(
                "HANG-DETECTOR SMOKE OK pid={} process={}",
                target.pid, target.process_name
            );
            0
        }
        Err(error) => {
            eprintln!("HANG-DETECTOR SMOKE FAILED: {error}");
            1
        }
    }
}

struct HarnessCleanup {
    child: Option<Child>,
    root: PathBuf,
}

impl HarnessCleanup {
    fn remove_tree(&mut self) -> std::io::Result<()> {
        if self.root.as_os_str().is_empty() {
            return Ok(());
        }
        std::fs::remove_dir_all(&self.root)?;
        self.root.clear();
        Ok(())
    }
}

impl Drop for HarnessCleanup {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                // This handle is created only by a smoke; normal application code cannot reach it.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = self.remove_tree();
    }
}

fn spawn_renamed_harness(
    source: &Path,
    duration: Duration,
    quiet: bool,
) -> Result<HarnessCleanup, String> {
    let root = std::env::temp_dir().join(format!(
        "lcdsirplus-hang-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir(&root).map_err(|_| "temporary harness directory creation failed")?;
    let mut cleanup = HarnessCleanup { child: None, root };
    let harness = cleanup.root.join("LCDSirPlusHangHarness.exe");
    if let Err(error) = std::fs::copy(source, &harness) {
        return Err(format!("temporary harness copy failed: {error}"));
    }
    let mut command = std::process::Command::new(&harness);
    command.args([
        "--hang-test-harness",
        "--duration-secs",
        &duration.as_secs().to_string(),
    ]);
    if quiet {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    cleanup.child = Some(
        command
            .spawn()
            .map_err(|error| format!("temporary harness start failed: {error}"))?,
    );
    Ok(cleanup)
}

fn spawn_harness(duration_secs: u64) -> Result<HarnessCleanup, String> {
    spawn_renamed_harness(&smoke_source()?, Duration::from_secs(duration_secs), true)
}

fn detect_harness(pid: u32) -> Result<HungTarget, String> {
    let started = Instant::now();
    let mut tracker = Tracker::default();
    let mut detected = None;
    let mut last_error = None;
    while started.elapsed() < Duration::from_secs(5) {
        match enumerate_and_probe(Duration::from_millis(100), &[], Some(pid)) {
            Ok(cycle) => {
                let targets = tracker.update(
                    started.elapsed(),
                    cycle.observations,
                    2,
                    Duration::from_millis(300),
                );
                if let Some(target) = targets.into_iter().next() {
                    detected = Some(target);
                    break;
                }
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    detected.ok_or_else(|| {
        last_error
            .unwrap_or("purpose-built hung window was not confirmed")
            .into()
    })
}

#[cfg(test)]
fn detect_harness_with_policy(pid: u32, policy: &Policy) -> Result<HungTarget, String> {
    let started = Instant::now();
    let mut tracker = Tracker::default();
    let mut cadence = Cadence::default();
    while started.elapsed() < Duration::from_secs(15) {
        if !cadence.poll_due(policy, started.elapsed()) {
            std::thread::sleep(TICK);
            continue;
        }
        let cycle =
            enumerate_and_probe(policy.timeout, &policy.ignore, None).map_err(str::to_string)?;
        if let Some(target) = tracker
            .update(
                started.elapsed(),
                cycle.observations,
                policy.failures,
                policy.minimum,
            )
            .into_iter()
            .find(|target| target.pid == pid)
        {
            return Ok(target);
        }
        std::thread::sleep(TICK);
    }
    Err("purpose-built hung window was not confirmed with shipped policy".into())
}

fn wait_harness(cleanup: &mut HarnessCleanup, timeout: Duration) -> Result<ExitStatus, String> {
    let exit_deadline = Instant::now() + timeout;
    loop {
        match cleanup.child.as_mut().unwrap().try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < exit_deadline => std::thread::sleep(TICK),
            Ok(None) => return Err("temporary harness did not exit within its bound".into()),
            Err(_) => return Err("temporary harness status unavailable".into()),
        }
    }
}

fn smoke() -> Result<HungTarget, String> {
    use std::io::BufRead;

    let source = smoke_source()?;
    if source.file_name().and_then(|name| name.to_str()) != Some("LCDSirPlus.exe") {
        return Err("public harness executable must be named LCDSirPlus.exe".into());
    }
    let owner = std::process::Command::new(&source)
        .args(["--hang-test-harness", "--duration-secs", "6"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("public harness start failed: {error}"))?;
    let mut owner = HarnessCleanup {
        child: Some(owner),
        root: PathBuf::new(),
    };
    let stdout = owner.child.as_mut().unwrap().stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = ready_tx.send(result);
    });
    let observed = (|| -> Result<(HungTarget, PathBuf), String> {
        let line = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "public harness child did not become ready")?
            .map_err(|_| "public harness output unavailable")?;
        let pid = line
            .trim()
            .strip_prefix("HANG-HARNESS READY pid=")
            .and_then(|pid| pid.parse::<u32>().ok())
            .ok_or("public harness did not report its renamed child")?;
        let target = detect_harness(pid)?;
        if !target
            .process_name
            .eq_ignore_ascii_case("LCDSirPlusHangHarness.exe")
        {
            return Err("public harness child retained the ignored production basename".into());
        }
        let owned_root = Path::new(&target.image_path)
            .parent()
            .ok_or("public harness child path unavailable")?
            .to_path_buf();
        Ok((target, owned_root))
    })();
    let status = wait_harness(&mut owner, Duration::from_secs(8))?;
    if !status.success() {
        return Err(format!("public harness owner exited with {status}"));
    }
    let (target, owned_root) = observed?;
    if owned_root.exists() {
        return Err("public harness owner did not clean its temporary tree".into());
    }
    Ok(target)
}

pub fn run_action_smoke(negative: bool) -> i32 {
    match action_smoke(negative) {
        Ok(pid) => {
            if negative {
                println!("HANG-ACTION NEGATIVE SMOKE OK survived_checks_pid={pid}");
            } else {
                println!("HANG-ACTION POSITIVE SMOKE OK killed_pid={pid}");
            }
            0
        }
        Err(error) => {
            eprintln!("HANG-ACTION SMOKE FAILED: {error}");
            1
        }
    }
}

fn action_smoke(negative: bool) -> Result<u32, String> {
    let mut cleanup = spawn_harness(20)?;
    let pid = cleanup.child.as_ref().unwrap().id();
    let target = detect_harness(pid)?;
    let cfg = Config {
        hang_enabled: true,
        hang_hold: Duration::from_secs(1),
        hang_probe_timeout: Duration::from_millis(100),
        ..Config::default()
    };
    let started = Instant::now();
    let mut hold = HoldState::default();
    hold.event(
        Event {
            index: 2,
            down: true,
            backward: false,
            canceled: false,
            at: started,
            source: "action-smoke",
            stamp: 0,
        },
        &cfg,
        std::slice::from_ref(&target),
        true,
    );

    if negative {
        let early = hold.event(
            Event {
                index: 2,
                down: false,
                backward: false,
                canceled: false,
                at: started + Duration::from_millis(100),
                source: "action-smoke",
                stamp: 0,
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        );
        if !matches!(early, HoldCommand::Audit(_))
            || !matches!(cleanup.child.as_mut().unwrap().try_wait(), Ok(None))
        {
            return Err("early release did not leave owned child alive".into());
        }
        let restarted = started + Duration::from_secs(1);
        hold.event(
            Event {
                index: 2,
                down: true,
                backward: false,
                canceled: false,
                at: restarted,
                source: "action-smoke",
                stamp: 0,
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        );
        let mut changed = target.clone();
        changed.creation_time += 1;
        if hold.reconcile(&cfg, &[changed], true).is_none() {
            return Err("wrong identity did not cancel hold".into());
        }
        let release = hold.event(
            Event {
                index: 2,
                down: false,
                backward: false,
                canceled: false,
                at: restarted + cfg.hang_hold,
                source: "action-smoke",
                stamp: 0,
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        );
        if !matches!(release, HoldCommand::Audit(_))
            || !matches!(cleanup.child.as_mut().unwrap().try_wait(), Ok(None))
        {
            return Err("wrong identity release did not leave owned child alive".into());
        }
        return Ok(pid);
    }

    if hold
        .reconcile(&cfg, std::slice::from_ref(&target), true)
        .is_some()
    {
        return Err("valid action hold canceled unexpectedly".into());
    }
    let command = hold.tick(
        started + cfg.hang_hold,
        &cfg,
        std::slice::from_ref(&target),
        true,
    );
    let HoldCommand::Terminate(bound) = command else {
        return Err("qualified hold did not request termination".into());
    };
    let report = terminate_bound_target(&bound, &cfg);
    if report.outcome != ActionOutcome::Success {
        return Err(format!(
            "owned child termination outcome={}",
            report.outcome.label()
        ));
    }
    if !matches!(
        hold.event(
            Event {
                index: 2,
                down: false,
                backward: false,
                canceled: false,
                at: started + cfg.hang_hold,
                source: "action-smoke",
                stamp: 0,
            },
            &cfg,
            std::slice::from_ref(&target),
            true,
        ),
        HoldCommand::None
    ) {
        return Err("release after action was not inert".into());
    }
    wait_harness(&mut cleanup, Duration::from_secs(1))?;
    cleanup
        .remove_tree()
        .map_err(|_| "temporary harness cleanup failed")?;
    Ok(pid)
}

fn smoke_source() -> Result<std::path::PathBuf, String> {
    let current = std::env::current_exe().map_err(|_| "current executable unavailable")?;
    if is_cargo_test_binary(&current) {
        let release_binary = current
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join("LCDSirPlus.exe"))
            .ok_or("release harness executable unavailable")?;
        if release_binary.is_file() {
            return Ok(release_binary);
        }
        return Err("build the release executable before running the ignored harness smoke".into());
    }
    Ok(current)
}

fn is_cargo_test_binary(current: &Path) -> bool {
    current
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().starts_with("lcdsirplus-"))
        && current
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("deps")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(hwnd: usize, pid: u32, creation: u64, path: &str) -> HungTarget {
        HungTarget {
            hwnd,
            pid,
            creation_time: creation,
            image_path: normalize_path(path),
            process_name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            title: format!("target {pid}"),
            ..Default::default()
        }
    }

    /// TC-1: a target as the detector publishes it — stated publish generation and
    /// a monotonic publish time (fresh by the bind bound).
    fn published(mut target: HungTarget, generation: u64) -> HungTarget {
        target.publish_generation = generation;
        target.publish_time = Some(Instant::now());
        target
    }

    fn failed(target: HungTarget) -> Observation {
        Observation {
            target,
            responsive: false,
        }
    }

    #[test]
    fn tracker_requires_failures_and_duration_then_resets_on_recovery() {
        let mut tracker = Tracker::default();
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        assert!(tracker
            .update(
                Duration::ZERO,
                vec![failed(item.clone())],
                2,
                Duration::from_secs(1)
            )
            .is_empty());
        assert!(tracker
            .update(
                Duration::from_secs(1),
                vec![failed(item.clone())],
                3,
                Duration::from_secs(1)
            )
            .is_empty());
        assert_eq!(
            tracker.update(
                Duration::from_secs(2),
                vec![failed(item.clone())],
                3,
                Duration::from_secs(1)
            ),
            vec![item.clone()]
        );
        assert!(tracker
            .update(
                Duration::from_secs(3),
                vec![Observation {
                    target: item,
                    responsive: true,
                }],
                3,
                Duration::from_secs(1),
            )
            .is_empty());
        assert!(tracker.records.is_empty());
    }

    #[test]
    fn tracker_removes_absent_and_resets_replaced_identity() {
        let mut tracker = Tracker::default();
        let old = target(1, 10, 20, r"C:\Games\game.exe");
        tracker.update(Duration::ZERO, vec![failed(old)], 2, Duration::ZERO);
        assert!(tracker
            .update(Duration::from_secs(1), Vec::new(), 2, Duration::ZERO)
            .is_empty());
        assert!(tracker.records.is_empty());

        let first = target(1, 10, 20, r"C:\Games\game.exe");
        let replacement = target(1, 10, 21, r"C:\Games\game.exe");
        tracker.update(Duration::ZERO, vec![failed(first)], 2, Duration::ZERO);
        assert!(tracker
            .update(
                Duration::from_secs(1),
                vec![failed(replacement)],
                2,
                Duration::ZERO
            )
            .is_empty());

        let moved = target(1, 10, 21, r"D:\Games\game.exe");
        assert!(tracker
            .update(
                Duration::from_secs(2),
                vec![failed(moved)],
                2,
                Duration::ZERO
            )
            .is_empty());
    }

    #[test]
    fn tracker_orders_multiple_targets_deterministically() {
        let mut tracker = Tracker::default();
        let b = target(2, 20, 30, r"C:\Games\b.exe");
        let a = target(1, 10, 20, r"C:\Games\a.exe");
        tracker.update(
            Duration::ZERO,
            vec![failed(b.clone()), failed(a.clone())],
            2,
            Duration::ZERO,
        );
        assert_eq!(
            tracker.update(
                Duration::from_secs(1),
                vec![failed(b), failed(a.clone())],
                2,
                Duration::ZERO,
            )[0],
            a
        );
    }

    #[test]
    fn ignore_and_windows_directory_policy_is_case_insensitive() {
        let ignored = ignored_names(&["MyGame.EXE".into()]);
        assert!(excluded(
            "mygame.exe",
            r"D:\Games\MyGame.exe",
            &ignored,
            r"C:\Windows"
        ));
        assert!(excluded(
            "tool.exe",
            r"C:\WINDOWS\System32\tool.exe",
            &ignored,
            r"C:\Windows"
        ));
        assert!(!excluded(
            "tool.exe",
            r"C:\WindowsOld\tool.exe",
            &ignored,
            r"C:\Windows"
        ));
    }

    #[test]
    fn path_and_display_text_normalization_is_bounded() {
        assert_eq!(
            normalize_path(r"\\?\C:/Games/Test.EXE\"),
            r"c:\games\test.exe"
        );
        assert_eq!(
            safe_display_text("  title\r\n with   space "),
            "title with space"
        );
        assert_eq!(
            safe_display_text(&"x".repeat(300)).chars().count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn probe_classification_counts_only_documented_timeout() {
        assert_eq!(
            classify_probe_result(false, ERROR_TIMEOUT),
            ProbeResult::Timeout
        );
        assert_eq!(
            classify_probe_result(false, ERROR_SUCCESS),
            ProbeResult::Indeterminate
        );
        assert_eq!(
            classify_probe_result(false, WIN32_ERROR(5)),
            ProbeResult::Indeterminate
        );
        assert_eq!(
            classify_probe_result(true, ERROR_TIMEOUT),
            ProbeResult::Responsive
        );
    }

    /// TC-4 helper: run one synthetic publish cycle through the real counting seam.
    /// A `Timeout` is given a confirmed identity (the arm the eligibility math
    /// requires); every other class runs with the identity cross-check unconfirmed.
    /// No Win32 handle, window, probe, process query or termination is involved.
    fn probe_cycle(results: &[ProbeResult], target: &HungTarget) -> ProbeCycle {
        let mut cycle = ProbeCycle::default();
        for &result in results {
            record_probe_result(
                result,
                result == ProbeResult::Timeout,
                target.clone(),
                &mut cycle,
            );
        }
        cycle
    }

    /// TC-4 helper: the publication the worker builds from one cycle.
    fn publication(cycle: &ProbeCycle, published: &mut usize, generation: u64) -> Update {
        let mut tracker = Tracker::default();
        Update {
            generation,
            targets: stamp_publish(
                tracker.update(
                    Duration::ZERO,
                    cycle.observations.clone(),
                    1,
                    Duration::ZERO,
                ),
                generation,
            ),
            available: true,
            error: None,
            probe_indeterminate: publish_indeterminate(cycle, published),
        }
    }

    #[test]
    fn not_evaluable_probes_publish_a_named_loss_cause_and_no_target() {
        // TC-4 acceptance (1): injecting `Indeterminate` probe results publishes the
        // named `probe-indeterminate` reason with ZERO targets.
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let cycle = probe_cycle(&[ProbeResult::Indeterminate; 3], &item);
        assert_eq!(cycle.indeterminate, 3, "one count per window per cycle");
        assert!(
            cycle.observations.is_empty(),
            "a not-evaluable probe never enters eligibility"
        );

        let mut published = 0usize;
        let update = publication(&cycle, &mut published, 1);
        assert_eq!(update.targets.len(), 0);
        assert_eq!(update.probe_indeterminate, 3);
        assert_eq!(
            update.availability_reason().as_deref(),
            Some("probe-indeterminate-3")
        );
        assert!(update.error.is_none(), "a loss cause is not a hard failure");
    }

    #[test]
    fn indeterminate_never_counts_toward_a_target() {
        // TC-4 ban: the eligibility math is only documented `Timeout` + confirmed
        // identity (the class pinned by
        // `probe_classification_counts_only_documented_timeout`, which stays byte-identical
        // and green). A window that only ever answers `Indeterminate` gets no record, no
        // failure credit and no target, in a cycle that also contains a real timeout.
        let hung = target(1, 10, 20, r"C:\Games\game.exe");
        let lost = target(2, 20, 30, r"C:\Games\lost.exe");
        let fine = target(3, 30, 40, r"C:\Games\fine.exe");
        let mut cycle = ProbeCycle::default();
        record_probe_result(ProbeResult::Timeout, true, hung.clone(), &mut cycle);
        for _ in 0..3 {
            record_probe_result(ProbeResult::Indeterminate, false, lost.clone(), &mut cycle);
        }
        record_probe_result(ProbeResult::Responsive, false, fine.clone(), &mut cycle);
        assert_eq!(cycle.indeterminate, 3);

        let mut published = 0usize;
        let update = publication(&cycle, &mut published, 1);
        assert_eq!(
            update.targets,
            vec![hung],
            "only the documented timeout became a target"
        );
        assert_eq!(update.probe_indeterminate, 3);
        assert_eq!(
            update.availability_reason().as_deref(),
            Some("probe-indeterminate-3")
        );

        // The indeterminate identity never reaches the `Tracker` records at all.
        let mut tracker = Tracker::default();
        tracker.update(Duration::ZERO, cycle.observations, 1, Duration::ZERO);
        assert!(!tracker.records.contains_key(&Identity::from(&lost)));
        assert!(tracker
            .records
            .contains_key(&Identity::from(&update.targets[0])));
        assert!(!tracker.records.contains_key(&Identity::from(&fine)));
    }

    #[test]
    fn the_not_evaluable_count_resets_every_publish_cycle() {
        // TC-4 acceptance (5): counters reset each cycle, so a stale count can never
        // leak into a new publication (including an aborted cycle's zero-target one).
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let mut published = 0usize;

        let lost = probe_cycle(&[ProbeResult::Indeterminate; 3], &item);
        assert_eq!(publish_indeterminate(&lost, &mut published), 3);
        assert_eq!(published, 3);

        // The next cycle saw no not-evaluable probe: it publishes 0 / no token.
        let quiet = probe_cycle(&[ProbeResult::Responsive], &item);
        assert_eq!(publish_indeterminate(&quiet, &mut published), 0);
        assert_eq!(published, 0);
        let update = publication(&quiet, &mut published, 2);
        assert_eq!(update.probe_indeterminate, 0);
        assert_eq!(
            update.availability_reason(),
            None,
            "the previous cycle's count did not leak"
        );

        // An aborted cycle (budget/limit/enumeration failure) publishes no count either.
        assert_eq!(
            publish_indeterminate(&ProbeCycle::default(), &mut published),
            0
        );
    }

    #[test]
    fn the_loss_cause_token_is_distinct_from_every_other_publication_signal() {
        // TC-4 anti-masking: "no timeout observed" (no token) and "N not-evaluable
        // probes" (token) are distinguishable, and the token is not a hard-failure class.
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let mut published = 0usize;

        let quiet = publication(&probe_cycle(&[], &item), &mut published, 1);
        assert_eq!(
            quiet.availability_reason(),
            None,
            "a cycle that observed nothing not-evaluable carries no token"
        );

        let lost = publication(
            &probe_cycle(&[ProbeResult::Indeterminate; 2], &item),
            &mut published,
            2,
        );
        assert_eq!(
            lost.availability_reason().as_deref(),
            Some("probe-indeterminate-2")
        );
        assert!(
            lost.targets.is_empty(),
            "the named loss cause never publishes a target"
        );

        let failed = Update {
            generation: 3,
            error: Some("hung-window poll budget exceeded"),
            ..Default::default()
        };
        assert_eq!(
            failed.availability_reason(),
            None,
            "a hard failure keeps its own named class"
        );
        for message in [
            "window enumeration failed",
            "hung-window poll budget exceeded",
            "hung-window enumeration limit exceeded",
        ] {
            assert!(
                !message.contains(PROBE_INDETERMINATE),
                "{message} must not absorb the loss-cause token"
            );
        }
    }

    #[test]
    fn the_not_evaluable_count_is_bounded_by_the_enumeration_cap() {
        // TC-4 boundedness: one count per visited window, and the existing enumeration
        // cap bounds the cycle. `enumerate_callback` aborts the WHOLE publication when a
        // cycle visits more than `MAX_WINDOWS` windows (`limit_exceeded` -> `Err` -> ZERO
        // targets), so the counter of a PUBLISHED cycle is at most `MAX_WINDOWS` — the
        // same derivation as `MAX_TERMINAL_MARKS` (no new budget invented).
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let cycle = probe_cycle(&[ProbeResult::Indeterminate; MAX_WINDOWS], &item);
        assert_eq!(cycle.indeterminate, MAX_WINDOWS);
        assert_eq!(cycle.observations.len(), 0);
        assert_eq!(
            probe_indeterminate_reason(cycle.indeterminate)
                .unwrap()
                .strip_prefix(PROBE_INDETERMINATE)
                .unwrap(),
            format!("-{MAX_WINDOWS}")
        );
    }

    #[test]
    fn post_probe_identity_requires_every_captured_field() {
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        assert!(identity_matches(&expected, 1, 10, 20, r"C:\GAMES\GAME.EXE"));
        assert!(!identity_matches(
            &expected,
            2,
            10,
            20,
            &expected.image_path
        ));
        assert!(!identity_matches(
            &expected,
            1,
            11,
            20,
            &expected.image_path
        ));
        assert!(!identity_matches(
            &expected,
            1,
            10,
            21,
            &expected.image_path
        ));
        assert!(!identity_matches(
            &expected,
            1,
            10,
            20,
            r"D:\Games\game.exe"
        ));
    }

    #[test]
    fn disabled_policy_clears_tracker_and_cadence_reloads() {
        let mut tracker = Tracker::default();
        tracker.update(
            Duration::ZERO,
            vec![failed(target(1, 2, 3, r"C:\Games\game.exe"))],
            1,
            Duration::ZERO,
        );
        tracker.clear();
        assert!(tracker.records.is_empty());

        let mut cadence = Cadence::default();
        let mut cfg = Config {
            hang_probe_interval: Duration::from_secs(10),
            ..Config::default()
        };
        cfg.hang_enabled = true;
        let mut policy = Policy::from(&cfg);
        assert!(cadence.poll_due(&policy, Duration::ZERO));
        assert!(!cadence.poll_due(&policy, Duration::from_secs(5)));
        cfg.hang_probe_interval = Duration::from_secs(2);
        policy = Policy::from(&cfg);
        assert!(cadence.poll_due(&policy, Duration::from_secs(5)));
        cfg.safe_mode = true;
        policy = Policy::from(&cfg);
        assert!(!cadence.poll_due(&policy, Duration::from_secs(6)));
        cfg.safe_mode = false;
        policy = Policy::from(&cfg);
        assert!(cadence.poll_due(&policy, Duration::from_secs(6)));
        cfg.hang_enabled = false;
        policy = Policy::from(&cfg);
        assert!(!cadence.poll_due(&policy, Duration::from_secs(7)));
    }

    #[test]
    fn changed_or_disabled_policy_discards_poll_and_resets_evidence() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let started = Policy::from(&cfg);
        assert!(poll_policy_current(&started, &started));

        let mut changed = started.clone();
        changed.timeout += Duration::from_millis(1);
        assert!(!poll_policy_current(&started, &changed));

        let mut disabled = started.clone();
        disabled.enabled = false;
        assert!(!poll_policy_current(&started, &disabled));

        let mut tracker = Tracker::default();
        tracker.update(
            Duration::ZERO,
            vec![failed(target(1, 2, 3, r"C:\Games\game.exe"))],
            1,
            Duration::ZERO,
        );
        let mut cadence = Cadence {
            last_attempt: Some(Duration::ZERO),
        };
        let mut current = Some(started);
        assert!(reset_on_policy_change(
            &mut current,
            &changed,
            &mut tracker,
            &mut cadence
        ));
        assert!(tracker.records.is_empty());
        assert_eq!(cadence.last_attempt, None);
    }

    #[test]
    fn policy_compares_every_runtime_probe_setting() {
        let base = Policy::from(&Config::default());
        let mut values = Vec::new();
        let mut value = base.clone();
        value.enabled = !value.enabled;
        values.push(value);
        let mut value = base.clone();
        value.safe_mode = !value.safe_mode;
        values.push(value);
        let mut value = base.clone();
        value.interval += Duration::from_millis(1);
        values.push(value);
        let mut value = base.clone();
        value.timeout += Duration::from_millis(1);
        values.push(value);
        let mut value = base.clone();
        value.failures += 1;
        values.push(value);
        let mut value = base.clone();
        value.minimum += Duration::from_millis(1);
        values.push(value);
        let mut value = base.clone();
        value.ignore.push("another.exe".into());
        values.push(value);
        let mut value = base.clone();
        value.button += 1;
        values.push(value);
        let mut value = base.clone();
        value.hold += Duration::from_millis(1);
        values.push(value);
        assert!(values.into_iter().all(|value| value != base));
    }

    fn button(index: usize, down: bool, at: Instant, source: &'static str) -> Event {
        Event {
            index,
            down,
            backward: false,
            canceled: false,
            at,
            source,
            stamp: 0,
        }
    }

    /// EC-7: the same event shape with an explicit transition stamp.
    fn button_stamped(
        index: usize,
        down: bool,
        at: Instant,
        source: &'static str,
        stamp: u64,
    ) -> Event {
        Event {
            index,
            down,
            backward: false,
            canceled: false,
            at,
            source,
            stamp,
        }
    }

    #[test]
    fn hold_requires_target_at_down_and_threshold_tick_acts_once() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let item = published(target(1, 10, 20, r"C:\Games\game.exe"), 1);
        let started = Instant::now();
        let mut hold = HoldState::default();
        assert!(matches!(
            hold.event(button(2, true, started, "hid"), &cfg, &[], true),
            HoldCommand::None
        ));
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));

        hold.event(
            button(2, true, started, "hid"),
            &cfg,
            std::slice::from_ref(&item),
            true,
        );
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));

        hold.event(
            button(2, true, started, "hid"),
            &cfg,
            std::slice::from_ref(&item),
            true,
        );
        assert_eq!(hold.progress(started + cfg.hang_hold, &cfg), 1.0);
        assert!(matches!(
            hold.tick(
                started + cfg.hang_hold,
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Terminate(bound) if bound == item
        ));
        assert_eq!(hold.progress(started + cfg.hang_hold, &cfg), 0.0);
        assert!(matches!(
            hold.tick(
                started + cfg.hang_hold + Duration::from_millis(1),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));

        let mut stale = HoldState::default();
        stale.event(
            button(2, true, started, "hid"),
            &cfg,
            std::slice::from_ref(&item),
            true,
        );
        assert!(matches!(
            stale.tick(
                started + maximum_press_duration(cfg.hang_hold) + Duration::from_millis(1),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Audit(Audit {
                outcome: "refused-stale-hold",
                ..
            })
        ));
        assert!(matches!(
            stale.tick(
                started + maximum_press_duration(cfg.hang_hold) + Duration::from_millis(2),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));
    }

    #[test]
    fn safe_mode_short_press_cycles_without_binding_a_target() {
        let cfg = Config {
            safe_mode: true,
            ..Config::default()
        };
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let started = Instant::now();
        let mut hold = HoldState::default();
        assert!(matches!(
            hold.event(
                button(2, true, started, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::None
        ));
        assert!(hold.presses[2]
            .as_ref()
            .is_some_and(|press| press.bound.is_none()));
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Cycle {
                index: 2,
                backward: false
            }
        ));
    }

    #[test]
    fn early_release_preserves_go_detail_and_duplicate_down_restarts() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let items = vec![
            target(1, 10, 20, r"C:\Games\a.exe"),
            target(2, 11, 21, r"C:\Games\b.exe"),
        ];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.event(
                button(2, false, started + Duration::from_millis(100), "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::Audit(Audit {
                outcome: "canceled-early-release",
                ..
            })
        ));
        assert!(hold.hung_detail);

        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        let restarted = started + cfg.hang_hold;
        hold.event(button(2, true, restarted, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.event(
                button(2, false, restarted + Duration::from_millis(100), "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::Audit(Audit {
                outcome: "canceled-early-release",
                ..
            })
        ));
        assert_eq!(hold.hung_index, 1);
        assert!(!hold.hung_detail);
    }

    #[test]
    fn hold_owner_and_monotonic_cancellation_are_exact() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let item = published(target(1, 10, 20, r"C:\Games\game.exe"), 1);
        let items = vec![item.clone()];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        hold.event(
            button(2, true, started + cfg.hang_hold, "preview"),
            &cfg,
            &items,
            true,
        );
        hold.event(
            button(1, true, started + cfg.hang_hold, "hid"),
            &cfg,
            &items,
            true,
        );
        assert_eq!(hold.presses.iter().flatten().count(), 1);
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "preview"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::None
        ));
        assert!(hold.presses[2].is_some());

        assert!(hold.reconcile(&cfg, &[], true).is_some());
        assert!(hold.reconcile(&cfg, &items, true).is_none());
        hold.event(
            button(2, true, started + cfg.hang_hold, "hid"),
            &cfg,
            &items,
            true,
        );
        assert!(hold.presses[2].as_ref().unwrap().canceled);
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::Audit(_)
        ));
    }

    #[test]
    fn device_policy_provider_and_selection_changes_cancel_until_release() {
        let base = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let a = published(target(1, 10, 20, r"C:\Games\a.exe"), 1);
        let b = published(target(2, 11, 21, r"C:\Games\b.exe"), 1);
        let started = Instant::now();
        for mutation in 0..7 {
            let mut hold = HoldState::default();
            hold.event(
                button(2, true, started, "hid"),
                &base,
                std::slice::from_ref(&a),
                true,
            );
            let mut cfg = base.clone();
            let mut targets = vec![a.clone()];
            let mut available = true;
            match mutation {
                0 => cfg.safe_mode = true,
                1 => cfg.hang_enabled = false,
                2 => cfg.hang_button = 4,
                3 => cfg.hang_hold += Duration::from_millis(1),
                4 => cfg.hang_ignore.push("other.exe".into()),
                5 => available = false,
                _ => targets = vec![b.clone(), a.clone()],
            }
            assert!(hold.reconcile(&cfg, &targets, available).is_some());
            assert_eq!(hold.progress(started + base.hang_hold, &base), 0.0);
            assert!(matches!(
                hold.tick(started + base.hang_hold, &cfg, &targets, available),
                HoldCommand::None
            ));
        }

        let mut hold = HoldState::default();
        hold.event(
            button(2, true, started, "hid"),
            &base,
            std::slice::from_ref(&a),
            true,
        );
        let mut canceled = button(2, false, started + base.hang_hold, "hid");
        canceled.canceled = true;
        assert!(matches!(
            hold.event(canceled, &base, std::slice::from_ref(&a), true),
            HoldCommand::Audit(Audit {
                outcome: "canceled-device-loss",
                ..
            })
        ));

        let mut hold = HoldState::default();
        hold.event(
            button(2, true, started, "hid"),
            &base,
            std::slice::from_ref(&a),
            true,
        );
        assert!(hold.cancel_reload().is_some());
        assert_eq!(hold.progress(started + base.hang_hold, &base), 0.0);
    }

    #[test]
    fn resolved_proc_hang_owner_binds_each_slot_and_movement_cancels() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let item = published(target(1, 10, 20, r"C:\Games\game.exe"), 1);
        for owner in 0..4 {
            let started = Instant::now();
            let mut hold = HoldState::default();
            hold.set_owner(Some(owner));
            hold.event(
                button(owner, true, started, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true,
            );
            assert!(hold.presses[owner]
                .as_ref()
                .is_some_and(|press| press.bound.is_some()));
            assert!(matches!(
                hold.tick(started + cfg.hang_hold, &cfg, std::slice::from_ref(&item), true),
                HoldCommand::Terminate(ref bound) if bound == &item
            ));
            assert!(matches!(
                hold.event(
                    button(owner, false, started + cfg.hang_hold, "hid"),
                    &cfg,
                    std::slice::from_ref(&item),
                    true
                ),
                HoldCommand::None
            ));
        }

        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(0));
        hold.event(
            button(0, true, started, "hid"),
            &cfg,
            std::slice::from_ref(&item),
            true,
        );
        assert!(hold.set_owner(Some(3)).is_some());
        assert_eq!(hold.progress(started + cfg.hang_hold, &cfg), 0.0);
        assert!(matches!(
            hold.event(
                button(0, false, started + cfg.hang_hold, "hid"),
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Audit(_)
        ));
    }

    #[test]
    fn stale_publication_refuses_the_bind_as_canceled_continuity() {
        // TC-1 acceptance (1): a publication older than the probe interval in force
        // at bind time plus PUBLISH_AGE_TOLERANCE cancels the press with the audited
        // canceled-continuity outcome — at the continuity pass and at the action
        // tick — and no Terminate command is emitted.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let mut stale = target(1, 10, 20, r"C:\Games\game.exe");
        stale.publish_generation = 1;
        stale.publish_time = Some(
            Instant::now()
                - (cfg.hang_probe_interval + PUBLISH_AGE_TOLERANCE + Duration::from_secs(1)),
        );
        let items = vec![stale.clone()];
        let started = Instant::now();

        let mut hold = HoldState::default();
        hold.set_owner(Some(2));
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(hold.presses[2]
            .as_ref()
            .is_some_and(|press| press.bound.is_some()));
        let audit = hold
            .reconcile(&cfg, &items, true)
            .expect("the stale publication cancels the bind");
        assert_eq!(audit.outcome, "canceled-continuity");
        assert_eq!(audit.target, stale);
        assert_eq!(audit.target.publish_generation, 1);

        let mut ticked = HoldState::default();
        ticked.set_owner(Some(2));
        ticked.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            ticked.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Audit(Audit {
                outcome: "canceled-continuity",
                ..
            })
        ));
        assert!(!matches!(
            ticked.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Terminate(_)
        ));
        assert_eq!(
            ticked.progress(started + cfg.hang_hold, &cfg),
            0.0,
            "the refused press is cancelled, never armed for a second action"
        );
    }

    #[test]
    fn unstated_generation_publication_refuses_the_action_as_canceled_continuity() {
        // TC-1 action bound: a publication with a fresh publish time and no stated
        // (non-zero) publish generation is not a current generation, so the press
        // is refused. This isolates the generation clause from the age clause.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let mut unstated = target(1, 10, 20, r"C:\Games\game.exe");
        unstated.publish_generation = 0;
        unstated.publish_time = Some(Instant::now());
        let items = vec![unstated.clone()];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Audit(Audit {
                outcome: "canceled-continuity",
                ..
            })
        ));
        assert!(hold.presses[2]
            .as_ref()
            .is_some_and(|press| press.canceled && !press.resolved));
    }

    #[test]
    fn republished_identity_stays_bound_until_the_action() {
        // TC-1 must not fight the publish cadence: the same identity republished at
        // a newer generation (fresh stamp) is the current generation for a press
        // bound at an older one, so the hold still reaches its action.
        let cfg = Config {
            hang_enabled: true,
            hang_probe_interval: Duration::from_millis(250),
            ..Config::default()
        };
        let item = target(1, 10, 20, r"C:\Games\game.exe");
        let older = published(item.clone(), 1);
        let latest = published(item, 9);
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));
        hold.event(
            button(2, true, started, "hid"),
            &cfg,
            std::slice::from_ref(&older),
            true,
        );
        assert!(matches!(
            hold.tick(
                started + Duration::from_millis(200),
                &cfg,
                std::slice::from_ref(&older),
                true
            ),
            HoldCommand::None
        ));
        assert!(matches!(
            hold.tick(
                started + cfg.hang_hold,
                &cfg,
                std::slice::from_ref(&latest),
                true
            ),
            HoldCommand::Terminate(bound) if bound == older
        ));
    }

    #[test]
    fn published_targets_carry_their_publish_generation_and_time() {
        // TC-1 stamping at the publish boundary: every published target states its
        // generation and monotonic publish time; a bare target is unstamped, so the
        // new gates are fail-closed.
        let stamped = stamp_publish(vec![target(1, 10, 20, r"C:\Games\game.exe")], 7);
        assert_eq!(stamped[0].publish_generation, 7);
        let published_at = stamped[0].publish_time.expect("publish time");
        assert!(Instant::now() >= published_at);
        let bare = target(1, 10, 20, r"C:\Games\game.exe");
        assert_eq!(bare.publish_generation, 0);
        assert!(bare.publish_time.is_none());
    }

    #[test]
    fn ec7_same_stamp_reemission_binds_one_press_and_never_rearms_the_hold() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let item = published(target(1, 10, 20, r"C:\Games\game.exe"), 1);
        let items = vec![item.clone()];
        let started = Instant::now();
        // One physical press: the stamp of its down transition is recorded at bind.
        let mut hold = HoldState::default();
        assert!(matches!(
            hold.event(
                button_stamped(2, true, started, "hid", 7),
                &cfg,
                &items,
                true
            ),
            HoldCommand::None
        ));
        assert!(hold.presses[2]
            .as_ref()
            .is_some_and(|press| press.bound.is_some() && press.stamp == 7));
        // The same transition re-emitted late (same stamp) is not a second press and
        // does not move the bound press's start, so the terminate budget stays intact.
        assert!(matches!(
            hold.event(
                button_stamped(2, true, started + cfg.hang_hold, "hid", 7),
                &cfg,
                &items,
                true
            ),
            HoldCommand::None
        ));
        assert_eq!(
            hold.presses.iter().flatten().count(),
            1,
            "one bound press per physical press"
        );
        assert_eq!(
            hold.presses[2].as_ref().unwrap().started,
            started,
            "a same-stamp re-emission must not re-arm the hold"
        );
        assert!(matches!(
            hold.tick(
                started + cfg.hang_hold,
                &cfg,
                std::slice::from_ref(&item),
                true
            ),
            HoldCommand::Terminate(bound) if bound == item
        ));
        // The gate is stamp-specific: a distinct stamp is a new physical press, so the
        // shipped duplicate-down restart behavior is unchanged.
        let mut fresh = HoldState::default();
        fresh.event(
            button_stamped(2, true, started, "hid", 7),
            &cfg,
            &items,
            true,
        );
        let second = started + Duration::from_millis(200);
        fresh.event(
            button_stamped(2, true, second, "hid", 8),
            &cfg,
            &items,
            true,
        );
        assert_eq!(
            fresh.presses[2].as_ref().unwrap().started,
            second,
            "a distinct stamp re-binds as a new press"
        );
    }

    struct FakeActionApi {
        target: HungTarget,
        windows: Vec<Result<HungTarget, ActionOutcome>>,
        probes: Vec<ProbeResult>,
        self_session: Result<u32, ActionOutcome>,
        target_session: Result<u32, ActionOutcome>,
        open: Result<(), ActionOutcome>,
        identity: Result<(u64, String), ActionOutcome>,
        terminate: Result<(), ActionOutcome>,
        wait: ActionWait,
        /// TC-3 (P2 §2 TC-3): the synthetic exit code the signalled completion reads
        /// back; `Err(())` = the read itself fails (named, never silent).
        exit: Result<u32, ()>,
        open_calls: usize,
        terminate_calls: usize,
        exit_calls: usize,
    }

    impl FakeActionApi {
        fn valid(target: HungTarget) -> Self {
            Self {
                identity: Ok((target.creation_time, target.image_path.clone())),
                target,
                windows: Vec::new(),
                probes: vec![ProbeResult::Timeout, ProbeResult::Timeout],
                self_session: Ok(1),
                target_session: Ok(1),
                open: Ok(()),
                terminate: Ok(()),
                wait: ActionWait::Signaled,
                exit: Ok(0),
                open_calls: 0,
                terminate_calls: 0,
                exit_calls: 0,
            }
        }
    }

    impl ActionApi for FakeActionApi {
        type Process = ();

        fn window(&mut self, _hwnd: usize) -> Result<HungTarget, ActionOutcome> {
            if self.windows.is_empty() {
                Ok(self.target.clone())
            } else {
                self.windows.remove(0)
            }
        }

        fn probe(&mut self, _hwnd: usize, _timeout: Duration) -> ProbeResult {
            self.probes.remove(0)
        }

        fn open(&mut self, _pid: u32) -> Result<Self::Process, ActionOutcome> {
            self.open_calls += 1;
            self.open
        }

        fn session(&mut self, pid: u32) -> Result<u32, ActionOutcome> {
            if pid == self.target.pid {
                self.target_session
            } else {
                self.self_session
            }
        }

        fn process_identity(
            &mut self,
            _process: &Self::Process,
        ) -> Result<(u64, String), ActionOutcome> {
            self.identity.clone()
        }

        fn terminate(&mut self, _process: &Self::Process) -> Result<(), ActionOutcome> {
            self.terminate_calls += 1;
            self.terminate
        }

        fn wait(&mut self, _process: &Self::Process, _timeout: Duration) -> ActionWait {
            match self.wait {
                ActionWait::Signaled => ActionWait::Signaled,
                ActionWait::Timeout => ActionWait::Timeout,
                ActionWait::Failed => ActionWait::Failed,
            }
        }

        fn exit_code(&mut self, _process: &Self::Process) -> Result<u32, ()> {
            self.exit_calls += 1;
            self.exit
        }
    }

    #[test]
    fn action_adversarial_checks_prevent_termination() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut cases = Vec::new();

        let mut api = FakeActionApi::valid(expected.clone());
        api.windows.push(Err(ActionOutcome::Refused));
        cases.push((api, ActionOutcome::Refused));
        for changed in [
            target(2, 10, 20, r"C:\Games\game.exe"),
            target(1, 11, 20, r"C:\Games\game.exe"),
            target(1, 10, 21, r"C:\Games\game.exe"),
            target(1, 10, 20, r"D:\Games\game.exe"),
        ] {
            let mut api = FakeActionApi::valid(expected.clone());
            api.windows.push(Ok(changed));
            cases.push((api, ActionOutcome::IdentityMismatch));
        }
        let mut api = FakeActionApi::valid(expected.clone());
        api.probes[0] = ProbeResult::Responsive;
        cases.push((api, ActionOutcome::Recovered));
        let mut api = FakeActionApi::valid(expected.clone());
        api.probes[0] = ProbeResult::Indeterminate;
        cases.push((api, ActionOutcome::Refused));
        let mut api = FakeActionApi::valid(expected.clone());
        api.open = Err(ActionOutcome::AccessDenied);
        cases.push((api, ActionOutcome::AccessDenied));
        // TC-2 (P2 §2 TC-2 acceptance (2)): the cross-session refusal joins the adversarial
        // set; every case above keeps its exact class, AccessDenied included, so the new
        // gate cannot absorb the privilege class.
        let mut api = FakeActionApi::valid(expected.clone());
        api.target_session = Ok(2);
        cases.push((api, ActionOutcome::CrossSession));
        let mut api = FakeActionApi::valid(expected.clone());
        api.identity = Ok((21, expected.image_path.clone()));
        cases.push((api, ActionOutcome::IdentityMismatch));
        let mut api = FakeActionApi::valid(expected.clone());
        api.identity = Ok((20, r"D:\Games\game.exe".into()));
        cases.push((api, ActionOutcome::IdentityMismatch));
        let mut api = FakeActionApi::valid(expected.clone());
        api.windows = vec![
            Ok(expected.clone()),
            Ok(expected.clone()),
            Ok(target(1, 11, 20, r"C:\Games\game.exe")),
        ];
        cases.push((api, ActionOutcome::IdentityMismatch));

        for (mut api, outcome) in cases {
            assert_eq!(
                terminate_bound_target_with(&mut api, &expected, &cfg),
                outcome
            );
            assert_eq!(api.terminate_calls, 0);
        }

        let mut ignored_cfg = cfg.clone();
        ignored_cfg.hang_ignore.push("game.exe".into());
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &ignored_cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);

        let mut safe_cfg = cfg.clone();
        safe_cfg.safe_mode = true;
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &safe_cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);

        let self_target = target(
            1,
            unsafe { GetCurrentProcessId() },
            20,
            r"C:\Games\self.exe",
        );
        let mut api = FakeActionApi::valid(self_target.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &self_target, &cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);

        let windows_target = target(
            1,
            10,
            20,
            &format!(r"{}\protected.exe", windows_directory().unwrap()),
        );
        let mut api = FakeActionApi::valid(windows_target.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &windows_target, &cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);
    }

    #[test]
    fn acceptance_gate_refuses_cross_session_and_query_failure() {
        // TC-2 acceptance site (P2 §2 TC-2): same session stays eligible, a foreign session
        // is refused by name, and ANY session-query failure is fail-closed (never proceed).
        assert_eq!(session_gate(Ok(1), Ok(1)), SessionGate::Same);
        assert_eq!(session_gate(Ok(2), Ok(1)), SessionGate::Cross);
        assert_eq!(session_gate(Ok(1), Ok(2)), SessionGate::Cross);
        assert_eq!(session_gate(Err(()), Ok(1)), SessionGate::Unknown);
        assert_eq!(session_gate(Ok(1), Err(())), SessionGate::Unknown);
        assert_eq!(session_gate(Err(()), Err(())), SessionGate::Unknown);
        // The named vocabulary that rides the audit plumbing (log_hang_audit / detector line).
        assert_eq!(ActionOutcome::CrossSession.label(), "refused-cross-session");
        assert_eq!(
            ActionOutcome::SessionUnknown.label(),
            "refused-session-unknown"
        );
    }

    #[test]
    fn action_refuses_cross_session_before_open() {
        // TC-2 acceptance (1): a target in another session yields the named outcome,
        // OpenProcess is never reached (so AccessDenied cannot absorb it) and
        // TerminateProcess is never called.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        api.target_session = Ok(2);
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::CrossSession
        );
        assert_eq!(
            api.open_calls, 0,
            "the session gate runs before OpenProcess"
        );
        assert_eq!(
            api.terminate_calls, 0,
            "a refused target is never terminated"
        );
    }

    #[test]
    fn action_refuses_when_the_session_query_fails() {
        // TC-2 acceptance (5): a session query failure is a named audited outcome and is
        // fail-closed — the action never proceeds to open or terminate.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        api.target_session = Err(ActionOutcome::SessionUnknown);
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::SessionUnknown,
            "a failed target-session query is a named fail-closed outcome"
        );
        assert_eq!(
            api.open_calls, 0,
            "a failed query never proceeds to OpenProcess"
        );
        assert_eq!(api.terminate_calls, 0);

        let mut api = FakeActionApi::valid(expected.clone());
        api.self_session = Err(ActionOutcome::SessionUnknown);
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::SessionUnknown
        );
        assert_eq!(
            api.open_calls, 0,
            "a failed query never proceeds to OpenProcess"
        );
        assert_eq!(api.terminate_calls, 0);
    }

    #[test]
    fn action_calls_terminate_once_only_after_all_checks() {
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::Success
        );
        assert_eq!(api.terminate_calls, 1);

        let mut api = FakeActionApi::valid(expected.clone());
        api.terminate = Err(ActionOutcome::TerminateFailure);
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::TerminateFailure
        );
        assert_eq!(api.terminate_calls, 1);

        let mut api = FakeActionApi::valid(expected.clone());
        api.wait = ActionWait::Timeout;
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::WaitTimeout
        );
        assert_eq!(api.terminate_calls, 1);
    }

    /// One published target at the stated generation (the publication the press binds).
    fn armed_items(generation: u64) -> Vec<HungTarget> {
        vec![published(
            target(1, 10, 20, r"C:\Games\game.exe"),
            generation,
        )]
    }

    #[test]
    fn signaled_completion_records_the_target_exit_code_in_the_audit_line() {
        // TC-3 acceptance (1): terminate + Signaled -> the audit line carries the exit
        // code read off the handle this action already opened. Synthetic code via Fake;
        // no real process is ever opened, queried or terminated by this test.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        api.exit = Ok(0x1234);
        let report = terminate_action_with(&mut api, &expected, &cfg);
        assert_eq!(report.outcome, ActionOutcome::Success);
        assert_eq!(report.exit, ExitInfo::Captured(0x1234));
        assert_eq!(api.terminate_calls, 1);
        assert_eq!(api.exit_calls, 1);
        let line = Audit {
            target: expected.clone(),
            outcome: report.outcome.label(),
            exit: report.exit,
        }
        .line();
        assert!(line.contains("outcome=success"), "{line}");
        assert!(line.contains("exit=0x1234"), "{line}");
        // An unclassified audit line keeps the shipped text exactly (no suffix).
        let plain = Audit::raised(expected.clone(), "canceled-continuity").line();
        assert_eq!(
            plain,
            format!(
                "hung action pid={} process={} outcome=canceled-continuity",
                expected.pid, expected.process_name
            )
        );
    }

    #[test]
    fn a_failed_exit_code_read_is_named_without_changing_the_success_class() {
        // TC-3 acceptance (5): a failed exit-code read is named `exit-code-unavailable`
        // on the same audit line; the shipped `Success` class stays untouched, and the
        // bounded 5 s wait remains the only completion mechanism.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        api.exit = Err(());
        let report = terminate_action_with(&mut api, &expected, &cfg);
        assert_eq!(
            report.outcome,
            ActionOutcome::Success,
            "a failed exit-code read never changes the outcome class"
        );
        assert_eq!(report.exit, ExitInfo::Unavailable);
        assert_eq!(api.terminate_calls, 1);
        assert_eq!(api.exit_calls, 1);
        let line = Audit {
            target: expected.clone(),
            outcome: report.outcome.label(),
            exit: report.exit,
        }
        .line();
        assert!(line.contains("outcome=success"), "{line}");
        assert!(line.contains("exit-code-unavailable"), "{line}");

        // A terminate that never reaches a signalled completion carries no
        // classification at all (named failure class, empty suffix).
        let mut api = FakeActionApi::valid(expected.clone());
        api.terminate = Err(ActionOutcome::TerminateFailure);
        let report = terminate_action_with(&mut api, &expected, &cfg);
        assert_eq!(report.outcome, ActionOutcome::TerminateFailure);
        assert_eq!(report.exit, ExitInfo::None);
        assert_eq!(api.exit_calls, 0);
    }

    #[test]
    fn a_wait_timeout_marks_the_published_identity_terminal() {
        // TC-3 acceptance (2): after a `wait-timeout`, a later press on the SAME
        // identity from the SAME publication is refused through the existing
        // canceled-continuity path — at the action tick and at the continuity pass —
        // so no second `HoldCommand::Terminate` is ever emitted, and the guarded
        // action (the only path that can call TerminateProcess) never runs again for
        // that stale publication.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let items = armed_items(3);
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Terminate(ref bound) if bound == &items[0]
        ));
        hold.report_action(&items[0], ActionOutcome::WaitTimeout);
        assert!(matches!(
            hold.event(
                button(2, false, started + cfg.hang_hold, "hid"),
                &cfg,
                &items,
                true
            ),
            HoldCommand::None
        ));

        // Second press, same identity, no re-publish: canceled-continuity, no Terminate.
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Audit(Audit {
                outcome: "canceled-continuity",
                ..
            })
        ));
        assert!(
            !matches!(
                hold.tick(started + cfg.hang_hold, &cfg, &items, true),
                HoldCommand::Terminate(_)
            ),
            "a terminal identity can never request a repeat terminate"
        );
        assert_eq!(
            hold.progress(started + cfg.hang_hold, &cfg),
            0.0,
            "the refused press is cancelled, never armed for a second action"
        );

        // The continuity pass cancels an armed press on the stale identity too.
        let mut armed = HoldState::default();
        armed.set_owner(Some(2));
        armed.event(button(2, true, started, "hid"), &cfg, &items, true);
        armed.report_action(&items[0], ActionOutcome::WaitTimeout);
        let audit = armed
            .reconcile(&cfg, &items, true)
            .expect("the terminal identity cancels the armed press");
        assert_eq!(audit.outcome, "canceled-continuity");
    }

    #[test]
    fn a_fresh_republish_rearms_the_terminal_identity() {
        // TC-3 acceptance (3): the mark is bound to the publication it acted on, so a
        // probe-cycle RE-PUBLISH at a newer generation of the same validated identity
        // re-arms a press, and the guarded action terminates again — no permanent
        // lockout. The re-arm stays bounded by `hang_probe_interval_ms` (P2 tradeoff).
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let stale = armed_items(3);
        let fresh = armed_items(4);
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));
        hold.event(button(2, true, started, "hid"), &cfg, &stale, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &stale, true),
            HoldCommand::Terminate(_)
        ));
        hold.report_action(&stale[0], ActionOutcome::WaitTimeout);
        hold.event(
            button(2, false, started + cfg.hang_hold, "hid"),
            &cfg,
            &stale,
            true,
        );

        hold.event(button(2, true, started, "hid"), &cfg, &fresh, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &fresh, true),
            HoldCommand::Terminate(ref bound) if bound == &fresh[0]
        ));

        // The re-published identity really reaches the guarded action (Fake api only).
        let expected = fresh[0].clone();
        let mut api = FakeActionApi::valid(expected.clone());
        api.exit = Ok(0x1234);
        let report = terminate_action_with(&mut api, &expected, &cfg);
        assert_eq!(report.outcome, ActionOutcome::Success);
        assert_eq!(report.exit, ExitInfo::Captured(0x1234));
        assert_eq!(
            api.terminate_calls, 1,
            "a re-published, re-validated identity is actionable again"
        );
    }

    #[test]
    fn a_signaled_success_marks_nothing_terminal() {
        // Only `wait-timeout` is terminal (P2 §2 TC-3 wording): a signalled success
        // records no mark, so this rule cannot widen into a permanent lockout.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let items = armed_items(3);
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Terminate(_)
        ));
        hold.report_action(&items[0], ActionOutcome::Success);
        hold.event(
            button(2, false, started + cfg.hang_hold, "hid"),
            &cfg,
            &items,
            true,
        );
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Terminate(_)
        ));
    }

    /// TC-3 (WO-B-03-FIX): press-hold the target the panel currently indexes
    /// (`hung_index`) through to the action tick, release the press, and report what
    /// the tick produced: `Some(index)` when it asked to terminate that published
    /// target — the only path that reaches the guarded action, so the strongest
    /// assertion available for "no second terminate" — `None` when it refused.
    fn press_to_action(
        hold: &mut HoldState,
        cfg: &Config,
        items: &[HungTarget],
        at: Instant,
    ) -> Option<usize> {
        hold.event(button(2, true, at, "hid"), cfg, items, true);
        let command = hold.tick(at + cfg.hang_hold, cfg, items, true);
        hold.event(
            button(2, false, at + cfg.hang_hold, "hid"),
            cfg,
            items,
            true,
        );
        if let HoldCommand::Terminate(target) = command {
            items.iter().position(|item| item == &target)
        } else {
            None
        }
    }

    #[test]
    fn a_second_wait_timeout_does_not_evict_an_earlier_identity() {
        // WO-B-03-FIX (T1, the WO-B-03-AUDIT §3 refuting sequence): ONE publication
        // (generation 5) carries TWO hung identities A and B, both stuck, at the legal
        // long probe interval 20 s, so the bind bound is `interval +
        // PUBLISH_AGE_TOLERANCE` = 25 s (`hang_probe_interval_ms` legal 250..60000,
        // config.rs L743-748). A times out, B times out, A is re-bound from the SAME,
        // never-re-published generation. The one-slot field overwrote A's mark with
        // B's, so that third press passed every gate and terminated A a second time;
        // the mark SET keeps both marks, so the press is refused through
        // `canceled-continuity` and Terminate is emitted exactly once for A.
        // Fake-only discipline: no real process is opened, queried or terminated; the
        // refusal is asserted at the command level, the only route to the action.
        let cfg = Config {
            hang_enabled: true,
            hang_probe_interval: Duration::from_secs(20),
            ..Config::default()
        };
        let items = vec![
            published(target(1, 10, 20, r"C:\Games\game.exe"), 5),
            published(target(2, 20, 30, r"C:\Games\other.exe"), 5),
        ];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));

        // A's press-hold acts, and its `wait-timeout` marks A at generation 5.
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            Some(0),
            "the first press on A is the one Terminate it ever gets"
        );
        hold.report_action(&items[0], ActionOutcome::WaitTimeout);

        // The panel cycles to B (the short-press cycle moves `hung_index`), B times out.
        hold.hung_index = 1;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            Some(1),
            "B is a different identity, so its first press acts"
        );
        hold.report_action(&items[1], ActionOutcome::WaitTimeout);
        assert_eq!(
            hold.terminal.len(),
            2,
            "B's timeout must not evict A's mark"
        );

        // The panel cycles back to A: same quadruple, SAME generation 5 (no probe cycle
        // in between at a 20 s interval), inside the 25 s bind bound: refused.
        hold.hung_index = 0;
        hold.event(button(2, true, started, "hid"), &cfg, &items, true);
        assert!(matches!(
            hold.tick(started + cfg.hang_hold, &cfg, &items, true),
            HoldCommand::Audit(Audit {
                outcome: "canceled-continuity",
                ..
            })
        ));
        assert!(
            !matches!(
                hold.tick(started + cfg.hang_hold, &cfg, &items, true),
                HoldCommand::Terminate(_)
            ),
            "the timed-out identity A can never request a repeat terminate"
        );
        assert_eq!(
            hold.progress(started + cfg.hang_hold, &cfg),
            0.0,
            "the refused press is cancelled, never armed for a second action"
        );
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            None,
            "no later press into A's publication acts either"
        );
        assert_eq!(
            hold.terminal.len(),
            2,
            "refusals add no marks: only a `wait-timeout` marks"
        );

        // The continuity pass cancels an armed press on A the same way (gate 1).
        let mut armed = HoldState::default();
        armed.set_owner(Some(2));
        armed.report_action(&items[0], ActionOutcome::WaitTimeout);
        armed.report_action(&items[1], ActionOutcome::WaitTimeout);
        armed.event(button(2, true, started, "hid"), &cfg, &items, true);
        let audit = armed
            .reconcile(&cfg, &items, true)
            .expect("the terminal identity cancels the armed press");
        assert_eq!(audit.outcome, "canceled-continuity");
    }

    #[test]
    fn a_republish_prunes_every_mark_and_rearms_each_identity_once() {
        // WO-B-03-FIX (T2): TC-3 acceptance (3) for the mark SET — a probe cycle
        // RE-PUBLISHED both identities at a newer generation, which is the specced
        // re-arm, and the gate pass PRUNES every mark of the older generation (the
        // bounded-growth half of the fix). Each identity is then actionable exactly
        // ONCE in the new generation and refused again inside it.
        // Probe P4b (prune call removed) fails the `is_empty` assert; probe P4c
        // (`contains` dedup removed) fails the `len == 1` assert.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let stale = vec![
            published(target(1, 10, 20, r"C:\Games\game.exe"), 5),
            published(target(2, 20, 30, r"C:\Games\other.exe"), 5),
        ];
        let fresh = vec![
            published(target(1, 10, 20, r"C:\Games\game.exe"), 6),
            published(target(2, 20, 30, r"C:\Games\other.exe"), 6),
        ];
        let started = Instant::now();
        let mut hold = HoldState::default();
        hold.set_owner(Some(2));

        assert_eq!(
            press_to_action(&mut hold, &cfg, &stale, started),
            Some(0),
            "A acts in generation 5"
        );
        hold.report_action(&stale[0], ActionOutcome::WaitTimeout);
        hold.report_action(&stale[0], ActionOutcome::WaitTimeout);
        assert_eq!(
            hold.terminal.len(),
            1,
            "one identity is marked once per generation (dedup keeps the set small)"
        );
        hold.hung_index = 1;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &stale, started),
            Some(1),
            "B acts in generation 5"
        );
        hold.report_action(&stale[1], ActionOutcome::WaitTimeout);
        assert_eq!(hold.terminal.len(), 2, "both generation-5 marks live");

        // Re-publish at generation 6 re-arms A and prunes the generation-5 marks.
        hold.hung_index = 0;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &fresh, started),
            Some(0),
            "A is re-armed by the re-publish (no permanent lockout)"
        );
        assert!(
            hold.terminal.is_empty(),
            "marks older than the current publication are pruned, not kept"
        );

        // A times out again in generation 6: marked, and the repeat press is refused.
        hold.report_action(&fresh[0], ActionOutcome::WaitTimeout);
        assert_eq!(
            press_to_action(&mut hold, &cfg, &fresh, started),
            None,
            "the re-armed A terminates once, not twice, in generation 6"
        );

        // B re-arms by the same re-publish, and is likewise once-only in generation 6.
        hold.hung_index = 1;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &fresh, started),
            Some(1),
            "B is re-armed by the same re-publish"
        );
        hold.report_action(&fresh[1], ActionOutcome::WaitTimeout);
        assert_eq!(
            hold.terminal.len(),
            2,
            "both generation-6 marks are live (neither evicts the other)"
        );
        assert_eq!(
            press_to_action(&mut hold, &cfg, &fresh, started),
            None,
            "the re-armed B terminates once, not twice, in generation 6"
        );
    }

    #[test]
    fn the_terminal_set_evicts_the_oldest_mark_at_capacity() {
        // WO-B-03-FIX (T3): the stated eviction policy, pinned. Filling the set to
        // `MAX_TERMINAL_MARKS` (one mark per identity a publication can carry, derived
        // from the `MAX_WINDOWS` enumerate cap) and pushing one more mark keeps the
        // bound and drops the OLDEST mark; the evicted identity is bindable again in
        // its own generation, and the mark this action just earned is always kept.
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        let marks: Vec<HungTarget> = (0..MAX_TERMINAL_MARKS)
            .map(|index| {
                published(
                    target(
                        index,
                        index as u32 + 1_000,
                        index as u64,
                        &format!(r"C:\Games\game{index}.exe"),
                    ),
                    1,
                )
            })
            .collect();
        let mut hold = HoldState::default();
        for mark in &marks {
            hold.report_action(mark, ActionOutcome::WaitTimeout);
        }
        assert_eq!(
            hold.terminal.len(),
            MAX_TERMINAL_MARKS,
            "the set is bounded by the publication cap"
        );
        assert_eq!(
            hold.terminal[0].0,
            Identity::from(&marks[0]),
            "marks are kept oldest-first, so eviction has a stated meaning"
        );

        let overflow = published(
            target(
                MAX_TERMINAL_MARKS,
                u32::MAX,
                u64::MAX,
                r"C:\Games\overflow.exe",
            ),
            1,
        );
        hold.report_action(&overflow, ActionOutcome::WaitTimeout);
        assert_eq!(
            hold.terminal.len(),
            MAX_TERMINAL_MARKS,
            "the set never grows past the bound"
        );
        assert_eq!(
            hold.terminal[0].0,
            Identity::from(&marks[1]),
            "the OLDEST mark is the one evicted"
        );
        assert!(
            !target_is_terminal(&hold.terminal, &marks[0]),
            "the evicted identity is no longer terminal"
        );
        assert!(
            target_is_terminal(&hold.terminal, &marks[MAX_TERMINAL_MARKS - 1]),
            "the newest marks survive"
        );
        assert!(
            target_is_terminal(&hold.terminal, &overflow),
            "the mark this action earned is always kept"
        );

        // Gate-level consequence of the same policy, in the evicted identity's own
        // generation: it re-arms; a surviving mark keeps refusing.
        let started = Instant::now();
        let items = vec![marks[0].clone(), marks[1].clone()];
        hold.set_owner(Some(2));
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            Some(0),
            "beyond the bound the evicted identity acts: re-arming is the documented effect"
        );
        assert_eq!(
            hold.terminal.len(),
            MAX_TERMINAL_MARKS,
            "the action itself adds no mark; only a `wait-timeout` report does"
        );
        hold.hung_index = 1;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            None,
            "the mark that survived the eviction still refuses its identity"
        );

        // The full set on a fresh timeout evicts the next-oldest mark, in order.
        hold.report_action(&items[0], ActionOutcome::WaitTimeout);
        assert_eq!(
            hold.terminal[0].0,
            Identity::from(&marks[2]),
            "the second-oldest is evicted next: the policy is stable under pressure"
        );
        hold.hung_index = 0;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            None,
            "A, re-marked by its own timeout, is refused again"
        );
        hold.hung_index = 1;
        assert_eq!(
            press_to_action(&mut hold, &cfg, &items, started),
            Some(1),
            "B, evicted by the fresh mark, is bindable again (documented beyond-bound behavior)"
        );
    }

    #[test]
    fn default_disabled_policy_never_polls_or_terminates() {
        let cfg = Config::default();
        assert!(!cfg.hang_enabled);
        assert!(!Cadence::default().poll_due(&Policy::from(&cfg), Duration::ZERO));

        let expected = target(1, 10, 20, r"C:\Games\game.exe");
        let mut api = FakeActionApi::valid(expected.clone());
        assert_eq!(
            terminate_bound_target_with(&mut api, &expected, &cfg),
            ActionOutcome::Refused
        );
        assert_eq!(api.terminate_calls, 0);
    }

    #[test]
    fn harness_cleanup_removes_only_its_owned_tree() {
        let root = std::env::temp_dir().join(format!(
            "lcdsirplus-hang-cleanup-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("owned"), b"test").unwrap();
        {
            let _cleanup = HarnessCleanup {
                child: None,
                root: root.clone(),
            };
        }
        assert!(!root.exists());
    }

    #[test]
    fn renamed_cargo_test_binary_is_recognized_case_insensitively() {
        assert!(is_cargo_test_binary(Path::new(
            r"C:\repo\target\release\deps\LCDSirPlus-0ffc5aa055826aa0.exe"
        )));
        assert!(!is_cargo_test_binary(Path::new(
            r"C:\repo\target\release\LCDSirPlus.exe"
        )));
    }

    #[test]
    #[ignore = "starts a visible purpose-built Windows window for bounded native smoke"]
    fn disposable_harness_is_detected_and_cleans_up() {
        smoke().unwrap();
    }

    #[test]
    #[ignore = "runs the pumping native timeout harness through an explicitly enabled policy"]
    fn disposable_harness_pumps_until_enabled_policy_confirms_timeouts() {
        let mut cleanup = spawn_harness(20).unwrap();
        let pid = cleanup.child.as_ref().unwrap().id();
        let cfg = Config {
            hang_enabled: true,
            ..Config::default()
        };
        detect_harness_with_policy(pid, &Policy::from(&cfg)).unwrap();
        assert!(matches!(
            cleanup.child.as_mut().unwrap().try_wait(),
            Ok(None)
        ));
    }

    #[test]
    #[ignore = "terminates only its directly spawned identity-bound disposable child"]
    fn disposable_harness_action_positive_smoke() {
        assert!(action_smoke(false).is_ok());
    }

    #[test]
    #[ignore = "proves early/wrong-identity release leaves owned child alive until cleanup"]
    fn disposable_harness_action_negative_smoke() {
        assert!(action_smoke(true).is_ok());
    }
}
