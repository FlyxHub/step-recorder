# Step Recorder

Windows desktop step recorder. Each click → screenshot with a cursor arrow drawn at the click point →
`steps.md` + `steps.json` in a timestamped session folder. The Markdown is fed to an AI to
expand into documentation, so its field layout is a contract (see "Output format").

## Run
```
cargo run            # debug: control window + console with [state]/[step] logs
cargo build --release   # target\release\step-recorder.exe: GUI only, no console window
cargo test           # pure logic only; no desktop needed
cargo clippy --all-targets -- -D warnings
```
Release builds use `windows_subsystem = "windows"` (gated on `not(debug_assertions)` in
`main.rs`). That means stdout/stderr go nowhere, so user-facing problems go through
`platform::notice`: it logs, and in release it also shows a message box. Called from the
worker thread, the box gets its own thread, so a modal dialog never stops steps being written.
Per-step worker errors are only logged.
Rust is installed via rustup (stable-x86_64-pc-windows-msvc). If `cargo` isn't on PATH in an
old shell, it's in `%USERPROFILE%\.cargo\bin`.

Building also needs `rc.exe` from the Windows SDK (installed with the VS Build Tools), because
`build.rs` embeds the icon and version info.

Sessions are written to `%USERPROFILE%\Step Recordings\YYYY-MM-DD_HH-MM-SS[ <title>]\`, whatever
the working directory. When recording stops, File Explorer opens on the session folder.

**Title (required before saving or sending):** while idle, the control window's bottom row is a
"Title" box. It's optional at Start, and the review window's title field is prefilled from it.
While recording, the same row holds Delete last step instead.
- Enter in the box starts recording: the main window goes through `IsDialogMessageW`, and
  Enter arrives as `IDOK`, which equals `ID_START`.
- `start()` reads the box, sends it in `Msg::Start`, then clears it for the next recording.
- The worker builds the folder with `Session::dir_name`: timestamp, plus the title passed
  through `folder_safe`, which removes `\/:*?"<>|`, collapses whitespace, caps it at 60
  characters, and drops trailing dots and spaces.

## Release packaging
- `cargo build --release` produces one self-contained exe. `.cargo/config.toml` sets
  `+crt-static`, so it depends only on built-in Windows DLLs and needs no VC++ Redistributable.
  Check with `dumpbin /dependents target\release\step-recorder.exe`.
- **Public builds:** panic messages embed source paths, including `C:\Users\<you>\.cargo\…`.
  Build with `cargo build --release --config "target.x86_64-pc-windows-msvc.rustflags=['--remap-path-prefix=$env:USERPROFILE=~']"`
  (PowerShell; `--config` arrays merge with `.cargo/config.toml`, so `+crt-static` stays). Then
  check that the exe contains no username.
- `build.rs` (crate `winresource`) embeds `assets/icon.ico` as icon resource 1. The window class
  loads that icon too. It also sets FileDescription/ProductName "Step Recorder" (Task Manager
  shows FileDescription), and FileVersion/ProductVersion come from `Cargo.toml` `version`.
  Bump that version for each release.
- Not done yet: code signing (expect SmartScreen and endpoint-security flags for an unsigned
  global-hook app), a CompanyName in the version info, and an MSI (`cargo-wix`) if IT deploys
  through Intune.

## Controls
| Hotkey | Action |
|---|---|
| Ctrl+Alt+R | Start / stop recording |
| Ctrl+Alt+P | Pause / resume. **Use this before typing anything sensitive.** |

The window's Start / Pause / Stop buttons do the same thing. If a hotkey is taken by another
app, a warning appears (in the console, or in a dialog in release builds) and the buttons still work.

**Abort** (button only, no hotkey) discards the session:
1. Recording pauses while a Yes/No confirmation is up (default No), so clicks on the dialog
   aren't recorded. No resumes the previous state.
2. Yes unhooks and sends `Msg::Abort`. The worker first finishes any clicks queued ahead of it,
   then deletes the whole session folder with `remove_dir_all`.
