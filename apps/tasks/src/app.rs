//! The window: a sidebar with the pages and live mini graphs, the process list, and the
//! performance graphs.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use wt_config::theme::Palette;
use wt_ui::{
    Look, color, eframe,
    egui::{
        self, Align, Align2, Color32, FontId, Key, Layout, Margin, Painter, Rect, RichText, Sense, Stroke,
        TextStyle, TextureHandle, Ui, pos2, vec2,
    },
};

use crate::{
    graph::{self, Line, Series},
    sample::{self, Process, Sampler, Snapshot},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Processes,
    Performance,
}

impl Page {
    const ALL: &[Page] = &[Page::Processes, Page::Performance];

    fn title(self) -> &'static str {
        match self {
            Page::Processes => "Processes",
            Page::Performance => "Performance",
        }
    }

    pub fn from_name(name: &str) -> Option<Page> {
        Page::ALL.iter().copied().find(|p| p.title().eq_ignore_ascii_case(name))
    }
}

/// How often to sample, in milliseconds; 0 pauses.
const SPEEDS: &[(u64, &str)] = &[(500, "Fast (0.5 s)"), (1000, "Normal (1 s)"), (3000, "Slow (3 s)"), (0, "Paused")];

/// Size of the app icons in the process list, in points.
const ICON: f32 = 18.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Name,
    Pid,
    User,
    Cpu,
    Memory,
    Threads,
}

impl Column {
    const ALL: &[Column] = &[
        Column::Name,
        Column::User,
        Column::Pid,
        Column::Cpu,
        Column::Memory,
        Column::Threads,
    ];

    fn title(self) -> &'static str {
        match self {
            Column::Name => "Name",
            Column::Pid => "PID",
            Column::User => "User",
            Column::Cpu => "CPU",
            Column::Memory => "Memory",
            Column::Threads => "Threads",
        }
    }

    /// Fixed width; the name takes what's left.
    fn width(self) -> f32 {
        match self {
            Column::Name => 0.0,
            Column::Pid => 90.0,
            Column::User => 110.0,
            Column::Cpu => 80.0,
            Column::Memory => 100.0,
            Column::Threads => 76.0,
        }
    }

    /// Numbers line up on the right.
    fn right_aligned(self) -> bool {
        !matches!(self, Column::Name | Column::User)
    }

    /// The order a first click sorts in: big numbers first, names A to Z.
    fn descending_first(self) -> bool {
        matches!(self, Column::Cpu | Column::Memory | Column::Threads)
    }

    fn compare(self, a: &Process, b: &Process) -> std::cmp::Ordering {
        match self {
            Column::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            Column::Pid => a.pid.cmp(&b.pid),
            Column::User => a.user.cmp(&b.user),
            Column::Cpu => a.cpu.total_cmp(&b.cpu),
            Column::Memory => a.memory.cmp(&b.memory),
            Column::Threads => a.threads.cmp(&b.threads),
        }
    }
}

#[derive(Default)]
struct History {
    cpu: Series,
    cores: Vec<Series>,
    memory: Series,
    swap: Series,
    disk_read: Series,
    disk_write: Series,
    net_rx: Series,
    net_tx: Series,
}

impl History {
    fn push(&mut self, s: &Snapshot) {
        self.cpu.push(s.cpu);
        self.cores.resize_with(s.cores.len(), Series::default);
        for (series, v) in self.cores.iter_mut().zip(&s.cores) {
            series.push(*v);
        }
        self.memory.push(percent(s.mem_used, s.mem_total));
        self.swap.push(percent(s.swap_used, s.swap_total));
        self.disk_read.push(s.disk_read as f32);
        self.disk_write.push(s.disk_write as f32);
        self.net_rx.push(s.net_rx as f32);
        self.net_tx.push(s.net_tx as f32);
    }
}

/// A process the user asked to end that needs a second look first.
struct Confirm {
    pid: i32,
    name: String,
    force: bool,
    why: &'static str,
}

/// App icons for process names, found once and kept as textures.
struct ProcessIcons {
    icons: wt_icons::Icons,
    /// The icon theme `icons` was made for.
    theme: String,
    textures: HashMap<String, Option<TextureHandle>>,
}

impl ProcessIcons {
    fn new(theme: &str) -> Self {
        Self {
            icons: wt_icons::Icons::new(theme),
            theme: theme.to_string(),
            textures: HashMap::new(),
        }
    }

    fn get(&mut self, ctx: &egui::Context, proc: &Process) -> Option<TextureHandle> {
        if proc.kernel {
            return None;
        }
        if let Some(t) = self.textures.get(&proc.name) {
            return t.clone();
        }
        let px = (ICON * ctx.pixels_per_point()).round() as u32;
        let tex = icon_names(proc)
            .iter()
            .find_map(|name| self.icons.get(name, px))
            .map(|icon| {
                let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
                ctx.load_texture(format!("icon:{}", proc.name), image, egui::TextureOptions::LINEAR)
            });
        self.textures.insert(proc.name.clone(), tex.clone());
        tex
    }
}

