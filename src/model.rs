//! Pure data for a recording session. No OS calls here.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
}

/// Local wall-clock time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Stamp {
    /// `2026-10-19_14-32-05`, used for the session folder.
    pub fn dir_name(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-10-19 14:32`, used for the Markdown heading.
    pub fn heading(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }

    /// `14:32:05`
    pub fn clock(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }
}

/// Raw capture details, kept in steps.json only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Raw {
    pub button: MouseButton,
    /// Virtual-screen coordinates in physical pixels.
    pub x: i32,
    pub y: i32,
    pub monitor_index: u32,
    /// Top-left of the captured monitor in virtual-screen physical pixels.
    pub monitor_origin: [i32; 2],
    pub scale: f32,
    /// The step was a double-click. The screenshot is from the first click.
    #[serde(default)]
    pub double: bool,
}

impl Raw {
    fn action(&self) -> String {
        let what = match (self.button, self.double) {
            (MouseButton::Left, false) => "Left-click",
            (MouseButton::Right, false) => "Right-click",
            (MouseButton::Left, true) => "Double-click",
            (MouseButton::Right, true) => "Right double-click",
        };
        format!("{what} at ({}, {})", self.x, self.y)
    }
}

/// The system's double-click limits, as Windows applies them: the second press must come within
/// `max_ms` and land within ±`max_dx`/`max_dy` physical pixels of the first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DoubleClick {
    pub max_ms: u32,
    pub max_dx: i32,
    pub max_dy: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub number: u32,
    pub action: String,
    /// What the user says they did, typed in the review window after recording.
    #[serde(default)]
    pub description: String,
    pub window: String,
    pub app: String,
    pub time: String,
    pub image: String,
    pub raw: Raw,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub started: Stamp,
    /// Title the user typed before recording; may be empty.
    #[serde(default)]
    pub title: String,
    pub steps: Vec<Step>,
    /// Input-clock time (ms) of the last single click, while it can still become a double-click.
    #[serde(skip)]
    last_click_ms: Option<u32>,
}

/// `title` made safe for a Windows folder name: no reserved characters, no trailing dots or
/// spaces, whitespace collapsed, at most 60 characters.
fn folder_safe(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() || r#"\/:*?"<>|"#.contains(c) { ' ' } else { c })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let joined: String = words.join(" ").chars().take(60).collect();
    joined.trim_end_matches(['.', ' ']).to_owned()
}

pub fn image_name(number: u32) -> String {
    format!("images/step-{number:03}.png")
}

impl Session {
    pub fn new(started: Stamp) -> Self {
        Self { started, title: String::new(), steps: Vec::new(), last_click_ms: None }
    }

    /// Session folder name: the timestamp, plus the title if there is one.
    pub fn dir_name(&self) -> String {
        match folder_safe(&self.title) {
            t if t.is_empty() => self.started.dir_name(),
            t => format!("{} {t}", self.started.dir_name()),
        }
    }

    pub fn next_number(&self) -> u32 {
        self.steps.len() as u32 + 1
    }

    /// If this press completes a double-click with the previous step, turns that step into a
    /// double-click and returns its number. Otherwise returns None and nothing changes.
    /// A press after a double-click starts a new click, as in Windows.
    pub fn try_double(&mut self, raw: &Raw, ms: u32, rule: DoubleClick) -> Option<u32> {
        let prev_ms = self.last_click_ms?;
        let last = self.steps.last_mut()?;
        let is_double = last.raw.button == raw.button
            && ms.wrapping_sub(prev_ms) <= rule.max_ms
            && (raw.x - last.raw.x).abs() <= rule.max_dx
            && (raw.y - last.raw.y).abs() <= rule.max_dy;
        if !is_double {
            return None;
        }
        last.raw.double = true;
        last.action = last.raw.action();
        self.last_click_ms = None;
        Some(last.number)
    }

    /// Removes step `index` and renumbers the ones after it. Returns the removed step's image
    /// and the (old, new) image renames the caller must apply on disk, in order: each target
    /// name is freed by the delete or the rename before it.
    pub fn remove(&mut self, index: usize) -> Option<(String, Vec<(String, String)>)> {
        if index >= self.steps.len() {
            return None;
        }
        let removed = self.steps.remove(index);
        let mut renames = Vec::new();
        for (i, step) in self.steps.iter_mut().enumerate().skip(index) {
            step.number = i as u32 + 1;
            let new = image_name(step.number);
            renames.push((std::mem::replace(&mut step.image, new.clone()), new));
        }
        if index == self.steps.len() {
            self.last_click_ms = None; // the newest step is gone; nothing to double-click onto
        }
        Some((removed.image, renames))
    }

    pub fn add_click(&mut self, raw: Raw, window: String, app: String, time: String, ms: u32) -> &Step {
        let number = self.next_number();
        self.last_click_ms = Some(ms);
        self.steps.push(Step {
            number,
            action: raw.action(),
            description: String::new(),
            window,
            app,
            time,
            image: image_name(number),
            raw,
        });
        self.steps.last().unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn stamp() -> Stamp {
        Stamp { year: 2026, month: 10, day: 19, hour: 14, minute: 32, second: 5 }
    }

    pub fn raw(button: MouseButton, x: i32, y: i32) -> Raw {
        Raw { button, x, y, monitor_index: 0, monitor_origin: [0, 0], scale: 1.0, double: false }
    }

    /// Windows defaults at 100% scale: 500 ms, 4x4 px rectangle.
    const RULE: DoubleClick = DoubleClick { max_ms: 500, max_dx: 2, max_dy: 2 };

    /// What the worker does with each press: upgrade to a double-click, or add a new step.
    fn press(s: &mut Session, button: MouseButton, x: i32, y: i32, ms: u32) {
        let r = raw(button, x, y);
        if s.try_double(&r, ms, RULE).is_none() {
            s.add_click(r, "W".into(), "a.exe".into(), "t".into(), ms);
        }
    }

    fn actions(s: &Session) -> Vec<&str> {
        s.steps.iter().map(|st| st.action.as_str()).collect()
    }

    #[test]
    fn stamp_formats() {
        let s = stamp();
        assert_eq!(s.dir_name(), "2026-10-19_14-32-05");
        assert_eq!(s.heading(), "2026-10-19 14:32");
        assert_eq!(s.clock(), "14:32:05");
    }

    #[test]
    fn image_names_are_zero_padded() {
        assert_eq!(image_name(1), "images/step-001.png");
        assert_eq!(image_name(42), "images/step-042.png");
        assert_eq!(image_name(1234), "images/step-1234.png");
    }

    #[test]
    fn steps_are_numbered_sequentially() {
        let mut s = Session::new(stamp());
        assert_eq!(s.next_number(), 1);
        s.add_click(raw(MouseButton::Left, 10, 20), "W".into(), "a.exe".into(), "14:32:05".into(), 0);
        let second = s.add_click(raw(MouseButton::Right, -5, 7), "".into(), "".into(), "14:32:06".into(), 9000);
        assert_eq!(second.number, 2);
        assert_eq!(second.image, "images/step-002.png");
        assert_eq!(second.action, "Right-click at (-5, 7)");
        assert_eq!(s.steps[0].action, "Left-click at (10, 20)");
        assert_eq!(s.next_number(), 3);
    }

    #[test]
    fn fast_close_second_press_is_a_double_click() {
        let mut s = Session::new(stamp());
        press(&mut s, MouseButton::Left, 100, 100, 1000);
        press(&mut s, MouseButton::Left, 102, 98, 1500); // at both limits
        assert_eq!(actions(&s), ["Double-click at (100, 100)"]);
        assert!(s.steps[0].raw.double);
        assert_eq!(s.steps[0].image, "images/step-001.png"); // first click's screenshot
        assert_eq!(s.next_number(), 2);
    }

    #[test]
    fn right_double_click() {
        let mut s = Session::new(stamp());
        press(&mut s, MouseButton::Right, 5, 5, 0);
        press(&mut s, MouseButton::Right, 5, 5, 200);
        assert_eq!(actions(&s), ["Right double-click at (5, 5)"]);
    }

    #[test]
    fn slow_far_or_other_button_stays_single() {
        let mut s = Session::new(stamp());
        press(&mut s, MouseButton::Left, 0, 0, 0);
        press(&mut s, MouseButton::Left, 0, 0, 501); // too slow
        press(&mut s, MouseButton::Left, 3, 0, 600); // too far in x
        press(&mut s, MouseButton::Left, 3, 3, 700); // too far in y
        press(&mut s, MouseButton::Right, 3, 3, 750); // other button
        assert_eq!(s.steps.len(), 5);
        assert!(s.steps.iter().all(|st| !st.raw.double));
    }

    #[test]
    fn third_press_starts_a_new_click() {
        let mut s = Session::new(stamp());
        for ms in [0, 100, 200, 300] {
            press(&mut s, MouseButton::Left, 50, 50, ms);
        }
        assert_eq!(actions(&s), ["Double-click at (50, 50)", "Double-click at (50, 50)"]);
        press(&mut s, MouseButton::Left, 50, 50, 400);
        assert_eq!(s.steps.len(), 3);
        assert_eq!(s.steps[2].action, "Left-click at (50, 50)");
    }

    #[test]
    fn double_click_across_input_clock_wraparound() {
        let mut s = Session::new(stamp());
        press(&mut s, MouseButton::Left, 0, 0, u32::MAX - 100);
        press(&mut s, MouseButton::Left, 0, 0, 100);
        assert_eq!(actions(&s), ["Double-click at (0, 0)"]);
    }

    #[test]
    fn removing_a_middle_step_renumbers_the_rest() {
        let mut s = Session::new(stamp());
        for (i, x) in [10, 20, 30, 40].into_iter().enumerate() {
            press(&mut s, MouseButton::Left, x, 0, i as u32 * 1000);
        }
        let (gone, renames) = s.remove(1).unwrap();
        assert_eq!(gone, "images/step-002.png");
        assert_eq!(
            renames,
            [
                ("images/step-003.png".to_owned(), "images/step-002.png".to_owned()),
                ("images/step-004.png".to_owned(), "images/step-003.png".to_owned()),
            ]
        );
        let summary: Vec<_> = s.steps.iter().map(|st| (st.number, st.raw.x, st.image.as_str())).collect();
        assert_eq!(
            summary,
            [(1, 10, "images/step-001.png"), (2, 30, "images/step-002.png"), (3, 40, "images/step-003.png")]
        );
        assert_eq!(s.next_number(), 4);
        assert!(s.remove(3).is_none()); // out of range
    }

    #[test]
    fn removing_the_newest_step_cancels_a_pending_double_click() {
        let mut s = Session::new(stamp());
        press(&mut s, MouseButton::Left, 5, 5, 0);
        press(&mut s, MouseButton::Left, 9, 9, 1000);
        let (gone, renames) = s.remove(1).unwrap();
        assert_eq!((gone.as_str(), renames.len()), ("images/step-002.png", 0));
        press(&mut s, MouseButton::Left, 9, 9, 1100); // would have doubled the removed step
        assert_eq!(actions(&s), ["Left-click at (5, 5)", "Left-click at (9, 9)"]);
        assert_eq!(s.steps[1].image, "images/step-002.png"); // number reused
    }

    #[test]
    fn folder_names_include_a_safe_title() {
        let mut s = Session::new(stamp());
        assert_eq!(s.dir_name(), "2026-10-19_14-32-05");
        s.title = "  Reset a user's password  ".into();
        assert_eq!(s.dir_name(), "2026-10-19_14-32-05 Reset a user's password");
        s.title = r#"A/B: "C" <d>|e?*\f..."#.into();
        assert_eq!(s.dir_name(), "2026-10-19_14-32-05 A B C d e f");
        s.title = "x".repeat(80);
        assert_eq!(s.dir_name().len(), "2026-10-19_14-32-05 ".len() + 60);
        s.title = " ...  ".into();
        assert_eq!(s.dir_name(), "2026-10-19_14-32-05");
    }

    #[test]
    fn json_round_trip() {
        let mut s = Session::new(stamp());
        press(&mut s, MouseButton::Left, 1, 2, 0);
        press(&mut s, MouseButton::Left, 1, 2, 10);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""button":"left""#));
        assert!(json.contains(r#""double":true"#));
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!((back.started, back.steps), (s.started, s.steps));
    }
}
