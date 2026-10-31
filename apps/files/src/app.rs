//! The picker window: places on the left, the folder's path and a search box on top, the
//! files in the middle, and at the bottom the name to save as, the file type filter, the app's
//! choices and the buttons.
//!
//! Keys: arrows move, Enter opens, Backspace or Alt+Up goes up, Alt+Left/Right go back and
//! forward, Ctrl+L types a path, Ctrl+F (or just typing) searches, Ctrl+H shows hidden files,
//! Escape cancels.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

use wt_config::theme::Palette;
use wt_ui::{
    Look, color, eframe,
    egui::{
        self, Align, Align2, Color32, FontId, Key, Layout, Margin, Rect, RichText, Sense, Stroke, TextStyle,
        TextureHandle, Ui, pos2, vec2,
    },
};

use crate::{
    fs::{self, Entry, Mime, Place, SortBy},
    request::{Answer, Mode, Request},
};

const ICON: f32 = 20.0;
const SIDE_ICON: f32 = 16.0;
/// Most search results kept.
const SEARCH_LIMIT: usize = 1000;

pub struct Picker {
    look: Look,
    /// Print bare paths (run by hand) rather than the portal's answer format.
    plain: bool,
    req: Request,
    mime: Mime,
    places: Vec<Place>,
    /// The folder shown; None shows recently used files.
    dir: Option<PathBuf>,
    back: Vec<Option<PathBuf>>,
    forward: Vec<Option<PathBuf>>,
    /// Everything in `dir`.
    all: Vec<Entry>,
    /// What's listed: `all` or the search results, filtered and sorted.
    shown: Vec<Entry>,
    error: Option<String>,
    selected: Vec<PathBuf>,
    /// The row the keyboard is on, and where a Shift range starts.
    cursor: Option<usize>,
    anchor: Option<usize>,
    sort: SortBy,
    descending: bool,
    show_hidden: bool,
    search: String,
    /// Results of searching below `dir`, while `search` isn't empty.
    found: Vec<Entry>,
    searching: Option<Receiver<Vec<Entry>>>,
    filter: Option<usize>,
    /// The value of each of the app's choices.
    choices: Vec<String>,
    /// For Save: the file name.
    name: String,
    /// Ctrl+L: the path being typed.
    editing_path: Option<String>,
    /// Asked whether to replace this file.
    confirm_replace: Option<PathBuf>,
    icons: Icons,
    /// Scroll so this row shows.
    scroll_to: Option<usize>,
    /// Where the list was scrolled to, and how tall it is, last frame.
    list_view: (f32, f32),
    focus_name: bool,
    focus_search: bool,
}

impl Picker {
    pub fn new(cc: &eframe::CreationContext<'_>, req: Request, plain: bool) -> Self {
        let look = Look::new(&cc.egui_ctx, wt_config::Config::default_path());
        let icons = Icons::new(&look.theme.icon_theme);
        let start = req.folder.clone().filter(|f| f.is_dir()).unwrap_or_else(fs::home);
        let mut me = Self {
            look,
            plain,
            mime: Mime::load(),
            places: fs::places(),
            dir: None,
            back: Vec::new(),
            forward: Vec::new(),
            all: Vec::new(),
            shown: Vec::new(),
            error: None,
            selected: Vec::new(),
            cursor: None,
            anchor: None,
            sort: SortBy::Name,
            descending: false,
            show_hidden: false,
            search: String::new(),
            found: Vec::new(),
            searching: None,
            filter: req.current_filter.or((!req.filters.is_empty()).then_some(0)),
            choices: req.choices.iter().map(|c| c.selected.clone()).collect(),
            name: req.name.clone(),
            editing_path: None,
            confirm_replace: None,
            icons,
            scroll_to: None,
            list_view: (0.0, 0.0),
            focus_name: req.mode == Mode::Save,
            focus_search: false,
            req,
        };
        me.open(Some(start), false);
        me
    }

    fn mode(&self) -> Mode {
        self.req.mode
    }

    /// Show a folder (None: recent files). `remember` puts the current one on the back list.
    fn open(&mut self, dir: Option<PathBuf>, remember: bool) {
        if remember && dir != self.dir {
            self.back.push(self.dir.clone());
            self.forward.clear();
        }
        self.dir = dir;
        self.search.clear();
        self.found.clear();
        self.searching = None;
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.editing_path = None;
        self.reload();
        self.scroll_to = Some(0);
    }