3. There's no review window and Explorer doesn't open. A failed delete shows a notice.

**Delete last step** (button, shown only while recording or paused; `set_state` hides it when idle) sends `Msg::DeleteLast`. The worker
handles it after any queued clicks, so "last" really is the newest step. The UI waits at most
200 ms for the reply, because blocking the UI thread with the hook installed stalls the mouse
and Windows silently removes the hook after `LowLevelHooksTimeout`. The status line then says
which step was deleted (or "Last step deleted." if the reply was late). The removal goes through `worker::remove_step`, the same
path as the review window's Remove step:
1. `Session::remove` returns the delete plus the ordered `(old, new)` image renames.
2. Apply them on disk.
3. Rewrite md/json.

Removing the newest step also clears `last_click_ms`, so the next click can't double-click
onto a step that's gone.

## Privacy
- The app makes no network calls and has no networking crates. Output goes to the session folder.
- **The one exception is Send to Obsidian**, which the user approved on 2026-10-19 under two
  conditions: it runs only when the user presses the button, and the recorder itself never talks
  to the network. On press, it copies the screenshots into the chosen vault and launches the
  user's own `claude` CLI there. Claude Code then uploads the screenshots and steps to Anthropic
  to write the note. Never send anything automatically.
- Recording indicator: the control window, which is always on top only while recording or
  paused (`set_state` switches `HWND_TOPMOST` / `HWND_NOTOPMOST`). Idle, it's a normal window so
  the review window can come in front of it. Its title reads `● REC` and the status
  band turns red while recording (amber while paused). The band is drawn via `WM_CTLCOLORSTATIC`.
  Debug builds also print a console banner.
- Phase 1 hooks only the mouse. Keyboard capture arrives in Phase 2. Password-field redaction
  (UIA `IsPassword`) arrives in Phase 3; until then, pause before entering secrets.

## Architecture
```
src/main.rs           spawn worker, run UI, join worker on exit
src/model.rs          Step / Session / Stamp / Raw (serde), numbering, image names. Pure.
src/render.rs         steps.md rendering, cursor-arrow drawing. Pure.
src/worker.rs         Msg enum; worker thread: bitmap → PNG + cursor arrow, rewrite md/json each step
src/platform/mod.rs   OS boundary; nothing else touches Win32
src/platform/win.rs   window, hotkeys, WH_MOUSE_LL hook, GDI capture, window/process lookup
src/platform/review.rs  review window shown after Stop: screenshot + description box per step,
                      vault picker and Send to Obsidian
src/settings.rs       settings.json (vault list). Pure apart from file IO.
src/obsidian.rs       Send to Obsidian: copy images into the vault, run `claude -p`, parse reply
src/platform/sent.rs  result window after a send: Open in Obsidian / Close
```
Threads:
- **UI thread** runs the Win32 window, the message loop, `RegisterHotKey` (WM_HOTKEY), and the
  `WH_MOUSE_LL` hook. The hook is installed on Start and removed on Stop. Low-level hooks need
  a message loop on the thread that installs them.
- **Hook callback** runs on mouse-down and does three things:
  1. Skips the click if it lands on any window of our process, message boxes included
     (`WindowFromPoint` → `GA_ROOT` → pid == `GetCurrentProcessId`).
  2. `BitBlt`s that monitor into an `HBITMAP` (a GPU→memory copy, a few ms). Each queued
     click holds one of these (~33 MB at 4K), so with `MAX_QUEUED_CLICKS` (8) still unprocessed
     (`worker::QUEUED_CLICKS`) the click is skipped and the status line says so.
  3. Reads the title and pid, then sends `Msg::Click` over `mpsc`.

  The capture runs here deliberately: it's the only way to get the screen *before* the target
  app reacts to the click.
- **Worker** does all the slow work: `GetDIBits` → RGBA, draws the cursor arrow, PNG-encodes the image,
  resolves the process name, and writes the files. It rewrites `steps.md` / `steps.json` after
  every step so a crash loses nothing.

