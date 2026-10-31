//! Drawing shared by the file picker and the file manager: icons, the places sidebar, the
//! path bar, the column header, and searching below a folder.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
};

use wt_config::theme::Palette;
use wt_ui::{
    color,
    egui::{
        self, Align2, Color32, FontId, Key, Rect, RichText, Sense, Stroke, TextStyle, TextureHandle, Ui, pos2, vec2,
    },
};

use crate::fs::{self, Entry, Mime, SortBy};

/// File icons in listings.
pub const ICON: f32 = 24.0;
/// Symbolic icons in the sidebar and on buttons.
pub const SIDE_ICON: f32 = 16.0;
/// Most search results kept.
pub const SEARCH_LIMIT: usize = 1000;

/// Height of a row in a listing.
pub fn row_height(font_size: f32) -> f32 {
    font_size + 16.0
}

/// A place in the sidebar. Returns whether it was clicked.
pub fn place_row(
    ui: &mut Ui,
    p: &Palette,
    icons: &mut Icons,
    label: &str,
    icon: &str,
    current: bool,
    tip: Option<&str>,
) -> egui::Response {
    place_row_with(ui, p, icons, label, icon, current, tip, None).0
}

/// A place in the sidebar with a small button at its right end (`button`: its icon and tooltip),
/// like the eject button on a mounted drive. Returns the row's and the button's responses.
#[allow(clippy::too_many_arguments)]
pub fn place_row_with(
    ui: &mut Ui,
    p: &Palette,
    icons: &mut Icons,
    label: &str,
    icon: &str,
    current: bool,
    tip: Option<&str>,
    button: Option<(&str, &str)>,
) -> (egui::Response, Option<egui::Response>) {
    let h = ui.spacing().interact_size.y + 6.0;
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    // Added after the row, so it's on top and gets its own clicks.
    let button = button.map(|(icon, tip)| {
        let size = h - 8.0;
        let b_rect = Rect::from_center_size(pos2(rect.right() - 4.0 - size / 2.0, rect.center().y), vec2(size, size));
        let b = ui.interact(b_rect, r.id.with("button"), Sense::click());
        (b, icon, tip, b_rect)
    });
    let rect_for_text = match &button {
        Some((_, _, _, b_rect)) => Rect::from_min_max(rect.min, pos2(b_rect.left() - 4.0, rect.bottom())),
        None => rect,
    };
    let radius = ui.visuals().widgets.inactive.corner_radius;
    if current {
        ui.painter().rect_filled(rect, radius, color(p.selected));
    } else if r.hovered() {
        ui.painter().rect_filled(rect, radius, color(p.hover));
    }
    let icon_rect = Rect::from_center_size(
        pos2(rect.left() + 10.0 + SIDE_ICON / 2.0, rect.center().y),
        vec2(SIDE_ICON, SIDE_ICON),
    );
    let tint = if current { p.text } else { p.muted };
    if let Some(t) = icons.symbolic(ui.ctx(), icon, SIDE_ICON, tint) {
        paint_image(ui, &t, icon_rect, Color32::WHITE);
    }
    let text_rect = Rect::from_min_max(pos2(icon_rect.right() + 10.0, rect.top()), rect_for_text.right_bottom());
    clipped_text(ui, text_rect, label, color(tint), TextStyle::Body.resolve(ui.style()));
    let button = button.map(|(b, icon, tip, b_rect)| {
        if b.hovered() {
            ui.painter().rect_filled(b_rect, radius, color(p.selected));
        }
        let tint = if b.hovered() || current { p.text } else { p.muted };
        if let Some(t) = icons.symbolic(ui.ctx(), icon, SIDE_ICON, tint) {
            paint_image(ui, &t, Rect::from_center_size(b_rect.center(), vec2(SIDE_ICON, SIDE_ICON)), Color32::WHITE);
        }
        b.on_hover_text(tip)
    });
    let r = match tip {
        Some(t) => r.on_hover_text(t),
        None => r,
    };
    (r, button)
}

/// A thin line across the sidebar, between groups of places.
pub fn sidebar_divider(ui: &mut Ui, p: &Palette) {
    ui.add_space(4.0);
    let r = ui.available_rect_before_wrap();
    ui.painter()
        .hline(r.x_range(), r.top(), Stroke::new(1.0, color(p.separator)));
    ui.add_space(6.0);
}

