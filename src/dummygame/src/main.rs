// Fake game process: Discord detects it by its exe name/path, so all it does is show a window.
// It gets copied and renamed to the real game's exe (e.g. games/<appId>/win64/cs2.exe).
// Usage: <exe> "<display name>" [minutes until it closes itself]
#![windows_subsystem = "windows"]

use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static NAME: OnceLock<String> = OnceLock::new();
static DEADLINE: OnceLock<Option<Instant>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn deadline() -> Option<Instant> {
    DEADLINE.get().copied().flatten()
}

fn label() -> Vec<u16> {
    let name = NAME.get().unwrap();
    match deadline() {
        Some(at) => {
            let left = at.saturating_duration_since(Instant::now()).as_secs();
            wide(&format!("{name} (fake process for Discord)\ncloses in {}:{:02}", left / 60, left % 60))
        }
        None => wide(&format!("{name} (fake process for Discord)")),
    }
}

fn main() {
    // Same contract as the old .NET DummyGame: argv[1] is the display name.
    let mut args = std::env::args().skip(1);
    let name = args.next().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "Game".into());
    let minutes = args.next().and_then(|m| m.parse::<f64>().ok()).filter(|m| *m > 0.0);
    let at = minutes
        .and_then(|m| Duration::try_from_secs_f64(m * 60.0).ok())
        .and_then(|d| Instant::now().checked_add(d));
    NAME.get_or_init(|| name.clone());
    DEADLINE.get_or_init(|| at);
    let title = wide(&name);
    let class = wide("DummyGame");

    unsafe {
        let hinstance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);

        let (w, h) = (480, 200);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            (GetSystemMetrics(SM_CXSCREEN) - w) / 2,
            (GetSystemMetrics(SM_CYSCREEN) - h) / 2,
            w,
            h,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        );
        if at.is_some() {
            // Ticks the countdown and closes the window when time is up.
            SetTimer(hwnd, 1, 1000, None);
        }

        let mut msg = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let face = wide("Segoe UI");
            let font = CreateFontW(-15, 0, 0, 0, FW_NORMAL as _, 0, 0, 0, DEFAULT_CHARSET as _, 0, 0, CLEARTYPE_QUALITY as _, 0, face.as_ptr());
            let old = SelectObject(hdc, font);
            SetBkMode(hdc, TRANSPARENT as _);
            SetTextColor(hdc, GetSysColor(COLOR_BTNTEXT));
            // Measure the (possibly two-line) text first so it can be centered vertically.
            let text = label();
            let mut measured = rc;
            DrawTextW(hdc, text.as_ptr(), -1, &mut measured, DT_CENTER | DT_WORDBREAK | DT_CALCRECT);
            rc.top = (rc.bottom - (measured.bottom - measured.top)) / 2;
            DrawTextW(hdc, text.as_ptr(), -1, &mut rc, DT_CENTER | DT_WORDBREAK);
            DeleteObject(SelectObject(hdc, old));
            EndPaint(hwnd, &ps);
            0
        }
        WM_TIMER => {
            if deadline().is_some_and(|at| Instant::now() >= at) {
                DestroyWindow(hwnd);
            } else {
                InvalidateRect(hwnd, null(), 1);
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
