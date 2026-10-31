//! The file manager window: places on the left, the path and a search box on top, the files in
//! the middle, and a status line at the bottom (what's selected, free space, copy progress).
//!
//! Right-click a file or the empty space for what can be done. Keys: Enter opens, F2 renames,
//! Delete moves to the trash (Shift+Delete deletes for good), Ctrl+C / Ctrl+X / Ctrl+V copy,
//! cut and paste, Ctrl+Shift+N makes a folder, Ctrl+A selects everything, Backspace or Alt+Up
//! goes up, Alt+Left/Right (or the mouse's side buttons) go back and forward, Ctrl+L types a
//! path, Ctrl+F (or just typing) searches, Ctrl+H shows hidden files, F5 reloads, Ctrl+W closes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use wayland_time_files::{
    fs::{self, Entry, Mime, Place, SortBy},
    view::{self, CrumbClick, ICON, Icons, Search, cell_text, clipped_text, paint_image},
};
use wt_config::theme::Palette;
use wt_ui::{
    Look, color, eframe,
    egui::{
        self, Align, Color32, Key, Layout, Margin, PointerButton, Pos2, Rect, RichText, Sense, Stroke, TextStyle, Ui,
        pos2, vec2,
    },
};

use crate::{
    dnd::Dnd,
    drives::{self, Drive, Drives},
    open::{self, Apps},
    ops::{self, Job, Kind},
    trash,
};

/// What the window shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Loc {
    Trash,
    Dir(PathBuf),
}

impl Loc {
    fn dir(&self) -> Option<&Path> {
        match self {
            Loc::Dir(d) => Some(d),
            _ => None,
        }
    }
}

/// Where files dragged over the window would go.
#[derive(Debug, Clone, PartialEq)]
enum DropTarget {
    /// A folder row, or a place in the sidebar.
    Folder(PathBuf),
    /// The Trash in the sidebar.
    Trash,
    /// The sidebar's empty space: folders dropped there are added to it.
    Sidebar,
}

/// Files copied or cut, waiting to be pasted.
struct Clip {
    paths: Vec<PathBuf>,
    cut: bool,
}

/// A question asked before doing something that can't be undone.
enum Confirm {
    Delete(Vec<PathBuf>),
    DeleteFromTrash(Vec<PathBuf>),
    EmptyTrash,
}

/// Something asked of a drive in the sidebar.
#[derive(Clone, Copy, PartialEq)]
enum DriveAct {
    Open,
    Mount,
    Unmount,
    Eject,
}

/// A rename in progress: the file, and the name being typed.
struct Rename {
    path: PathBuf,
    name: String,
    /// Not shown yet: focus the field and select the name.
    fresh: bool,
}

pub struct Manager {
    look: Look,
    icons: Icons,
    mime: Mime,
    apps: Apps,
    places: Vec<Place>,
    /// Drives that can be mounted, listed below the places.
    drives: Drives,
    drive_list: Vec<Drive>,
    loc: Loc,
    back: Vec<Loc>,
    forward: Vec<Loc>,
    /// Everything at `loc`.
    all: Vec<Entry>,
    /// In the trash: where each file came from, by its path in the trash.
    trash_origin: HashMap<PathBuf, PathBuf>,
    trash_full: bool,
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
    found: Vec<Entry>,
    searching: Option<Search>,
    editing_path: Option<String>,
    renaming: Option<Rename>,
    clip: Option<Clip>,
    job: Option<Job>,
    /// Jobs waiting for the one running to finish.
    queue: Vec<(Kind, Vec<PathBuf>, Option<PathBuf>)>,
    confirm: Option<Confirm>,
    /// A note in the status line: (text, is an error, when).
    message: Option<(String, bool, Instant)>,
    /// When the folder shown last changed, to notice changes made elsewhere.
    stamp: Option<SystemTime>,
    last_check: Instant,
    free_space: Option<u64>,
    title: String,
    scroll_to: Option<usize>,
    list_view: (f32, f32),
    focus_search: bool,
    /// Files dragged in from other apps.
    dnd: Option<Dnd>,
    /// Where a drag carrying files is over the window, and what's under it (from the last
    /// frame drawn).
    drop_hover: Option<Pos2>,
    drop_target: Option<DropTarget>,
    /// A drag out started: the compositor now has the pointer, so the button's release never
    /// reaches egui, which would wait for it forever. We hand it one.
    release_pending: bool,
}