    fn reload(&mut self) {
        self.error = None;
        self.all = match &self.dir {
            None => fs::recent(200),
            Some(d) => fs::list(d).unwrap_or_else(|e| {
                self.error = Some(match e.kind() {
                    std::io::ErrorKind::PermissionDenied => "You don't have permission to see this folder".into(),
                    _ => format!("Can't open this folder: {e}"),
                });
                Vec::new()
            }),
        };
        self.refresh();
    }

    /// Work out `shown` again after the listing, search, filter or sort changed.
    fn refresh(&mut self) {
        let source = if self.search.is_empty() { &self.all } else { &self.found };
        let globs = self
            .filter
            .and_then(|i| self.req.filters.get(i))
            .map(|f| f.globs(&self.mime))
            .unwrap_or_default();
        let needle = self.search.to_lowercase();
        let mut shown: Vec<Entry> = source
            .iter()
            .filter(|e| self.show_hidden || !e.hidden)
            .filter(|e| !self.req.directory || e.is_dir)
            .filter(|e| e.is_dir || globs.is_empty() || globs.iter().any(|g| fs::glob_match(g, &e.name)))
            .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        // Recent files stay newest first unless a column is picked.
        if self.dir.is_some() || self.sort != SortBy::Name || self.descending {
            fs::sort(&mut shown, self.sort, self.descending);
        }
        self.shown = shown;
        self.selected.retain(|p| self.shown.iter().any(|e| e.path == *p));
        self.cursor = self.cursor.filter(|&c| c < self.shown.len());
    }

    fn go_up(&mut self) {
        let parent = self.dir.as_ref().and_then(|d| d.parent()).map(Path::to_path_buf);
        if let Some(p) = parent {
            let from = self.dir.clone();
            self.open(Some(p), true);
            // Land on the folder we came from.
            if let Some(i) = from.and_then(|f| self.shown.iter().position(|e| e.path == f)) {
                self.select_only(i);
            }
        }
    }

    fn go_back(&mut self) {
        if let Some(d) = self.back.pop() {
            self.forward.push(self.dir.clone());
            self.open(d, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(d) = self.forward.pop() {
            self.back.push(self.dir.clone());
            self.open(d, false);
        }
    }

    fn select_only(&mut self, i: usize) {
        if let Some(e) = self.shown.get(i) {
            self.selected = vec![e.path.clone()];
            self.cursor = Some(i);
            self.anchor = Some(i);
            self.scroll_to = Some(i);
            if self.mode() == Mode::Save && !e.is_dir {
                self.name = e.name.clone();
            }
        }
    }

    /// A click on row `i`, with Ctrl or Shift held to pick several.
    fn click(&mut self, i: usize, ctrl: bool, shift: bool) {
        let many = self.req.multiple && self.mode() == Mode::Open;
        if many
            && shift
            && let Some(a) = self.anchor
        {
            let (lo, hi) = (a.min(i), a.max(i));
            self.selected = self.shown[lo..=hi].iter().map(|e| e.path.clone()).collect();
            self.cursor = Some(i);
        } else if many && ctrl {
            let p = self.shown[i].path.clone();
            if let Some(at) = self.selected.iter().position(|s| *s == p) {
                self.selected.remove(at);
            } else {
                self.selected.push(p);
            }
            self.cursor = Some(i);
            self.anchor = Some(i);
        } else {
            self.select_only(i);
        }
    }

    /// Double click or Enter on a row: folders open, files are picked.
    fn activate(&mut self, i: usize) {
        let Some(e) = self.shown.get(i).cloned() else { return };
        if e.is_dir {
            self.open(Some(e.path), true);
            return;
        }
        match self.mode() {
            Mode::Open if !self.req.directory => {
                if !self.selected.contains(&e.path) {
                    self.selected = vec![e.path.clone()];
                }
                self.accept();
            }
            Mode::Save => {
                self.name = e.name;
                self.accept();
            }
            _ => {}
        }
    }

    /// What the accept button would pick, if anything.
    fn can_accept(&self) -> bool {
        match self.mode() {
            Mode::Open if self.req.directory => self.dir.is_some() || !self.selected.is_empty(),
            Mode::Open => !self.selected.is_empty(),
            Mode::Save => !self.name.trim().is_empty() && self.dir.is_some(),
            Mode::SaveFiles => self.dir.is_some(),
        }
    }

    fn accept(&mut self) {
        if !self.can_accept() {
            return;
        }
        match self.mode() {
            Mode::Open => {
                let picked: Vec<Entry> = self
                    .selected
                    .iter()
                    .filter_map(|p| self.shown.iter().find(|e| e.path == *p).cloned())
                    .collect();
                if self.req.directory {
                    let dirs: Vec<PathBuf> = picked.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();
                    let dirs = if dirs.is_empty() {
                        self.dir.clone().into_iter().collect()
                    } else {
                        dirs
                    };
                    return self.finish(dirs);
                }
                // A single folder picked: go into it.
                if let [one] = picked.as_slice()
                    && one.is_dir
                {
                    return self.open(Some(one.path.clone()), true);
                }
                let files = picked.into_iter().filter(|e| !e.is_dir).map(|e| e.path).collect();
                self.finish(files);
            }
            Mode::Save => {
                let Some(dir) = self.dir.clone() else { return };
                let typed = fs_expand(self.name.trim());
                let target = if typed.is_absolute() { typed } else { dir.join(typed) };
                if target.is_dir() {
                    self.name.clear();
                    return self.open(Some(target), true);
                }
                if target.exists() && self.confirm_replace.as_ref() != Some(&target) {
                    self.confirm_replace = Some(target);
                    return;
                }
                self.finish(vec![target]);
            }
            Mode::SaveFiles => {
                let Some(dir) = self.dir.clone() else { return };
                let files = self.req.files.iter().map(|f| free_name(&dir, f)).collect();
                self.finish(files);
            }
        }
    }

    /// Print the answer for the portal and quit.
    fn finish(&mut self, paths: Vec<PathBuf>) {
        let answer = Answer {
            paths,
            filter: self.filter,
            choices: self
                .req
                .choices
                .iter()
                .zip(&self.choices)
                .map(|(c, v)| (c.id.clone(), v.clone()))
                .collect(),
        };
        if self.plain {
            for p in &answer.paths {
                println!("{}", p.display());
            }
        } else {
            print!("{}", answer.encode());
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }

    fn cancel(&self) -> ! {
        std::process::exit(1);
    }

    /// Start searching below the current folder (or home, from Recent) in the background.
    fn start_search(&mut self, ctx: &egui::Context) {
        self.found.clear();
        if self.search.trim().is_empty() {
            self.searching = None;
            return self.refresh();
        }
        // Matches in this folder show right away.
        let needle = self.search.to_lowercase();
        self.found = self
            .all
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        let root = self.dir.clone().unwrap_or_else(fs::home);
        let hidden = self.show_hidden;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || search(&root, &needle, hidden, &tx, &ctx));
        self.searching = Some(rx);
        self.refresh();
    }

    fn keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.memory(|m| m.focused().is_some());
        let (ctrl, shift, alt) = ctx.input(|i| (i.modifiers.command, i.modifiers.shift, i.modifiers.alt));
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));