/// A square button showing a symbolic icon.
pub fn icon_button(
    ui: &mut Ui,
    p: &Palette,
    icons: &mut Icons,
    icon: &str,
    tip: &str,
    enabled: bool,
) -> egui::Response {
    let size = ui.spacing().interact_size.y;
    let (rect, r) = ui.allocate_exact_size(vec2(size, size), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && r.hovered() {
        ui.painter()
            .rect_filled(rect, ui.visuals().widgets.inactive.corner_radius, color(p.hover));
    }
    let tint = if enabled { p.text } else { p.subtext };
    if let Some(t) = icons.symbolic(ui.ctx(), icon, SIDE_ICON, tint) {
        paint_image(
            ui,
            &t,
            Rect::from_center_size(rect.center(), vec2(SIDE_ICON, SIDE_ICON)),
            Color32::WHITE,
        );
    }
    r.on_hover_text(tip)
}

pub enum CrumbClick {
    /// A folder on the path was clicked.
    Go(PathBuf),
    /// The empty space after the path was clicked: type a path instead.
    Edit,
}

/// A folder's path as buttons ("Home / Documents / x"), the end kept in view.
pub fn crumbs(ui: &mut Ui, p: &Palette, dir: &Path) -> Option<CrumbClick> {
    let home = fs::home();
    let mut parts: Vec<(String, PathBuf)> = Vec::new();
    let rest = match dir.strip_prefix(&home) {
        Ok(rest) if home != Path::new("/") => {
            parts.push(("Home".into(), home.clone()));
            rest.to_path_buf()
        }
        _ => {
            parts.push(("Computer".into(), PathBuf::from("/")));
            dir.strip_prefix("/").unwrap_or(dir).to_path_buf()
        }
    };
    let mut at = parts[0].1.clone();
    for c in rest.components() {
        at = at.join(c);
        parts.push((c.as_os_str().to_string_lossy().into_owned(), at.clone()));
    }
    let mut out = None;
    egui::ScrollArea::horizontal()
        .stick_to_right(true)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let last = parts.len() - 1;
                for (i, (label, path)) in parts.iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("/").color(color(p.subtext)));
                    }
                    let text = RichText::new(label).color(color(if i == last { p.text } else { p.muted }));
                    let b = egui::Button::new(if i == last { text.strong() } else { text }).frame(false);
                    if ui.add(b).clicked() && i != last {
                        out = Some(CrumbClick::Go(path.clone()));
                    }
                }
                let (_, r) = ui.allocate_exact_size(
                    vec2(ui.available_width().max(24.0), ui.spacing().interact_size.y),
                    Sense::click(),
                );
                if r.on_hover_cursor(egui::CursorIcon::Text).clicked() {
                    out = Some(CrumbClick::Edit);
                }
            });
        });
    out
}

/// The box for typing a path (Ctrl+L). None while still typing; Some(true) when Enter was
/// pressed, Some(false) when it was left.
pub fn path_field(ui: &mut Ui, width: f32, typed: &mut String) -> Option<bool> {
    let r = ui.add_sized(
        [width, wt_ui::field_height(ui.style())],
        wt_ui::field(ui, egui::TextEdit::singleline(typed).hint_text("Type a folder or file path")),
    );
    if !r.has_focus() && !r.lost_focus() {
        r.request_focus();
    }
    r.lost_focus().then(|| ui.input(|i| i.key_pressed(Key::Enter)))
}

/// The search box. `focus` puts the cursor in it, at the end of what's typed.
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str, width: f32, focus: bool) -> egui::Response {
    let r = ui.add(wt_ui::field(ui, egui::TextEdit::singleline(text).hint_text(hint).desired_width(width)));
    if focus {
        r.request_focus();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), r.id) {
            let end = egui::text::CCursor::new(text.chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
            state.store(ui.ctx(), r.id);
        }
    }
    r
}

/// Put the text cursor of a text field over `text` up to its extension, ready to be typed over.
pub fn select_stem(ctx: &egui::Context, id: egui::Id, text: &str) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let stem = match text.rfind('.') {
            Some(dot) if dot > 0 => text[..dot].chars().count(),
            _ => text.chars().count(),
        };
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(stem),
        )));
        state.store(ctx, id);
    }
}

