use super::appearance::{AMBER, INK, MUTED};
use super::codes::{Category, identifiers};
use super::freeze_frames::{Frames, trigger_code};
use super::model::{Snapshot, ecu_label, unavailable, valid_vin};
use super::readiness::Period;
use super::report_sections::{
    code_results, frame_reading_qualification, frame_value, monitor_status, qualification,
    read_issues, readiness_monitors, readiness_qualification, readiness_reports, verified,
};
use super::scan_notes::{self, NOTES_LIMIT, Notes, UNSAVED_MESSAGE, WORK_ORDER_LIMIT};
use super::views::{caption, card, freeze_frame_link, open_local, readiness_color};
use super::{Page, Workbench, recordings};
use egui::{RichText, Ui, Vec2};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct Review {
    pub(super) snapshot: Arc<Snapshot>,
    pub(super) from: Page,
    path: Option<PathBuf>,
    saved_notes: Option<Notes>,
    draft_notes: Notes,
    notes_error: Option<String>,
    section: Section,
    frame: Option<(u32, u8)>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Section {
    Codes,
    Readiness,
    FreezeFrame,
    Notes,
}

impl Review {
    pub(super) fn new(snapshot: Arc<Snapshot>, path: Option<PathBuf>, from: Page) -> Self {
        let mut review = Self {
            snapshot,
            from,
            path,
            saved_notes: None,
            draft_notes: Notes::default(),
            notes_error: None,
            section: Section::Codes,
            frame: None,
        };
        review.reload_notes();
        review
    }

    fn reload_notes(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        match scan_notes::load(path) {
            Ok(notes) => {
                self.draft_notes = notes.clone();
                self.saved_notes = Some(notes);
                self.notes_error = None;
            }
            Err(error) => {
                self.saved_notes = None;
                self.notes_error = Some(format!("Could not read saved notes. {error}"));
            }
        }
    }

    pub(super) fn has_unsaved_notes(&self) -> bool {
        self.saved_notes
            .as_ref()
            .is_some_and(|saved| &self.draft_notes != saved)
    }

    pub(super) fn has_unsaved_notes_for(&self, path: &std::path::Path) -> bool {
        self.path.as_deref() == Some(path) && self.has_unsaved_notes()
    }

    fn save_notes(&mut self) -> bool {
        let (Some(path), Some(original)) = (&self.path, &self.saved_notes) else {
            return false;
        };
        match scan_notes::save(path, original, &self.draft_notes) {
            Ok(notes) => {
                self.draft_notes = notes.clone();
                self.saved_notes = Some(notes);
                self.notes_error = None;
                true
            }
            Err(error) => {
                self.notes_error = Some(error);
                false
            }
        }
    }

    pub(super) fn show(&mut self, ui: &mut Ui, describe: impl Fn(&str) -> String) -> (bool, bool) {
        let mut print = false;
        let mut notes_saved = false;
        let snapshot = self.snapshot.clone();
        if let Some(scan) = &snapshot.emissions_scan {
            ui.horizontal_wrapped(|ui| {
                ui.strong(if snapshot.demo {
                    "Practice scan".into()
                } else {
                    snapshot.vehicle.label()
                });
                ui.label(recordings::recording_date(scan.started_ms));
                ui.label(scan.state.label());
                if let Some(stage) = scan.clear_stage {
                    caption(ui, stage.label());
                }
                print = ui
                    .add_enabled(
                        !self.has_unsaved_notes(),
                        egui::Button::new("Print scan report"),
                    )
                    .on_disabled_hover_text(UNSAVED_MESSAGE)
                    .clicked();
                if self.has_unsaved_notes() {
                    notes_saved = ui.button("Save scan notes").clicked() && self.save_notes();
                    if ui.button("Discard edits").clicked() {
                        self.draft_notes = self.saved_notes.clone().unwrap_or_default();
                        self.reload_notes();
                    }
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            if valid_vin(&snapshot.vehicle.vin) {
                ui.monospace(&snapshot.vehicle.vin);
            }
            if !snapshot.vehicle.registration.trim().is_empty() {
                ui.label(&snapshot.vehicle.registration);
            }
        });
        ui.horizontal_wrapped(|ui| {
            let sections = [
                (Section::Codes, "Trouble codes"),
                (Section::Readiness, "Readiness"),
                (Section::FreezeFrame, "Freeze frame"),
                (
                    Section::Notes,
                    if self.has_unsaved_notes() {
                        "Notes *"
                    } else {
                        "Notes"
                    },
                ),
            ];
            for (section, label) in sections {
                if section == Section::Notes
                    && self.path.is_none()
                    && snapshot.vehicle.notes.trim().is_empty()
                {
                    continue;
                }
                if ui
                    .selectable_value(&mut self.section, section, label)
                    .clicked()
                {
                    self.frame = None;
                }
            }
        });
        if let Some(error) = &self.notes_error {
            ui.colored_label(AMBER, error);
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt(("scan_review", self.section, snapshot.at_ms, self.frame))
            .auto_shrink([false, false])
            .show(ui, |ui| match self.section {
                Section::Codes => {
                    let frames = Frames::from_rows(snapshot.rows.iter().chain(&snapshot.failures));
                    if let Some(frame) = show_codes(ui, &snapshot, &frames, describe) {
                        self.frame = Some(frame);
                        self.section = Section::FreezeFrame;
                        ui.ctx().request_repaint();
                    }
                }
                Section::Readiness => show_readiness(ui, &snapshot),
                Section::FreezeFrame => {
                    let frames = Frames::from_rows(snapshot.rows.iter().chain(&snapshot.failures));
                    if self.frame.is_some() && ui.link("Show all frames").clicked() {
                        self.frame = None;
                    }
                    show_frames(ui, &snapshot, frames, self.frame, describe);
                }
                Section::Notes => self.notes(ui),
            });
        (print, notes_saved)
    }

    fn notes(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            if let Some(path) = &self.path {
                let available = path.is_file();
                if !available {
                    ui.colored_label(
                        AMBER,
                        "Save this scan from Scan history before adding notes.",
                    );
                }
                if self.saved_notes.is_some() && (available || self.has_unsaved_notes()) {
                    ui.label("Work order");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft_notes.work_order)
                            .id_salt(("scan_work_order", path))
                            .char_limit(WORK_ORDER_LIMIT)
                            .desired_width(260.0)
                            .hint_text("Optional reference"),
                    );
                    ui.label("Technician notes");
                    egui::ScrollArea::vertical()
                        .id_salt("scan_notes_editor")
                        .max_height(140.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.draft_notes.technician)
                                    .id_salt(("scan_technician_notes", path))
                                    .char_limit(NOTES_LIMIT)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(3)
                                    .hint_text("Findings, work performed, or follow-up"),
                            );
                        });
                } else if available && ui.button("Reload scan notes").clicked() {
                    self.reload_notes();
                }
            }
            if !self.snapshot.vehicle.notes.trim().is_empty() {
                if self.path.is_some() {
                    ui.separator();
                }
                ui.strong("Notes at capture");
                ui.label(&self.snapshot.vehicle.notes);
            }
        });
    }
}

