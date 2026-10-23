//! OS boundary. Nothing outside this module calls platform APIs.

#[cfg(windows)]
mod review;
#[cfg(windows)]
mod sent;
#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{Click, notice, open_folder, process_name, run};
