use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use egui::{RichText, Ui};
use serde::{Deserialize, Serialize};

use super::units::{Distance, MassFlow, Pressure, Speed, Temperature, Units};
use super::{Settings, Workbench};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserProfile {
    pub id: String,
    pub name: String,
    pub units: Units,
    pub favorites: BTreeSet<String>,
    pub live_presets: BTreeMap<String, BTreeSet<String>>,
}

pub(super) struct Dialog {
    original: Option<UserProfile>,
    draft: UserProfile,
    first_run: bool,
    focus_name: bool,
    error: Option<String>,
}

impl Dialog {
    fn new(units: Units, first_run: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self {
            original: None,
            draft: UserProfile {
                id: format!(
                    "profile-{}-{}-{}",
                    chrono::Utc::now().timestamp_millis(),
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ),
                units,
                ..Default::default()
            },
            first_run,
            focus_name: true,
            error: None,
        }
    }
}

fn unit_controls(ui: &mut Ui, units: &mut Units) {
    macro_rules! row {
        ($ui:ident, $title:literal, $field:ident, $type:ident) => {
            $ui.label($title);
            $ui.horizontal_wrapped(|ui| {
                ui.set_max_width(280.0);
                for &choice in $type::ALL {
                    ui.selectable_value(&mut units.$field, choice, choice.label());
                }
            });
            $ui.end_row();
        };
    }
    egui::Grid::new("profile_units")
        .num_columns(2)
        .max_col_width(280.0)
        .spacing([18.0, 10.0])
        .show(ui, |ui| {
            row!(ui, "Speed", speed, Speed);
            row!(ui, "Airflow", mass_flow, MassFlow);
            row!(ui, "Temperature", temperature, Temperature);
            row!(ui, "Pressure", pressure, Pressure);
            row!(ui, "Distance", distance, Distance);
        });
}

impl Workbench {
    pub(super) fn unit_preferences(&self) -> Units {
        self.settings
            .profile()
            .map_or_else(Units::default, |profile| profile.units)
    }

    pub(super) fn prompt_for_profile(&mut self) {
        if self.settings.profile().is_none() && self.profile_dialog.is_none() {
            self.profile_dialog = Some(Dialog::new(Units::default(), true));
        }
    }

    pub(super) fn save_profile(
        &mut self,
        draft: &UserProfile,
        original: Option<&UserProfile>,
    ) -> Result<(), String> {
        let mut draft = draft.clone();
        draft.name = draft.name.trim().to_owned();
        if draft.name.is_empty()
            || draft.name.chars().count() > 60
            || draft.name.chars().any(char::is_control)
        {
            return Err("Enter a profile name of up to 60 characters.".into());
        }
        self.settings = Settings::try_edit(&self.root, |settings| {
            if settings.profiles.values().any(|profile| {
                profile.id != draft.id && profile.name.to_lowercase() == draft.name.to_lowercase()
            }) {
                return Err("A profile already uses that name.".into());
            }
            if let Some(original) = original {
                let saved = settings
                    .profiles
                    .get_mut(&original.id)
                    .ok_or("This profile is unavailable.")?;
                if saved.name != original.name || saved.units != original.units {
                    return Err(
                        "This profile changed in another window. Reopen the profile to edit it."
                            .into(),
                    );
                }
                saved.name = draft.name;
                saved.units = draft.units;
            } else {
                if settings.profiles.contains_key(&draft.id) {
                    return Err("This profile already exists.".into());
                }
                if settings.profiles.is_empty() {
                    draft.favorites = settings.favorites.clone();
                    draft.live_presets = settings.live_presets.clone();
                }
                settings.profiles.insert(draft.id.clone(), draft.clone());
            }
            settings.active_profile = Some(draft.id.clone());
            Ok(())
        })?;
        self.status = "Profile saved. Display units updated.".into();
        Ok(())
    }

    pub(super) fn switch_profile(&mut self, id: &str) -> Result<(), String> {
        self.settings = Settings::try_edit(&self.root, |settings| {
            if !settings.profiles.contains_key(id) {
                return Err("This profile is unavailable.".into());
            }
            settings.active_profile = Some(id.to_owned());
            Ok(())
        })?;
        self.status = "Profile selected. Display units updated.".into();
        Ok(())
    }

