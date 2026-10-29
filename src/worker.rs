//! Worker thread: turns captured clicks into PNGs, steps.md and steps.json.

use crate::model::{Raw, Session, Stamp, image_name};
use crate::platform::{self, Click};
use crate::render;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};

/// Clicks sent but not yet processed. Each one holds a full-monitor bitmap, so the hook stops
/// queueing past `platform::MAX_QUEUED_CLICKS`.
pub static QUEUED_CLICKS: AtomicUsize = AtomicUsize::new(0);

pub enum Msg {
    /// Starts a session in `root`, in a folder named from the timestamp and title.
    Start { root: PathBuf, started: Stamp, title: String },
    Click(Click),
    /// Ends the session. With `review`, the finished session is handed back for the review
    /// window. Without it (app closing), the folder is opened straight away.
    Stop { review: Option<Sender<(PathBuf, Session)>> },
    /// Ends the session and deletes its folder: screenshots, steps.md and steps.json.
    Abort,
    /// Deletes the newest step (after any queued clicks are saved). Replies with its number,
    /// None if there was nothing to delete, or why it failed.
    DeleteLast { reply: Sender<Result<Option<u32>, String>> },
}

/// Removes step `index` from the session and its folder: deletes the screenshot, renames the
/// later screenshots to their new numbers, and rewrites steps.md / steps.json.
/// All or nothing: if a file is locked or missing, the files are put back and `session` is
/// unchanged, so its numbering still matches the disk and the next click can't overwrite a
/// screenshot.
pub fn remove_step(dir: &Path, session: &mut Session, index: usize) -> Result<(), Box<dyn Error>> {
    let mut s = session.clone();
    let (gone, renames) = s.remove(index).ok_or("no such step")?;
    // Set aside rather than deleted, so a failed rename below can be undone completely.
    let aside = dir.join(format!("{gone}.removed"));
    fs::rename(dir.join(&gone), &aside)?;
    for (i, (old, new)) in renames.iter().enumerate() {
        if let Err(e) = fs::rename(dir.join(old), dir.join(new)) {
            for (old, new) in renames[..i].iter().rev() {
                let _ = fs::rename(dir.join(new), dir.join(old));
            }
            let _ = fs::rename(&aside, dir.join(&gone));
            return Err(e.into());
        }
    }
    let _ = fs::remove_file(&aside); // if locked, a stray .removed file is harmless
    *session = s;
    write_outputs(dir, session)
}

/// Writes steps.md and steps.json for the session.
pub fn write_outputs(dir: &Path, session: &Session) -> Result<(), Box<dyn Error>> {
    write_whole(&dir.join("steps.md"), render::markdown(session).as_bytes())?;
    write_whole(&dir.join("steps.json"), serde_json::to_string_pretty(session)?.as_bytes())?;
    Ok(())
}

/// Writes a temp file, flushes it, then renames it over `path`, so a crash or power loss
/// leaves either the old file or the new one, never a truncated one.
fn write_whole(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    if fs::rename(&tmp, path).is_err() {
        // A viewer holding the file open without delete sharing blocks the rename, but not this.
        let _ = fs::remove_file(&tmp);
        return fs::write(path, data);
    }
    Ok(())
}

