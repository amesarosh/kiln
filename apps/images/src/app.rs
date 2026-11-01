//! The viewer window: the picture, as big as fits (or zoomed and dragged around), a bar on top
//! with the name, position in the folder and the buttons, and the keys in `main.rs`'s help.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

use wayland_time_files::{fs, trash, view::Icons};
use wt_config::theme::Palette;
use wt_ui::{
    Look, color, eframe,
    egui::{
        self, Align, Color32, Key, Layout, Margin, PointerButton, Pos2, Rect, RichText, Sense, TextureHandle,
        TextureOptions, Ui, Vec2, ViewportCommand, pos2, vec2,
    },
};

use crate::load::{self, Decoded, Loader};

/// Zoom limits, as a fraction of the image's own size.
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;
/// How long the "moved to the trash" note stays.
const TOAST: Duration = Duration::from_secs(6);

/// An image as textures.
struct Shown {
    frames: Vec<(TextureHandle, Duration)>,
    /// Its own size, in pixels.
    size: Vec2,
    alpha: bool,
    format: &'static str,
    bytes: u64,
}

enum Slot {
    Loading,
    Ready(Rc<Shown>),
    Failed(String),
}

/// How the current image is shown.
#[derive(Clone, Copy, Default)]
struct View {
    /// Image pixels per screen pixel; None fits it to the window.
    zoom: Option<f32>,
    /// Of the image's center from the canvas's, in points.
    offset: Vec2,
    /// Clockwise quarter turns.
    turns: u8,
}

struct Toast {
    text: String,
    /// The trashed file, where it was and where it went, for Undo.
    undo: Option<(PathBuf, PathBuf)>,
    until: Instant,
}

pub struct Viewer {
    look: Look,
    icons: Icons,
    loader: Loader,
    /// The folder's images, in name order, and which is shown.
    dir: Option<PathBuf>,
    dir_modified: Option<SystemTime>,
    files: Vec<PathBuf>,
    index: usize,
    cache: HashMap<PathBuf, Slot>,
    view: View,
    /// The animation frame showing, and since when.
    frame: usize,
    frame_since: Instant,
    toast: Option<Toast>,
    title: String,
}

