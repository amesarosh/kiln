//! Desktop notifications on screen: pop-ups in a corner of the focused monitor, and the list
//! behind the bell on the bar.
//!
//! A pop-up stays for the app's timeout (or `[notifications] timeout`), longer while the
//! pointer is on it; urgent ones stay until dismissed. Afterwards the notification waits in
//! the bell's list, unread. Click a pop-up to open it (its "default" action, or its app's
//! window), click one of its buttons for that action, right-click to dismiss it. Do not
//! disturb keeps pop-ups away (except urgent ones); they still collect in the list.

use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    output::Output,
    reexports::calloop::{
        RegistrationToken,
        timer::{TimeoutAction, Timer},
    },
    utils::{Logical, Point, Rectangle, Transform},
};

use crate::{
    config::{Color, Corner},
    dbus::{
        Command,
        notify::{CloseReason, Notification, Timeout, Urgency},
    },
    layout::Rect,
    state::{State, toplevel_ids},
    text::{Canvas, Text},
    theme::Theme,
};

const WIDTH: i32 = 360;
const PAD: i32 = 12;
const ICON: i32 = 40;
/// Space from the screen edge, and between pop-ups.
const MARGIN: i32 = 12;
const GAP: i32 = 8;
const MAX_BODY_LINES: usize = 4;
/// Most action buttons on a pop-up; more wouldn't fit.
const MAX_BUTTONS: usize = 3;
/// Longest body read; anything after can't show in four lines anyway.
const MAX_BODY_CHARS: usize = 2000;
/// Most notifications kept in the list.
const KEEP: usize = 50;

pub struct Item {
    pub n: Notification,
    /// While showing as a pop-up: when it goes (None: until dismissed).
    popup: Option<Option<Instant>>,
    pub unread: bool,
    /// Last painted image: (scale, hovered action, image).
    cache: RefCell<Option<(i32, Option<Hover>, MemoryRenderBuffer)>>,
}

/// What the pointer is on: a pop-up's body, or one of its buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hover {
    Body,
    Action(usize),
}

#[derive(Default)]
pub struct Notifications {
    /// Newest first.
    pub items: Vec<Item>,
    pub dnd: bool,
    timer: Option<RegistrationToken>,
    hover: Option<(u32, Hover)>,
}

impl Notifications {
    pub fn unread(&self) -> usize {
        self.items.iter().filter(|i| i.unread).count()
    }
}

/// How a pop-up is laid out, in logical pixels from its corner.
struct Layout {
    size: (i32, i32),
    summary: String,
    body: Vec<String>,
    /// Buttons for the actions other than "default": (rect, index into actions).
    buttons: Vec<(Rect, usize)>,
    /// Sizes of the three text lines kinds: app name, summary, body.
    sizes: (f32, f32, f32),
    line_h: f32,
}

fn layout(text: &Text, theme: &Theme, n: &Notification) -> Layout {
    let fs = theme.font_size;
    let sizes = (fs * 0.8, fs * 1.05, fs * 0.95);
    let line_h = (fs * 1.45).round();
    let text_w = (WIDTH - 2 * PAD - ICON - PAD) as f32;
    let summary = text.fit(&n.summary, sizes.1, text_w);
    let body_text: String = n.body.chars().take(MAX_BODY_CHARS).collect();
    let mut body = wrap(text, &body_text, sizes.2, text_w);
    if body.len() > MAX_BODY_LINES {
        body.truncate(MAX_BODY_LINES);
        if let Some(last) = body.last_mut() {
            *last = text.fit(&format!("{last}…"), sizes.2, text_w);
        }
    }
    let lines = 2 + body.len();
    let text_h = (lines as f32 * line_h) as i32;
    let mut h = PAD * 2 + text_h.max(ICON);
    let mut buttons = Vec::new();
    let actions: Vec<(usize, &str)> = n
        .actions
        .iter()
        .enumerate()
        .filter(|(_, (k, l))| k != "default" && !l.is_empty())
        .map(|(i, (_, l))| (i, l.as_str()))
        .take(MAX_BUTTONS)
        .collect();
    if !actions.is_empty() {
        let bh = line_h as i32 + 8;
        let bw = (WIDTH - 2 * PAD - GAP * (actions.len() as i32 - 1)) / actions.len() as i32;
        for (k, (i, _)) in actions.iter().enumerate() {
            buttons.push((Rect::new(PAD + k as i32 * (bw + GAP), h - PAD + 4, bw, bh), *i));
        }
        h += bh + 4;
    }
    Layout {
        size: (WIDTH, h),
        summary,
        body,
        buttons,
        sizes,
        line_h,
    }
}

