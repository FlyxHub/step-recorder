//! Windows: control window, hotkeys, low-level mouse hook, GDI capture, window/process lookup.
//!
//! Everything except `Shot::to_rgba` and `process_name` runs on the UI thread, which owns
//! the message loop the low-level hook needs.

use crate::model::{DoubleClick, MouseButton, Stamp};
use super::{review, sent};
use crate::worker::{Msg, QUEUED_CLICKS};
use image::RgbaImage;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const ID_START: usize = 1; // = IDOK, so Enter in the Title box starts recording
const ID_PAUSE: usize = 6; // not 2 (IDCANCEL), or Esc would toggle Pause
const ID_STOP: usize = 3;
const ID_ABORT: usize = 4;
const ID_DELETE_LAST: usize = 5;
/// While the hook is installed: re-arms it and checks for elevated windows, every second.
const TIMER_HOOK: usize = 1;
const HK_RECORD: i32 = 1;
const HK_PAUSE: i32 = 2;
const CLASS: PCWSTR = w!("StepRecorderWindow");
// shortcut: clicks past this many unprocessed ones are skipped (each holds a full-monitor
// bitmap, ~33 MB at 4K); raise it if the status line reports skips in normal use.
pub const MAX_QUEUED_CLICKS: usize = 8;

/// Id of the UI thread, so `notice` knows whether a modal box would block the worker.
static UI_THREAD: AtomicU32 = AtomicU32::new(0);
/// Send to Obsidian runs in progress (counted up by the review window, down by `WM_APP_SENT`).
/// A count, so the first of two sends finishing doesn't drop the close guard for the second.
pub(super) static SENDING: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    Recording,
    Paused,
}

thread_local! {
    static STATE: Cell<State> = const { Cell::new(State::Idle) };
    static MAIN: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static STATUS: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// Delete last step: only shown while a recording is running (or paused).
    static DELETE_LAST: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// "Title:" label and box, shown in the same row while idle.
    static TITLE_LABEL: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static TITLE: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    /// The status line is showing the "admin window" warning.
    static ELEVATED: Cell<bool> = const { Cell::new(false) };
    static TX: RefCell<Option<Sender<Msg>>> = const { RefCell::new(None) };
    /// Status band backgrounds: (recording, paused).
    static BRUSHES: Cell<(HBRUSH, HBRUSH)> =
        const { Cell::new((HBRUSH(std::ptr::null_mut()), HBRUSH(std::ptr::null_mut()))) };
    static FONT: Cell<HFONT> = const { Cell::new(HFONT(std::ptr::null_mut())) };
}

fn set_status(text: &str) {
    unsafe {
        let _ = SetWindowTextW(STATUS.get(), &HSTRING::from(text));
    }
}

/// Posted to the control window when a Send to Obsidian run ends, to clear its status line.
pub(super) const WM_APP_SENT: u32 = WM_APP + 1;

pub(super) fn main_hwnd() -> HWND {
    MAIN.get()
}

/// Shows `text` in the status line while idle (recording states own it otherwise).
pub(super) fn show_activity(text: &str) {
    if STATE.get() == State::Idle {
        set_status(text);
    }
}

/// Top-left for a `w` x `h` window centred on the monitor under the mouse, away from the
/// control window that sits at the default position.
pub(super) fn centered(w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST), &mut mi);
        let work = mi.rcWork;
        (work.left + ((work.right - work.left - w) / 2).max(0), work.top + ((work.bottom - work.top - h) / 2).max(0))
    }
}

/// `%APPDATA%\Step Recorder\settings.json`.
pub(super) fn settings_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Step Recorder")
        .join("settings.json")
}

/// Logical pixels → physical, at the system DPI.
// ponytail: windows are sized for the primary monitor's DPI at startup; no WM_DPICHANGED handling.
pub(super) fn px(v: i32) -> i32 {
    v * unsafe { GetDpiForSystem() } as i32 / 96
}