impl Viewer {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>) -> Self {
        let look = Look::new(&cc.egui_ctx, wt_config::Config::default_path());
        let icons = Icons::new(&look.theme.icon_theme);
        let max_side = cc.egui_ctx.input(|i| i.max_texture_side).max(2048);
        let mut v = Self {
            look,
            icons,
            loader: Loader::new(cc.egui_ctx.clone(), max_side),
            dir: None,
            dir_modified: None,
            files: Vec::new(),
            index: 0,
            cache: HashMap::new(),
            view: View::default(),
            frame: 0,
            frame_since: Instant::now(),
            toast: None,
            title: String::new(),
        };
        if let Some(path) = path {
            v.open(&path);
        }
        v
    }

    /// Show `path` (an image, or a folder: its first image) and list the rest of its folder.
    fn open(&mut self, path: &Path) {
        let (dir, file) = if path.is_dir() {
            (path.to_path_buf(), None)
        } else {
            (
                path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
                Some(path),
            )
        };
        self.dir = Some(dir);
        self.rescan(file);
        self.go(self.index);
    }

    /// List the folder again, keeping `keep` (or else the current image) shown if it's still there.
    fn rescan(&mut self, keep: Option<&Path>) {
        let Some(dir) = &self.dir else { return };
        let keep = keep.map(Path::to_path_buf).or_else(|| self.current().cloned());
        self.dir_modified = modified(dir);
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| load::is_image(p) && p.is_file())
                    .collect()
            })
            .unwrap_or_default();
        // Asked for by name: shown even if its name doesn't look like an image.
        if let Some(k) = &keep
            && !files.contains(k)
            && k.is_file()
        {
            files.push(k.clone());
        }
        files.sort_by(|a, b| fs::natural(&file_name(a), &file_name(b)));
        self.index = keep
            .and_then(|k| files.iter().position(|f| *f == k))
            .unwrap_or(self.index.min(files.len().saturating_sub(1)));
        self.files = files;
    }

    fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }

    /// Show image `i`, decode its neighbours too, and forget the far-away ones.
    fn go(&mut self, i: usize) {
        if self.files.is_empty() {
            self.cache.clear();
            return;
        }
        let i = i.min(self.files.len() - 1);
        if i != self.index || self.view.zoom.is_some() || self.view.turns != 0 {
            self.view = View::default();
        }
        self.index = i;
        self.frame = 0;
        self.frame_since = Instant::now();
        let n = self.files.len();
        let near: Vec<PathBuf> = [0isize, 1, -1, 2, -2]
            .iter()
            .map(|d| (i as isize + d).rem_euclid(n as isize) as usize)
            .map(|j| self.files[j].clone())
            .fold(Vec::new(), |mut v, p| {
                if !v.contains(&p) {
                    v.push(p);
                }
                v
            });
        self.cache.retain(|p, _| near.contains(p));
        // Decode the shown one first, then the next and previous; not the ones two away.
        let wanted: Vec<PathBuf> = near
            .iter()
            .take(3)
            .filter(|p| !self.cache.contains_key(*p))
            .cloned()
            .collect();
        for p in &wanted {
            self.cache.insert(p.clone(), Slot::Loading);
        }
        self.loader.want(wanted);
    }

    /// Next (+1) or previous (-1), round the end of the folder.
    fn step(&mut self, by: isize) {
        if self.files.is_empty() {
            return;
        }
        // New or deleted files in the folder: list it again first.
        if let Some(dir) = &self.dir
            && modified(dir) != self.dir_modified
        {
            self.rescan(None);
        }
        let n = self.files.len() as isize;
        self.go((self.index as isize + by).rem_euclid(n) as usize);
    }

    fn take_decoded(&mut self, ctx: &egui::Context) {
        while let Ok((path, result)) = self.loader.done.try_recv() {
            if !self.cache.contains_key(&path) {
                continue;
            }
            let slot = match result {
                Ok(d) => Slot::Ready(Rc::new(upload(ctx, &path, d))),
                Err(e) => Slot::Failed(e),
            };
            self.cache.insert(path, slot);
        }
    }

    fn shown(&self) -> Option<&Slot> {
        self.cache.get(self.current()?)
    }

    fn trash_current(&mut self) {
        let Some(path) = self.current().cloned() else { return };
        match trash::trash(&path) {
            Ok(trashed) => {
                self.toast = Some(Toast {
                    text: format!("“{}” moved to the trash", file_name(&path)),
                    undo: Some((path.clone(), trashed)),
                    until: Instant::now() + TOAST,
                });
                self.cache.remove(&path);
                self.files.retain(|f| *f != path);
                self.index = self.index.min(self.files.len().saturating_sub(1));
                self.dir_modified = self.dir.as_deref().and_then(modified);
                self.view = View::default();
                self.go(self.index);
            }
            Err(e) => self.note(format!("Couldn't move it to the trash: {e}")),
        }
    }

    fn undo_trash(&mut self, original: &Path, trashed: &Path) {
        match trash::restore_from(trashed, original) {
            Ok(back) => {
                self.toast = None;
                self.rescan(Some(&back));
                self.go(self.index);
            }
            Err(e) => self.note(format!("Couldn't restore it: {e}")),
        }
    }

    fn note(&mut self, text: String) {
        self.toast = Some(Toast {
            text,
            undo: None,
            until: Instant::now() + TOAST,
        });
    }

    fn set_fullscreen(&self, ctx: &egui::Context, on: bool) {
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(on));
    }
}

