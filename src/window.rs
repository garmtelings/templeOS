//! The TempleOS window: shows the frames the VM thread renders.
//!
//! - Windowed: the picture at the largest whole-number scale that fits the
//!   client area (sharp pixels), centered on black. Below 1x it shrinks to fit.
//! - Fullscreen (host key + F or Enter toggles): borderless on the current
//!   monitor, as large as fits with the shape a real monitor gives it (see
//!   [`Frame::fit`]).
//!
//! Input, as in VirtualBox: the keyboard goes to TempleOS whenever the
//! window has focus (read with Raw Input, so Alt, F10 and Pause arrive as
//! real key events and the window menu never opens). Clicking the picture
//! captures the mouse (hidden and confined to the window); the host key,
//! Right Ctrl, releases it. The host key itself and host key combinations
//! never reach the guest. Keys still held when the window loses focus are
//! released in the guest, so nothing sticks.
//!
//! The VM thread never touches the window: it stores each frame in [`Shared`]
//! and posts a message; painting happens here, on the UI thread. Input goes
//! the other way through [`Shared::input`].

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use devices::input::{InputEvent, InputQueue};
use devices::keymap::{HostKey, KeyMapper};
use devices::vga_render::Frame;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, FillRect, GetMonitorInfoW, GetStockObject,
    InvalidateRect, MonitorFromWindow, SetStretchBltMode, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH, COLORONCOLOR, DIB_RGB_COLORS, HBRUSH, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS, RAWINPUTHEADER, RAWMOUSE, RID_INPUT, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE,
};
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
    /// Keyboard and mouse events for the guest.
    pub input: Arc<InputQueue>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Shared {
            frame: Mutex::new(Frame::default()),
            frame_posted: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            exit_error: Mutex::new(None),
            hwnd: Mutex::new(None),
            input: Arc::new(InputQueue::new()),
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
    keymap: KeyMapper,
    /// Keys the guest has seen pressed and not released.
    pressed: HashSet<HostKey>,
    /// Keys used in a host key combination: their release is swallowed too.
    host_consumed: HashSet<HostKey>,
    /// The host key is down; `host_used` if another key was pressed with it.
    host_down: bool,
    host_used: bool,
    /// The mouse is captured: hidden, confined, and its motion goes to the guest.
    captured: bool,
    /// Mouse buttons the guest has seen held (PS/2 bit order).
    buttons: u8,
    /// Wheel movement not yet a whole notch (high-resolution wheels).
    wheel_rest: i32,
    /// Last absolute position (remote desktop sessions report absolute).
    last_abs: Option<(i32, i32)>,
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
            *ui.borrow_mut() = Some(UiState {
                shared: shared.clone(),
                frame: Frame::default(),
                fullscreen: None,
                keymap: KeyMapper::new(),
                pressed: HashSet::new(),
                host_consumed: HashSet::new(),
                host_down: false,
                host_used: false,
                captured: false,
                buttons: 0,
                wheel_rest: 0,
                last_abs: None,
            });
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            TITLE_FREE,
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
        // Keyboard (usage 6) and mouse (usage 2) raw input while focused.
        let devices = [2u16, 6].map(|usage| RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: usage,
            dwFlags: RAWINPUTDEVICE_FLAGS(0),
            hwndTarget: hwnd,
        });
        RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            .map_err(|e| format!("RegisterRawInputDevices: {e}"))?;
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
            WM_INPUT => {
                raw_input(hwnd, HRAWINPUT(lparam.0 as _));
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // Keys arrive through WM_INPUT. Swallowing the legacy messages
            // keeps Alt/F10 from opening the window menu and stops beeps.
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_DEADCHAR
            | WM_SYSDEADCHAR => LRESULT(0),
            WM_LBUTTONDOWN => {
                capture(hwnd, true);
                LRESULT(0)
            }
            WM_SETCURSOR if captured() && (lparam.0 & 0xffff) as u32 == HTCLIENT => {
                SetCursor(None);
                LRESULT(1)
            }
            WM_SIZE | WM_MOVE => {
                if captured() {
                    clip_to_client(hwnd);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_KILLFOCUS => {
                capture(hwnd, false);
                release_all_keys();
                LRESULT(0)
            }
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
                let _ = ClipCursor(None);
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
    if captured() {
        clip_to_client(hwnd);
    }
    let _ = InvalidateRect(hwnd, None, false);
}

const TITLE_FREE: PCWSTR = w!("TempleOS - click to capture the mouse");
const TITLE_CAPTURED: PCWSTR = w!("TempleOS - Right Ctrl releases the mouse");

fn captured() -> bool {
    UI.with(|ui| ui.borrow().as_ref().is_some_and(|ui| ui.captured))
}

/// Capture or release the mouse. Releasing lets go of any buttons the guest
/// thinks are held.
unsafe fn capture(hwnd: HWND, on: bool) {
    let changed = UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let Some(ui) = ui.as_mut() else { return false };
        if ui.captured == on {
            return false;
        }
        ui.captured = on;
        ui.last_abs = None;
        if !on && ui.buttons != 0 {
            ui.buttons = 0;
            ui.shared.input.push(InputEvent::Mouse { dx: 0, dy: 0, dz: 0, buttons: 0 });
        }
        true
    });
    if !changed {
        return;
    }
    if on {
        clip_to_client(hwnd);
        SetCursor(None);
        let _ = SetWindowTextW(hwnd, TITLE_CAPTURED);
    } else {
        let _ = ClipCursor(None);
        if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
            SetCursor(arrow);
        }
        let _ = SetWindowTextW(hwnd, TITLE_FREE);
    }
}

