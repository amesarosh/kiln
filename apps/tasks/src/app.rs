//! The window: a sidebar with the pages and a short summary, the process list, and the
//! performance graphs.

use std::{
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
    egui::{self, Align, Align2, Color32, Key, Layout, Rect, RichText, Sense, TextStyle, Ui, pos2, vec2},
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
            Column::Threads => 70.0,
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
        ui.add_space(12.0);
        ui.label(RichText::new("Task Manager").heading().color(color(p.text)));
        ui.add_space(8.0);
        for page in Page::ALL {
            if nav_item(ui, p, page.title(), self.page == *page).clicked() {
                self.page = *page;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(12.0);
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
            small(ui, p, "Update speed");
            ui.add_space(10.0);
            let s = &self.snap;
            let load = s.load;
            small(ui, p, &format!("Load {:.2}  {:.2}  {:.2}", load[0], load[1], load[2]));
            small(ui, p, &format!("Up {}", duration(s.uptime_secs)));
            small(
                ui,
                p,
                &format!(
                    "Memory {:.0}%  ({} of {})",
                    percent(s.mem_used, s.mem_total),
                    bytes(s.mem_used),
                    bytes(s.mem_total)
                ),
            );
            small(ui, p, &format!("CPU {:.0}%", s.cpu));
        });
    }

    fn processes(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search by name, command or PID")
                    .desired_width(300.0),
            );
            ui.checkbox(&mut self.all_users, "All users and system")
                .on_hover_text("Also list other users' processes and kernel threads");
        });
        ui.add_space(8.0);

        let list = self.visible();
        let pids: Vec<i32> = list.iter().map(|p| p.pid).collect();
        self.keyboard(ui, &pids);

        // Column header.
        let row_h = ui.spacing().interact_size.y;
        let width = ui.available_width();
        let fixed: f32 = Column::ALL.iter().map(|c| c.width()).sum();
        let name_w = (width - fixed).max(160.0);
        let col_w = |c: Column| if c == Column::Name { name_w } else { c.width() };
        let (header, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
        let mut x = header.left();
        for col in Column::ALL {
            let cell = Rect::from_min_size(pos2(x, header.top()), vec2(col_w(*col), row_h));
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
            let c = color(if r.hovered() || self.sort == *col { p.text } else { p.muted });
            // The sorted column gets a small arrow, drawn (not every font has the glyphs).
            let arrow = 12.0;
            let text_cell = if self.sort == *col {
                let (text_cell, mark) = if col.right_aligned() {
                    let mut t = cell;
                    t.set_right(cell.right() - arrow);
                    (t, Rect::from_min_size(pos2(cell.right() - arrow - 6.0, cell.top()), vec2(arrow, row_h)))
                } else {
                    let font = TextStyle::Button.resolve(ui.style());
                    let w = ui.fonts_mut(|f| f.layout_no_wrap(col.title().into(), font, c).size().x);
                    let x = cell.left() + 8.0 + w + 6.0;
                    (cell, Rect::from_min_size(pos2(x, cell.top()), vec2(arrow, row_h)))
                };
                let m = mark.center();
                let d = if self.descending { 1.0 } else { -1.0 };
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![pos2(m.x - 4.0, m.y - 2.0 * d), pos2(m.x + 4.0, m.y - 2.0 * d), pos2(m.x, m.y + 3.0 * d)],
                    c,
                    egui::Stroke::NONE,
                ));
                text_cell
            } else {
                cell
            };
            cell_text(ui, text_cell, col.title(), col.right_aligned(), c, TextStyle::Button);
        }
        ui.painter().line_segment(
            [header.left_bottom(), header.right_bottom()],
            egui::Stroke::new(1.0, color(p.separator)),
        );

        let mut clicked = None;
        let mut end = None;
        let mem_total = self.snap.mem_total.max(1) as f32;
        let selected = self.selected;
        let rows_h = ui.available_height() - row_h - 16.0;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .max_height(rows_h)
            .show_rows(ui, row_h, list.len(), |ui, range| {
                for proc in &list[range] {
                    let (rect, r) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                    let is_sel = selected == Some(proc.pid);
                    let radius = ui.visuals().widgets.noninteractive.corner_radius;
                    if is_sel {
                        ui.painter().rect_filled(rect, radius, color(p.selected));
                    } else if r.hovered() {
                        ui.painter().rect_filled(rect, radius, color(p.hover));
                    }
                    let mut x = rect.left();
                    for col in Column::ALL {
                        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(col_w(*col), row_h));
                        x += cell.width();
                        // A faint bar behind CPU and memory, so heavy processes stand out.
                        let share = match col {
                            Column::Cpu => proc.cpu / 100.0,
                            Column::Memory => proc.memory as f32 / mem_total,
                            _ => 0.0,
                        };
                        let mut bar = cell.shrink2(vec2(4.0, 3.0));
                        let bar_w = bar.width() * share.clamp(0.0, 1.0);
                        // Thinner than this reads as a stray line, not a bar.
                        if bar_w >= 4.0 {
                            bar.set_left(bar.right() - bar_w);
                            ui.painter().rect_filled(bar, radius, color(p.accent).gamma_multiply(0.3));
                        }
                        let text = match col {
                            Column::Name => proc.name.clone(),
                            Column::Pid => proc.pid.to_string(),
                            Column::User => proc.user.clone(),
                            Column::Cpu => format!("{:.1}%", proc.cpu),
                            Column::Memory => bytes(proc.memory),
                            Column::Threads => proc.threads.to_string(),
                        };
                        let c = if *col == Column::Name || is_sel { p.text } else { p.muted };
                        cell_text(ui, cell, &text, col.right_aligned(), color(c), TextStyle::Body);
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
                        if ui.button("Force quit").clicked() {
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
            });
        let count = list.len();
        if let Some(pid) = clicked {
            self.selected = Some(pid);
        }
        if let Some((pid, force)) = end {
            self.request_end(pid, force);
        }

        // Footer: count, the last message, and the buttons for the selected process.
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let sel = self.selected.and_then(|pid| self.find(pid)).map(|p| (p.pid, p.pid == self.my_pid));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = sel.is_some_and(|(_, me)| !me);
                if ui
                    .add_enabled(enabled, egui::Button::new("Force quit"))
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
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.set_max_width(900.0);
            let w = ui.available_width() - 32.0;

            card(ui, p, "CPU", &format!("{:.0}%", s.cpu), |ui| {
                graph::draw(ui, p, vec2(w, 140.0), 100.0, &[Line { series: &h.cpu, color: accent, fill: true }]);
                ui.add_space(10.0);
                // Each core: a label, its use and a meter, in as many columns as fit.
                let cell_w = 150.0;
                let cols = ((w + 8.0) / (cell_w + 8.0)).floor().max(1.0) as usize;
                egui::Grid::new("cores").spacing(vec2(8.0, 8.0)).show(ui, |ui| {
                    for (i, v) in s.cores.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.set_width(cell_w);
                            ui.horizontal(|ui| {
                                small(ui, p, &format!("Core {i}"));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    small(ui, p, &format!("{v:.0}%"));
                                });
                            });
                            graph::meter(ui, p, cell_w, v / 100.0);
                        });
                        if (i + 1) % cols == 0 {
                            ui.end_row();
                        }
                    }
                });
            });

            card(
                ui,
                p,
                "Memory",
                &format!("{} of {}", bytes(s.mem_used), bytes(s.mem_total)),
                |ui| {
                    let mut lines = vec![Line { series: &h.memory, color: accent, fill: true }];
                    if s.swap_total > 0 {
                        lines.push(Line { series: &h.swap, color: second, fill: false });
                    }
                    graph::draw(ui, p, vec2(w, 120.0), 100.0, &lines);
                    ui.horizontal(|ui| {
                        legend(ui, accent, &format!("In use {:.0}%", percent(s.mem_used, s.mem_total)));
                        if s.swap_total > 0 {
                            legend(
                                ui,
                                second,
                                &format!("Swap {} of {}", bytes(s.swap_used), bytes(s.swap_total)),
                            );
                        } else {
                            small(ui, p, "No swap");
                        }
                    });
                },
            );

            rate_card(ui, p, w, "Disk", ("Read", &h.disk_read, s.disk_read), ("Write", &h.disk_write, s.disk_write));
            rate_card(ui, p, w, "Network", ("Received", &h.net_rx, s.net_rx), ("Sent", &h.net_tx, s.net_tx));
            ui.add_space(16.0);
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
                    .inner_margin(egui::Margin::symmetric(12, 0)),
            )
            .show(ui, |ui| self.sidebar(ui, &p));
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(color(p.base))
                    .inner_margin(egui::Margin::symmetric(20, 12)),
            )
            .show(ui, |ui| {
                ui.label(RichText::new(self.page.title()).heading().color(color(p.text)));
                ui.add_space(8.0);
                match self.page {
                    Page::Processes => self.processes(ui, &p),
                    Page::Performance => self.performance(ui, &p),
                }
            });
        let ctx = ui.ctx().clone();
        self.confirm_dialog(&ctx, &p);
    }
}

