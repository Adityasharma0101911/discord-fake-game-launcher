// Fake game process: Discord detects it by its exe name/path, so all it does is show a window.
// It gets copied and renamed to the real game's exe (e.g. games/<appId>/win64/cs2.exe).
// Usage: <exe> "<display name>" [minutes until it closes itself] [--rpc=<discord application id>]
// --rpc is for games without a Windows exe: the game is shown through Rich Presence over Discord's local IPC pipe.
// A custom status (details, score, party...) comes from <this exe>.presence, written by the app: line 1 is the
// Discord app id, line 2 the activity JSON. It is re-read every 2 s, so edits show while the game runs.
// Debug builds keep a console so the IPC traffic printed by log_frame is visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// The fixed part of the window text (name + mode lines); the countdown is appended on each paint.
static HEAD: OnceLock<String> = OnceLock::new();
static DEADLINE: OnceLock<Option<Instant>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn deadline() -> Option<Instant> {
    DEADLINE.get().copied().flatten()
}

fn label() -> Vec<u16> {
    let head = HEAD.get().unwrap();
    match deadline() {
        Some(at) => {
            let left = at.saturating_duration_since(Instant::now()).as_secs();
            wide(&format!("{head}\ncloses in {}:{:02}", left / 60, left % 60))
        }
        None => wide(head),
    }
}

// Same contract as the old .NET DummyGame: argv[1] is the display name. After it, in any order:
// a positive number (minutes until auto-close) and --rpc=<id> (ignored unless 1..=25 ASCII digits).
fn parse_args(args: Vec<String>) -> (String, Option<f64>, Option<String>) {
    let mut args = args.into_iter();
    let name = args.next().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "Game".into());
    let (mut minutes, mut rpc) = (None, None);
    for a in args {
        if let Some(id) = a.strip_prefix("--rpc=") {
            if is_app_id(id) {
                rpc = Some(id.to_string());
            }
        } else if let Some(m) = a.parse::<f64>().ok().filter(|m| *m > 0.0) {
            minutes = Some(m);
        }
    }
    (name, minutes, rpc)
}

fn is_app_id(id: &str) -> bool {
    (1..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn presence_path() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?.into_os_string();
    path.push(".presence");
    Some(path.into())
}

/// (app id, activity JSON object) from a presence file's text.
fn parse_presence(text: &str) -> Option<(String, String)> {
    let (id, activity) = text.split_once('\n')?;
    let (id, activity) = (id.trim(), activity.trim());
    (is_app_id(id) && activity.starts_with('{') && activity.ends_with('}')).then(|| (id.to_string(), activity.to_string()))
}

fn read_presence(path: Option<&Path>) -> Option<(String, String)> {
    parse_presence(&fs::read_to_string(path?).ok()?)
}

/// The activity sent to Discord: the app's custom fields, if any, plus the launch time unless they set their own timer.
fn activity_json(custom: Option<&str>, start_ms: u128) -> String {
    // User text can't fake this key: quotes inside JSON strings are escaped.
    if let Some(c) = custom.filter(|c| c.contains(r#""timestamps":"#)) {
        return c.to_string();
    }
    let head = format!(r#"{{"timestamps":{{"start":{start_ms}}}"#);
    match custom.and_then(|c| c.strip_prefix('{')).map(str::trim_start) {
        Some(rest) if rest != "}" => format!("{head},{rest}"),
        _ => format!("{head}}}"),
    }
}

// Discord IPC frame: LE u32 opcode, LE u32 payload length, UTF-8 JSON.
fn frame(op: u32, json: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(8 + json.len());
    f.extend_from_slice(&op.to_le_bytes());
    f.extend_from_slice(&(json.len() as u32).to_le_bytes());
    f.extend_from_slice(json.as_bytes());
    f
}

// Reads one frame: the JSON of a normal frame (opcode 1), or None after answering a PING. Fails on CLOSE.
fn read_frame<P: Read + Write>(pipe: &mut P) -> io::Result<Option<String>> {
    let mut h = [0u8; 8];
    pipe.read_exact(&mut h)?;
    let op = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
    if len > 1 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC frame too large"));
    }
    let mut buf = vec![0; len];
    pipe.read_exact(&mut buf)?;
    let json = String::from_utf8_lossy(&buf).into_owned();
    if cfg!(debug_assertions) {
        log_frame(op, &json);
    }
    match op {
        1 => return Ok(Some(json)),
        2 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "Discord closed the IPC connection")),
        3 => pipe.write_all(&frame(4, &json))?,
        _ => {}
    }
    Ok(None)
}

