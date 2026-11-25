//! Virtual preview window (scaled 160x43 dashboard) and tray icon.
//!
//! The preview replicates the physical button contract: left-click a slot
//! simulates its short press (cycle forward), right-click cycles backward.
//! The UI thread owns all Win32 window state. The app publishes each logical
//! frame into the capacity-1 slot and posts one coalesced paint edge, so a
//! hidden preview performs no pixel work (WO-OPT03, handoff finding O2). The
//! thread that stores the window handle runs a window-ready handoff, so an edge
//! or an exit signal that arrived before the window exists is never stranded
//! (WO-OPT03 repair, audit findings F-AUD-OPT03-1/-2).

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::backends::WakeSender;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, SetStretchBltMode, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DIB_RGB_COLORS, HDC, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadCursorW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, ShowWindow, TrackPopupMenu,
    TranslateMessage, CW_USEDEFAULT, HICON, HMENU, IDC_ARROW, IDI_APPLICATION, MA_NOACTIVATE,
    MENU_ITEM_FLAGS, MSG, SW_HIDE, SW_SHOWNOACTIVATE, TRACK_POPUP_MENU_FLAGS, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE, WM_DESTROY, WM_LBUTTONDOWN,
    WM_MOUSEACTIVATE, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSW, WS_EX_NOACTIVATE, WS_OVERLAPPEDWINDOW,
};

pub const FRAME_W: u32 = crate::model::WIDTH as u32;
pub const FRAME_H: u32 = crate::model::HEIGHT as u32;
const WM_APP_FRAME: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const ID_MENU_SHOW: usize = 1001;
const ID_MENU_EXIT: usize = 1002;

/// WO-OPT03 seam: BGRA conversions performed by the paint-path converter.
#[cfg(test)]
static CONVERSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// WO-OPT03 seam: notify edges produced (one `WM_APP_FRAME` post per edge).
#[cfg(test)]
static NOTIFY_EDGES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// WO-OPT03 repair seam (F-AUD-OPT03-4, wording per F-AUD-OPT032-1): `WM_APP_FRAME` posts
/// TARGETED AT A KNOWN WINDOW, counted inside the `hwnd != 0` leg of `notify_frame`. An edge
/// is not a paint: only this counter proves a post was addressed to a window whose handle is
/// known. It does NOT prove OS-queue acceptance: the increment precedes the `PostMessageW`
/// return, which is ignored, so a queue-quota rejection is not detected. The armed edge is
/// the recovery path (a quota-failed post leaves `pending` armed; visibility 0->1 recovers
/// it) - registered residual, not closed by this counter.
#[cfg(test)]
static POSTS_DELIVERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// WO-OPT03 repair seam: `WM_DESTROY` posts delivered (the loop-end signal), at
/// both shipped post sites (the app's shutdown and the window-ready handoff).
#[cfg(test)]
static DESTROY_POSTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn preview_size(scale: i32) -> (i32, i32) {
    (FRAME_W as i32 * scale, FRAME_H as i32 * scale)
}

fn preview_ex_style() -> WINDOW_EX_STYLE {
    WS_EX_NOACTIVATE
}

fn preview_show_command(visible: bool) -> windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD {
    if visible {
        SW_SHOWNOACTIVATE
    } else {
        SW_HIDE
    }
}

fn preview_mouse_activation(msg: u32) -> Option<LRESULT> {
    (msg == WM_MOUSEACTIVATE).then_some(LRESULT(MA_NOACTIVATE as isize))
}

unsafe fn load_app_icon(hinstance: HINSTANCE) -> HICON {
    LoadIconW(hinstance, PCWSTR(1 as _))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default()
}

unsafe fn paint_frame(hdc: HDC, pixels: &[u8], scale: i32) {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: FRAME_W as i32,
            biHeight: -(FRAME_H as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let (client_w, client_h) = preview_size(scale);
    let _ = SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = StretchDIBits(
        hdc,
        0,
        0,
        client_w,
        client_h,
        0,
        0,
        FRAME_W as i32,
        FRAME_H as i32,
        Some(pixels.as_ptr() as *const core::ffi::c_void),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
}

/// Events the UI sends back to the application.
#[derive(Clone, Copy, Debug)]
pub enum UiEvent {
    /// Preview click: cycle the given slot (physical button index 0..4).
    SlotCycle { slot: usize, backward: bool },
    /// User requested exit from the tray menu.
    Exit,
}

/// Capacity-1 latest-frame slot (WO-OPT03, handoff finding O2). The app thread
/// publishes the newest logical frame here; the UI thread consumes the `pending`
/// edge. Capacity 1 by construction, so a stalled UI thread retains exactly one
/// frame, and the allocation is reused (`clear` + `extend_from_slice`, never a
/// fresh `Vec`).
struct FrameSlot {
    /// Logical 160x43 bytes, the newest published frame.
    frame: Mutex<Vec<u8>>,
    /// A frame is waiting to be painted: set by a publisher, taken by the paint
    /// path. The 0->1 transition is the only notify edge.
    pending: AtomicBool,
}

impl FrameSlot {
    /// Store the newest logical frame. A frame identical to the stored one is no
    /// work at all. The return value is the coalesced notify edge: true only on
    /// the `pending` 0->1 transition, so 100 publishes wake the UI at most once.
    fn publish(&self, pixels: &[u8]) -> bool {
        let mut slot = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_slice() == pixels {
            return false;
        }
        slot.clear();
        slot.extend_from_slice(pixels);
        !self.pending.swap(true, Ordering::SeqCst)
    }

    /// Take the pending frame for a paint. `pending` is cleared BEFORE the slot
    /// is read: a frame published between the read and the clear would be
    /// stranded with the flag already low, so its paint edge would never arrive
    /// (unit 50's rule, the `app.rs:4664` form). WO-OPT03 repair (F-AUD-OPT03-6):
    /// the frame lock is handed to the caller as a view, so the paint converts
    /// straight out of the slot and allocates no payload per paint.
    fn take_pending(&self) -> Option<SlotRead<'_>> {
        if !self.pending.swap(false, Ordering::SeqCst) {
            return None;
        }
        let guard = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        (!guard.is_empty()).then_some(SlotRead { guard })
    }
}

/// The slot view handed back by `take_pending`: it holds the slot lock for the
/// paint's lifetime. The paint path locks `pixels` first and takes this `frame`
/// lock inside it (order `pixels` -> `frame`, one nested section); no path in
/// the file takes them in the opposite order, so no lock cycle exists.
struct SlotRead<'a> {
    guard: std::sync::MutexGuard<'a, Vec<u8>>,
}