unsafe fn clip_to_client(hwnd: HWND) {
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    let mut tl = POINT { x: r.left, y: r.top };
    let mut br = POINT { x: r.right, y: r.bottom };
    let _ = ClientToScreen(hwnd, &mut tl);
    let _ = ClientToScreen(hwnd, &mut br);
    let _ = ClipCursor(Some(&RECT { left: tl.x, top: tl.y, right: br.x, bottom: br.y }));
}

/// Send a release for every key the guest has seen pressed.
fn release_all_keys() {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let Some(ui) = ui.as_mut() else { return };
        let keys: Vec<HostKey> = ui.pressed.drain().collect();
        for k in keys {
            let bytes = ui.keymap.translate(k.code, k.e0, false, false);
            if !bytes.is_empty() {
                ui.shared.input.push(InputEvent::Key(bytes));
            }
        }
        ui.host_down = false;
        ui.host_consumed.clear();
    });
}

/// What the host key combinations do.
enum HostAction {
    None,
    ReleaseMouse,
    ToggleFullscreen,
}

unsafe fn raw_input(hwnd: HWND, handle: HRAWINPUT) {
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let n = GetRawInputData(
        handle,
        RID_INPUT,
        Some(&mut raw as *mut RAWINPUT as _),
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    if n == u32::MAX || n == 0 {
        return;
    }
    let action = UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let Some(ui) = ui.as_mut() else { return HostAction::None };
        if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
            let k = raw.data.keyboard;
            // 0xFF: keyboard overrun; 0: keys without a scan code (media keys).
            if k.MakeCode == 0 || k.MakeCode > 0x7f {
                return HostAction::None;
            }
            let pressed = k.Flags & RI_KEY_BREAK as u16 == 0;
            let e0 = k.Flags & RI_KEY_E0 as u16 != 0;
            let e1 = k.Flags & RI_KEY_E1 as u16 != 0;
            key_event(ui, HostKey { code: k.MakeCode as u8, e0 }, e1, pressed)
        } else if raw.header.dwType == RIM_TYPEMOUSE.0 {
            if ui.captured {
                mouse_event(ui, &raw.data.mouse);
            }
            HostAction::None
        } else {
            HostAction::None
        }
    });
    match action {
        HostAction::None => {}
        HostAction::ReleaseMouse => capture(hwnd, false),
        HostAction::ToggleFullscreen => toggle_fullscreen(hwnd),
    }
}

fn key_event(ui: &mut UiState, key: HostKey, e1: bool, pressed: bool) -> HostAction {
    if key == HostKey::RIGHT_CTRL && !e1 {
        if pressed {
            if !ui.host_down {
                ui.host_down = true;
                ui.host_used = false;
            }
            return HostAction::None;
        }
        ui.host_down = false;
        return if ui.host_used { HostAction::None } else { HostAction::ReleaseMouse };
    }
    if ui.host_down && pressed {
        ui.host_used = true;
        ui.host_consumed.insert(key);
        return match (key.code, key.e0) {
            (0x21, false) | (0x1c, _) => HostAction::ToggleFullscreen, // F, Enter
            _ => HostAction::None,
        };
    }
    if !pressed && ui.host_consumed.remove(&key) {
        return HostAction::None;
    }
    let bytes = ui.keymap.translate(key.code, key.e0, e1, pressed);
    if !e1 {
        if pressed {
            ui.pressed.insert(key);
        } else {
            ui.pressed.remove(&key);
        }
    }
    if !bytes.is_empty() {
        ui.shared.input.push(InputEvent::Key(bytes));
    }
    HostAction::None
}

fn mouse_event(ui: &mut UiState, m: &RAWMOUSE) {
    let (mut dx, mut dy) = (m.lLastX, m.lLastY);
    if m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        // Absolute 0..65535 over the (virtual) screen: turn into deltas.
        // SAFETY: plain metric queries.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
        let (x, y) = (m.lLastX * w / 65536, m.lLastY * h / 65536);
        (dx, dy) = match ui.last_abs.replace((x, y)) {
            Some((px, py)) => (x - px, y - py),
            None => (0, 0),
        };
    }
    // SAFETY: the button fields of the RAWMOUSE union.
    let (flags, data) = unsafe { (m.Anonymous.Anonymous.usButtonFlags, m.Anonymous.Anonymous.usButtonData) };
    let flags = u32::from(flags);
    let mut buttons = ui.buttons;
    for (bit, down, up) in [
        (0, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP),
        (1, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP),
        (2, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP),
        (3, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP),
        (4, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP),
    ] {
        if flags & down != 0 {
            buttons |= 1 << bit;
        }
        if flags & up != 0 {
            buttons &= !(1 << bit);
        }
    }
    let mut dz = 0;
    if flags & RI_MOUSE_WHEEL != 0 {
        // Windows: positive = away from the user, 120 per notch.
        // PS/2: positive = towards the user.
        ui.wheel_rest -= i32::from(data as i16);
        dz = ui.wheel_rest / 120;
        ui.wheel_rest %= 120;
    }
    if dx == 0 && dy == 0 && dz == 0 && buttons == ui.buttons {
        return;
    }
    ui.buttons = buttons;
    // PS/2 Y grows upwards; the screen's grows downwards.
    ui.shared.input.push(InputEvent::Mouse { dx, dy: -dy, dz, buttons });
}

/// Show an error before any window exists (e.g. WHPX missing when started
/// by double-click, where there is no console to print to).
pub fn error_box(text: &str) {
    // SAFETY: plain Win32 call with valid strings.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("TempleOS"), MB_ICONERROR | MB_OK);
    }
}