/// A column of a listing: what it sorts by, its title, width, and whether it's right-aligned.
pub type Column<'a> = (SortBy, &'a str, f32, bool);

/// The column titles; a click sorts by that column, again to reverse. `own_order`: the listing
/// is in an order of its own (like recent files) while sorted by name, ascending. Returns
/// whether the sort changed.
pub fn sort_header(
    ui: &mut Ui,
    p: &Palette,
    columns: &[Column<'_>],
    sort: &mut SortBy,
    descending: &mut bool,
    own_order: bool,
) -> bool {
    let width = ui.available_width();
    let small = TextStyle::Small.resolve(ui.style());
    let header_h = small.size + 16.0;
    let (header, _) = ui.allocate_exact_size(vec2(width, header_h), Sense::hover());
    let mut x = header.left();
    let mut changed = false;
    for &(col, title, w, right) in columns {
        let cell = Rect::from_min_size(pos2(x, header.top()), vec2(w, header_h));
        x += w;
        let r = ui.interact(cell, ui.id().with(("sort", title)), Sense::click());
        if r.clicked() {
            if *sort == col {
                *descending = !*descending;
            } else {
                *sort = col;
                *descending = col == SortBy::Modified;
            }
            changed = true;
        }
        let sorted = *sort == col && !(own_order && col == SortBy::Name && !*descending);
        let c = color(
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).hovered() || sorted {
                p.text
            } else {
                p.subtext
            },
        );
        // Right-aligned titles leave room for the arrow on their right.
        let text_cell = if right && sorted {
            cell.with_max_x(cell.right() - 14.0)
        } else {
            cell
        };
        let r = cell_text(ui, text_cell, title, right, c, small.clone());
        if sorted {
            // Drawn, not a glyph: not every font has arrows.
            let m = pos2(if right { cell.right() - 14.0 } else { r.right() + 10.0 }, r.center().y);
            let d = if *descending { 1.0 } else { -1.0 };
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    pos2(m.x - 3.5, m.y - 2.0 * d),
                    pos2(m.x + 3.5, m.y - 2.0 * d),
                    pos2(m.x, m.y + 2.5 * d),
                ],
                c,
                Stroke::NONE,
            ));
        }
    }
    ui.painter().line_segment(
        [header.left_bottom(), header.right_bottom()],
        Stroke::new(1.0, color(p.separator)),
    );
    ui.add_space(4.0);
    changed
}

/// A grey line of text in the middle of an empty listing.
pub fn empty_message(ui: &mut Ui, p: &Palette, msg: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| ui.label(RichText::new(msg).color(color(p.subtext))));
}

/// "~/Documents" for a folder under home, the full path otherwise.
pub fn short_dir(dir: &Path) -> String {
    match dir.strip_prefix(fs::home()) {
        Ok(r) if r.as_os_str().is_empty() => "~".to_string(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => dir.display().to_string(),
    }
}

pub fn paint_image(ui: &Ui, t: &TextureHandle, rect: Rect, tint: Color32) {
    ui.painter()
        .image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
}

/// Text in a cell, cut off at its right edge. Returns where the text ends.
pub fn clipped_text(ui: &Ui, cell: Rect, text: &str, c: Color32, font: FontId) -> f32 {
    let painter = ui.painter().with_clip_rect(cell.intersect(ui.clip_rect()));
    let r = painter.text(pos2(cell.left(), cell.center().y), Align2::LEFT_CENTER, text, font, c);
    r.right().min(cell.right())
}

/// Text in a column cell, with some padding, left or right aligned.
pub fn cell_text(ui: &Ui, cell: Rect, text: &str, right: bool, c: Color32, font: FontId) -> Rect {
    let inner = cell.shrink2(vec2(10.0, 0.0));
    let painter = ui.painter().with_clip_rect(inner.intersect(ui.clip_rect()));
    let (x, align) = if right {
        (inner.right(), Align2::RIGHT_CENTER)
    } else {
        (inner.left(), Align2::LEFT_CENTER)
    };
    painter.text(pos2(x, inner.center().y), align, text, font, c)
}

/// Searching below a folder in the background, results arriving in batches.
pub struct Search {
    rx: mpsc::Receiver<Vec<Entry>>,
}

impl Search {
    /// Look below `root` for names containing `needle` (lowercase).
    pub fn start(root: PathBuf, needle: String, hidden: bool, ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || search(&root, &needle, hidden, &tx, &ctx));
        Self { rx }
    }

    /// Results found since last time, added to `found`. Returns (anything new, still going).
    pub fn poll(&self, found: &mut Vec<Entry>) -> (bool, bool) {
        let mut got = false;
        loop {
            match self.rx.try_recv() {
                Ok(batch) => {
                    for e in batch {
                        if found.len() < SEARCH_LIMIT && !found.iter().any(|f| f.path == e.path) {
                            found.push(e);
                        }
                    }
                    got = true;
                }
                Err(mpsc::TryRecvError::Empty) => return (got, true),
                Err(mpsc::TryRecvError::Disconnected) => return (got, false),
            }
        }
    }
}