/// Names a process's icon might go by: its own name, without a "-bin" style suffix, and the
/// folder its program lives in (/opt/vivaldi/vivaldi-bin is "vivaldi").
fn icon_names(proc: &Process) -> Vec<String> {
    if matches!(proc.name.as_str(), "bash" | "zsh" | "fish" | "sh" | "dash" | "nu" | "tmux") {
        return vec!["utilities-terminal".into()];
    }
    let mut names = vec![proc.name.clone()];
    for suffix in ["-bin", "-wrapped", ".bin", "-real"] {
        if let Some(short) = proc.name.strip_suffix(suffix) {
            names.push(short.to_string());
        }
    }
    let program = proc.cmdline.split_whitespace().next().unwrap_or("");
    let path = std::path::Path::new(program);
    if let Some(file) = path.file_name().and_then(|f| f.to_str()) {
        names.push(file.to_string());
    }
    if let Some(dir) = path.parent().and_then(|d| d.file_name()).and_then(|d| d.to_str())
        && !matches!(dir, "bin" | "sbin" | "libexec" | "lib" | "usr" | "local")
    {
        names.push(dir.to_string());
    }
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| !n.is_empty() && seen.insert(n.clone()));
    names
}

pub struct TasksApp {
    look: Look,
    page: Page,
    rx: Receiver<Snapshot>,
    interval: Arc<AtomicU64>,
    snap: Snapshot,
    history: History,
    search: String,
    all_users: bool,
    sort: Column,
    descending: bool,
    selected: Option<i32>,
    /// A short note after ending a process: text, when, and whether it went wrong.
    message: Option<(String, Instant, bool)>,
    confirm: Option<Confirm>,
    icons: ProcessIcons,
    /// "AMD Ryzen 5 5600X 6-Core Processor", from /proc/cpuinfo.
    cpu_model: Option<String>,
    my_uid: u32,
    my_pid: i32,
}

