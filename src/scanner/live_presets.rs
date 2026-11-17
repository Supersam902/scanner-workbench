use super::appearance::title;
use super::{Page, Workbench, query_key};
use std::collections::BTreeSet;

const BUILT_IN: [&str; 5] = [
    "All supported",
    "Engine",
    "Fuel trims",
    "Temperature",
    "Favorites",
];
const MAX_NAME_LENGTH: usize = 48;

enum Action {
    Save(String),
    Delete {
        name: String,
        original: BTreeSet<String>,
    },
}

pub(super) struct Dialog {
    action: Action,
    error: Option<String>,
    focus_name: bool,
}

impl Workbench {
    pub(super) fn select_all_live(&mut self) {
        self.auto_select_live = true;
        self.selected = self.generic.iter().filter(|query| {
            matches!(query.bytes.as_slice(), [1, pid] if !self.connected || self.pid_support.reported.contains(pid))
        }).map(query_key).collect();
    }

    pub(super) fn preset(&mut self, name: &str) {
        if name == "All supported" {
            self.select_all_live();
            return;
        }
        self.auto_select_live = false;
        let pids: &[u8] = match name {
            "Fuel trims" => &[6, 7, 8, 9, 4, 12, 16],
            "Temperature" => &[5, 15, 70, 92],
            _ => &[4, 5, 6, 7, 12, 13, 16, 17, 66],
        };
        self.selected = self
            .generic
            .iter()
            .filter_map(|query| {
                let key = query_key(query);
                let included = if name == "Favorites" {
                    self.settings.favorites().contains(&key)
                } else {
                    query.target.is_none() && query.bytes.get(1).is_some_and(|p| pids.contains(p))
                };
                included.then_some(key)
            })
            .collect();
    }

    pub(super) fn save_live_preset(&mut self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_NAME_LENGTH
            || name.chars().any(char::is_control)
        {
            return Err(format!(
                "Enter a name of up to {MAX_NAME_LENGTH} characters."
            ));
        }
        if self.selected.is_empty() {
            return Err("Select at least one data item.".into());
        }

        let name = name.to_owned();
        let selection = self.selected.clone();
        let folded_name = name.to_lowercase();
        let profile = self.settings.active_profile.clone();
        self.try_edit_settings(|settings| {
            let presets = settings.user_presets_mut(profile.as_deref())?;
            let duplicate = BUILT_IN
                .into_iter()
                .chain(presets.keys().map(String::as_str))
                .any(|existing| existing.to_lowercase() == folded_name);
            if duplicate {
                return Err("That preset name is already in use.".into());
            }
            presets.insert(name, selection);
            Ok(())
        })?;
        Ok(())
    }

    pub(super) fn load_live_preset(&mut self, name: &str) -> Result<(), String> {
        let saved = self
            .settings
            .live_presets()
            .get(name)
            .ok_or_else(|| format!("Preset “{name}” is unavailable."))?;
        let selection: BTreeSet<_> = self
            .generic
            .iter()
            .map(query_key)
            .filter(|key| saved.contains(key))
            .collect();
        if selection.is_empty() {
            return Err(format!(
                "Preset “{name}” has no data items in this version."
            ));
        }
        if self.selected != selection {
            self.selected = selection;
            self.graph_signals = None;
        }
        self.auto_select_live = false;
        Ok(())
    }

    pub(super) fn delete_live_preset(
        &mut self,
        name: &str,
        original: &BTreeSet<String>,
    ) -> Result<(), String> {
        let profile = self.settings.active_profile.clone();
        self.try_edit_settings(|settings| {
            let presets = settings.user_presets_mut(profile.as_deref())?;
            if presets.get(name).is_some_and(|current| current != original) {
                return Err(
                    "This preset changed in another window. Reopen the scanner to refresh it."
                        .into(),
                );
            }
            presets.remove(name);
            Ok(())
        })?;
        Ok(())
    }

    pub(super) fn live_presets_menu(&mut self, ui: &mut egui::Ui) {
        let mut load = None;
        ui.menu_button("Presets", |ui| {
            for name in BUILT_IN {
                if ui.button(name).clicked() {
                    self.preset(name);
                    ui.close_menu();
                }
            }
            ui.separator();
            if ui
                .add_enabled(
                    !self.selected.is_empty(),
                    egui::Button::new("Save current selection…"),
                )
                .clicked()
            {
                self.live_preset_dialog = Some(Dialog {
                    action: Action::Save(String::new()),
                    error: None,
                    focus_name: true,
                });
                ui.close_menu();
            }
            if !self.settings.live_presets().is_empty() {
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (name, original) in self.settings.live_presets() {
                            ui.horizontal(|ui| {
                                if ui
                                    .add_sized([200.0, 22.0], egui::Button::new(name).truncate())
                                    .on_hover_text(format!(
                                        "{name} · {} data items",
                                        original.len()
                                    ))
                                    .clicked()
                                {
                                    load = Some(name.clone());
                                    ui.close_menu();
                                }
                                if ui
                                    .small_button("Delete…")
                                    .on_hover_text(format!("Delete preset {name}"))
                                    .clicked()
                                {
                                    self.live_preset_dialog = Some(Dialog {
                                        action: Action::Delete {
                                            name: name.clone(),
                                            original: original.clone(),
                                        },
                                        error: None,
                                        focus_name: false,
                                    });
                                    ui.close_menu();
                                }
                            });
                        }
                    });
            }
        });
        if let Some(name) = load {
            self.status = match self.load_live_preset(&name) {
                Ok(()) => format!("Loaded preset “{name}”."),
                Err(message) => message,
            };
        }
    }

    pub(super) fn show_live_preset_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.live_preset_dialog.take() else {
            return;
        };
        if self.page != Page::Live || self.playback.is_some() {
            return;
        }
        let response = egui::Modal::new(egui::Id::new("live_preset_dialog")).show(ctx, |ui| {
            ui.set_width(320.0);
            let (confirm, enabled, enter) = match &mut dialog.action {
                Action::Save(name) => {
                    ui.label(title("Save live-data preset", 23.0));
                    let input = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Preset name")
                            .char_limit(MAX_NAME_LENGTH)
                            .desired_width(f32::INFINITY),
                    );
                    if std::mem::take(&mut dialog.focus_name) {
                        input.request_focus();
                    }
                    let enter =
                        input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    ("Save preset", !name.trim().is_empty(), enter)
                }
                Action::Delete { name, .. } => {
                    ui.label(title("Delete live-data preset", 23.0));
                    ui.label(&*name);
                    ("Delete preset", true, false)
                }
            };
            if let Some(message) = &dialog.error {
                ui.colored_label(super::appearance::AMBER, message.as_str());
            }
            let (confirmed, cancelled) = ui
                .horizontal(|ui| {
                    let confirmed = ui
                        .add_enabled(enabled, egui::Button::new(confirm))
                        .clicked();
                    (confirmed || enabled && enter, ui.button("Cancel").clicked())
                })
                .inner;
            if cancelled {
                return true;
            }
            if confirmed {
                let (result, message) = match &dialog.action {
                    Action::Save(name) => (
                        self.save_live_preset(name),
                        format!("Saved preset “{}”.", name.trim()),
                    ),
                    Action::Delete { name, original } => (
                        self.delete_live_preset(name, original),
                        format!("Deleted preset “{name}”."),
                    ),
                };
                match result {
                    Ok(()) => {
                        self.status = message;
                        return true;
                    }
                    Err(message) => dialog.error = Some(message),
                }
            }
            false
        });
        if !response.inner && !response.should_close() {
            self.live_preset_dialog = Some(dialog);
        }
    }
}
