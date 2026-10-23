//! Review window, opened when recording stops: the recording's title (required), then one step
//! at a time with its screenshot and a text box where the user describes what they did.
//! - Save (or closing the window) writes the title and descriptions into steps.md /
//!   steps.json, renames the session folder to include the title, and opens the folder.
//! - Send to Obsidian saves the same way, then hands the recording to Claude Code in the
//!   chosen vault on a background thread. The vault list is managed here too (Add / Remove).
//! - Neither works without a title: the window stays open and shows an error under the field.
//!
//! Runs on the UI thread. `REVIEW` is borrowed only from button clicks, paint and close, never
//! from notifications the controls send back while we're updating them.

use super::sent::{self, Outcome};
use super::win::{
    SENDING, WM_APP_SENT, centered, child, control_text, main_hwnd, notice, open_folder, px, register_class,
    settings_path, show_activity,
};
use crate::model::Session;
use crate::obsidian;
use crate::render::{fit, or_unknown};
use crate::settings::Settings;
use crate::worker::{remove_step, write_outputs};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const CLASS: PCWSTR = w!("StepRecorderReview");
const ID_BACK: usize = 10;
const ID_NEXT: usize = 11;
const ID_SAVE: usize = 12;
const ID_EDIT: usize = 13;
const ID_VAULT: usize = 14;
const ID_ADD: usize = 15;
const ID_REMOVE: usize = 16;
const ID_SEND: usize = 17;
const ID_REMOVE_STEP: usize = 18;
const ID_TITLE: usize = 19;
/// Screenshot preview box, logical pixels.
const PREVIEW: (i32, i32, i32, i32) = (12, 102, 736, 360);
const EM_SETSEL: u32 = 0x00B1;
const EN_CHANGE: usize = 0x0300;
const ES_AUTOHSCROLL: u32 = 0x0080;
const CBS_DROPDOWNLIST: u32 = 0x0003;
const CB_ADDSTRING: u32 = 0x0143;
const CB_GETCURSEL: u32 = 0x0147;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_SETCURSEL: u32 = 0x014E;
const ERROR_RED: COLORREF = COLORREF(0x0000_00C8); // 0x00BBGGRR

struct Review {
    hwnd: HWND,
    dir: PathBuf,
    session: Session,
    index: usize,
    title: HWND,
    header: HWND,
    action: HWND,
    place: HWND,
    edit: HWND,
    back: HWND,
    next: HWND,
    remove_step: HWND,
    vault: HWND,
    /// The vault row (label, dropdown, Add, Remove, Send): hidden until the last step is reached.
    send_row: Vec<HWND>,
    /// Shown in the vault row's place until then.
    send_hint: HWND,
    reached_end: bool,
    settings: Settings,
    /// Current screenshot as top-down BGRA, ready for StretchDIBits.
    image: Option<(Vec<u8>, u32, u32)>,
}