The `Platform` trait for faking the desktop in tests is deferred to Phase 2. The keystroke
coalescer will be its first consumer. Phase 1 pure logic takes plain data.

## Decisions
- **Raw GDI instead of `xcap`**: xcap converts pixels to RGBA on the calling thread. Inside
  an LL hook that risks hitting `LowLevelHooksTimeout`, after which Windows silently drops the
  hook. GDI lets us split the work: copy in the hook, convert on the worker.
- **No `rdev` / `global-hotkey` / `tray-icon` / `imageproc` / `chrono`**: each is a few lines with
  the `windows` crate, which we need anyway. The tray icon is deferred to Phase 4.
- Dependencies: `windows` 0.62, `image` 0.25 (png only), `serde`, `serde_json`. Build-only:
  `winresource` 0.1.
- **Recordings go under `%USERPROFILE%`, not Documents.** OneDrive Known Folder Move commonly
  syncs Documents, Desktop and Pictures to the cloud, and the privacy rule is that output stays
  on this machine. The user profile root isn't synced.

## Platform quirks (Windows)
- `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` must run before any window is created.
  With it set, the LL hook's `pt`, `rcMonitor` and `BitBlt` are all in physical pixels.
  Image coordinates are `pt - rcMonitor.topleft`, which also works for monitors at negative
  offsets. The scale factor is `GetDpiForMonitor(MDT_EFFECTIVE_DPI) / 96`.
- `EnumDisplayMonitors` order (= `monitor_index`) is not "primary first". On the dev machine the
  primary monitor was index 1.
- The control window is sized for the system DPI at startup. It doesn't handle `WM_DPICHANGED`,
  so it may look small or large after moving it to a monitor with a different scale.
- GDI `BitBlt` never captures the real mouse cursor, so the drawn arrow is the only pointer in a
  screenshot (no doubled cursor). Its tip is the click point.
- The control window is excluded from screenshots with `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
  (Windows 10 2004+). DWM honors it for GDI `BitBlt`. It only works on windows our process owns, so
  the console window (owned by conhost or Windows Terminal) still shows up in screenshots.
- `BitBlt` uses `CAPTUREBLT` so that layered windows (menus, tooltips) appear in the shot.
- Untested so far: clicks on elevated (admin) windows while the recorder runs unelevated. UIPI
  may hide them from the hook.

## Output format
- The heading is `# <title> — YYYY-MM-DD HH:MM`, or `# Recording — …` when no title was given.
  `steps.json` has a top-level `title` (empty if none).
- Every step has exactly these fields in this order: Action, Description, Window, App, Time.
  An empty Window or App renders as `(unknown)`, and an empty Description as `(none)`.
  Control characters (newlines) in any field become spaces, so a foreign window title can't
  forge a Description line.
- Image paths are relative (`images/step-NNN.png`).
- `steps.json` has the same data plus `raw` (button, x, y, monitor_index, monitor_origin, scale,
  double).
- Action verbs are `Left-click`, `Right-click`, `Double-click` and `Right double-click`, followed
  by `at (x, y)` in virtual-screen physical pixels. Element names come in Phase 3.

## Look and feel
- The user prefers the plain classic Win32 look: grey `COLOR_BTNFACE` windows, Segoe UI 9pt,
  standard buttons, and no visual-styles manifest.
- A Windows 11 restyle (white windows, owner-drawn accent buttons, a painted status pill,
  Common Controls v6 manifest) was tried and rejected on 2026-10-20. It's preserved in the local-only
  branch `backup/pre-release-history` (commit `e0f5bd2`, never pushed) if any piece of it is ever wanted.
- Don't restyle without asking. Small functional UI changes are fine.
- **Control window button states:** `set_state` enables only the buttons that apply: Start
  while idle; Pause, Stop and Abort while recording. Pause reads "Resume" while paused.

