//! Pop-up menus drawn by the compositor: the window menu (right-click a tab in the bar), the
//! power menu (the power button at the end of the bar), tray icon menus, and the bell's list
//! of notifications. Painted on the CPU like the bar, from the theme's font, palette and radius, and
//! drawn above everything except the pointer.
//!
//! Mouse: click an item, or press-drag-release from where the menu opened. Click elsewhere to
//! close. Keys: arrows move, Enter picks, 1-9 move to a workspace, Escape closes.

use std::cell::RefCell;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, Transform},
};

use crate::{
    config::{Action, Color},
    layout::Rect,
    power::PowerAction,
    state::{State, close_window, toplevel_ids},
    text::{Canvas, Text},
    theme::Theme,
};

/// Space around the rows, inside the menu's edge.
const PAD: i32 = 5;
/// Text inset from a row's left edge.
const INSET: i32 = 10;
const SEPARATOR_H: i32 = 9;
const MIN_WIDTH: i32 = 200;
const MAX_WIDTH: i32 = 360;
/// How far the pointer must travel after opening before releasing the button picks an item.
const DRAG_PICK: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Close,
    ToggleFloating,
    ToggleFullscreen,
    /// Workspace index on the window's monitor.
    MoveToWorkspace(usize),
    /// Monitor index.
    MoveToMonitor(usize),
    Power(PowerAction),
    /// An entry of a tray icon's menu, by its id there.
    TrayItem(i32),
    /// Open a notification from the bell's list.
    Notification(u32),
    ClearNotifications,
    ToggleDnd,
}

/// What a menu is for.
pub enum MenuTarget {
    Window(Window),
    Power,
    /// A tray icon's menu, by the item's id.
    Tray(String),
    Notifications,
}

enum Entry {
    /// Grey text that can't be picked, like the window's title.
    Heading(String),
    Item {
        label: String,
        action: MenuAction,
        /// Drawn in the `urgent` color (Close).
        danger: bool,
        /// A check item: Some(on). Drawn with a dot in front when on.
        checked: Option<bool>,
    },
    Separator,
    /// A row of numbered buttons, one per workspace.
    Workspaces {
        current: usize,
        occupied: Vec<bool>,
    },
}

/// Something in the menu that can be picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Index into the entries.
    Item(usize),
    Workspace(usize),
}

/// Key presses the menu reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    Up,
    Down,
    Left,
    Right,
    Pick,
    Close,
    Workspace(usize),
}

pub struct Menu {
    pub target: MenuTarget,
    pub output: Output,
    /// Top-left corner, in global logical coordinates.
    pub loc: Point<i32, Logical>,
    /// Where the pointer was when the menu opened.
    opened_at: Point<f64, Logical>,
    panel: Panel,
}

impl Menu {
    pub fn rect(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(self.loc, self.panel.size.into())
    }

    /// What's under a global point.
    pub fn hit_at(&self, pos: Point<f64, Logical>) -> Option<Hit> {
        self.panel.hit_at(pos.x - self.loc.x as f64, pos.y - self.loc.y as f64)
    }
}

/// What a menu shows and how it's laid out, apart from where it is and what it's for.
struct Panel {
    size: (i32, i32),
    entries: Vec<Entry>,
    hover: Option<Hit>,
    theme: Theme,
    row_h: i32,
    /// Last painted image: (scale, hover, image).
    cache: RefCell<Option<(i32, Option<Hit>, MemoryRenderBuffer)>>,
}

