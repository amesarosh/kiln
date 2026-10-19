//! Saved themes: pick the one in use, save the current look as a new one, delete one.

use wt_config::theme::Palette;
use wt_ui::{
    color,
    egui::{self, RichText, Ui},
};

use crate::{
    doc::ConfigDoc,
    widgets::{self, RowInfo},
};

#[derive(Default)]
pub struct ThemesPage {
    new_name: String,
    /// The theme whose Delete was clicked once; a second click deletes it.
    confirm_delete: Option<String>,
    message: Option<Result<String, String>>,
}

impl ThemesPage {
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, doc: &mut ConfigDoc) {
        let current = doc.theme_name();
        let names = doc.theme_names();
        widgets::card(
            ui,
            p,
            "Theme",
            "A theme is the font, corner rounding, icons and colors on this page. Save the look as a \
             theme to come back to it, or to try something new without losing it.",
            |ui| {
                let help = match &current {
                    Some(n) => format!("Changes on this page are saved to the theme \"{n}\"."),
                    None => "Changes on this page are kept in the config file.".to_string(),
                };
                let mut picked = current.clone().unwrap_or_default();
                let info = RowInfo {
                    label: "Theme in use",
                    help: &help,
                    changed: false,
                    restart: false,
                };
                widgets::row(ui, p, info, |ui| {
                    let options: Vec<(&str, &str)> = std::iter::once(("", "Built-in"))
                        .chain(names.iter().map(|n| (n.as_str(), n.as_str())))
                        .collect();
                    if widgets::choice(ui, "theme-in-use", &mut picked, &options) {
                        let kept = doc.use_theme((!picked.is_empty()).then_some(picked.as_str()));
                        self.confirm_delete = None;
                        self.message =
                            kept.map(|k| Ok(format!("Your changes to the built-in theme were saved as \"{k}\".")));
                    }
                });

                widgets::divider(ui, p);
                let name = self.new_name.trim().to_string();
                let taken = names.iter().any(|n| n.eq_ignore_ascii_case(&name));
                let valid = wt_config::valid_theme_name(&name) && !taken;
                let info = RowInfo {
                    label: "Save as a new theme",
                    help: "A copy of the current look, which then becomes the theme in use.",
                    changed: false,
                    restart: false,
                };
                widgets::row(ui, p, info, |ui| {
                    if ui.add_enabled(valid, egui::Button::new("Save")).clicked() {
                        doc.save_theme_as(&name);
                        self.new_name.clear();
                        self.confirm_delete = None;
                        self.message = Some(Ok(format!("Saved \"{name}\"; changes now go to it.")));
                    }
                    widgets::text(ui, &mut self.new_name, "name, e.g. Nord");
                });
                if taken {
                    widgets::error(ui, p, "There's already a theme with that name.");
                } else if !name.is_empty() && !valid {
                    widgets::error(ui, p, "Names can't have slashes or start with a dot.");
                }

                if let Some(n) = &current {
                    widgets::divider(ui, p);
                    let confirming = self.confirm_delete.as_ref() == Some(n);
                    let info = RowInfo {
                        label: "Delete this theme",
                        help: "Goes back to the built-in theme. The file can't be brought back.",
                        changed: false,
                        restart: false,
                    };
                    widgets::row(ui, p, info, |ui| {
                        let label = if confirming {
                            RichText::new("Click again to delete").color(color(p.urgent))
                        } else {
                            RichText::new("Delete")
                        };
                        if ui.button(label).clicked() {
                            if confirming {
                                self.confirm_delete = None;
                                self.message = Some(doc.delete_theme(n).map(|()| format!("Deleted \"{n}\".")));
                            } else {
                                self.confirm_delete = Some(n.clone());
                            }
                        }
                    });

                    let overrides = doc.theme_overrides();
                    if !overrides.is_empty() {
                        widgets::divider(ui, p);
                        widgets::note(
                            ui,
                            p,
                            &format!(
                                "The config file also sets {}, which wins over every theme.",
                                overrides.join(", ")
                            ),
                        );
                        if ui.button("Move into this theme").clicked() {
                            doc.save_theme_as(n);
                        }
                    }
                }

                match &self.message {
                    Some(Ok(m)) => {
                        widgets::divider(ui, p);
                        ui.label(RichText::new(m).small().color(color(p.accent)));
                    }
                    Some(Err(e)) => {
                        widgets::divider(ui, p);
                        widgets::error(ui, p, e);
                    }
                    None => {}
                }
            },
        );
    }
}