/// Break text into lines no wider than `max`, at spaces where possible.
fn wrap(text: &Text, s: &str, px: f32, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if text.width(&candidate, px) <= max {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // A word too long for a line is split wherever it has to be.
            let mut chunk = String::new();
            for c in word.chars() {
                chunk.push(c);
                if text.width(&chunk, px) > max && chunk.chars().count() > 1 {
                    let last = chunk.pop().unwrap();
                    lines.push(std::mem::take(&mut chunk));
                    chunk.push(last);
                }
            }
            line = chunk;
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

fn to_u8(c: Color) -> [u8; 4] {
    // Palette colors are straight (not premultiplied) 0..1.
    c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

impl State {
    /// A notification arrived from an app.
    pub fn show_notification(&mut self, n: Notification) {
        tracing::debug!("notification {} from {:?}: {:?}", n.id, n.app_name, n.summary);
        self.load_ui_text();
        let popup = (!self.notifications.dnd || n.urgency == Urgency::Critical).then(|| self.expiry(&n));
        let item = Item {
            n,
            popup,
            unread: true,
            cache: RefCell::new(None),
        };
        // An update to one already listed takes its place.
        match self.notifications.items.iter().position(|i| i.n.id == item.n.id) {
            Some(i) => self.notifications.items[i] = item,
            None => self.notifications.items.insert(0, item),
        }
        while self.notifications.items.len() > KEEP {
            if let Some(old) = self.notifications.items.pop() {
                self.bus_command(Command::NotificationClosed {
                    id: old.n.id,
                    reason: CloseReason::Expired,
                });
            }
        }
        self.arm_notification_timer();
        self.needs_redraw = true;
    }

    /// When a pop-up for `n` goes away on its own.
    fn expiry(&self, n: &Notification) -> Option<Instant> {
        let default = Duration::from_secs(self.config.notifications.timeout.max(1));
        match (n.urgency, n.timeout) {
            (Urgency::Critical, _) | (_, Timeout::Never) => None,
            (_, Timeout::Millis(ms)) => Some(Instant::now() + Duration::from_millis(ms as u64)),
            (_, Timeout::Default) => Some(Instant::now() + default),
        }
    }

    /// The app took it back.
    pub fn withdraw_notification(&mut self, id: u32) {
        if self.remove_notification(id) {
            self.bus_command(Command::NotificationClosed {
                id,
                reason: CloseReason::Closed,
            });
        }
    }

    fn remove_notification(&mut self, id: u32) -> bool {
        let before = self.notifications.items.len();
        self.notifications.items.retain(|i| i.n.id != id);
        self.needs_redraw = true;
        before != self.notifications.items.len()
    }

    /// Dismissed by the user: gone from the screen and the list.
    pub fn dismiss_notification(&mut self, id: u32) {
        if self.remove_notification(id) {
            self.bus_command(Command::NotificationClosed {
                id,
                reason: CloseReason::Dismissed,
            });
        }
    }

    /// Open a notification: its default action, else its app's window. Then it's done.
    pub fn activate_notification(&mut self, id: u32) {
        let Some(item) = self.notifications.items.iter().find(|i| i.n.id == id) else {
            return;
        };
        if item.n.actions.iter().any(|(k, _)| k == "default") {
            self.bus_command(Command::ActionInvoked {
                id,
                key: "default".into(),
            });
        } else {
            let names = [item.n.app_icon.to_lowercase(), item.n.app_name.to_lowercase()];
            let window = self
                .monitors
                .iter()
                .flat_map(|m| &m.workspaces)
                .flat_map(|ws| &ws.entries)
                .map(|e| e.window.clone())
                .find(|w| {
                    let app = toplevel_ids(w).0.unwrap_or_default().to_lowercase();
                    !app.is_empty()
                        && names
                            .iter()
                            .any(|n| !n.is_empty() && (app == *n || app.ends_with(&format!(".{n}"))))
                });
            if let Some(w) = window {
                self.activate_window(&w);
            }
        }
        self.dismiss_notification(id);
    }

    fn invoke_notification_action(&mut self, id: u32, action: usize) {
        let Some(key) = self
            .notifications
            .items
            .iter()
            .find(|i| i.n.id == id)
            .and_then(|i| i.n.actions.get(action))
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        self.bus_command(Command::ActionInvoked { id, key });
        self.dismiss_notification(id);
    }

    pub fn clear_notifications(&mut self) {
        let ids: Vec<u32> = self.notifications.items.iter().map(|i| i.n.id).collect();
        for id in ids {
            self.dismiss_notification(id);
        }
    }

    pub fn toggle_dnd(&mut self) {
        let n = &mut self.notifications;
        n.dnd = !n.dnd;
        tracing::info!("do not disturb {}", if n.dnd { "on" } else { "off" });
        if n.dnd {
            for i in &mut n.items {
                if i.n.urgency != Urgency::Critical {
                    i.popup = None;
                }
            }
        }
        self.needs_redraw = true;
    }

    /// The bell's list was opened: everything in it has been seen.
    pub fn mark_notifications_read(&mut self) {
        for i in &mut self.notifications.items {
            i.unread = false;
        }
        self.needs_redraw = true;
    }

    /// Wake when the next pop-up is due to go.
    fn arm_notification_timer(&mut self) {
        if let Some(t) = self.notifications.timer.take() {
            self.loop_handle.remove(t);
        }
        let next = self.notifications.items.iter().filter_map(|i| i.popup.flatten()).min();
        let Some(next) = next else { return };
        let timer = Timer::from_deadline(next);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            state.notifications.timer = None;
            state.expire_notifications();
            TimeoutAction::Drop
        }) {
            Ok(t) => self.notifications.timer = Some(t),
            Err(e) => tracing::warn!("notification timer: {e}"),
        }
    }

    fn expire_notifications(&mut self) {
        let now = Instant::now();
        let hovered = self.notifications.hover.map(|(id, _)| id);
        let mut gone = Vec::new();
        for i in &mut self.notifications.items {
            if let Some(Some(until)) = i.popup
                && until <= now
            {
                // The pointer on it holds it a little longer.
                if Some(i.n.id) == hovered {
                    i.popup = Some(Some(now + Duration::from_secs(2)));
                    continue;
                }
                i.popup = None;
                if i.n.transient {
                    gone.push(i.n.id);
                }
            }
        }
        for id in gone {
            self.remove_notification(id);
            self.bus_command(Command::NotificationClosed {
                id,
                reason: CloseReason::Expired,
            });
        }
        self.needs_redraw = true;
        self.arm_notification_timer();
    }

    // ---------- on screen ----------

    /// The pop-ups showing, with where they go, on the focused monitor. Empty while locked.
    fn popup_rects(&self) -> Vec<(u32, Rectangle<i32, Logical>)> {
        if self.is_locked() {
            return Vec::new();
        }
        let Some((_, text)) = &self.ui_text else {
            return Vec::new();
        };
        let Some(area) = self.work_area(self.active_mon) else {
            return Vec::new();
        };
        let corner = self.config.notifications.position;
        let right = matches!(corner, Corner::TopRight | Corner::BottomRight);
        let top = matches!(corner, Corner::TopRight | Corner::TopLeft);
        let x = if right {
            area.loc.x + area.size.w - WIDTH - MARGIN
        } else {
            area.loc.x + MARGIN
        };
        let mut y = if top {
            area.loc.y + MARGIN
        } else {
            area.loc.y + area.size.h - MARGIN
        };
        let mut out = Vec::new();
        let showing = self.notifications.items.iter().filter(|i| i.popup.is_some());
        for item in showing.take(self.config.notifications.max_visible) {
            let h = layout(text, &self.config.theme, &item.n).size.1;
            let rect = if top {
                let r = Rectangle::new((x, y).into(), (WIDTH, h).into());
                y += h + GAP;
                r
            } else {
                y -= h;
                let r = Rectangle::new((x, y).into(), (WIDTH, h).into());
                y -= GAP;
                r
            };
            if !area.contains_rect(rect) && !out.is_empty() {
                break;
            }
            out.push((item.n.id, rect));
        }
        out
    }

    pub fn over_notification(&self, pos: Point<f64, Logical>) -> bool {
        self.popup_rects().iter().any(|(_, r)| r.to_f64().contains(pos))
    }

    fn notification_hit(&self, pos: Point<f64, Logical>) -> Option<(u32, Hover)> {
        let (_, text) = self.ui_text.as_ref()?;
        let (id, rect) = self.popup_rects().into_iter().find(|(_, r)| r.to_f64().contains(pos))?;
        let item = self.notifications.items.iter().find(|i| i.n.id == id)?;
        let l = layout(text, &self.config.theme, &item.n);
        let (x, y) = ((pos.x - rect.loc.x as f64) as i32, (pos.y - rect.loc.y as f64) as i32);
        let hover = l
            .buttons
            .iter()
            .find(|(r, _)| r.contains(x as f64, y as f64))
            .map_or(Hover::Body, |(_, i)| Hover::Action(*i));
        Some((id, hover))
    }

    pub fn notification_motion(&mut self, pos: Point<f64, Logical>) {
        let hover = self.notification_hit(pos);
        if hover != self.notifications.hover {
            self.notifications.hover = hover;
            self.needs_redraw = true;
        }
    }

    /// A press on a pop-up. Returns true if it was on one.
    pub fn notification_press(&mut self, pos: Point<f64, Logical>, button: u32) -> bool {
        let Some((id, hover)) = self.notification_hit(pos) else {
            return false;
        };
        match (button, hover) {
            (crate::bar::BTN_LEFT, Hover::Action(i)) => self.invoke_notification_action(id, i),
            (crate::bar::BTN_LEFT, Hover::Body) => self.activate_notification(id),
            _ => self.dismiss_notification(id),
        }
        true
    }

    /// Images of the pop-ups for `output`, with where they go.
    pub fn notification_buffers(&self, output: &Output, scale: i32) -> Vec<(Point<i32, Logical>, MemoryRenderBuffer)> {
        if self.output().as_ref() != Some(output) {
            return Vec::new();
        }
        let Some((_, text)) = &self.ui_text else {
            return Vec::new();
        };
        let scale = scale.max(1);
        let mut out = Vec::new();
        for (id, rect) in self.popup_rects() {
            let Some(item) = self.notifications.items.iter().find(|i| i.n.id == id) else {
                continue;
            };
            let hover = self.notifications.hover.filter(|(h, _)| *h == id).map(|(_, h)| h);
            let mut cache = item.cache.borrow_mut();
            if let Some((s, h, b)) = &*cache
                && *s == scale
                && *h == hover
            {
                out.push((rect.loc, b.clone()));
                continue;
            }
            let canvas = self.paint_notification(text, item, hover, scale);
            let buffer = MemoryRenderBuffer::from_slice(
                &canvas.to_premultiplied_rgba(),
                Fourcc::Abgr8888,
                (canvas.w, canvas.h),
                scale,
                Transform::Normal,
                None,
            );
            *cache = Some((scale, hover, buffer.clone()));
            out.push((rect.loc, buffer));
        }
        out
    }

    fn paint_notification(&self, text: &Text, item: &Item, hover: Option<Hover>, s: i32) -> Canvas {
        let theme = &self.config.theme;
        let p = &theme.palette;
        let n = &item.n;
        let l = layout(text, theme, n);
        let (w, h) = (l.size.0 * s, l.size.1 * s);
        let radius = theme.radius * s as f32;
        let mut canvas = Canvas::new(w, h, [0.0; 4]);
        let edge = if n.urgency == Urgency::Critical {
            p.urgent
        } else {
            p.separator
        };
        canvas.fill_rounded_rect(0, 0, w, h, radius + s as f32, edge);
        let body = if hover == Some(Hover::Body) {
            p.raised
        } else {
            p.surface
        };
        canvas.fill_rounded_rect(s, s, w - 2 * s, h - 2 * s, radius, body);

        // The picture: the notification's own, else the app's icon.
        let px = (ICON * s) as u32;
        let picture = n
            .image
            .as_ref()
            .map(|img| std::rc::Rc::new(kiln_icons::scaled(&img.rgba, img.width, img.height, px)))
            .or_else(|| {
                let path = n.image_path.as_deref()?.trim_start_matches("file://");
                kiln_icons::load_file(std::path::Path::new(path), px).map(std::rc::Rc::new)
            })
            .or_else(|| self.bar.any_icon(&n.app_icon, px, to_u8(p.text)))
            .or_else(|| self.bar.icon(&n.app_name, px))
            .or_else(|| self.bar.symbolic("dialog-information-symbolic", px, to_u8(p.muted)));
        if let Some(icon) = picture {
            canvas.draw_image(PAD * s, PAD * s, icon.size, &icon.rgba);
        }

        let x = (PAD + ICON + PAD) * s;
        let right = w - PAD * s;
        let mut y = PAD as f32 * s as f32;
        let lh = l.line_h * s as f32;
        let mut line = |canvas: &mut Canvas, t: &str, size: f32, color: Color| {
            let baseline = text.centered_baseline(y, lh, size * s as f32);
            text.draw(canvas, t, size * s as f32, (x as f32, baseline), color, (x, right));
            y += lh;
        };
        line(&mut canvas, &n.app_name, l.sizes.0, p.subtext);
        line(&mut canvas, &l.summary, l.sizes.1, p.text);
        for b in &l.body {
            line(&mut canvas, b, l.sizes.2, p.muted);
        }

        for (r, i) in &l.buttons {
            let r = Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
            let fill = if hover == Some(Hover::Action(*i)) {
                p.hover
            } else {
                p.raised
            };
            canvas.fill_rounded_rect(r.x, r.y, r.w, r.h, (radius - 2.0 * s as f32).max(2.0), fill);
            let label = text.fit(&n.actions[*i].1, l.sizes.2 * s as f32, (r.w - 8 * s) as f32);
            let tw = text.width(&label, l.sizes.2 * s as f32);
            let tx = r.x as f32 + (r.w as f32 - tw) / 2.0;
            let baseline = text.centered_baseline(r.y as f32, r.h as f32, l.sizes.2 * s as f32);
            text.draw(
                &mut canvas,
                &label,
                l.sizes.2 * s as f32,
                (tx, baseline),
                p.text,
                (r.x, r.x + r.w),
            );
        }
        canvas
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::notify::{Timeout, Urgency};

    fn note(summary: &str, body: &str, actions: &[(&str, &str)]) -> Notification {
        Notification {
            id: 1,
            replaces: false,
            app_name: "Test".into(),
            app_icon: String::new(),
            image: None,
            image_path: None,
            summary: summary.into(),
            body: body.into(),
            actions: actions.iter().map(|(k, l)| (k.to_string(), l.to_string())).collect(),
            urgency: Urgency::Normal,
            timeout: Timeout::Default,
            transient: false,
        }
    }

    #[test]
    fn layout_grows_with_content() {
        let Some(text) = Text::load("sans") else { return };
        let theme = crate::config::Config::default().theme;
        let short = layout(&text, &theme, &note("Hi", "", &[]));
        let long = layout(
            &text,
            &theme,
            &note("Hi", &"word ".repeat(80), &[("reply", "Reply"), ("default", "")]),
        );
        assert!(long.size.1 > short.size.1);
        assert_eq!(long.body.len(), MAX_BODY_LINES);
        // Only the non-default action gets a button.
        assert_eq!(long.buttons.len(), 1);
        for line in &long.body {
            assert!(text.width(line, long.sizes.2) <= (WIDTH - 2 * PAD - ICON - PAD) as f32 + 1.0);
        }
    }
}