impl Panel {
    fn new(text: &Text, theme: &Theme, entries: Vec<Entry>) -> Self {
        let row_h = (theme.font_size * 2.1).round().max(22.0) as i32;
        let text_w = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Heading(t) => Some(text.width(t, theme.font_size)),
                // Check items have their dot's room in front.
                Entry::Item { label: t, checked, .. } => {
                    Some(text.width(t, theme.font_size) + if checked.is_some() { theme.font_size * 1.2 } else { 0.0 })
                }
                _ => None,
            })
            .fold(0.0f32, f32::max)
            .ceil() as i32;
        let ws_w = entries
            .iter()
            .find_map(|e| match e {
                Entry::Workspaces { occupied, .. } => Some(occupied.len() as i32 * ws_button(row_h)),
                _ => None,
            })
            .unwrap_or(0);
        let w = (text_w + 2 * INSET).max(ws_w).clamp(MIN_WIDTH, MAX_WIDTH) + 2 * PAD;
        let h = entries.iter().map(|e| entry_height(e, row_h)).sum::<i32>() + 2 * PAD;
        Self {
            size: (w, h),
            entries,
            hover: None,
            theme: theme.clone(),
            row_h,
            cache: RefCell::new(None),
        }
    }

    /// Each entry's row, relative to the menu's corner.
    fn rows(&self) -> Vec<Rect> {
        let mut y = PAD;
        self.entries
            .iter()
            .map(|e| {
                let h = entry_height(e, self.row_h);
                let r = Rect::new(PAD, y, self.size.0 - 2 * PAD, h);
                y += h;
                r
            })
            .collect()
    }

    /// Everything pickable and where it is, relative to the menu's corner.
    fn targets(&self) -> Vec<(Rect, Hit)> {
        let mut out = Vec::new();
        for (i, (e, r)) in self.entries.iter().zip(self.rows()).enumerate() {
            match e {
                Entry::Item { .. } => out.push((r, Hit::Item(i))),
                Entry::Workspaces { current, occupied } => {
                    for (n, rect) in ws_buttons(r, occupied.len(), self.row_h).into_iter().enumerate() {
                        if n != *current {
                            out.push((rect, Hit::Workspace(n)));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// What's under a point relative to the corner.
    fn hit_at(&self, x: f64, y: f64) -> Option<Hit> {
        self.targets()
            .into_iter()
            .find(|(r, _)| x >= r.x as f64 && x < (r.x + r.w) as f64 && y >= r.y as f64 && y < (r.y + r.h) as f64)
            .map(|(_, h)| h)
    }

    fn action(&self, hit: Hit) -> Option<MenuAction> {
        match hit {
            Hit::Workspace(n) => self
                .entries
                .iter()
                .any(|e| matches!(e, Entry::Workspaces { occupied, .. } if n < occupied.len()))
                .then_some(MenuAction::MoveToWorkspace(n)),
            Hit::Item(i) => match self.entries.get(i)? {
                Entry::Item { action, .. } => Some(*action),
                _ => None,
            },
        }
    }

    /// Keyboard movement: rows go up and down, the workspace row also left and right.
    fn step(&self, key: MenuKey) -> Option<Hit> {
        // Pickable things grouped by row.
        let mut rows: Vec<Vec<Hit>> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            match e {
                Entry::Item { .. } => rows.push(vec![Hit::Item(i)]),
                Entry::Workspaces { current, occupied } => rows.push(
                    (0..occupied.len())
                        .filter(|n| n != current)
                        .map(Hit::Workspace)
                        .collect(),
                ),
                _ => {}
            }
        }
        rows.retain(|r| !r.is_empty());
        let pos = self.hover.and_then(|h| {
            rows.iter()
                .enumerate()
                .find_map(|(r, row)| Some((r, row.iter().position(|x| *x == h)?)))
        });
        let Some((r, c)) = pos else {
            return match key {
                MenuKey::Up => rows.last().map(|row| row[0]),
                _ => rows.first().map(|row| row[0]),
            };
        };
        let n = rows.len();
        Some(match key {
            MenuKey::Down => rows[(r + 1) % n][c.min(rows[(r + 1) % n].len() - 1)],
            MenuKey::Up => rows[(r + n - 1) % n][c.min(rows[(r + n - 1) % n].len() - 1)],
            MenuKey::Right => rows[r][(c + 1) % rows[r].len()],
            MenuKey::Left => rows[r][(c + rows[r].len() - 1) % rows[r].len()],
            _ => rows[r][c],
        })
    }

    /// The image at an integer scale, repainted only when the hover changes.
    fn buffer(&self, text: &Text, scale: i32) -> MemoryRenderBuffer {
        let mut cache = self.cache.borrow_mut();
        if let Some((s, hover, buffer)) = &*cache
            && *s == scale
            && *hover == self.hover
        {
            return buffer.clone();
        }
        let canvas = self.paint(text, scale);
        let buffer = MemoryRenderBuffer::from_slice(
            &canvas.to_premultiplied_rgba(),
            Fourcc::Abgr8888,
            (canvas.w, canvas.h),
            scale,
            Transform::Normal,
            None,
        );
        *cache = Some((scale, self.hover, buffer.clone()));
        buffer
    }

    fn paint(&self, text: &Text, s: i32) -> Canvas {
        let p = &self.theme.palette;
        let px = self.theme.font_size * s as f32;
        let radius = self.theme.radius * s as f32;
        let (w, h) = (self.size.0 * s, self.size.1 * s);
        let mut canvas = Canvas::new(w, h, [0.0; 4]);
        // A hairline edge, then the body.
        canvas.fill_rounded_rect(0, 0, w, h, radius + s as f32, p.separator);
        canvas.fill_rounded_rect(s, s, w - 2 * s, h - 2 * s, radius, p.surface);

        let scaled = |r: Rect| Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
        let label = |canvas: &mut Canvas, t: &str, r: Rect, size: f32, color: Color| {
            let x = r.x + INSET * s;
            let right = r.x + r.w - INSET * s;
            let fitted = text.fit(t, size, (right - x) as f32);
            let baseline = text.centered_baseline(r.y as f32, r.h as f32, size);
            text.draw(canvas, &fitted, size, (x as f32, baseline), color, (x, right));
        };
        let item_radius = (radius - PAD as f32 * s as f32 / 2.0).max(2.0 * s as f32);

        for (i, (e, r)) in self.entries.iter().zip(self.rows()).enumerate() {
            let rs = scaled(r);
            match e {
                Entry::Heading(t) => label(&mut canvas, t, rs, px * 0.85, p.subtext),
                Entry::Separator => {
                    canvas.fill_rect(rs.x + 4 * s, rs.y + rs.h / 2, rs.w - 8 * s, s, p.separator);
                }
                Entry::Item {
                    label: t, danger, checked, ..
                } => {
                    let hovered = self.hover == Some(Hit::Item(i));
                    if hovered {
                        let fill = if *danger { p.urgent } else { p.hover };
                        canvas.fill_rounded_rect(rs.x, rs.y, rs.w, rs.h, item_radius, fill);
                    }
                    let color = match (*danger, hovered) {
                        (true, true) => p.on_accent,
                        (true, false) => p.urgent,
                        _ => p.text,
                    };
                    // Check items: a dot when on, and the label moved over to make room.
                    let rs = match checked {
                        Some(on) => {
                            let d = (px * 0.45) as i32;
                            let slot = (px * 1.2) as i32;
                            if *on {
                                let (cx, cy) = (rs.x + INSET * s + (slot - d) / 2, rs.y + (rs.h - d) / 2);
                                canvas.fill_rounded_rect(cx, cy, d, d, d as f32 / 2.0, p.accent);
                            }
                            Rect::new(rs.x + slot, rs.y, rs.w - slot, rs.h)
                        }
                        None => rs,
                    };
                    label(&mut canvas, t, rs, px, color);
                }
                Entry::Workspaces { current, occupied } => {
                    for (n, b) in ws_buttons(r, occupied.len(), self.row_h).into_iter().enumerate() {
                        let b = scaled(b);
                        let pill = Rect::new(b.x + s, b.y + s, b.w - 2 * s, b.h - 2 * s);
                        let color = if n == *current {
                            canvas.fill_rounded_rect(pill.x, pill.y, pill.w, pill.h, item_radius, p.accent);
                            p.on_accent
                        } else {
                            if self.hover == Some(Hit::Workspace(n)) {
                                canvas.fill_rounded_rect(pill.x, pill.y, pill.w, pill.h, item_radius, p.hover);
                            }
                            if occupied[n] { p.text } else { p.subtext }
                        };
                        let t = (n + 1).to_string();
                        let size = px * 0.9;
                        let tx = b.x as f32 + (b.w as f32 - text.width(&t, size)) / 2.0;
                        let baseline = text.centered_baseline(b.y as f32, b.h as f32, size);
                        text.draw(&mut canvas, &t, size, (tx, baseline), color, (b.x, b.x + b.w));
                    }
                }
            }
        }
        canvas
    }
}

fn entry_height(e: &Entry, row_h: i32) -> i32 {
    match e {
        Entry::Heading(_) => row_h * 9 / 10,
        Entry::Separator => SEPARATOR_H,
        Entry::Item { .. } | Entry::Workspaces { .. } => row_h,
    }
}

/// Width of one workspace button.
fn ws_button(row_h: i32) -> i32 {
    row_h
}

/// The workspace buttons, centred in their row.
fn ws_buttons(row: Rect, count: usize, row_h: i32) -> Vec<Rect> {
    let bw = ws_button(row_h);
    let total = bw * count as i32;
    let x0 = row.x + (row.w - total).max(0) / 2;
    (0..count as i32)
        .map(|n| Rect::new(x0 + n * bw, row.y, bw, row.h))
        .collect()
}

/// A tray menu as rows: submenus become a heading with their entries under it, indented.
fn flatten_tray_menu(nodes: &[crate::dbus::tray::MenuNode], depth: usize, out: &mut Vec<Entry>) {
    let indent = "    ".repeat(depth);
    for n in nodes {
        if n.separator {
            if !matches!(out.last(), None | Some(Entry::Separator)) {
                out.push(Entry::Separator);
            }
            continue;
        }
        let label = format!("{indent}{}", n.label);
        if !n.children.is_empty() {
            out.push(Entry::Heading(label));
            flatten_tray_menu(&n.children, depth + 1, out);
        } else if n.enabled {
            out.push(Entry::Item {
                label,
                action: MenuAction::TrayItem(n.id),
                danger: false,
                checked: n.toggle,
            });
        } else {
            out.push(Entry::Heading(label));
        }
    }
    if matches!(out.last(), Some(Entry::Separator)) {
        out.pop();
    }
}

fn power_entries() -> Vec<Entry> {
    let item = |label: &str, action: PowerAction| Entry::Item {
        label: label.into(),
        action: MenuAction::Power(action),
        danger: false,
        checked: None,
    };
    vec![
        item("Lock", PowerAction::Lock),
        item("Suspend", PowerAction::Suspend),
        Entry::Separator,
        item("Log out", PowerAction::Logout),
        Entry::Separator,
        item("Restart", PowerAction::Restart),
        item("Shut down", PowerAction::Shutdown),
    ]
}

impl State {
    /// Right-click on a window's tab: open its menu at the pointer.
    pub fn open_window_menu(&mut self, window: Window, pos: Point<f64, Logical>) {
        let Some(mon) = self.monitor_of(&window) else { return };

        let m = &self.monitors[mon];
        let ws = m.ws();
        let floating = ws.entry(&window).is_some_and(|e| e.floating.is_some());
        let fullscreen = ws.fullscreen.as_ref() == Some(&window);
        let (app_id, title) = toplevel_ids(&window);
        let name = title
            .filter(|t| !t.is_empty())
            .or(app_id)
            .unwrap_or_else(|| "Window".into());

        let mut entries = vec![
            Entry::Heading(name),
            Entry::Item {
                label: if floating { "Tile" } else { "Float" }.into(),
                action: MenuAction::ToggleFloating,
                danger: false,
                checked: None,
            },
            Entry::Item {
                label: if fullscreen { "Exit fullscreen" } else { "Fullscreen" }.into(),
                action: MenuAction::ToggleFullscreen,
                danger: false,
                checked: None,
            },
            Entry::Separator,
            Entry::Heading("Move to workspace".into()),
            Entry::Workspaces {
                current: m.active_ws,
                occupied: m.workspaces.iter().map(|w| !w.entries.is_empty()).collect(),
            },
        ];
        let others: Vec<_> = self
            .monitors
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != mon)
            .map(|(i, o)| (i, o.output.name()))
            .collect();
        if !others.is_empty() {
            entries.push(Entry::Separator);
            for (i, name) in others {
                entries.push(Entry::Item {
                    label: format!("Move to {name}"),
                    action: MenuAction::MoveToMonitor(i),
                    danger: false,
                    checked: None,
                });
            }
        }
        entries.push(Entry::Separator);
        entries.push(Entry::Item {
            label: "Close".into(),
            action: MenuAction::Close,
            danger: true,
            checked: None,
        });

        let output = self.monitors[mon].output.clone();
        self.show_menu(MenuTarget::Window(window), output, entries, pos);
    }

    /// The power button: lock, suspend, log out, restart, shut down.
    pub fn open_power_menu(&mut self, mon: usize, pos: Point<f64, Logical>) {
        let entries = power_entries();
        let Some(output) = self.monitors.get(mon).map(|m| m.output.clone()) else {
            return;
        };
        self.show_menu(MenuTarget::Power, output, entries, pos);
    }

    /// A tray icon's menu arrived: show it where its icon was clicked.
    pub fn open_tray_menu(&mut self, item: &str, nodes: Vec<crate::dbus::tray::MenuNode>) {
        let Some((wanted, pos, mon)) = self.tray_menu_request.take() else {
            return;
        };
        if wanted != item || nodes.is_empty() {
            return;
        }
        let Some(output) = self.monitors.get(mon).map(|m| m.output.clone()) else {
            return;
        };
        let mut entries = Vec::new();
        flatten_tray_menu(&nodes, 0, &mut entries);
        self.show_menu(MenuTarget::Tray(item.to_string()), output, entries, pos);
    }

    /// The bell: recent notifications, do not disturb, clear all.
    pub fn open_notifications_menu(&mut self, mon: usize, pos: Point<f64, Logical>) {
        let Some(output) = self.monitors.get(mon).map(|m| m.output.clone()) else {
            return;
        };
        let items = &self.notifications.items;
        let mut entries = vec![Entry::Heading(if items.is_empty() {
            "No notifications".into()
        } else {
            "Notifications".into()
        })];
        for item in items.iter().take(10) {
            let n = &item.n;
            let label = match (n.app_name.is_empty(), n.summary.is_empty()) {
                (false, false) => format!("{}: {}", n.app_name, n.summary),
                (true, _) => n.summary.clone(),
                (false, true) => n.app_name.clone(),
            };
            entries.push(Entry::Item {
                label,
                action: MenuAction::Notification(n.id),
                danger: false,
                checked: None,
            });
        }
        if items.len() > 10 {
            entries.push(Entry::Heading(format!("{} more", items.len() - 10)));
        }
        entries.push(Entry::Separator);
        entries.push(Entry::Item {
            label: "Do not disturb".into(),
            action: MenuAction::ToggleDnd,
            danger: false,
            checked: Some(self.notifications.dnd),
        });
        if !items.is_empty() {
            entries.push(Entry::Item {
                label: "Clear all".into(),
                action: MenuAction::ClearNotifications,
                danger: true,
                checked: None,
            });
        }
        self.mark_notifications_read();
        self.show_menu(MenuTarget::Notifications, output, entries, pos);
    }

    /// Open a menu at the pointer, pushed back inside the monitor if it would hang off an edge.
    fn show_menu(&mut self, target: MenuTarget, output: Output, entries: Vec<Entry>, pos: Point<f64, Logical>) {
        self.load_ui_text();
        let Some((_, text)) = &self.ui_text else { return };
        let panel = Panel::new(text, &self.config.theme, entries);
        let mut menu = Menu {
            target,
            output: output.clone(),
            loc: pos.to_i32_round(),
            opened_at: pos,
            panel,
        };
        if let Some(geo) = self.space.output_geometry(&output) {
            let (w, h) = menu.panel.size;
            let x = (pos.x as i32).min(geo.loc.x + geo.size.w - w).max(geo.loc.x);
            let mut y = pos.y as i32;
            if y + h > geo.loc.y + geo.size.h {
                // Not enough room below (a bottom bar): open upwards from the pointer.
                y = (y - h).max(geo.loc.y);
            }
            menu.loc = (x, y).into();
        }
        self.menu = Some(menu);
        self.needs_redraw = true;
    }

    /// Load the theme font for menus and the switcher, or reload it if the theme changed.
    pub fn load_ui_text(&mut self) {
        if self
            .ui_text
            .as_ref()
            .is_none_or(|(font, _)| *font != self.config.theme.font)
        {
            let font = self.config.theme.font.clone();
            self.ui_text = Text::load(&font).map(|t| (font, t));
        }
    }

    pub fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.needs_redraw = true;
        }
    }

    /// The open menu, unless its window closed or left the screen (then it's hidden, and
    /// ignores input until it's closed).
    fn live_menu(&self) -> Option<&Menu> {
        self.menu.as_ref().filter(|m| match &m.target {
            MenuTarget::Window(w) => self
                .monitor_showing(w)
                .is_some_and(|mon| self.monitors[mon].output == m.output),
            MenuTarget::Power | MenuTarget::Tray(_) | MenuTarget::Notifications => {
                self.monitors.iter().any(|mon| mon.output == m.output)
            }
        })
    }

    pub fn over_menu(&self, pos: Point<f64, Logical>) -> bool {
        self.live_menu().is_some_and(|m| m.rect().to_f64().contains(pos))
    }

    /// Pointer moved: highlight the item under it.
    pub fn menu_motion(&mut self, pos: Point<f64, Logical>) {
        let Some(menu) = &mut self.menu else { return };
        // Leaving the menu keeps the last highlight, so keyboard and mouse don't fight.
        if let Some(hit) = menu.hit_at(pos)
            && menu.panel.hover != Some(hit)
        {
            menu.panel.hover = Some(hit);
            self.needs_redraw = true;
        }
    }

    /// A button press while the menu is open. Returns true if it was used up by the menu.
    pub fn menu_press(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(menu) = self.live_menu() else {
            self.close_menu();
            return false;
        };
        if !menu.rect().to_f64().contains(pos) {
            // Clicking elsewhere only closes the menu, except on the bar, which works as usual
            // (so right-clicking another tab opens its menu straight away).
            self.close_menu();
            return !self.over_bar(pos);
        }
        if let Some(hit) = menu.hit_at(pos) {
            self.menu_pick(hit);
        }
        true
    }

    /// A button release. Pressing on a tab, dragging onto an item and letting go picks it.
    pub fn menu_release(&mut self, pos: Point<f64, Logical>) {
        let Some(menu) = self.live_menu() else { return };
        let dx = pos.x - menu.opened_at.x;
        let dy = pos.y - menu.opened_at.y;
        if (dx * dx + dy * dy).sqrt() < DRAG_PICK {
            return;
        }
        if let Some(hit) = menu.hit_at(pos) {
            self.menu_pick(hit);
        }
    }

    pub fn menu_key(&mut self, key: MenuKey) {
        let Some(menu) = &mut self.menu else { return };
        match key {
            MenuKey::Close => self.close_menu(),
            MenuKey::Pick => {
                if let Some(hit) = menu.panel.hover {
                    self.menu_pick(hit);
                }
            }
            MenuKey::Workspace(n) => self.menu_pick(Hit::Workspace(n)),
            _ => {
                menu.panel.hover = menu.panel.step(key);
                self.needs_redraw = true;
            }
        }
    }

    fn menu_pick(&mut self, hit: Hit) {
        let Some(menu) = self.menu.take() else { return };
        self.needs_redraw = true;
        let Some(action) = menu.panel.action(hit) else { return };
        match (action, &menu.target) {
            (MenuAction::Power(p), _) => return self.power(p),
            (MenuAction::TrayItem(id), MenuTarget::Tray(item)) => {
                return self.bus_command(crate::dbus::Command::TrayMenuClick { item: item.clone(), id });
            }
            (MenuAction::Notification(id), _) => return self.activate_notification(id),
            (MenuAction::ClearNotifications, _) => return self.clear_notifications(),
            (MenuAction::ToggleDnd, _) => return self.toggle_dnd(),
            _ => {}
        }
        let MenuTarget::Window(w) = menu.target else { return };
        let Some(mon) = self.monitor_showing(&w) else { return };
        // Every action works on the focused window, so focus the menu's window first.
        self.active_mon = mon;
        self.focused_layer = None;
        self.focus_window(&w);
        match action {
            MenuAction::Close => close_window(&w),
            MenuAction::ToggleFloating => self.run_action(Action::ToggleFloating),
            MenuAction::ToggleFullscreen => self.run_action(Action::ToggleFullscreen),
            MenuAction::MoveToWorkspace(n) => self.run_action(Action::MoveToWorkspace(n + 1)),
            MenuAction::MoveToMonitor(target) => self.move_window_to_monitor(target),
            MenuAction::Power(_)
            | MenuAction::TrayItem(_)
            | MenuAction::Notification(_)
            | MenuAction::ClearNotifications
            | MenuAction::ToggleDnd => {}
        }
    }

    /// The open menu's image for `output`, if it's on that monitor and still means something.
    pub fn menu_buffer(&self, output: &Output, scale: i32) -> Option<(Point<i32, Logical>, MemoryRenderBuffer)> {
        let menu = self.live_menu().filter(|m| &m.output == output)?;
        let (_, text) = self.ui_text.as_ref()?;
        Some((menu.loc, menu.panel.buffer(text, scale.max(1))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_menu(entries: Vec<Entry>) -> (Vec<Rect>, i32) {
        let row_h = 24;
        let mut y = PAD;
        let rows = entries
            .iter()
            .map(|e| {
                let h = entry_height(e, row_h);
                let r = Rect::new(PAD, y, 200, h);
                y += h;
                r
            })
            .collect();
        (rows, y + PAD)
    }

    #[test]
    fn rows_stack_without_gaps() {
        let (rows, total) = rows_menu(vec![
            Entry::Heading("t".into()),
            Entry::Separator,
            Entry::Workspaces {
                current: 0,
                occupied: vec![false; 9],
            },
        ]);
        for pair in rows.windows(2) {
            assert_eq!(pair[0].y + pair[0].h, pair[1].y);
        }
        assert_eq!(total, rows.last().map(|r| r.y + r.h).unwrap() + PAD);
    }

    fn sample(theme: &Theme, text: &Text) -> Panel {
        Panel::new(
            text,
            theme,
            vec![
                Entry::Heading("uaio@host: ~/git/wayland-time".into()),
                Entry::Item {
                    label: "Float".into(),
                    action: MenuAction::ToggleFloating,
                    danger: false,
                    checked: None,
                },
                Entry::Item {
                    label: "Fullscreen".into(),
                    action: MenuAction::ToggleFullscreen,
                    danger: false,
                    checked: None,
                },
                Entry::Separator,
                Entry::Heading("Move to workspace".into()),
                Entry::Workspaces {
                    current: 0,
                    occupied: vec![true, true, false, true, false, false, false, false, false],
                },
                Entry::Separator,
                Entry::Item {
                    label: "Move to DP-4".into(),
                    action: MenuAction::MoveToMonitor(0),
                    danger: false,
                    checked: None,
                },
                Entry::Item {
                    label: "Move to HDMI-A-5".into(),
                    action: MenuAction::MoveToMonitor(2),
                    danger: false,
                    checked: None,
                },
                Entry::Separator,
                Entry::Item {
                    label: "Close".into(),
                    action: MenuAction::Close,
                    danger: true,
                    checked: None,
                },
            ],
        )
    }

    #[test]
    fn keyboard_walks_rows_and_skips_current_workspace() {
        let Some(text) = Text::load("sans") else { return };
        let theme = crate::config::Config::default().theme;
        let mut panel = sample(&theme, &text);
        panel.hover = panel.step(MenuKey::Down);
        assert_eq!(panel.hover, Some(Hit::Item(1)));
        panel.hover = panel.step(MenuKey::Up);
        assert_eq!(panel.hover, Some(Hit::Item(10)), "wraps to Close");
        panel.hover = Some(Hit::Item(2));
        panel.hover = panel.step(MenuKey::Down);
        assert_eq!(
            panel.hover,
            Some(Hit::Workspace(1)),
            "workspace 1 is current, so 2 is first"
        );
        panel.hover = panel.step(MenuKey::Left);
        assert_eq!(panel.hover, Some(Hit::Workspace(8)));
    }

    #[test]
    fn power_menu_picks_its_actions_and_ignores_workspace_keys() {
        let text = Text::load("sans").expect("font");
        let theme = crate::config::Config::default().theme;
        let panel = Panel::new(&text, &theme, power_entries());
        assert_eq!(panel.action(Hit::Item(0)), Some(MenuAction::Power(PowerAction::Lock)));
        assert_eq!(
            panel.action(Hit::Item(6)),
            Some(MenuAction::Power(PowerAction::Shutdown))
        );
        assert_eq!(panel.action(Hit::Workspace(0)), None);
    }

    #[test]
    fn hits_match_rows() {
        let Some(text) = Text::load("sans") else { return };
        let theme = crate::config::Config::default().theme;
        let panel = sample(&theme, &text);
        let rows = panel.rows();
        let close = rows[10];
        assert_eq!(
            panel.hit_at((close.x + 5) as f64, (close.y + 5) as f64),
            Some(Hit::Item(10))
        );
        let heading = rows[0];
        assert_eq!(panel.hit_at((heading.x + 5) as f64, (heading.y + 5) as f64), None);
        assert_eq!(panel.action(Hit::Item(10)), Some(MenuAction::Close));
    }

    /// `MENU_PREVIEW=/path/out.rgba cargo test menu_preview` writes the window menu as raw RGBA.
    #[test]
    fn menu_preview() {
        let Some(path) = std::env::var_os("MENU_PREVIEW") else {
            return;
        };
        let text = Text::load("sans").expect("font");
        let theme = crate::config::Config::default().theme;
        // MENU_PREVIEW_POWER shows the power menu instead of the window menu.
        let mut panel = if std::env::var_os("MENU_PREVIEW_POWER").is_some() {
            Panel::new(&text, &theme, power_entries())
        } else {
            sample(&theme, &text)
        };
        panel.hover = Some(Hit::Item(1));
        let scale = std::env::var("MENU_PREVIEW_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);
        let canvas = panel.paint(&text, scale);
        std::fs::write(&path, canvas.to_premultiplied_rgba()).unwrap();
        eprintln!("{}x{}", canvas.w, canvas.h);
    }

    #[test]
    fn workspace_buttons_fit_their_row() {
        let row = Rect::new(5, 40, 260, 24);
        let b = ws_buttons(row, 9, 24);
        assert_eq!(b.len(), 9);
        assert!(b[0].x >= row.x && b[8].x + b[8].w <= row.x + row.w);
        assert!(b.windows(2).all(|p| p[0].x + p[0].w == p[1].x));
    }
}