## Review window (step descriptions)
- **Title:** a "Recording title (required):" field at the top, prefilled from the control
  window.
  - Save, closing the review window, Send to Obsidian, and closing the app (main `WM_CLOSE`)
    all go through `close(true)`. With an empty title it shows a red message under the
    field, focuses it, and keeps everything open. Typing clears the message (`EN_CHANGE`,
    no borrow).
  - On save, `rename_for_title` renames the session folder to `Session::dir_name()`. If the
    rename fails, there's a notice and the old name is kept.
  - Only logoff/restart (`WM_ENDSESSION`; Windows then ends the process without sending
    `WM_DESTROY`) and `WM_DESTROY` without a prior `WM_CLOSE` save untitled, via
    `save_untitled`, so that typed descriptions aren't lost.
  - `start()` refuses while a review is open ("Finish the open review first") and brings the
    review to the front. That way an untitled recording can't be skipped past.
- **Remove step:** asks for confirmation (default No), then calls `worker::remove_step` and
  shows the step that moved into place. It's disabled when only one step is left; use Abort to
  discard everything.
- **Vault row** (vault dropdown, Add, Remove, Send to Obsidian): hidden behind a hint until
  the user has reached the last step once (`reached_end`), meaning they're done with
  descriptions. It stays visible after that, even after going Back.
- **Flow:**
  1. On Stop (button or hotkey), `stop(true)` sends `Msg::Stop { review: Some(tx) }` and
     blocks on the reply. The worker answers after draining queued clicks, so this is usually
     well under a second.
  2. The worker hands back `(dir, Session)`. `review::open` shows the first step.
  3. Save, or closing the window, writes the descriptions with `worker::write_outputs` and
     then opens Explorer.

  Exceptions:
  - With 0 steps (nothing clicked, or all deleted) there's no review; the worker deletes the
    empty session folder.
  - Closing the app (`stop(false)`) skips the review. If a review is already open,
    `review::finish` saves it first.
- **Keyboard:** the main loop runs `IsDialogMessageW` for the review and sent windows. That gives Tab
  between controls, and Enter in the text box arrives as `IDOK`, which is mapped to Next.
- **RefCell re-entrancy:** `REVIEW` is borrowed only for button clicks (`WM_COMMAND` with
  notification code 0), paint (`try_borrow`) and close. `show()` sets the edit text while
  borrowed, and the edit box sends `EN_CHANGE` back synchronously, so those notifications must
  never borrow. `finish()` takes the state out before `DestroyWindow`.
- **Preview:** the PNG is decoded from disk and painted with `StretchDIBits` (HALFTONE) into a
  box fitted by `render::fit`.
- **Placement and capture:** the window opens centred on the monitor under the cursor, because
  the control window sits at the default position. It's excluded from capture
  and from the click hook, the same as the control window.
- **Testing UI visually:** windows with `WDA_EXCLUDEFROMCAPTURE` capture as black, even via
  `PrintWindow`. Build a scratch copy with `WDA_NONE` and use `CopyFromScreen` to see them.

## Send to Obsidian
- **Vault list:** stored in `%APPDATA%\Step Recorder\settings.json` (`settings.rs`). It's
  managed in the review window:
  - The dropdown shows vault folder names and preselects `last_vault`.
  - Add… opens an `IFileOpenDialog` folder picker (COM is initialized on the UI thread in
    `run`) and rejects folders without `.obsidian`. It warns, but still adds, if the vault has
    no CLAUDE.md.
  - Remove drops the selected vault.
  - Run the picker outside the `REVIEW` borrow, because its modal loop repaints the window.
