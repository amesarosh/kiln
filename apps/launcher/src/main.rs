//! kiln-launcher: type to find an app, Enter to start it.
//!
//! Arrows (or Tab, Ctrl+N/P) move, Enter starts, Escape or clicking elsewhere closes. Text
//! that matches no app runs as a command. Apps launched often come first.

mod apps;
mod search;

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
};

use kiln_ui::{
    Look, color, eframe,
    egui::{self, Key, RichText, Sense, TextureHandle, Ui, vec2},
};

use apps::{App, History};

const HELP: &str = "\
kiln-launcher — find and start apps

USAGE:
    kiln-launcher

Type to search, arrows to move, Enter to start, Escape to close. Text that matches no app
runs as a command.
";

const APP_ID: &str = "kiln-launcher";
/// Rows shown at once.
const ROWS: usize = 8;
const ROW_H: f32 = 46.0;
const ICON: f32 = 32.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([580.0, 78.0 + ROWS as f32 * ROW_H])
        .with_decorations(false)
        .with_resizable(false);
    kiln_ui::run_viewport(APP_ID, "Launcher", viewport, Launcher::new)?;
    Ok(())
}

struct Launcher {
    look: Look,
    apps: Vec<App>,
    history: History,
    query: String,
    /// Indices into `apps`, best first, for the current query.
    matches: Vec<usize>,
    selected: usize,
    /// Scrolled-off rows at the top.
    first: usize,
    icons: kiln_icons::Icons,
    textures: HashMap<String, Option<TextureHandle>>,
    /// The window has had focus; losing it after that closes the launcher.
    had_focus: bool,
}

