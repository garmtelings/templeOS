//! The TempleOS window: shows the frames the VM thread renders.
//!
//! - Windowed: the picture at the largest whole-number scale that fits the
//!   client area (sharp pixels), centered on black. Below 1x it shrinks to fit.
//! - Fullscreen (Alt+Enter toggles): borderless on the current monitor, as
//!   large as fits with the shape a real monitor gives it: 4:3 for VGA text
//!   and graphics modes (a 720x400 text screen filled a 4:3 CRT too), square
//!   pixels for VBE modes.
//!
//! The VM thread never touches the window: it stores each frame in [`Shared`]
//! and posts a message; painting happens here, on the UI thread.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use devices::vga_render::Frame;
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect,
    MonitorFromWindow, SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLACK_BRUSH, COLORONCOLOR, DIB_RGB_COLORS, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted by the VM thread when a new frame is in [`Shared::frame`].
const WM_APP_FRAME: u32 = WM_APP;
/// Posted by the VM thread when the machine has stopped for good.
const WM_APP_VM_EXIT: u32 = WM_APP + 1;

/// State shared between the UI thread and the VM thread.
pub struct Shared {
    frame: Mutex<Frame>,
    /// A frame message is in the queue; don't post another.
    frame_posted: AtomicBool,
    /// Set by the UI when the window closes; the VM thread stops.
    pub stop: Arc<AtomicBool>,
    /// Why the VM stopped, shown to the user if it was an error.
    exit_error: Mutex<Option<String>>,
    hwnd: Mutex<Option<usize>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Shared {
            frame: Mutex::new(Frame::default()),
            frame_posted: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            exit_error: Mutex::new(None),
            hwnd: Mutex::new(None),
        })
    }

    /// Called on the VM thread with each rendered frame.
    pub fn present(&self, frame: &Frame) {
        self.frame.lock().unwrap().clone_from(frame);
        if !self.frame_posted.swap(true, Ordering::AcqRel) {
            self.post(WM_APP_FRAME);
        }
    }

    /// Called on the VM thread when it's done; `error` is shown in a message box.
    pub fn vm_exited(&self, error: Option<String>) {
        *self.exit_error.lock().unwrap() = error;
        self.post(WM_APP_VM_EXIT);
    }

    fn post(&self, msg: u32) {
        if let Some(h) = *self.hwnd.lock().unwrap() {
            // SAFETY: posting to a window handle is thread safe; if the window
            // is already gone the call just fails.
            let _ = unsafe { PostMessageW(HWND(h as _), msg, WPARAM(0), LPARAM(0)) };
        }
    }
}

struct UiState {
    shared: Arc<Shared>,
    /// Pixels of the frame being shown (copied out of Shared on WM_APP_FRAME).
    frame: Frame,
    fullscreen: Option<WINDOWPLACEMENT>,
}