    pub(super) fn profiles_page(&mut self, ui: &mut Ui) {
        super::views::card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(super::appearance::title("User profiles", 23.0));
            ui.label("Save your name, display units, favorites and live-data presets.");
            if ui.button("New profile").clicked() {
                self.profile_dialog = Some(Dialog::new(self.unit_preferences(), false));
            }
            let profiles: Vec<_> = self.settings.profiles.values().cloned().collect();
            if profiles.is_empty() {
                super::views::caption(ui, "Create a profile to save your unit preferences.");
            }
            for profile in profiles {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.strong(&profile.name);
                    if self.settings.active_profile.as_deref() == Some(&profile.id) {
                        ui.colored_label(super::appearance::ACCENT, "Active");
                    } else if ui.button("Use profile").clicked() {
                        if let Err(error) = self.switch_profile(&profile.id) {
                            self.status = error;
                        }
                    }
                    if ui.button("Edit profile").clicked() {
                        self.profile_dialog = Some(Dialog {
                            original: Some(profile.clone()),
                            draft: profile.clone(),
                            first_run: false,
                            focus_name: true,
                            error: None,
                        });
                    }
                });
                super::views::caption(ui, &profile.units.summary());
            }
        });
    }

    pub(super) fn show_profile_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.profile_dialog.take() else {
            return;
        };
        let response = egui::Modal::new(egui::Id::new("user_profile_dialog")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(super::appearance::title(if dialog.first_run { "Welcome to Scanner Workbench" } else if dialog.original.is_some() { "Edit profile" } else { "Create a profile" }, 25.0));
            if dialog.first_run { ui.label("Create your profile and choose how readings are displayed."); }
            ui.label("Profile name");
            let input = ui.add(egui::TextEdit::singleline(&mut dialog.draft.name).hint_text("Your name or workshop").char_limit(60).desired_width(f32::INFINITY));
            if std::mem::take(&mut dialog.focus_name) { input.request_focus(); }
            ui.add_space(10.0);
            unit_controls(ui, &mut dialog.draft.units);
            ui.add_space(10.0);
            super::views::caption(ui, "Manage profiles under Connection > Profiles. Recorded evidence keeps its original values and units.");
            if let Some(error) = &dialog.error { ui.add(egui::Label::new(RichText::new(error).color(super::appearance::AMBER)).wrap()); }
            ui.horizontal_wrapped(|ui| {
                let save = ui.add_enabled(!dialog.draft.name.trim().is_empty(), egui::Button::new("Save profile")).clicked();
                let defaults = dialog.first_run && ui.button("Use default profile").clicked();
                let cancel = !dialog.first_run && ui.button("Cancel").clicked();
                (save, defaults, cancel)
            }).inner
        });
        let (save, defaults, cancel) = response.inner;
        if defaults {
            dialog.draft.name = "Default".into();
            dialog.draft.units = Units::default();
        }
        if save || defaults {
            match self.save_profile(&dialog.draft, dialog.original.as_ref()) {
                Ok(()) => return,
                Err(error) => dialog.error = Some(error),
            }
        }
        if !cancel && (dialog.first_run || !response.should_close()) {
            self.profile_dialog = Some(dialog);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{load_json, save_json};
    use super::*;

    #[test]
    fn profiles_migrate_legacy_preferences_and_keep_user_choices_separate() {
        let folder = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.favorites.insert("OBD:010C".into());
        settings
            .live_presets
            .insert("Idle".into(), BTreeSet::from(["OBD:010C".into()]));
        save_json(&folder.path().join("settings.json"), &settings).unwrap();
        let mut app = Workbench::new(folder.path().to_owned());
        let mut first = Dialog::new(Units::default(), true).draft;
        first.name = "Sam".into();
        first.units.speed = Speed::Mph;
        first.units.temperature = Temperature::Fahrenheit;
        app.save_profile(&first, None).unwrap();
        assert_eq!(app.settings.favorites(), &settings.favorites);
        assert_eq!(app.settings.live_presets(), &settings.live_presets);
        let mut other_window = Workbench::new(folder.path().to_owned());
        let mut second = Dialog::new(Units::default(), false).draft;
        second.name = "Workshop".into();
        other_window.save_profile(&second, None).unwrap();
        assert!(other_window.settings.favorites().is_empty());
        assert!(other_window.settings.live_presets().is_empty());
        app.selected = BTreeSet::from(["OBD:010D".into()]);
        app.save_live_preset("Road test").unwrap();
        assert_eq!(
            app.settings.active_profile.as_deref(),
            Some(first.id.as_str())
        );
        let stored: Settings = load_json(&folder.path().join("settings.json")).unwrap();
        assert!(
            stored.profiles[&first.id]
                .live_presets
                .contains_key("Road test")
        );
        assert!(stored.profiles[&second.id].live_presets.is_empty());
        app.switch_profile(&second.id).unwrap();
        assert_eq!(app.unit_preferences().speed, Speed::Kmh);
        app.switch_profile(&first.id).unwrap();
        assert_eq!(app.unit_preferences().speed, Speed::Mph);
        assert_eq!(app.unit_preferences().temperature, Temperature::Fahrenheit);
        let mut duplicate = Dialog::new(Units::default(), false).draft;
        duplicate.name = " sam ".into();
        assert!(app.save_profile(&duplicate, None).is_err());
    }

    #[test]
    fn profile_edit_rejects_stale_units_and_preserves_unsaved_changes_on_write_failure() {
        let folder = tempfile::tempdir().unwrap();
        let mut app = Workbench::new(folder.path().to_owned());
        let mut draft = Dialog::new(Units::default(), true).draft;
        draft.name = "Sam".into();
        app.save_profile(&draft, None).unwrap();
        let original = app.settings.profile().unwrap().clone();
        let mut newer = original.clone();
        newer.units.speed = Speed::Mph;
        app.save_profile(&newer, Some(&original)).unwrap();
        let mut stale = original.clone();
        stale.units.temperature = Temperature::Fahrenheit;
        assert!(app.save_profile(&stale, Some(&original)).is_err());
        assert_eq!(app.unit_preferences().speed, Speed::Mph);
        let bad = folder.path().join("blocked");
        std::fs::write(&bad, b"not a directory").unwrap();
        let mut blocked = Workbench::new(bad);
        assert!(blocked.save_profile(&draft, None).is_err());
        assert!(blocked.settings.profiles.is_empty());
    }
}