impl eframe::App for Viewer {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        if self.icons.theme != self.look.theme.icon_theme {
            self.icons = Icons::new(&self.look.theme.icon_theme);
        }
        self.take_decoded(ctx);
        let title = self
            .current()
            .map_or_else(|| "Image Viewer".to_string(), |p| file_name(p));
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        if let Some(t) = &self.toast {
            match t.until.checked_duration_since(Instant::now()) {
                Some(left) => ctx.request_repaint_after(left),
                None => self.toast = None,
            }
        }
        self.keys(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
        if !fullscreen {
            egui::Panel::top("bar")
                .frame(
                    egui::Frame::new()
                        .fill(color(p.surface))
                        .inner_margin(Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| self.bar(ui, &p));
        }
        egui::CentralPanel::default_margins()
            .frame(egui::Frame::new().fill(color(p.base)))
            .show(ui, |ui| self.canvas(ui, &p));
        self.toast(ui.ctx(), &p);
    }
}

impl Viewer {
    fn keys(&mut self, ctx: &egui::Context) {
        let (pressed, shift, back, forward) = ctx.input(|i| {
            let keys: Vec<Key> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, .. } => Some(*key),
                    _ => None,
                })
                .collect();
            (
                keys,
                i.modifiers.shift,
                i.pointer.button_pressed(PointerButton::Extra1),
                i.pointer.button_pressed(PointerButton::Extra2),
            )
        });
        if back {
            self.step(-1);
        }
        if forward {
            self.step(1);
        }
        for key in pressed {
            match key {
                Key::ArrowRight | Key::PageDown | Key::Space => self.step(1),
                Key::ArrowLeft | Key::PageUp | Key::Backspace => self.step(-1),
                Key::Home => self.go(0),
                Key::End => self.go(self.files.len().saturating_sub(1)),
                Key::Plus | Key::Equals => self.zoom_by(ctx, 1.25, None),
                Key::Minus => self.zoom_by(ctx, 0.8, None),
                Key::Num0 => self.fit(),
                Key::Num1 => self.actual_size(ctx, None),
                Key::R if shift => self.turn(3),
                Key::R => self.turn(1),
                Key::F | Key::F11 => {
                    let on = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    self.set_fullscreen(ctx, !on);
                }
                Key::Escape => self.set_fullscreen(ctx, false),
                Key::Delete => self.trash_current(),
                _ => {}
            }
        }
    }

    fn turn(&mut self, quarter_turns: u8) {
        self.view.turns = (self.view.turns + quarter_turns) % 4;
        self.view.offset = Vec2::ZERO;
    }

    fn fit(&mut self) {
        self.view.zoom = None;
        self.view.offset = Vec2::ZERO;
    }

    /// The last canvas size and the current image's fit zoom, saved by `canvas` for the
    /// toolbar and keys, which act outside it.
    fn layout(&self, ctx: &egui::Context) -> Option<(Rect, f32, f32)> {
        let rect = ctx.data(|d| d.get_temp::<Rect>(egui::Id::new("canvas")))?;
        let Some(Slot::Ready(s)) = self.shown() else {
            return None;
        };
        let ppp = ctx.pixels_per_point();
        let fit = fit_zoom(rect, displayed(s.size, self.view.turns), ppp);
        Some((rect, fit, ppp))
    }

    /// Zoom by `factor`, keeping the image point under `at` (or the middle) where it is.
    fn zoom_by(&mut self, ctx: &egui::Context, factor: f32, at: Option<Pos2>) {
        let Some((rect, fit, ppp)) = self.layout(ctx) else {
            return;
        };
        let old = self.view.zoom.unwrap_or(fit);
        let new = (old * factor).clamp(MIN_ZOOM.min(fit), MAX_ZOOM);
        self.set_zoom(rect, fit, ppp, old, new, at);
    }

    /// 100%, or back to fitting if it's already at 100%.
    fn actual_size(&mut self, ctx: &egui::Context, at: Option<Pos2>) {
        let Some((rect, fit, ppp)) = self.layout(ctx) else {
            return;
        };
        let old = self.view.zoom.unwrap_or(fit);
        self.set_zoom(rect, fit, ppp, old, 1.0, at);
    }

    fn set_zoom(&mut self, rect: Rect, fit: f32, _ppp: f32, old: f32, new: f32, at: Option<Pos2>) {
        // Back to (or below) the fitting size: just fit.
        if new <= fit * 1.001 && self.view.zoom.is_none_or(|z| z > fit) && new < old {
            return self.fit();
        }
        let at = at.unwrap_or(rect.center()) - rect.center();
        self.view.offset = at - (at - self.view.offset) * (new / old);
        self.view.zoom = Some(new);
    }

    fn bar(&mut self, ui: &mut Ui, p: &Palette) {
        let ctx = ui.ctx().clone();
        let has = !self.files.is_empty();
        let several = self.files.len() > 1;
        ui.horizontal(|ui| {
            if wayland_time_files::view::icon_button(
                ui,
                p,
                &mut self.icons,
                "go-previous-symbolic",
                "Previous (Left)",
                several,
            )
            .clicked()
            {
                self.step(-1);
            }
            if wayland_time_files::view::icon_button(
                ui,
                p,
                &mut self.icons,
                "go-next-symbolic",
                "Next (Right)",
                several,
            )
            .clicked()
            {
                self.step(1);
            }
            ui.add_space(6.0);

            // The name gets what the buttons on the right leave.
            let button = ui.spacing().interact_size.y;
            let right_w = 7.0 * button + 54.0 + 3.0 * 6.0 + 8.0 * ui.spacing().item_spacing.x;
            let text_w = (ui.available_width() - right_w).max(40.0);
            ui.allocate_ui_with_layout(
                vec2(text_w, ui.spacing().interact_size.y + 8.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let Some(path) = self.current().cloned() else {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Image Viewer").color(color(p.text)));
                        return;
                    };
                    ui.add(egui::Label::new(RichText::new(file_name(&path)).color(color(p.text))).truncate());
                    let mut detail = format!("{} of {}", self.index + 1, self.files.len());
                    if let Some(Slot::Ready(s)) = self.shown() {
                        detail += &format!(
                            " · {} × {} · {} · {}",
                            s.size.x as u32,
                            s.size.y as u32,
                            s.format,
                            fs::size_text(s.bytes)
                        );
                    }
                    ui.add(egui::Label::new(RichText::new(detail).small().color(color(p.muted))).truncate());
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
                let (icon, tip) = if fullscreen {
                    ("view-restore-symbolic", "Leave full screen (F)")
                } else {
                    ("view-fullscreen-symbolic", "Full screen (F)")
                };
                if wayland_time_files::view::icon_button(ui, p, &mut self.icons, icon, tip, true).clicked() {
                    self.set_fullscreen(&ctx, !fullscreen);
                }
                if wayland_time_files::view::icon_button(
                    ui,
                    p,
                    &mut self.icons,
                    "user-trash-symbolic",
                    "Move to the trash (Delete)",
                    has,
                )
                .clicked()
                {
                    self.trash_current();
                }
                ui.add_space(6.0);
                if wayland_time_files::view::icon_button(
                    ui,
                    p,
                    &mut self.icons,
                    "object-rotate-right-symbolic",
                    "Turn right (R)",
                    has,
                )
                .clicked()
                {
                    self.turn(1);
                }
                if wayland_time_files::view::icon_button(
                    ui,
                    p,
                    &mut self.icons,
                    "object-rotate-left-symbolic",
                    "Turn left (Shift+R)",
                    has,
                )
                .clicked()
                {
                    self.turn(3);
                }
                ui.add_space(6.0);
                if wayland_time_files::view::icon_button(ui, p, &mut self.icons, "zoom-in-symbolic", "Zoom in (+)", has)
                    .clicked()
                {
                    self.zoom_by(&ctx, 1.25, None);
                }
                let pct = self
                    .layout(&ctx)
                    .map(|(_, fit, _)| format!("{:.0}%", self.view.zoom.unwrap_or(fit) * 100.0))
                    .unwrap_or_default();
                let label = ui
                    .add_sized(
                        vec2(54.0, ui.spacing().interact_size.y),
                        egui::Button::new(RichText::new(pct).color(color(p.subtext))).frame(false),
                    )
                    .on_hover_text(if self.view.zoom.is_some() {
                        "Fit to the window (0)"
                    } else {
                        "Actual size (1)"
                    });
                if label.clicked() {
                    if self.view.zoom.is_some() {
                        self.fit();
                    } else {
                        self.actual_size(&ctx, None);
                    }
                }
                if wayland_time_files::view::icon_button(
                    ui,
                    p,
                    &mut self.icons,
                    "zoom-out-symbolic",
                    "Zoom out (−)",
                    has,
                )
                .clicked()
                {
                    self.zoom_by(&ctx, 0.8, None);
                }
            });
        });
    }

    fn canvas(&mut self, ui: &mut Ui, p: &Palette) {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("canvas"), rect));
        let ctx = ui.ctx().clone();
        let shown = match self.shown() {
            None => {
                return message(
                    ui,
                    p,
                    &mut self.icons,
                    rect,
                    "image-x-generic-symbolic",
                    "No image open",
                    "Open a picture from Files, or run: wayland-time-image-viewer <file>",
                );
            }
            Some(Slot::Loading) => {
                egui::Spinner::new()
                    .size(28.0)
                    .color(color(p.muted))
                    .paint_at(ui, Rect::from_center_size(rect.center(), vec2(28.0, 28.0)));
                return;
            }
            Some(Slot::Failed(e)) => {
                let e = e.clone();
                return message(
                    ui,
                    p,
                    &mut self.icons,
                    rect,
                    "image-missing-symbolic",
                    "Can't open this image",
                    &e,
                );
            }
            Some(Slot::Ready(s)) => s.clone(),
        };

        let ppp = ctx.pixels_per_point();
        let size = displayed(shown.size, self.view.turns);
        let fit = fit_zoom(rect, size, ppp);

        // Scroll or pinch to zoom around the pointer; double-click for actual size and back.
        if response.hovered() {
            let (scroll, pinch, pointer) =
                ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
            let factor = pinch * (scroll * 0.0035).exp();
            if (factor - 1.0).abs() > 1e-4 {
                let old = self.view.zoom.unwrap_or(fit);
                let new = (old * factor).clamp(MIN_ZOOM.min(fit), MAX_ZOOM);
                self.set_zoom(rect, fit, ppp, old, new, pointer);
            }
        }
        if response.double_clicked() {
            let at = response.interact_pointer_pos();
            if self.view.zoom.is_some() {
                self.fit();
            } else {
                self.set_zoom(rect, fit, ppp, fit, 1.0, at);
            }
        }
        if response.dragged() && self.view.zoom.is_some() {
            self.view.offset += response.drag_delta();
        }
        let zoom = self.view.zoom.unwrap_or(fit);
        let points = size * zoom / ppp;
        // Keep it on screen: centered along a side that fits, edges no further in than the window's.
        let slack = ((points - rect.size()) / 2.0).max(Vec2::ZERO);
        self.view.offset = self.view.offset.clamp(-slack, slack);
        let image_rect = Rect::from_center_size(rect.center() + self.view.offset, points);
        if self.view.zoom.is_some() && (slack.x > 0.0 || slack.y > 0.0) {
            ctx.set_cursor_icon(if response.dragged() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }

        let painter = ui.painter_at(rect);
        if shown.alpha {
            checkerboard(&painter, image_rect.intersect(rect), p);
        }
        // The animation's frame.
        let mut frame = self.frame.min(shown.frames.len() - 1);
        if shown.frames.len() > 1 {
            let mut since = self.frame_since.elapsed();
            while since >= shown.frames[frame].1 {
                since -= shown.frames[frame].1;
                self.frame_since += shown.frames[frame].1;
                frame = (frame + 1) % shown.frames.len();
            }
            self.frame = frame;
            ctx.request_repaint_after(shown.frames[frame].1 - since);
        }
        paint_turned(&painter, &shown.frames[frame].0, image_rect, self.view.turns);
    }

    fn toast(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(t) = &self.toast else { return };
        let (text, undo) = (t.text.clone(), t.undo.clone());
        let mut clicked_undo = false;
        let mut dismiss = false;
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -24.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(color(p.surface))
                    .stroke(egui::Stroke::new(1.0, color(p.separator)))
                    .corner_radius(ui.visuals().widgets.noninteractive.corner_radius)
                    .inner_margin(Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(text).color(color(p.text)));
                            if undo.is_some() {
                                ui.add_space(8.0);
                                clicked_undo = ui.button("Undo").clicked();
                            }
                            dismiss = wayland_time_files::view::icon_button(
                                ui,
                                p,
                                &mut self.icons,
                                "window-close-symbolic",
                                "Dismiss",
                                true,
                            )
                            .clicked();
                        });
                    });
            });
        if clicked_undo && let Some((original, trashed)) = undo {
            self.undo_trash(&original, &trashed);
        } else if dismiss {
            self.toast = None;
        }
    }
}