thread_local! {
    static UI: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

/// Open the window and run the message loop until it closes. The caller
/// starts the VM thread with `shared` (after this has registered the
/// window, `start` is called with it).
pub fn run(shared: Arc<Shared>, fullscreen: bool, start: impl FnOnce()) -> Result<(), String> {
    // SAFETY: plain Win32 calls on the UI thread; every pointer passed is to
    // a live local.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = w!("TempleOSWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW failed".into());
        }

        // 2x of 640x480 when it fits the work area, else 1x.
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as _), Default::default());
        let scale = if work.right - work.left >= 1400 && work.bottom - work.top >= 1060 { 2 } else { 1 };
        let mut rect = RECT { left: 0, top: 0, right: 640 * scale, bottom: 480 * scale };
        let style = WS_OVERLAPPEDWINDOW;
        let _ = AdjustWindowRect(&mut rect, style, false);

        UI.with(|ui| {
            *ui.borrow_mut() = Some(UiState { shared: shared.clone(), frame: Frame::default(), fullscreen: None });
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("TempleOS"),
            style | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            instance,
            None,
        )
        .map_err(|e| e.to_string())?;
        *shared.hwnd.lock().unwrap() = Some(hwnd.0 as usize);
        if fullscreen {
            toggle_fullscreen(hwnd);
        }
        start();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        *shared.hwnd.lock().unwrap() = None;
    }
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by Windows on the UI thread with a valid window.
    unsafe {
        match msg {
            WM_APP_FRAME => {
                UI.with(|ui| {
                    if let Some(ui) = ui.borrow_mut().as_mut() {
                        ui.shared.frame_posted.store(false, Ordering::Release);
                        ui.frame.clone_from(&ui.shared.frame.lock().unwrap());
                    }
                });
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_APP_VM_EXIT => {
                let error = UI.with(|ui| ui.borrow().as_ref().and_then(|ui| ui.shared.exit_error.lock().unwrap().take()));
                if let Some(e) = error {
                    MessageBoxW(hwnd, &HSTRING::from(e), w!("TempleOS"), MB_ICONERROR | MB_OK);
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SYSKEYDOWN if wparam.0 as u16 == VK_RETURN.0 && lparam.0 & (1 << 29) != 0 => {
                toggle_fullscreen(hwnd);
                LRESULT(0)
            }
            // Alt+Enter would otherwise beep.
            WM_SYSCHAR if wparam.0 == '\r' as usize => LRESULT(0),
            WM_CLOSE => {
                UI.with(|ui| {
                    if let Some(ui) = ui.borrow().as_ref() {
                        ui.shared.stop.store(true, Ordering::Release);
                    }
                });
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
    UI.with(|ui| {
        let ui = ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        let f = &ui.frame;
        let (x, y, w, h) = f.fit(client.right, client.bottom, ui.fullscreen.is_some());
        let dst = RECT { left: x, top: y, right: x + w, bottom: y + h };
        // Black borders around the picture (the picture itself is opaque).
        for r in [
            RECT { left: 0, top: 0, right: client.right, bottom: dst.top },
            RECT { left: 0, top: dst.bottom, right: client.right, bottom: client.bottom },
            RECT { left: 0, top: dst.top, right: dst.left, bottom: dst.bottom },
            RECT { left: dst.right, top: dst.top, right: client.right, bottom: dst.bottom },
        ] {
            if r.right > r.left && r.bottom > r.top {
                FillRect(hdc, &r, black);
            }
        }
        if f.pixels.is_empty() || dst.right <= dst.left {
            return;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: f.width as i32,
                biHeight: -(f.height as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        SetStretchBltMode(hdc, COLORONCOLOR);
        StretchDIBits(
            hdc,
            dst.left,
            dst.top,
            dst.right - dst.left,
            dst.bottom - dst.top,
            0,
            0,
            f.width as i32,
            f.height as i32,
            Some(f.pixels.as_ptr() as _),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    });
    let _ = EndPaint(hwnd, &ps);
}

/// Switch between a normal window and borderless fullscreen on the
/// window's monitor, restoring the old placement on the way back.
unsafe fn toggle_fullscreen(hwnd: HWND) {
    let saved = UI.with(|ui| ui.borrow_mut().as_mut().and_then(|ui| ui.fullscreen.take()));
    if let Some(placement) = saved {
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as isize);
        let _ = SetWindowPlacement(hwnd, &placement);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
        );
    } else {
        let mut placement = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(hwnd, &mut placement);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        let m = mi.rcMonitor;
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        let _ = SetWindowPos(
            hwnd,
            HWND_TOP,
            m.left,
            m.top,
            m.right - m.left,
            m.bottom - m.top,
            SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
        );
        UI.with(|ui| {
            if let Some(ui) = ui.borrow_mut().as_mut() {
                ui.fullscreen = Some(placement);
            }
        });
    }
    let _ = InvalidateRect(hwnd, None, false);
}

/// Show an error before any window exists (e.g. WHPX missing when started
/// by double-click, where there is no console to print to).
pub fn error_box(text: &str) {
    // SAFETY: plain Win32 call with valid strings.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("TempleOS"), MB_ICONERROR | MB_OK);
    }
}
