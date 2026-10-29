# Step Recorder

A small Windows app that records how to do something. Every click becomes a screenshot with a
cursor arrow at the click point. The steps are written as Markdown and JSON, ready for you (or an
AI) to turn into documentation.

## Download

Get `step-recorder.exe` from the [latest release](https://github.com/FlyxHub/step-recorder/releases/latest).
It's a single 64-bit exe for Windows 10 (version 2004 or later) or Windows 11. There's no
installer and no runtime to install. The exe is unsigned, so SmartScreen will warn the first time you run it.

## Use

1. Run the exe. A small control window appears.
2. Optionally type a title, then press **Start** (or Enter, or Ctrl+Alt+R).
3. Click through the task. While recording, the window stays on top and shows `● REC`.
4. Press **Stop** (or Ctrl+Alt+R). A review window opens: add a description to each step, give
   the recording a title, and save. File Explorer then opens on the recording.

| Hotkey | Action |
|---|---|
| Ctrl+Alt+R | Start / stop recording |
| Ctrl+Alt+P | Pause / resume |

> **Pause before typing anything sensitive.** Screenshots capture whatever is on screen, and
> password fields aren't redacted yet.

While recording you can also **Delete last step** or **Abort**, which discards the whole
recording.

## Output

Recordings go to `%USERPROFILE%\Step Recordings\<date>_<time> <title>\`:

```
steps.md          one section per step: Action, Description, Window, App, Time, screenshot
steps.json        the same data, plus raw click coordinates and monitor info
images/step-001.png …
```

The `steps.md` layout is fixed, so it works well as input to an AI prompt.

## Send to Obsidian (optional)

The review window can send a recording to an [Obsidian](https://obsidian.md) vault. It copies
the screenshots into the vault's attachment folder and runs your own
[Claude Code](https://claude.com/claude-code) CLI (`claude`, version 2.1.248 or later) in the vault to
write a note from the steps, following the vault's `CLAUDE.md` if it has one. When it's done,
**Open in Obsidian** opens the note.

Claude Code runs with a fixed set of file tools (Read, Glob, Grep, Write, Edit), with no MCP servers
and with `--restricted`. That way text captured in a recording, or settings in a shared vault,
can't make it run commands or reach other services.

## Privacy

Step Recorder makes no network calls. Recordings stay on your machine, in a folder that OneDrive
doesn't sync by default. The only exception is **Send to Obsidian**, which runs only when you
press it. Claude Code then sends that recording's steps and screenshots to Anthropic to write
the note.

## Build from source

Requires Rust (stable, `x86_64-pc-windows-msvc`) and the Visual Studio Build Tools with the Windows
SDK (`build.rs` uses `rc.exe` to embed the icon and version info).

```
cargo test
cargo build --release
```

The exe lands in `target\release\step-recorder.exe`. For a build you'll share, remap your home
path so it isn't embedded in the exe (PowerShell):

```
cargo build --release --config "target.x86_64-pc-windows-msvc.rustflags=['--remap-path-prefix=$env:USERPROFILE=~']"
```

Before uploading, check that it worked. This must print `False`:

```
Select-String -Path target\release\step-recorder.exe -SimpleMatch $env:USERPROFILE -Quiet
```

## Limitations

- Records mouse clicks only; keystrokes aren't captured yet.
- Password fields aren't redacted yet, so pause first.
- Clicks on apps running as administrator aren't recorded unless Step Recorder runs as
  administrator too. The status line warns while such a window is in front.
