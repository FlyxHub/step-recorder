//! Pure output rendering: steps.md text and the click highlight.

use crate::model::Session;
use image::{Rgba, RgbaImage};

/// Window titles and process names come from other apps, so newlines are flattened to keep
/// them from forging fields or headings in steps.md.
fn or_default(s: &str, default: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if s.trim().is_empty() { default.into() } else { s.trim().into() }
}

pub fn or_unknown(s: &str) -> String {
    or_default(s, "(unknown)")
}

/// Backslash-escapes Markdown in foreign text, so a web page titled `![](https://host/x.png)`
/// can't make steps.md load a remote image (or a link or HTML) when it's viewed.
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\`*_[]()!<>#|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn markdown(session: &Session) -> String {
    let title = or_default(&session.title, "Recording");
    let mut out = format!("# {title} — {}\n", session.started.heading());
    for s in &session.steps {
        out += &format!(
            "\n## Step {n}\n\
             - **Action:** {}\n\
             - **Description:** {}\n\
             - **Window:** {}\n\
             - **App:** {}\n\
             - **Time:** {}\n\
             \n![Step {n}]({})\n",
            s.action,
            or_default(&s.description, "(none)"),
            or_unknown(&md_escape(&s.window)),
            or_unknown(&md_escape(&s.app)),
            s.time,
            s.image,
            n = s.number,
        );
    }
    out
}

/// Largest rectangle with the image's aspect ratio that fits in a `box_w` x `box_h` box,
/// centred in it. Returns (x, y, w, h) relative to the box.
pub fn fit(img_w: u32, img_h: u32, box_w: i32, box_h: i32) -> (i32, i32, i32, i32) {
    if img_w == 0 || img_h == 0 {
        return (0, 0, 0, 0);
    }
    let k = (box_w as f32 / img_w as f32).min(box_h as f32 / img_h as f32);
    let (w, h) = ((img_w as f32 * k).round() as i32, (img_h as f32 * k).round() as i32);
    ((box_w - w) / 2, (box_h - h) / 2, w, h)
}

/// Classic arrow pointer outline, tip (the hot spot) at the origin, ~18.5 units tall.
const ARROW: [(f32, f32); 7] =
    [(0.0, 0.0), (0.0, 16.0), (4.0, 12.5), (7.0, 18.5), (9.5, 17.5), (6.5, 11.5), (11.5, 11.5)];

/// Even-odd ray cast.
fn inside(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut hit = false;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        if (a.1 > y) != (b.1 > y) && x < a.0 + (y - a.1) * (b.0 - a.0) / (b.1 - a.1) {
            hit = !hit;
        }
    }
    hit
}

/// Distance from (x, y) to the polygon's outline.
fn edge_dist(poly: &[(f32, f32)], x: f32, y: f32) -> f32 {
    (0..poly.len())
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            (x - a.0 - t * dx).hypot(y - a.1 - t * dy)
        })
        .fold(f32::MAX, f32::min)
}

/// Black arrow pointer with a white outline, tip at (tx, ty). Sizes scale with monitor DPI.
/// GDI screenshots never include the real cursor, so this is the only pointer in the image.
pub fn draw_cursor(img: &mut RgbaImage, tx: i32, ty: i32, scale: f32) {
    let (k, border) = (1.5 * scale, 1.5 * scale);
    let poly = ARROW.map(|(x, y)| (tx as f32 + x * k, ty as f32 + y * k));
    let pad = border.ceil() as i32 + 1;
    let (w, h) = (img.width() as i32, img.height() as i32);
    let x_max = tx + (11.5 * k).ceil() as i32 + pad;
    let y_max = ty + (18.5 * k).ceil() as i32 + pad;
    for y in (ty - pad).max(0)..=y_max.min(h - 1) {
        for x in (tx - pad).max(0)..=x_max.min(w - 1) {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let (inn, d) = (inside(&poly, px, py), edge_dist(&poly, px, py));
            let color = if inn && d > border {
                Rgba([0, 0, 0, 255])
            } else if inn || d <= border {
                Rgba([255, 255, 255, 255])
            } else {
                continue;
            };
            img.put_pixel(x as u32, y as u32, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{raw, stamp};
    use crate::model::{MouseButton, Session};

    #[test]
    fn markdown_matches_format() {
        let mut s = Session::new(stamp());
        s.add_click(
            raw(MouseButton::Left, 812, 344),
            "Contoso Admin Portal — Microsoft Edge".into(),
            "msedge.exe".into(),
            "14:32:05".into(),
            0,
        );
        s.add_click(raw(MouseButton::Right, 5, 6), " ".into(), "".into(), "14:32:11".into(), 6000);
        s.steps[0].description = "  Opened the Settings page ".into();
        s.steps[1].description = "   ".into();
        let expected = "# Recording — 2026-10-19 14:32

## Step 1
- **Action:** Left-click at (812, 344)
- **Description:** Opened the Settings page
- **Window:** Contoso Admin Portal — Microsoft Edge
- **App:** msedge.exe
- **Time:** 14:32:05

![Step 1](images/step-001.png)

## Step 2
- **Action:** Right-click at (5, 6)
- **Description:** (none)
- **Window:** (unknown)
- **App:** (unknown)
- **Time:** 14:32:11

![Step 2](images/step-002.png)
";
        assert_eq!(markdown(&s), expected);
        s.title = " Reset a user password ".into();
        assert!(markdown(&s).starts_with("# Reset a user password — 2026-10-19 14:32\n\n## Step 1\n"));
    }

    #[test]
    fn foreign_text_cannot_forge_fields() {
        let mut s = Session::new(stamp());
        s.add_click(
            raw(MouseButton::Left, 1, 2),
            "A\n- **Description:** evil\r\n".into(),
            "x.exe\n## Step 9".into(),
            "14:32:05".into(),
            0,
        );
        let md = markdown(&s);
        assert!(md.contains("- **Description:** (none)\n"));
        assert!(md.contains("- **Window:** A - \\*\\*Description:\\*\\* evil\n"));
        assert!(md.contains("- **App:** x.exe \\#\\# Step 9\n"));
        assert_eq!(md.matches("**Description:**").count(), 1); // the quoted text is escaped
        assert_eq!(md.lines().filter(|l| l.starts_with('#')).count(), 2); // title + Step 1
    }

    #[test]
    fn foreign_text_cannot_embed_images_or_html() {
        let mut s = Session::new(stamp());
        s.add_click(
            raw(MouseButton::Left, 1, 2),
            "![](https://evil/x.png?id) <img src=x> [[https://evil]] — Edge".into(),
            "my_app.exe".into(),
            "14:32:05".into(),
            0,
        );
        let md = markdown(&s);
        assert!(md.contains(
            r"- **Window:** \!\[\]\(https://evil/x.png?id\) \<img src=x\> \[\[https://evil\]\] — Edge"
        ));
        assert!(md.contains(r"- **App:** my\_app.exe"));
        assert!(!md.contains("![]("));
    }

    #[test]
    fn fit_keeps_aspect_and_centres() {
        assert_eq!(fit(1920, 1080, 736, 414), (0, 0, 736, 414)); // same ratio fills the box
        assert_eq!(fit(1920, 1080, 736, 600), (0, 93, 736, 414)); // wide image: bars top and bottom
        assert_eq!(fit(1080, 1920, 736, 414), (251, 0, 233, 414)); // tall image: bars left and right
        assert_eq!(fit(0, 0, 736, 414), (0, 0, 0, 0));
    }

    #[test]
    fn cursor_tip_sits_on_click_point() {
        let mut img = RgbaImage::new(100, 100);
        draw_cursor(&mut img, 50, 50, 1.0);
        let (black, white, none) = ([0, 0, 0, 255], [255, 255, 255, 255], [0, 0, 0, 0]);
        assert_eq!(img.get_pixel(50, 50).0, white); // tip outline at the hot spot
        assert_eq!(img.get_pixel(54, 65).0, black); // arrow body, below and right of tip
        assert_eq!(img.get_pixel(49, 60).0, white); // left outline
        assert_eq!(img.get_pixel(40, 60).0, none); // left of the arrow
        assert_eq!(img.get_pixel(70, 52).0, none); // right of the diagonal edge
        assert_eq!(img.get_pixel(50, 45).0, none); // above the tip
    }

    #[test]
    fn cursor_scales_with_dpi() {
        let mut img = RgbaImage::new(100, 100);
        draw_cursor(&mut img, 10, 10, 2.0);
        assert_eq!(img.get_pixel(16, 40).0, [0, 0, 0, 255]); // inside only at 2x
        let mut small = RgbaImage::new(100, 100);
        draw_cursor(&mut small, 10, 10, 1.0);
        assert_eq!(small.get_pixel(16, 40).0, [0, 0, 0, 0]);
    }

    #[test]
    fn cursor_clips_at_edges() {
        let mut img = RgbaImage::new(10, 10);
        draw_cursor(&mut img, 8, 8, 2.0);
        draw_cursor(&mut img, -500, 9000, 1.0);
        let mut empty = RgbaImage::new(0, 0);
        draw_cursor(&mut empty, 0, 0, 1.0);
    }
}