fn show_codes(
    ui: &mut Ui,
    snapshot: &Snapshot,
    frames: &Frames<'_>,
    describe: impl Fn(&str) -> String,
) -> Option<(u32, u8)> {
    let mut open = None;
    for category in Category::ALL {
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.strong(category.label());
            let mut read = false;
            for row in code_results(snapshot, category) {
                read = true;
                let reading = &row.reading;
                let ecu = format!("ECU {}", ecu_label(reading.source));
                let codes: BTreeSet<_> = identifiers(&reading.value).collect();
                if codes.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(&ecu);
                        ui.label(&reading.value);
                    });
                    if reading.status != "ECU responded" && !reading.status.is_empty() {
                        caption(ui, &reading.status);
                    }
                }
                for code in codes {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(&ecu);
                        ui.monospace(code);
                        ui.label(describe(code));
                        let links = frames.links(row, code, |row| verified(snapshot, row));
                        ui.push_id((category.request(), reading.source, code), |ui| {
                            if let Some(number) = freeze_frame_link(ui, links)
                                && let Some(source) = reading.source
                            {
                                open = Some((source, number));
                            }
                        });
                    });
                }
                if let Some(state) = qualification(snapshot, row) {
                    ui.colored_label(AMBER, state);
                }
            }
            read |= show_issues(ui, snapshot, category.request());
            if !read {
                caption(ui, "No result from this scan");
            }
        });
    }
    open
}