impl Manager {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Loc, select: Option<PathBuf>) -> Self {
        let look = Look::new(&cc.egui_ctx, wt_config::Config::default_path());
        let icons = Icons::new(&look.theme.icon_theme);
        let mut me = Self {
            look,
            icons,
            mime: Mime::load(),
            apps: Apps::load(),
            places: places(),
            drives: Drives::start(&cc.egui_ctx),
            drive_list: Vec::new(),
            loc: start.clone(),
            back: Vec::new(),
            forward: Vec::new(),
            all: Vec::new(),
            trash_origin: HashMap::new(),
            trash_full: !trash::list().is_empty(),
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
            editing_path: None,
            renaming: None,
            clip: None,
            job: None,
            queue: Vec::new(),
            confirm: None,
            message: None,
            stamp: None,
            last_check: Instant::now(),
            free_space: None,
            title: String::new(),
            scroll_to: None,
            list_view: (0.0, 0.0),
            focus_search: false,
            dnd: Dnd::start(cc),
            drop_hover: None,
            drop_target: None,
            release_pending: false,
        };
        me.open(start, false);
        if let Some(i) = select.and_then(|s| me.shown.iter().position(|e| e.path == s)) {
            me.select_only(i);
        }
        me
    }

    // ---------- where we are ----------

    /// Show a place. `remember` puts the current one on the back list.
    fn open(&mut self, loc: Loc, remember: bool) {
        if remember && loc != self.loc {
            self.back.push(self.loc.clone());
            self.forward.clear();
        }
        self.loc = loc;
        self.search.clear();
        self.found.clear();
        self.searching = None;
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.editing_path = None;
        self.renaming = None;
        // The trash is newest deleted first.
        (self.sort, self.descending) = match self.loc {
            Loc::Trash => (SortBy::Modified, true),
            _ => (SortBy::Name, false),
        };
        self.reload();
        self.scroll_to = Some(0);
    }

    /// Read the place shown again, keeping the selection.
    fn reload(&mut self) {
        self.error = None;
        self.trash_origin.clear();
        self.all = match &self.loc {
            Loc::Trash => {
                let items = trash::list();
                self.trash_full = !items.is_empty();
                items
                    .into_iter()
                    .map(|i| {
                        self.trash_origin.insert(i.entry.path.clone(), i.original.clone());
                        // The date column shows when it was deleted.
                        Entry {
                            modified: i.deleted.or(i.entry.modified),
                            ..i.entry
                        }
                    })
                    .collect()
            }
            Loc::Dir(d) => fs::list(d).unwrap_or_else(|e| {
                self.error = Some(match e.kind() {
                    std::io::ErrorKind::PermissionDenied => "You don't have permission to see this folder".into(),
                    std::io::ErrorKind::NotFound => "This folder doesn't exist anymore".into(),
                    _ => format!("Can't open this folder: {e}"),
                });
                Vec::new()
            }),
        };
        self.stamp = self.watched().and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
        self.free_space = self.loc.dir().and_then(free_space);
        self.refresh();
    }

    /// The folder whose changes mean the listing is out of date.
    fn watched(&self) -> Option<PathBuf> {
        match &self.loc {
            Loc::Dir(d) => Some(d.clone()),
            Loc::Trash => Some(trash::home_trash().join("info")),
        }
    }

    /// Work out `shown` again after the listing, search or sort changed.
    fn refresh(&mut self) {
        let source = if self.search.is_empty() { &self.all } else { &self.found };
        let needle = self.search.to_lowercase();
        let mut shown: Vec<Entry> = source
            .iter()
            .filter(|e| self.show_hidden || !e.hidden)
            .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        fs::sort(&mut shown, self.sort, self.descending);
        self.shown = shown;
        self.selected.retain(|p| self.shown.iter().any(|e| e.path == *p));
        // Keep the keyboard on the same file if it moved.
        self.cursor = self.cursor.filter(|&c| c < self.shown.len());
    }

    fn go_up(&mut self) {
        let Some(parent) = self.loc.dir().and_then(Path::parent).map(Path::to_path_buf) else {
            return;
        };
        let from = self.loc.dir().map(Path::to_path_buf);
        self.open(Loc::Dir(parent), true);
        if let Some(i) = from.and_then(|f| self.shown.iter().position(|e| e.path == f)) {
            self.select_only(i);
        }
    }

    fn go_back(&mut self) {
        if let Some(l) = self.back.pop() {
            self.forward.push(self.loc.clone());
            self.open(l, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(l) = self.forward.pop() {
            self.back.push(self.loc.clone());
            self.open(l, false);
        }
    }

    /// Go to a typed path: a folder opens; for a file, its folder opens with it selected.
    fn go_to_typed(&mut self, typed: &str) {
        let path = fs::expand(typed.trim());
        let path = match self.loc.dir() {
            Some(d) if path.is_relative() => d.join(path),
            _ => path,
        };
        if path.is_dir() {
            return self.open(Loc::Dir(path), true);
        }
        match path.parent().filter(|d| d.is_dir()) {
            Some(parent) if path.exists() => {
                self.open(Loc::Dir(parent.to_path_buf()), true);
                if let Some(i) = self.shown.iter().position(|e| e.path == path) {
                    self.select_only(i);
                }
            }
            _ => self.say_error(format!("There's nothing at {}", path.display())),
        }
    }

    // ---------- selection ----------

    fn select_only(&mut self, i: usize) {
        if let Some(e) = self.shown.get(i) {
            self.selected = vec![e.path.clone()];
            self.cursor = Some(i);
            self.anchor = Some(i);
            self.scroll_to = Some(i);
        }
    }

    /// A click on row `i`, with Ctrl or Shift held to pick several.
    fn click(&mut self, i: usize, ctrl: bool, shift: bool) {
        if shift && let Some(a) = self.anchor {
            let (lo, hi) = (a.min(i), a.max(i));
            self.selected = self.shown[lo..=hi].iter().map(|e| e.path.clone()).collect();
            self.cursor = Some(i);
        } else if ctrl {
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

    fn select_all(&mut self) {
        self.selected = self.shown.iter().map(|e| e.path.clone()).collect();
    }

    /// The selected entries, in listing order.
    fn selection(&self) -> Vec<Entry> {
        self.shown
            .iter()
            .filter(|e| self.selected.contains(&e.path))
            .cloned()
            .collect()
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selection().into_iter().map(|e| e.path).collect()
    }

    // ---------- doing things ----------

    fn say(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), false, Instant::now()));
    }

    fn say_error(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), true, Instant::now()));
    }

    /// Open entries: a single folder is shown here; files open in their default apps.
    fn open_entries(&mut self, entries: Vec<Entry>) {
        if self.loc == Loc::Trash {
            return self.say("Restore files from the trash to open them");
        }
        let (dirs, files): (Vec<Entry>, Vec<Entry>) = entries.into_iter().partition(|e| e.is_dir);
        let mut by_app: Vec<(Option<open::App>, Vec<PathBuf>)> = Vec::new();
        for f in files {
            let app = self.apps.default_for(&open::type_of(&self.mime, &f.path)).cloned();
            match by_app.iter_mut().find(|(a, _)| *a == app) {
                Some((_, paths)) => paths.push(f.path),
                None => by_app.push((app, vec![f.path])),
            }
        }
        for (app, paths) in by_app {
            let result = match &app {
                Some(app) => open::launch(app, &paths),
                None => paths.iter().try_for_each(|p| open::open_with_system(p)),
            };
            if let Err(e) = result {
                self.say_error(e);
            }
        }
        if let [dir] = dirs.as_slice() {
            self.open(Loc::Dir(dir.path.clone()), true);
        }
    }

    fn open_with(&mut self, app: &open::App) {
        let paths = self.selected_paths();
        if let Err(e) = open::launch(app, &paths) {
            self.say_error(e);
        }
    }

    /// Start a job, or queue it behind the one running.
    fn run_job(&mut self, kind: Kind, paths: Vec<PathBuf>, dest: Option<PathBuf>, ctx: &egui::Context) {
        if paths.is_empty() {
            return;
        }
        if self.job.is_some() {
            self.queue.push((kind, paths, dest));
            return;
        }
        let ctx = ctx.clone();
        let last = std::sync::Mutex::new(Instant::now());
        // Redraw for progress at most 20 times a second.
        let wake = move || {
            let mut last = last.lock().unwrap();
            if last.elapsed() >= Duration::from_millis(50) {
                *last = Instant::now();
                ctx.request_repaint();
            }
        };
        self.job = Some(Job::start(kind, paths, dest, wake));
    }

    /// A job finished: say how it went, show the change, start the next one.
    fn job_done(&mut self, kind: Kind, count: usize, outcome: ops::Outcome, ctx: &egui::Context) {
        let things = plural(count, "item");
        if outcome.cancelled {
            self.say(format!("{} stopped", kind.verb()));
        } else if let Some(first) = outcome.errors.first() {
            let more = outcome.errors.len() - 1;
            let extra = if more > 0 {
                format!(" (and {more} more)")
            } else {
                String::new()
            };
            self.say_error(format!("Couldn't finish: {first}{extra}"));
        } else {
            self.say(match kind {
                Kind::Copy => format!("Copied {things}"),
                Kind::Move => format!("Moved {things}"),
                Kind::Trash => format!("Moved {things} to the trash"),
                Kind::Delete => format!("Deleted {things}"),
            });
        }
        if kind == Kind::Trash {
            self.trash_full = true;
        }
        self.reload();
        // Select what was pasted, if it's here.
        let here: Vec<PathBuf> = outcome
            .made
            .into_iter()
            .filter(|p| self.shown.iter().any(|e| e.path == *p))
            .collect();
        if !here.is_empty() {
            self.selected = here;
            self.cursor = self.shown.iter().position(|e| self.selected.contains(&e.path));
            self.anchor = self.cursor;
            self.scroll_to = self.cursor;
        }
        if !self.queue.is_empty() {
            let (k, p, d) = self.queue.remove(0);
            self.run_job(k, p, d, ctx);
        }
    }

    fn copy(&mut self, cut: bool, ctx: &egui::Context) {
        if self.loc == Loc::Trash {
            return;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        // Other apps (a terminal, a text field) get the paths as text.
        let text: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        ctx.copy_text(text.join("\n"));
        let n = paths.len();
        self.clip = Some(Clip { paths, cut });
        self.say(format!(
            "{} {}; paste with Ctrl+V",
            plural(n, "item"),
            if cut { "cut" } else { "copied" }
        ));
    }

    /// Paste into the folder shown: the files in `text` if it names some, else what we copied.
    fn paste(&mut self, text: Option<&str>, ctx: &egui::Context) {
        let Some(dest) = self.loc.dir().map(Path::to_path_buf) else {
            return self.say("Open a folder to paste into");
        };
        let from_text = text.and_then(paths_in_text);
        let (paths, cut) = match (from_text, &self.clip) {
            (Some((paths, cut)), Some(clip)) if paths == clip.paths => (paths, cut || clip.cut),
            (Some((paths, cut)), _) => (paths, cut),
            (None, Some(clip)) => (clip.paths.clone(), clip.cut),
            (None, None) => return self.say("Nothing to paste"),
        };
        if cut {
            self.clip = None;
        }
        self.run_job(if cut { Kind::Move } else { Kind::Copy }, paths, Some(dest), ctx);
    }

    fn trash_selected(&mut self, ctx: &egui::Context) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        if self.loc == Loc::Trash {
            self.confirm = Some(Confirm::DeleteFromTrash(paths));
        } else {
            self.run_job(Kind::Trash, paths, None, ctx);
        }
    }

    fn restore_selected(&mut self) {
        let items: Vec<trash::Item> = trash::list()
            .into_iter()
            .filter(|i| self.selected.contains(&i.entry.path))
            .collect();
        let mut errors = Vec::new();
        for item in &items {
            if let Err(e) = trash::restore(item) {
                errors.push(format!("{}: {e}", item.entry.name));
            }
        }
        match errors.first() {
            Some(e) => self.say_error(format!("Couldn't restore {e}")),
            None => self.say(format!("Restored {}", plural(items.len(), "item"))),
        }
        self.reload();
    }

    fn new_folder(&mut self) {
        let Some(dir) = self.loc.dir().map(Path::to_path_buf) else {
            return;
        };
        match ops::new_folder(&dir) {
            Ok(path) => {
                self.search.clear();
                self.reload();
                if let Some(i) = self.shown.iter().position(|e| e.path == path) {
                    self.select_only(i);
                }
                self.start_rename(path);
            }
            Err(e) => self.say_error(format!("Can't make a folder here: {e}")),
        }
    }

    fn start_rename(&mut self, path: PathBuf) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.renaming = Some(Rename {
            path,
            name,
            fresh: true,
        });
    }

    fn finish_rename(&mut self) {
        let Some(r) = self.renaming.take() else { return };
        match ops::rename(&r.path, &r.name) {
            Ok(new) => {
                self.reload();
                if let Some(i) = self.shown.iter().position(|e| e.path == new) {
                    self.select_only(i);
                }
            }
            Err(e) => self.say_error(format!("Can't rename: {e}")),
        }
    }

    fn confirmed(&mut self, c: Confirm, ctx: &egui::Context) {
        match c {
            Confirm::Delete(paths) => self.run_job(Kind::Delete, paths, None, ctx),
            Confirm::DeleteFromTrash(paths) => {
                let items: Vec<trash::Item> = trash::list()
                    .into_iter()
                    .filter(|i| paths.contains(&i.entry.path))
                    .collect();
                let errors: Vec<String> = items.iter().filter_map(|i| trash::delete(i).err()).collect();
                match errors.first() {
                    Some(e) => self.say_error(format!("Couldn't delete everything: {e}")),
                    None => self.say(format!("Deleted {}", plural(items.len(), "item"))),
                }
                self.reload();
            }
            Confirm::EmptyTrash => {
                let errors: Vec<String> = trash::list().iter().filter_map(|i| trash::delete(i).err()).collect();
                match errors.first() {
                    Some(e) => self.say_error(format!("Couldn't empty the trash: {e}")),
                    None => self.say("The trash is empty"),
                }
                self.reload();
            }
        }
    }

    fn terminal_here(&mut self, dir: Option<PathBuf>) {
        let Some(dir) = dir.or_else(|| self.loc.dir().map(Path::to_path_buf)) else {
            return;
        };
        if let Err(e) = open::terminal_in(&dir) {
            self.say_error(e);
        }
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        self.found.clear();
        if self.search.trim().is_empty() {
            self.searching = None;
            return self.refresh();
        }
        let needle = self.search.to_lowercase();
        self.found = self
            .all
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        // The trash is searched as listed; folders below too.
        self.searching = self
            .loc
            .dir()
            .map(|d| Search::start(d.to_path_buf(), needle, self.show_hidden, ctx));
        self.refresh();
    }

    // ---------- keys ----------

    fn keys(&mut self, ctx: &egui::Context) {
        if self.confirm.is_some() {
            return;
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        let (ctrl, shift, alt) = ctx.input(|i| (i.modifiers.command, i.modifiers.shift, i.modifiers.alt));
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));

        if ctx.input(|i| i.pointer.button_pressed(PointerButton::Extra1)) {
            self.go_back();
        }
        if ctx.input(|i| i.pointer.button_pressed(PointerButton::Extra2)) {
            self.go_forward();
        }
        if ctrl && (pressed(Key::W) || pressed(Key::Q)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if typing {
            return;
        }
        if pressed(Key::Escape) {
            if self.editing_path.take().is_some() {
                return;
            }
            if !self.search.is_empty() {
                self.search.clear();
                return self.start_search(ctx);
            }
            self.selected.clear();
        }
        if ctrl && pressed(Key::H) {
            self.show_hidden = !self.show_hidden;
            self.refresh();
        }
        if ctrl && pressed(Key::L) {
            self.editing_path = Some(
                self.loc
                    .dir()
                    .map_or(String::new(), |d| d.to_string_lossy().into_owned()),
            );
        }
        if ctrl && pressed(Key::F) {
            self.focus_search = true;
        }
        if ctrl && pressed(Key::D) {
            let dirs = self.sidebar_candidates();
            if !dirs.is_empty() {
                self.add_to_sidebar(dirs);
            }
        }
        if ctrl && pressed(Key::A) {
            self.select_all();
        }
        if ctrl && shift && pressed(Key::N) {
            self.new_folder();
        }
        if pressed(Key::F5) || (ctrl && pressed(Key::R)) {
            self.reload();
        }
        if pressed(Key::F2)
            && self.loc != Loc::Trash
            && let [one] = self.selected_paths().as_slice()
        {
            self.start_rename(one.clone());
        }
        if pressed(Key::Delete) {
            if shift && self.loc != Loc::Trash {
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    self.confirm = Some(Confirm::Delete(paths));
                }
            } else {
                self.trash_selected(ctx);
            }
        }
        // egui turns Ctrl+C, Ctrl+X and Ctrl+V into these events.
        let events = ctx.input(|i| i.events.clone());
        for e in &events {
            match e {
                egui::Event::Copy => self.copy(false, ctx),
                egui::Event::Cut => self.copy(true, ctx),
                egui::Event::Paste(text) => self.paste(Some(text), ctx),
                _ => {}
            }
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
        if alt {
            return;
        }
        if pressed(Key::Backspace) {
            self.go_up();
        }
        let n = self.shown.len();
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
            let sel = self.selection();
            self.open_entries(sel);
        }
        // Typing a name starts a search.
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
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
        view::row_height(self.look.theme.font_size)
    }

    /// Notice changes made elsewhere (another app, a download finishing).
    fn check_for_changes(&mut self) {
        if self.last_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_check = Instant::now();
        let stamp = self.watched().and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
        if stamp != self.stamp && self.renaming.is_none() {
            self.reload();
            if !self.search.is_empty() {
                // Keep the results, only refreshed.
                self.found.retain(|e| e.path.symlink_metadata().is_ok());
                self.refresh();
            }
        }
        let places = places();
        if places != self.places {
            self.places = places;
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.loc {
            Loc::Trash => "Trash".to_string(),
            Loc::Dir(d) if d == Path::new("/") => "Computer".to_string(),
            Loc::Dir(d) if *d == fs::home() => "Home".to_string(),
            Loc::Dir(d) => d
                .file_name()
                .map_or_else(|| d.display().to_string(), |n| n.to_string_lossy().into_owned()),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{title} — Files")));
            self.title = title;
        }
    }
}