impl TasksApp {
    pub fn new(cc: &eframe::CreationContext<'_>, page: Page) -> Self {
        let look = Look::new(&cc.egui_ctx, wt_config::Config::default_path());
        let interval = Arc::new(AtomicU64::new(1000));
        let (tx, rx) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        let every = interval.clone();
        // Reading /proc for every process takes a few milliseconds; keep it off the UI thread.
        std::thread::Builder::new()
            .name("sampler".into())
            .spawn(move || {
                let mut sampler = Sampler::new();
                loop {
                    let ms = every.load(Ordering::Relaxed);
                    if ms == 0 {
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    if tx.send(sampler.sample()).is_err() {
                        return; // the window closed
                    }
                    ctx.request_repaint();
                    std::thread::sleep(Duration::from_millis(ms));
                }
            })
            .expect("could not start the sampling thread");
        let icons = ProcessIcons::new(&look.theme.icon_theme);
        Self {
            look,
            page,
            rx,
            interval,
            snap: Snapshot::default(),
            history: History::default(),
            search: String::new(),
            all_users: false,
            sort: Column::Cpu,
            descending: true,
            selected: None,
            message: None,
            confirm: None,
            icons,
            cpu_model: cpu_model(),
            // SAFETY: getuid can't fail.
            my_uid: unsafe { libc::getuid() },
            my_pid: std::process::id() as i32,
        }
    }

    /// The processes to list, filtered and sorted.
    fn visible(&self) -> Vec<Process> {
        let needle = self.search.to_lowercase();
        let mut list: Vec<Process> = self
            .snap
            .processes
            .iter()
            .filter(|p| self.all_users || (p.uid == self.my_uid && !p.kernel))
            .filter(|p| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.cmdline.to_lowercase().contains(&needle)
                    || p.pid.to_string() == needle
            })
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            let o = self.sort.compare(a, b).then(a.pid.cmp(&b.pid));
            if self.descending { o.reverse() } else { o }
        });
        list
    }

    fn find(&self, pid: i32) -> Option<&Process> {
        self.snap.processes.iter().find(|p| p.pid == pid)
    }

    /// End a process, asking first when that would take the desktop down with it.
    fn request_end(&mut self, pid: i32, force: bool) {
        let Some(p) = self.find(pid) else { return };
        let why = if p.name == "wayland-time" {
            Some("This is the desktop itself: ending it logs you out and closes every app.")
        } else if p.pid == 1 {
            Some("This is the system's first process; ending it shuts the system down.")
        } else if p.name == "Xwayland" {
            Some("Every X11 app (Steam, games, some older programs) closes with it.")
        } else {
            None
        };
        match why {
            Some(why) => {
                self.confirm = Some(Confirm {
                    pid,
                    name: p.name.clone(),
                    force,
                    why,
                })
            }
            None => self.end(pid, force),
        }
    }

    fn end(&mut self, pid: i32, force: bool) {
        let name = self.find(pid).map(|p| p.name.clone()).unwrap_or_else(|| pid.to_string());
        let verb = if force { "Force quit" } else { "Asked to quit" };
        self.message = Some(match sample::signal(pid, force) {
            Ok(()) => (format!("{verb}: {name}"), Instant::now(), false),
            Err(e) => (format!("Couldn't end {name}: {e}"), Instant::now(), true),
        });
    }

    fn sidebar(&mut self, ui: &mut Ui, p: &Palette) {
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
            logo(ui.painter(), p, rect);
            ui.add_space(2.0);
            let size = TextStyle::Body.resolve(ui.style()).size * 1.15;
            ui.label(RichText::new("Task Manager").size(size).strong().color(color(p.text)));
        });
        ui.add_space(14.0);
        for page in Page::ALL {
            if nav_item(ui, p, *page, self.page == *page).clicked() {
                self.page = *page;
            }
            ui.add_space(2.0);
        }

        // Live mini graphs, when there's room for them above the bottom part.
        let tiles_h = 4.0 * (TILE_H + 8.0) + 28.0;
        if ui.available_height() > tiles_h + 120.0 {
            ui.add_space(18.0);
            section_label(ui, p, "Now");
            let s = &self.snap;
            let h = &self.history;
            let (accent, second) = (color(p.accent), color(p.muted));
            let mut open = false;
            open |= tile(ui, p, "CPU", &format!("{:.0}%", s.cpu), 100.0, &[Line {
                series: &h.cpu,
                color: accent,
                fill: true,
            }])
            .clicked();
            open |= tile(ui, p, "Memory", &format!("{:.0}%", percent(s.mem_used, s.mem_total)), 100.0, &[
                Line { series: &h.memory, color: accent, fill: true },
            ])
            .clicked();
            let top = h.disk_read.max().max(h.disk_write.max()).max(1024.0);
            open |= tile(ui, p, "Disk", &rate(s.disk_read + s.disk_write), top, &[
                Line { series: &h.disk_read, color: accent, fill: true },
                Line { series: &h.disk_write, color: second, fill: false },
            ])
            .clicked();
            let top = h.net_rx.max().max(h.net_tx.max()).max(1024.0);
            open |= tile(ui, p, "Network", &rate(s.net_rx + s.net_tx), top, &[
                Line { series: &h.net_rx, color: accent, fill: true },
                Line { series: &h.net_tx, color: second, fill: false },
            ])
            .clicked();
            if open {
                self.page = Page::Performance;
            }
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(14.0);
            let current = self.interval.load(Ordering::Relaxed);
            let label = SPEEDS.iter().find(|s| s.0 == current).map_or("", |s| s.1);
            egui::ComboBox::from_id_salt("speed")
                .selected_text(label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (ms, name) in SPEEDS {
                        if ui.selectable_label(current == *ms, *name).clicked() {
                            self.interval.store(*ms, Ordering::Relaxed);
                        }
                    }
                });
            ui.add_space(2.0);
            section_label(ui, p, "Update speed");
        });
    }

    /// The page title, with the search box and filter on the right for the process list.
    fn header(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            ui.label(RichText::new(self.page.title()).heading().color(color(p.text)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.page {
                Page::Processes => {
                    let id = egui::Id::new("search");
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
                        ui.memory_mut(|m| m.request_focus(id));
                    }
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .id(id)
                            .hint_text("Search  (Ctrl+F)")
                            .margin(Margin { left: 30, right: 10, top: 6, bottom: 6 })
                            .desired_width(240.0),
                    );
                    if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                        self.search.clear();
                        r.surrender_focus();
                    }
                    magnifier(ui.painter(), r.rect.left_center() + vec2(15.0, 0.0), color(p.subtext));
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.all_users, "All users")
                        .on_hover_text("Also list other users' processes and kernel threads");
                }
                Page::Performance => {
                    let up = format!("Up {}", duration(self.snap.uptime_secs));
                    ui.label(RichText::new(up).color(color(p.subtext)));
                }
            });
        });
        ui.add_space(10.0);
    }

    fn processes(&mut self, ui: &mut Ui, p: &Palette) {
        let list = self.visible();
        let pids: Vec<i32> = list.iter().map(|p| p.pid).collect();
        self.keyboard(ui, &pids);

        let row_h = ui.spacing().interact_size.y + 6.0;
        let footer_h = ui.spacing().interact_size.y + 14.0;
        let card_h = (ui.available_height() - footer_h).max(120.0);
        let radius = ui.visuals().widgets.noninteractive.corner_radius;
        let pad = 8.0;

        let mut clicked = None;
        let mut end = None;
        egui::Frame::new()
            .fill(color(p.surface))
            .corner_radius(radius)
            .inner_margin(Margin::same(pad as i8))
            .show(ui, |ui| {
                let width = ui.available_width();
                let fixed: f32 = Column::ALL.iter().map(|c| c.width()).sum();
                // The name gets what's left, up to a point; past that the other columns share
                // the extra, so the numbers don't drift far from the names they belong to.
                let name_w = (width - fixed).clamp(160.0, 320.0);
                let extra = (width - fixed - name_w).max(0.0) / (Column::ALL.len() - 1) as f32;
                let col_w = |c: Column| if c == Column::Name { name_w } else { c.width() + extra };

                // Column header: the column names, with the machine's totals above CPU and memory.
                let small = TextStyle::Small.resolve(ui.style());
                let body = TextStyle::Body.resolve(ui.style());
                let header_h = small.size + body.size + 18.0;
                let (header, _) = ui.allocate_exact_size(vec2(width, header_h), Sense::hover());
                let mut x = header.left();
                for col in Column::ALL {
                    let cell = Rect::from_min_size(pos2(x, header.top()), vec2(col_w(*col), header_h));
                    x += cell.width();
                    let r = ui.interact(cell, ui.id().with(("header", col.title())), Sense::click());
                    if r.clicked() {
                        if self.sort == *col {
                            self.descending = !self.descending;
                        } else {
                            self.sort = *col;
                            self.descending = col.descending_first();
                        }
                    }
                    // Hovering brightens the title (below) rather than filling the cell, which
                    // with round themes would cut into the text.
                    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                    let sorted = self.sort == *col;
                    let c = color(if r.hovered() || sorted { p.text } else { p.subtext });
                    let total = match col {
                        Column::Cpu => Some(format!("{:.0}%", self.snap.cpu)),
                        Column::Memory => Some(format!("{:.0}%", percent(self.snap.mem_used, self.snap.mem_total))),
                        _ => None,
                    };
                    if let Some(total) = total {
                        let at = Rect::from_min_max(cell.min, pos2(cell.right(), cell.top() + body.size + 10.0));
                        cell_text(ui, at, &total, true, Align::BOTTOM, color(p.text), body.clone());
                    }
                    let name_line =
                        Rect::from_min_max(pos2(cell.left(), cell.bottom() - small.size - 8.0), cell.right_bottom());
                    // The sorted column gets a small arrow, drawn (not every font has the glyphs).
                    let arrow = 12.0;
                    let mut text_cell = name_line;
                    if sorted {
                        let font = small.clone();
                        let w = ui.fonts_mut(|f| f.layout_no_wrap(col.title().into(), font, c).size().x);
                        let mx = if col.right_aligned() {
                            text_cell.set_right(name_line.right() - arrow);
                            name_line.right() - arrow - 2.0
                        } else {
                            name_line.left() + 10.0 + w + 8.0
                        };
                        let m = pos2(mx, name_line.center().y);
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
                    cell_text(ui, text_cell, col.title(), col.right_aligned(), Align::Center, c, small.clone());
                }
                ui.painter().line_segment(
                    [header.left_bottom(), header.right_bottom()],
                    Stroke::new(1.0, color(p.separator)),
                );
                ui.add_space(4.0);

                let mem_total = self.snap.mem_total.max(1) as f32;
                let selected = self.selected;
                let my_uid = self.my_uid;
                let icons = &mut self.icons;
                let rows_h = card_h - 2.0 * pad - header_h - 4.0 - ui.spacing().item_spacing.y;
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .max_height(rows_h)
                    .min_scrolled_height(rows_h)
                    .show_rows(ui, row_h, list.len(), |ui, range| {
                        for proc in &list[range] {
                            let (rect, r) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                            let is_sel = selected == Some(proc.pid);
                            if is_sel {
                                ui.painter().rect_filled(rect, radius, color(p.selected));
                            } else if r.hovered() {
                                ui.painter().rect_filled(rect, radius, color(p.hover));
                            }
                            let mine = proc.uid == my_uid && !proc.kernel;
                            let mut x = rect.left();
                            for col in Column::ALL {
                                let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(col_w(*col), row_h));
                                x += cell.width();
                                // Shade CPU and memory by how much is used, so heavy processes
                                // stand out; the root makes small shares visible too.
                                let share = match col {
                                    Column::Cpu => proc.cpu / 100.0,
                                    Column::Memory => proc.memory as f32 / mem_total,
                                    _ => 0.0,
                                };
                                if share > 0.004 {
                                    let heat = (share.sqrt() * 0.7).min(0.6);
                                    ui.painter().rect_filled(
                                        cell.shrink2(vec2(3.0, 4.0)),
                                        radius,
                                        color(p.accent).gamma_multiply(heat),
                                    );
                                }
                                let text = match col {
                                    Column::Name => {
                                        let icon = Rect::from_center_size(
                                            pos2(cell.left() + 10.0 + ICON / 2.0, cell.center().y),
                                            vec2(ICON, ICON),
                                        );
                                        match icons.get(ui.ctx(), proc) {
                                            Some(tex) => {
                                                ui.painter().image(
                                                    tex.id(),
                                                    icon,
                                                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                                    Color32::WHITE,
                                                );
                                            }
                                            None => badge(ui, p, icon, &proc.name),
                                        }
                                        let mut name_cell = cell;
                                        name_cell.set_left(icon.right() - 2.0);
                                        let c = if mine || is_sel { p.text } else { p.muted };
                                        cell_text(ui, name_cell, &proc.name, false, Align::Center, color(c), body.clone());
                                        continue;
                                    }
                                    Column::Pid => proc.pid.to_string(),
                                    Column::User => proc.user.clone(),
                                    Column::Cpu => format!("{:.1}%", proc.cpu),
                                    Column::Memory => bytes(proc.memory),
                                    Column::Threads => proc.threads.to_string(),
                                };
                                let c = if is_sel || share > 0.02 { p.text } else { p.muted };
                                cell_text(ui, cell, &text, col.right_aligned(), Align::Center, color(c), body.clone());
                            }
                            if r.clicked() || r.secondary_clicked() {
                                clicked = Some(proc.pid);
                            }
                            let r = r.on_hover_text(format!("{}\n{}", proc.cmdline, state_name(proc.state)));
                            r.context_menu(|ui| {
                                if ui.button("End process").clicked() {
                                    end = Some((proc.pid, false));
                                    ui.close();
                                }
                                if ui.button(RichText::new("Force quit").color(color(p.urgent))).clicked() {
                                    end = Some((proc.pid, true));
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Copy command line").clicked() {
                                    ui.ctx().copy_text(proc.cmdline.clone());
                                    ui.close();
                                }
                                if ui.button("Copy PID").clicked() {
                                    ui.ctx().copy_text(proc.pid.to_string());
                                    ui.close();
                                }
                            });
                        }
                        if list.is_empty() {
                            ui.add_space(24.0);
                            ui.vertical_centered(|ui| {
                                let text = if self.search.is_empty() {
                                    "No processes".to_string()
                                } else {
                                    format!("Nothing matches \"{}\"", self.search)
                                };
                                ui.label(RichText::new(text).color(color(p.subtext)));
                            });
                        }
                    });
            });
        let count = list.len();
        if let Some(pid) = clicked {
            self.selected = Some(pid);
        }
        if let Some((pid, force)) = end {
            self.request_end(pid, force);
        }

        // Footer: count, the last message, and the buttons for the selected process.
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let sel = self.selected.and_then(|pid| self.find(pid)).map(|p| (p.pid, p.pid == self.my_pid));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = sel.is_some_and(|(_, me)| !me);
                let force = RichText::new("Force quit").color(color(if enabled { p.urgent } else { p.subtext }));
                if ui
                    .add_enabled(enabled, egui::Button::new(force))
                    .on_hover_text("Stop it at once, without letting it save (SIGKILL)")
                    .clicked()
                    && let Some((pid, _)) = sel
                {
                    self.request_end(pid, true);
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("End process"))
                    .on_hover_text("Ask it to quit (SIGTERM). Delete does the same.")
                    .clicked()
                    && let Some((pid, _)) = sel
                {
                    self.request_end(pid, false);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(4.0);
                    match &self.message {
                        Some((text, at, err)) if at.elapsed() < Duration::from_secs(6) => {
                            let c = if *err { p.urgent } else { p.accent };
                            ui.label(RichText::new(text).color(color(c)));
                        }
                        _ => {
                            let total = self.snap.processes.len();
                            let text = if count == total {
                                format!("{total} processes")
                            } else {
                                format!("{count} of {total} processes")
                            };
                            ui.label(RichText::new(text).color(color(p.subtext)));
                        }
                    }
                });
            });
        });
    }

    /// Up and Down move the selection; Delete ends the selected process.
    fn keyboard(&mut self, ui: &Ui, pids: &[i32]) {
        if ui.ctx().egui_wants_keyboard_input() || pids.is_empty() {
            return;
        }
        let (up, down, delete) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Delete),
            )
        });
        let at = self.selected.and_then(|s| pids.iter().position(|p| *p == s));
        if down {
            self.selected = Some(pids[at.map_or(0, |i| (i + 1).min(pids.len() - 1))]);
        }
        if up {
            self.selected = Some(pids[at.map_or(0, |i| i.saturating_sub(1))]);
        }
        if delete && let Some(pid) = self.selected.filter(|p| *p != self.my_pid) {
            self.request_end(pid, false);
        }
    }

    fn performance(&mut self, ui: &mut Ui, p: &Palette) {
        let s = &self.snap;
        let h = &self.history;
        let accent = color(p.accent);
        let second = color(p.muted);
        let cpu_model = self.cpu_model.as_deref();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            // Room for the scroll bar, so it doesn't sit on the cards' edge.
            ui.set_max_width(ui.available_width() - 14.0);
            let cores = format!("{} cores", s.cores.len());
            let subtitle = cpu_model.map_or(cores.clone(), |m| format!("{m}  ·  {cores}"));
            card(ui, p, "CPU", &subtitle, &format!("{:.0}%", s.cpu), |ui| {
                let w = ui.available_width();
                graph::draw(ui, p, vec2(w, 150.0), 100.0, &[Line { series: &h.cpu, color: accent, fill: true }]);
                ui.add_space(12.0);
                let threads: u32 = s.processes.iter().map(|p| p.threads).sum();
                stats(ui, p, &[
                    (Some(accent), "Utilization", format!("{:.0}%", s.cpu)),
                    (None, "Processes", s.processes.len().to_string()),
                    (None, "Threads", threads.to_string()),
                    (None, "Load average", format!("{:.2}  {:.2}  {:.2}", s.load[0], s.load[1], s.load[2])),
                    (None, "Up time", duration(s.uptime_secs)),
                ]);
                ui.add_space(14.0);
                // Each core: a small graph with its name and use, in as many columns as fit.
                let gap = 8.0;
                let cols = ((w + gap) / (130.0 + gap)).floor().max(1.0) as usize;
                let cell_w = (w - gap * (cols - 1) as f32) / cols as f32;
                let small = TextStyle::Small.resolve(ui.style());
                for (row, chunk) in h.cores.chunks(cols).enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for (i, series) in chunk.iter().enumerate() {
                            let n = row * cols + i;
                            let (rect, _) = ui.allocate_exact_size(vec2(cell_w, 52.0), Sense::hover());
                            let painter = ui.painter();
                            graph::paint(painter, p, rect, graph::radius(ui), 100.0, &[Line {
                                series,
                                color: accent,
                                fill: true,
                            }], true);
                            let inner = rect.shrink2(vec2(8.0, 6.0));
                            painter.text(inner.left_top(), Align2::LEFT_TOP, format!("Core {n}"), small.clone(), color(p.subtext));
                            let v = s.cores.get(n).copied().unwrap_or(0.0);
                            painter.text(inner.right_top(), Align2::RIGHT_TOP, format!("{v:.0}%"), small.clone(), color(p.text));
                        }
                    });
                }
            });

            let available = s.mem_total.saturating_sub(s.mem_used);
            card(
                ui,
                p,
                "Memory",
                &format!("{} total", bytes(s.mem_total)),
                &format!("{:.0}%", percent(s.mem_used, s.mem_total)),
                |ui| {
                    let w = ui.available_width();
                    let mut lines = vec![Line { series: &h.memory, color: accent, fill: true }];
                    if s.swap_total > 0 {
                        lines.push(Line { series: &h.swap, color: second, fill: false });
                    }
                    graph::draw(ui, p, vec2(w, 120.0), 100.0, &lines);
                    ui.add_space(12.0);
                    let swap = if s.swap_total > 0 {
                        format!("{} of {}", bytes(s.swap_used), bytes(s.swap_total))
                    } else {
                        "None".to_string()
                    };
                    stats(ui, p, &[
                        (Some(accent), "In use", bytes(s.mem_used)),
                        (None, "Available", bytes(available)),
                        (s.swap_total.gt(&0).then_some(second), "Swap", swap),
                    ]);
                },
            );

            let disk = (&h.disk_read, s.disk_read, &h.disk_write, s.disk_write);
            let net = (&h.net_rx, s.net_rx, &h.net_tx, s.net_tx);
            if ui.available_width() >= 760.0 {
                ui.columns(2, |c| {
                    c[0].spacing_mut().item_spacing.x = 12.0;
                    rate_card(&mut c[0], p, "Disk", "All disks", ("Read", "Write"), disk);
                    rate_card(&mut c[1], p, "Network", "All connections", ("Received", "Sent"), net);
                });
            } else {
                rate_card(ui, p, "Disk", "All disks", ("Read", "Write"), disk);
                rate_card(ui, p, "Network", "All connections", ("Received", "Sent"), net);
            }
            ui.add_space(8.0);
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(c) = &self.confirm else { return };
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("confirm-end")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new(format!("End {}?", c.name)).strong().color(color(p.text)));
            ui.add_space(4.0);
            ui.label(RichText::new(c.why).color(color(p.muted)));
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let end = egui::Button::new(RichText::new("End it").color(color(p.on_accent))).fill(color(p.urgent));
                if ui.add(end).clicked() {
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
        if let Some(yes) = answer {
            let c = self.confirm.take().unwrap();
            if yes {
                self.end(c.pid, c.force);
            }
        }
    }
}