impl std::ops::Deref for SlotRead<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.guard
    }
}

impl PartialEq<Vec<u8>> for SlotRead<'_> {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.guard.as_slice() == other.as_slice()
    }
}

impl std::fmt::Debug for SlotRead<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SlotRead(len={})", self.guard.len())
    }
}

struct UiShared {
    /// BGRA pixel buffer (160x43 x 4 B), grown once by the paint path and reused.
    pixels: Mutex<Vec<u8>>,
    /// The newest logical frame, published by the app thread (WO-OPT03).
    slot: FrameSlot,
    scale: AtomicU32,
    preview_visible: AtomicBool,
    manual_visibility: AtomicBool,
    event_tx: OnceLock<WakeSender<UiEvent>>,
    hwnd: AtomicIsize,
    /// An exit has been requested. The flag is the window-independent half of the
    /// exit signal: a shutdown that races window creation is honored by the
    /// window-ready handoff (WO-OPT03 repair, F-AUD-OPT03-2).
    shutdown_requested: AtomicBool,
}

static UI: UiShared = UiShared {
    pixels: Mutex::new(Vec::new()),
    slot: FrameSlot {
        frame: Mutex::new(Vec::new()),
        pending: AtomicBool::new(false),
    },
    scale: AtomicU32::new(4),
    preview_visible: AtomicBool::new(false),
    manual_visibility: AtomicBool::new(false),
    event_tx: OnceLock::new(),
    hwnd: AtomicIsize::new(0),
    shutdown_requested: AtomicBool::new(false),
};

/// Paint-path conversion (WO-OPT03, handoff finding O2): turn a stored logical
/// frame into the BGRA paint buffer in place, growing its allocation once and
/// reusing it. This is the file's only pixel conversion, and the app thread never
/// calls it: it runs in the paint path, only while the preview is visible.
fn convert_frame(pixels: &mut Vec<u8>, frame: &[u8]) {
    #[cfg(test)]
    CONVERSIONS.fetch_add(1, Ordering::Relaxed);
    pixels.clear();
    pixels.reserve(frame.len() * 4);
    for &p in frame {
        let v: u32 = if p != 0 { 0x00000000 } else { 0x00FF_FFFF };
        pixels.extend_from_slice(&v.to_le_bytes());
    }
}

/// The coalesced notify: one cross-thread `PostMessageW(WM_APP_FRAME)` per
/// `pending` 0->1 edge, the shipped post mechanism now driven by the publisher
/// and by the visibility legs (WO-OPT03). The return value is delivery: with no
/// window yet the post cannot be made. An undeliverable post must not consume
/// the coalesced edge, or the frame is stranded (F-AUD-OPT03-1): the caller
/// leaves `pending` armed and the window-ready handoff delivers it.
unsafe fn notify_frame() -> bool {
    #[cfg(test)]
    NOTIFY_EDGES.fetch_add(1, Ordering::Relaxed);
    // `SeqCst` pairs with the `hwnd` store and the handoff's `pending` load, so a
    // publisher that saw no window is seen by the handoff (the Dekker pair).
    let hwnd = UI.hwnd.load(Ordering::SeqCst);
    if hwnd == 0 {
        return false;
    }
    #[cfg(test)]
    POSTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
    let _ = PostMessageW(HWND(hwnd as _), WM_APP_FRAME, WPARAM(0), LPARAM(0));
    true
}