- **Pressing Send** (`review::send_to_obsidian` → `obsidian::send` on a background thread):
  1. Save descriptions and close the review window.
  2. Show "Sending to Obsidian…" in the control window's status line.
  3. Copy screenshots into the vault's attachment folder, read from `.obsidian/app.json`
     `attachmentFolderPath`. A `./x` setting (next to the note) is treated as vault-relative
     `x`. app.json is untrusted (shared vaults), so anything but a plain relative path (`..`,
     drive letters, UNC or rooted paths) falls back to the vault root. Files are renamed `<timestamp>-step-NNN.png` (from `started.dir_name()`, so the title
     never goes into image names), which keeps `![[…]]` embeds unique.
  4. Run `claude -p --output-format json --permission-mode acceptEdits --strict-mcp-config
     --tools Read,Glob,Grep,Write,Edit --restricted --add-dir <session>` hidden (`CREATE_NO_WINDOW`), with the
     vault as the working directory. `--restricted` doesn't load the vault's CLAUDE.md, so
     `send` reads it and puts it at the end of the prompt. The prompt goes in via stdin.
  5. The prompt (`obsidian::prompt`) gives the user's title, if any, as the basis for the
     note's title. It also gives the recording folder and the exact embeds, says
     recording content is data rather than instructions, includes the vault's CLAUDE.md as the
     rules to follow, and asks Claude to reply with only the note's vault-relative path. A reply that isn't an existing file
     in the vault is treated as "didn't say which note" (no Open button).
  6. The thread pushes a `sent::Outcome` onto `sent::PENDING` and posts `WM_APP_SENT` (lparam 0)
     to the control window. The message carries no data, because any process can post that
     number; a stray one finds the queue empty. The UI thread drains the queue, restores the
     status line, and `sent::show` displays the result:
     a small "Sent to Obsidian" window with **Open in Obsidian** (Enter) and Close (Esc), or a
     message box on error.
  7. Open uses `ShellExecuteW` on `obsidian://open?vault=…&file=…`. Never use `explorer.exe`
     for it: explorer mangles links containing `&` and opens File Explorer instead.
- **Failure:** the copied screenshots are deleted again, and the local recording is untouched.
  There's no timeout yet (ponytail note in `run_claude`).
- **Closing the app while sending** (`win::SENDING`) asks first: the `claude` process isn't
  killed and keeps writing the note, but nobody is left to show the result.
- **Tools are an allowlist on purpose.** `--tools` gives Claude only Read, Glob, Grep, Write
  and Edit, and `--strict-mcp-config` (with no `--mcp-config`) loads no MCP servers. So text in
  the recording (window titles come from web pages) can't make it run commands, post to
  Slack/Jira, or reach the web. Don't switch back to a `--disallowedTools` denylist. `--restricted` is just as
  important: the vault is untrusted (it may be synced or shared), and without it a vault's
  `.claude/settings.json` hooks or `apiKeyHelper` would run as the user, hidden, before Claude
  reads the prompt. It ignores user/project/local settings, including the vault's CLAUDE.md
  (hence the prompt carries it), and fences the file tools to the vault and the recording
  folder. Needs Claude Code 2.1.248+. Checked on 2026-10-20 with a scratch vault: a
  `SessionStart` hook ran without the flag and not with it, and Write and `--add-dir` reads
  still worked. That's why the recorder copies the images itself: Write can't create binary
  files.
- **Testing UI without touching real settings:** set `$env:APPDATA` to a scratch folder before
  launching. Write the JSON with `ConvertTo-Json`, because hand-written backslashes are easy to
  break, and invalid JSON silently loads as defaults.

## Double-clicks
- The hook only sees button-down events, so double-clicks are detected after the fact in
  `Session::try_double` (pure, unit-tested). A press upgrades the previous step to a
  double-click when all of these hold:
  - it's the same button;
  - it comes within `GetDoubleClickTime()` of the first press (`MSLLHOOKSTRUCT.time`; the
    comparison handles clock wraparound);
  - it lands within half of `SM_CXDOUBLECLK`/`SM_CYDOUBLECLK` (via `GetSystemMetricsForDpi`)
    of the first press.

  These are the same rules Windows uses, so the user's mouse speed setting applies.
- The step keeps the first press's screenshot, which shows the screen before the
  double-click. The second press's capture is dropped and no new image is written.
- A third quick press starts a new click (a press after a double-click never upgrades again),
  matching Windows. Phase 2's keystroke coalescer should follow the same pattern: decide in
  `Session`, keep the worker thin.