fn search(root: &Path, needle: &str, hidden: bool, tx: &mpsc::Sender<Vec<Entry>>, ctx: &egui::Context) {
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0u32)]);
    let mut batch = Vec::new();
    let mut sent = 0;
    let mut last = std::time::Instant::now();
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !hidden && name.starts_with('.') {
                continue;
            }
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() && depth < 8 && !matches!(e.path().to_str(), Some("/proc" | "/sys" | "/dev" | "/run")) {
                queue.push_back((e.path(), depth + 1));
            }
            if name.to_lowercase().contains(needle)
                && let Some(entry) = Entry::from_path(e.path())
            {
                batch.push(entry);
            }
        }
        if !batch.is_empty() && last.elapsed().as_millis() > 80 {
            sent += batch.len();
            if tx.send(std::mem::take(&mut batch)).is_err() {
                return; // a newer search took over
            }
            ctx.request_repaint();
            last = std::time::Instant::now();
        }
        if sent >= SEARCH_LIMIT {
            break;
        }
    }
    let _ = tx.send(batch);
    ctx.request_repaint();
}

/// File, app and place icons as textures, looked up once.
pub struct Icons {
    icons: wt_icons::Icons,
    pub theme: String,
    /// File icons by kind ("/dir", or the MIME type).
    files: HashMap<String, Option<TextureHandle>>,
    /// Full-color icons by name (apps).
    named: HashMap<String, Option<TextureHandle>>,
    symbolic: HashMap<(String, [u8; 4]), Option<TextureHandle>>,
}

impl Icons {
    pub fn new(theme: &str) -> Self {
        Self {
            icons: wt_icons::Icons::new(theme),
            theme: theme.to_string(),
            files: HashMap::new(),
            named: HashMap::new(),
            symbolic: HashMap::new(),
        }
    }

    /// The icon for a listed entry. Folders get an `outline` drawn around them.
    pub fn file(
        &mut self,
        ctx: &egui::Context,
        mime: &Mime,
        e: &Entry,
        outline: wt_config::Color,
    ) -> Option<TextureHandle> {
        let outline = color(outline).to_srgba_unmultiplied();
        let key = if e.is_dir {
            // Keyed by the outline color, so a theme change draws it afresh.
            format!("/dir{outline:?}")
        } else {
            mime.type_of(&e.name).unwrap_or("").to_string()
        };
        if let Some(t) = self.files.get(&key) {
            return t.clone();
        }
        let ppp = ctx.pixels_per_point();
        let px = (ICON * ppp).round() as u32;
        // About 1.5 points wide, drawn around the icon (so the texture is a little bigger than
        // ICON; see `file_icon_rect`).
        let width = if e.is_dir && outline[3] > 0 { (1.5 * ppp).round().max(1.0) as u32 } else { 0 };
        let tex = mime
            .icons_for(e)
            .iter()
            .find_map(|name| self.icons.any(name, px, [255; 4]))
            .map(|icon| {
                if width > 0 {
                    texture(ctx, &format!("file:{key}"), &outlined(&icon, width, outline))
                } else {
                    texture(ctx, &format!("file:{key}"), &icon)
                }
            });
        self.files.insert(key, tex.clone());
        tex
    }