impl eframe::App for TasksApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        if self.icons.theme != self.look.theme.icon_theme {
            self.icons = ProcessIcons::new(&self.look.theme.icon_theme);
        }
        // Only the newest sample matters if several arrived since the last frame.
        while let Ok(snap) = self.rx.try_recv() {
            self.history.push(&snap);
            self.snap = snap;
        }
        if self.selected.is_some_and(|pid| self.find(pid).is_none()) {
            self.selected = None;
        }
        if let Some((_, at, _)) = &self.message {
            // Repaint once more when the note should disappear.
            let left = Duration::from_secs(6).saturating_sub(at.elapsed());
            if !left.is_zero() {
                ctx.request_repaint_after(left);
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        egui::Panel::left("pages")
            .resizable(false)
            .exact_size(220.0)
            .frame(
                egui::Frame::new()
                    .fill(color(p.surface))
                    .inner_margin(Margin::symmetric(12, 0)),
            )
            .show(ui, |ui| self.sidebar(ui, &p));
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(color(p.base))
                    .inner_margin(Margin { left: 24, right: 24, top: 14, bottom: 16 }),
            )
            .show(ui, |ui| {
                self.header(ui, &p);
                match self.page {
                    Page::Processes => self.processes(ui, &p),
                    Page::Performance => self.performance(ui, &p),
                }
            });
        let ctx = ui.ctx().clone();
        self.confirm_dialog(&ctx, &p);
    }
}

