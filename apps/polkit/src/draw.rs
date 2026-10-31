//! What the password dialog looks like: the screen dimmed with the `scrim` color, and in the
//! middle a card with what is being asked, whose password, the password field, a status line,
//! and Cancel / Authenticate buttons.

use wt_config::{config::Color, theme::Theme};
use wt_draw::{Canvas, Text};

use crate::password::Password;

pub struct Look {
    pub theme: Theme,
    pub text: Option<Text>,
}

/// Something on the card that can be clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Cancel,
    Ok,
    SwitchUser,
}

/// Everything on the card that changes.
pub struct Ui<'a> {
    pub message: &'a str,
    pub action_id: &'a str,
    /// Whose password is asked for.
    pub user_label: &'a str,
    pub can_switch: bool,
    /// The helper's prompt, once it's ready for an answer.
    pub prompt: Option<&'a str>,
    /// The answer isn't secret (the prompt asked with echo on).
    pub echo: bool,
    pub input: &'a Password,
    pub checking: bool,
    /// A line to show under the field, and whether it's an error.
    pub status: Option<(&'a str, bool)>,
    pub caps_lock: bool,
    pub hover: Option<Hit>,
}

/// A rectangle in buffer pixels: (x, y, w, h).
pub type Rect = (i32, i32, i32, i32);

/// A drawn card, where it goes on the buffer, and where its buttons are (in buffer pixels).
pub struct Card {
    pub canvas: Canvas,
    pub at: (i32, i32),
    pub hits: Vec<(Hit, Rect)>,
}

pub fn scrim(theme: &Theme) -> Color {
    theme.palette.scrim
}