impl eframe::App for Manager {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if std::mem::take(&mut self.release_pending) {
            let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
            raw.events.push(egui::Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: raw.modifiers,
            });
            raw.events.push(egui::Event::PointerGone);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        if self.icons.theme != self.look.theme.icon_theme {
            self.icons = Icons::new(&self.look.theme.icon_theme);
        }
        if let Some(search) = &self.searching {
            let (got, going) = search.poll(&mut self.found);
            if !going {
                self.searching = None;
            }
            if got {
                self.refresh();
            }
        }
        if let Some(job) = &mut self.job
            && let Some(outcome) = job.finished()
        {
            let (kind, count) = (job.kind, job.count);
            self.job = None;
            self.job_done(kind, count, outcome, ctx);
        }
        self.drive_events();
        self.drag_events(ctx);
        // Notes fade after a while; errors stay until the next one.
        if self
            .message
            .as_ref()
            .is_some_and(|(_, err, t)| !err && t.elapsed() > Duration::from_secs(6))
        {
            self.message = None;
        }
        self.check_for_changes();
        self.update_title(ctx);
        self.keys(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        // Found again while drawing the sidebar and the list.
        self.drop_target = None;
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
            .frame(
                egui::Frame::new()
                    .fill(color(p.surface))
                    .inner_margin(Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| self.path_bar(ui, &p));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(color(p.surface))
                    .inner_margin(Margin::symmetric(12, 6)),
            )
            .show(ui, |ui| self.status(ui, &p));
        let list = egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(color(p.base))
                    .inner_margin(Margin::symmetric(8, 6)),
            )
            .show(ui, |ui| self.list(ui, &p));
        // A drag over the files, not on a folder row: they'll land in the place shown.
        if let Some(at) = self.drop_hover
            && list.response.rect.contains(at)
            && self.drop_target.is_none()
        {
            let radius = ui.visuals().widgets.inactive.corner_radius;
            let edge = Stroke::new(2.0, color(p.accent));
            ui.painter()
                .rect_stroke(list.response.rect.shrink(2.0), radius, edge, egui::StrokeKind::Inside);
        }
        self.confirm_dialog(ui.ctx(), &p);
    }
}