/// A card: title and subtitle on the left, the current value large on the right.
fn card(ui: &mut Ui, p: &Palette, title: &str, subtitle: &str, value: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(color(p.surface))
        .corner_radius(ui.visuals().widgets.noninteractive.corner_radius)
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(title).strong().color(color(p.text)));
                    ui.add(egui::Label::new(RichText::new(subtitle).small().color(color(p.subtext))).truncate());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(value).heading().color(color(p.text)));
                });
            });
            ui.add_space(10.0);
            add(ui);
        });
    ui.add_space(12.0);
}

/// A card graphing two rates (bytes per second), scaled to the busiest moment shown.
fn rate_card(
    ui: &mut Ui,
    p: &Palette,
    title: &str,
    subtitle: &str,
    names: (&str, &str),
    (a, a_now, b, b_now): (&Series, f64, &Series, f64),
) {
    let (accent, second) = (color(p.accent), color(p.muted));
    let top = a.max().max(b.max()).max(1024.0);
    card(ui, p, title, subtitle, &rate(a_now + b_now), |ui| {
        let w = ui.available_width();
        let r = graph::draw(ui, p, vec2(w, 100.0), top, &[
            Line { series: a, color: accent, fill: true },
            Line { series: b, color: second, fill: false },
        ]);
        ui.painter().text(
            r.rect.left_top() + vec2(8.0, 6.0),
            Align2::LEFT_TOP,
            rate(top as f64),
            TextStyle::Small.resolve(ui.style()),
            color(p.subtext),
        );
        ui.add_space(12.0);
        stats(ui, p, &[
            (Some(accent), names.0, rate(a_now)),
            (Some(second), names.1, rate(b_now)),
        ]);
    });
}