/// A card with a title on the left and the current value on the right.
fn card(ui: &mut Ui, p: &Palette, title: &str, value: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(12.0);
    egui::Frame::new()
        .fill(color(p.surface))
        .corner_radius(ui.visuals().widgets.noninteractive.corner_radius)
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong().color(color(p.text)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(value).color(color(p.muted)));
                });
            });
            ui.add_space(6.0);
            add(ui);
        });
}

/// A card graphing two rates (bytes per second), scaled to the busiest moment shown.
fn rate_card(
    ui: &mut Ui,
    p: &Palette,
    w: f32,
    title: &str,
    a: (&str, &Series, f64),
    b: (&str, &Series, f64),
) {
    let (accent, second) = (color(p.accent), color(p.muted));
    let top = a.1.max().max(b.1.max()).max(1024.0);
    let value = format!("{}/s", bytes((a.2 + b.2) as u64));
    card(ui, p, title, &value, |ui| {
        let r = graph::draw(
            ui,
            p,
            vec2(w, 100.0),
            top,
            &[
                Line { series: a.1, color: accent, fill: true },
                Line { series: b.1, color: second, fill: false },
            ],
        );
        ui.painter().text(
            r.rect.left_top() + vec2(6.0, 4.0),
            Align2::LEFT_TOP,
            format!("{}/s", bytes(top as u64)),
            TextStyle::Small.resolve(ui.style()),
            color(p.subtext),
        );
        ui.horizontal(|ui| {
            legend(ui, accent, &format!("{} {}/s", a.0, bytes(a.2 as u64)));
            legend(ui, second, &format!("{} {}/s", b.0, bytes(b.2 as u64)));
        });
    });
}

/// A colored square and a label, naming a line on a graph.
fn legend(ui: &mut Ui, c: Color32, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, c);
    ui.label(RichText::new(text).small());
    ui.add_space(12.0);
}

fn small(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).small().color(color(p.subtext)));
}

/// Text in a table cell, cut off at the cell's edge.
fn cell_text(ui: &Ui, cell: Rect, text: &str, right: bool, c: Color32, style: TextStyle) {
    let inner = cell.shrink2(vec2(8.0, 0.0));
    let painter = ui.painter().with_clip_rect(inner.intersect(ui.clip_rect()));
    let (pos, align) = if right {
        (inner.right_center(), Align2::RIGHT_CENTER)
    } else {
        (inner.left_center(), Align2::LEFT_CENTER)
    };
    painter.text(pos, align, text, style.resolve(ui.style()), c);
}

/// A sidebar entry: full width, highlighted when it's the open page.
fn nav_item(ui: &mut Ui, p: &Palette, label: &str, selected: bool) -> egui::Response {
    let h = ui.spacing().interact_size.y + 4.0;
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
    ui.painter().text(
        rect.left_center() + vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        TextStyle::Button.resolve(ui.style()),
        color(if selected { p.text } else { p.muted }),
    );
    response
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
}