/// Creates a child control in logical-pixel coordinates, using the app font.
#[allow(clippy::too_many_arguments)]
pub(super) fn child(
    parent: HWND,
    class: PCWSTR,
    text: PCWSTR,
    style: u32,
    (x, y, w, h): (i32, i32, i32, i32),
    id: usize,
) -> Result<HWND> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let c = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            text,
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            px(x),
            px(y),
            px(w),
            px(h),
            Some(parent),
            Some(HMENU(id as *mut _)),
            Some(hinst),
            None,
        )?;
        SendMessageW(c, WM_SETFONT, Some(WPARAM(FONT.get().0 as usize)), Some(LPARAM(1)));
        Ok(c)
    }
}

/// The embedded app icon (resource 1, from build.rs), or none.
pub(super) fn app_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).ok().map(HINSTANCE::from);
        // MAKEINTRESOURCE(1)
        LoadIconW(hinst, PCWSTR(std::ptr::without_provenance(1))).unwrap_or_default()
    }
}

/// Registers a window class with the app icon and the classic grey background. Registering the
/// same class again (a second review window) fails harmlessly with "class already exists".
pub(super) fn register_class(name: PCWSTR, proc: WNDPROC) -> Result<()> {
    unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: proc,
            hInstance: GetModuleHandleW(None)?.into(),
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: app_icon(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        });
    }
    Ok(())
}

const RED: COLORREF = COLORREF(0x0000_00C8); // 0x00BBGGRR
const AMBER: COLORREF = COLORREF(0x0000_AAF0);

/// Logs a problem. Release builds have no console, so it's also shown in a dialog.
pub fn notice(text: &str) {
    eprintln!("[warn] {text}");
    if cfg!(debug_assertions) {
        return;
    }
    let text = HSTRING::from(text);
    let show = move || unsafe {
        MessageBoxW(Some(MAIN.get()), &text, w!("Step Recorder"), MB_ICONWARNING);
    };
    if unsafe { GetCurrentThreadId() } == UI_THREAD.load(Relaxed) {
        show();
    } else {
        // From the worker: a modal box there would stop steps being written until dismissed.
        std::thread::spawn(show);
    }
}

fn send(msg: Msg) {
    TX.with_borrow(|tx| {
        if let Some(tx) = tx {
            let _ = tx.send(msg);
        }
    });
}

/// A monitor screenshot still in GDI memory. Converting it to pixels happens on the worker.
pub struct Shot {
    bmp: HBITMAP,
    w: i32,
    h: i32,
}

// SAFETY: a GDI bitmap not selected into any DC may be used from any thread.
unsafe impl Send for Shot {}

impl Shot {
    /// Copies a screen rectangle (physical pixels) into a new bitmap. Fast: no pixel conversion.
    fn grab(r: RECT) -> Option<Shot> {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            // Null when GDI memory runs out. BitBlt would then "succeed" into the DC's 1x1 bitmap.
            if bmp.is_invalid() {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                return None;
            }
            let old = SelectObject(mem, bmp.into());
            // CAPTUREBLT includes layered windows (menus, tooltips).
            let ok = BitBlt(mem, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY | CAPTUREBLT);
            SelectObject(mem, old);
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            match ok {
                Ok(()) => Some(Shot { bmp, w, h }),
                Err(_) => {
                    let _ = DeleteObject(bmp.into());
                    None
                }
            }
        }
    }

    pub fn to_rgba(&self) -> Option<RgbaImage> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.w,
                biHeight: -self.h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; self.w as usize * self.h as usize * 4];
        let lines = unsafe {
            let dc = GetDC(None);
            let n = GetDIBits(
                dc,
                self.bmp,
                0,
                self.h as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, dc);
            n
        };
        if lines != self.h {
            return None;
        }
        for px in buf.as_chunks_mut::<4>().0 {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255;
        }
        RgbaImage::from_raw(self.w as u32, self.h as u32, buf)
    }
}

impl Drop for Shot {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bmp.into());
        }
    }
}

