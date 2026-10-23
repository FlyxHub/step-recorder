// Release builds are a GUI app with no console window. `cargo run` (debug) keeps the console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod model;
mod obsidian;
mod platform;
mod render;
mod settings;
mod worker;

fn main() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || worker::run(rx));
    if let Err(e) = platform::run(tx) {
        platform::notice(&format!("Step Recorder failed: {e}"));
    }
    // The UI dropped its sender on exit; wait for the worker to finish writing.
    let _ = worker.join();
}