/// Lay out and draw the card for a `w`×`h` buffer at `scale`.
pub fn card(look: &Look, ui: &Ui, scale: f64, w: i32, h: i32) -> Card {
    let p = &look.theme.palette;
    let s = scale as f32;
    let fs = look.theme.font_size * s;
    let pad = (fs * 1.6).round();
    let cw = ((fs * 30.0) as i32).min(w - (fs * 2.0) as i32).max((fs * 12.0) as i32);
    let inner = cw as f32 - 2.0 * pad;

    // Work out the height first: the message may wrap onto several lines.
    let title_px = fs * 1.3;
    let body_px = fs;
    let small_px = fs * 0.85;
    let lines = match &look.text {
        Some(t) => wrap(t, ui.message, body_px, inner, 6),
        None => Vec::new(),
    };
    let line_h = body_px * 1.45;
    let field_h = (fs * 2.6).round();
    let button_h = (fs * 2.4).round();
    let ch = pad
        + title_px * 1.5
        + lines.len() as f32 * line_h
        + fs * 1.0
        + body_px * 1.6
        + field_h
        + fs * 2.2
        + button_h
        + pad;
    let ch = (ch.round() as i32).min(h);
    let at = ((w - cw) / 2, ((h - ch) as f32 * 0.42).round().max(0.0) as i32);

    let mut c = Canvas::new(cw, ch, scrim(&look.theme));
    let radius = look.theme.radius * s * 1.5;
    let border = (s).round().max(1.0) as i32;
    c.fill_rounded_rect(0, 0, cw, ch, radius, p.separator);
    c.fill_rounded_rect(
        border,
        border,
        cw - 2 * border,
        ch - 2 * border,
        (radius - border as f32).max(0.0),
        p.surface,
    );
    let mut hits = Vec::new();
    let left = pad;
    let clip = (0, cw);

    let mut y = pad;
    if let Some(t) = &look.text {
        y += title_px;
        t.draw(&mut c, "Authentication required", title_px, (left, y), p.text, clip);
        y += title_px * 0.5;
        for line in &lines {
            y += line_h;
            t.draw(&mut c, line, body_px, (left, y - body_px * 0.3), p.muted, clip);
        }
        y += fs * 1.0;

        // Whose password, with a way to pick someone else when there's a choice.
        let who = format!("Password for {}", ui.user_label);
        let switch = "Switch user";
        let switch_w = if ui.can_switch {
            t.width(switch, small_px) + fs * 1.2
        } else {
            0.0
        };
        let baseline = y + body_px * 1.1;
        let who = t.fit(&who, body_px, inner - switch_w - fs * 0.5);
        t.draw(&mut c, &who, body_px, (left, baseline), p.text, clip);
        if ui.can_switch {
            let bh = (fs * 1.6).round() as i32;
            let r = (
                (left + inner - switch_w) as i32,
                (baseline - body_px * 0.35 - bh as f32 / 2.0).round() as i32,
                switch_w as i32,
                bh,
            );
            let bg = if ui.hover == Some(Hit::SwitchUser) {
                p.hover
            } else {
                p.raised
            };
            c.fill_rounded_rect(r.0, r.1, r.2, r.3, look.theme.radius * s, bg);
            let tb = t.centered_baseline(r.1 as f32, r.3 as f32, small_px);
            t.draw(&mut c, switch, small_px, (r.0 as f32 + fs * 0.6, tb), p.text, clip);
            hits.push((Hit::SwitchUser, r));
        }
        y += body_px * 1.6;
    }

    // The field.
    let (fx, fy, fw, fh) = (left as i32, y.round() as i32, inner as i32, field_h as i32);
    let wrong = matches!(ui.status, Some((_, true))) && ui.input.is_empty() && !ui.checking;
    let edge = if wrong {
        p.urgent
    } else if ui.checking || !ui.input.is_empty() {
        p.accent
    } else {
        p.separator
    };
    let fr = look.theme.radius * s;
    let fb = (s * 2.0).round().max(1.0) as i32;
    c.fill_rounded_rect(fx, fy, fw, fh, fr, edge);
    c.fill_rounded_rect(
        fx + fb,
        fy + fb,
        fw - 2 * fb,
        fh - 2 * fb,
        (fr - fb as f32).max(0.0),
        p.base,
    );
    let mid_y = fy as f32 + fh as f32 / 2.0;
    if let Some(t) = &look.text {
        let tx = fx as f32 + fs * 0.8;
        let room = fw as f32 - fs * 1.6;
        let baseline = t.centered_baseline(fy as f32, fh as f32, body_px);
        if ui.checking {
            t.draw(&mut c, "Checking…", body_px, (tx, baseline), p.subtext, clip);
        } else if ui.input.is_empty() {
            let hint = match ui.prompt {
                Some(pr) => pr.trim_end_matches(':').trim(),
                None => "Starting…",
            };
            let hint = t.fit(hint, body_px, room);
            t.draw(&mut c, &hint, body_px, (tx, baseline), p.subtext, clip);
        } else if ui.echo {
            // Show the end of what was typed when it's too long.
            let typed = ui.input.as_str();
            let mut start = 0;
            while t.width(&typed[start..], body_px) > room {
                start += typed[start..].chars().next().map_or(1, char::len_utf8);
            }
            t.draw(&mut c, &typed[start..], body_px, (tx, baseline), p.text, clip);
        }
    }
    if !ui.echo && !ui.input.is_empty() && !ui.checking {
        let r = fs * 0.3;
        let step = fs * 0.95;
        let room = ((fw as f32 - fs * 1.6) / step).floor().max(1.0) as usize;
        let n = ui.input.chars().min(room);
        let d = (r * 2.0).round() as i32;
        for i in 0..n {
            let cx = fx as f32 + fs * 0.8 + r + i as f32 * step;
            c.fill_rounded_rect((cx - r).round() as i32, (mid_y - r).round() as i32, d, d, r, p.text);
        }
    }
    y += field_h;

    // The status line.
    if let Some(t) = &look.text {
        let line = match ui.status {
            Some((msg, err)) => Some((msg, if err { p.urgent } else { p.muted })),
            None if ui.caps_lock => Some(("Caps Lock is on", p.muted)),
            None => None,
        };
        if let Some((msg, color)) = line {
            let msg = t.fit(msg, small_px, inner);
            t.draw(&mut c, &msg, small_px, (left, y + fs * 1.45), color, clip);
        }
    }
    y += fs * 2.2;

    // The buttons, right-aligned, and what is being asked for on the left.
    if let Some(t) = &look.text {
        let by = y.round() as i32;
        let bw = |label: &str| (t.width(label, body_px) + fs * 2.4).round() as i32;
        let ok_label = "Authenticate";
        let ok_w = bw(ok_label);
        let cancel_w = bw("Cancel");
        let ok = ((left + inner) as i32 - ok_w, by, ok_w, button_h as i32);
        let cancel = (ok.0 - (fs * 0.6) as i32 - cancel_w, by, cancel_w, button_h as i32);
        let br = look.theme.radius * s;
        let cancel_bg = if ui.hover == Some(Hit::Cancel) {
            p.hover
        } else {
            p.raised
        };
        c.fill_rounded_rect(cancel.0, cancel.1, cancel.2, cancel.3, br, cancel_bg);
        c.fill_rounded_rect(ok.0, ok.1, ok.2, ok.3, br, p.accent);
        let tb = t.centered_baseline(by as f32, button_h, body_px);
        let center = |r: Rect, label: &str| r.0 as f32 + (r.2 as f32 - t.width(label, body_px)) / 2.0;
        t.draw(&mut c, "Cancel", body_px, (center(cancel, "Cancel"), tb), p.text, clip);
        t.draw(&mut c, ok_label, body_px, (center(ok, ok_label), tb), p.on_accent, clip);
        hits.push((Hit::Cancel, cancel));
        hits.push((Hit::Ok, ok));

        let room = cancel.0 as f32 - left - fs;
        if room > fs * 4.0 {
            let id = t.fit(ui.action_id, small_px, room);
            let ib = t.centered_baseline(by as f32, button_h, small_px);
            t.draw(&mut c, &id, small_px, (left, ib), p.subtext, clip);
        }
    }

    Card {
        canvas: c,
        at,
        hits: hits
            .into_iter()
            .map(|(h, r)| (h, (r.0 + at.0, r.1 + at.1, r.2, r.3)))
            .collect(),
    }
}

/// Break `text` into lines no wider than `max`, at most `limit` of them (the last one cut
/// short with "…" if there's more).
fn wrap(t: &Text, text: &str, px: f32, max: f32, limit: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if t.width(&candidate, px) <= max || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    if lines.len() > limit {
        lines.truncate(limit);
        if let Some(last) = lines.last_mut() {
            last.push_str(" …");
        }
    }
    lines.into_iter().map(|l| t.fit(&l, px, max)).collect()
}