/// The image's size in pixels as it shows, turned.
fn displayed(size: Vec2, turns: u8) -> Vec2 {
    if turns % 2 == 1 { vec2(size.y, size.x) } else { size }
}

/// The zoom that fits `size` in `rect`, but never bigger than actual size.
fn fit_zoom(rect: Rect, size: Vec2, ppp: f32) -> f32 {
    let room = rect.size() * ppp;
    (room.x / size.x.max(1.0)).min(room.y / size.y.max(1.0)).min(1.0)
}

/// Draw `t` filling `rect`, turned `turns` quarter turns clockwise.
fn paint_turned(painter: &egui::Painter, t: &TextureHandle, rect: Rect, turns: u8) {
    let mut mesh = egui::Mesh::with_texture(t.id());
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let uvs = [pos2(0.0, 0.0), pos2(1.0, 0.0), pos2(1.0, 1.0), pos2(0.0, 1.0)];
    for (i, pos) in corners.into_iter().enumerate() {
        mesh.vertices.push(egui::epaint::Vertex {
            pos,
            uv: uvs[(i + 4 - turns as usize) % 4],
            color: Color32::WHITE,
        });
    }
    mesh.indices = vec![0, 1, 2, 0, 2, 3];
    painter.add(egui::Shape::mesh(mesh));
}