impl Manager {
    /// Add folders to the sidebar (GTK's bookmarks, shared with other apps).
    fn add_to_sidebar(&mut self, dirs: Vec<PathBuf>) {
        let name = |d: &Path| d.file_name().map_or_else(|| d.display().to_string(), |n| n.to_string_lossy().into_owned());
        let mut added = Vec::new();
        for dir in &dirs {
            if self.places.iter().any(|p| p.path.as_ref() == Some(dir)) {
                continue;
            }
            if let Err(e) = fs::add_bookmark(dir) {
                return self.say_error(format!("Couldn't add {} to the sidebar: {e}", name(dir)));
            }
            added.push(dir);
        }
        self.places = places();
        match added.as_slice() {
            [] if dirs.len() == 1 => self.say(format!("{} is already in the sidebar", name(&dirs[0]))),
            [] => self.say("Those folders are already in the sidebar"),
            [one] => self.say(format!("Added {} to the sidebar", name(one))),
            more => self.say(format!("Added {} to the sidebar", plural(more.len(), "folder"))),
        }
    }

    fn remove_from_sidebar(&mut self, dir: &Path) {
        match fs::remove_bookmark(dir) {
            Ok(()) => self.places = places(),
            Err(e) => self.say_error(format!("Couldn't change the sidebar: {e}")),
        }
    }

    /// What Ctrl+D and the menus add: the folders selected, else the folder shown.
    fn sidebar_candidates(&self) -> Vec<PathBuf> {
        let dirs: Vec<PathBuf> = self.selection().into_iter().filter(|e| e.is_dir).map(|e| e.path).collect();
        if !dirs.is_empty() || self.loc == Loc::Trash {
            return dirs;
        }
        self.loc.dir().map(Path::to_path_buf).into_iter().collect()
    }

    /// Files dragged in from other apps: where they hover, and what's dropped.
    fn drag_events(&mut self, ctx: &egui::Context) {
        let mut drops = Vec::new();
        self.drop_hover = None;
        if let Some(d) = &self.dnd {
            // Folders take drops, and so does the trash (it trashes them).
            d.set_can_drop(true);
            let (hover, dropped) = d.poll();
            // The other app gives window coordinates; egui's are those over the zoom.
            let zoom = ctx.zoom_factor();
            self.drop_hover = hover.map(|(x, y)| pos2(x as f32 / zoom, y as f32 / zoom));
            drops.extend(dropped.into_iter().map(|d| (uris_to_paths(&d.uris), d.moved)));
        }
        // On X11, winit takes drops itself.
        ctx.input(|i| {
            if !i.raw.hovered_files.is_empty() {
                self.drop_hover = i.pointer.latest_pos();
            }
            let paths: Vec<PathBuf> = i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect();
            if !paths.is_empty() {
                drops.push((paths, false));
            }
        });
        for (paths, moved) in drops {
            self.drop_files(paths, moved, ctx);
        }
    }