fn next_frame<P: Read + Write>(pipe: &mut P) -> io::Result<String> {
    loop {
        if let Some(json) = read_frame(pipe)? {
            return Ok(json);
        }
    }
}

// Bytes Discord has sent that are not read yet. Fails once Discord has closed the pipe.
fn bytes_waiting(pipe: &File) -> io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut n = 0;
    if unsafe { PeekNamedPipe(pipe.as_raw_handle(), null_mut(), 0, null_mut(), &mut n, null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n)
}

// Debug only. Never print whole payloads: READY carries the user's account (username, id, avatar).
fn log_frame(op: u32, json: &str) {
    // Naive "key":value lookup, good enough for a debug line.
    let field = |key: &str| {
        let pat = format!("\"{key}\":");
        let Some(i) = json.find(&pat) else { return "-" };
        let v = json[i + pat.len()..].trim_start();
        match v.strip_prefix('"') {
            Some(s) => &s[..s.find('"').unwrap_or(s.len())],
            None => &v[..v.find([',', '}']).unwrap_or(v.len())],
        }
    };
    let cmd = field("cmd");
    if cmd == "SET_ACTIVITY" {
        eprintln!("ipc op={op} cmd={cmd} evt={} name={} application_id={}", field("evt"), field("name"), field("application_id"));
    } else if op == 2 {
        eprintln!("ipc op=2 code={} message={}", field("code"), field("message"));
    } else {
        eprintln!("ipc op={op} cmd={cmd} evt={}", field("evt"));
    }
}