/// Light and dark squares behind see-through images.
fn checkerboard(painter: &egui::Painter, area: Rect, p: &Palette) {
    if !area.is_positive() {
        return;
    }
    const CELL: f32 = 12.0;
    painter.rect_filled(area, 0.0, color(p.surface));
    let dark = color(p.raised);
    let (x0, y0) = ((area.left() / CELL).floor() as i64, (area.top() / CELL).floor() as i64);
    let (x1, y1) = (
        (area.right() / CELL).ceil() as i64,
        (area.bottom() / CELL).ceil() as i64,
    );
    for y in y0..y1 {
        for x in x0..x1 {
            if (x + y) % 2 == 0 {
                continue;
            }
            let cell = Rect::from_min_size(pos2(x as f32 * CELL, y as f32 * CELL), vec2(CELL, CELL)).intersect(area);
            painter.rect_filled(cell, 0.0, dark);
        }
    }
}

/// A big icon, a title and a line of text in the middle of `rect`.
fn message(ui: &mut Ui, p: &Palette, icons: &mut Icons, rect: Rect, icon: &str, title: &str, body: &str) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Center)),
    );
    child.add_space((rect.height() / 2.0 - 80.0).max(10.0));
    if let Some(t) = icons.symbolic(child.ctx(), icon, 64.0, p.muted) {
        child.add(egui::Image::new(&t).fit_to_exact_size(vec2(64.0, 64.0)));
    }
    child.add_space(12.0);
    child.label(RichText::new(title).heading().color(color(p.text)));
    child.add_space(4.0);
    child.add(egui::Label::new(RichText::new(body).color(color(p.subtext))).wrap());
}

fn upload(ctx: &egui::Context, path: &Path, d: Decoded) -> Shown {
    let options = TextureOptions {
        mipmap_mode: Some(egui::TextureFilter::Linear),
        ..TextureOptions::LINEAR
    };
    let name = path.to_string_lossy();
    let frames = d
        .frames
        .into_iter()
        .enumerate()
        .map(|(i, (image, delay))| (ctx.load_texture(format!("{name}#{i}"), image, options), delay))
        .collect();
    Shown {
        frames,
        size: vec2(d.size[0] as f32, d.size[1] as f32),
        alpha: d.alpha,
        format: d.format,
        bytes: d.bytes,
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map_or_else(|| p.to_string_lossy(), |n| n.to_string_lossy())
        .into_owned()
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}
