//! Result of a Send to Obsidian run, shown on the UI thread: a small window with an
//! "Open in Obsidian" button on success, a message box on failure.

use super::win::{centered, child, main_hwnd, px, register_class};
use crate::obsidian;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const CLASS: PCWSTR = w!("StepRecorderSent");
const ID_OPEN: usize = 1; // = IDOK, so Enter opens
const ID_CLOSE: usize = 2; // = IDCANCEL, so Esc closes

/// Handed from the background send thread to the UI thread through `PENDING`.
pub struct Outcome {
    pub vault: PathBuf,
    pub dir: PathBuf,
    pub result: std::result::Result<String, String>,
}

/// Outcomes wait here, not in `WM_APP_SENT`'s lparam: any process can post that message number,
/// so the handler must not trust its parameters.
static PENDING: Mutex<Vec<Outcome>> = Mutex::new(Vec::new());

pub(super) fn push(o: Outcome) {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner).push(o);
}

pub(super) fn take() -> Vec<Outcome> {
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(PoisonError::into_inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_drains_pushed_outcomes_once() {
        assert!(take().is_empty()); // a stray WM_APP_SENT finds nothing
        push(Outcome { vault: "v".into(), dir: "d".into(), result: Ok("n.md".into()) });
        assert_eq!(take().len(), 1);
        assert!(take().is_empty());
    }
}

thread_local! {
    /// The open result window and the obsidian:// link its button opens.
    static OPEN: RefCell<Option<(HWND, String)>> = const { RefCell::new(None) };
}

pub(super) fn hwnd() -> HWND {
    OPEN.with_borrow(|o| o.as_ref().map(|(h, _)| *h)).unwrap_or(HWND(std::ptr::null_mut()))
}

fn message(text: &str, flags: MESSAGEBOX_STYLE) {
    unsafe {
        MessageBoxW(Some(main_hwnd()), &HSTRING::from(text), w!("Send to Obsidian"), flags | MB_SETFOREGROUND);
    }
}

pub(super) fn show(o: Outcome) {
    let name = o.vault.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match o.result {
        Err(e) => {
            eprintln!("[obsidian] {e}");
            message(
                &format!("Couldn't send the recording to Obsidian.\n\n{e}\n\nThe recording is still saved in {}.", o.dir.display()),
                MB_ICONERROR,
            );
        }
        Ok(note) if note.is_empty() => message(&format!("Claude finished in {name}, but didn't say which note it wrote."), MB_ICONINFORMATION),
        Ok(note) => {
            println!("[obsidian] wrote {note}");
            let uri = obsidian::note_uri(&o.vault, &note);
            if let Err(e) = create(&format!("The recording was written to {name}:
{note}"), uri) {
                message(&format!("The recording was written to {name}:\n\n{note}\n\n(Couldn't show the Open button: {e})"), MB_ICONINFORMATION);
            }
        }
    }
}

fn close() {
    if let Some((h, _)) = OPEN.with_borrow_mut(Option::take) {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

fn create(text: &str, uri: String) -> Result<()> {
    close(); // one at a time
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        register_class(CLASS, Some(wndproc))?;
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let mut outer = RECT { left: 0, top: 0, right: px(460), bottom: px(118) };
        AdjustWindowRect(&mut outer, style, false)?;
        let (w, h) = (outer.right - outer.left, outer.bottom - outer.top);
        let (x, y) = centered(w, h);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            CLASS,
            w!("Sent to Obsidian"),
            style | WS_VISIBLE,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        let text = HSTRING::from(text);
        child(hwnd, w!("STATIC"), PCWSTR(text.as_ptr()), 0x80 /* SS_NOPREFIX: keep & literal */, (12, 12, 436, 52), 0)?;
        let tab = WS_TABSTOP.0;
        child(hwnd, w!("BUTTON"), w!("Open in Obsidian"), BS_DEFPUSHBUTTON as u32 | tab, (182, 76, 160, 30), ID_OPEN)?;
        child(hwnd, w!("BUTTON"), w!("Close"), BS_PUSHBUTTON as u32 | tab, (348, 76, 100, 30), ID_CLOSE)?;
        OPEN.set(Some((hwnd, uri)));
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(())
}

fn open_in_obsidian() {
    let uri = OPEN.with_borrow(|o| o.as_ref().map(|(_, u)| u.clone()));
    close();
    if let Some(uri) = uri {
        // ShellExecute hands the obsidian:// link to its registered handler (Obsidian).
        // explorer.exe mangles links containing '&' and opens File Explorer instead.
        let r = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(&uri), None, None, SW_SHOWNORMAL) };
        if r.0 as isize <= 32 {
            message(&format!("Couldn't open Obsidian. Is it installed?\n\n{uri}"), MB_ICONWARNING);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if wparam.0 >> 16 == 0 => match wparam.0 & 0xffff {
            ID_OPEN => open_in_obsidian(),
            ID_CLOSE => close(),
            _ => {}
        },
        WM_CLOSE => close(),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