fn show_readiness(ui: &mut Ui, snapshot: &Snapshot) {
    let ecus = readiness_reports(snapshot);
    let mut read = !ecus.is_empty();
    for (source, reports) in ecus {
        let reports = reports.each_ref().map(Option::as_ref);
        let qualifications = reports
            .map(|report| report.and_then(|report| readiness_qualification(snapshot, report)));
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.strong(format!("ECU {}", ecu_label(source)));
            if let Some(report) = reports[0].filter(|report| report.latest.is_some()) {
                ui.horizontal_wrapped(|ui| {
                    let color = if qualifications[0].is_some() {
                        MUTED
                    } else {
                        INK
                    };
                    ui.colored_label(
                        color,
                        format!("MIL {}", report.mil.unwrap_or("unavailable")),
                    );
                    ui.colored_label(
                        color,
                        format!(
                            "Confirmed DTCs {}",
                            report.dtc_count.unwrap_or("unavailable")
                        ),
                    );
                });
            }
            egui::Grid::new(("saved_readiness", source))
                .striped(true)
                .spacing(Vec2::new(22.0, 6.0))
                .show(ui, |ui| {
                    ui.strong("Monitor");
                    ui.strong(Period::SinceClear.label());
                    ui.strong(Period::DriveCycle.label());
                    ui.end_row();
                    ui.label("Read result");
                    for (report, qualification) in reports.into_iter().zip(qualifications) {
                        match qualification {
                            Some(state) => {
                                ui.colored_label(AMBER, state);
                            }
                            None if report.is_some() => {
                                ui.label("ECU replied");
                            }
                            None => caption(ui, "No result"),
                        }
                    }
                    ui.end_row();
                    let mut monitors: Vec<_> = readiness_monitors(reports).collect();
                    monitors.sort_by_key(|(name, states)| {
                        (states.iter().flatten().min().copied(), *name)
                    });
                    if monitors.is_empty()
                        && reports
                            .iter()
                            .flatten()
                            .any(|report| report.latest.is_some())
                    {
                        caption(ui, "No supported monitor states reported");
                        ui.end_row();
                    }
                    for (name, states) in monitors {
                        ui.label(name);
                        for (state, qualification) in states.into_iter().zip(qualifications) {
                            let color = state
                                .filter(|_| qualification.is_none())
                                .map_or(MUTED, readiness_color);
                            ui.colored_label(color, monitor_status(state));
                        }
                        ui.end_row();
                    }
                });
        });
    }
    read |= show_issues(ui, snapshot, Period::SinceClear.request());
    read |= show_issues(ui, snapshot, Period::DriveCycle.request());
    if !read {
        caption(ui, "No readiness result from this scan");
    }
}