        if pressed(Key::Escape) {
            if self.confirm_replace.take().is_some() {
                return;
            }
            if self.editing_path.take().is_some() {
                return;
            }
            if !self.search.is_empty() {
                self.search.clear();
                self.start_search(ctx);
                return;
            }
            self.cancel();
        }
        if ctrl && pressed(Key::H) {
            self.show_hidden = !self.show_hidden;
            self.refresh();
        }
        if ctrl && pressed(Key::L) {
            self.editing_path = Some(
                self.dir
                    .as_ref()
                    .map_or(String::new(), |d| d.to_string_lossy().into_owned()),
            );
        }
        if ctrl && pressed(Key::F) {
            self.focus_search = true;
        }
        if alt && pressed(Key::ArrowUp) {
            self.go_up();
        }
        if alt && pressed(Key::ArrowLeft) {
            self.go_back();
        }
        if alt && pressed(Key::ArrowRight) {
            self.go_forward();
        }
        if typing || alt {
            return;
        }
        if pressed(Key::Backspace) {
            self.go_up();
        }
        let n = self.shown.len();
        // Every press counts, even several in one frame (fast typing, key repeat).
        let presses = |k: Key| ctx.input(|i| i.num_presses(k));
        let (down, up) = (presses(Key::ArrowDown), presses(Key::ArrowUp));
        if n > 0 {
            let cur = self.cursor;
            let to = if down > 0 {
                Some(cur.map_or(down - 1, |c| c + down).min(n - 1))
            } else if up > 0 {
                Some(cur.map_or(0, |c| c.saturating_sub(up)))
            } else if pressed(Key::Home) {
                Some(0)
            } else if pressed(Key::End) {
                Some(n - 1)
            } else if pressed(Key::PageDown) {
                Some(cur.map_or(0, |c| (c + self.page_rows()).min(n - 1)))
            } else if pressed(Key::PageUp) {
                Some(cur.map_or(0, |c| c.saturating_sub(self.page_rows())))
            } else {
                None
            };
            if let Some(i) = to {
                if shift {
                    self.click(i, false, true);
                } else {
                    self.select_only(i);
                }
                self.scroll_to = Some(i);
            }
        }
        if pressed(Key::Enter) {
            match self.cursor.filter(|c| self.selected.contains(&self.shown[*c].path)) {
                Some(c) if self.selected.len() == 1 => self.activate(c),
                _ => self.accept(),
            }
        }
        // Typing a name starts a search.
        let text: String = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        });
        if !ctrl && !text.trim().is_empty() {
            self.search.push_str(&text);
            self.focus_search = true;
            self.start_search(ctx);
        }
    }

    fn page_rows(&self) -> usize {
        ((self.list_view.1 / self.row_h()).floor() as usize).max(1)
    }

    fn row_h(&self) -> f32 {
        self.look.theme.font_size + 16.0
    }
}