/// A row of labeled values under a graph; a color marks the line a value belongs to.
fn stats(ui: &mut Ui, p: &Palette, items: &[(Option<Color32>, &str, String)]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(28.0, 10.0);
        for (mark, label, value) in items {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 2.0);
                ui.horizontal(|ui| {
                    if let Some(c) = mark {
                        let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 4.0, *c);
                    }
                    ui.label(RichText::new(*label).small().color(color(p.subtext)));
                });
                ui.label(RichText::new(value).color(color(p.text)));
            });
        }
    });
}

/// Height of a sidebar mini graph.
const TILE_H: f32 = 62.0;

/// A sidebar entry with a name, the current value and a small graph of the last minute.
fn tile(ui: &mut Ui, p: &Palette, name: &str, value: &str, top: f32, lines: &[Line<'_>]) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), TILE_H), Sense::click());
    let radius = ui.visuals().widgets.noninteractive.corner_radius;
    let painter = ui.painter();
    painter.rect_filled(rect, radius, color(if r.hovered() { p.hover } else { p.raised }));
    let inner = rect.shrink2(vec2(14.0, 9.0));
    let small = TextStyle::Small.resolve(ui.style());
    let body = TextStyle::Body.resolve(ui.style());
    let line = pos2(inner.left(), inner.top() + body.size / 2.0);
    painter.text(line, Align2::LEFT_CENTER, name, small, color(p.muted));
    painter.text(pos2(inner.right(), line.y), Align2::RIGHT_CENTER, value, body.clone(), color(p.text));
    let spark = Rect::from_min_max(pos2(inner.left(), inner.top() + body.size + 6.0), inner.right_bottom());
    graph::paint(painter, p, spark, Default::default(), top, lines, false);
    ui.add_space(8.0);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn section_label(ui: &mut Ui, p: &Palette, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new(text).small().color(color(p.subtext)));
    });
}