fn show_frames(
    ui: &mut Ui,
    snapshot: &Snapshot,
    frames: Frames<'_>,
    selected: Option<(u32, u8)>,
    describe: impl Fn(&str) -> String,
) {
    if frames.by_ecu.is_empty() && frames.discovery.is_empty() {
        caption(ui, "No freeze-frame result from this scan");
    }
    for ((source, number), frame) in frames.by_ecu {
        if selected.is_some_and(|(ecu, wanted)| (Some(ecu), wanted) != (source, number)) {
            continue;
        }
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.strong(format!("Frame {number} · ECU {}", ecu_label(source)));
            match frame.trigger {
                Some(trigger) => {
                    let reading = &trigger.reading;
                    if let Some(code) = trigger_code(reading) {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("Trigger code {code}")).strong());
                            ui.label(describe(code));
                        });
                        if verified(snapshot, trigger)
                            && !frame.has_values(trigger, |row| verified(snapshot, row))
                        {
                            caption(ui, "No verified values captured");
                        }
                    } else {
                        ui.label(&reading.value);
                    }
                    if let Some(state) = qualification(snapshot, trigger) {
                        ui.colored_label(AMBER, state);
                    }
                }
                None => {
                    ui.colored_label(AMBER, "Trigger code unavailable");
                }
            }
            if !frame.readings.is_empty() {
                let mut readings = frame.readings.clone();
                readings.sort_by_key(|row| row.reading.request.starts_with("0201"));
                egui::Grid::new(("saved_frame", source, number))
                    .striped(true)
                    .spacing(Vec2::new(18.0, 6.0))
                    .show(ui, |ui| {
                        ui.strong("Parameter");
                        ui.strong("Captured value");
                        ui.end_row();
                        for row in readings {
                            let reading = &row.reading;
                            ui.label(&reading.name);
                            ui.vertical(|ui| {
                                if unavailable(reading) {
                                    ui.colored_label(AMBER, &reading.status);
                                } else {
                                    ui.label(frame_value(reading));
                                }
                                if let Some(state) =
                                    frame_reading_qualification(snapshot, &frame, row)
                                {
                                    caption(ui, state);
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
        });
    }
    for (number, issue) in frames.discovery {
        if selected.is_none_or(|(_, frame)| frame == number) {
            ui.colored_label(AMBER, format!("Frame {number} · {}", issue.reading.status));
        }
    }
}

fn show_issues(ui: &mut Ui, snapshot: &Snapshot, request: &'static str) -> bool {
    let mut read = false;
    for failure in read_issues(snapshot, request) {
        read = true;
        ui.colored_label(
            AMBER,
            format!(
                "ECU {} · {}",
                ecu_label(failure.reading.source),
                failure.reading.status
            ),
        );
    }
    read
}

impl Workbench {
    #[cfg(windows)]
    pub(super) fn save_scan_notes_for_update(&mut self) -> bool {
        let Some(review) = self
            .scan_review
            .as_mut()
            .filter(|review| review.has_unsaved_notes())
        else {
            return true;
        };
        if !review.save_notes() {
            self.page = Page::ScanReview;
            self.status = format!(
                "Update waiting. {}",
                review
                    .notes_error
                    .as_deref()
                    .unwrap_or("Could not save the scan notes.")
            );
            return false;
        }
        if let (Some(path), Some(notes)) = (&review.path, &review.saved_notes) {
            self.scan_history.record_work_order(path, &notes.work_order);
        }
        true
    }

    pub(super) fn scan_review(&mut self, ui: &mut Ui) {
        let Some(review) = self.scan_review.as_mut() else {
            caption(ui, "Choose a scan from Scan history or Overview.");
            return;
        };
        ui.label(super::appearance::title("Scan review", 23.0));
        let back = match review.from {
            Page::ScanHistory => "Back to scan history",
            Page::Codes => "Back to trouble codes",
            _ => "Back to overview",
        };
        if ui.link(back).clicked() {
            self.page = review.from;
            if !review.has_unsaved_notes() {
                self.scan_review = None;
            }
            return;
        }
        if let Some(notes) = &review.saved_notes
            && !notes.work_order.is_empty()
        {
            caption(ui, &format!("Work order {}", notes.work_order));
        }
        let snapshot = review.snapshot.clone();
        let db = &self.db;
        let (print, saved) =
            review.show(ui, |code| Self::describe_code(db, &snapshot.vehicle, code));
        let path = review.path.clone();
        if saved && let (Some(path), Some(notes)) = (&path, &review.saved_notes) {
            self.scan_history.record_work_order(path, &notes.work_order);
            self.status = "Scan notes saved.".into();
        }
        if print {
            let result = self.save_scan_report(&snapshot, path.as_deref());
            if let Some(path) = self.report_saved(result) {
                open_local(&path);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn update_note_save_preserves_a_draft_when_another_window_is_saving() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("scan.json");
        let snapshot = Arc::new(Snapshot {
            vehicle: Default::default(),
            demo: true,
            at_ms: 1000,
            rows: vec![],
            failures: vec![],
            clear_requested_at_ms: None,
            emissions_scan: None,
        });
        super::super::model::save_json(&path, snapshot.as_ref()).unwrap();
        let evidence = std::fs::read(&path).unwrap();
        let mut review = Review::new(snapshot, Some(path.clone()), Page::ScanHistory);
        review.draft_notes.work_order = "WO-UPDATE".into();
        review.draft_notes.technician = "Readings saved before updating".into();
        let mut app = Workbench::new(folder.path().join("data"));
        app.scan_review = Some(review);
        let lock = super::super::model::lock_file(&path.with_extension("notes.lock")).unwrap();
        assert!(!app.save_scan_notes_for_update());
        assert!(app.scan_review.as_ref().unwrap().has_unsaved_notes());
        assert_eq!(app.page, Page::ScanReview);
        drop(lock);
        assert!(app.save_scan_notes_for_update());
        assert!(!app.scan_review.as_ref().unwrap().has_unsaved_notes());
        assert_eq!(scan_notes::load(&path).unwrap().work_order, "WO-UPDATE");
        assert_eq!(std::fs::read(path).unwrap(), evidence);
    }
}
