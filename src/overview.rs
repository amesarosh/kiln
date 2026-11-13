//! The overview: every workspace of every monitor at once, as a grid of live thumbnails.
//!
//! Opening it zooms the current workspace down into its card while the others fade in; closing
//! zooms the chosen card back up to fill the screen. Click a window to go to it, click a card
//! to go to that workspace, or drag a window onto another card to move it there. Keys: arrows
//! move between cards, Enter picks, 1-9 jump straight to a workspace, Escape goes back.
//!
//! Thumbnails are the windows themselves drawn smaller, so they stay live. Cards (their
//! background and label) are painted on the CPU like the bar, and cached.

use std::{
    cell::RefCell,
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportAll, ImportMem, Renderer, Texture,
            element::{
                Id, Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                surface::WaylandSurfaceRenderElement,
                utils::RescaleRenderElement,
            },
            utils::CommitCounter,
        },
    },
    desktop::Window,
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Transform},
};

use crate::{
    config::Color,
    layout::{LayoutKind, Rect},
    menu::MenuKey,
    render::OutputElement,
    state::State,
    text::{Canvas, Text},
};

/// Space around the grid and between cards, in logical pixels.
const PAD: f64 = 48.0;
const GAP: f64 = 28.0;
/// How far the pointer moves before a press on a window becomes a drag.
const DRAG_THRESHOLD: f64 = 6.0;

/// A window thumbnail being dragged to another workspace.
struct Drag {
    window: Window,
    /// Pointer position when pressed, and now.
    start: Point<f64, Logical>,
    pos: Point<f64, Logical>,
    /// Where on the thumbnail it was grabbed, as a fraction of its size.
    grab: (f64, f64),
    moved: bool,
}

/// What the pointer is over.
#[derive(Debug, Clone, PartialEq)]
struct Hover {
    mon: usize,
    ws: usize,
    window: Option<Window>,
}

pub struct Overview {
    start: Instant,
    duration: Duration,
    /// Closing, zooming into `(monitor, workspace)`.
    closing: Option<(usize, usize)>,
    hover: Option<Hover>,
    /// The card picked with the keyboard, on the focused monitor.
    selected: usize,
    drag: Option<Drag>,
    /// Painted cards, by (output name, workspace).
    cards: RefCell<HashMap<(String, usize), (CardKey, MemoryRenderBuffer)>>,
    backdrop_id: Id,
    /// Bumped when the backdrop's opacity changes, so it's redrawn.
    backdrop_commit: std::cell::Cell<(CommitCounter, u32)>,
    hover_ids: [Id; 4],
}

/// Everything a card's picture depends on.
#[derive(Debug, Clone, PartialEq)]
struct CardKey {
    size: (i32, i32),
    label_h: i32,
    scale: i32,
    label: String,
    icon: String,
    active: bool,
    hovered: bool,
    selected: bool,
}

/// Where one workspace's card goes, and its windows inside it, in global logical coordinates.
pub struct Card {
    pub ws: usize,
    pub rect: Rectangle<f64, Logical>,
    /// The card's scale: card size over the workspace's size.
    pub scale: f64,
    /// Bottom to top, with each window's full-size rectangle.
    pub windows: Vec<(Window, Rectangle<f64, Logical>)>,
    /// The area the workspace's windows live in, at full size.
    pub area: Rectangle<f64, Logical>,
}

impl Card {
    /// Where a full-size rectangle lands on the card.
    fn map(&self, r: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        let x = self.rect.loc.x + (r.loc.x - self.area.loc.x) * self.scale;
        let y = self.rect.loc.y + (r.loc.y - self.area.loc.y) * self.scale;
        Rectangle::new((x, y).into(), (r.size.w * self.scale, r.size.h * self.scale).into())
    }
}

fn ease(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

impl Overview {
    /// 0 when fully closed, 1 when fully open.
    fn progress(&self, now: Instant) -> f64 {
        let t = if self.duration.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(self.start).as_secs_f64() / self.duration.as_secs_f64()).min(1.0)
        };
        match self.closing {
            Some(_) => 1.0 - ease(t),
            None => ease(t),
        }
    }

    fn finished_closing(&self, now: Instant) -> bool {
        self.closing.is_some() && now.saturating_duration_since(self.start) >= self.duration
    }
}