/// The window-ready handoff (WO-OPT03 repair, F-AUD-OPT03-1/-2): a publish edge
/// and an exit signal can both arrive before the window exists, so the thread
/// that stores the handle re-drives them. The exit wins over a paint (a
/// shutdown must not be undone by a frame), and a pending frame gets the shipped
/// post its publisher could not deliver.
unsafe fn window_ready_handoff(hwnd: HWND, notify: unsafe fn() -> bool) {
    if UI.shutdown_requested.load(Ordering::SeqCst) {
        #[cfg(test)]
        DESTROY_POSTS.fetch_add(1, Ordering::Relaxed);
        let _ = PostMessageW(hwnd, WM_DESTROY, WPARAM(0), LPARAM(0));
        return;
    }
    if UI.slot.pending.load(Ordering::SeqCst) {
        let _ = notify();
    }
}

/// Only a 0->1 visibility transition forces a paint; the slot already holds the
/// newest frame, so showing the preview shows the current frame (WO-OPT03).
fn visibility_needs_paint(before: bool, after: bool) -> bool {
    !before && after
}

/// Handle for the application to drive the UI thread.
pub struct Ui {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ui {
    /// Spawn the UI thread with a tray icon; the preview window appears per
    /// `show_preview` (config `preview_mode` resolution).
    ///
    /// WO-OPT02A (finding O1): `event_tx` is the app's wake-wrapped sender, so a
    /// preview click and a tray Exit are immediate app-loop wakes (the Exit edge
    /// is what lets a parked loop take the graceful exit at once).
    pub fn spawn(scale: u32, show_preview: bool, event_tx: WakeSender<UiEvent>) -> Ui {
        UI.scale.store(scale.max(1), Ordering::Relaxed);
        UI.preview_visible.store(show_preview, Ordering::Relaxed);
        UI.manual_visibility.store(false, Ordering::Relaxed);
        let _ = UI.event_tx.set(event_tx);
        let thread = std::thread::Builder::new()
            .name("lcdsirplus-ui".into())
            .spawn(ui_main)
            .expect("spawn ui thread");
        Ui {
            thread: Some(thread),
        }
    }

    /// Push a logical frame (160x43 bytes) to the preview.
    ///
    /// WO-OPT03 (handoff finding O2): the frame is published into the capacity-1
    /// slot and the hidden preview returns without a BGRA conversion and without
    /// a notification, while a visible preview wakes only on the `pending` 0->1
    /// edge, so an identical frame and a stalled UI thread cost no per-frame work.
    pub fn show_frame(&self, frame: &crate::render::Frame) {
        let edged = UI.slot.publish(&frame.pixels);
        if !UI.preview_visible.load(Ordering::Relaxed) {
            return;
        }
        if edged {
            // An undeliverable post must not consume the coalesced edge, or the
            // frame is stranded: `pending` stays armed and the window-ready
            // handoff posts it (F-AUD-OPT03-1).
            let _delivered = unsafe { notify_frame() };
        }
    }

    #[allow(dead_code)] // tray toggle drives visibility directly; API kept for config changes
    pub fn set_preview_visible(&self, visible: bool) {
        let before = UI.preview_visible.swap(visible, Ordering::Relaxed);
        if before == visible {
            return;
        }
        unsafe {
            let hwnd = UI.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                let _ = ShowWindow(HWND(hwnd as _), preview_show_command(visible));
            }
            if visibility_needs_paint(before, visible) {
                notify_frame();
            }
        }
    }

    pub fn set_preview_auto_visible(&self, visible: bool) {
        if !UI.manual_visibility.load(Ordering::Relaxed) {
            self.set_preview_visible(visible);
        }
    }