thread_local! {
    static REVIEW: RefCell<Option<Review>> = const { RefCell::new(None) };
    /// Kept separately so the mouse hook and message loop can check it without borrowing.
    static HWND_OPEN: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// "Give the recording a title…" under the title box. A Cell so typing can hide it, and
    /// WM_CTLCOLORSTATIC can colour it red, without borrowing REVIEW.
    static TITLE_ERROR: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

/// The open review window, or a null HWND.
pub(super) fn hwnd() -> HWND {
    HWND_OPEN.get()
}

pub(super) fn bring_to_front() {
    unsafe {
        let _ = SetForegroundWindow(hwnd());
    }
}

pub(super) fn open(dir: PathBuf, session: Session) {
    save_untitled(); // can't normally happen: recording is blocked while a review is open
    if let Err(e) = create(dir.clone(), session) {
        notice(&format!("Could not open the review window ({e}). Steps are saved without descriptions."));
        open_folder(&dir);
    }
}

/// Save / close: needs a title. Saves, closes, opens the session folder, and returns true.
/// Without a title it shows the error, keeps the window open and returns false.
/// Also true when no review is open.
pub(super) fn finish() -> bool {
    if hwnd().is_invalid() {
        return true;
    }
    match close(true) {
        Some(r) => {
            open_folder(&r.dir);
            true
        }
        None => false,
    }
}

/// Last resort when the app is going away: keep descriptions even without a title.
pub(super) fn save_untitled() {
    close(false);
}

/// Stores the title box into the session. False if it's empty.
fn keep_title(r: &mut Review) -> bool {
    r.session.title = control_text(r.title).trim().to_owned();
    !r.session.title.is_empty()
}

/// Saves the title and descriptions, renames the folder to match, and closes the window,
/// returning its state. With `require_title`, refuses (None) and flags the field if untitled.
fn close(require_title: bool) -> Option<Review> {
    let titled = REVIEW.with_borrow_mut(|r| {
        let r = r.as_mut()?;
        keep_text(r);
        Some(keep_title(r))
    })?;
    if require_title && !titled {
        flag_missing_title();
        return None;
    }
    let mut r = REVIEW.with_borrow_mut(Option::take)?;
    HWND_OPEN.set(HWND(std::ptr::null_mut()));
    TITLE_ERROR.set(HWND(std::ptr::null_mut()));
    unsafe {
        let _ = DestroyWindow(r.hwnd);
    }
    rename_for_title(&mut r);
    match write_outputs(&r.dir, &r.session) {
        Ok(()) => println!("[review] saved {}", r.dir.display()),
        Err(e) => notice(&format!("Could not save descriptions: {e}")),
    }
    Some(r)
}

fn flag_missing_title() {
    let title = REVIEW.with_borrow(|r| r.as_ref().map(|r| r.title));
    unsafe {
        let _ = ShowWindow(TITLE_ERROR.get(), SW_SHOW);
        if let Some(t) = title {
            let _ = SetFocus(Some(t));
        }
    }
}

/// Renames the session folder to `<timestamp> <title>` if the title changed it.
fn rename_for_title(r: &mut Review) {
    let Some(parent) = r.dir.parent() else { return };
    let target = parent.join(r.session.dir_name());
    if target == r.dir {
        return;
    }
    match std::fs::rename(&r.dir, &target) {
        Ok(()) => r.dir = target,
        Err(e) => notice(&format!("Saved, but the folder couldn't be renamed to include the title: {e}")),
    }
}

fn message(owner: Option<HWND>, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(owner, &HSTRING::from(text), w!("Step Recorder"), flags) }
}

fn vault_name(vault: &Path) -> String {
    vault.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| vault.display().to_string())
}