// Shows the game in Discord until Discord goes away (Err) or, for exe games, the custom status is removed (Ok).
// `fixed_id` (--rpc) means the game is always shown; otherwise only while a presence file exists.
fn rpc_session(fixed_id: Option<&str>, path: Option<&Path>, start_ms: u128) -> io::Result<()> {
    let mut custom = read_presence(path);
    let Some(id) = fixed_id.map(str::to_string).or_else(|| custom.as_ref().map(|c| c.0.clone())) else {
        return Ok(());
    };
    let mut pipe = (0..10)
        .find_map(|i| OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{i}")).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Discord IPC pipe (Discord not running?)"))?;
    pipe.write_all(&frame(0, &format!(r#"{{"v":1,"client_id":"{id}"}}"#)))?;
    while !next_frame(&mut pipe)?.contains(r#""evt":"READY""#) {}
    let pid = std::process::id();
    let send = |pipe: &mut File, custom: &Option<(String, String)>| {
        let activity = activity_json(custom.as_ref().map(|c| c.1.as_str()), start_ms);
        pipe.write_all(&frame(1, &format!(r#"{{"cmd":"SET_ACTIVITY","nonce":"1","args":{{"pid":{pid},"activity":{activity}}}}}"#)))
    };
    send(&mut pipe, &custom)?;
    // Polled rather than a blocking read: a pending read on this pipe handle would also hold up writes.
    loop {
        std::thread::sleep(Duration::from_secs(2));
        while bytes_waiting(&pipe)? > 0 {
            read_frame(&mut pipe)?;
        }
        let now = read_presence(path);
        if fixed_id.is_none() && now.is_none() {
            return Ok(()); // closing the pipe clears the status, so Discord falls back to detecting the exe
        }
        if now != custom {
            custom = now;
            send(&mut pipe, &custom)?;
        }
    }
}

// Runs on its own thread forever; the process still exits when main returns (ExitProcess kills it).
fn rpc_loop(fixed_id: Option<String>, start_ms: u128) {
    let path = presence_path();
    loop {
        let wait = match rpc_session(fixed_id.as_deref(), path.as_deref(), start_ms) {
            Ok(()) => 2,
            Err(e) => {
                if cfg!(debug_assertions) {
                    eprintln!("rpc: {e}; retrying in 5 s");
                }
                5
            }
        };
        std::thread::sleep(Duration::from_secs(wait));
    }
}

fn main() {
    let (name, minutes, rpc) = parse_args(std::env::args().skip(1).collect());
    let at = minutes
        .and_then(|m| Duration::try_from_secs_f64(m * 60.0).ok())
        .and_then(|d| Instant::now().checked_add(d));
    let mut head = format!("{name} (fake process for Discord)");
    if rpc.is_some() {
        head.push_str("\nshown in Discord through Rich Presence");
    }
    // Fixed once so Discord's elapsed timer survives reconnects.
    let start_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    std::thread::spawn(move || rpc_loop(rpc, start_ms));
    HEAD.get_or_init(|| head);
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
            // Measure the (possibly multi-line) text first so it can be centered vertically.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> (String, Option<f64>, Option<String>) {
        parse_args(a.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parses_args() {
        assert_eq!(args(&[]), ("Game".into(), None, None));
        assert_eq!(args(&["CS2", "30"]), ("CS2".into(), Some(30.0), None));
        assert_eq!(args(&["RoK", "--rpc=1441992604911665204", "0.5"]), ("RoK".into(), Some(0.5), Some("1441992604911665204".into())));
        assert_eq!(args(&["RoK", "0.5", "--rpc=1441992604911665204"]), ("RoK".into(), Some(0.5), Some("1441992604911665204".into())));
        assert_eq!(args(&["X", "--rpc=12a", "--rpc=", "-3"]), ("X".into(), None, None));
        assert_eq!(args(&["X", &format!("--rpc={}", "1".repeat(26))]), ("X".into(), None, None));
    }

    // Fake pipe: reads from `0`, collects writes in `1`.
    struct Pipe(io::Cursor<Vec<u8>>, Vec<u8>);
    impl Read for Pipe {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Write for Pipe {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.1.write(b)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn encodes_and_reads_frames() {
        assert_eq!(frame(1, "{}"), [1, 0, 0, 0, 2, 0, 0, 0, b'{', b'}']);
        // A PING is answered with a PONG carrying the same payload, then the opcode 1 frame is returned.
        let mut pipe = Pipe(io::Cursor::new([frame(3, "p"), frame(1, "{\"a\":1}")].concat()), vec![]);
        assert_eq!(next_frame(&mut pipe).unwrap(), "{\"a\":1}");
        assert_eq!(pipe.1, frame(4, "p"));
        // CLOSE and EOF are errors (both mean reconnect).
        assert!(next_frame(&mut Pipe(io::Cursor::new(frame(2, "{}")), vec![])).is_err());
        assert!(next_frame(&mut pipe).is_err());
        // A lone PING is answered without waiting for another frame.
        assert_eq!(read_frame(&mut Pipe(io::Cursor::new(frame(3, "p")), vec![])).unwrap(), None);
    }

    #[test]
    fn builds_presence() {
        let custom = r#"{"details":"Competitive","party":{"size":[2,5]}}"#;
        assert_eq!(parse_presence(&format!("700136079562375258\n{custom}\n")), Some(("700136079562375258".into(), custom.into())));
        assert_eq!(parse_presence("abc\n{}"), None);
        assert_eq!(parse_presence("123\nnot json"), None);
        assert_eq!(parse_presence("123"), None);
        assert_eq!(activity_json(Some(custom), 5), r#"{"timestamps":{"start":5},"details":"Competitive","party":{"size":[2,5]}}"#);
        assert_eq!(activity_json(Some("{}"), 5), r#"{"timestamps":{"start":5}}"#);
        assert_eq!(activity_json(None, 5), r#"{"timestamps":{"start":5}}"#);
        let countdown = r#"{"state":"In Match","timestamps":{"end":9}}"#;
        assert_eq!(activity_json(Some(countdown), 5), countdown);
        let tricky = r#"{"state":"\"timestamps\":"}"#;
        assert_eq!(activity_json(Some(tricky), 5), r#"{"timestamps":{"start":5},"state":"\"timestamps\":"}"#);
    }
}