impl eframe::App for Picker {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        if self.icons.theme != self.look.theme.icon_theme {
            self.icons = Icons::new(&self.look.theme.icon_theme);
        }
        if let Some(rx) = &self.searching {
            let mut got = false;
            loop {
                match rx.try_recv() {
                    Ok(batch) => {
                        for e in batch {
                            if self.found.len() < SEARCH_LIMIT && !self.found.iter().any(|f| f.path == e.path) {
                                self.found.push(e);
                            }
                        }
                        got = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.searching = None;
                        break;
                    }
                }
            }
            if got {
                self.refresh();
            }
        }
        self.keys(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        egui::Panel::left("places")
            .resizable(false)
            .exact_size(200.0)
            .frame(
                egui::Frame::new()
                    .fill(color(p.surface))
                    .inner_margin(Margin::symmetric(10, 12)),
            )
            .show(ui, |ui| self.sidebar(ui, &p));
        egui::Panel::top("path")
            .frame(egui::Frame::new().fill(color(p.surface)).inner_margin(Margin {
                left: 12,
                right: 12,
                top: 10,
                bottom: 10,
            }))
            .show(ui, |ui| self.path_bar(ui, &p));
        egui::Panel::bottom("actions")
            .frame(egui::Frame::new().fill(color(p.surface)).inner_margin(Margin {
                left: 12,
                right: 12,
                top: 10,
                bottom: 12,
            }))
            .show(ui, |ui| self.actions(ui, &p));
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(color(p.base))
                    .inner_margin(Margin::symmetric(8, 6)),
            )
            .show(ui, |ui| self.list(ui, &p));
    }
}