/// Refills the vault dropdown from settings and selects `select` (or nothing).
fn fill_vaults(r: &Review, select: Option<usize>) {
    unsafe {
        SendMessageW(r.vault, CB_RESETCONTENT, None, None);
        for v in &r.settings.vaults {
            let name = HSTRING::from(vault_name(v));
            SendMessageW(r.vault, CB_ADDSTRING, None, Some(LPARAM(name.as_ptr() as isize)));
        }
        let sel = select.map_or(-1, |i| i as isize);
        SendMessageW(r.vault, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
}

fn selected_vault(r: &Review) -> Option<usize> {
    let i = unsafe { SendMessageW(r.vault, CB_GETCURSEL, None, None) }.0;
    (i >= 0 && (i as usize) < r.settings.vaults.len()).then_some(i as usize)
}

fn save_settings(s: &Settings) {
    if let Err(e) = s.save(&settings_path()) {
        notice(&format!("Could not save settings: {e}"));
    }
}

/// Folder picker. None if cancelled or unavailable.
fn pick_folder(owner: HWND) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dialog.SetTitle(w!("Choose an Obsidian vault folder")).ok()?;
        dialog.Show(Some(owner)).ok()?; // Err when cancelled
        let path = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

/// Add… : pick a folder, check it's a vault, add it to settings and select it.
fn add_vault() {
    // Pick outside any borrow: the dialog runs a modal loop that repaints this window.
    let Some(folder) = pick_folder(hwnd()) else { return };
    if !folder.join(".obsidian").is_dir() {
        message(
            Some(hwnd()),
            &format!("{} isn't an Obsidian vault (it has no .obsidian folder).\n\nOpen it in Obsidian once, then add it here.", folder.display()),
            MB_ICONWARNING,
        );
        return;
    }
    let has_rules = folder.join("CLAUDE.md").is_file();
    REVIEW.with_borrow_mut(|r| {
        let Some(r) = r else { return };
        let i = r.settings.add_vault(folder);
        save_settings(&r.settings);
        fill_vaults(r, Some(i));
    });
    if !has_rules {
        message(
            Some(hwnd()),
            "Added. This vault has no CLAUDE.md, so Claude will choose the note's format and location itself.",
            MB_ICONINFORMATION,
        );
    }
}

fn remove_vault() {
    REVIEW.with_borrow_mut(|r| {
        let Some(r) = r else { return };
        let Some(i) = selected_vault(r) else { return };
        r.settings.remove_vault(i);
        save_settings(&r.settings);
        fill_vaults(r, r.settings.preferred());
    });
}

/// Send to Obsidian: save, close, and run Claude Code in the chosen vault in the background.
fn send_to_obsidian() {
    let vault = REVIEW.with_borrow_mut(|r| {
        let r = r.as_mut()?;
        let vault = r.settings.vaults.get(selected_vault(r)?)?.clone();
        r.settings.last_vault = Some(vault.clone());
        save_settings(&r.settings);
        Some(vault)
    });
    let Some(vault) = vault else {
        message(Some(hwnd()), "Add an Obsidian vault with Add…, then pick it from the list.", MB_ICONINFORMATION);
        return;
    };
    let Some(r) = close(true) else { return }; // untitled: error shown, window stays
    let (dir, session) = (r.dir, r.session);
    let name = vault_name(&vault);
    show_activity(&format!("Sending to {name}… a window opens when done."));
    println!("[obsidian] sending {} to {}", dir.display(), vault.display());
    let main = main_hwnd().0 as isize; // HWND isn't Send; the raw handle is fine to post to
    SENDING.store(true, Relaxed); // closing the app now asks first
    std::thread::spawn(move || {
        let result = obsidian::send(&dir, &session, &vault);
        // The UI thread drains the queue (sent::take) and shows the result. If the post fails the
        // app is closing; nobody to tell.
        sent::push(Outcome { vault, dir, result });
        let _ = unsafe { PostMessageW(Some(HWND(main as *mut _)), WM_APP_SENT, WPARAM(0), LPARAM(0)) };
    });
}

fn create(dir: PathBuf, session: Session) -> Result<()> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        register_class(CLASS, Some(wndproc))?;
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut outer = RECT { left: 0, top: 0, right: px(760), bottom: px(652) };
        AdjustWindowRect(&mut outer, style, false)?;
        let (w, h) = (outer.right - outer.left, outer.bottom - outer.top);
        let (x, y) = centered(w, h);
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS,
            &HSTRING::from(format!("Review steps - {name}")),
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
        // Keep it out of screenshots if a new recording starts while it's open.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

        let label = |text: PCWSTR, y: i32| child(hwnd, w!("STATIC"), text, 0, (12, y, 736, 20), 0);
        let tab = WS_TABSTOP.0;
        let text_box = WS_BORDER.0 | tab | ES_AUTOHSCROLL;
        // Created in tab order: title, description, Back, Next, Remove step, vault row, Save.
        label(w!("Recording title (required):"), 10)?;
        let title = child(hwnd, w!("EDIT"), w!(""), text_box, (12, 30, 736, 26), ID_TITLE)?;
        let title_error = label(w!("Give the recording a title before saving or sending."), 58)?;
        let _ = ShowWindow(title_error, SW_HIDE);
        let header = label(w!(""), 80)?;
        let action = label(w!(""), 468)?;
        let place = label(w!(""), 490)?;
        label(w!("Describe what you did in this step:"), 516)?;
        let edit = child(hwnd, w!("EDIT"), w!(""), text_box, (12, 536, 736, 26), ID_EDIT)?;
        let push = BS_PUSHBUTTON as u32 | tab;
        let back = child(hwnd, w!("BUTTON"), w!("< Back"), push, (12, 612, 90, 30), ID_BACK)?;
        let next = child(hwnd, w!("BUTTON"), w!("Next >"), BS_DEFPUSHBUTTON as u32 | tab, (110, 612, 90, 30), ID_NEXT)?;
        let remove_step = child(hwnd, w!("BUTTON"), w!("Remove step"), push, (220, 612, 110, 30), ID_REMOVE_STEP)?;
        let send_hint = child(
            hwnd,
            w!("STATIC"),
            w!("Send to Obsidian appears once you've reached the last step."),
            0,
            (12, 578, 736, 20),
            0,
        )?;
        let vault_label = child(hwnd, w!("STATIC"), w!("Obsidian vault:"), 0, (12, 578, 96, 20), 0)?;
        // For a drop-down list the height is the open list's height.
        let vault = child(hwnd, w!("COMBOBOX"), w!(""), CBS_DROPDOWNLIST | WS_VSCROLL.0 | tab, (110, 574, 300, 200), ID_VAULT)?;
        let add = child(hwnd, w!("BUTTON"), w!("Add…"), push, (416, 573, 74, 28), ID_ADD)?;
        let remove = child(hwnd, w!("BUTTON"), w!("Remove"), push, (496, 573, 74, 28), ID_REMOVE)?;
        let send = child(hwnd, w!("BUTTON"), w!("Send to Obsidian"), push, (588, 573, 160, 28), ID_SEND)?;
        let send_row = vec![vault_label, vault, add, remove, send];
        child(hwnd, w!("BUTTON"), w!("Save"), push, (658, 612, 90, 30), ID_SAVE)?;

        let has_title = !session.title.is_empty();
        let _ = SetWindowTextW(title, &HSTRING::from(&session.title));
        TITLE_ERROR.set(title_error);
        let settings = Settings::load(&settings_path());
        REVIEW.set(Some(Review {
            hwnd,
            dir,
            session,
            index: 0,
            title,
            header,
            action,
            place,
            edit,
            back,
            next,
            remove_step,
            vault,
            send_row,
            send_hint,
            reached_end: false,
            settings,
            image: None,
        }));
        HWND_OPEN.set(hwnd);
        REVIEW.with_borrow_mut(|r| {
            if let Some(r) = r {
                fill_vaults(r, r.settings.preferred());
                show(r, 0);
            }
        });
        if !has_title {
            let _ = SetFocus(Some(title)); // start where it's required
        }
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(())
}

/// Stores the text box contents into the current step.
fn keep_text(r: &mut Review) {
    r.session.steps[r.index].description = control_text(r.edit);
}

/// Shows step `index`: labels, text box, buttons and screenshot.
fn show(r: &mut Review, index: usize) {
    r.index = index;
    let total = r.session.steps.len();
    let step = &r.session.steps[index];
    r.image = load_bgra(&r.dir.join(&step.image));
    unsafe {
        let _ = SetWindowTextW(r.header, &HSTRING::from(format!("Step {} of {total}", index + 1)));
        let _ = SetWindowTextW(r.action, &HSTRING::from(&step.action));
        let place = format!("Window: {}    App: {}", or_unknown(&step.window), or_unknown(&step.app));
        let _ = SetWindowTextW(r.place, &HSTRING::from(place));
        let _ = SetWindowTextW(r.edit, &HSTRING::from(&step.description));
        let _ = EnableWindow(r.back, index > 0);
        let _ = EnableWindow(r.next, index + 1 < total);
        let _ = EnableWindow(r.remove_step, total > 1); // keep at least one step; Abort discards all
        // "Done with descriptions" = reached the last step once; the row stays after that.
        r.reached_end |= index + 1 == total;
        let (row, hint) = if r.reached_end { (SW_SHOW, SW_HIDE) } else { (SW_HIDE, SW_SHOW) };
        for &w in &r.send_row {
            let _ = ShowWindow(w, row);
        }
        let _ = ShowWindow(r.send_hint, hint);
        let _ = SetFocus(Some(r.edit));
        SendMessageW(r.edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1))); // select all
        let _ = InvalidateRect(Some(r.hwnd), Some(&preview_rect()), false);
    }
}