/// Everything known about a click at the moment it happened.
pub struct Click {
    pub shot: Shot,
    pub button: MouseButton,
    pub x: i32,
    pub y: i32,
    pub monitor_index: u32,
    pub monitor_origin: [i32; 2],
    pub scale: f32,
    pub window: String,
    pub pid: u32,
    pub time: Stamp,
    /// Input-clock timestamp of the press (`MSLLHOOKSTRUCT.time`, ms, wraps).
    pub ms: u32,
    /// The user's double-click time and distance, in this monitor's physical pixels.
    pub double_click: DoubleClick,
}

/// Opens a File Explorer window on `dir`. Doesn't wait for it.
pub fn open_folder(dir: &Path) {
    use std::os::windows::process::CommandExt;
    // Full path: a bare "explorer" would look in the recorder's own folder (often Downloads) first.
    let mut buf = [0u16; 260];
    let n = unsafe { windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW(Some(&mut buf)) };
    // 0 = failed, > len = buffer too small; either way don't fall back to a search.
    let Some(windir) = buf.get(..n as usize).filter(|w| !w.is_empty()) else {
        eprintln!("[warn] could not find the Windows folder to open {}", dir.display());
        return;
    };
    let explorer = Path::new(&String::from_utf16_lossy(windir)).join("explorer.exe");
    // Always quoted: unquoted, Explorer splits its command line on commas (a title may have one).
    if let Err(e) = std::process::Command::new(explorer).raw_arg(format!("\"{}\"", dir.display())).spawn() {
        eprintln!("[warn] could not open {}: {e}", dir.display());
    }
}

/// Executable file name for a process id, or "" if it can't be read.
pub fn process_name(pid: u32) -> String {
    let path = unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    };
    Path::new(&path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn now() -> Stamp {
    let t = unsafe { GetLocalTime() };
    Stamp {
        year: t.wYear,
        month: t.wMonth as u8,
        day: t.wDay as u8,
        hour: t.wHour as u8,
        minute: t.wMinute as u8,
        second: t.wSecond as u8,
    }
}

/// Position of `target` in EnumDisplayMonitors order.
fn monitor_index(target: HMONITOR) -> u32 {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let (target, count, found) = unsafe { &mut *(data.0 as *mut (HMONITOR, u32, u32)) };
        if m == *target {
            *found = *count;
            return false.into();
        }
        *count += 1;
        true.into()
    }
    let mut data = (target, 0u32, 0u32);
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut data as *mut _ as isize));
    }
    data.2
}

/// Runs inside the hook: capture first, then look up cheap metadata, then hand off.
fn on_click(button: MouseButton, pt: POINT, ms: u32) {
    unsafe {
        let root = GetAncestor(WindowFromPoint(pt), GA_ROOT);
        let mut pid = 0u32;
        GetWindowThreadProcessId(root, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return; // our own windows, message boxes and dialogs included
        }
        if QUEUED_CLICKS.load(Relaxed) >= MAX_QUEUED_CLICKS {
            eprintln!("[warn] click skipped: {MAX_QUEUED_CLICKS} still being saved");
            set_status("● RECORDING. Too fast: a click was skipped.");
            return;
        }
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let r = mi.rcMonitor;
        let Some(shot) = Shot::grab(r) else {
            eprintln!("[error] screen capture failed");
            set_status("● RECORDING. Screen capture failed: a click was skipped.");
            return;
        };
        let (mut dpi, mut _dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut _dpi_y);
        // Same limits Windows uses to synthesize WM_LBUTTONDBLCLK (Mouse settings → double-click
        // speed). The rectangle is centred on the first press, so allow half of it each way.
        let double_click = DoubleClick {
            max_ms: GetDoubleClickTime(),
            max_dx: GetSystemMetricsForDpi(SM_CXDOUBLECLK, dpi) / 2,
            max_dy: GetSystemMetricsForDpi(SM_CYDOUBLECLK, dpi) / 2,
        };
        QUEUED_CLICKS.fetch_add(1, Relaxed); // before send, so the worker's decrement can't come first
        send(Msg::Click(Click {
            shot,
            button,
            x: pt.x,
            y: pt.y,
            monitor_index: monitor_index(mon),
            monitor_origin: [r.left, r.top],
            scale: dpi as f32 / 96.0,
            window: control_text(root),
            pid,
            time: now(),
            ms,
            double_click,
        }));
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && STATE.get() == State::Recording {
        let button = match wparam.0 as u32 {
            WM_LBUTTONDOWN => Some(MouseButton::Left),
            WM_RBUTTONDOWN => Some(MouseButton::Right),
            _ => None,
        };
        if let Some(button) = button {
            // pt is in physical pixels because the process is per-monitor DPI aware.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            on_click(button, info.pt, info.time);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn install_hook() -> Result<HHOOK> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok().map(HINSTANCE::from);
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0)
    }
}

/// Mandatory integrity level (RID) of a process, or None if its token can't be read.
fn integrity(process: HANDLE) -> Option<u32> {
    use windows::Win32::Security::*;
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut buf = [0u64; 8]; // TOKEN_MANDATORY_LABEL plus a one-part SID; u64 for alignment
        let mut len = 0;
        let ok = GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), 64, &mut len);
        let _ = CloseHandle(token);
        ok.ok()?;
        let sid = (*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        let last = (*GetSidSubAuthorityCount(sid)).checked_sub(1)?;
        Some(*GetSidSubAuthority(sid, last as u32))
    }
}