    /// Files dropped on the window: into the folder row or sidebar place they landed on, else
    /// the folder shown (or the trash, when that's shown). Folders dropped on the sidebar's
    /// empty space are added to it.
    fn drop_files(&mut self, paths: Vec<PathBuf>, moved: bool, ctx: &egui::Context) {
        let target = self.drop_target.take();
        if paths.is_empty() {
            return self.say("Only files can be dropped here");
        }
        let folder = match target {
            Some(DropTarget::Folder(f)) => Some(f),
            Some(DropTarget::Trash) => return self.run_job(Kind::Trash, paths, None, ctx),
            Some(DropTarget::Sidebar) => {
                let dirs: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_dir()).collect();
                if dirs.is_empty() {
                    return self.say("Only folders can be added to the sidebar");
                }
                return self.add_to_sidebar(dirs);
            }
            None if self.loc == Loc::Trash => return self.run_job(Kind::Trash, paths, None, ctx),
            None => None,
        };
        let Some(dest) = folder.or_else(|| self.loc.dir().map(Path::to_path_buf)) else {
            return self.say("Drop onto a folder, or open one first");
        };
        // Dropping files where they already are does nothing.
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| p.parent() != Some(&dest) && *p != dest)
            .collect();
        self.run_job(if moved { Kind::Move } else { Kind::Copy }, paths, Some(dest), ctx);
    }

    /// Drives listed, mounted or unmounted in the background.
    fn drive_events(&mut self) {
        for event in self.drives.poll() {
            match event {
                drives::Event::List(list) => self.drive_list = list,
                drives::Event::Mounted { label, path, open } => {
                    self.say(format!("Mounted {label}"));
                    if open {
                        self.open(Loc::Dir(path), true);
                    }
                }
                drives::Event::Unmounted { label, path, ejected } => {
                    self.say(format!("{} {label}", if ejected { "Ejected" } else { "Unmounted" }));
                    // Don't stay in a folder that's gone.
                    if self.loc.dir().is_some_and(|d| d.starts_with(&path)) {
                        self.open(Loc::Dir(fs::home()), true);
                    }
                    self.back.retain(|l| !l.dir().is_some_and(|d| d.starts_with(&path)));
                    self.forward.retain(|l| !l.dir().is_some_and(|d| d.starts_with(&path)));
                }
                drives::Event::Failed { what, error } => self.say_error(format!("{what}: {error}")),
            }
        }
    }

    /// The drives in the sidebar. Returns what was asked of one: (drive, action).
    fn drive_rows(&mut self, ui: &mut Ui, p: &Palette) -> Option<(Drive, DriveAct)> {
        let mut act = None;
        for d in &self.drive_list {
            let current = d.mount.as_ref().is_some_and(|m| self.loc == Loc::Dir(m.clone()));
            let tip = match &d.mount {
                Some(m) => m.display().to_string(),
                None => "Not mounted: click to mount and open".into(),
            };
            let button = d.mount.is_some().then(|| {
                if d.ejectable {
                    ("media-eject-symbolic", "Eject")
                } else {
                    ("media-eject-symbolic", "Unmount")
                }
            });
            let (r, b) = view::place_row_with(ui, p, &mut self.icons, &d.label, &d.icon, current, Some(&tip), button);
            if b.is_some_and(|b| b.clicked()) {
                act = Some((d.clone(), if d.ejectable { DriveAct::Eject } else { DriveAct::Unmount }));
            } else if r.clicked() {
                act = Some((d.clone(), DriveAct::Open));
            }
            r.context_menu(|ui| {
                wt_ui::roomy_menu(ui);
                if ui.button("Open").clicked() {
                    act = Some((d.clone(), DriveAct::Open));
                }
                if d.mount.is_some() {
                    if ui.button("Unmount").clicked() {
                        act = Some((d.clone(), DriveAct::Unmount));
                    }
                    if d.ejectable && ui.button("Eject").clicked() {
                        act = Some((d.clone(), DriveAct::Eject));
                    }
                } else if ui.button("Mount").clicked() {
                    act = Some((d.clone(), DriveAct::Mount));
                }
            });
        }
        act
    }

    fn drive_act(&mut self, d: Drive, act: DriveAct) {
        match (act, &d.mount) {
            (DriveAct::Open, Some(m)) => self.open(Loc::Dir(m.clone()), true),
            (DriveAct::Open | DriveAct::Mount, None) => {
                self.say(format!("Mounting {}…", d.label));
                self.drives.mount(&d, act == DriveAct::Open);
            }
            (DriveAct::Unmount | DriveAct::Eject, Some(_)) => {
                // Leave the drive first, so this window isn't what keeps it busy.
                if let Some(m) = &d.mount
                    && self.loc.dir().is_some_and(|dir| dir.starts_with(m))
                {
                    self.open(Loc::Dir(fs::home()), true);
                }
                self.drives.unmount(&d, act == DriveAct::Eject);
            }
            _ => {}
        }
    }

    fn sidebar(&mut self, ui: &mut Ui, p: &Palette) {
        ui.add(egui::Label::new(RichText::new("Files").heading().color(color(p.text))).truncate());
        ui.add_space(10.0);
        let mut go = None;
        let mut drive_act = None;
        let mut remove = None;
        // A drag over a place drops into it (highlighted like a folder row); over the empty
        // space it adds folders to the sidebar (the whole sidebar outlined).
        let panel = ui.clip_rect();
        let drop_at = self.drop_hover.filter(|at| panel.contains(*at));
        let mut target = drop_at.map(|_| DropTarget::Sidebar);
        let accent = color(p.accent);
        let radius = ui.visuals().widgets.inactive.corner_radius;
        let drop_on = |ui: &Ui, r: &egui::Response| {
            let on = drop_at.is_some_and(|at| r.rect.contains(at) && ui.clip_rect().contains(at));
            if on {
                let edge = Stroke::new(1.5, accent);
                ui.painter().rect_stroke(r.rect, radius, edge, egui::StrokeKind::Inside);
            }
            on
        };
        // Drives mounted under /media are listed with the other drives, not as plain places.
        let places: Vec<Place> = self
            .places
            .iter()
            .filter(|pl| !self.drive_list.iter().any(|d| d.mount.is_some() && d.mount == pl.path))
            .cloned()
            .collect();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, place) in places.iter().enumerate() {
                if i > 0 && place.icon.starts_with("drive") && !places[i - 1].icon.starts_with("drive") {
                    view::sidebar_divider(ui, p);
                    drive_act = self.drive_rows(ui, p);
                }
                let Some(dir) = place.path.clone() else { continue };
                let tip = dir.display().to_string();
                let loc = Loc::Dir(dir.clone());
                let current = self.loc == loc;
                let r = view::place_row(ui, p, &mut self.icons, &place.label, place.icon, current, Some(&tip));
                if r.clicked() {
                    go = Some(loc);
                }
                if drop_on(ui, &r) {
                    target = Some(DropTarget::Folder(dir.clone()));
                }
                if place.bookmark {
                    r.context_menu(|ui| {
                        wt_ui::roomy_menu(ui);
                        if ui.button("Remove from Sidebar").clicked() {
                            remove = Some(dir.clone());
                        }
                    });
                }
            }
            view::sidebar_divider(ui, p);
            let icon = if self.trash_full {
                "user-trash-full-symbolic"
            } else {
                "user-trash-symbolic"
            };
            let r = view::place_row(ui, p, &mut self.icons, "Trash", icon, self.loc == Loc::Trash, None);
            if r.clicked() {
                go = Some(Loc::Trash);
            }
            if drop_on(ui, &r) {
                target = Some(DropTarget::Trash);
            }
            r.context_menu(|ui| {
                wt_ui::roomy_menu(ui);
                if ui
                    .add_enabled(self.trash_full, egui::Button::new("Empty Trash…"))
                    .clicked()
                {
                    self.confirm = Some(Confirm::EmptyTrash);
                }
            });
        });
        if target == Some(DropTarget::Sidebar) {
            ui.painter()
                .rect_stroke(panel.shrink(2.0), radius, Stroke::new(2.0, accent), egui::StrokeKind::Inside);
        }
        if target.is_some() {
            self.drop_target = target;
        }
        if let Some(dir) = remove {
            self.remove_from_sidebar(&dir);
        }
        if let Some(loc) = go {
            self.open(loc, true);
        }
        if let Some((d, act)) = drive_act {
            self.drive_act(d, act);
        }
    }

    fn path_bar(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (can_back, can_forward) = (!self.back.is_empty(), !self.forward.is_empty());
            let can_up = self.loc.dir().is_some_and(|d| d.parent().is_some());
            if view::icon_button(
                ui,
                p,
                &mut self.icons,
                "go-previous-symbolic",
                "Back (Alt+Left)",
                can_back,
            )
            .clicked()
            {
                self.go_back();
            }
            if view::icon_button(
                ui,
                p,
                &mut self.icons,
                "go-next-symbolic",
                "Forward (Alt+Right)",
                can_forward,
            )
            .clicked()
            {
                self.go_forward();
            }
            if view::icon_button(ui, p, &mut self.icons, "go-up-symbolic", "Up (Alt+Up)", can_up).clicked() {
                self.go_up();
            }
            ui.add_space(6.0);

            let search_w = 220.0;
            let menu_w = ui.spacing().interact_size.y + 4.0;
            let crumbs_w = (ui.available_width() - search_w - menu_w - 16.0).max(80.0);
            if let Some(mut typed) = self.editing_path.take() {
                match view::path_field(ui, crumbs_w, &mut typed) {
                    None => self.editing_path = Some(typed),
                    Some(true) => self.go_to_typed(&typed),
                    Some(false) => {}
                }
            } else {
                ui.allocate_ui(vec2(crumbs_w, ui.spacing().interact_size.y), |ui| {
                    match self.loc.clone() {
                        Loc::Dir(dir) => match view::crumbs(ui, p, &dir) {
                            Some(CrumbClick::Go(d)) => self.open(Loc::Dir(d), true),
                            Some(CrumbClick::Edit) => self.editing_path = Some(dir.to_string_lossy().into_owned()),
                            None => {}
                        },
                        Loc::Trash => {
                            ui.label(RichText::new("Trash").color(color(p.text)));
                        }
                    }
                });
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // The menu of things to do here, also on right-click in the empty space.
                let r = view::icon_button(ui, p, &mut self.icons, "open-menu-symbolic", "Menu", true);
                egui::Popup::menu(&r).show(|ui| self.background_menu(ui));
                let hint = match self.loc {
                    Loc::Dir(_) => "Search in this folder",
                    Loc::Trash => "Search the trash",
                };
                let focus = std::mem::take(&mut self.focus_search);
                let r = view::search_field(ui, &mut self.search, hint, search_w, focus);
                if r.changed() {
                    let ctx = ui.ctx().clone();
                    self.start_search(&ctx);
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.shown.is_empty() {
                    self.select_only(0);
                    let first = self.shown[0].clone();
                    self.open_entries(vec![first]);
                }
            });
        });
    }

    /// What can be done here: right-click on empty space, or the menu button.
    fn background_menu(&mut self, ui: &mut Ui) {
        wt_ui::roomy_menu(ui);
        let ctx = ui.ctx().clone();
        match self.loc {
            Loc::Dir(_) => {
                if ui
                    .add(egui::Button::new("New Folder").shortcut_text("Ctrl+Shift+N"))
                    .clicked()
                {
                    self.new_folder();
                }
                let can_paste = self.clip.is_some();
                if ui
                    .add_enabled(can_paste, egui::Button::new("Paste").shortcut_text("Ctrl+V"))
                    .clicked()
                {
                    self.paste(None, &ctx);
                }
                if ui.button("Open Terminal Here").clicked() {
                    self.terminal_here(None);
                }
                let here = self.loc.dir().map(Path::to_path_buf);
                let listed = self.places.iter().any(|p| p.path == here);
                if ui
                    .add_enabled(!listed, egui::Button::new("Add to Sidebar").shortcut_text("Ctrl+D"))
                    .clicked()
                {
                    self.add_to_sidebar(here.into_iter().collect());
                }
            }
            Loc::Trash => {
                if ui
                    .add_enabled(self.trash_full, egui::Button::new("Empty Trash…"))
                    .clicked()
                {
                    self.confirm = Some(Confirm::EmptyTrash);
                }
            }
        }
        if ui
            .add(egui::Button::new("Select All").shortcut_text("Ctrl+A"))
            .clicked()
        {
            self.select_all();
        }
        ui.separator();
        let mut hidden = self.show_hidden;
        // Indented to line up with the buttons' text.
        let toggled = ui
            .horizontal(|ui| {
                ui.add_space(ui.spacing().button_padding.x - ui.spacing().item_spacing.x);
                ui.checkbox(&mut hidden, "Show Hidden Files")
                    .on_hover_text("Ctrl+H")
                    .changed()
            })
            .inner;
        if toggled {
            self.show_hidden = hidden;
            self.refresh();
            ui.close();
        }
        if ui.add(egui::Button::new("Reload").shortcut_text("F5")).clicked() {
            self.reload();
        }
    }

    /// What can be done with the selected files: right-click on one.
    fn file_menu(&mut self, ui: &mut Ui) {
        wt_ui::roomy_menu(ui);
        let ctx = ui.ctx().clone();
        let sel = self.selection();
        let Some(first) = sel.first().cloned() else {
            ui.close();
            return;
        };
        if self.loc == Loc::Trash {
            if ui.button("Restore").clicked() {
                self.restore_selected();
            }
            if ui
                .add(egui::Button::new("Delete Permanently…").shortcut_text("Delete"))
                .clicked()
            {
                self.confirm = Some(Confirm::DeleteFromTrash(self.selected_paths()));
            }
            return;
        }

        let all_dirs = sel.iter().all(|e| e.is_dir);
        let mime = open::type_of(&self.mime, &first.path);
        let default = (!all_dirs).then(|| self.apps.default_for(&mime).cloned()).flatten();
        let open_label = match (&default, all_dirs) {
            (_, true) => "Open".to_string(),
            (Some(app), _) => format!("Open with {}", app.name),
            (None, _) => "Open".to_string(),
        };
        if ui.add(egui::Button::new(open_label).shortcut_text("Enter")).clicked() {
            self.open_entries(sel.clone());
        }
        if all_dirs {
            let new = sel.iter().any(|e| !self.places.iter().any(|p| p.path.as_ref() == Some(&e.path)));
            if ui
                .add_enabled(new, egui::Button::new("Add to Sidebar").shortcut_text("Ctrl+D"))
                .clicked()
            {
                self.add_to_sidebar(sel.iter().map(|e| e.path.clone()).collect());
            }
        }
        if !all_dirs {
            let apps: Vec<open::App> = self.apps.for_type(&mime).into_iter().cloned().collect();
            ui.menu_button("Open With", |ui| {
                wt_ui::roomy_menu(ui);
                for app in &apps {
                    if self.app_button(ui, app).clicked() {
                        self.open_with(app);
                    }
                }
                if !apps.is_empty() {
                    ui.separator();
                }
                ui.menu_button("Other App", |ui| {
                    wt_ui::roomy_menu(ui);
                    let all: Vec<open::App> = self.apps.all().into_iter().cloned().collect();
                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        for app in &all {
                            if self.app_button(ui, app).clicked() {
                                self.open_with(app);
                                ui.close();
                            }
                        }
                    });
                });
            });
        }
        // Search results live elsewhere.
        if let Some(parent) = first.path.parent().filter(|d| Some(*d) != self.loc.dir())
            && sel.len() == 1
            && ui.button("Open Containing Folder").clicked()
        {
            let (parent, path) = (parent.to_path_buf(), first.path.clone());
            self.open(Loc::Dir(parent), true);
            if let Some(i) = self.shown.iter().position(|e| e.path == path) {
                self.select_only(i);
            }
        }
        if all_dirs && sel.len() == 1 && ui.button("Open Terminal Here").clicked() {
            self.terminal_here(Some(first.path.clone()));
        }
        ui.separator();
        if ui.add(egui::Button::new("Cut").shortcut_text("Ctrl+X")).clicked() {
            self.copy(true, &ctx);
        }
        if ui.add(egui::Button::new("Copy").shortcut_text("Ctrl+C")).clicked() {
            self.copy(false, &ctx);
        }
        if sel.len() == 1 && ui.add(egui::Button::new("Rename…").shortcut_text("F2")).clicked() {
            self.start_rename(first.path.clone());
        }
        ui.separator();
        if ui
            .add(egui::Button::new("Move to Trash").shortcut_text("Delete"))
            .clicked()
        {
            self.trash_selected(&ctx);
        }
        if ui
            .add(egui::Button::new("Delete Permanently…").shortcut_text("Shift+Delete"))
            .clicked()
        {
            self.confirm = Some(Confirm::Delete(self.selected_paths()));
        }
    }

    fn app_button(&mut self, ui: &mut Ui, app: &open::App) -> egui::Response {
        let size = self.look.theme.font_size + 2.0;
        match self.icons.named(ui.ctx(), &app.icon, size) {
            Some(t) => ui.add(egui::Button::image_and_text(
                egui::Image::new(&t).fit_to_exact_size(vec2(size, size)),
                &app.name,
            )),
            None => ui.button(&app.name),
        }
    }

    fn list(&mut self, ui: &mut Ui, p: &Palette) {
        let width = ui.available_width();
        let row_h = self.row_h();
        let small = TextStyle::Small.resolve(ui.style());
        let body = TextStyle::Body.resolve(ui.style());
        // Folders stand out by their names in semi-bold.
        let bold = wt_ui::semibold(body.size);
        let name_font = |e: &Entry| if e.is_dir { bold.clone() } else { body.clone() };
        let size_w = 100.0;
        let date_w = 130.0;
        let name_w = (width - size_w - date_w).max(120.0);
        let radius = ui.visuals().widgets.inactive.corner_radius;
        // Files dragged over a folder row go into that folder (not in the trash, whose folders
        // are trashed ones).
        let drop_at = self
            .drop_hover
            .filter(|at| self.loc != Loc::Trash && ui.clip_rect().contains(*at));
        let mut drop_folder = None;

        let date_title = if self.loc == Loc::Trash { "Deleted" } else { "Modified" };
        let columns = [
            (SortBy::Name, "Name", name_w, false),
            (SortBy::Size, "Size", size_w, true),
            (SortBy::Modified, date_title, date_w, true),
        ];
        if view::sort_header(
            ui,
            p,
            &columns,
            &mut self.sort,
            &mut self.descending,
            false,
        ) {
            self.refresh();
        }

        // The empty space: a click clears the selection, a right-click offers what can be done
        // here. Added first, so the rows on top of it get their own clicks.
        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("background"),
            Sense::click(),
        );
        if bg.clicked() {
            self.selected.clear();
            self.renaming = None;
        }
        bg.context_menu(|ui| self.background_menu(ui));

        if self.shown.is_empty() {
            let msg = if let Some(e) = &self.error {
                e.clone()
            } else if !self.search.is_empty() {
                if self.searching.is_some() {
                    "Searching…"
                } else {
                    "Nothing found"
                }
                .into()
            } else {
                match self.loc {
                    Loc::Trash => "The trash is empty",
                    Loc::Dir(_) => "This folder is empty",
                }
                .into()
            };
            view::empty_message(ui, p, &msg);
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
        let mut menu_on = None;
        let mut drag_out = None;
        // Files can be dragged out of folders and search results, not the trash.
        let can_drag = self.dnd.is_some() && self.loc != Loc::Trash;
        let mut rows: Vec<(usize, egui::Response)> = Vec::new();
        let mut rename_done = None;
        let mime = &self.mime;
        let icons = &mut self.icons;
        let shown = &self.shown;
        let selected = &self.selected;
        let dir = self.loc.dir();
        let trash_origin = &self.trash_origin;
        let cut: &[PathBuf] = self.clip.as_ref().filter(|c| c.cut).map_or(&[], |c| &c.paths);
        let renaming = &mut self.renaming;
        let out = area.show_rows(ui, row_h, shown.len(), |ui, range| {
            for i in range {
                let e = &shown[i];
                let sense = if can_drag { Sense::click_and_drag() } else { Sense::click() };
                let (rect, r) = ui.allocate_exact_size(vec2(width, row_h), sense);
                let is_sel = selected.contains(&e.path);
                if is_sel {
                    ui.painter().rect_filled(rect, radius, color(p.selected));
                } else if r.hovered() {
                    ui.painter().rect_filled(rect, radius, color(p.hover));
                }
                if e.is_dir && drop_at.is_some_and(|at| rect.contains(at) && ui.clip_rect().contains(at)) {
                    if !is_sel {
                        ui.painter().rect_filled(rect, radius, color(p.hover));
                    }
                    let edge = Stroke::new(1.5, color(p.accent));
                    ui.painter().rect_stroke(rect, radius, edge, egui::StrokeKind::Inside);
                    drop_folder = Some(e.path.clone());
                }
                // Files waiting to be moved by a paste are dimmed.
                let dim = cut.contains(&e.path) || e.hidden;
                let icon_rect =
                    Rect::from_center_size(pos2(rect.left() + 10.0 + ICON / 2.0, rect.center().y), vec2(ICON, ICON));
                if let Some(t) = icons.file(ui.ctx(), mime, e, p.icon_outline) {
                    let tint = if dim {
                        Color32::from_white_alpha(140)
                    } else {
                        Color32::WHITE
                    };
                    paint_image(ui, &t, view::file_icon_rect(ui.ctx(), &t, icon_rect.center()), tint);
                }
                let name_cell = Rect::from_min_max(
                    pos2(icon_rect.right() + 10.0, rect.top()),
                    pos2(rect.left() + name_w, rect.bottom()),
                );
                match renaming.as_mut().filter(|rn| rn.path == e.path) {
                    Some(rn) => {
                        // Placed so the name being edited stays where it was drawn.
                        let pad = wt_ui::side_padding(ui.style());
                        let field = Rect::from_min_max(
                            pos2(name_cell.left() - pad, rect.top() + 3.0),
                            pos2(name_cell.right() - 8.0, rect.bottom() - 3.0),
                        );
                        // In a child of its own, so it can't push the rows below.
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
                        let edit = wt_ui::field(&child, egui::TextEdit::singleline(&mut rn.name))
                            .min_size(field.size());
                        let te = child.add_sized(field.size(), edit);
                        if std::mem::take(&mut rn.fresh) {
                            te.request_focus();
                            view::select_stem(ui.ctx(), te.id, &rn.name);
                        } else if te.lost_focus() {
                            rename_done = Some(!ui.input(|i| i.key_pressed(Key::Escape)));
                        }
                    }
                    None => {
                        let text_c = color(if dim { p.subtext } else { p.text });
                        let name_end = clipped_text(ui, name_cell, &e.name, text_c, name_font(e));
                        // Search results and trashed files say where they're from.
                        let elsewhere = match trash_origin.get(&e.path) {
                            Some(orig) => orig.parent(),
                            None => e.path.parent().filter(|d| Some(*d) != dir),
                        };
                        if let Some(folder) = elsewhere {
                            let rest = Rect::from_min_max(pos2(name_end + 10.0, rect.top()), name_cell.right_bottom());
                            if rest.width() > 40.0 {
                                clipped_text(ui, rest, &view::short_dir(folder), color(p.subtext), small.clone());
                            }
                        }
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
                } else if r.secondary_clicked() {
                    menu_on = Some(i);
                } else if r.drag_started_by(PointerButton::Primary) {
                    drag_out = Some(i);
                }
                rows.push((i, r));
            }
        });
        self.list_view = (out.state.offset.y, out.inner_rect.height());
        if let Some(f) = drop_folder {
            self.drop_target = Some(DropTarget::Folder(f));
        }
        match rename_done {
            Some(true) => self.finish_rename(),
            Some(false) => self.renaming = None,
            None => {}
        }
        if let Some(i) = clicked {
            if self.renaming.is_some() {
                self.finish_rename();
            }
            self.click(i, ctrl, shift);
        }
        if let Some(i) = activated {
            self.select_only(i);
            let e = self.shown[i].clone();
            self.open_entries(vec![e]);
        }
        // Right-clicking a file outside the selection picks just that file.
        if let Some(i) = menu_on
            && !self.selected.contains(&self.shown[i].path)
        {
            self.select_only(i);
        }
        for (_, r) in rows {
            r.context_menu(|ui| self.file_menu(ui));
        }
        if let Some(i) = drag_out {
            self.drag_out(i, ui.ctx());
        }
    }

    /// Start dragging files out: the selection if row `i` is in it, else just that row.
    fn drag_out(&mut self, i: usize, ctx: &egui::Context) {
        if self.renaming.is_some() {
            self.finish_rename();
        }
        if !self.selected.contains(&self.shown[i].path) {
            self.select_only(i);
        }
        let paths = self.selected_paths();
        let Some(dnd) = &self.dnd else { return };
        // The dragged row's icon under the pointer, at the window's scale (whole pixels).
        let scale = ctx.pixels_per_point().ceil().max(1.0) as i32;
        let icon = self
            .icons
            .file_pixels(&self.mime, &self.shown[i], DRAG_ICON * scale as u32);
        if dnd.start_drag(&paths, icon.as_deref(), scale) {
            self.release_pending = true;
        } else {
            tracing::warn!("couldn't start a drag: no button press to start it from");
        }
    }

    fn status(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            let small = |t: &str, c| RichText::new(t).small().color(color(c));
            if let Some((text, err, _)) = self.message.clone() {
                ui.label(small(&text, if err { p.urgent } else { p.accent }));
                if err && ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                    self.message = None;
                }
            } else {
                let sel = self.selection();
                let text = if sel.is_empty() {
                    plural(self.shown.len(), "item")
                } else {
                    let bytes: u64 = sel.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
                    let size = if bytes > 0 {
                        format!(" · {}", fs::size_text(bytes))
                    } else {
                        String::new()
                    };
                    format!("{} selected{size}", sel.len())
                };
                ui.label(small(&text, p.muted));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(job) = &self.job {
                    if ui.small_button("Cancel").clicked() {
                        job.cancel();
                    }
                    if let Some(f) = job.progress() {
                        ui.add(egui::ProgressBar::new(f).desired_width(120.0).desired_height(6.0));
                    }
                    let waiting = if self.queue.is_empty() {
                        String::new()
                    } else {
                        format!(" (+{} waiting)", self.queue.len())
                    };
                    ui.label(small(
                        &format!("{} {}…{waiting}", job.kind.verb(), plural(job.count, "item")),
                        p.text,
                    ));
                } else if let Some(free) = self.free_space {
                    ui.label(small(&format!("{} free", fs::size_text(free)), p.subtext));
                }
            });
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(c) = &self.confirm else { return };
        let (title, body, button) = match c {
            Confirm::Delete(paths) | Confirm::DeleteFromTrash(paths) => (
                if paths.len() == 1 {
                    format!(
                        "Delete “{}” for good?",
                        paths[0].file_name().unwrap_or_default().to_string_lossy()
                    )
                } else {
                    format!("Delete {} for good?", plural(paths.len(), "item"))
                },
                "This can't be undone.",
                "Delete",
            ),
            Confirm::EmptyTrash => (
                "Empty the trash?".to_string(),
                "Everything in it is deleted for good.",
                "Empty Trash",
            ),
        };
        let mut answer = None;
        let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(Margin::same(18));
        let modal = egui::Modal::new(egui::Id::new("confirm")).frame(frame).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new(&title).strong().color(color(p.text)));
            ui.add_space(4.0);
            ui.label(RichText::new(body).color(color(p.muted)));
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let go = egui::Button::new(RichText::new(button).color(color(p.on_accent))).fill(color(p.urgent));
                if ui.add(go).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if modal.should_close() && answer.is_none() {
            answer = Some(false);
        }
        if let Some(yes) = answer
            && let Some(c) = self.confirm.take()
            && yes
        {
            self.confirmed(c, ctx);
        }
    }
}

