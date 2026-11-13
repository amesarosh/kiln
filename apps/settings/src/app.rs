//! The window: a sidebar of pages and a search box, the page itself, and the save status.

use std::time::Duration;

use kiln_config::theme::Palette;
use kiln_ui::{
    Look, color, eframe,
    egui::{self, Align, Layout, RichText, Sense, Ui, vec2},
};

use crate::{
    doc::{ConfigDoc, Status},
    pages::{self, Cache, Page, binds::BindsPage, colors::ColorsPage, fields, themes::ThemesPage},
    widgets,
};

pub struct SettingsApp {
    doc: ConfigDoc,
    look: Look,
    page: Page,
    search: String,
    cache: Cache,
    colors: ColorsPage,
    themes: ThemesPage,
    binds: BindsPage,
}

impl SettingsApp {
    pub fn new(cc: &eframe::CreationContext<'_>, path: std::path::PathBuf, page: Page) -> Self {
        let doc = ConfigDoc::open(path);
        let mut look = Look::new(&cc.egui_ctx, None);
        look.set(&cc.egui_ctx, doc.config.theme.clone());
        Self {
            doc,
            look,
            page,
            search: String::new(),
            cache: Cache::default(),
            colors: ColorsPage::default(),
            themes: ThemesPage::default(),
            binds: BindsPage::default(),
        }
    }

    fn sidebar(&mut self, ui: &mut Ui, p: &Palette) {
        ui.add_space(12.0);
        ui.label(RichText::new("Settings").heading().color(color(p.text)));
        ui.add_space(8.0);
        ui.add(widgets::padded(
            ui,
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Search settings")
                .desired_width(ui.available_width()),
        ));
        ui.add_space(8.0);
        for page in Page::ALL {
            let selected = self.page == *page && self.search.is_empty();
            if nav_item(ui, p, page.title(), selected).clicked() {
                self.page = *page;
                self.search.clear();
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(10.0);
            self.status(ui, p);
        });
    }

    fn status(&self, ui: &mut Ui, p: &Palette) {
        let (text, c) = match &self.doc.status {
            Status::Idle => ("Changes apply as you make them".to_string(), p.subtext),
            Status::Pending => ("Saving…".to_string(), p.subtext),
            Status::Saved(t) if t.elapsed() < Duration::from_secs(3) => ("Saved and applied".to_string(), p.accent),
            Status::Saved(_) => ("All changes saved".to_string(), p.subtext),
            Status::Reloaded(t) if t.elapsed() < Duration::from_secs(5) => {
                ("The file changed elsewhere and was read again".to_string(), p.accent)
            }
            Status::Reloaded(_) => ("Changes apply as you make them".to_string(), p.subtext),
            Status::Invalid(_) => ("Not saved, see above".to_string(), p.urgent),
            Status::WriteFailed(_) => ("Couldn't save".to_string(), p.urgent),
        };
        ui.label(RichText::new(text).small().color(color(c)));
        ui.label(
            RichText::new(self.doc.path.display().to_string())
                .small()
                .color(color(p.subtext)),
        );
    }

    fn page(&mut self, ui: &mut Ui, p: &Palette) {
        if let Some(msg) = &self.doc.broken {
            widgets::card(ui, p, "The config file can't be read", "", |ui| {
                widgets::error(ui, p, msg);
                widgets::note(
                    ui,
                    p,
                    "Fix it in a text editor; this window picks the fix up by itself.",
                );
            });
            return;
        }
        if let Status::Invalid(msg) | Status::WriteFailed(msg) = &self.doc.status {
            widgets::card(ui, p, "Your last change isn't saved yet", "", |ui| {
                for line in msg.lines() {
                    widgets::error(ui, p, line);
                }
                widgets::note(ui, p, "The desktop keeps the previous settings until this is fixed.");
            });
        }

        if !self.search.is_empty() {
            return self.search_results(ui, p);
        }
        ui.add_space(4.0);
        ui.label(RichText::new(self.page.title()).heading().color(color(p.text)));
        let (doc, cache) = (&mut self.doc, &mut self.cache);
        match self.page {
            Page::General => fields::show_sections(ui, p, doc, cache, fields::GENERAL),
            Page::Appearance => {
                self.themes.show(ui, p, doc);
                fields::show_sections(ui, p, doc, cache, fields::APPEARANCE);
                self.colors.show(ui, p, doc);
            }
            Page::Layout => {
                fields::show_sections(ui, p, doc, cache, fields::LAYOUT);
                pages::workspaces::show(ui, p, doc);
            }
            Page::Bar => fields::show_sections(ui, p, doc, cache, fields::BAR),
            Page::Notifications => fields::show_sections(ui, p, doc, cache, fields::NOTIFICATIONS),
            Page::Monitors => pages::monitors::show(ui, p, doc, cache),
            Page::Keyboard => fields::show_sections(ui, p, doc, cache, fields::KEYBOARD),
            Page::Input => fields::show_sections(ui, p, doc, cache, fields::INPUT),
            Page::Keybinds => self.binds.show(ui, p, doc),
            Page::Rules => pages::rules::show(ui, p, doc, cache),
            Page::Session => fields::show_sections(ui, p, doc, cache, fields::SESSION),
        }
        ui.add_space(24.0);
    }

    fn search_results(&mut self, ui: &mut Ui, p: &Palette) {
        ui.add_space(4.0);
        ui.label(RichText::new("Search").heading().color(color(p.text)));
        let mut any = false;
        for section in fields::all() {
            let hits: Vec<_> = section
                .fields
                .iter()
                .filter(|f| fields::matches(f, &self.search))
                .collect();
            if hits.is_empty() {
                continue;
            }
            any = true;
            widgets::card(ui, p, section.title, "", |ui| {
                for (i, field) in hits.into_iter().enumerate() {
                    if i > 0 {
                        widgets::divider(ui, p);
                    }
                    fields::show_field(ui, p, &mut self.doc, &mut self.cache, field);
                }
            });
        }
        if !any {
            ui.add_space(14.0);
            widgets::note(
                ui,
                p,
                "Nothing matches. Keybinds, colors, monitors and rules have their own pages.",
            );
        }
    }
}

impl eframe::App for SettingsApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.doc.tick() {
            self.look.set(ctx, self.doc.config.theme.clone());
        }
        // Wake for the next save, and now and then to notice outside edits.
        ctx.request_repaint_after(self.doc.next_due().unwrap_or(Duration::from_secs(1)));
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let p = self.look.theme.palette;
        egui::Panel::left("pages")
            .resizable(false)
            .exact_size(230.0)
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
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    // Long lines are hard to read; keep the page to a comfortable width.
                    ui.set_max_width(780.0);
                    self.page(ui, &p);
                });
            });
    }
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
    let text_color = if selected { p.text } else { p.muted };
    ui.painter().text(
        rect.left_center() + vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::TextStyle::Button.resolve(ui.style()),
        color(text_color),
    );
    response
}