/// Whether the foreground window belongs to a process above our integrity level (an admin app,
/// when we aren't elevated). UIPI keeps its clicks from our hook, so they'd silently go missing.
fn foreground_is_elevated() -> bool {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        if pid == 0 {
            return false; // nothing in front (mid-switch)
        }
        // A process or token we can't read is an elevated or system one, as far as we can tell.
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return true };
        let theirs = integrity(h);
        let _ = CloseHandle(h);
        theirs.is_none_or(|t| integrity(GetCurrentProcess()).is_some_and(|ours| t > ours))
    }
}

/// Every second while the hook is installed.
fn hook_tick() {
    // Windows silently drops a hook that once overran LowLevelHooksTimeout (a hung window under
    // the cursor, a slow capture). Re-arming costs nothing and caps the loss at a second.
    // New hook first: no gap, and no hook call can run between the two (it needs our message loop).
    if let Some(old) = HOOK.get()
        && let Ok(new) = install_hook()
    {
        HOOK.set(Some(new));
        let _ = unsafe { UnhookWindowsHookEx(old) };
    }
    if STATE.get() == State::Recording {
        let elevated = foreground_is_elevated();
        if elevated != ELEVATED.get() {
            set_state(State::Recording); // resets the status line and ELEVATED
            if elevated {
                ELEVATED.set(true);
                set_status("● RECORDING. Admin window: its clicks can't be recorded.");
            }
        }
    }
}