impl State {
    pub fn overview_open(&self) -> bool {
        self.overview.is_some()
    }

    /// Open and not on its way out: apps don't get the keyboard.
    pub fn overview_holds_keyboard(&self) -> bool {
        self.overview.as_ref().is_some_and(|o| o.closing.is_none())
    }

    /// The `overview` action: open it, or go back to where you were.
    pub fn toggle_overview(&mut self) {
        match &self.overview {
            Some(o) if o.closing.is_none() => {
                let target = (
                    self.active_mon,
                    self.monitors.get(self.active_mon).map_or(0, |m| m.active_ws),
                );
                self.close_overview(target);
            }
            Some(_) => {}
            None => self.open_overview(),
        }
    }

    fn open_overview(&mut self) {
        if self.monitors.is_empty() || self.is_locked() {
            return;
        }
        self.close_menu();
        self.switcher_cancel();
        self.load_ui_text();
        let duration = self.config.animations.duration();
        self.overview = Some(Overview {
            start: Instant::now(),
            duration,
            closing: None,
            hover: None,
            selected: self.monitors[self.active_mon].active_ws,
            drag: None,
            cards: RefCell::new(HashMap::new()),
            backdrop_id: Id::new(),
            backdrop_commit: Default::default(),
            hover_ids: [Id::new(), Id::new(), Id::new(), Id::new()],
        });
        // Apps don't get keys while it's open; it takes them.
        self.keyboard()
            .set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
        self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        self.keep_animating(duration);
        let pos = self.pointer().current_location();
        self.overview_motion(pos);
        self.needs_redraw = true;
    }

    /// Zoom into a workspace and close. It becomes the one shown on its monitor, and that
    /// monitor gets focus.
    fn close_overview(&mut self, (mon, ws): (usize, usize)) {
        let Some(o) = &mut self.overview else { return };
        if o.closing.is_some() {
            return;
        }
        // Close from wherever the opening animation got to.
        let p = o.progress(Instant::now());
        o.start = Instant::now() - o.duration.mul_f64(1.0 - p);
        o.closing = Some((mon, ws));
        o.drag = None;
        let duration = o.duration;
        if let Some(m) = self.monitors.get_mut(mon) {
            m.active_ws = ws.min(m.workspaces.len() - 1);
            self.active_mon = mon;
        }
        self.focused_layer = None;
        self.arrange();
        self.keep_animating(duration);
        if duration.is_zero() {
            self.overview_tick();
        }
    }

    /// Called every frame while it animates: forget it once it has closed.
    pub fn overview_tick(&mut self) {
        if self
            .overview
            .as_ref()
            .is_some_and(|o| o.finished_closing(Instant::now()))
        {
            self.overview = None;
            self.refocus();
            self.refresh_pointer_focus();
            self.needs_redraw = true;
        }
    }

    /// Close without animating, e.g. when the monitors change or the screen locks.
    pub fn overview_cancel(&mut self) {
        if self.overview.take().is_some() {
            self.refocus();
            self.needs_redraw = true;
        }
    }

    // ---------- geometry ----------