impl Picker {
    fn sidebar(&mut self, ui: &mut Ui, p: &Palette) {
        let title = if self.req.title.is_empty() {
            match self.mode() {
                Mode::Open if self.req.directory => "Choose a Folder",
                Mode::Open => "Open",
                _ => "Save",
            }
        } else {
            self.req.title.as_str()
        };
        ui.add(egui::Label::new(RichText::new(title).heading().color(color(p.text))).truncate());
        ui.add_space(10.0);
        let mut go = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, place) in self.places.iter().enumerate() {
                // A line before the drives and the computer.
                if i > 0
                    && place.icon != self.places[i - 1].icon
                    && matches!(place.icon, "drive-harddisk-symbolic" | "drive-removable-media-symbolic")
                    && !self.places[i - 1].icon.starts_with("drive")
                {
                    ui.add_space(4.0);
                    let r = ui.available_rect_before_wrap();
                    ui.painter()
                        .hline(r.x_range(), r.top(), Stroke::new(1.0, color(p.separator)));
                    ui.add_space(6.0);
                }
                let current = self.dir == place.path;
                let h = ui.spacing().interact_size.y + 6.0;
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
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
                if let Some(t) = self.icons.symbolic(ui.ctx(), place.icon, SIDE_ICON, tint) {
                    ui.painter().image(
                        t.id(),
                        icon_rect,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                let text_rect = Rect::from_min_max(pos2(icon_rect.right() + 10.0, rect.top()), rect.right_bottom());
                clipped_text(
                    ui,
                    text_rect,
                    &place.label,
                    color(if current { p.text } else { p.muted }),
                    TextStyle::Body.resolve(ui.style()),
                );
                if r.clicked() {
                    go = Some(place.path.clone());
                }
                if let Some(path) = &place.path {
                    r.on_hover_text(path.display().to_string());
                }
            }
        });
        if let Some(dir) = go {
            self.open(dir, true);
        }
    }

    fn path_bar(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let can_back = !self.back.is_empty();
            let can_forward = !self.forward.is_empty();
            let can_up = self.dir.as_ref().is_some_and(|d| d.parent().is_some());
            if self.icon_button(ui, p, "go-previous-symbolic", "Back (Alt+Left)", can_back) {
                self.go_back();
            }
            if self.icon_button(ui, p, "go-next-symbolic", "Forward (Alt+Right)", can_forward) {
                self.go_forward();
            }
            if self.icon_button(ui, p, "go-up-symbolic", "Up (Alt+Up)", can_up) {
                self.go_up();
            }
            ui.add_space(6.0);

            let search_w = 220.0;
            let crumbs_w = (ui.available_width() - search_w - 12.0).max(80.0);
            if let Some(mut typed) = self.editing_path.take() {
                let r = ui.add_sized(
                    [crumbs_w, ui.spacing().interact_size.y],
                    egui::TextEdit::singleline(&mut typed).hint_text("Type a folder or file path"),
                );
                if !r.has_focus() && !r.lost_focus() {
                    r.request_focus();
                }
                if r.lost_focus() {
                    if ui.input(|i| i.key_pressed(Key::Enter)) {
                        self.go_to_typed(&typed);
                    }
                } else {
                    self.editing_path = Some(typed);
                }
            } else {
                ui.allocate_ui(vec2(crumbs_w, ui.spacing().interact_size.y), |ui| self.crumbs(ui, p));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text(if self.dir.is_some() {
                            "Search in this folder"
                        } else {
                            "Search in Home"
                        })
                        .desired_width(search_w),
                );
                if std::mem::take(&mut self.focus_search) {
                    r.request_focus();
                    // Keep the typed letters, cursor at the end.
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                        let end = egui::text::CCursor::new(self.search.chars().count());
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                        state.store(ui.ctx(), r.id);
                    }
                }
                if r.changed() {
                    let ctx = ui.ctx().clone();
                    self.start_search(&ctx);
                }
                // Enter in the search box opens the first match.
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.shown.is_empty() {
                    self.select_only(0);
                    self.activate(0);
                }
            });
        });
    }

    /// The folder's path as buttons, the end kept in view.
    fn crumbs(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(dir) = self.dir.clone() else {
            ui.label(RichText::new("Recent files").color(color(p.text)));
            return;
        };
        let home = fs::home();
        let mut parts: Vec<(String, PathBuf)> = Vec::new();
        let rest = match dir.strip_prefix(&home) {
            Ok(rest) if home != Path::new("/") => {
                parts.push(("Home".into(), home.clone()));
                rest.to_path_buf()
            }
            _ => {
                parts.push(("Computer".into(), PathBuf::from("/")));
                dir.strip_prefix("/").unwrap_or(&dir).to_path_buf()
            }
        };
        let mut at = parts[0].1.clone();
        for c in rest.components() {
            at = at.join(c);
            parts.push((c.as_os_str().to_string_lossy().into_owned(), at.clone()));
        }
        let mut go = None;
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
                            go = Some(path.clone());
                        }
                    }
                    // A click past the end types a path instead.
                    let (_, r) = ui.allocate_exact_size(
                        vec2(ui.available_width().max(24.0), ui.spacing().interact_size.y),
                        Sense::click(),
                    );
                    if r.on_hover_cursor(egui::CursorIcon::Text).clicked() {
                        self.editing_path = Some(dir.to_string_lossy().into_owned());
                    }
                });
            });
        if let Some(d) = go {
            self.open(Some(d), true);
        }
    }

    /// Go to a typed path: a folder opens; a file is picked (or named, when saving).
    fn go_to_typed(&mut self, typed: &str) {
        let path = fs_expand(typed.trim());
        let path = match &self.dir {
            Some(d) if path.is_relative() => d.join(path),
            _ => path,
        };
        if path.is_dir() {
            return self.open(Some(path), true);
        }
        let parent = path.parent().filter(|d| d.is_dir()).map(Path::to_path_buf);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if let (Some(parent), Some(name)) = (parent, name) {
            self.open(Some(parent), true);
            match self.mode() {
                Mode::Save => self.name = name,
                _ => {
                    if let Some(i) = self.shown.iter().position(|e| e.name == name) {
                        self.select_only(i);
                    }
                }
            }
        }
    }

    fn icon_button(&mut self, ui: &mut Ui, p: &Palette, icon: &str, tip: &str, enabled: bool) -> bool {
        let size = ui.spacing().interact_size.y;
        let (rect, r) = ui.allocate_exact_size(vec2(size, size), if enabled { Sense::click() } else { Sense::hover() });
        if enabled && r.hovered() {
            ui.painter()
                .rect_filled(rect, ui.visuals().widgets.inactive.corner_radius, color(p.hover));
        }
        let tint = if enabled { p.text } else { p.subtext };
        if let Some(t) = self.icons.symbolic(ui.ctx(), icon, SIDE_ICON, tint) {
            let ir = Rect::from_center_size(rect.center(), vec2(SIDE_ICON, SIDE_ICON));
            ui.painter().image(
                t.id(),
                ir,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        let clicked = enabled && r.clicked();
        r.on_hover_text(tip);
        clicked
    }

    fn list(&mut self, ui: &mut Ui, p: &Palette) {
        let width = ui.available_width();
        let row_h = self.row_h();
        let small = TextStyle::Small.resolve(ui.style());
        let body = TextStyle::Body.resolve(ui.style());
        let size_w = 100.0;
        let date_w = 130.0;
        let name_w = (width - size_w - date_w).max(120.0);
        let radius = ui.visuals().widgets.inactive.corner_radius;

        // The column header; a click sorts by that column, again to reverse.
        let header_h = small.size + 16.0;
        let (header, _) = ui.allocate_exact_size(vec2(width, header_h), Sense::hover());
        let mut x = header.left();
        for (col, title, w, right) in [
            (SortBy::Name, "Name", name_w, false),
            (SortBy::Size, "Size", size_w, true),
            (SortBy::Modified, "Modified", date_w, true),
        ] {
            let cell = Rect::from_min_size(pos2(x, header.top()), vec2(w, header_h));
            x += w;
            let r = ui.interact(cell, ui.id().with(("sort", title)), Sense::click());
            if r.clicked() {
                if self.sort == col {
                    self.descending = !self.descending;
                } else {
                    self.sort = col;
                    self.descending = col == SortBy::Modified;
                }
                self.refresh();
            }
            let sorted = self.sort == col && (self.dir.is_some() || col != SortBy::Name || self.descending);
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
                let d = if self.descending { 1.0 } else { -1.0 };
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

        if self.shown.is_empty() {
            let msg = if let Some(e) = &self.error {
                e.clone()
            } else if !self.search.is_empty() {
                if self.searching.is_some() {
                    "Searching…".into()
                } else {
                    "Nothing found".into()
                }
            } else if self.dir.is_none() {
                "No recent files".into()
            } else {
                "This folder is empty".into()
            };
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.label(RichText::new(msg).color(color(p.subtext))));
            return;
        }

        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if let Some(i) = self.scroll_to.take() {
            let (offset, view) = self.list_view;
            let top = i as f32 * (row_h + ui.spacing().item_spacing.y);
            let bottom = top + row_h;
            if top < offset {
                area = area.vertical_scroll_offset(top);
            } else if view > 0.0 && bottom > offset + view {
                area = area.vertical_scroll_offset(bottom - view);
            }
        }
        let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
        let mut clicked = None;
        let mut activated = None;
        let mime = &self.mime;
        let icons = &mut self.icons;
        let shown = &self.shown;
        let selected = &self.selected;
        let dir = self.dir.as_deref();
        let save = self.req.mode == Mode::Save;
        let picking_dirs = self.req.directory;
        let out = area.show_rows(ui, row_h, shown.len(), |ui, range| {
            for i in range {
                let e = &shown[i];
                let (rect, r) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                let is_sel = selected.contains(&e.path);
                if is_sel {
                    ui.painter().rect_filled(rect, radius, color(p.selected));
                } else if r.hovered() {
                    ui.painter().rect_filled(rect, radius, color(p.hover));
                }
                // Files that can't be picked (saving, or picking folders) are dimmed.
                let dim = !e.is_dir && (picking_dirs || save);
                let icon_rect =
                    Rect::from_center_size(pos2(rect.left() + 10.0 + ICON / 2.0, rect.center().y), vec2(ICON, ICON));
                if let Some(t) = icons.file(ui.ctx(), mime, e) {
                    let tint = if dim {
                        Color32::from_white_alpha(140)
                    } else {
                        Color32::WHITE
                    };
                    ui.painter().image(
                        t.id(),
                        icon_rect,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        tint,
                    );
                }
                let name_cell = Rect::from_min_max(
                    pos2(icon_rect.right() + 10.0, rect.top()),
                    pos2(rect.left() + name_w, rect.bottom()),
                );
                let text_c = color(if dim { p.subtext } else { p.text });
                // Search results and recent files say where they are.
                let elsewhere = e.path.parent().filter(|d| Some(*d) != dir);
                let name_end = clipped_text(ui, name_cell, &e.name, text_c, body.clone());
                if let Some(folder) = elsewhere {
                    let home = fs::home();
                    let shown_dir = match folder.strip_prefix(&home) {
                        Ok(r) if r.as_os_str().is_empty() => "~".to_string(),
                        Ok(r) => format!("~/{}", r.display()),
                        Err(_) => folder.display().to_string(),
                    };
                    let rest = Rect::from_min_max(pos2(name_end + 10.0, rect.top()), name_cell.right_bottom());
                    if rest.width() > 40.0 {
                        clipped_text(ui, rest, &shown_dir, color(p.subtext), small.clone());
                    }
                }
                let size_cell = Rect::from_min_size(pos2(rect.left() + name_w, rect.top()), vec2(size_w, row_h));
                if !e.is_dir {
                    cell_text(
                        ui,
                        size_cell,
                        &fs::size_text(e.size),
                        true,
                        color(p.muted),
                        small.clone(),
                    );
                }
                let date_cell = Rect::from_min_size(pos2(size_cell.right(), rect.top()), vec2(date_w, row_h));
                if let Some(m) = e.modified {
                    cell_text(ui, date_cell, &fs::date_text(m), true, color(p.muted), small.clone());
                }
                if r.double_clicked() {
                    activated = Some(i);
                } else if r.clicked() {
                    clicked = Some(i);
                }
                r.on_hover_text_at_pointer(e.path.display().to_string());
            }
        });
        self.list_view = (out.state.offset.y, out.inner_rect.height());
        if let Some(i) = clicked {
            self.click(i, ctrl, shift);
        }
        if let Some(i) = activated {
            self.activate(i);
        }
    }

    fn actions(&mut self, ui: &mut Ui, p: &Palette) {
        let field_h = ui.spacing().interact_size.y;
        if self.mode() == Mode::Save {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Name").color(color(p.muted)));
                let r = ui.add_sized(
                    [ui.available_width(), field_h],
                    egui::TextEdit::singleline(&mut self.name).hint_text("File name"),
                );
                if std::mem::take(&mut self.focus_name) {
                    r.request_focus();
                    // Select the name without its extension, ready to be typed over.
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                        let stem = match self.name.rfind('.') {
                            Some(dot) if dot > 0 => self.name[..dot].chars().count(),
                            _ => self.name.chars().count(),
                        };
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(stem),
                        )));
                        state.store(ui.ctx(), r.id);
                    }
                }
                if r.changed() {
                    self.confirm_replace = None;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.accept();
                }
            });
            ui.add_space(8.0);
        }
        if self.mode() == Mode::SaveFiles && !self.req.files.is_empty() {
            let n = self.req.files.len();
            let what = if n == 1 {
                self.req.files[0].clone()
            } else {
                format!("{n} files")
            };
            ui.label(RichText::new(format!("Saving {what} into the folder shown")).color(color(p.muted)));
            ui.add_space(6.0);
        }

        if let Some(target) = self.confirm_replace.clone() {
            let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
            egui::Frame::new()
                .fill(color(p.raised))
                .corner_radius(ui.visuals().widgets.inactive.corner_radius)
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("“{name}” already exists. Replace it?")).color(color(p.text)));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let replace = egui::Button::new(RichText::new("Replace").color(color(p.on_accent)))
                                .fill(color(p.urgent));
                            if ui.add(replace).clicked() {
                                self.accept();
                            }
                            if ui.button("Keep it").clicked() {
                                self.confirm_replace = None;
                            }
                        });
                    });
                });
            ui.add_space(8.0);
        }

        ui.horizontal(|ui| {
            let mut hidden = self.show_hidden;
            if ui
                .checkbox(&mut hidden, RichText::new("Hidden files").color(color(p.muted)))
                .on_hover_text("Ctrl+H")
                .changed()
            {
                self.show_hidden = hidden;
                self.refresh();
            }
            if !self.req.filters.is_empty() {
                ui.add_space(8.0);
                let current = self
                    .filter
                    .and_then(|i| self.req.filters.get(i))
                    .map_or("All files", |f| f.name.as_str())
                    .to_string();
                let mut pick = self.filter;
                egui::ComboBox::from_id_salt("filter")
                    .selected_text(current)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for (i, f) in self.req.filters.iter().enumerate() {
                            ui.selectable_value(&mut pick, Some(i), f.name.as_str());
                        }
                    });
                if pick != self.filter {
                    self.filter = pick;
                    self.refresh();
                }
            }
            for (i, c) in self.req.choices.iter().enumerate() {
                ui.add_space(8.0);
                let value = &mut self.choices[i];
                if c.options.is_empty() {
                    let mut on = value == "true";
                    if ui
                        .checkbox(&mut on, RichText::new(&c.label).color(color(p.muted)))
                        .changed()
                    {
                        *value = on.to_string();
                    }
                } else {
                    ui.label(RichText::new(&c.label).color(color(p.muted)));
                    let current = c
                        .options
                        .iter()
                        .find(|(id, _)| id == value)
                        .map_or(value.clone(), |(_, l)| l.clone());
                    egui::ComboBox::from_id_salt(("choice", i))
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for (id, label) in &c.options {
                                ui.selectable_value(value, id.clone(), label.as_str());
                            }
                        });
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if !self.req.accept.is_empty() {
                    self.req.accept.clone()
                } else {
                    match self.mode() {
                        Mode::Open if self.req.directory => "Select".into(),
                        Mode::Open => "Open".into(),
                        _ => "Save".into(),
                    }
                };
                let ok = egui::Button::new(RichText::new(label).color(color(p.on_accent)))
                    .fill(color(p.accent))
                    .min_size(vec2(96.0, field_h));
                if ui.add_enabled(self.can_accept(), ok).clicked() {
                    self.accept();
                }
                if ui
                    .add(egui::Button::new("Cancel").min_size(vec2(96.0, field_h)))
                    .clicked()
                {
                    self.cancel();
                }
                if !self.selected.is_empty() && self.mode() == Mode::Open && self.req.multiple {
                    let n = self.selected.len();
                    ui.label(RichText::new(format!("{n} selected")).color(color(p.subtext)));
                }
            });
        });
    }
}