impl Launcher {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let config = kiln_config::Config::default_path();
        let theme = config
            .as_ref()
            .and_then(|p| kiln_config::Config::read(p).ok())
            .map(|c| c.theme)
            .unwrap_or_else(|| kiln_config::Config::default().theme);
        let mut look = Look::new(&cc.egui_ctx, config);
        let icon_theme = theme.icon_theme.clone();
        look.set(&cc.egui_ctx, theme);
        let mut me = Self {
            look,
            apps: apps::load(),
            history: History::load(),
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            first: 0,
            icons: kiln_icons::Icons::new(&icon_theme),
            textures: HashMap::new(),
            had_focus: false,
        };
        me.refilter();
        me
    }

    /// Match the apps against the query again.
    fn refilter(&mut self) {
        let q = self.query.trim();
        let count = |a: &App| self.history.counts.get(&a.id).copied().unwrap_or(0) as i32;
        let mut scored: Vec<(i32, usize)> = self
            .apps
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let s = if q.is_empty() { Some(0) } else { search::score(q, a) }?;
                // Apps you start often rise, without burying a much better match.
                Some((s + (count(a) * 15).min(300), i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| self.apps[a.1].name.cmp(&self.apps[b.1].name)));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        self.first = 0;
    }

    /// Rows: the matches, then "run as a command" when there's text and nothing matched.
    fn row_count(&self) -> usize {
        self.matches.len() + usize::from(self.run_row())
    }

    fn run_row(&self) -> bool {
        self.matches.is_empty() && !self.query.trim().is_empty()
    }

    fn step(&mut self, by: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + by).rem_euclid(n as isize) as usize;
        if self.selected < self.first {
            self.first = self.selected;
        } else if self.selected >= self.first + ROWS {
            self.first = self.selected + 1 - ROWS;
        }
    }

    /// Start the selected row and close.
    fn launch(&mut self, ctx: &egui::Context, row: usize) {
        let cmd = match self.matches.get(row) {
            Some(&i) => {
                let app = self.apps[i].clone();
                self.history.bump(&app.id);
                match (app.terminal, apps::terminal()) {
                    (true, Some(term)) => format!("{term} -e {}", app.exec),
                    _ => app.exec,
                }
            }
            None if self.run_row() => self.query.trim().to_string(),
            None => return,
        };
        start(&cmd);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn texture(&mut self, ctx: &egui::Context, name: &str, px: u32) -> Option<TextureHandle> {
        if let Some(t) = self.textures.get(name) {
            return t.clone();
        }
        let tint = kiln_ui::color(self.look.theme.palette.text).to_array();
        let tex = self.icons.any(name, px, tint).map(|icon| {
            let image = egui::ColorImage::from_rgba_premultiplied([icon.size as usize; 2], &icon.rgba);
            ctx.load_texture(format!("icon:{name}"), image, egui::TextureOptions::LINEAR)
        });
        self.textures.insert(name.to_string(), tex.clone());
        tex
    }

    fn keys(&mut self, ctx: &egui::Context) {
        let (down, up, enter, escape, page_down, page_up) = ctx.input(|i| {
            let ctrl = i.modifiers.ctrl;
            (
                i.key_pressed(Key::ArrowDown) || (i.key_pressed(Key::Tab) && !i.modifiers.shift) || (ctrl && i.key_pressed(Key::N)),
                i.key_pressed(Key::ArrowUp) || (i.key_pressed(Key::Tab) && i.modifiers.shift) || (ctrl && i.key_pressed(Key::P)),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::PageUp),
            )
        });
        if escape {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if down {
            self.step(1);
        }
        if up {
            self.step(-1);
        }
        if page_down {
            self.step(ROWS as isize);
        }
        if page_up {
            self.step(-(ROWS as isize));
        }
        if enter {
            self.launch(ctx, self.selected);
        }
    }

    fn rows(&mut self, ui: &mut Ui, p: &kiln_config::theme::Palette) {
        let ctx = ui.ctx().clone();
        let px = (ICON * ctx.pixels_per_point()).round() as u32;
        let radius = self.look.theme.radius;
        let end = (self.first + ROWS).min(self.row_count());
        let mut clicked = None;
        ui.spacing_mut().item_spacing.y = 0.0;
        for row in self.first..end {
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            if response.hovered() && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) {
                self.selected = row;
            }
            if response.clicked() {
                clicked = Some(row);
            }
            let painter = ui.painter_at(rect);
            if row == self.selected {
                painter.rect_filled(rect, radius, color(p.selected));
            }
            let icon_rect = egui::Rect::from_min_size(rect.min + vec2(10.0, (ROW_H - ICON) / 2.0), vec2(ICON, ICON));
            let text_x = icon_rect.max.x + 12.0;
            let (title, detail, icon) = match self.matches.get(row) {
                Some(&i) => {
                    let a = &self.apps[i];
                    let detail = if !a.comment.is_empty() { a.comment.clone() } else { a.generic.clone() };
                    (a.name.clone(), detail, a.icon.clone())
                }
                None => (format!("Run “{}”", self.query.trim()), "As a command".into(), String::new()),
            };
            match (!icon.is_empty()).then(|| self.texture(&ctx, &icon, px)).flatten() {
                Some(t) => {
                    painter.image(t.id(), icon_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                }
                None => {
                    painter.rect_filled(icon_rect, radius.min(ICON / 3.0), color(p.hover));
                    let letter: String = title.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                    painter.text(icon_rect.center(), egui::Align2::CENTER_CENTER, letter, egui::FontId::proportional(18.0), color(p.text));
                }
            }
            let size = self.look.theme.font_size;
            let galley_title = painter.layout_no_wrap(title, egui::FontId::proportional(size), color(p.text));
            let has_detail = !detail.is_empty();
            let title_y = if has_detail { rect.min.y + 6.0 } else { rect.center().y - galley_title.size().y / 2.0 };
            painter.galley(egui::pos2(text_x, title_y), galley_title, color(p.text));
            if has_detail {
                let galley = painter.layout(
                    detail,
                    egui::FontId::proportional(size * 0.82),
                    color(p.subtext),
                    rect.max.x - text_x - 10.0,
                );
                // One line only.
                let row_h = galley.rows.first().map_or(0.0, |r| r.rect().height());
                let clip = egui::Rect::from_min_max(egui::pos2(text_x, rect.min.y), egui::pos2(rect.max.x - 10.0, rect.min.y + 8.0 + size * 1.25 + row_h));
                painter.with_clip_rect(clip).galley(egui::pos2(text_x, rect.min.y + 8.0 + size * 1.25), galley, color(p.subtext));
            }
        }
        if self.row_count() == 0 {
            ui.add_space(12.0);
            ui.label(RichText::new("No apps found").color(color(p.subtext)));
        }
        if let Some(row) = clicked {
            self.launch(&ctx, row);
        }
        // Scrolling the list.
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.5 && self.row_count() > ROWS {
            let max_first = self.row_count() - ROWS;
            let steps = (-scroll / ROW_H).round() as isize;
            if steps != 0 {
                self.first = (self.first as isize + steps).clamp(0, max_first as isize) as usize;
                self.selected = self.selected.clamp(self.first, self.first + ROWS - 1);
            }
        }
    }
}

impl eframe::App for Launcher {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.look.poll(ctx);
        // Clicking elsewhere closes it, like a menu.
        match ctx.input(|i| i.viewport().focused) {
            Some(true) => self.had_focus = true,
            Some(false) if self.had_focus => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
        self.keys(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(color(p.surface))
                    .stroke(egui::Stroke::new(1.0, color(p.separator)))
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ui, |ui| {
                let edit = egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search apps, or type a command")
                    .desired_width(f32::INFINITY)
                    .margin(egui::Margin::symmetric(kiln_ui::side_padding(ui.style()) as i8, 12))
                    .lock_focus(true);
                let r = ui.add(edit);
                r.request_focus();
                if r.changed() {
                    self.refilter();
                }
                ui.add_space(8.0);
                self.rows(ui, &p);
            });
    }
}

/// Start a command through the compositor, so it gets the session's environment and may bring
/// its window forward. Without the compositor's socket, start it directly.
fn start(cmd: &str) {
    if let Some(path) = std::env::var_os("KILN_SOCKET").filter(|p| !p.is_empty())
        && let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&path)
    {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        if writeln!(stream, "spawn {cmd}").is_ok() {
            let mut reply = String::new();
            let _ = BufReader::new(stream).read_line(&mut reply);
            if reply.trim() == "ok" {
                return;
            }
            tracing::warn!("compositor said: {}", reply.trim());
        }
    }
    use std::os::unix::process::CommandExt;
    if let Err(e) = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        tracing::warn!("could not start \"{cmd}\": {e}");
    }
}