/// Creates a new session folder. Never reuses one: if the clock repeats a second (DST
/// fall-back, a clock correction), the name gets " (2)", " (3)" … so Abort can't delete an
/// older recording.
fn new_session_dir(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    for n in 1.. {
        let dir = if n == 1 { root.join(name) } else { root.join(format!("{name} ({n})")) };
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

pub fn run(rx: Receiver<Msg>) {
    let mut active: Option<(PathBuf, Session)> = None;
    for msg in rx {
        match msg {
            Msg::Start { root, started, title } => {
                let mut session = Session::new(started);
                session.title = title.trim().to_owned();
                let name = session.dir_name();
                match new_session_dir(&root, &name).and_then(|dir| fs::create_dir(dir.join("images")).map(|()| dir)) {
                    Ok(dir) => {
                        println!("[session] {}", dir.display());
                        active = Some((dir, session));
                    }
                    Err(e) => platform::notice(&format!("Nothing will be saved: can't create {}: {e}", root.join(name).display())),
                }
            }
            Msg::Click(click) => {
                QUEUED_CLICKS.fetch_sub(1, Relaxed);
                if let Some((dir, session)) = &mut active {
                    match record(dir, session, click) {
                        Ok(msg) => println!("[step] {msg}"),
                        Err(e) => eprintln!("[error] step not saved: {e}"),
                    }
                }
            }
            Msg::DeleteLast { reply } => {
                let mut deleted = Ok(None);
                if let Some((dir, session)) = &mut active
                    && let Some(n) = session.steps.last().map(|s| s.number)
                {
                    deleted = match remove_step(dir, session, n as usize - 1) {
                        Ok(()) => {
                            println!("[step] {n} deleted");
                            Ok(Some(n))
                        }
                        Err(e) => {
                            eprintln!("[error] could not delete step {n}: {e}");
                            Err(e.to_string())
                        }
                    };
                }
                let _ = reply.send(deleted);
            }
            // Clicks queued before the abort were already written above; this removes them too.
            Msg::Abort => {
                if let Some((dir, _)) = active.take() {
                    match fs::remove_dir_all(&dir) {
                        Ok(()) => println!("[session] aborted, deleted {}", dir.display()),
                        Err(e) => platform::notice(&format!(
                            "Recording aborted, but {} could not be fully deleted: {e}",
                            dir.display()
                        )),
                    }
                }
            }
            Msg::Stop { review } => {
                if let Some((dir, session)) = active.take() {
                    let dir = std::path::absolute(&dir).unwrap_or(dir);
                    if session.steps.is_empty() {
                        // Nothing captured (or every step deleted): don't leave an empty folder.
                        match fs::remove_dir_all(&dir) {
                            Ok(()) => println!("[session] no steps, removed {}", dir.display()),
                            Err(e) => eprintln!("[warn] could not remove empty {}: {e}", dir.display()),
                        }
                        continue;
                    }
                    println!("[session] {} steps saved to {}", session.steps.len(), dir.display());
                    match review {
                        Some(tx) => {
                            let _ = tx.send((dir, session));
                        }
                        None => platform::open_folder(&dir),
                    }
                }
            }
        }
    }
}

fn record(dir: &Path, session: &mut Session, c: Click) -> Result<String, Box<dyn Error>> {
    let raw = Raw {
        button: c.button,
        x: c.x,
        y: c.y,
        monitor_index: c.monitor_index,
        monitor_origin: c.monitor_origin,
        scale: c.scale,
        double: false,
    };
    // Second press of a double-click: relabel the previous step and keep its (pre-click)
    // screenshot. This press's screenshot is dropped.
    let msg = if let Some(n) = session.try_double(&raw, c.ms, c.double_click) {
        format!("{n} is now a double-click")
    } else {
        let mut img = c.shot.to_rgba().ok_or("screen capture failed")?;
        let (ox, oy) = (c.monitor_origin[0], c.monitor_origin[1]);
        render::draw_cursor(&mut img, c.x - ox, c.y - oy, c.scale);
        // Save the image before adding the step so the docs never point at a missing file.
        img.save(dir.join(image_name(session.next_number())))?;
        let app = platform::process_name(c.pid);
        let n = session.add_click(raw, c.window, app, c.time.clock(), c.ms).number;
        format!("{n} saved")
    };

    // Rewritten after every step so a crash loses nothing already captured.
    write_outputs(dir, session)?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MouseButton;
    use crate::model::tests::{raw, stamp};
    use std::os::windows::fs::OpenOptionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("step-recorder-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("images")).unwrap();
        dir
    }

    /// A session with `n` steps, each screenshot file holding its own step number.
    fn session_on_disk(dir: &Path, n: u32) -> Session {
        let mut s = Session::new(stamp());
        for i in 1..=n {
            fs::write(dir.join(image_name(i)), i.to_string()).unwrap();
            s.add_click(raw(MouseButton::Left, 0, 0), "W".into(), "a.exe".into(), "t".into(), i * 10_000);
        }
        s
    }

    fn contents(dir: &Path, n: u32) -> String {
        fs::read_to_string(dir.join(image_name(n))).unwrap_or_default()
    }

    #[test]
    fn remove_step_renumbers_files() {
        let dir = scratch("remove-ok");
        let mut s = session_on_disk(&dir, 3);
        remove_step(&dir, &mut s, 0).unwrap();
        assert_eq!(s.steps.len(), 2);
        assert_eq!((contents(&dir, 1), contents(&dir, 2)), ("2".into(), "3".into()));
        assert!(!dir.join(image_name(3)).exists());
        assert_eq!(fs::read_dir(dir.join("images")).unwrap().count(), 2); // no .removed left over
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_remove_changes_nothing() {
        let dir = scratch("remove-locked");
        let mut s = session_on_disk(&dir, 3);
        let before = s.clone();
        // An AV scanner or viewer holding the newest PNG open without sharing.
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(dir.join(image_name(3))).unwrap();
        assert!(remove_step(&dir, &mut s, 0).is_err());
        drop(lock);
        assert_eq!(s, before);
        assert_eq!((contents(&dir, 1), contents(&dir, 2), contents(&dir, 3)), ("1".into(), "2".into(), "3".into()));
        assert_eq!(s.next_number(), 4); // the next click can't overwrite step 3
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn session_folders_are_never_reused() {
        let root = scratch("dirs");
        let a = new_session_dir(&root, "2026-10-19_14-32-05").unwrap();
        let b = new_session_dir(&root, "2026-10-19_14-32-05").unwrap();
        let c = new_session_dir(&root, "2026-10-19_14-32-05").unwrap();
        assert_eq!(a, root.join("2026-10-19_14-32-05"));
        assert_eq!(b, root.join("2026-10-19_14-32-05 (2)"));
        assert_eq!(c, root.join("2026-10-19_14-32-05 (3)"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_whole_replaces_and_leaves_no_temp() {
        let dir = scratch("write");
        let path = dir.join("steps.md");
        write_whole(&path, b"old, longer text").unwrap();
        write_whole(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert!(!dir.join("steps.md.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