fn set_state(new: State) {
    let old = STATE.replace(new);
    if old != new {
        println!("[state] {old:?} -> {new:?}");
    }
    ELEVATED.set(false); // the status line is reset below; the next tick re-checks
    let (title, status) = match new {
        State::Idle => ("Step Recorder", "Idle. Ctrl+Alt+R to record."),
        State::Recording => ("● REC - Step Recorder", "● RECORDING. Ctrl+Alt+P pause, Ctrl+Alt+R stop."),
        State::Paused => ("❚❚ PAUSED - Step Recorder", "Paused. Ctrl+Alt+P resume, Ctrl+Alt+R stop."),
    };
    set_status(status);
    unsafe {
        let _ = SetWindowTextW(MAIN.get(), &HSTRING::from(title));
        // Only the buttons that do something now are enabled.
        let idle = new == State::Idle;
        let button = |id: usize| GetDlgItem(Some(MAIN.get()), id as i32).unwrap_or_default();
        let _ = EnableWindow(button(ID_START), idle);
        for id in [ID_PAUSE, ID_STOP, ID_ABORT] {
            let _ = EnableWindow(button(id), !idle);
        }
        let _ = SetWindowTextW(button(ID_PAUSE), if new == State::Paused { w!("Resume") } else { w!("Pause") });
        // The bottom row is the title box while idle, Delete last step while recording.
        let (title, delete) = if new == State::Idle { (SW_SHOW, SW_HIDE) } else { (SW_HIDE, SW_SHOW) };
        let _ = ShowWindow(TITLE_LABEL.get(), title);
        let _ = ShowWindow(TITLE.get(), title);
        let _ = ShowWindow(DELETE_LAST.get(), delete);
        // On top only while recording, as the indicator. Idle, it mustn't cover the review window.
        let z = if idle { HWND_NOTOPMOST } else { HWND_TOPMOST };
        let _ = SetWindowPos(MAIN.get(), Some(z), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// `%USERPROFILE%\Step Recordings`. Deliberately not Documents: OneDrive "Known Folder Move"
/// often syncs Documents to the cloud, and recordings must stay on this machine.
fn recordings_root() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("Step Recordings")
}

/// Text of a window or control, e.g. the title box. Only sends a message (WM_GETTEXTLENGTH)
/// for our own windows, so it's safe in the hook for other apps' windows.
pub(super) fn control_text(h: HWND) -> String {
    let mut buf = vec![0u16; unsafe { GetWindowTextLengthW(h) } as usize + 1];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Delete last step: removes the newest step of the running session and says so in the status.
fn delete_last() {
    let label = match STATE.get() {
        State::Idle => return,
        State::Recording => "● RECORDING.",
        State::Paused => "Paused.",
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // The worker handles this after any queued clicks, so "last" is the newest step either way.
    // Wait only briefly: the hook is installed, and a blocked UI thread stalls the mouse until
    // Windows silently removes the hook (LowLevelHooksTimeout).
    send(Msg::DeleteLast { reply: tx });
    match rx.recv_timeout(std::time::Duration::from_millis(200)) {
        Ok(Ok(Some(n))) => set_status(&format!("{label} Step {n} deleted.")),
        Ok(Ok(None)) => set_status(&format!("{label} No steps to delete.")),
        Ok(Err(e)) => set_status(&format!("{label} Couldn't delete the last step: {e}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => set_status(&format!("{label} Deleting the last step…")),
        Err(_) => {}
    }
}

fn start() {
    if STATE.get() != State::Idle {
        return;
    }
    // One recording at a time: the open review must be titled and saved (or sent) first.
    if !review::hwnd().is_invalid() {
        set_status("Finish the open review first.");
        review::bring_to_front();
        return;
    }
    let stamp = now();
    let title = control_text(TITLE.get());
    let hook = match install_hook() {
        Ok(h) => h,
        Err(e) => return notice(&format!("Recording could not start: mouse hook failed ({e})")),
    };
    HOOK.set(Some(hook));
    unsafe { SetTimer(Some(MAIN.get()), TIMER_HOOK, 1000, None) };
    send(Msg::Start { root: recordings_root(), started: stamp, title: title.clone() });
    unsafe {
        let _ = SetWindowTextW(TITLE.get(), w!("")); // fresh box for the next recording
    }
    println!("==================== RECORDING ====================");
    println!(" Title: {}", if title.trim().is_empty() { "(none)" } else { title.trim() });
    println!(" Every left/right click is screenshotted.");
    println!(" Ctrl+Alt+P pause/resume   Ctrl+Alt+R stop");
    println!("===================================================");
    set_state(State::Recording);
}

/// Ends recording. With `review`, opens the review window on the finished session; the worker
/// opens the folder itself when there's nothing to review.
fn stop(review: bool) {
    if STATE.get() == State::Idle {
        return;
    }
    unhook();
    set_state(State::Idle);
    if !review {
        return send(Msg::Stop { review: None });
    }
    let (tx, rx) = std::sync::mpsc::channel();
    send(Msg::Stop { review: Some(tx) });
    // ponytail: blocks the UI thread while the worker finishes queued clicks (normally well
    // under a second). Hand back via PostMessage if that ever becomes noticeable.
    if let Ok((dir, session)) = rx.recv() {
        review::open(dir, session);
    }
}

fn unhook() {
    unsafe {
        let _ = KillTimer(Some(MAIN.get()), TIMER_HOOK);
        if let Some(h) = HOOK.take() {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

/// Ends recording and deletes everything captured in this session, after confirming.
fn abort() {
    let before = STATE.get();
    if before == State::Idle {
        return;
    }
    // Pause while asking, so clicks on the dialog aren't recorded.
    set_state(State::Paused);
    let answer = unsafe {
        MessageBoxW(
            Some(MAIN.get()),
            w!("Discard this recording?\n\nAll screenshots and steps captured so far will be deleted."),
            w!("Abort recording"),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };
    if STATE.get() == State::Idle {
        return; // stopped with the hotkey while the dialog was up
    }
    if answer != IDYES {
        return set_state(before);
    }
    unhook();
    send(Msg::Abort);
    set_state(State::Idle);
}

fn toggle_pause() {
    match STATE.get() {
        State::Recording => set_state(State::Paused),
        State::Paused => set_state(State::Recording),
        State::Idle => {}
    }
}

/// Whether a `WM_COMMAND` is a click on our own button: lparam is that button's handle. Any
/// process on the desktop can post `WM_COMMAND` with just an id (say, Send or Delete last step);
/// this turns those away. One that also looks up the button's handle still gets through.
pub(super) fn from_button(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> bool {
    let (id, code) = ((wparam.0 & 0xffff) as i32, wparam.0 >> 16);
    code == BN_CLICKED as usize
        && lparam.0 != 0
        && unsafe { GetDlgItem(Some(hwnd), id) }.is_ok_and(|b| b.0 as isize == lparam.0)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Enter and Esc (via IsDialogMessage) carry the Start / Pause button's handle too.
        WM_COMMAND if from_button(hwnd, wparam, lparam) => match wparam.0 & 0xffff {
            ID_START => start(),
            ID_PAUSE => toggle_pause(),
            ID_STOP => stop(true),
            ID_ABORT => abort(),
            ID_DELETE_LAST => delete_last(),
            _ => {}
        },
        WM_TIMER if wparam.0 == TIMER_HOOK => hook_tick(),
        WM_HOTKEY => match wparam.0 as i32 {
            HK_RECORD if STATE.get() == State::Idle => start(),
            HK_RECORD => stop(true),
            HK_PAUSE => toggle_pause(),
            _ => {}
        },
        // Red band while recording, amber while paused, so the state is obvious at a glance.
        WM_CTLCOLORSTATIC if lparam.0 == STATUS.get().0 as isize && STATE.get() != State::Idle => {
            let hdc = HDC(wparam.0 as *mut _);
            let (rec, paused) = BRUSHES.get();
            let (bg, fg, brush) = match STATE.get() {
                State::Recording => (RED, COLORREF(0x00FF_FFFF), rec),
                _ => (AMBER, COLORREF(0), paused),
            };
            unsafe {
                SetTextColor(hdc, fg);
                SetBkColor(hdc, bg);
            }
            return LRESULT(brush.0 as isize);
        }
        // Closing the app with a review open: it must be titled and saved like any other close.
        WM_CLOSE => {
            // The `claude` process would carry on without us; the result would just never show.
            let still_sending = SENDING.load(Relaxed) > 0
                && unsafe {
                    MessageBoxW(
                        Some(hwnd),
                        w!("Still sending to Obsidian. Close anyway?\n\nClaude keeps writing the note in the background, but the result won't be shown."),
                        w!("Step Recorder"),
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                    )
                } != IDYES;
            if !still_sending && review::finish() {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
        WM_APP_SENT => {
            // The message carries nothing (anyone can post it); a stray one finds an empty queue.
            for outcome in sent::take() {
                SENDING.fetch_sub(1, Relaxed);
                if STATE.get() == State::Idle {
                    set_state(State::Idle); // clear "Sending to Obsidian…"
                }
                sent::show(outcome);
            }
        }
        // Logoff / restart: Windows ends the process after this returns, without WM_DESTROY.
        // Steps are already on disk; keep typed descriptions even without a title.
        WM_ENDSESSION if wparam.0 != 0 => review::save_untitled(),
        WM_DESTROY => {
            stop(false);
            // Normally WM_CLOSE already finished the review; keep descriptions if it didn't.
            review::save_untitled();
            unsafe { PostQuitMessage(0) };
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

pub fn run(tx: Sender<Msg>) -> Result<()> {
    TX.set(Some(tx));
    unsafe {
        UI_THREAD.store(GetCurrentThreadId(), Relaxed);
        // Must come before any window exists. Makes hook points, monitor rects and BitBlt
        // all use physical pixels, so scaled and secondary monitors line up.
        // Fails if awareness was already set, e.g. by the exe's Compatibility-tab DPI override.
        // That's fine: hook points, monitor rects and BitBlt still agree with each other.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // The vault folder picker (IFileOpenDialog) is a COM object.
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        register_class(CLASS, Some(wndproc))?;

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(), // set_state makes it topmost while recording
            CLASS,
            w!("Step Recorder"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            px(380),
            px(166),
            None,
            None,
            Some(hinst),
            None,
        )?;
        MAIN.set(hwnd);
        // Keeps our window out of every screenshot (Windows 10 2004+). DWM applies it to BitBlt too.
        if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
            notice(&format!("The control window will appear in screenshots ({e})"));
        }

        FONT.set(CreateFontW(
            -px(12), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
        ));
        let btn = BS_PUSHBUTTON as u32;
        child(hwnd, w!("BUTTON"), w!("Start"), btn, (10, 10, 80, 30), ID_START)?;
        child(hwnd, w!("BUTTON"), w!("Pause"), btn, (100, 10, 80, 30), ID_PAUSE)?;
        child(hwnd, w!("BUTTON"), w!("Stop"), btn, (190, 10, 80, 30), ID_STOP)?;
        child(hwnd, w!("BUTTON"), w!("Abort"), btn, (280, 10, 80, 30), ID_ABORT)?;
        BRUSHES.set((CreateSolidBrush(RED), CreateSolidBrush(AMBER)));
        // SS_CENTER | SS_CENTERIMAGE | SS_ENDELLIPSIS (live behind the Win32_UI_Controls feature).
        // Single line, so text that doesn't fit (a long vault name) ends in "…" instead of being cut off.
        let centered = 0x1 | 0x200 | 0x4000;
        STATUS.set(child(hwnd, w!("STATIC"), w!(""), centered, (10, 50, 350, 28), 0)?);
        DELETE_LAST.set(child(hwnd, w!("BUTTON"), w!("Delete last step"), btn, (10, 88, 170, 28), ID_DELETE_LAST)?);
        TITLE_LABEL.set(child(hwnd, w!("STATIC"), w!("Title:"), 0x200 /* SS_CENTERIMAGE */, (10, 88, 40, 26), 0)?);
        // Enter in the box arrives as IDOK (= ID_START) via IsDialogMessage: type a title, press Enter.
        let edit_style = WS_BORDER.0 | WS_TABSTOP.0 | 0x80 /* ES_AUTOHSCROLL */;
        TITLE.set(child(hwnd, w!("EDIT"), w!(""), edit_style, (52, 88, 308, 26), 0)?);

        for (id, key, name) in [(HK_RECORD, 'R', "Ctrl+Alt+R"), (HK_PAUSE, 'P', "Ctrl+Alt+P")] {
            if let Err(e) = RegisterHotKey(Some(hwnd), id, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, key as u32) {
                notice(&format!("Could not register {name} ({e}). Use the buttons instead."));
            }
        }
        set_state(State::Idle);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Tab between controls and Enter → Next in the review window.
            // (and Enter → the default button in the Sent to Obsidian window).
            if [MAIN.get(), review::hwnd(), sent::hwnd()]
                .into_iter()
                .any(|w| !w.is_invalid() && IsDialogMessageW(w, &msg).as_bool())
            {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DeleteObject(FONT.get().into());
        let (rec, paused) = BRUSHES.get();
        let _ = DeleteObject(rec.into());
        let _ = DeleteObject(paused.into());
    }
    TX.take(); // drop the sender so the worker drains and exits
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_integrity_level() {
        // Medium (0x2000) unelevated, high (0x3000) when run as administrator.
        let own = integrity(unsafe { GetCurrentProcess() });
        assert!(matches!(own, Some(0x2000 | 0x3000)), "{own:?}");
    }
}