/// "1 item", "3 items".
/// Size of the icon under the pointer while dragging files out, in window units.
const DRAG_ICON: u32 = 32;

/// The sidebar's places: the picker's, less Recent (the file manager only shows folders).
fn places() -> Vec<Place> {
    fs::places().into_iter().filter(|p| p.path.is_some()).collect()
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Free space on the drive holding `dir`.
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills our own struct.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut s) == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
    }
}

/// Files in a dropped `text/uri-list`: file:// URIs that exist (others, like web links, are
/// left out).
fn uris_to_paths(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(fs::uri_to_path)
        .filter(|p| p.symlink_metadata().is_ok())
        .collect()
}

/// Files named in clipboard text: one path or file:// URI per line, all existing. GNOME's
/// "copy"/"cut" first line is understood too. Returns the paths and whether they were cut.
fn paths_in_text(text: &str) -> Option<(Vec<PathBuf>, bool)> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    let cut = match lines.peek() {
        Some(&"cut") => {
            lines.next();
            true
        }
        Some(&"copy") => {
            lines.next();
            false
        }
        _ => false,
    };
    let paths: Vec<PathBuf> = lines
        .map(|l| {
            if l.starts_with("file://") {
                fs::uri_to_path(l)
            } else {
                Some(fs::expand(l)).filter(|p| p.is_absolute())
            }
        })
        .collect::<Option<_>>()?;
    (!paths.is_empty() && paths.iter().all(|p| p.symlink_metadata().is_ok())).then_some((paths, cut))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_text() {
        assert_eq!(paths_in_text("/").unwrap(), (vec![PathBuf::from("/")], false));
        assert_eq!(
            paths_in_text("cut\nfile:///tmp\n").unwrap(),
            (vec![PathBuf::from("/tmp")], true)
        );
        assert!(paths_in_text("hello world").is_none());
        assert!(paths_in_text("/no/such/file/here").is_none());
        assert!(paths_in_text("").is_none());
    }

    #[test]
    fn uri_lists() {
        assert_eq!(
            uris_to_paths("# from somewhere\r\nfile:///tmp\r\nhttps://example.com/\r\nfile:///no/such/file\r\n"),
            vec![PathBuf::from("/tmp")]
        );
        assert!(uris_to_paths("").is_empty());
    }
}