/// Text in a table cell, cut off at the cell's edge.
fn cell_text(ui: &Ui, cell: Rect, text: &str, right: bool, valign: Align, c: Color32, font: FontId) {
    let inner = cell.shrink2(vec2(10.0, 0.0));
    let painter = ui.painter().with_clip_rect(inner.intersect(ui.clip_rect()));
    let y = match valign {
        Align::Min => inner.top(),
        Align::Center => inner.center().y,
        Align::Max => inner.bottom(),
    };
    let align = Align2([if right { Align::RIGHT } else { Align::LEFT }, valign]);
    let x = if right { inner.right() } else { inner.left() };
    painter.text(pos2(x, y), align, text, font, c);
}

/// A sidebar entry with its icon: full width, highlighted when it's the open page.
fn nav_item(ui: &mut Ui, p: &Palette, page: Page, selected: bool) -> egui::Response {
    let h = ui.spacing().interact_size.y + 10.0;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let bg = if selected {
        Some(p.selected)
    } else if response.hovered() {
        Some(p.hover)
    } else {
        None
    };
    if let Some(bg) = bg {
        ui.painter()
            .rect_filled(rect, ui.visuals().widgets.noninteractive.corner_radius, color(bg));
    }
    let icon = Rect::from_center_size(rect.left_center() + vec2(24.0, 0.0), vec2(16.0, 16.0));
    page_icon(ui.painter(), page, icon, color(if selected { p.accent } else { p.muted }));
    ui.painter().text(
        rect.left_center() + vec2(44.0, 0.0),
        Align2::LEFT_CENTER,
        page.title(),
        TextStyle::Button.resolve(ui.style()),
        color(if selected { p.text } else { p.muted }),
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The pages' icons, drawn so they follow the theme: a list, and a graph.
fn page_icon(painter: &Painter, page: Page, r: Rect, c: Color32) {
    let stroke = Stroke::new(1.6, c);
    match page {
        Page::Processes => {
            for i in 0..3 {
                let y = r.top() + 2.5 + i as f32 * 5.5;
                painter.circle_filled(pos2(r.left() + 1.5, y), 1.5, c);
                painter.line_segment([pos2(r.left() + 5.5, y), pos2(r.right(), y)], stroke);
            }
        }
        Page::Performance => {
            let pts = [(0.0, 0.75), (0.25, 0.75), (0.4, 0.2), (0.6, 0.95), (0.75, 0.5), (1.0, 0.5)];
            let pts = pts.map(|(x, y)| pos2(r.left() + x * r.width(), r.top() + y * r.height()));
            painter.add(egui::Shape::line(pts.to_vec(), stroke));
        }
    }
}

/// The app's mark: an accent square with a pulse line through it.
fn logo(painter: &Painter, p: &Palette, r: Rect) {
    painter.rect_filled(r, 7.0, color(p.accent));
    let inner = r.shrink(6.0);
    let pts = [(0.0, 0.6), (0.3, 0.6), (0.45, 0.1), (0.6, 0.95), (0.72, 0.6), (1.0, 0.6)];
    let pts = pts.map(|(x, y)| pos2(inner.left() + x * inner.width(), inner.top() + y * inner.height()));
    painter.add(egui::Shape::line(pts.to_vec(), Stroke::new(1.8, color(p.on_accent))));
}

/// A search magnifier centered on `c`.
fn magnifier(painter: &Painter, c: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let o = c - vec2(1.5, 1.5);
    painter.circle_stroke(o, 5.0, stroke);
    painter.line_segment([o + vec2(3.6, 3.6), o + vec2(7.5, 7.5)], stroke);
}

/// For a process without an icon: its first letter in a circle.
fn badge(ui: &Ui, p: &Palette, r: Rect, name: &str) {
    let painter = ui.painter();
    painter.circle_filled(r.center(), r.width() / 2.0, color(p.raised));
    painter.circle_stroke(r.center(), r.width() / 2.0 - 0.5, Stroke::new(1.0, color(p.separator)));
    let letter: String = name.chars().find(|c| c.is_alphanumeric()).map_or('?', |c| c.to_ascii_uppercase()).into();
    painter.text(r.center(), Align2::CENTER_CENTER, letter, FontId::proportional(r.height() * 0.55), color(p.muted));
}

/// The CPU's marketing name, without the clutter some vendors add.
fn cpu_model() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("model name"))?;
    let name = line.split_once(':')?.1;
    let name = name.replace("(R)", "").replace("(TM)", "").replace(" CPU", "");
    let name = name.split(" @ ").next()?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(name)
}

