//! Worker thread: turns captured clicks into PNGs, steps.md and steps.json.

use crate::model::{Raw, Session, Stamp, image_name};
use crate::platform::{self, Click};
use crate::render;
use std::error::Error;
use std::fs;
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
    /// or None if there was nothing to delete.
    DeleteLast { reply: Sender<Option<u32>> },
}

/// Removes step `index` from the session and its folder: deletes the screenshot, renames the
/// later screenshots to their new numbers, and rewrites steps.md / steps.json.
pub fn remove_step(dir: &Path, session: &mut Session, index: usize) -> Result<(), Box<dyn Error>> {
    let (gone, renames) = session.remove(index).ok_or("no such step")?;
    fs::remove_file(dir.join(gone))?;
    for (old, new) in renames {
        fs::rename(dir.join(old), dir.join(new))?;
    }
    write_outputs(dir, session)
}

/// Writes steps.md and steps.json for the session.
pub fn write_outputs(dir: &Path, session: &Session) -> Result<(), Box<dyn Error>> {
    fs::write(dir.join("steps.md"), render::markdown(session))?;
    fs::write(dir.join("steps.json"), serde_json::to_string_pretty(session)?)?;
    Ok(())
}

pub fn run(rx: Receiver<Msg>) {
    let mut active: Option<(PathBuf, Session)> = None;
    for msg in rx {
        match msg {
            Msg::Start { root, started, title } => {
                let mut session = Session::new(started);
                session.title = title.trim().to_owned();
                let dir = root.join(session.dir_name());
                match fs::create_dir_all(dir.join("images")) {
                    Ok(()) => {
                        println!("[session] {}", dir.display());
                        active = Some((dir, session));
                    }
                    Err(e) => platform::notice(&format!("Nothing will be saved: can't create {}: {e}", dir.display())),
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
                let mut deleted = None;
                if let Some((dir, session)) = &mut active
                    && let Some(n) = session.steps.last().map(|s| s.number)
                {
                    match remove_step(dir, session, n as usize - 1) {
                        Ok(()) => {
                            println!("[step] {n} deleted");
                            deleted = Some(n);
                        }
                        Err(e) => eprintln!("[error] could not delete step {n}: {e}"),
                    }
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