    /// The pixels of an entry's icon, `px` big (for a drag icon, drawn outside egui).
    pub fn file_pixels(&mut self, mime: &Mime, e: &Entry, px: u32) -> Option<std::rc::Rc<wt_icons::Icon>> {
        mime.icons_for(e).iter().find_map(|name| self.icons.any(name, px, [255; 4]))
    }

    /// A full-color icon by name or path, like an app's.
    pub fn named(&mut self, ctx: &egui::Context, name: &str, size: f32) -> Option<TextureHandle> {
        if let Some(t) = self.named.get(name) {
            return t.clone();
        }
        let px = (size * ctx.pixels_per_point()).round() as u32;
        let icon = if name.starts_with('/') {
            wt_icons::load_file(Path::new(name), px).map(std::rc::Rc::new)
        } else {
            self.icons.any(name, px, [255; 4])
        };
        let tex = icon.map(|icon| texture(ctx, &format!("named:{name}"), &icon));
        self.named.insert(name.to_string(), tex.clone());
        tex
    }

    pub fn symbolic(
        &mut self,
        ctx: &egui::Context,
        name: &str,
        size: f32,
        tint: wt_config::Color,
    ) -> Option<TextureHandle> {
        let c = color(tint).to_srgba_unmultiplied();
        let key = (name.to_string(), c);
        if let Some(t) = self.symbolic.get(&key) {
            return t.clone();
        }
        let px = (size * ctx.pixels_per_point()).round() as u32;
        let tex = self
            .icons
            .symbolic(name, px, c)
            .map(|icon| texture(ctx, &format!("sym:{name}"), &icon));
        self.symbolic.insert(key, tex.clone());
        tex
    }
}

/// Where to draw a file icon texture centered at `center`: ICON big, plus any outline around it.
pub fn file_icon_rect(ctx: &egui::Context, t: &TextureHandle, center: egui::Pos2) -> Rect {
    Rect::from_center_size(center, t.size_vec2() / ctx.pixels_per_point())
}

/// `icon` with a `width`-pixel outline of color `c` (unmultiplied) around its shape, on a canvas
/// `width` bigger on each side.
fn outlined(icon: &wt_icons::Icon, width: u32, c: [u8; 4]) -> wt_icons::Icon {
    let (n, w) = (icon.size as i32, width as i32);
    let size = n + 2 * w;
    // How solid the icon is at a pixel. Faint edges (soft shadows) are left out, so the outline
    // follows the icon's shape rather than its shadow.
    let alpha_at = |x: i32, y: i32| -> u8 {
        if (0..n).contains(&x) && (0..n).contains(&y) {
            let a = icon.rgba[((y * n + x) * 4 + 3) as usize] as u32;
            (a.saturating_sub(96) * 255 / (255 - 96)) as u8
        } else {
            0
        }
    };
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (ix, iy) = (x - w, y - w);
            // The outline covers whatever the icon covers within `width` (a round brush).
            let mut cover = 0u8;
            for dy in -w..=w {
                for dx in -w..=w {
                    if dx * dx + dy * dy <= w * w {
                        cover = cover.max(alpha_at(ix + dx, iy + dy));
                    }
                }
            }
            let line_a = cover as u32 * c[3] as u32 / 255;
            let i = ((y * size + x) * 4) as usize;
            let src = if (0..n).contains(&ix) && (0..n).contains(&iy) {
                let j = ((iy * n + ix) * 4) as usize;
                [icon.rgba[j], icon.rgba[j + 1], icon.rgba[j + 2], icon.rgba[j + 3]]
            } else {
                [0; 4]
            };
            // The icon (premultiplied) over the outline.
            let under = 255 - src[3] as u32;
            for k in 0..3 {
                rgba[i + k] = (src[k] as u32 + c[k] as u32 * line_a / 255 * under / 255) as u8;
            }
            rgba[i + 3] = (src[3] as u32 + line_a * under / 255) as u8;
        }
    }
    wt_icons::Icon { size: size as u32, rgba }
}

fn texture(ctx: &egui::Context, name: &str, icon: &wt_icons::Icon) -> TextureHandle {
    let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
    ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
}
