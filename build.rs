// Embeds the app icon (resource ID 1) and version info from Cargo.toml into the exe.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Step Recorder");
        res.set("FileDescription", "Step Recorder"); // shown in Task Manager
        res.set("OriginalFilename", "step-recorder.exe");
        res.compile().expect("embedding Windows resources needs rc.exe from the Windows SDK");
    }
}