/// Text in a cell, cut off at its right edge. Returns where the text ends.
fn clipped_text(ui: &Ui, cell: Rect, text: &str, c: Color32, font: FontId) -> f32 {
    let painter = ui.painter().with_clip_rect(cell.intersect(ui.clip_rect()));
    let r = painter.text(pos2(cell.left(), cell.center().y), Align2::LEFT_CENTER, text, font, c);
    r.right().min(cell.right())
}

fn cell_text(ui: &Ui, cell: Rect, text: &str, right: bool, c: Color32, font: FontId) -> Rect {
    let inner = cell.shrink2(vec2(10.0, 0.0));
    let painter = ui.painter().with_clip_rect(inner.intersect(ui.clip_rect()));
    let (x, align) = if right {
        (inner.right(), Align2::RIGHT_CENTER)
    } else {
        (inner.left(), Align2::LEFT_CENTER)
    };
    painter.text(pos2(x, inner.center().y), align, text, font, c)
}

/// `~` and `~/x` in typed paths.
fn fs_expand(s: &str) -> PathBuf {
    PathBuf::from(wt_config::expand_home(s))
}

/// `dir/name`, or `dir/name (2).ext` and so on if that's taken.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Look below `root` for names containing `needle`, sending batches as they're found.
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

/// File and place icons as textures, looked up once.
struct Icons {
    icons: wt_icons::Icons,
    theme: String,
    /// File icons by kind ("dir", or the extension).
    files: HashMap<String, Option<TextureHandle>>,
    symbolic: HashMap<(String, [u8; 4]), Option<TextureHandle>>,
}

impl Icons {
    fn new(theme: &str) -> Self {
        Self {
            icons: wt_icons::Icons::new(theme),
            theme: theme.to_string(),
            files: HashMap::new(),
            symbolic: HashMap::new(),
        }
    }

    fn file(&mut self, ctx: &egui::Context, mime: &Mime, e: &Entry) -> Option<TextureHandle> {
        let key = if e.is_dir {
            "/dir".to_string()
        } else {
            mime.type_of(&e.name).unwrap_or("").to_string()
        };
        if let Some(t) = self.files.get(&key) {
            return t.clone();
        }
        let px = (ICON * ctx.pixels_per_point()).round() as u32;
        let tex = mime
            .icons_for(e)
            .iter()
            .find_map(|name| self.icons.any(name, px, [255; 4]))
            .map(|icon| {
                let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
                ctx.load_texture(format!("file:{key}"), image, egui::TextureOptions::LINEAR)
            });
        self.files.insert(key, tex.clone());
        tex
    }

    fn symbolic(
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
        let tex = self.icons.symbolic(name, px, c).map(|icon| {
            let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
            ctx.load_texture(format!("sym:{name}"), image, egui::TextureOptions::LINEAR)
        });
        self.symbolic.insert(key, tex.clone());
        tex
    }
}