    /// The cards of monitor `mon`, as they sit when the overview is fully open.
    pub fn overview_cards(&self, mon: usize) -> Vec<Card> {
        let Some(m) = self.monitors.get(mon) else {
            return Vec::new();
        };
        let Some(geo) = self.space.output_geometry(&m.output) else {
            return Vec::new();
        };
        let Some(area) = self.work_area(mon) else {
            return Vec::new();
        };
        let n = m.workspaces.len().max(1);
        let cols = (n as f64).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);
        let label_h = self.overview_label_h();
        let geo = geo.to_f64();
        let cell_w = (geo.size.w - 2.0 * PAD - GAP * (cols - 1) as f64) / cols as f64;
        let cell_h = (geo.size.h - 2.0 * PAD - GAP * (rows - 1) as f64) / rows as f64 - label_h;
        let aspect = area.size.w as f64 / area.size.h.max(1) as f64;
        let (w, h) = if cell_w / cell_h.max(1.0) > aspect {
            (cell_h * aspect, cell_h)
        } else {
            (cell_w, cell_w / aspect)
        };
        let (w, h) = (w.max(1.0), h.max(1.0));
        let scale = w / area.size.w.max(1) as f64;
        // Center the whole grid.
        let grid_w = cols as f64 * w + (cols - 1) as f64 * GAP;
        let grid_h = rows as f64 * (h + label_h) + (rows - 1) as f64 * GAP;
        let x0 = geo.loc.x + (geo.size.w - grid_w) / 2.0;
        let y0 = geo.loc.y + (geo.size.h - grid_h) / 2.0;
        (0..m.workspaces.len())
            .map(|i| {
                let (c, r) = (i % cols, i / cols);
                // The last row is centered when it isn't full.
                let in_row = if r == rows - 1 { n - r * cols } else { cols };
                let row_x = x0 + (grid_w - (in_row as f64 * w + (in_row - 1) as f64 * GAP)) / 2.0;
                let rect = Rectangle::new(
                    (row_x + c as f64 * (w + GAP), y0 + r as f64 * (h + label_h + GAP)).into(),
                    (w, h).into(),
                );
                Card {
                    ws: i,
                    rect,
                    scale,
                    windows: self.workspace_window_rects(mon, i),
                    area: area.to_f64(),
                }
            })
            .collect()
    }

    fn overview_label_h(&self) -> f64 {
        (self.config.theme.font_size as f64 * 2.0).round()
    }

    /// Where each window of a workspace is (or would be, if it isn't shown), bottom to top.
    fn workspace_window_rects(&self, mon: usize, idx: usize) -> Vec<(Window, Rectangle<f64, Logical>)> {
        let m = &self.monitors[mon];
        let ws = &m.workspaces[idx];
        let Some(geo) = self.space.output_geometry(&m.output) else {
            return Vec::new();
        };
        if let Some(fs) = &ws.fullscreen {
            return vec![(fs.clone(), geo.to_f64())];
        }
        let Some(area) = self.work_area(mon) else {
            return Vec::new();
        };
        let border = self.config.decoration.border_width;
        let mut params = crate::layout::params(&self.config);
        params.master_ratio = ws.master_ratio;
        params.master_count = ws.master_count;
        let rects = crate::layout::arrange(
            ws.layout,
            &ws.tiled_weights(),
            Rect::new(area.loc.x, area.loc.y, area.size.w, area.size.h),
            &params,
        );
        let mut out: Vec<(Window, Rectangle<f64, Logical>)> = Vec::new();
        let tiled: Vec<&Window> = ws.tiled().collect();
        for (w, r) in tiled.iter().zip(rects) {
            // In monocle only the focused window shows.
            if ws.layout == LayoutKind::Monocle && ws.focus.as_ref().is_some_and(|f| f != *w) && tiled.len() > 1 {
                continue;
            }
            let r = r.inset(border);
            out.push((
                (*w).clone(),
                Rectangle::new((r.x as f64, r.y as f64).into(), (r.w as f64, r.h as f64).into()),
            ));
        }
        for (w, r) in ws.floating() {
            if r.w > 0 {
                out.push((
                    w.clone(),
                    Rectangle::new((r.x as f64, r.y as f64).into(), (r.w as f64, r.h as f64).into()),
                ));
            }
        }
        // The focused floating window is on top.
        if let Some(f) = ws.focus.as_ref().filter(|f| ws.is_floating(f))
            && let Some(i) = out.iter().position(|(w, _)| w == f)
        {
            let e = out.remove(i);
            out.push(e);
        }
        out
    }

    /// The card, and window thumbnail if any, under a point.
    fn overview_hit(&self, pos: Point<f64, Logical>) -> Option<Hover> {
        let mon = self.monitor_at(pos)?;
        let card = self.overview_cards(mon).into_iter().find(|c| c.rect.contains(pos))?;
        let window = card
            .windows
            .iter()
            .rev()
            .find(|(_, r)| card.map(*r).contains(pos))
            .map(|(w, _)| w.clone());
        Some(Hover {
            mon,
            ws: card.ws,
            window,
        })
    }

    // ---------- input ----------

    pub fn overview_motion(&mut self, pos: Point<f64, Logical>) {
        let Some(o) = &self.overview else { return };
        if o.closing.is_some() {
            return;
        }
        let hover = self.overview_hit(pos);
        let Some(o) = &mut self.overview else { return };
        if let Some(d) = &mut o.drag {
            d.pos = pos;
            if !d.moved && (pos - d.start).to_f64().x.hypot((pos - d.start).y) >= DRAG_THRESHOLD {
                d.moved = true;
            }
            self.needs_redraw = true;
        }
        if hover != o.hover {
            o.hover = hover;
            self.needs_redraw = true;
        }
    }

    pub fn overview_press(&mut self, pos: Point<f64, Logical>, button: u32) {
        if self.overview.as_ref().is_none_or(|o| o.closing.is_some()) {
            return;
        }
        let hit = self.overview_hit(pos);
        if button != crate::bar::BTN_LEFT {
            // Any other button on empty space goes back.
            if hit.is_none() {
                self.toggle_overview();
            }
            return;
        }
        match hit {
            Some(Hover {
                mon,
                ws,
                window: Some(w),
            }) => {
                let grab = self
                    .overview_cards(mon)
                    .into_iter()
                    .find(|c| c.ws == ws)
                    .and_then(|c| {
                        let r = c.map(c.windows.iter().find(|(x, _)| *x == w)?.1);
                        Some((
                            (pos.x - r.loc.x) / r.size.w.max(1.0),
                            (pos.y - r.loc.y) / r.size.h.max(1.0),
                        ))
                    })
                    .unwrap_or((0.5, 0.5));
                if let Some(o) = &mut self.overview {
                    o.drag = Some(Drag {
                        window: w,
                        start: pos,
                        pos,
                        grab,
                        moved: false,
                    });
                }
            }
            Some(Hover { mon, ws, window: None }) => self.close_overview((mon, ws)),
            None => {}
        }
    }

    pub fn overview_release(&mut self, pos: Point<f64, Logical>) {
        let Some(drag) = self.overview.as_mut().and_then(|o| o.drag.take()) else {
            return;
        };
        let target = self.overview_hit(pos);
        if !drag.moved {
            // A click: go to the window.
            if let Some(mon) = self.monitor_of(&drag.window)
                && let Some(ws) = self.monitors[mon]
                    .workspaces
                    .iter()
                    .position(|w| w.contains(&drag.window))
            {
                self.monitors[mon].workspaces[ws].focus = Some(drag.window.clone());
                self.close_overview((mon, ws));
            }
            return;
        }
        if let Some(t) = target {
            self.move_window_to(&drag.window, t.mon, t.ws);
        }
        self.needs_redraw = true;
    }

    /// Put a window on another workspace (of any monitor), keeping it floating if it was.
    pub fn move_window_to(&mut self, w: &Window, mon: usize, ws: usize) {
        let Some(from) = self.monitor_of(w) else { return };
        let Some(from_ws) = self.monitors[from].workspaces.iter().position(|x| x.contains(w)) else {
            return;
        };
        if (from, from_ws) == (mon, ws) || ws >= self.monitors.get(mon).map_or(0, |m| m.workspaces.len()) {
            return;
        }
        let floating = self.monitors[from].workspaces[from_ws]
            .entry(w)
            .and_then(|e| e.floating);
        self.monitors[from].workspaces[from_ws].remove(w);
        let (a, b) = (
            self.monitors[from].output.current_location(),
            self.monitors[mon].output.current_location(),
        );
        let floating = floating.map(|mut r| {
            if r.w > 0 {
                r.x += b.x - a.x;
                r.y += b.y - a.y;
            }
            r
        });
        let at_master = self.config.general.new_window == crate::config::NewWindow::Master;
        let target = &mut self.monitors[mon].workspaces[ws];
        let focus_before = target.focus.clone();
        target.add(w.clone(), at_master, floating);
        target.fullscreen = None;
        // Moving a window doesn't change what's focused where it lands.
        if focus_before.is_some() {
            target.focus = focus_before;
        }
        w.set_activated(false);
        self.arrange();
    }

    pub fn overview_key(&mut self, key: MenuKey) {
        if self.overview.as_ref().is_none_or(|o| o.closing.is_some()) {
            return;
        }
        let mon = self.active_mon;
        let n = self.monitors.get(mon).map_or(0, |m| m.workspaces.len());
        if n == 0 {
            return;
        }
        let cols = (n as f64).sqrt().ceil() as usize;
        let Some(o) = &mut self.overview else { return };
        let sel = o.selected.min(n - 1);
        match key {
            MenuKey::Close => return self.toggle_overview(),
            MenuKey::Pick => return self.close_overview((mon, sel)),
            MenuKey::Workspace(i) if i < n => return self.close_overview((mon, i)),
            MenuKey::Workspace(_) => return,
            MenuKey::Left => o.selected = sel.saturating_sub(1),
            MenuKey::Right => o.selected = (sel + 1).min(n - 1),
            MenuKey::Up => o.selected = sel.saturating_sub(cols),
            MenuKey::Down => o.selected = (sel + cols).min(n - 1),
        }
        self.needs_redraw = true;
    }

    /// Windows whose thumbnails are showing on `output`, for frame callbacks.
    pub fn overview_windows_on(&self, output: &Output) -> Vec<Window> {
        if self.overview.is_none() {
            return Vec::new();
        }
        self.monitor_index(output)
            .map(|mon| {
                self.monitors[mon]
                    .workspaces
                    .iter()
                    .flat_map(|ws| ws.entries.iter().map(|e| e.window.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---------- drawing ----------

    /// A card's picture: its background, a ring if it's the current, hovered or selected one,
    /// and its label underneath.
    fn card_buffer(&self, output: &Output, ws: usize, key: CardKey) -> Option<MemoryRenderBuffer> {
        let o = self.overview.as_ref()?;
        let cache_key = (output.name(), ws);
        if let Some((k, b)) = o.cards.borrow().get(&cache_key)
            && *k == key
        {
            return Some(b.clone());
        }
        let (_, text) = self.ui_text.as_ref()?;
        let canvas = paint_card(&key, text, &self.config.theme, self, ws);
        let buffer = MemoryRenderBuffer::from_slice(
            &canvas.to_premultiplied_rgba(),
            Fourcc::Abgr8888,
            (canvas.w, canvas.h),
            key.scale,
            Transform::Normal,
            None,
        );
        o.cards.borrow_mut().insert(cache_key, (key, buffer.clone()));
        Some(buffer)
    }
}

/// Paint one card at `key.scale`.
fn paint_card(key: &CardKey, text: &Text, theme: &crate::theme::Theme, state: &State, ws: usize) -> Canvas {
    let p = &theme.palette;
    let s = key.scale;
    let (w, h) = (key.size.0 * s, key.size.1 * s);
    let lh = key.label_h * s;
    let mut canvas = Canvas::new(w, h + lh, [0.0; 4]);
    let radius = theme.radius * s as f32;
    let ring = if key.active {
        Some(p.accent)
    } else if key.hovered || key.selected {
        Some(p.hover)
    } else {
        None
    };
    let ring_w = 3 * s;
    match ring {
        Some(c) => {
            canvas.fill_rounded_rect(0, 0, w, h, radius + ring_w as f32, c);
            canvas.fill_rounded_rect(ring_w, ring_w, w - 2 * ring_w, h - 2 * ring_w, radius, p.base);
        }
        None => canvas.fill_rounded_rect(0, 0, w, h, radius, p.base),
    }
    // The label: icon and name if set, else the number.
    let px = theme.font_size * s as f32;
    let name = if key.label.is_empty() {
        (ws + 1).to_string()
    } else {
        key.label.clone()
    };
    let color = if key.active || key.selected { p.text } else { p.muted };
    let icon_px = (px * 1.2) as i32;
    let tw = text.width(&name, px);
    let total = tw
        + if key.icon.is_empty() {
            0.0
        } else {
            (icon_px + 6 * s) as f32
        };
    let mut x = (w as f32 - total) / 2.0;
    if !key.icon.is_empty()
        && let Some(icon) = state.bar.icon(&key.icon, icon_px as u32)
    {
        canvas.draw_image(x as i32, h + (lh - icon_px) / 2, icon.size, &icon.rgba);
        x += (icon_px + 6 * s) as f32;
    }
    let baseline = text.centered_baseline(h as f32, lh as f32, px);
    text.draw(&mut canvas, &name, px, (x, baseline), color, (0, w));
    canvas
}

fn faded(c: Color, a: f32) -> Color {
    [c[0] * a, c[1] * a, c[2] * a, c[3] * a]
}

/// Draw the overview for `output`, front to back, if it's open. Returns false when it isn't,
/// so the normal scene is drawn instead.
pub fn push_elements<R>(state: &State, renderer: &mut R, output: &Output, out: &mut Vec<OutputElement<R>>) -> bool
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let Some(o) = &state.overview else { return false };
    let Some(mon) = state.monitor_index(output) else {
        return false;
    };
    let Some(output_geo) = state.space.output_geometry(output) else {
        return false;
    };
    let now = Instant::now();
    let p = o.progress(now);
    let out_scale = output.current_scale().fractional_scale();
    let int_scale = output.current_scale().integer_scale().max(1);
    let origin = output_geo.loc.to_f64();
    let to_phys =
        |pt: Point<f64, Logical>| -> Point<i32, Physical> { (pt - origin).to_physical(out_scale).to_i32_round() };
    let cards = state.overview_cards(mon);
    let m = &state.monitors[mon];
    // The card that zooms: where it's closing to, or the workspace shown when it opened.
    let zoom_ws = match o.closing {
        Some((cm, cw)) if cm == mon => cw,
        _ => m.active_ws,
    };

    // A dragged thumbnail, on top of everything, under the pointer. It keeps the size it has
    // on its own monitor's cards.
    if let Some(d) = o.drag.as_ref().filter(|d| d.moved && d.window.alive())
        && output_geo.to_f64().contains(d.pos)
        && let Some((scale, r)) = (0..state.monitors.len()).find_map(|mi| {
            state.overview_cards(mi).into_iter().find_map(|c| {
                c.windows
                    .iter()
                    .find(|(w, _)| *w == d.window)
                    .map(|(_, r)| (c.scale, *r))
            })
        })
    {
        let size = (r.size.w * scale, r.size.h * scale);
        let loc = d.pos - Point::from((d.grab.0 * size.0, d.grab.1 * size.1));
        push_window(out, renderer, &d.window, loc, scale, &to_phys, out_scale, 0.85);
    }

    // Hover ring around the window under the pointer.
    let hover = o
        .hover
        .as_ref()
        .filter(|h| h.mon == mon && o.closing.is_none() && o.drag.as_ref().is_none_or(|d| !d.moved));
    if let Some(h) = hover
        && let Some(w) = &h.window
        && let Some(card) = cards.iter().find(|c| c.ws == h.ws)
        && let Some((_, r)) = card.windows.iter().find(|(x, _)| x == w)
    {
        let t = card.map(*r);
        let b = 2.0;
        let sides = [
            Rectangle::new((t.loc.x - b, t.loc.y - b).into(), (t.size.w + 2.0 * b, b).into()),
            Rectangle::new((t.loc.x - b, t.loc.y + t.size.h).into(), (t.size.w + 2.0 * b, b).into()),
            Rectangle::new((t.loc.x - b, t.loc.y).into(), (b, t.size.h).into()),
            Rectangle::new((t.loc.x + t.size.w, t.loc.y).into(), (b, t.size.h).into()),
        ];
        for (id, side) in o.hover_ids.iter().zip(sides) {
            let geo = Rectangle::new(to_phys(side.loc), side.size.to_physical(out_scale).to_i32_round());
            out.push(OutputElement::Solid(SolidColorRenderElement::new(
                id.clone(),
                geo,
                CommitCounter::default(),
                state.config.theme.palette.accent,
                Kind::Unspecified,
            )));
        }
    }

    // Thumbnails, then the cards behind them.
    let mut backs = Vec::new();
    let full = |c: &Card| Rectangle::<f64, Logical>::new(c.area.loc, c.area.size);
    for card in &cards {
        let zooming = card.ws == zoom_ws;
        // The zooming card grows from its spot to the whole work area as p goes to 0.
        let (rect, scale, alpha) = if zooming {
            let f = full(card);
            (
                Rectangle::new(
                    (lerp(f.loc.x, card.rect.loc.x, p), lerp(f.loc.y, card.rect.loc.y, p)).into(),
                    (lerp(f.size.w, card.rect.size.w, p), lerp(f.size.h, card.rect.size.h, p)).into(),
                ),
                lerp(1.0, card.scale, p),
                1.0,
            )
        } else {
            (card.rect, card.scale, p as f32)
        };
        let place = |r: Rectangle<f64, Logical>| -> Point<f64, Logical> {
            (
                rect.loc.x + (r.loc.x - card.area.loc.x) * scale,
                rect.loc.y + (r.loc.y - card.area.loc.y) * scale,
            )
                .into()
        };
        for (w, r) in card.windows.iter().rev() {
            if o.drag.as_ref().is_some_and(|d| d.moved && d.window == *w) {
                continue;
            }
            push_window(out, renderer, w, place(*r), scale, &to_phys, out_scale, alpha);
        }
        let hovered = hover.is_some_and(|h| h.ws == card.ws && h.window.is_none())
            || o.drag.as_ref().is_some_and(|d| d.moved)
                && o.hover.as_ref().is_some_and(|h| h.mon == mon && h.ws == card.ws);
        let key = CardKey {
            size: (card.rect.size.w.round() as i32, card.rect.size.h.round() as i32),
            label_h: state.overview_label_h() as i32,
            scale: int_scale,
            label: m.workspaces[card.ws].label.name.clone(),
            icon: m.workspaces[card.ws].label.icon.clone(),
            active: card.ws == m.active_ws,
            hovered,
            selected: mon == state.active_mon && o.selected == card.ws,
        };
        backs.push((card.rect, key, card.ws, if zooming { p as f32 } else { alpha }));
    }
    for (rect, key, ws, alpha) in backs {
        if let Some(buffer) = state.card_buffer(output, ws, key) {
            let loc = (rect.loc - origin).to_physical(out_scale);
            if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                loc,
                &buffer,
                Some(alpha),
                None,
                None,
                Kind::Unspecified,
            ) {
                out.push(OutputElement::Memory(e));
            }
        }
    }

    // The backdrop over everything else on the monitor.
    let size = output_geo.size.to_physical_precise_round(out_scale);
    let (mut commit, last) = o.backdrop_commit.get();
    let level = (p * 1000.0) as u32;
    if level != last {
        commit.increment();
        o.backdrop_commit.set((commit, level));
    }
    out.push(OutputElement::Solid(SolidColorRenderElement::new(
        o.backdrop_id.clone(),
        Rectangle::from_size(size),
        commit,
        faded(state.config.theme.palette.backdrop, p as f32),
        Kind::Unspecified,
    )));
    true
}

/// A window drawn at `loc` (its geometry's top-left, global logical), `scale` times its size.
#[allow(clippy::too_many_arguments)]
fn push_window<R>(
    out: &mut Vec<OutputElement<R>>,
    renderer: &mut R,
    window: &Window,
    loc: Point<f64, Logical>,
    scale: f64,
    to_phys: &impl Fn(Point<f64, Logical>) -> Point<i32, Physical>,
    out_scale: f64,
    alpha: f32,
) where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let geo = window.geometry();
    // The surface's own origin sits before its geometry (client-side shadows).
    let origin = to_phys(loc - geo.loc.to_f64().upscale(scale));
    // Surfaces take their size from the output's scale when drawn, so draw them full size at
    // `origin` and shrink the result around that point.
    out.extend(
        smithay::backend::renderer::element::AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
            window,
            renderer,
            origin,
            Scale::from(out_scale),
            crate::state::window_opacity(window) * alpha,
        )
        .into_iter()
        .map(|e| OutputElement::Rescaled(RescaleRenderElement::from_element(e, origin, scale))),
    );
}