    /// End the UI thread. With the frame pump gone (WO-OPT03) the exit is the
    /// explicit `WM_DESTROY` the pump used to post when the channel closed, then
    /// the join: the loop's `WM_CLOSE | WM_DESTROY` leg quits the message loop,
    /// the tray icon is removed and the thread ends, so no thread survives.
    pub fn shutdown(mut self) {
        // Arm the exit signal BEFORE looking for a window, so a window created
        // microseconds later ends its own loop (F-AUD-OPT03-2).
        UI.shutdown_requested.store(true, Ordering::SeqCst);
        unsafe {
            let hwnd = UI.hwnd.load(Ordering::SeqCst);
            if hwnd != 0 {
                #[cfg(test)]
                DESTROY_POSTS.fetch_add(1, Ordering::Relaxed);
                let _ = PostMessageW(HWND(hwnd as _), WM_DESTROY, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
impl Ui {
    /// A handle with no UI thread: `show_frame` and the visibility legs drive the
    /// static slot, so the tests observe the mechanism without a window.
    fn test_ui() -> Ui {
        Ui { thread: None }
    }
}

fn ui_main() {
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let hinstance = HINSTANCE::from(hinstance);
        let icon = load_app_icon(hinstance);
        let class_name: Vec<u16> = "LCDSIRPLUS_PREVIEW\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hIcon: icon,
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let scale = UI.scale.load(Ordering::Relaxed) as i32;
        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false);
        let (client_w, client_h) = preview_size(scale);
        let width = client_w + (rect.right - rect.left);
        let height = client_h + (rect.bottom - rect.top);
        let window_name: Vec<u16> = "LCDSirPlus Preview\0".encode_utf16().collect();
        let Ok(hwnd) = CreateWindowExW(
            preview_ex_style(),
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(window_name.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            None,
            None,
            hinstance,
            None,
        ) else {
            return;
        };
        UI.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
        // The window-ready handoff (WO-OPT03 repair): an exit signal or a publish
        // edge that arrived while `hwnd` was still 0 is delivered here, before the
        // tray icon exists, so neither is stranded (F-AUD-OPT03-1/-2).
        window_ready_handoff(hwnd, notify_frame);

        add_tray_icon(hwnd, icon);

        let visible = UI.preview_visible.load(Ordering::Relaxed);
        let _ = ShowWindow(hwnd, preview_show_command(visible));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_tray_icon(hwnd);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(result) = preview_mouse_activation(msg) {
        return result;
    }
    match msg {
        WM_CREATE => LRESULT(0),
        WM_APP_FRAME => {
            // Paint path (WO-OPT03, repair F-AUD-OPT03-6): convert the newest
            // frame into the reused BGRA buffer only while the preview is
            // visible, straight out of the slot - `pixels` is locked first and
            // the slot lock is taken inside it, so a paint allocates no payload.
            if UI.preview_visible.load(Ordering::Relaxed) {
                let mut pixels = UI.pixels.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(frame) = UI.slot.take_pending() {
                    convert_frame(&mut pixels, &frame);
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let scale = UI.scale.load(Ordering::Relaxed) as i32;
            if let Ok(pixels) = UI.pixels.lock() {
                if pixels.len() == (FRAME_W * FRAME_H * 4) as usize {
                    paint_frame(hdc, &pixels, scale);
                }
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            let backward = msg == WM_RBUTTONDOWN;
            let x = (lparam.0 & 0xFFFF) as u32;
            let scale = UI.scale.load(Ordering::Relaxed);
            let slot = (x / (FRAME_W * scale / 4)).min(3) as usize;
            if let Some(tx) = UI.event_tx.get() {
                tx.send(UiEvent::SlotCycle { slot, backward });
            }
            LRESULT(0)
        }
        WM_APP_TRAY => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if event == WM_LBUTTONDOWN {
                toggle_preview(hwnd);
            } else if event == WM_RBUTTONDOWN {
                show_tray_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => match wparam.0 & 0xFFFF {
            ID_MENU_SHOW => {
                toggle_preview(hwnd);
                LRESULT(0)
            }
            ID_MENU_EXIT => {
                if let Some(tx) = UI.event_tx.get() {
                    tx.send(UiEvent::Exit);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => LRESULT(0),
        },
        WM_CONTEXTMENU => {
            show_tray_menu(hwnd);
            LRESULT(0)
        }
        WM_CLOSE | WM_DESTROY => {
            if let Some(tx) = UI.event_tx.get() {
                tx.send(UiEvent::Exit);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn toggle_preview(hwnd: HWND) {
    let before = UI.preview_visible.load(Ordering::Relaxed);
    let visible = !before;
    UI.manual_visibility.store(true, Ordering::Relaxed);
    UI.preview_visible.store(visible, Ordering::Relaxed);
    let _ = ShowWindow(hwnd, preview_show_command(visible));
    if visibility_needs_paint(before, visible) {
        notify_frame();
    }
}

unsafe fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(
        menu,
        MENU_ITEM_FLAGS(0),
        ID_MENU_SHOW,
        PCWSTR::from_raw(wide("Toggle Preview\0").as_ptr()),
    );
    let _ = AppendMenuW(
        menu,
        MENU_ITEM_FLAGS(0),
        ID_MENU_EXIT,
        PCWSTR::from_raw(wide("Exit\0").as_ptr()),
    );
    let mut point = windows::Win32::Foundation::POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TRACK_POPUP_MENU_FLAGS(0),
        point.x,
        point.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(HMENU(menu.0));
}

unsafe fn add_tray_icon(hwnd: HWND, icon: HICON) {
    let mut nid = NOTIFYICONDATAW {
        hWnd: hwnd,
        uID: 1,
        uFlags: NOTIFY_ICON_DATA_FLAGS(0x01 | 0x02 | 0x04), // MESSAGE | ICON | TIP
        uCallbackMessage: WM_APP_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (i, c) in "LCDSirPlus\0".encode_utf16().enumerate() {
        if i < nid.szTip.len() {
            nid.szTip[i] = c;
        }
    }
    let _ = Shell_NotifyIconW(NOTIFY_ICON_MESSAGE(0x00000000), &nid); // NIM_ADD
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NOTIFY_ICON_MESSAGE(0x00000002), &nid); // NIM_DELETE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Frame;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetPixel, PatBlt,
        SelectObject, WHITENESS,
    };

    #[test]
    fn preview_scale_four_fills_the_configured_client() {
        unsafe {
            let (width, height) = preview_size(4);
            let dc = CreateCompatibleDC(None);
            assert!(!dc.0.is_null());
            let bitmap = CreateCompatibleBitmap(dc, width, height);
            assert!(!bitmap.0.is_null());
            let previous = SelectObject(dc, bitmap);
            let _ = PatBlt(dc, 0, 0, width, height, WHITENESS);
            paint_frame(dc, &vec![0; (FRAME_W * FRAME_H * 4) as usize], 4);
            assert_eq!(
                (GetPixel(dc, 0, 0).0, GetPixel(dc, width - 1, height - 1).0),
                (0, 0)
            );
            let _ = SelectObject(dc, previous);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(dc);
        }
    }

    #[test]
    fn app_icon_falls_back_to_a_stock_icon() {
        unsafe {
            let module = GetModuleHandleW(None).unwrap();
            assert!(!load_app_icon(HINSTANCE::from(module)).0.is_null());
        }
    }

    #[test]
    fn preview_uses_no_activate_window_behavior() {
        assert_ne!(preview_ex_style().0 & WS_EX_NOACTIVATE.0, 0);
        assert_eq!(preview_show_command(true), SW_SHOWNOACTIVATE);
        assert_eq!(
            preview_mouse_activation(WM_MOUSEACTIVATE),
            Some(LRESULT(MA_NOACTIVATE as isize))
        );
        assert_eq!(preview_mouse_activation(WM_LBUTTONDOWN), None);
        assert_eq!(preview_mouse_activation(WM_RBUTTONDOWN), None);
    }

    // WO-OPT03 (handoff finding O2): the hidden-preview, coalesced-notify and
    // capacity-1-slot tests. The slot, the visibility flag and the seams are
    // process-global statics, so these tests share one serial lock.
    const LOGICAL: usize = FRAME_W as usize * FRAME_H as usize;

    /// A non-zero, provably invalid handle: it puts the shipped legs on their
    /// `window exists` side. `PostMessageW` to it fails and the result is ignored,
    /// so no window and no panel is involved (the Win32 legs stay code-path
    /// proofs, per the B7 manual list).
    const NO_WINDOW: isize = 1;

    /// The slot, the visibility flag and the seams are process-global statics.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn slot_guard() -> std::sync::MutexGuard<'static, Vec<u8>> {
        UI.slot.frame.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The production slice of this file, so a pin's own literals cannot
    /// self-count (the `app.rs:4667` form).
    fn production_source() -> String {
        include_str!("ui.rs").replace("\r\n", "\n")
    }

    #[test]
    fn a_hidden_preview_performs_no_conversion_and_no_notification() {
        let _guard = serial();
        let ui = Ui::test_ui();
        let frame = Frame::default();
        UI.preview_visible.store(false, Ordering::Relaxed);
        let conversions = CONVERSIONS.load(Ordering::Relaxed);
        let edges = NOTIFY_EDGES.load(Ordering::Relaxed);
        for _ in 0..10 {
            ui.show_frame(&frame);
        }
        assert_eq!(
            CONVERSIONS.load(Ordering::Relaxed) - conversions,
            0,
            "the hidden steady state converts no pixel (the conversion lives in the paint path)"
        );
        assert_eq!(
            NOTIFY_EDGES.load(Ordering::Relaxed) - edges,
            0,
            "the hidden steady state posts no per-frame notification"
        );
    }

    #[test]
    fn an_identical_frame_is_never_republished() {
        let _guard = serial();
        let ui = Ui::test_ui();
        let frame = Frame::default();
        UI.preview_visible.store(true, Ordering::Relaxed);
        UI.slot.pending.store(false, Ordering::Relaxed);
        slot_guard().clear();
        let edges = NOTIFY_EDGES.load(Ordering::Relaxed);
        ui.show_frame(&frame);
        assert_eq!(
            NOTIFY_EDGES.load(Ordering::Relaxed) - edges,
            1,
            "the first new frame is exactly one notify edge"
        );
        ui.show_frame(&frame);
        ui.show_frame(&frame);
        assert_eq!(
            NOTIFY_EDGES.load(Ordering::Relaxed) - edges,
            1,
            "an identical frame stores nothing and notifies no more"
        );
        assert_eq!(slot_guard().len(), LOGICAL, "the slot holds one frame");
        assert!(
            slot_guard().iter().all(|&b| b == 0),
            "the identical frames stored nothing new"
        );
    }

    #[test]
    fn a_stalled_ui_thread_retains_only_the_newest_frame() {
        let _guard = serial();
        UI.slot.pending.store(false, Ordering::Relaxed);
        slot_guard().clear();
        let mut edges = 0;
        for i in 0..50u8 {
            let frame = Frame {
                pixels: [i; LOGICAL],
            };
            if UI.slot.publish(&frame.pixels) {
                edges += 1;
            }
        }
        assert_eq!(
            edges, 1,
            "50 publishes with a pending edge wake the UI thread once, not 50 times"
        );
        let capacity = slot_guard().capacity();
        for i in 50..150u8 {
            let frame = Frame {
                pixels: [i; LOGICAL],
            };
            assert!(!UI.slot.publish(&frame.pixels), "the edge stays coalesced");
        }
        assert_eq!(
            slot_guard().capacity(),
            capacity,
            "100 publishes reuse one allocation: the slot's capacity never grows"
        );
        let newest = UI.slot.take_pending().expect("the pending frame");
        assert_eq!(
            newest,
            vec![149u8; LOGICAL],
            "the consumer stall retains only the newest frame"
        );
    }

    #[test]
    fn the_pending_flag_is_cleared_before_the_slot_is_read() {
        let _guard = serial();
        UI.slot.pending.store(false, Ordering::Relaxed);
        slot_guard().clear();
        let first = Frame {
            pixels: [7u8; LOGICAL],
        };
        let second = Frame {
            pixels: [9u8; LOGICAL],
        };
        assert!(UI.slot.publish(&first.pixels), "a new frame is an edge");
        assert_eq!(
            UI.slot.take_pending().as_deref(),
            Some(&first.pixels[..]),
            "the first frame is observed"
        );
        assert!(
            UI.slot.publish(&second.pixels),
            "the next new frame is an edge"
        );
        assert_eq!(
            UI.slot.take_pending().as_deref(),
            Some(&second.pixels[..]),
            "the second frame is observed, so no frame is stranded between take and publish"
        );
        assert!(
            UI.slot.take_pending().is_none(),
            "a consumed slot posts no further edge"
        );
        let prod = production_source();
        let take = prod
            .find("fn take_pending(&self)")
            .expect("the slot's take exists");
        let take_body = &prod[take..take + prod[take..].find("\n    }").expect("the take's end")];
        let clear = take_body
            .find("pending.swap(false")
            .expect("pending is cleared in the take");
        let read = take_body
            .find("self.frame.lock()")
            .expect("the slot is read in the take");
        assert!(
            clear < read,
            "pending is cleared BEFORE the slot is read: a frame published between the read and \
             the clear keeps its edge, which is the unit-50 ordering rule"
        );
    }

    #[test]
    fn showing_the_preview_forces_a_paint_of_the_current_frame() {
        let _guard = serial();
        let ui = Ui::test_ui();
        let frame = Frame {
            pixels: [3u8; LOGICAL],
        };
        UI.preview_visible.store(false, Ordering::Relaxed);
        UI.slot.pending.store(false, Ordering::Relaxed);
        ui.show_frame(&frame);
        assert_eq!(
            slot_guard().as_slice(),
            &frame.pixels[..],
            "the hidden preview still keeps the newest frame, so a show paints it"
        );
        assert!(visibility_needs_paint(false, true));
        assert!(
            !visibility_needs_paint(true, true),
            "a repeat show paints twice"
        );
        assert!(
            !visibility_needs_paint(true, false),
            "hiding needs no paint"
        );
        assert!(!visibility_needs_paint(false, false));
        let edges = NOTIFY_EDGES.load(Ordering::Relaxed);
        ui.set_preview_visible(true);
        assert_eq!(
            NOTIFY_EDGES.load(Ordering::Relaxed) - edges,
            1,
            "the 0->1 visibility leg posts exactly one paint edge"
        );
        ui.set_preview_visible(true);
        assert_eq!(
            NOTIFY_EDGES.load(Ordering::Relaxed) - edges,
            1,
            "a visibility that did not change posts nothing"
        );
        let prod = production_source();
        let show = prod
            .find("pub fn set_preview_visible")
            .expect("the visibility leg");
        let auto = prod
            .find("pub fn set_preview_auto_visible")
            .expect("the auto leg");
        assert_eq!(
            prod[show..auto].matches("notify_frame()").count(),
            1,
            "the visibility leg notifies on the 0->1 edge"
        );
        let toggle = prod
            .find("unsafe fn toggle_preview")
            .expect("the tray toggle");
        let wide = prod.find("unsafe fn wide").expect("the tray menu helper");
        assert_eq!(
            prod[toggle..wide].matches("notify_frame()").count(),
            1,
            "the tray toggle notifies on the 0->1 edge, so a tray show paints the current frame"
        );
    }

    /// WO-OPT03 repair (F-AUD-OPT03-1): a publish that lands before the window
    /// exists is painted the moment the window appears. The Win32 legs cannot run
    /// in a unit test (no panel), so the test drives the shipped decision points
    /// with `hwnd` 0 (no window) and a non-zero sentinel handle, and counts the
    /// seams that separate an edge PRODUCED from a post DELIVERED.
    #[test]
    fn a_publish_before_the_window_exists_is_painted_when_the_window_appears() {
        let _guard = serial();
        let ui = Ui::test_ui();
        UI.hwnd.store(0, Ordering::SeqCst);
        UI.shutdown_requested.store(false, Ordering::SeqCst);
        UI.preview_visible.store(true, Ordering::Relaxed);
        UI.slot.pending.store(false, Ordering::Relaxed);
        slot_guard().clear();
        let attempted = NOTIFY_EDGES.load(Ordering::Relaxed);
        let delivered = POSTS_DELIVERED.load(Ordering::Relaxed);
        let frame = Frame {
            pixels: [11u8; LOGICAL],
        };
        ui.show_frame(&frame);
        assert_eq!(
            NOTIFY_EDGES.load(Ordering::Relaxed) - attempted,
            1,
            "the publish produced one notify edge"
        );
        assert_eq!(
            POSTS_DELIVERED.load(Ordering::Relaxed) - delivered,
            0,
            "with no window the post is produced but not delivered"
        );
        assert!(
            UI.slot.pending.load(Ordering::SeqCst),
            "an undeliverable post must not consume the coalesced edge"
        );
        // The window appears: store the handle, then the shipped handoff.
        UI.hwnd.store(NO_WINDOW, Ordering::SeqCst);
        unsafe { window_ready_handoff(HWND(NO_WINDOW as _), notify_frame) };
        assert_eq!(
            POSTS_DELIVERED.load(Ordering::Relaxed) - delivered,
            1,
            "the window-ready handoff delivers the pending edge (F-AUD-OPT03-1)"
        );
        // The delivered post runs the paint leg, which takes the newest frame.
        let taken = UI.slot.take_pending();
        assert_eq!(
            taken.as_deref(),
            Some(&frame.pixels[..]),
            "the handoff's post reaches the newest frame"
        );
        println!(
            "U53B-STARTUP-POSTS attempted={} delivered={} (startup sequence: publish at hwnd==0, then the window-ready handoff)",
            NOTIFY_EDGES.load(Ordering::Relaxed) - attempted,
            POSTS_DELIVERED.load(Ordering::Relaxed) - delivered
        );
        UI.hwnd.store(0, Ordering::SeqCst);
    }

    /// WO-OPT03 repair (F-AUD-OPT03-2): an exit requested before the window
    /// exists ends the message loop, so the shutdown's `join()` cannot block.
    #[test]
    fn shutdown_before_the_window_exists_ends_the_loop() {
        let _guard = serial();
        let ui = Ui::test_ui();
        UI.hwnd.store(0, Ordering::SeqCst);
        UI.shutdown_requested.store(false, Ordering::SeqCst);
        let destroys = DESTROY_POSTS.load(Ordering::Relaxed);
        ui.shutdown();
        assert!(
            UI.shutdown_requested.load(Ordering::SeqCst),
            "the shutdown arms the exit signal before it looks for a window"
        );
        assert_eq!(
            DESTROY_POSTS.load(Ordering::Relaxed) - destroys,
            0,
            "with no window the shutdown has nothing to post yet"
        );
        // The window is created microseconds later: the handoff honors the exit.
        UI.hwnd.store(NO_WINDOW, Ordering::SeqCst);
        unsafe { window_ready_handoff(HWND(NO_WINDOW as _), notify_frame) };
        assert_eq!(
            DESTROY_POSTS.load(Ordering::Relaxed) - destroys,
            1,
            "the handoff posts the WM_DESTROY that ends the loop (F-AUD-OPT03-2)"
        );
        let prod = production_source();
        let handoff = prod
            .find("unsafe fn window_ready_handoff")
            .expect("the handoff exists");
        let body =
            &prod[handoff..handoff + prod[handoff..].find("\n}").expect("the handoff's end")];
        let exit_check = body
            .find("shutdown_requested.load")
            .expect("the exit signal is checked");
        let frame_notify = body.find("notify()").expect("the frame notify exists");
        assert!(
            exit_check < frame_notify,
            "the shutdown check precedes the frame notify: a shutdown is never undone by a paint"
        );
        UI.hwnd.store(0, Ordering::SeqCst);
        UI.shutdown_requested.store(false, Ordering::SeqCst);
    }

    /// WO-OPT03 repair (F-AUD-OPT03-5): the positive control for the paint seam.
    /// This test asserts the CONVERSIONS counter; the shipped hidden-preview test
    /// asserts the same counter's delta of 0, which is vacuous while nothing ever
    /// reaches the converter, so this row is what makes that 0 meaningful.
    #[test]
    fn the_visible_paint_path_performs_exactly_one_conversion() {
        let _guard = serial();
        let ui = Ui::test_ui();
        UI.hwnd.store(0, Ordering::SeqCst);
        UI.preview_visible.store(true, Ordering::Relaxed);
        UI.slot.pending.store(false, Ordering::Relaxed);
        slot_guard().clear();
        UI.pixels.lock().unwrap_or_else(|e| e.into_inner()).clear();
        ui.show_frame(&Frame {
            pixels: [5u8; LOGICAL],
        });
        let conversions = CONVERSIONS.load(Ordering::Relaxed);
        {
            // The shipped `WM_APP_FRAME` leg, in its shipped lock order.
            let mut pixels = UI.pixels.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(frame) = UI.slot.take_pending() {
                convert_frame(&mut pixels, &frame);
            }
        }
        assert_eq!(
            CONVERSIONS.load(Ordering::Relaxed) - conversions,
            1,
            "one visible paint performs exactly one BGRA conversion"
        );
        assert_eq!(
            UI.pixels.lock().unwrap_or_else(|e| e.into_inner()).len(),
            (FRAME_W * FRAME_H * 4) as usize,
            "the conversion produces exactly the DIB length"
        );
        // Positive control: delete the seam from the leg and these rows go red.
        let prod = production_source();
        let leg_start = prod.find("WM_APP_FRAME => {").expect("the notify leg");
        let leg = &prod[leg_start
            ..leg_start
                + prod[leg_start..]
                    .find("WM_PAINT =>")
                    .expect("the paint leg")];
        assert_eq!(
            leg.matches("take_pending(").count(),
            1,
            "the leg reads the slot: the paint seam is wired"
        );
        assert_eq!(
            leg.matches("convert_frame(").count(),
            1,
            "the leg converts: delete the seam and this row is the red"
        );
    }

    /// WO-OPT03 repair (F-AUD-OPT03-6): the paint path carries the frame in the
    /// slot's own allocation, so 20 paints allocate no payload.
    #[test]
    fn the_visible_paint_path_allocates_no_payload_per_paint() {
        let _guard = serial();
        let ui = Ui::test_ui();
        UI.hwnd.store(0, Ordering::SeqCst);
        UI.preview_visible.store(true, Ordering::Relaxed);
        UI.slot.pending.store(false, Ordering::Relaxed);
        slot_guard().clear();
        let mut pixels = UI.pixels.lock().unwrap_or_else(|e| e.into_inner());
        convert_frame(&mut pixels, &[7u8; LOGICAL]);
        let paint_capacity = pixels.capacity();
        let slot_capacity = UI
            .slot
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capacity();
        for i in 0..20u8 {
            ui.show_frame(&Frame {
                pixels: [i; LOGICAL],
            });
            if let Some(frame) = UI.slot.take_pending() {
                convert_frame(&mut pixels, &frame);
            }
        }
        assert_eq!(
            pixels.capacity(),
            paint_capacity,
            "the 27,520 B paint buffer is grown once: no payload allocation per paint"
        );
        assert_eq!(
            pixels.len(),
            (FRAME_W * FRAME_H * 4) as usize,
            "every paint conversion writes the full DIB"
        );
        assert_eq!(
            UI.slot
                .frame
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .capacity(),
            slot_capacity,
            "the slot reuses one 6,880 B allocation"
        );
        let prod = production_source();
        let take = prod
            .find("fn take_pending(&self)")
            .expect("the slot's take exists");
        let body = &prod[take..take + prod[take..].find("\n    }").expect("the take's end")];
        assert_eq!(
            body.matches(".clone()").count(),
            0,
            "the take hands back a view, not a 6,880 B clone (F-AUD-OPT03-6)"
        );
    }

    /// WO-OPT03 structural pins in the lane's repo form: the per-frame `Vec`
    /// allocation, the unbounded channel and the frame-pump thread are gone, the
    /// conversion sits in the paint path only, the notify post is the shipped
    /// `WM_APP_FRAME`, and the shutdown ends the loop explicitly.
    #[test]
    fn the_frame_path_source_pins_the_slot_the_paint_and_the_shutdown() {
        let prod = production_source();
        let production = prod.split("mod tests {").next().unwrap_or_default();
        assert_eq!(
            production
                .matches("Vec::with_capacity((FRAME_W * FRAME_H * 4)")
                .count(),
            0,
            "show_frame allocates no 27,520 B BGRA Vec any more"
        );
        assert_eq!(
            production.matches("mpsc::channel::<Vec<u8>>").count(),
            0,
            "the unbounded frame channel is gone"
        );
        assert_eq!(
            production.matches("frame_rx").count(),
            0,
            "no frame receiver survives: the pump thread is removed"
        );
        assert_eq!(
            production.matches("while let Ok(bgra)").count(),
            0,
            "the pump's receive loop is gone"
        );
        assert_eq!(
            production.matches("to_le_bytes()").count(),
            1,
            "the file has exactly one pixel-conversion loop"
        );
        assert_eq!(
            production.matches("convert_frame(").count(),
            2,
            "one converter definition and one call site"
        );
        let notify_leg = production
            .find("WM_APP_FRAME => {")
            .expect("the notify leg exists");
        let leg = &production[notify_leg
            ..notify_leg
                + production[notify_leg..]
                    .find("WM_PAINT =>")
                    .expect("the paint leg")];
        assert_eq!(
            leg.matches("convert_frame(").count(),
            1,
            "the conversion runs in the paint path, never on the app thread"
        );
        assert_eq!(
            leg.matches("preview_visible.load").count(),
            1,
            "the paint path consults visibility before it converts"
        );
        assert_eq!(
            leg.matches("take_pending(").count(),
            1,
            "the paint path takes the slot"
        );
        assert_eq!(
            production
                .matches("PostMessageW(HWND(hwnd as _), WM_APP_FRAME, WPARAM(0), LPARAM(0))")
                .count(),
            1,
            "the notify is the shipped cross-thread WM_APP_FRAME post"
        );
        assert_eq!(
            production.matches("notify_frame()").count(),
            4,
            "the notify fires from the publisher, the visibility leg and the tray toggle"
        );
        let shut = production
            .find("pub fn shutdown(mut self)")
            .expect("the shutdown exists");
        let shut_body = &production[shut..shut
            + production[shut..]
                .find("\n    }")
                .expect("the shutdown's end")];
        assert!(
            shut_body.contains("PostMessageW(HWND(hwnd as _), WM_DESTROY, WPARAM(0), LPARAM(0))"),
            "the shutdown posts the WM_DESTROY that ends the message loop"
        );
        assert!(
            shut_body.contains("join()"),
            "the shutdown joins the UI thread"
        );
        assert_eq!(
            production.matches("WM_APP + 1").count(),
            1,
            "WM_APP_FRAME stays WM_APP + 1"
        );
        assert_eq!(
            production
                .matches("pixels.len() == (FRAME_W * FRAME_H * 4) as usize")
                .count(),
            1,
            "the WM_PAINT leg keeps its shipped guard"
        );
        assert_eq!(
            production.matches("StretchDIBits(").count(),
            1,
            "the shipped BITMAPINFO/StretchDIBits paint form stays"
        );
    }
}
