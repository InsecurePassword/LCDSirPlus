//! Virtual preview window (scaled 160x43 dashboard) and tray icon.
//!
//! The preview replicates the physical button contract: left-click a slot
//! simulates its short press (cycle forward), right-click cycles backward.
//! The UI thread owns all Win32 window state; the app communicates through
//! channels only.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadCursorW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, ShowWindow, TrackPopupMenu,
    TranslateMessage, CW_USEDEFAULT, HMENU, IDC_ARROW, IDI_APPLICATION, MENU_ITEM_FLAGS, MSG,
    SW_HIDE, SW_SHOW, TRACK_POPUP_MENU_FLAGS, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_CREATE, WM_DESTROY, WM_LBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

pub const FRAME_W: u32 = crate::model::WIDTH as u32;
pub const FRAME_H: u32 = crate::model::HEIGHT as u32;
const WM_APP_FRAME: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const ID_MENU_SHOW: usize = 1001;
const ID_MENU_EXIT: usize = 1002;

/// Events the UI sends back to the application.
#[derive(Clone, Copy, Debug)]
pub enum UiEvent {
    /// Preview click: cycle the given slot (physical button index 0..4).
    SlotCycle { slot: usize, backward: bool },
    /// User requested exit from the tray menu.
    Exit,
}

struct UiShared {
    /// BGRA pixel buffer (160x43), rebuilt on every frame.
    pixels: Mutex<Vec<u8>>,
    scale: AtomicU32,
    preview_visible: AtomicBool,
    manual_visibility: AtomicBool,
    event_tx: OnceLock<Sender<UiEvent>>,
    hwnd: AtomicIsize,
}

static UI: UiShared = UiShared {
    pixels: Mutex::new(Vec::new()),
    scale: AtomicU32::new(4),
    preview_visible: AtomicBool::new(false),
    manual_visibility: AtomicBool::new(false),
    event_tx: OnceLock::new(),
    hwnd: AtomicIsize::new(0),
};

/// Handle for the application to drive the UI thread.
pub struct Ui {
    frame_tx: Sender<Vec<u8>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ui {
    /// Spawn the UI thread with a tray icon; the preview window appears per
    /// `show_preview` (config `preview_mode` resolution).
    pub fn spawn(scale: u32, show_preview: bool, event_tx: Sender<UiEvent>) -> Ui {
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        UI.scale.store(scale.max(1), Ordering::Relaxed);
        UI.preview_visible.store(show_preview, Ordering::Relaxed);
        UI.manual_visibility.store(false, Ordering::Relaxed);
        let _ = UI.event_tx.set(event_tx);
        let thread = std::thread::Builder::new()
            .name("lcdforge-ui".into())
            .spawn(move || ui_main(frame_rx))
            .expect("spawn ui thread");
        Ui {
            frame_tx,
            thread: Some(thread),
        }
    }

    /// Push a logical frame (160x43 bytes) to the preview.
    pub fn show_frame(&self, frame: &crate::render::Frame) {
        let mut bgra = Vec::with_capacity((FRAME_W * FRAME_H * 4) as usize);
        for &p in &frame.pixels {
            let v: u32 = if p != 0 { 0x00000000 } else { 0x00FF_FFFF };
            bgra.extend_from_slice(&v.to_le_bytes());
        }
        let _ = self.frame_tx.send(bgra);
    }

    #[allow(dead_code)] // tray toggle drives visibility directly; API kept for config changes
    pub fn set_preview_visible(&self, visible: bool) {
        if UI.preview_visible.swap(visible, Ordering::Relaxed) == visible {
            return;
        }
        unsafe {
            let hwnd = UI.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                let _ = ShowWindow(HWND(hwnd as _), if visible { SW_SHOW } else { SW_HIDE });
            }
        }
    }

    pub fn set_preview_auto_visible(&self, visible: bool) {
        if !UI.manual_visibility.load(Ordering::Relaxed) {
            self.set_preview_visible(visible);
        }
    }

    pub fn shutdown(mut self) {
        drop(self.frame_tx);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ui_main(frame_rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let class_name: Vec<u16> = "LCDFORGE_PREVIEW\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        // Frame pump thread: latest frame wins; wake the UI thread.
        std::thread::spawn(move || {
            while let Ok(bgra) = frame_rx.recv() {
                if let Ok(mut slot) = UI.pixels.lock() {
                    *slot = bgra;
                }
                let hwnd = UI.hwnd.load(Ordering::Relaxed);
                if hwnd != 0 {
                    let _ = PostMessageW(HWND(hwnd as _), WM_APP_FRAME, WPARAM(0), LPARAM(0));
                }
            }
            // Channel closed: app is shutting down; end the message loop.
            let hwnd = UI.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                let _ = PostMessageW(HWND(hwnd as _), WM_DESTROY, WPARAM(0), LPARAM(0));
            }
        });

        let scale = UI.scale.load(Ordering::Relaxed) as i32;
        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false);
        let width = (FRAME_W as i32 * scale) + (rect.right - rect.left);
        let height = (FRAME_H as i32 * scale) + (rect.bottom - rect.top);
        let window_name: Vec<u16> = "LCDForge Preview\0".encode_utf16().collect();
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(window_name.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            None,
            None,
            HINSTANCE::from(hinstance),
            None,
        ) else {
            return;
        };
        UI.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);

        add_tray_icon(hwnd);

        let visible = UI.preview_visible.load(Ordering::Relaxed);
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_tray_icon(hwnd);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => LRESULT(0),
        WM_APP_FRAME => {
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let scale = UI.scale.load(Ordering::Relaxed) as i32;
            if let Ok(pixels) = UI.pixels.lock() {
                if pixels.len() == (FRAME_W * FRAME_H * 4) as usize {
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
                    let client_w = FRAME_W as i32 * scale;
                    let client_h = FRAME_H as i32 * scale;
                    let _ = SetDIBitsToDevice(
                        hdc,
                        0,
                        0,
                        client_w as u32,
                        client_h as u32,
                        0,
                        0,
                        0,
                        FRAME_H,
                        pixels.as_ptr() as *const core::ffi::c_void,
                        &bmi,
                        DIB_RGB_COLORS,
                    );
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
                let _ = tx.send(UiEvent::SlotCycle { slot, backward });
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
                    let _ = tx.send(UiEvent::Exit);
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
                let _ = tx.send(UiEvent::Exit);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn toggle_preview(hwnd: HWND) {
    let visible = !UI.preview_visible.load(Ordering::Relaxed);
    UI.manual_visibility.store(true, Ordering::Relaxed);
    UI.preview_visible.store(visible, Ordering::Relaxed);
    let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
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

unsafe fn add_tray_icon(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW {
        hWnd: hwnd,
        uID: 1,
        uFlags: NOTIFY_ICON_DATA_FLAGS(0x01 | 0x02 | 0x04), // MESSAGE | ICON | TIP
        uCallbackMessage: WM_APP_TRAY,
        hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        ..Default::default()
    };
    for (i, c) in "LCDForge\0".encode_utf16().enumerate() {
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