/// What a /proc state letter means.
fn state_name(state: char) -> &'static str {
    match state {
        'R' => "Running",
        'S' => "Sleeping",
        'D' => "Waiting for the disk",
        'Z' => "Exited, waiting for its parent (zombie)",
        'T' | 't' => "Stopped",
        'I' => "Idle",
        _ => "Unknown state",
    }
}

fn percent(part: u64, whole: u64) -> f32 {
    if whole == 0 { 0.0 } else { part as f32 / whole as f32 * 100.0 }
}

fn rate(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec as u64))
}

/// A size in bytes, in the largest unit that keeps it above 1.
pub fn bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{m} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(300 * 1024 * 1024), "300 MB");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(59), "0 min");
        assert_eq!(duration(3 * 3600 + 120), "3 h 2 min");
        assert_eq!(duration(2 * 86400 + 5 * 3600), "2 d 5 h");
    }

    #[test]
    fn icon_name_guesses() {
        let proc = |name: &str, cmdline: &str| Process {
            pid: 1,
            name: name.into(),
            cmdline: cmdline.into(),
            user: String::new(),
            uid: 0,
            cpu: 0.0,
            memory: 0,
            threads: 1,
            state: 'S',
            kernel: false,
        };
        assert_eq!(
            icon_names(&proc("vivaldi-bin", "/opt/vivaldi/vivaldi-bin --type=renderer")),
            ["vivaldi-bin", "vivaldi"]
        );
        assert_eq!(icon_names(&proc("code", "/usr/share/code/code")), ["code"]);
        assert_eq!(icon_names(&proc("zsh", "-zsh")), ["utilities-terminal"]);
    }
}