fn preview_rect() -> RECT {
    let (x, y, w, h) = PREVIEW;
    RECT { left: px(x), top: px(y), right: px(x + w), bottom: px(y + h) }
}

fn load_bgra(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut buf = img.into_raw();
    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2); // RGBA -> BGRA
    }
    Some((buf, w, h))
}

fn paint(hdc: HDC, r: &Review) {
    let area = preview_rect();
    unsafe {
        FillRect(hdc, &area, HBRUSH(GetStockObject(DKGRAY_BRUSH).0));
        let Some((buf, iw, ih)) = &r.image else {
            return;
        };
        let (x, y, w, h) = fit(*iw, *ih, area.right - area.left, area.bottom - area.top);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: *iw as i32,
                biHeight: -(*ih as i32), // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        SetStretchBltMode(hdc, HALFTONE); // smooth downscale
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
        StretchDIBits(
            hdc,
            area.left + x,
            area.top + y,
            w,
            h,
            0,
            0,
            *iw as i32,
            *ih as i32,
            Some(buf.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

/// Remove step: after confirming, deletes the current step and its screenshot, renumbers the
/// rest, and shows the step that took its place (or the new last step).
fn remove_current_step() {
    let Some((n, total)) = REVIEW.with_borrow(|r| r.as_ref().map(|r| (r.index + 1, r.session.steps.len()))) else {
        return;
    };
    if total < 2 {
        return;
    }
    // Ask outside the borrow: the message box's modal loop repaints this window.
    let ask = format!("Remove step {n} and its screenshot?\n\nLater steps move up one number. This can't be undone.");
    if message(Some(hwnd()), &ask, MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) != IDYES {
        return;
    }
    let failed = REVIEW.with_borrow_mut(|r| {
        let r = r.as_mut()?;
        keep_text(r);
        let index = r.index;
        let result = remove_step(&r.dir, &mut r.session, index);
        show(r, index.min(r.session.steps.len() - 1));
        result.err().map(|e| e.to_string())
    });
    match failed {
        None => println!("[review] step {n} removed"),
        Some(e) => notice(&format!("Step {n} was removed, but its files weren't fully updated: {e}")),
    }
}

/// Moves by `delta` steps, keeping what was typed.
fn go(delta: isize) {
    REVIEW.with_borrow_mut(|r| {
        let Some(r) = r else { return };
        let target = r.index as isize + delta;
        if target < 0 || target as usize >= r.session.steps.len() {
            return;
        }
        keep_text(r);
        show(r, target as usize);
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let (id, code) = (wparam.0 & 0xffff, wparam.0 >> 16);
    match msg {
        // Typing a title clears the "needs a title" error. No borrow: this also fires while
        // `create` fills the box.
        WM_COMMAND if id == ID_TITLE && code == EN_CHANGE => unsafe {
            let _ = ShowWindow(TITLE_ERROR.get(), SW_HIDE);
        },
        // Only button clicks (notification code 0). Edit-box notifications arrive while `show`
        // is updating controls with REVIEW borrowed, so they must not touch it.
        WM_COMMAND if code == 0 => match id {
            ID_BACK => go(-1),
            // IDOK is Enter in a text box (via IsDialogMessage in the main loop).
            ID_NEXT | 1 => go(1),
            ID_SAVE => {
                finish();
            }
            ID_ADD => add_vault(),
            ID_REMOVE => remove_vault(),
            ID_SEND => send_to_obsidian(),
            ID_REMOVE_STEP => remove_current_step(),
            _ => {}
        },
        // The missing-title message in red.
        WM_CTLCOLORSTATIC if lparam.0 == TITLE_ERROR.get().0 as isize => unsafe {
            let hdc = HDC(wparam.0 as *mut _);
            SetTextColor(hdc, ERROR_RED);
            SetBkColor(hdc, COLORREF(GetSysColor(COLOR_BTNFACE)));
            return LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize);
        },
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                REVIEW.with(|cell| {
                    if let Ok(r) = cell.try_borrow()
                        && let Some(r) = r.as_ref()
                    {
                        paint(hdc, r);
                    }
                });
                let _ = EndPaint(hwnd, &ps);
            }
        }
        // Closing saves too, so typed text is never lost; like Save it needs a title.
        WM_CLOSE => {
            finish();
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
