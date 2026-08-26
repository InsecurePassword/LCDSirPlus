//! Deterministic physical hardware-test sequence (STEP 01..10).
//!
//! Non-destructive by contract: no slot cycling, no process termination, no
//! provider side effects. Each stage is time-bounded; every native submission
//! result and every observed button transition is recorded. Software
//! transport success is never treated as physical confirmation — that
//! requires the human observer.

use std::time::{Duration, Instant, SystemTime};

use crate::backends::g13::{pack_report, parse_input, G13_OUTPUT_REPORT_LENGTH};
use crate::render::Frame;

pub const STEP_DURATION: Duration = Duration::from_secs(2);
pub const STEP_COUNT: usize = 10;
const SUBMIT_INTERVAL: Duration = Duration::from_millis(100);

/// Transport boundary for the test. The direct-HID transport reports real
/// native submission results; the channel transport (virtual backend) is a
/// no-device smoke path.
pub enum Transport<'a> {
    DirectHid(&'a crate::backends::hid::HidDevice),
    Virtual,
}

#[allow(dead_code)]
pub struct StepResult {
    pub step: usize,
    pub name: &'static str,
    pub submissions: usize,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // t timestamps feed Phase 4 evidence records
pub struct ButtonObservation {
    pub at: Instant,
    pub index: usize,
    pub down: bool,
}

#[allow(dead_code)]
pub struct TestConfig {
    pub duration: Duration,
    pub backend_name: &'static str,
    /// Frame for STEP 10 (live dashboard render from the app).
    pub dashboard: Option<Frame>,
    /// Per-step wall time (default 2 s); configurable for fast self-tests.
    pub step_duration: Duration,
    /// Submission cadence within a step (default 100 ms).
    pub submit_interval: Duration,
}

impl Default for TestConfig {
    fn default() -> Self {
        TestConfig {
            duration: Duration::from_secs(30),
            backend_name: "hid",
            dashboard: None,
            step_duration: STEP_DURATION,
            submit_interval: SUBMIT_INTERVAL,
        }
    }
}

type DrawFn = fn(u64) -> Frame;

/// Run the full sequence. Blocks for `min(duration, steps*2s)`; the final
/// dashboard stage holds for the remainder of `duration`.
pub fn run(
    transport: &Transport<'_>,
    test: &TestConfig,
) -> (Vec<StepResult>, Vec<ButtonObservation>) {
    let mut results = Vec::new();
    let mut buttons = Vec::new();
    let start = Instant::now();

    let steps: [(&'static str, DrawFn); STEP_COUNT - 1] = [
        ("all-pixels-off", |_| Frame::new()),
        ("all-pixels-on", all_on),
        ("border-and-corners", border_and_corners),
        ("vertical-stripes", vertical_stripes),
        ("checkerboard", checkerboard),
        ("horizontal-rows", horizontal_rows),
        ("identity", identity),
        ("moving-bar", moving_bar),
        ("counter-timestamp", counter_timestamp),
    ];

    let mut transport_error: Option<String> = None;
    let mut tick: u64 = 0;

    for (index, (name, draw)) in steps.iter().enumerate() {
        let step = index + 1;
        let (result, observed, error) = run_step(
            transport,
            step,
            name,
            draw,
            tick,
            test.step_duration,
            test.submit_interval,
        );
        buttons.extend(observed);
        tick += 1;
        let failed = error.is_some();
        results.push(StepResult {
            step,
            name,
            submissions: result,
            error,
        });
        if failed {
            transport_error = Some(format!("step {} failed", step));
            break;
        }
    }

    // STEP 10: normal dashboard (live frame when provided).
    if transport_error.is_none() {
        let dashboard = test.dashboard.clone().unwrap_or_else(placeholder_dashboard);
        let hold_frame = dashboard.clone();
        let (result, observed, error) = run_step(
            transport,
            10,
            "normal-dashboard",
            move |_| dashboard.clone(),
            tick,
            test.step_duration,
            test.submit_interval,
        );
        buttons.extend(observed);
        results.push(StepResult {
            step: 10,
            name: "normal-dashboard",
            submissions: result,
            error,
        });
        // Hold the dashboard for the remaining duration.
        let remaining = test.duration.saturating_sub(start.elapsed());
        let hold_until = Instant::now() + remaining;
        while Instant::now() < hold_until {
            let report = pack_report(&hold_frame);
            if submit(transport, &report).is_err() {
                break;
            }
            poll_buttons(transport, &mut buttons, test.submit_interval);
        }
    }

    (results, buttons)
}

#[allow(clippy::too_many_arguments)]
fn run_step(
    transport: &Transport<'_>,
    step: usize,
    _name: &'static str,
    mut draw: impl FnMut(u64) -> Frame,
    mut tick: u64,
    step_duration: Duration,
    submit_interval: Duration,
) -> (usize, Vec<ButtonObservation>, Option<String>) {
    let mut buttons = Vec::new();
    let mut submissions = 0usize;
    let mut error = None;
    let deadline = Instant::now() + step_duration;
    while Instant::now() < deadline && error.is_none() {
        let mut frame = draw(tick);
        stamp_step(&mut frame, step);
        let report = pack_report(&frame);
        if let Err(e) = submit(transport, &report) {
            error = Some(e);
            break;
        }
        submissions += 1;
        poll_buttons(transport, &mut buttons, submit_interval);
        tick += 1;
    }
    (submissions, buttons, error)
}

fn submit(
    transport: &Transport<'_>,
    report: &[u8; G13_OUTPUT_REPORT_LENGTH],
) -> Result<(), String> {
    match transport {
        Transport::DirectHid(device) => device.write_report(report),
        Transport::Virtual => Ok(()),
    }
}

fn poll_buttons(
    transport: &Transport<'_>,
    buttons: &mut Vec<ButtonObservation>,
    timeout: Duration,
) {
    if let Transport::DirectHid(device) = transport {
        let mut raw = [0u8; 8];
        match device.read_input_timeout(&mut raw, timeout) {
            Ok(true) => {
                if let Ok(states) = parse_input(&raw) {
                    let now = Instant::now();
                    for (i, down) in states.iter().enumerate() {
                        buttons.push(ButtonObservation {
                            at: now,
                            index: i,
                            down: *down,
                        });
                    }
                }
            }
            Ok(false) => {}
            Err(_) => {}
        }
    } else {
        std::thread::sleep(timeout);
    }
}

fn stamp_step(frame: &mut Frame, step: usize) {
    frame.text(1, 36, &format!("STEP {:02}", step), true);
}

fn all_on(_tick: u64) -> Frame {
    let mut f = Frame::new();
    f.fill_rect(
        0,
        0,
        crate::model::WIDTH as i32,
        crate::model::HEIGHT as i32,
        true,
    );
    f
}

fn border_and_corners(_tick: u64) -> Frame {
    let mut f = Frame::new();
    f.rect(
        0,
        0,
        crate::model::WIDTH as i32,
        crate::model::HEIGHT as i32,
        true,
    );
    f.fill_rect(2, 2, 2, 2, true);
    f.fill_rect(156, 2, 2, 2, true);
    f.fill_rect(2, 39, 2, 2, true);
    f.fill_rect(156, 39, 2, 2, true);
    f
}

fn vertical_stripes(_tick: u64) -> Frame {
    let mut f = Frame::new();
    for x in 0..crate::model::WIDTH {
        if x % 4 < 2 {
            f.v_line(x as i32, 0, 43, true);
        }
    }
    f
}

fn checkerboard(_tick: u64) -> Frame {
    let mut f = Frame::new();
    for y in 0..crate::model::HEIGHT {
        for x in 0..crate::model::WIDTH {
            if (x + y) % 4 < 2 {
                f.set(x as i32, y as i32, true);
            }
        }
    }
    f
}

fn horizontal_rows(_tick: u64) -> Frame {
    let mut f = Frame::new();
    for y in (0..crate::model::HEIGHT).step_by(4) {
        f.h_line(0, 159, y as i32, true);
    }
    f
}

fn identity(_tick: u64) -> Frame {
    let mut f = Frame::new();
    f.rect(0, 0, 160, 43, true);
    f.text_centered(3, 154, 6, "LCDFORGE", 2, true);
    f.text_centered(
        3,
        154,
        22,
        &format!("V{} BACKEND HID", env!("CARGO_PKG_VERSION")),
        1,
        true,
    );
    f
}

fn moving_bar(tick: u64) -> Frame {
    let mut f = Frame::new();
    f.rect(0, 0, 160, 43, true);
    let pos = (tick % 14) as i32 * 10;
    f.fill_rect(pos.min(140), 16, 20, 6, true);
    f
}

fn counter_timestamp(tick: u64) -> Frame {
    let mut f = Frame::new();
    f.rect(0, 0, 160, 43, true);
    f.text_centered(3, 154, 8, &format!("FRAME {}", tick), 1, true);
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    f.text_centered(3, 154, 20, &format!("T+{}S", secs), 1, true);
    f
}

fn placeholder_dashboard() -> Frame {
    let mut f = Frame::new();
    f.rect(0, 0, 160, 43, true);
    f.text_centered(3, 154, 16, "DASHBOARD", 1, true);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_frames_have_step_stamps() {
        let mut f = border_and_corners(0);
        stamp_step(&mut f, 3);
        // 'S' glyph row 0 covers columns 1-2, row 1 covers column 0.
        assert!(f.get(2, 36) && f.get(1, 37), "STEP stamp text");
        assert!(f.get(0, 0) && f.get(159, 42), "border present");
    }

    #[test]
    fn all_on_fills_every_pixel() {
        let f = all_on(0);
        assert!(f.pixels.iter().all(|&p| p == 1));
    }

    #[test]
    fn moving_bar_changes_position() {
        let a = pack_report(&moving_bar(0));
        let b = pack_report(&moving_bar(1));
        assert_ne!(a, b, "bar must move");
    }

    #[test]
    fn virtual_transport_runs_all_steps_without_error() {
        let test = TestConfig {
            duration: Duration::from_millis(50),
            backend_name: "virtual",
            dashboard: None,
            step_duration: Duration::from_millis(5),
            submit_interval: Duration::from_millis(1),
        };
        let (results, _buttons) = run(&Transport::Virtual, &test);
        assert_eq!(results.len(), STEP_COUNT);
        assert!(
            results.iter().all(|r| r.error.is_none()),
            "virtual transport never fails"
        );
        assert!(results.iter().all(|r| r.submissions >= 1));
        assert_eq!(results[0].name, "all-pixels-off");
        assert_eq!(results[9].name, "normal-dashboard");
    }
}
