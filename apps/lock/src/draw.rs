//! What the lock screen looks like: the `locked` theme color everywhere, and in the middle
//! the time, the date, who is logged in, the password field and a status line.
//!
//! Only the middle block (the "card") is drawn with text; the rest is a flat fill. The card
//! keeps the same size for a given screen, so a redraw only damages that rectangle.

use kiln_config::{config::Color, theme::Theme};
use kiln_draw::{Canvas, Text};

use crate::password::Password;

pub struct Look {
    pub theme: Theme,
    pub text: Option<Text>,
    /// Name shown above the password field.
    pub user_label: String,
}

/// Everything on screen that changes.
pub struct Ui {
    pub password: Password,
    /// A password is being checked.
    pub checking: bool,
    /// Last check turned the password down; with what PAM said, if anything.
    pub denied: Option<Option<String>>,
    /// Wrong passwords in a row.
    pub failures: u32,
    pub caps_lock: bool,
}

/// The card's size in logical pixels, for a theme font size.
fn card_size(fs: f32) -> (f32, f32) {
    (fs * 34.0, fs * 21.0)
}

/// Where the card goes on a `w`×`h` buffer drawn at `scale`: (x, y, width, height).
pub fn card_rect(theme: &Theme, scale: f64, w: i32, h: i32) -> (i32, i32, i32, i32) {
    let (cw, ch) = card_size(theme.font_size);
    let cw = ((cw as f64 * scale).round() as i32).min(w);
    let ch = ((ch as f64 * scale).round() as i32).min(h);
    // A little above the middle reads as centered.
    let y = ((h - ch) as f64 * 0.42).round() as i32;
    ((w - cw) / 2, y.max(0), cw, ch)
}

pub fn background(theme: &Theme) -> Color {
    theme.palette.locked
}

/// Draw the card for a buffer at `scale` (`cw`×`ch` from `card_rect`).
pub fn card(look: &Look, ui: &Ui, scale: f64, cw: i32, ch: i32) -> Canvas {
    let p = &look.theme.palette;
    let s = scale as f32;
    let fs = look.theme.font_size * s;
    let mut c = Canvas::new(cw, ch, background(&look.theme));
    let full = (0, cw);
    let center = |text: &Text, t: &str, px: f32| (cw as f32 - text.width(t, px)) / 2.0;

    let mut y = 0.0;
    if let Some(text) = &look.text {
        // The time: "3:07" large with a smaller "PM" beside it.
        let (time, ampm) = clock();
        let big = fs * 5.0;
        let small = fs * 1.6;
        let gap = fs * 0.4;
        let total = text.width(&time, big) + gap + text.width(ampm, small);
        let x = (cw as f32 - total) / 2.0;
        let baseline = y + big * 1.05;
        text.draw(&mut c, &time, big, (x, baseline), p.text, full);
        text.draw(&mut c, ampm, small, (x + text.width(&time, big) + gap, baseline), p.subtext, full);
        y = baseline + fs * 0.9;

        let day = date();
        let px = fs * 1.35;
        y += px;
        text.draw(&mut c, &day, px, (center(text, &day, px), y), p.subtext, full);
        y += fs * 4.0;

        let px = fs * 1.1;
        y += px;
        let name = text.fit(&look.user_label, px, cw as f32);
        text.draw(&mut c, &name, px, (center(text, &name, px), y), p.muted, full);
        y += fs * 1.0;
    }

    // The password field.
    let field_w = (fs * 20.0).min(cw as f32) as i32;
    let field_h = (fs * 3.0) as i32;
    let fx = (cw - field_w) / 2;
    let fy = y as i32;
    let radius = look.theme.radius * s;
    let border = (s * 2.0).round().max(1.0) as i32;
    let wrong = ui.denied.is_some() && ui.password.is_empty() && !ui.checking;
    let edge = if wrong {
        p.urgent
    } else if ui.checking || !ui.password.is_empty() {
        p.accent
    } else {
        p.separator
    };
    c.fill_rounded_rect(fx, fy, field_w, field_h, radius, edge);
    c.fill_rounded_rect(
        fx + border,
        fy + border,
        field_w - 2 * border,
        field_h - 2 * border,
        (radius - border as f32).max(0.0),
        p.surface,
    );
    let mid_y = fy as f32 + field_h as f32 / 2.0;
    if ui.password.is_empty() || ui.checking {
        if let Some(text) = &look.text {
            let hint = if ui.checking { "Checking…" } else { "Type your password" };
            let px = fs;
            let baseline = text.centered_baseline(fy as f32, field_h as f32, px);
            text.draw(&mut c, hint, px, (center(text, hint, px), baseline), p.subtext, full);
        }
    } else {
        // One dot per character, as many as fit.
        let r = fs * 0.32;
        let step = fs * 1.05;
        let room = ((field_w as f32 - fs * 2.0) / step).floor().max(1.0) as usize;
        let n = ui.password.chars().min(room);
        let start = cw as f32 / 2.0 - (n as f32 - 1.0) * step / 2.0;
        for i in 0..n {
            let cx = start + i as f32 * step;
            let d = (r * 2.0).round() as i32;
            c.fill_rounded_rect((cx - r).round() as i32, (mid_y - r).round() as i32, d, d, r, p.text);
        }
    }

    // The status line under the field.
    if let Some(text) = &look.text {
        let (msg, color) = match &ui.denied {
            Some(said) if wrong => {
                let msg = match said {
                    Some(m) => m.clone(),
                    None if ui.failures > 1 => format!("Wrong password ({} tries)", ui.failures),
                    None => "Wrong password".to_string(),
                };
                (msg, p.urgent)
            }
            _ if ui.caps_lock => ("Caps Lock is on".to_string(), p.muted),
            _ => (String::new(), p.muted),
        };
        if !msg.is_empty() {
            let px = fs;
            let msg = text.fit(&msg, px, cw as f32);
            let baseline = (fy + field_h) as f32 + fs * 2.2;
            text.draw(&mut c, &msg, px, (center(text, &msg, px), baseline), color, full);
        }
    }
    c
}

/// The local time as a 12-hour clock, like the bar's: ("3:07", "PM").
fn clock() -> (String, &'static str) {
    let tm = local_time();
    let h12 = if tm.tm_hour % 12 == 0 { 12 } else { tm.tm_hour % 12 };
    (format!("{h12}:{:02}", tm.tm_min), if tm.tm_hour < 12 { "AM" } else { "PM" })
}

/// Today, like "Wednesday, October 21", in the user's language.
fn date() -> String {
    let tm = local_time();
    let mut buf = [0u8; 128];
    // SAFETY: strftime writes at most `buf.len()` bytes and returns how many it wrote.
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%A, %B %-d".as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn local_time() -> libc::tm {
    // SAFETY: `time(NULL)` only returns the time, and `localtime_r` writes into our own `tm`.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    }
}

/// Time left until the next minute starts, plus a little so the clock never lands just short.
pub fn until_next_minute() -> std::time::Duration {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let into_minute = since_epoch.as_millis() % 60_000;
    std::time::Duration::from_millis((60_000 - into_minute) as u64 + 50)
}
