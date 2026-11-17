use super::appearance::{
    self, ACCENT, AMBER, BORDER, CANVAS, CHROME, GREEN, INK, MUTED, RAISED, RED, SELECTED, SURFACE,
};
use super::freeze_frames::{self, Frames, Links};
use super::readiness::{MonitorState, ecu_reports};
use super::worker::AttemptOutcome;
use super::*;
use crate::j2534::format_voltage;
use egui::{Color32, RichText, Stroke, Vec2};

fn action(ui: &mut egui::Ui, label: &str, enabled: bool) -> bool {
    let (fill, ink) = if enabled {
        (ACCENT, CHROME)
    } else {
        (SURFACE, MUTED)
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(appearance::label(label, 15.0).color(ink))
            .min_size(Vec2::new(104.0, 34.0))
            .corner_radius(6)
            .stroke(Stroke::NONE)
            .fill(fill),
    )
    .clicked()
}
pub(super) fn card<R>(
    ui: &mut egui::Ui,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(9)
        .inner_margin(16)
        .show(ui, add)
}
pub(super) fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().color(MUTED));
}

#[derive(Clone, Copy)]
enum ValueTable {
    Live,
    Data,
    FreezeFrame {
        trigger_at: Option<u64>,
        trigger_unverified: bool,
    },
}

pub(super) fn freeze_frame_link(ui: &mut egui::Ui, links: Links) -> Option<u8> {
    match links {
        Links::Matching(frames) => {
            let mut selected = None;
            let mut show_link = |ui: &mut egui::Ui, frame: &freeze_frames::MatchingFrame| {
                let link = ui.link(format!("Frame {}", frame.number));
                let link = if frame.has_values {
                    link
                } else {
                    link.on_hover_text("Only the trigger code is verified for this frame.")
                };
                if link.clicked() {
                    selected = Some(frame.number);
                    ui.close_menu();
                }
            };
            if let [frame] = frames.as_slice() {
                show_link(ui, frame);
            } else {
                ui.menu_button(format!("{} frames", frames.len()), |ui| {
                    for frame in &frames {
                        show_link(ui, frame);
                    }
                });
            }
            selected
        }
        Links::Empty(frames) => {
            caption(
                ui,
                &match frames.as_slice() {
                    [number] => format!("Frame {number} empty"),
                    _ => format!("{} empty frames", frames.len()),
                },
            );
            None
        }
        state => {
            caption(ui, state.label());
            None
        }
    }
}
pub(super) fn readiness_color(state: MonitorState) -> Color32 {
    match state {
        MonitorState::Ready => GREEN,
        MonitorState::NotReady | MonitorState::DisabledOrUnknown => AMBER,
        MonitorState::Disabled | MonitorState::Unsupported => MUTED,
    }
}
fn monitor_label(id: u8, can: bool) -> String {
    if !can {
        return format!("Test {id:02X}");
    }
    // Common CAN monitor IDs from SAE J1979 Appendix D. Other IDs stay in hex.
    let name = match id {
        0x01..=0x10 => format!(
            "Exhaust gas sensor B{}S{}",
            (id - 1) / 4 + 1,
            (id - 1) % 4 + 1
        ),
        0x21..=0x24 => format!("Catalyst bank {}", id - 0x20),
        0x31..=0x34 => format!("EGR bank {}", id - 0x30),
        0x35..=0x38 => format!("VVT bank {}", id - 0x34),
        0x39 => "EVAP cap off / 0.150 in".into(),
        0x3a => "EVAP 0.090 in".into(),
        0x3b => "EVAP 0.040 in".into(),
        0x3c => "EVAP 0.020 in".into(),
        0x3d => "EVAP purge flow".into(),
        0x41..=0x50 => format!(
            "Exhaust heater B{}S{}",
            (id - 0x41) / 4 + 1,
            (id - 0x41) % 4 + 1
        ),
        0x61..=0x64 => format!("Heated catalyst bank {}", id - 0x60),
        0x71..=0x74 => format!("Secondary air {}", id - 0x70),
        0x81..=0x84 => format!("Fuel system bank {}", id - 0x80),
        0x85..=0x86 => format!("Boost pressure bank {}", id - 0x84),
        0xa1 => "Misfire general data".into(),
        0xa2..=0xb1 => format!("Misfire cylinder {}", id - 0xa1),
        0xb2..=0xb3 => format!("Particulate filter bank {}", id - 0xb1),
        _ => return format!("Monitor {id:02X}"),
    };
    format!("{id:02X} · {name}")
}
fn display_value(r: &Reading) -> String {
    if r.value.parse::<f64>().is_ok() {
        r.numeric
            .map(|n| format!("{n:.2}"))
            .unwrap_or_else(|| r.value.clone())
    } else {
        r.value.clone()
    }
}
fn reading_label(r: &Reading) -> &str {
    match r.request.as_str() {
        "0902" => "VIN",
        "0904" => "Calibration ID",
        "0906" => "Calibration verification number",
        "090A" => "ECU name",
        "090D" => "Engine serial number",
        "090F" => "Emissions type approval",
        _ => &r.name,
    }
}
pub(super) fn open_local(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

impl Workbench {
    pub(super) fn draw_app(&mut self, ctx: &egui::Context, now: Instant) {
        if self.splash.visible(now) {
            appearance::splash(ctx);
            ctx.request_repaint_after(Duration::from_millis(40));
            return;
        }
        self.draw(ctx);
        #[cfg(windows)]
        self.show_startup_update(ctx, now);
    }

    pub(super) fn draw(&mut self, ctx: &egui::Context) {
        if self.page != Page::ScanReview
            && self
                .scan_review
                .as_ref()
                .is_some_and(|review| !review.has_unsaved_notes())
        {
            self.scan_review = None;
        }
        egui::TopBottomPanel::top("scanner_header")
            .frame(
                egui::Frame::new()
                    .fill(CHROME)
                    .inner_margin(egui::Margin::symmetric(20, 10)),
            )
            .show(ctx, |ui| self.header(ui));
        egui::TopBottomPanel::bottom("scanner_status")
            .frame(
                egui::Frame::new()
                    .fill(CHROME)
                    .inner_margin(egui::Margin::symmetric(14, 7)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if self.busy {
                        ui.spinner();
                        if self.connected
                            && !self.connecting
                            && ui
                                .add_enabled(
                                    !self.cancel.load(Ordering::Relaxed),
                                    egui::Button::new("Stop"),
                                )
                                .clicked()
                        {
                            self.stop_operation();
                        }
                    }
                    if self.busy && self.progress.1 > 0 {
                        ui.add(
                            egui::ProgressBar::new(self.progress.0 as f32 / self.progress.1 as f32)
                                .desired_width(104.0)
                                .text(format!("{} / {}", self.progress.0, self.progress.1)),
                        );
                    }
                    if self.busy && !self.progress.2.is_empty() {
                        caption(ui, &self.progress.2);
                    }
                    ui.add(
                        egui::Label::new(RichText::new(&self.status).small().color(MUTED)).wrap(),
                    );
                });
            });
        egui::TopBottomPanel::top("scanner_navigation")
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .inner_margin(egui::Margin::symmetric(20, 7)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for page in Page::NAVIGATION {
                        ui.push_id(page, |ui| self.nav_button(ui, page));
                    }
                });
                if self.page != Page::ScanReview
                    && self
                        .scan_review
                        .as_ref()
                        .is_some_and(|review| review.has_unsaved_notes())
                {
                    ui.horizontal(|ui| {
                        if ui
                            .push_id("unsaved_scan_notes_navigation", |ui| {
                                ui.link("Unsaved scan notes")
                            })
                            .inner
                            .clicked()
                        {
                            self.page = Page::ScanReview;
                        }
                    });
                }
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(CANVAS)
                    .inner_margin(egui::Margin::symmetric(26, 16)),
            )
            .show(ctx, |ui| {
                ui.label(appearance::title(self.page.title(), 32.0));
                let tabs = self.page.tabs();
                if !tabs.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for &(page, label) in tabs {
                            if ui.selectable_label(self.page == page, label).clicked() {
                                self.page = page;
                            }
                        }
                    });
                }
                ui.add_space(8.0);
                if self.page == Page::ScanReview {
                    self.scan_review(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt(("workspace_scroll", self.page))
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.page {
                        Page::Home => self.home(ui),
                        Page::Connection => self.connection(ui),
                        Page::VehicleInfo => self.vehicle_info(ui),
                        Page::Garage => self.garage(ui),
                        Page::Systems => self.systems(ui),
                        Page::Codes => self.codes(ui),
                        Page::Live => self.live_page(ui),
                        Page::Emissions => self.readiness(ui),
                        Page::FreezeFrame => self.freeze_frame(ui),
                        Page::MonitorTests => self.monitor_tests(ui),
                        Page::Recordings => self.recordings(ui),
                        Page::ScanHistory => self.scan_history(ui),
                        Page::ScanReview => {
                            unreachable!("scan review scrolls its results below the controls")
                        }
                        Page::Reports => self.reports(ui),
                        Page::Help => self.help(ui),
                        Page::Updates => self.update_controls(ui),
                    });
            });
        self.show_live_preset_dialog(ctx);
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            appearance::logo(ui, 36.0);
            ui.label(appearance::title("Scanner Workbench", 24.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.connecting {
                    if ui
                        .add_enabled(
                            !self.cancel.load(Ordering::Relaxed),
                            egui::Button::new("Cancel").min_size(Vec2::new(104.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.stop_operation();
                    }
                } else if self.connected && !self.demo {
                    if ui
                        .add(egui::Button::new("Disconnect").min_size(Vec2::new(104.0, 34.0)))
                        .clicked()
                    {
                        self.disconnect();
                    }
                } else if action(ui, "Connect", !self.busy) {
                    self.connect();
                }

                let sample = self.voltage.as_ref().filter(|sample| {
                    sample.sampled_at.elapsed() < connection_report::VOLTAGE_FRESHNESS
                });
                let voltage =
                    sample.map_or_else(|| "—".into(), |sample| format_voltage(sample.millivolts));
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(7)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.add_sized(
                            [74.0, 22.0],
                            egui::Label::new(RichText::new(voltage).monospace().size(19.0))
                                .halign(egui::Align::Max),
                        );
                        caption(ui, "DLC voltage");
                    })
                    .response
                    .on_hover_ui(|ui| {
                        ui.label("Voltage reported by the adapter");
                        if let Some(sample) = sample {
                            ui.label(sample.adapter.label());
                        }
                    });

                self.header_vin(ui);

                let (label, color) = if self.connecting {
                    ("Connecting", AMBER)
                } else if self.playback.is_some() {
                    ("Replay", AMBER)
                } else if self.demo && self.connected {
                    ("Practice", AMBER)
                } else if self.connected {
                    if self.vehicle_reply.is_some_and(|reply| reply.missed) {
                        ("No vehicle reply", AMBER)
                    } else {
                        ("Connected", GREEN)
                    }
                } else {
                    ("Disconnected", MUTED)
                };
                let state = ui.label(appearance::label(label, 15.0).color(color));
                if self.vehicle_reply.is_some_and(|reply| reply.missed)
                    || self.connection_error.is_some() && !self.connected
                {
                    let link = ui.link("Details").on_hover_text("Open connection details");
                    if link.clicked() {
                        self.page = Page::Connection;
                    }
                } else if let Some(driver) = &self.driver {
                    state.on_hover_text(driver);
                }
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(6.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.0, color);
                if self.recording.is_some() {
                    ui.colored_label(RED, "Recording");
                }
            });
        });
    }

    fn header_vin(&mut self, ui: &mut egui::Ui) {
        let vin = self
            .connected
            .then_some(self.observed_vin.as_deref())
            .flatten();
        let text = match vin {
            Some(vin) => vin,
            None if self.vin_request.is_some() => "Reading…",
            None if self.connected && self.vin_conflict => "Conflicting replies",
            None if self.connected && !self.demo => "Unavailable",
            None => "—",
        };
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(204.0, 50.0),
            if vin.is_some() {
                egui::Sense::click()
            } else {
                egui::Sense::hover()
            },
        );
        let now = Instant::now();
        if let Some(vin) = vin {
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Copy VIN {vin}"))
            });
            if response.clicked() {
                ui.ctx().copy_text(vin.to_owned());
                self.vin_copied_until = Some(now + Duration::from_millis(1600));
            }
        }
        let copied = vin.is_some() && self.vin_copied_until.is_some_and(|end| end > now);
        let highlighted = vin.is_some() && (response.hovered() || response.has_focus());
        let painter = ui.painter();
        painter.rect(
            rect,
            7,
            SURFACE,
            Stroke::new(1.0_f32, if highlighted { ACCENT } else { BORDER }),
            egui::StrokeKind::Inside,
        );
        let line = rect.min + Vec2::new(10.0, 7.0);
        let number = painter.layout_no_wrap(
            text.into(),
            egui::FontId::monospace(15.0),
            if vin.is_some() { INK } else { MUTED },
        );
        if highlighted {
            painter.rect_filled(
                egui::Rect::from_min_size(line, number.size()).expand2(Vec2::new(3.0, 1.0)),
                3,
                SELECTED,
            );
        }
        painter.galley(line, number, INK);
        painter.text(
            rect.left_bottom() + Vec2::new(10.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            "VIN",
            egui::FontId::proportional(13.0),
            MUTED,
        );
        if copied {
            painter.text(
                rect.right_bottom() + Vec2::new(-10.0, -6.0),
                egui::Align2::RIGHT_BOTTOM,
                "Copied",
                egui::FontId::proportional(13.0),
                GREEN,
            );
            if let Some(end) = self.vin_copied_until {
                ui.ctx()
                    .request_repaint_after(end.saturating_duration_since(now));
            }
        }
        if vin.is_some() {
            response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(if copied {
                    "Copied"
                } else {
                    "Click to copy VIN"
                });
        } else if self.connected && self.vin_conflict {
            response.on_hover_text("ECUs reported different VINs. Review Vehicle Information.");
        } else if self.connected && !self.demo && self.vin_request.is_none() {
            response.on_hover_text("No VIN was received. Use Read VIN under Vehicle Information.");
        }
    }

    fn nav_button(&mut self, ui: &mut egui::Ui, page: Page) {
        #[cfg(windows)]
        if page == Page::Updates && self.update_ready() {
            let button = egui::Button::new(appearance::label("Updates", 15.0).color(CHROME))
                .fill(ACCENT)
                .stroke(Stroke::NONE)
                .min_size(Vec2::new(82.0, 32.0));
            if ui
                .add(button)
                .on_hover_text("An update is ready. Open Updates for installation and patch notes.")
                .clicked()
            {
                self.page = page;
            }
            return;
        }
        let selected = self.page.section() == page;
        let label =
            appearance::label(page.title(), 15.0).color(if selected { ACCENT } else { INK });
        let button = egui::Button::new(label)
            .fill(if selected { SELECTED } else { SURFACE })
            .stroke(if selected {
                Stroke::new(1.0_f32, ACCENT)
            } else {
                Stroke::NONE
            })
            .min_size(Vec2::new(82.0, 32.0));
        if ui.add(button).clicked() {
            self.page = page;
        }
    }
    fn home(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new((ui.available_width() - 180.0).max(320.0), 65.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let vehicle = self.vehicle.label();
                        let title = if self.playback.is_some() {
                            "Recording playback"
                        } else if self.demo && self.connected {
                            "Practice session"
                        } else if self.connected && vehicle != "Unidentified vehicle" {
                            &vehicle
                        } else if self.connected {
                            "Vehicle connected"
                        } else {
                            "Connect your adapter"
                        };
                        ui.label(appearance::title(title, 27.0));
                        if let Some(driver) = &self.driver {
                            caption(ui, &format!("{driver} · {}", self.protocol));
                            if valid_vin(&self.vehicle.vin) {
                                ui.monospace(&self.vehicle.vin);
                            }
                        } else if self.playback.is_some() {
                            caption(ui, &vehicle);
                        } else if self.demo && self.connected {
                            caption(ui, "Simulated diagnostic data");
                        } else {
                            caption(ui, "Plug in the cable and turn the ignition on before pressing Connect.");
                        }
                    },
                );
                ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                    if self.driver.is_some() {
                        if ui.add_enabled(!self.busy, egui::Button::new("Read VIN")).clicked() {
                            self.page = Page::VehicleInfo;
                            self.once(vec![self.base_query(9, Some(2))]);
                        }
                    } else if self.playback.is_some() {
                        if ui.button("Open recordings").clicked() {
                            self.page = Page::Recordings;
                        }
                    } else if self.demo && self.connected {
                        if ui.button("End practice").clicked() {
                            self.disconnect();
                        }
                    } else {
                        if ui.add_enabled(!self.busy, egui::Button::new("Start practice")).clicked() {
                            self.start_demo();
                        }
                        if self.has_connection_details() && ui.link("Connection details").clicked() {
                            self.page = Page::Connection;
                        }
                    }
                });
            });
        });
        ui.add_space(9.0);
        let ready = self.connected && !self.busy;
        ui.columns(3, |columns| {
            for (ui, (page, button)) in columns.iter_mut().zip([
                (Page::Codes, "Read codes"),
                (Page::Live, "Open live data"),
                (Page::Emissions, "Read readiness"),
            ]) {
                let button = egui::Button::new(appearance::label(button, 16.0))
                    .fill(RAISED)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .min_size(Vec2::new(ui.available_width(), 42.0));
                if ui
                    .add_enabled(ready || page == Page::Live, button)
                    .clicked()
                {
                    self.page = page;
                    match page {
                        Page::Codes => self.read_codes(),
                        Page::Emissions => {
                            self.readiness_period = ReadinessPeriod::SinceClear;
                            self.read_readiness();
                        }
                        _ => {}
                    }
                }
            }
        });
        ui.add_space(10.0);
        self.emissions_scan_panel(ui, ready);
    }
    fn emissions_scan_panel(&mut self, ui: &mut egui::Ui, ready: bool) {
        use emissions_scan::{Check, State};
        let mut follow = None;
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(appearance::title("Emissions scan", 23.0));
            ui.horizontal_wrapped(|ui| {
                if action(ui, "Run emissions scan", ready) {
                    self.read_emissions_scan();
                }
                if let Some(scan) = &self.results.emissions_scan {
                    let finished = scan.state != State::Running;
                    let status = if scan.state == State::Running && self.playback.is_some() {
                        "In progress at replay position"
                    } else {
                        scan.state.label()
                    };
                    caption(ui, status);
                    if let Some(stage) = scan.clear_stage {
                        caption(ui, stage.label());
                    }
                    caption(ui, &recordings::recording_date(scan.started_ms));
                    if finished {
                        if ui.button("Review scan").clicked()
                            && let Some((snapshot, path)) = self.latest_scan_snapshot()
                        {
                            self.open_scan_review(snapshot, path, Page::Home);
                        }
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("Print scan report"))
                            .clicked()
                            && let Some(path) = self.export_latest_scan()
                        {
                            open_local(&path);
                        }
                    }
                    if ui.button("Open scan history").clicked() {
                        self.page = Page::ScanHistory;
                    }
                    if self.scan_history.pending_count() > 0 {
                        ui.colored_label(AMBER, "Scan waiting to be saved");
                    }
                } else {
                    let has_scan_readings = self
                        .results
                        .rows
                        .values()
                        .map(|row| &row.latest)
                        .chain(self.results.failures.values())
                        .any(|row| super::report_sections::is_scan_reading(&row.reading));
                    if has_scan_readings {
                        if action(ui, "Print session report", self.can_export_report()) {
                            if let Some(path) = self.export_report() {
                                open_local(&path);
                            }
                        }
                    } else {
                        caption(
                            ui,
                            "Read codes, readiness, VIN and freeze frame in one pass.",
                        );
                    }
                }
            });
            if let Some(scan) = &self.results.emissions_scan {
                egui::Grid::new("emissions_scan_summary")
                    .num_columns(2)
                    .striped(true)
                    .spacing(Vec2::new(18.0, 5.0))
                    .show(ui, |ui| {
                        for (check, result) in scan.checks() {
                            if ui.link(check.label()).clicked() {
                                follow = Some(check);
                            }
                            let text = scan.check_label(result);
                            if result.is_some_and(|r| r.attention()) {
                                ui.colored_label(AMBER, text);
                            } else {
                                ui.label(text);
                            }
                            ui.end_row();
                        }
                    });
            }
        });
        match follow {
            Some(Check::Readiness) => {
                self.readiness_period = ReadinessPeriod::SinceClear;
                self.page = Page::Emissions;
            }
            Some(Check::DriveCycle) => {
                self.readiness_period = ReadinessPeriod::DriveCycle;
                self.page = Page::Emissions;
            }
            Some(Check::FreezeFrame) => {
                self.frame = 0;
                self.frame_ecu = None;
                self.page = Page::FreezeFrame;
            }
            Some(Check::Vin) => self.page = Page::VehicleInfo,
            Some(check) => {
                self.code_category = Some(match check {
                    Check::Stored => codes::Category::Stored,
                    Check::Pending => codes::Category::Pending,
                    Check::Permanent => codes::Category::Permanent,
                    _ => unreachable!(),
                });
                self.page = Page::Codes;
            }
            None => {}
        }
    }

    fn connection(&mut self, ui: &mut egui::Ui) {
        if self.connection_error.is_some() && self.vehicle_reply_check.is_none() {
            card(ui, |ui| self.connection_attempts(ui));
            ui.add_space(8.0);
        }
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if let Some(driver) = &self.driver {
                ui.label(RichText::new(driver).size(18.0).strong());
                caption(ui, &self.protocol);
            }
            self.adapter_picker(ui);
            if let Some(reply) = self.vehicle_reply {
                ui.horizontal_wrapped(|ui| {
                    let age = reply.last_reply.elapsed().as_secs();
                    ui.label(
                        RichText::new(format!("Last vehicle reply {age} s ago"))
                            .small()
                            .color(if reply.missed { AMBER } else { MUTED }),
                    );
                    if ui
                        .add_enabled(
                            self.can_check_vehicle_reply(),
                            egui::Button::new("Check vehicle reply"),
                        )
                        .clicked()
                    {
                        self.check_vehicle_reply();
                    }
                });
            }
            if let Some((checked_at, result)) = &self.vehicle_reply_check {
                use super::worker::ReplyCheckOutcome;
                ui.horizontal_wrapped(|ui| {
                    ui.strong("Last vehicle reply check");
                    caption(ui, &format!("{} s ago", checked_at.elapsed().as_secs()));
                    if ui.button("Copy reply check").clicked() {
                        ui.ctx().copy_text(format!(
                            "{}\nRequest 0100\n{}",
                            result.protocol.label(),
                            result.summary()
                        ));
                    }
                });
                let color = if result.outcome == ReplyCheckOutcome::Verified {
                    GREEN
                } else {
                    AMBER
                };
                ui.colored_label(color, result.outcome.label());
                for reply in &result.responses {
                    ui.add(egui::Label::new(RichText::new(reply.label()).color(MUTED)).wrap());
                }
            }
            if self.adapter_kind == AdapterKind::J2534 {
                ui.collapsing("Protocol override", |ui| {
                ui.add_enabled_ui(!self.connected && !self.busy, |ui| {
                    egui::ComboBox::from_id_salt("scanner_protocol")
                        .width(325.0)
                        .selected_text(self.protocol_override.map_or("Automatic", CanProtocol::label))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.protocol_override, None, "Automatic");
                            for protocol in CanProtocol::ALL {
                                ui.selectable_value(&mut self.protocol_override, Some(protocol), protocol.label());
                            }
                        });
                    if self.protocol_override.is_none_or(CanProtocol::extended) {
                        ui.horizontal(|ui| {
                            let mut use_override = self.ecu_source.is_some();
                            if ui.checkbox(&mut use_override, "Use a known 29-bit ECU source")
                                .on_hover_text("Limits Connect to 29-bit CAN while enabled.").changed() {
                                self.ecu_source = use_override.then_some(0x10);
                            }
                            if let Some(ecu) = &mut self.ecu_source {
                                ui.add(egui::DragValue::new(ecu).hexadecimal(2, false, true));
                            }
                        });
                    }
                });
                caption(ui, "Automatic tries the last verified protocol for the driver first. An override limits Connect to the selected protocol until the app closes.");
                });
            }
        });
        if self.has_connection_details()
            && (self.connection_error.is_none() || self.vehicle_reply_check.is_some())
        {
            ui.add_space(8.0);
            card(ui, |ui| self.connection_attempts(ui));
        }
        self.log_view(ui);
        ui.collapsing("About this scanner", |ui| {
            ui.label(if VERSION == "development" {
                "Development build".to_owned()
            } else {
                format!("Version {VERSION}")
            });
            ui.hyperlink_to(
                "OBDb source definitions / CC BY-SA 4.0",
                "https://github.com/OBDb",
            );
            caption(
                ui,
                &format!(
                    "Local fault descriptions · {} makes / {} codes",
                    self.db.make_count(),
                    self.db.code_count()
                ),
            );
        });
    }

    fn adapter_picker(&mut self, ui: &mut egui::Ui) {
        let enabled = !self.busy && (!self.connected || self.demo);
        let previous = self.adapter_kind;
        let mut driver_selected = None;
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                egui::ComboBox::from_id_salt("workbench_adapter_type")
                    .width(218.0)
                    .selected_text(self.adapter_kind.label())
                    .show_ui(ui, |ui| {
                        for kind in AdapterKind::ALL {
                            ui.selectable_value(&mut self.adapter_kind, kind, kind.label());
                        }
                    })
                    .response
                    .on_hover_text("Adapter type");
                if previous == AdapterKind::J2534 && self.adapter_kind == AdapterKind::J2534 {
                    driver_selected = self.driver_controls(ui);
                }
            });
        });
        if previous != self.adapter_kind {
            self.adapter_problem = None;
            if self.adapter_kind == AdapterKind::J2534 {
                self.apply_adapter_choice();
            }
        }
        if self.adapter_kind == AdapterKind::J2534 {
            if let Some(driver) = driver_selected {
                self.select_driver(driver);
            }
            self.driver_status(ui, enabled);
            if (self.elm_connection.is_some() || self.settings.elm_connection.is_some())
                && ui
                    .add_enabled(enabled, egui::Button::new("Use J2534"))
                    .clicked()
            {
                self.apply_adapter_choice();
            }
        } else {
            ui.add_enabled_ui(enabled, |ui| match self.adapter_kind {
                AdapterKind::Serial => {
                    let ports = self
                        .serial_ports
                        .get_or_insert_with(crate::elm327::scan_ports);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("COM / serial port");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.serial_port)
                                .desired_width(112.0)
                                .hint_text("COM port"),
                        );
                        egui::ComboBox::from_id_salt("workbench_serial_ports")
                            .selected_text("Available ports")
                            .show_ui(ui, |ui| {
                                for port in ports.iter() {
                                    ui.selectable_value(&mut self.serial_port, port.clone(), port);
                                }
                            });
                        if ui.button("Refresh ports").clicked() {
                            *ports = crate::elm327::scan_ports();
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Baud rate");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.serial_baud)
                                .desired_width(112.0)
                                .hint_text("Automatic"),
                        );
                    });
                    caption(
                        ui,
                        "USB and paired Bluetooth adapters can appear as a serial port.",
                    );
                }
                AdapterKind::Tcp => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Adapter address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.tcp_address)
                                .desired_width(225.0)
                                .hint_text("192.168.0.10:35000"),
                        );
                    });
                    caption(ui, "Use the host and port specified by your adapter.");
                }
                AdapterKind::J2534 => unreachable!("J2534 has its own picker"),
            });
            if self.elm_connection_draft().ok().as_ref() == Some(&self.elm_connection)
                && self.settings.elm_connection == self.elm_connection
                && self.adapter_problem.is_none()
            {
                caption(ui, "Saved for Connect and DLC voltage.");
            } else if ui
                .add_enabled(enabled, egui::Button::new("Use this adapter"))
                .clicked()
            {
                self.apply_adapter_choice();
            }
        }
        if let Some(problem) = &self.adapter_problem {
            ui.colored_label(AMBER, problem);
        }
    }

    fn driver_controls(&mut self, ui: &mut egui::Ui) -> Option<Option<DriverInfo>> {
        const AUTOMATIC: &str = "Automatic";
        let mut change = None;
        let drivers = self.installed_drivers.get_or_insert_with(discover_drivers);
        let current = self.driver_override.as_ref();
        let installed = current.and_then(|selected| {
            drivers
                .iter()
                .find(|driver| *driver == selected)
                .or_else(|| {
                    drivers
                        .iter()
                        .find(|driver| same_driver_path(&driver.path, &selected.path))
                })
        });
        ui.label("Driver");
        egui::ComboBox::from_id_salt("scanner_driver")
            .width(230.0)
            .selected_text(
                installed
                    .or(current)
                    .map_or(AUTOMATIC, |driver| &driver.name),
            )
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_none(), AUTOMATIC).clicked() {
                    change = Some(None);
                }
                for driver in drivers.iter() {
                    if ui
                        .selectable_label(installed == Some(driver), &driver.name)
                        .on_hover_text(driver.path.display().to_string())
                        .clicked()
                    {
                        change = Some(Some(driver.clone()));
                    }
                }
            });
        if ui.button("Refresh drivers").clicked() {
            *drivers = discover_drivers();
        }
        change
    }

    fn driver_status(&mut self, ui: &mut egui::Ui, enabled: bool) {
        if self.driver.is_some() {
            return;
        }
        let unsaved = self.driver_save_error.is_some()
            || self.settings.preferred_driver != self.driver_override;
        if let Some(driver) = &self.driver_override
            && self.installed_drivers.as_ref().is_some_and(|installed| {
                !installed
                    .iter()
                    .any(|entry| same_driver_path(&entry.path, &driver.path))
            })
        {
            ui.colored_label(
                AMBER,
                "Selected driver is missing. Reinstall it and refresh, or choose another driver.",
            )
            .on_hover_text(driver.path.display().to_string());
        } else if self.installed_drivers.as_ref().is_some_and(Vec::is_empty) {
            caption(ui, "No matching J2534 04.04 drivers found.");
        } else if !unsaved && self.driver_override.is_some() {
            caption(ui, "Saved for Connect and DLC voltage.");
        }
        if unsaved {
            ui.horizontal_wrapped(|ui| {
                let status = ui.colored_label(AMBER, "Driver choice is active for this session.");
                if let Some(error) = &self.driver_save_error {
                    status.on_hover_text(error);
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Save driver choice"))
                    .clicked()
                {
                    self.save_driver_preference();
                }
            });
        }
    }

    #[cfg(windows)]
    pub(super) fn update_ready(&self) -> bool {
        self.updates
            .as_ref()
            .is_some_and(|updates| matches!(updates.state(), updates::State::Ready(_)))
    }

    #[cfg(windows)]
    pub(super) fn show_startup_update(&mut self, ctx: &egui::Context, now: Instant) -> bool {
        if !matches!(self.splash, StartupSplash::Hidden) {
            return false;
        }
        if self.page != Page::Home || self.update_restart_blocker().is_some() {
            self.startup_update.dismiss();
            return false;
        }
        let Some(seconds) = self.startup_update.remaining(self.update_ready(), now) else {
            return false;
        };
        let version = match self.updates.as_ref().map(updates::Updates::state) {
            Some(updates::State::Ready(update)) => update.version().to_owned(),
            _ => return false,
        };
        let response = egui::Modal::new(egui::Id::new("startup_update"))
            .frame(
                egui::Frame::popup(&ctx.style())
                    .fill(SURFACE)
                    .inner_margin(24)
                    .corner_radius(10),
            )
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.label(appearance::title("Update ready", 27.0).color(ACCENT));
                ui.add_space(8.0);
                ui.label(format!("Scanner Workbench {version} is ready to install."));
                ui.strong("This process is automatic. No action is needed.");
                ui.label("Scanner Workbench will install the update and restart on its own.");
                ui.add_space(14.0);
                ui.label(appearance::label(
                    match seconds {
                        0 => "Updating and restarting…".into(),
                        1 => "Updating in 1 second…".into(),
                        _ => format!("Updating in {seconds} seconds…"),
                    },
                    21.0,
                ));
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let restart = action(ui, "Update now", true);
                    let later = ui.button("Later").clicked();
                    (restart, later)
                })
                .inner
            });
        let (restart, later) = response.inner;
        if later || response.should_close() {
            self.startup_update.dismiss();
        } else if restart || seconds == 0 {
            self.startup_update.dismiss();
            self.restart_for_update(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        true
    }

    fn update_controls(&mut self, ui: &mut egui::Ui) {
        #[cfg(windows)]
        self.release_notes.load_latest();
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(appearance::title("Software updates", 23.0));
            if VERSION == "development" {
                caption(ui, "Development build");
            } else {
                caption(ui, &format!("Running version {VERSION}"));
            }
            #[cfg(windows)]
            self.installed_update_controls(ui);
            #[cfg(not(windows))]
            caption(
                ui,
                "Automatic updates are available in the Windows installer.",
            );
        });
        ui.add_space(12.0);
        self.release_history(ui);
    }

    fn release_history(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(appearance::title("Patch notes", 23.0));
            caption(
                ui,
                "Changes in every published Scanner Workbench version, newest first.",
            );
            #[cfg(windows)]
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !self.release_notes.loading(),
                        egui::Button::new("Refresh patch notes"),
                    )
                    .clicked()
                {
                    self.release_notes.refresh();
                }
                if self.release_notes.loading() {
                    ui.spinner();
                    caption(ui, "Checking for new patch notes…");
                }
            });
            if let Some(error) = &self.release_notes.error {
                ui.add(egui::Label::new(RichText::new(error).small().color(AMBER)).wrap());
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.release_notes.query)
                    .hint_text("Search versions or changes")
                    .desired_width(f32::INFINITY),
            );
            let query = self.release_notes.query.trim().to_lowercase();
            let mut found = false;
            for (index, release) in self.release_notes.entries().iter().enumerate() {
                if !query.is_empty()
                    && !format!("{} {}", release.version, release.notes)
                        .to_lowercase()
                        .contains(&query)
                {
                    continue;
                }
                found = true;
                let current = release.version == VERSION;
                let heading = format!(
                    "Version {}{}",
                    release.version,
                    if current { " · Installed" } else { "" }
                );
                egui::CollapsingHeader::new(RichText::new(heading).strong())
                    .id_salt(("release_notes", &release.version))
                    .default_open(index == 0 || current)
                    .show(ui, |ui| {
                        if let Some(date) = release.date_label() {
                            caption(ui, &date);
                        }
                        for paragraph in release.notes.split("\n\n") {
                            ui.add(egui::Label::new(paragraph.trim().replace("**", "")).wrap());
                        }
                        ui.hyperlink_to("View this release on GitHub", release.url());
                    });
            }
            if !found {
                caption(ui, "No versions match your search.");
            }
            ui.hyperlink_to("All releases on GitHub", release_notes::releases_url());
        });
    }

    #[cfg(windows)]
    fn installed_update_controls(&mut self, ui: &mut egui::Ui) {
        use super::updates::{REPOSITORY, State};

        let stopping = self.connected || self.busy || self.live;
        let Some(updates) = &mut self.updates else {
            caption(
                ui,
                "Install Scanner Workbench from Setup to enable automatic updates.",
            );
            if let Some(repository) = REPOSITORY {
                ui.hyperlink_to(
                    "Download Windows Setup",
                    format!("https://github.com/{repository}/releases/latest/download/ScannerWorkbenchApp-win-x86-Setup.exe"),
                );
            }
            return;
        };
        match updates.state() {
            State::Idle | State::Current => {
                if matches!(updates.state(), State::Current) {
                    ui.colored_label(GREEN, "You’re up to date.");
                }
                if ui.button("Check for updates").clicked() {
                    updates.check();
                }
            }
            State::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking for updates…");
                });
            }
            State::Downloading { version, percent } => {
                if *percent == 100 {
                    ui.label(format!("Verifying version {version}…"));
                } else {
                    ui.label(format!("Downloading version {version}…"));
                }
                ui.add(
                    egui::ProgressBar::new(f32::from(*percent) / 100.0)
                        .desired_width(280.0)
                        .show_percentage(),
                );
            }
            State::Ready(update) => {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
                ui.colored_label(ACCENT, format!("Version {} is ready", update.version()));
                if ui
                    .push_id("restart_ready_update", |ui| {
                        action(
                            ui,
                            if stopping {
                                "Stop diagnostics and restart to update"
                            } else {
                                "Restart to update"
                            },
                            true,
                        )
                    })
                    .inner
                {
                    self.restart_for_update(ui.ctx());
                }
            }
            State::Failed(message) => {
                ui.colored_label(AMBER, "Update failed.");
                ui.collapsing("Error details", |ui| caption(ui, message));
                if ui.button("Retry update").clicked() {
                    updates.check();
                }
            }
        }
        if let Some(repository) = REPOSITORY {
            ui.hyperlink_to(
                "Release downloads",
                format!("https://github.com/{repository}/releases/latest"),
            );
        }
    }

    fn has_connection_details(&self) -> bool {
        self.connection_report.is_some() || self.connection_error.is_some()
    }

    fn connection_attempts(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(ui.available_width());
        let Some(report) = &self.connection_report else {
            if let Some(error) = &self.connection_error {
                ui.add(egui::Label::new(RichText::new(error).color(AMBER)).wrap());
            }
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.strong("Connection attempts");
            let copied = self
                .connection_copied_until
                .is_some_and(|until| Instant::now() < until);
            if ui
                .add_enabled(
                    report.is_finished(),
                    egui::Button::new(if copied {
                        "Copied"
                    } else {
                        "Copy connection report"
                    }),
                )
                .on_hover_text(
                    "Includes adapter settings, DLC voltage and diagnostic connection results",
                )
                .clicked()
            {
                let reply_check = self.vehicle_reply_check.as_ref().map(|(_, check)| check);
                ui.ctx().copy_text(report.text(reply_check));
                self.connection_copied_until = Some(Instant::now() + Duration::from_millis(1600));
            }
        });
        if let Some(result) = report.result() {
            let color = if self.connection_error.is_some() {
                AMBER
            } else {
                GREEN
            };
            ui.add(egui::Label::new(RichText::new(result).color(color)).wrap());
        }
        if let Some(voltage) = report.voltage_label() {
            caption(ui, &format!("Last DLC voltage at Connect · {voltage}"));
        }
        if report.is_finished()
            && let Some(step) = report.next_step()
        {
            caption(ui, step);
        }
        if report.attempts.is_empty() {
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("connection_attempts")
            .max_height(165.0)
            .auto_shrink([false, true])
            .stick_to_bottom(self.adapter_connect.is_some())
            .show(ui, |ui| {
                for (index, attempt) in report.attempts.iter().enumerate() {
                    if index > 0 {
                        ui.separator();
                    }
                    let color = match attempt.outcome {
                        AttemptOutcome::Verified => ACCENT,
                        AttemptOutcome::Opened | AttemptOutcome::Cancelled => MUTED,
                        _ => AMBER,
                    };
                    ui.horizontal_wrapped(|ui| {
                        if let Some(protocol) = attempt.protocol {
                            ui.strong(protocol.short_label());
                            caption(ui, &attempt.driver);
                            if attempt.remembered_protocol {
                                caption(ui, "Last verified protocol");
                            }
                        } else {
                            ui.strong(&attempt.driver);
                        }
                        ui.colored_label(color, attempt.outcome.label());
                    });
                    if let Some(detail) = &attempt.detail {
                        ui.add(egui::Label::new(RichText::new(detail).small().color(MUTED)).wrap());
                    }
                }
            });
    }

    fn vehicle_info(&mut self, ui: &mut egui::Ui) {
        let ready = self.connected && !self.busy && self.playback.is_none();
        ui.horizontal_wrapped(|ui| {
            if action(ui, "Read vehicle information", ready) {
                self.read_vehicle_information();
            }
            if ui
                .add_enabled(ready, egui::Button::new("Read VIN"))
                .clicked()
            {
                self.once(vec![self.base_query(9, Some(2))]);
            }
        });
        if let Some(support) = &self.vehicle_info_support {
            if let Some(problem) = &support.problem {
                ui.colored_label(AMBER, "Vehicle information discovery incomplete.")
                    .on_hover_text(problem);
            }
        }
        let mut ecus = std::collections::BTreeMap::<_, (Vec<_>, Vec<_>)>::new();
        for value in self.results.rows.values() {
            let row = &value.latest;
            if row.reading.request.starts_with("09") {
                ecus.entry(row.reading.source).or_default().0.push(row);
            }
        }
        for issue in self.results.failures.values() {
            if issue.reading.request.starts_with("09") {
                ecus.entry(issue.reading.source).or_default().1.push(issue);
            }
        }
        let vin_conflict = self.vin_conflict || self.results.reported_vin().1;
        if vin_conflict {
            ui.colored_label(AMBER, "ECUs reported different VINs.");
        }
        if ecus.is_empty() {
            caption(
                ui,
                "Read the VIN, calibrations and in-use performance data.",
            );
        }
        for (source, (mut rows, issues)) in ecus {
            rows.sort_by_key(|row| &row.reading.request);
            card(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.strong(format!("ECU {}", ecu_label(source)));
                egui::Grid::new(("vehicle_information", source))
                    .num_columns(2)
                    .spacing(Vec2::new(14.0, 6.0))
                    .striped(true)
                    .show(ui, |ui| {
                        for row in rows
                            .iter()
                            .filter(|row| !matches!(row.reading.request.as_str(), "0908" | "090B"))
                        {
                            let r = &row.reading;
                            ui.label(reading_label(r));
                            ui.horizontal_wrapped(|ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(&r.value).monospace())
                                        .selectable(true),
                                )
                                .on_hover_text(&r.raw);
                                if self.results.refresh_failed(row) {
                                    ui.colored_label(AMBER, "Refresh failed");
                                } else if r.status == "Legacy message count unavailable" {
                                    ui.colored_label(AMBER, "Message count unavailable");
                                }
                            });
                            ui.end_row();
                        }
                    });
                for (request, label) in [
                    ("0908", "In-use performance (spark)"),
                    ("090B", "In-use performance (compression)"),
                ] {
                    let counters: Vec<_> = rows
                        .iter()
                        .filter(|row| row.reading.request == request)
                        .collect();
                    if counters.is_empty() {
                        continue;
                    }
                    ui.collapsing(label, |ui| {
                        if counters.iter().any(|row| self.results.refresh_failed(row)) {
                            ui.colored_label(AMBER, "Refresh failed");
                        } else if counters.iter().any(|row| {
                            row.reading
                                .status
                                .contains("Legacy message count unavailable")
                        }) {
                            ui.colored_label(AMBER, "Message count unavailable");
                        }
                        caption(ui, "Completions / conditions encountered");
                        egui::Grid::new(("vehicle_performance", source, request))
                            .num_columns(2)
                            .striped(true)
                            .show(ui, |ui| {
                                for row in counters {
                                    ui.label(&row.reading.name);
                                    ui.monospace(&row.reading.value)
                                        .on_hover_text(&row.reading.raw);
                                    ui.end_row();
                                }
                            });
                    });
                }
                for issue in issues {
                    let r = &issue.reading;
                    ui.colored_label(AMBER, format!("{} · {}", reading_label(r), r.status))
                        .on_hover_text(&r.raw);
                }
            });
            ui.add_space(6.0);
        }
    }
    fn garage(&mut self, ui: &mut egui::Ui) {
        if !self.settings.garage.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label("Saved vehicles");
                egui::ComboBox::from_id_salt("saved_vehicle")
                    .width(320.0)
                    .selected_text("Choose a saved vehicle")
                    .show_ui(ui, |ui| {
                        for vehicle in &self.settings.garage {
                            let label = format!("{}  {}", vehicle.label(), vehicle.registration);
                            if ui.selectable_label(false, label).clicked() {
                                self.draft = vehicle.clone();
                            }
                        }
                    });
            });
        }
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Make");
                ui.add(egui::TextEdit::singleline(&mut self.draft.make).desired_width(145.0));
                ui.label("Model");
                ui.add(egui::TextEdit::singleline(&mut self.draft.model).desired_width(160.0));
                ui.label("Year");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.year)
                        .desired_width(65.0)
                        .hint_text("YYYY"),
                );
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("VIN");
                ui.add(egui::TextEdit::singleline(&mut self.draft.vin).desired_width(240.0));
                if let Some(vin) = &self.observed_vin {
                    if self.connected && ui.button("Use read VIN").clicked() {
                        self.draft.vin = vin.clone();
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Plate / fleet ID");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.registration).desired_width(140.0),
                );
                ui.label("Odometer + unit");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.odometer)
                        .desired_width(150.0)
                        .hint_text("e.g. 125000 mi"),
                );
            });
            ui.label("Work order / technician notes");
            ui.add(
                egui::TextEdit::multiline(&mut self.draft.notes)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal_wrapped(|ui| {
                if action(
                    ui,
                    "Use for this session",
                    self.connected && !self.busy && !self.demo,
                ) {
                    self.apply_vehicle();
                }
                if ui.button("Save to garage").clicked() {
                    self.save_garage();
                }
            });
        });
    }
    fn systems(&mut self, ui: &mut egui::Ui) {
        if action(ui, "Check OBD-II ECUs", self.connected && !self.busy) {
            self.read_readiness();
        }
        let values: Vec<_> = self
            .results
            .rows
            .values()
            .filter(|v| {
                let r = &v.latest.reading;
                r.request == "0101"
                    && matches!(
                        r.name.as_str(),
                        "Malfunction indicator lamp" | "Number of DTCs stored in this ECU"
                    )
            })
            .collect();
        if values.is_empty() {
            caption(
                ui,
                "Check ECUs to see which report a check-engine light or stored codes.",
            );
        }
        self.value_table(ui, &values, ValueTable::Data);
        self.failures(ui, |r| r.request == "0101");
    }
    pub(super) fn description(&self, code: &str) -> String {
        self.description_for(&self.vehicle, code)
    }
    pub(super) fn description_for(&self, vehicle: &Vehicle, code: &str) -> String {
        Self::describe_code(&self.db, vehicle, code)
    }
    pub(super) fn describe_code(
        db: &crate::dtc_database::DtcDatabase,
        vehicle: &Vehicle,
        code: &str,
    ) -> String {
        if code.len() != 5 {
            return "Raw ECU identifier; no OEM description mapped".into();
        }
        if let Some((text, source)) = db.lookup_with_source(&vehicle.make, code) {
            return format!("{text} [{}]", source.unwrap_or(&vehicle.make));
        }
        let generic = crate::dtc_descriptions::describe(code);
        if generic.is_empty() {
            "No description in the local library".into()
        } else {
            format!("{generic} [SAE library]")
        }
    }
    fn codes(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if action(ui, "Read codes", self.connected && !self.busy) {
                self.read_codes();
            }
            if ui.link("View readiness").clicked() {
                self.page = Page::Emissions;
            }
            if ui
                .add_enabled(
                    self.connected && !self.busy,
                    egui::Button::new(RichText::new("Clear emissions codes").color(AMBER)),
                )
                .clicked()
            {
                self.clear_emissions_codes();
            }
            if ui
                .add_enabled(self.can_export_report(), egui::Button::new("Save report"))
                .clicked()
            {
                self.save_report();
            }
        });
        if let Some((path, snapshot)) = self.before_clear_scan.clone() {
            ui.horizontal_wrapped(|ui| {
                caption(ui, "Before-clear scan saved");
                if ui.link("Review before-clear scan").clicked() {
                    self.open_scan_review(snapshot, Some(path), Page::Codes);
                }
            });
        }
        if self.results.clear_attempt_ms().is_some() {
            card(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("Latest emissions clear request").strong());
                let mut acknowledged = false;
                for value in self.results.rows.values() {
                    let r = &value.latest.reading;
                    if r.request == "04" {
                        acknowledged = true;
                        ui.colored_label(
                            ACCENT,
                            format!("ECU {} acknowledged", ecu_label(r.source)),
                        );
                        if self.details {
                            ui.monospace(&r.raw);
                        }
                    }
                }
                if self.clearing && self.busy {
                    caption(ui, "Waiting for ECU replies.");
                } else if !acknowledged {
                    ui.colored_label(AMBER, "No ECU acknowledgment received.");
                }
                self.failures(ui, |r| r.request == "04");
                ui.horizontal_wrapped(|ui| {
                    let ready = self.connected && !self.busy;
                    if action(ui, "Save follow-up scan", ready) {
                        self.capture_follow_up();
                    }
                    if ui
                        .add_enabled(ready, egui::Button::new("Recheck codes and readiness"))
                        .clicked()
                    {
                        self.recheck_after_clear();
                    }
                });
            });
            ui.add_space(4.0);
        }
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.code_category, None, "All codes");
            for category in codes::Category::ALL {
                ui.selectable_value(&mut self.code_category, Some(category), category.label());
            }
        });
        let selected = self.code_category;
        ui.horizontal_wrapped(|ui| {
            for category in codes::Category::ALL
                .into_iter()
                .filter(|category| selected.is_none_or(|selected| selected == *category))
            {
                caption(
                    ui,
                    &format!(
                        "{} · {}",
                        category.label(),
                        codes::summary(&self.results, category)
                    ),
                );
            }
        });
        let visible_request = |request: &str| {
            selected.map_or_else(
                || is_code(request),
                |category| category.request() == request,
            )
        };
        let mut rows = Vec::new();
        for value in self
            .results
            .rows
            .values()
            .filter(|v| visible_request(&v.latest.reading.request))
        {
            let r = &value.latest.reading;
            if r.request == "1902FF" && r.name.starts_with("DTC ") {
                rows.push((value, Some(r.value.as_str())));
                continue;
            }
            let start = rows.len();
            if r.request != "1902FF" {
                let mut seen = BTreeSet::new();
                rows.extend(
                    codes::identifiers(&r.value)
                        .filter(|code| seen.insert(*code))
                        .map(|code| (value, Some(code))),
                );
            }
            if start == rows.len() {
                rows.push((value, None));
            }
        }
        rows.sort_by_key(|(value, _)| {
            let reading = &value.latest.reading;
            (reading.request.as_str(), reading.source)
        });
        let mut open_frame = None;
        if rows.is_empty() {
            if !self.busy
                && selected.is_none()
                && !self
                    .results
                    .failures
                    .values()
                    .any(|v| visible_request(&v.reading.request))
            {
                caption(ui, "Read codes to see results by ECU.");
            }
        } else {
            let captures = Frames::from_results(&self.results);
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(egui_extras::Column::initial(88.0))
                .column(egui_extras::Column::initial(92.0))
                .column(egui_extras::Column::initial(74.0))
                .column(egui_extras::Column::remainder().at_least(190.0))
                .column(egui_extras::Column::initial(153.0))
                .column(egui_extras::Column::initial(114.0))
                .max_scroll_height(390.0)
                .header(27.0, |mut row| {
                    for label in [
                        "ECU",
                        "Category",
                        "Code",
                        "Result",
                        "Freeze frame",
                        "Last read",
                    ] {
                        row.col(|ui| {
                            ui.strong(label);
                        });
                    }
                })
                .body(|body| {
                    body.rows(33.0, rows.len(), |mut row| {
                        let (value, code) = rows[row.index()];
                        let r = &value.latest.reading;
                        let historical = self.results.predates_clear(&value.latest);
                        let failed = self.results.refresh_failed(&value.latest);
                        let category = codes::Category::from_request(&r.request);
                        row.col(|ui| {
                            ui.monospace(ecu_label(r.source));
                        });
                        row.col(|ui| {
                            ui.label(category.map_or("Module", codes::Category::label));
                        });
                        row.col(|ui| {
                            ui.label(
                                RichText::new(code.unwrap_or("—"))
                                    .monospace()
                                    .color(if code.is_some() { AMBER } else { MUTED }),
                            );
                        });
                        row.col(|ui| {
                            let result = match code {
                                Some(code) if r.request != "1902FF" => self.description(code),
                                Some(_) => r.status.clone(),
                                None => r.value.clone(),
                            };
                            ui.add(egui::Label::new(&result).truncate())
                                .on_hover_text(&result);
                        });
                        row.col(|ui| {
                            if let (Some(_), Some(code)) = (category, code) {
                                let links = captures.links(&value.latest, code, |row| {
                                    !unavailable(&row.reading)
                                        && self.results.evidence_issue(row).is_none()
                                });
                                ui.push_id((r.source, &r.request, &r.name, code), |ui| {
                                    if let Some(number) = freeze_frame_link(ui, links) {
                                        open_frame = Some((number, r.source));
                                    }
                                });
                            } else {
                                caption(ui, "—");
                            }
                        });
                        row.col(|ui| {
                            let age =
                                self.view_time().saturating_sub(value.latest.at_ms) as f64 / 1000.0;
                            let (label, color) = if historical {
                                ("Before clear".into(), AMBER)
                            } else if failed {
                                ("Refresh failed".into(), AMBER)
                            } else {
                                (format!("{age:.1} s ago"), MUTED)
                            };
                            ui.colored_label(color, label).on_hover_text(if historical {
                                "Reading from before the clear request. Reread to verify.".into()
                            } else {
                                format!("Read {age:.1} s ago. {}", r.status)
                            });
                        });
                    });
                });
        }
        if let Some((number, source)) = open_frame {
            self.frame = number;
            self.frame_ecu = source;
            self.page = Page::FreezeFrame;
            ui.ctx().request_repaint();
        }
        self.failures(ui, |r| visible_request(&r.request));
        ui.checkbox(&mut self.details, "Raw replies");
        if self.details {
            for value in self
                .results
                .rows
                .values()
                .map(|v| &v.latest)
                .chain(self.results.failures.values())
                .filter(|v| visible_request(&v.reading.request) && !v.reading.raw.is_empty())
            {
                let r = &value.reading;
                ui.monospace(format!("{}  {}  {}", ecu_label(r.source), r.request, r.raw))
                    .on_hover_text(&r.status);
            }
        }
        ui.collapsing("Code lookup", |ui| {
            ui.horizontal(|ui| {
                ui.label("DTC");
                ui.add(egui::TextEdit::singleline(&mut self.lookup).desired_width(110.0));
            });
            ui.label(self.description(&self.lookup.trim().to_ascii_uppercase()));
            caption(ui, "Code descriptions are reference information.");
        });
    }
    fn live_page(&mut self, ui: &mut egui::Ui) {
        let replay = self.playback.is_some();
        let read_count = self.selected_live_queries().count();
        ui.horizontal_wrapped(|ui| {
            if replay {
                if ui.button("Replay controls").clicked() {
                    self.page = Page::Recordings;
                }
            } else {
                let ready = self.connected && !self.busy && read_count > 0;
                if action(
                    ui,
                    if self.live { "Stop live" } else { "Start live" },
                    self.connected && (self.live || ready),
                ) {
                    if self.live {
                        self.stop_operation();
                    } else {
                        self.start_live();
                    }
                }
                if ui
                    .add_enabled(ready, egui::Button::new("Read once"))
                    .clicked()
                {
                    self.break_graphs();
                    self.read_live(self.selected_queries(), false);
                }
                self.live_presets_menu(ui);
                let recording = self.recording.is_some();
                if ui
                    .add_enabled(
                        self.connected && (recording || !self.scanning_emissions),
                        egui::Button::new(if recording {
                            "Save recording"
                        } else {
                            "Record"
                        }),
                    )
                    .clicked()
                {
                    if recording {
                        self.finish_recording();
                    } else {
                        self.start_recording();
                    }
                }
                if self.connected && self.live_grouping_fell_back {
                    ui.label("Individual reads").on_hover_text(
                        "Live Data switched to individual requests. Reconnect to try grouped reads again.",
                    );
                }
            }
            ui.checkbox(&mut self.graphs, "Graphs");
        });

        if replay {
            self.replay_timeline(ui, "live_replay");
            if self.graphs
                && let Some(at) = self.replay_graphs(ui)
            {
                self.replay_playing = false;
                self.seek_replay(at);
            }
        } else {
            self.recording_mark_controls(ui);
            self.live_selection(ui, read_count);
        }

        let allowed: BTreeSet<_> = if replay {
            BTreeSet::new()
        } else {
            self.generic
                .iter()
                .filter(|q| self.selected.is_empty() || self.selected.contains(&query_key(q)))
                .map(|q| encode_hex(&q.bytes))
                .collect()
        };
        let visible = |r: &Reading| {
            if replay {
                graph_request(&r.request)
            } else {
                allowed.contains(&r.request)
            }
        };
        let values: Vec<_> = self
            .results
            .rows
            .values()
            .filter(|v| visible(&v.latest.reading))
            .collect();
        if values.is_empty() {
            caption(
                ui,
                if replay {
                    "No live readings at the replay position."
                } else if !self.connected {
                    "Connect to read live data."
                } else if self.selected.is_empty() {
                    "Select data items to begin."
                } else if read_count == 0 {
                    NO_REPORTED_SELECTION
                } else {
                    "Start live or read once to see readings."
                },
            );
        }
        if self.graphs && !replay {
            let now = self.view_time();
            let choices: Vec<_> = self
                .results
                .rows
                .iter()
                .filter(|(_, value)| !value.history.is_empty())
                .map(|(key, value)| (key.as_str(), graphs::signal_title(&value.latest.reading)))
                .collect();
            let automatic = choices.iter().filter_map(|(key, _)| {
                visible(&self.results.rows[*key].latest.reading).then_some(*key)
            });
            let selected = graph_selection(
                ui,
                &choices,
                &mut self.graph_signals,
                &mut self.graph_window,
                automatic,
            );
            let values: Vec<_> = selected
                .iter()
                .filter_map(|key| self.results.rows.get(key))
                .collect();
            if values.is_empty() && !choices.is_empty() {
                caption(ui, "Choose signals to graph.");
            }
            let columns = if ui.available_width() >= 670.0 { 2 } else { 1 };
            for chunk in values.chunks(columns) {
                ui.columns(columns, |columns| {
                    for (ui, value) in columns.iter_mut().zip(chunk) {
                        card(ui, |ui| {
                            let failed = self.results.refresh_failed(&value.latest);
                            let reading = &value.latest.reading;
                            let unit = unit_label(&reading.unit);
                            ui.strong(graphs::signal_title(reading));
                            graph_current(ui, Some(value), failed);
                            let axis = graphs::TimeAxis::live(now, self.graph_window);
                            let trace =
                                graphs::Trace::new(&value.history, axis.start_ms, axis.end_ms);
                            plot(ui, &trace, unit, axis, None);
                        });
                    }
                });
                ui.add_space(6.0);
            }
        }
        self.value_table(ui, &values, ValueTable::Live);
        self.failures(ui, |r| {
            visible(r) || self.selected.is_empty() && r.request.starts_with("01")
        });
    }

    fn replay_graphs(&mut self, ui: &mut egui::Ui) -> Option<u64> {
        let playback = self.playback.as_ref()?;
        let signals = &playback.graphs().signals;
        if signals.is_empty() {
            caption(ui, "This recording has no numeric live data to graph.");
            return None;
        }
        let choices: Vec<_> = signals
            .iter()
            .map(|(key, signal)| (key.as_str(), signal.title.as_str()))
            .collect();
        let selected = graph_selection(
            ui,
            &choices,
            &mut self.graph_signals,
            &mut self.graph_window,
            choices.iter().map(|(key, _)| *key),
        );
        if selected.is_empty() {
            caption(ui, "Choose signals to graph.");
        }
        let replay = playback.axis(self.replay_at, self.graph_window);
        let columns = if ui.available_width() >= 670.0 { 2 } else { 1 };
        let mut jump = None;
        for chunk in selected.chunks(columns) {
            ui.columns(columns, |columns| {
                for (ui, key) in columns.iter_mut().zip(chunk) {
                    let signal = &signals[key];
                    let series = signal.series_at(replay.cursor_ms);
                    card(ui, |ui| {
                        ui.strong(&signal.title);
                        let current = self.results.rows.get(key);
                        let failed =
                            current.is_some_and(|value| self.results.refresh_failed(&value.latest));
                        graph_current(ui, current, failed);
                        let trace = graphs::Trace::new(
                            series.points_in(replay.axis),
                            replay.axis.start_ms,
                            replay.axis.end_ms,
                        );
                        if let Some(at) = plot(ui, &trace, &series.unit, replay.axis, Some(replay))
                        {
                            jump = Some(at);
                        }
                    });
                }
            });
            ui.add_space(6.0);
        }
        jump
    }

    fn live_selection(&mut self, ui: &mut egui::Ui, read_count: usize) {
        let selected = self.selected.len();
        let title = if selected == read_count {
            format!("Data items · {selected} selected")
        } else {
            format!("Data items · {selected} selected / {read_count} to read")
        };
        egui::CollapsingHeader::new(title)
            .id_salt("live_data_selection")
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            self.connected && !self.busy,
                            egui::Button::new("Refresh PID support"),
                        )
                        .clicked()
                    {
                        self.refresh_pid_support();
                    }
                    ui.add_enabled(
                        self.pid_support.checked_through > 0,
                        egui::Checkbox::new(&mut self.skip_unreported, "Skip unreported PIDs"),
                    )
                    .on_hover_text(
                        "Skips unlisted PIDs in checked ranges. Unchecked ranges stay available.",
                    );
                    if !self.connected
                        || self.protocol.contains("CAN") && !self.live_grouping_fell_back
                    {
                        ui.add_enabled(
                            !self.busy,
                            egui::Checkbox::new(&mut self.group_live, "Group CAN reads"),
                        )
                        .on_hover_text("Requests several reported PIDs together when possible.");
                    }
                    ui.label("Cycle delay");
                    ui.add(
                        egui::DragValue::new(&mut self.interval)
                            .range(0.2..=10.0)
                            .speed(0.2)
                            .suffix(" s"),
                    );
                });
                if self.connected {
                    let reported = self.pid_support.reported.len();
                    if let Some(problem) = &self.pid_support.problem {
                        ui.colored_label(
                            AMBER,
                            format!("{reported} PIDs reported · Check incomplete"),
                        )
                        .on_hover_text(problem);
                    } else if self.pid_support.next_range.is_some() {
                        caption(
                            ui,
                            &format!("{reported} PIDs reported · Checking more ranges"),
                        );
                    } else {
                        caption(ui, &format!("{reported} PIDs reported"));
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Search names or request IDs")
                            .desired_width(280.0),
                    );
                    ui.checkbox(&mut self.favorites_only, "Favorites only");
                });
                if ui.button("Clear selection").clicked() {
                    self.selected.clear();
                }
                egui::ScrollArea::vertical()
                    .id_salt("data_picker")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        let mut favorite_edits = Vec::new();
                        let search = self.search.trim().to_ascii_lowercase();
                        for q in self.generic {
                            let key = query_key(q);
                            let favorite = self.settings.favorites.contains(&key);
                            let matches = q.name.to_ascii_lowercase().contains(&search)
                                || key.to_ascii_lowercase().contains(&search);
                            let availability = self.pid_availability(q);
                            if !matches
                                || self.favorites_only && !favorite
                                || self.skip_unreported
                                    && availability == PidAvailability::Unreported
                            {
                                continue;
                            }
                            ui.horizontal(|ui| {
                                let mut picked = self.selected.contains(&key);
                                if ui.checkbox(&mut picked, "").changed() {
                                    if picked {
                                        self.selected.insert(key.clone());
                                    } else {
                                        self.selected.remove(&key);
                                    }
                                }
                                if ui
                                    .small_button(if favorite { "Pinned" } else { "Pin" })
                                    .clicked()
                                {
                                    favorite_edits.push((key.clone(), !favorite));
                                }
                                let support = match availability {
                                    PidAvailability::Reported => "Reported by a responding ECU",
                                    PidAvailability::Unreported => {
                                        "Unreported in the checked range"
                                    }
                                    PidAvailability::Unchecked => "Support unchecked",
                                };
                                ui.add(egui::Label::new(&q.name).wrap())
                                    .on_hover_text(format!("{key}\n{support}"));
                                if availability == PidAvailability::Unreported {
                                    caption(ui, "Unreported");
                                }
                            });
                        }
                        if !favorite_edits.is_empty() {
                            let result = self.edit_settings(|settings| {
                                for (key, pin) in favorite_edits {
                                    if pin {
                                        settings.favorites.insert(key);
                                    } else {
                                        settings.favorites.remove(&key);
                                    }
                                }
                            });
                            self.status = match result {
                                Ok(()) => "Favorites saved.".into(),
                                Err(error) => format!("Could not save favorites. {error}"),
                            };
                        }
                    });
            });
    }
    fn readiness(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for period in [ReadinessPeriod::SinceClear, ReadinessPeriod::DriveCycle] {
                ui.selectable_value(&mut self.readiness_period, period, period.label());
            }
            if action(ui, "Read readiness", self.connected && !self.busy) {
                self.read_readiness_period(self.readiness_period);
            }
            ui.checkbox(&mut self.show_unsupported_monitors, "Show unsupported");
        });
        let period = self.readiness_period;
        let reports = ecu_reports(&self.results, period);
        if reports.is_empty()
            && !self
                .results
                .failures
                .values()
                .any(|v| v.reading.request == period.request())
        {
            caption(ui, "Read the vehicle to see monitor status.");
        }
        for report in reports {
            card(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.strong(format!("ECU {}", ecu_label(report.source)));
                    if let Some(ignition) = report.ignition {
                        caption(ui, &format!("{ignition} ignition"));
                    }
                    if let Some(value) = report.latest {
                        let seconds = self.view_time().saturating_sub(value.at_ms) / 1000;
                        if report.historical {
                            ui.colored_label(AMBER, "Before clear");
                        }
                        if report.refresh_failed {
                            ui.colored_label(AMBER, "Refresh failed");
                        }
                        caption(ui, &format!("Read {seconds} s ago"))
                    } else {
                        ui.colored_label(AMBER, "No reading");
                    }
                });
                if report.latest.is_none() {
                    return;
                }
                let current = !report.historical && !report.refresh_failed;
                ui.horizontal_wrapped(|ui| {
                    if let Some(mil) = report.mil {
                        ui.colored_label(
                            if current && mil == "On" { RED } else { MUTED },
                            format!("MIL {mil}"),
                        );
                    }
                    if let Some(count) = report.dtc_count {
                        caption(ui, &format!("Confirmed DTCs {count}"));
                    }
                    for (state, label) in [
                        (MonitorState::NotReady, "not ready"),
                        (MonitorState::Ready, "ready"),
                        (MonitorState::Disabled, "disabled"),
                        (MonitorState::Unsupported, "unsupported"),
                        (MonitorState::DisabledOrUnknown, "disabled or unsupported"),
                    ] {
                        let count = report
                            .monitors
                            .iter()
                            .filter(|monitor| monitor.state == state)
                            .count();
                        if count > 0 {
                            ui.colored_label(
                                if current {
                                    readiness_color(state)
                                } else {
                                    MUTED
                                },
                                format!("{count} {label}"),
                            );
                        }
                    }
                });
                egui::Grid::new(("readiness_monitors", period, report.source))
                    .num_columns(2)
                    .spacing(Vec2::new(18.0, 4.0))
                    .striped(true)
                    .show(ui, |ui| {
                        for monitor in report.monitors.iter().filter(|monitor| {
                            self.show_unsupported_monitors
                                || monitor.state != MonitorState::Unsupported
                        }) {
                            ui.label(monitor.name);
                            ui.colored_label(
                                if current {
                                    readiness_color(monitor.state)
                                } else {
                                    MUTED
                                },
                                monitor.state.label(),
                            );
                            ui.end_row();
                        }
                    });
            });
            ui.add_space(6.0);
        }
        self.failures(ui, |r| r.request == period.request());
    }

    fn freeze_frame(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.link("Back to trouble codes").clicked() {
                self.page = Page::Codes;
                ui.ctx().request_repaint();
            }
            ui.separator();
            ui.label("Frame");
            ui.add_enabled(
                !self.busy,
                egui::DragValue::new(&mut self.frame).range(0..=255),
            );
            let ready = self.connected && !self.busy;
            if action(ui, "Read freeze frame", ready) {
                self.read_freeze_frame();
            }
        });
        let frame = self.frame;
        let mut values_by_source = std::collections::BTreeMap::<_, Vec<_>>::new();
        for value in self.results.rows.values() {
            let reading = &value.latest.reading;
            if reading.freeze_frame_number() == Some(frame) {
                values_by_source
                    .entry(reading.source)
                    .or_default()
                    .push(value);
            }
        }
        let mut failures_by_source = std::collections::BTreeMap::<_, Vec<_>>::new();
        let mut discovery_issues = Vec::new();
        for failure in self.results.failures.values() {
            let reading = &failure.reading;
            if reading.freeze_frame_number() != Some(frame) {
                continue;
            }
            if reading.name == freeze_frames::DISCOVERY {
                discovery_issues.push(reading);
            } else {
                values_by_source.entry(reading.source).or_default();
                failures_by_source
                    .entry(reading.source)
                    .or_default()
                    .push(reading);
            }
        }
        if self
            .frame_ecu
            .is_some_and(|ecu| !values_by_source.contains_key(&Some(ecu)))
        {
            self.frame_ecu = None;
        }
        if values_by_source.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.frame_ecu, None, "All ECUs");
                for source in values_by_source.keys().copied().flatten() {
                    ui.selectable_value(
                        &mut self.frame_ecu,
                        Some(source),
                        format!("ECU {source:X}"),
                    );
                }
            });
        }
        if values_by_source.is_empty() {
            caption(
                ui,
                "Read a frame to see the stored conditions and trigger code.",
            );
        }
        for (source, values) in values_by_source {
            let show_values = self.frame_ecu.is_none() || source == self.frame_ecu;
            if !show_values && source.is_some() {
                continue;
            }
            let trigger = values
                .iter()
                .map(|value| &value.latest)
                .filter(|row| row.reading.request.starts_with("0202"))
                .max_by_key(|row| row.at_ms);
            let measurements: Vec<_> = values
                .into_iter()
                .filter(|value| !value.latest.reading.request.starts_with("0202"))
                .collect();
            let trigger_issue = trigger.and_then(|row| self.results.evidence_issue(row));
            ui.push_id((frame, source), |ui| {
                if show_values {
                    card(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(appearance::title(
                            format!("Frame {frame} · ECU {}", ecu_label(source)),
                            23.0,
                        ));
                        if let Some(trigger) = trigger {
                            let reading = &trigger.reading;
                            let code = freeze_frames::trigger_code(reading);
                            if let Some(code) = code {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        RichText::new(format!("Trigger code {code}")).strong(),
                                    );
                                    ui.label(self.description(code));
                                });
                            } else {
                                ui.label(&reading.value);
                            }
                            match trigger_issue {
                                Some(EvidenceIssue::BeforeClear) => {
                                    ui.colored_label(AMBER, "Trigger recorded before clear");
                                }
                                Some(EvidenceIssue::RefreshFailed) => {
                                    ui.colored_label(AMBER, "Trigger refresh failed · unverified");
                                }
                                None => {}
                            }
                            if code.is_some()
                                && trigger_issue.is_none()
                                && !measurements.iter().any(|value| {
                                    value.latest.at_ms >= trigger.at_ms
                                        && self.results.evidence_issue(&value.latest).is_none()
                                })
                            {
                                caption(ui, "No verified values from this trigger read.");
                            }
                            if !reading.raw.is_empty() {
                                ui.collapsing("Trigger reply", |ui| {
                                    ui.monospace(&reading.raw).on_hover_text(&reading.status);
                                });
                            }
                        } else {
                            caption(ui, "Trigger code unavailable.");
                        }
                        self.value_table(
                            ui,
                            &measurements,
                            ValueTable::FreezeFrame {
                                trigger_at: trigger.map(|row| row.at_ms),
                                trigger_unverified: trigger_issue.is_some(),
                            },
                        );
                    });
                }
                if let Some(failures) = failures_by_source.get(&source) {
                    for reading in failures {
                        reading_issue(ui, reading);
                    }
                }
            });
        }
        for reading in discovery_issues {
            ui.colored_label(AMBER, &reading.status);
        }
    }

    fn monitor_tests(&mut self, ui: &mut egui::Ui) {
        let ready = self.connected && !self.busy && self.playback.is_none();
        let can = self.protocol.contains("CAN");
        let mut read_one = false;
        ui.horizontal_wrapped(|ui| {
            if action(ui, "Read supported monitors", ready) {
                self.read_supported_monitors();
            }
            ui.add_enabled_ui(ready, |ui| {
                ui.label(if can { "Monitor ID" } else { "Test ID" });
                ui.add(
                    egui::DragValue::new(&mut self.monitor)
                        .range(1..=255)
                        .hexadecimal(2, false, true),
                );
                if let Some(support) = &self.monitor_support {
                    if !support.reported.is_empty() {
                        egui::ComboBox::from_id_salt("reported_monitor_ids")
                            .selected_text("Reported IDs")
                            .show_ui(ui, |ui| {
                                for &mid in &support.reported {
                                    ui.selectable_value(
                                        &mut self.monitor,
                                        mid,
                                        monitor_label(mid, can),
                                    );
                                }
                            });
                    }
                }
                read_one = ui
                    .add_enabled(self.monitor % 32 != 0, egui::Button::new("Read monitor"))
                    .clicked();
            });
        });
        if read_one {
            let mut query = self.base_query(6, Some(self.monitor));
            query.name = monitor_label(self.monitor, can);
            self.once(vec![query]);
        }
        if ready && self.monitor % 32 == 0 {
            caption(
                ui,
                "This ID is a support bitmap. Choose a test ID to read its results.",
            );
        }
        if let Some(support) = &self.monitor_support {
            if let Some(problem) = &support.problem {
                ui.colored_label(
                    AMBER,
                    if support.reported.is_empty() {
                        "Monitor discovery unavailable. Enter a known ID to read it directly."
                    } else {
                        "Monitor discovery incomplete. Results cover the IDs that were reported."
                    },
                )
                .on_hover_text(problem);
            } else {
                caption(
                    ui,
                    &format!("{} monitor IDs reported", support.reported.len()),
                );
            }
        }
        if self.connected && !can && self.playback.is_none() {
            ui.add_space(8.0);
            self.oxygen_test_controls(ui, ready);
        }
        let values: Vec<_> = self
            .results
            .rows
            .values()
            .filter(|v| is_monitor_result(&v.latest.reading))
            .collect();
        if values.is_empty() {
            if self.monitor_support.is_none() {
                caption(ui, "Read the vehicle’s stored Mode 06 test results.");
            }
        } else {
            caption(
                ui,
                "These are stored test results. Check Readiness for monitor completion.",
            );
            self.monitor_table(ui, &values);
        }
        self.failures(ui, is_monitor_result);
    }

    fn oxygen_test_controls(&mut self, ui: &mut egui::Ui, ready: bool) {
        use diagnostics::oxygen_monitor::{sensor_label, test_name};
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.strong("Oxygen-sensor tests");
            ui.horizontal_wrapped(|ui| {
                if action(ui, "Read oxygen tests", ready) {
                    self.read_oxygen_tests();
                }
                ui.add_enabled_ui(ready, |ui| {
                    let label = |sensor| {
                        let width = self
                            .oxygen_support
                            .sensor_widths
                            .get(&sensor)
                            .copied()
                            .flatten();
                        sensor_label(sensor, width)
                    };
                    egui::ComboBox::from_id_salt("oxygen_sensor")
                        .selected_text(label(self.oxygen_sensor))
                        .show_ui(ui, |ui| {
                            for sensor in (0..8).map(|bit| 1u8 << bit) {
                                ui.selectable_value(&mut self.oxygen_sensor, sensor, label(sensor));
                            }
                        });
                    ui.label("Test ID");
                    ui.add(
                        egui::DragValue::new(&mut self.oxygen_test)
                            .range(1..=255)
                            .hexadecimal(2, false, true),
                    );
                    if let Some(tests) = self.oxygen_support.tests.get(&self.oxygen_sensor) {
                        egui::ComboBox::from_id_salt("oxygen_test")
                            .selected_text("Reported tests")
                            .show_ui(ui, |ui| {
                                for &test in tests {
                                    ui.selectable_value(
                                        &mut self.oxygen_test,
                                        test,
                                        format!("{test:02X}  {}", test_name(test)),
                                    );
                                }
                            });
                    }
                    if ui
                        .add_enabled(self.oxygen_test % 32 != 0, egui::Button::new("Read test"))
                        .clicked()
                    {
                        self.read_oxygen_test();
                    }
                });
            });
            if let Some(problem) = &self.oxygen_support.problem {
                ui.colored_label(AMBER, problem);
            } else if !self.oxygen_support.sensor_widths.is_empty() {
                let count = self.oxygen_support.sensor_widths.len();
                caption(
                    ui,
                    &format!(
                        "{count} sensor {} reported",
                        if count == 1 { "location" } else { "locations" }
                    ),
                );
            }
        });
    }

    fn monitor_table(&self, ui: &mut egui::Ui, values: &[&ValueState]) {
        ui.push_id("monitor_results", |ui| {
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(egui_extras::Column::initial(84.0).at_least(76.0))
                .column(egui_extras::Column::initial(190.0).at_least(135.0))
                .column(egui_extras::Column::initial(116.0).at_least(94.0))
                .column(egui_extras::Column::remainder().at_least(158.0))
                .max_scroll_height(380.0)
                .header(27.0, |mut header| {
                    for title in ["ECU", "Monitor / test", "Result", "Value and limits"] {
                        header.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|body| {
                    body.rows(44.0, values.len(), |mut row| {
                        let value = &values[row.index()].latest;
                        let r = &value.reading;
                        let oxygen = diagnostics::oxygen_monitor::request_parts(&r.request);
                        let mut description = match monitor_id(r) {
                            Some(mid) if r.name.starts_with("MID ") => monitor_label(mid, true),
                            _ => r.name.clone(),
                        };
                        if let Some((_, sensor)) = oxygen {
                            if !description.contains(" / ") {
                                description = format!(
                                    "{} / {description}",
                                    diagnostics::oxygen_monitor::sensor_label(sensor, None)
                                );
                            }
                        }
                        let (state, color) = if self.results.predates_clear(value) {
                            ("Before clear", AMBER)
                        } else if self.results.refresh_failed(value) {
                            ("Refresh failed", AMBER)
                        } else {
                            match r.status.as_str() {
                                "PASS" => ("Pass", ACCENT),
                                "FAIL" => ("Fail", RED),
                                diagnostics::MONITOR_NOT_COMPLETED => ("Not completed", AMBER),
                                diagnostics::MONITOR_INVALID_LIMITS => ("Invalid limits", AMBER),
                                diagnostics::oxygen_monitor::THRESHOLD => ("ECU parameter", MUTED),
                                diagnostics::oxygen_monitor::COMPLETION_UNKNOWN => {
                                    ("Completion unknown", AMBER)
                                }
                                diagnostics::oxygen_monitor::LIMITS_UNKNOWN => {
                                    ("Limits unknown", AMBER)
                                }
                                _ => ("Raw data", MUTED),
                            }
                        };
                        row.col(|ui| {
                            ui.monospace(ecu_label(r.source));
                        });
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                let mut lines = description.as_str().splitn(2, " / ");
                                if let Some((test, _)) = oxygen {
                                    let location = lines.next().unwrap_or("");
                                    let title = lines.next().unwrap_or(location);
                                    ui.add(egui::Label::new(title).truncate())
                                        .on_hover_text(&description);
                                    let subheading = format!("{location} · TID {test:02X}");
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&subheading).small().color(MUTED),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(subheading);
                                } else {
                                    ui.add(egui::Label::new(&description).truncate())
                                        .on_hover_text(&description);
                                    if description != r.name {
                                        caption(ui, &r.name);
                                    }
                                }
                            });
                        });
                        row.col(|ui| {
                            let age = self.view_time().saturating_sub(value.at_ms) as f64 / 1000.0;
                            ui.vertical(|ui| {
                                ui.colored_label(color, state).on_hover_text(&r.status);
                                caption(ui, &format!("Read {age:.1} s ago"));
                            });
                        });
                        row.col(|ui| {
                            let (measurement, limits) = r
                                .value
                                .split_once(" (")
                                .map_or((r.value.as_str(), ""), |(value, limits)| {
                                    (value, limits.trim_end_matches(')'))
                                });
                            let measurement = match r.unit.as_str() {
                                "" if r.status == diagnostics::MONITOR_NOT_COMPLETED => "—".into(),
                                "" | "raw" => measurement.into(),
                                _ => format!("{measurement} {}", unit_label(&r.unit)),
                            };
                            ui.vertical(|ui| {
                                let detail = format!("{} {}\nReply  {}", r.value, r.unit, r.raw);
                                ui.add(egui::Label::new(measurement).truncate())
                                    .on_hover_text(&detail);
                                if !limits.is_empty() {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(limits).small().color(MUTED),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(detail);
                                }
                            });
                        });
                    });
                });
        });
    }
    fn recording_mark_controls(&mut self, ui: &mut egui::Ui) {
        let Some(capture) = &self.recording else {
            return;
        };
        let count = capture.markers.len();
        ui.horizontal_wrapped(|ui| {
            let enabled = count < MAX_RECORDING_MARKERS;
            let mark = ui
                .add_enabled(enabled, egui::Button::new("Mark event"))
                .on_hover_text("Add a marker at the current recording time")
                .on_disabled_hover_text("This recording has reached its marker limit.")
                .clicked();
            ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut self.marker_note)
                    .desired_width(210.0)
                    .char_limit(MAX_MARKER_NOTE_CHARS)
                    .hint_text("Optional marker note"),
            );
            if mark {
                self.mark_recording();
            }
            if count > 0 {
                caption(
                    ui,
                    &format!("{count} {}", if count == 1 { "marker" } else { "markers" }),
                );
            }
        });
    }

    fn replay_timeline(&mut self, ui: &mut egui::Ui, id: &'static str) {
        let Some(capture) = &self.playback else {
            return;
        };
        let start = capture.started_ms;
        let end = capture.end_ms();
        let duration = end.saturating_sub(start) as f64 / 1000.0;
        let mut position = self.replay_at.saturating_sub(start) as f64 / 1000.0;
        ui.push_id(id, |ui| {
            ui.horizontal_wrapped(|ui| {
                let playback = if self.replay_playing { "Pause" } else { "Play" };
                if ui
                    .add_enabled(end > start, egui::Button::new(playback))
                    .clicked()
                {
                    if !self.replay_playing && self.replay_at >= end {
                        self.seek_replay(start);
                    }
                    self.replay_playing = !self.replay_playing;
                    self.replay_tick = Instant::now();
                }
                if ui
                    .add_enabled(self.replay_at > start, egui::Button::new("Rewind"))
                    .clicked()
                {
                    self.replay_playing = false;
                    self.seek_replay(start);
                }
                ui.spacing_mut().slider_width = ui.available_width().clamp(100.0, 270.0);
                if ui
                    .add_enabled(
                        end > start,
                        egui::Slider::new(&mut position, 0.0..=duration.max(0.001))
                            .suffix(" s")
                            .max_decimals(2),
                    )
                    .on_hover_text("Replay position")
                    .changed()
                {
                    self.replay_playing = false;
                    self.seek_replay(start + (position * 1000.0).round() as u64);
                }
                caption(ui, &format!("of {duration:.1} s"));
            });
            self.replay_markers(ui);
        });
    }

    fn replay_markers(&mut self, ui: &mut egui::Ui) {
        let Some(capture) = &self.playback else {
            return;
        };
        if capture.markers.is_empty() {
            return;
        }
        let at = self.replay_at;
        let index = capture.markers.partition_point(|marker| marker.at_ms < at);
        let current = capture
            .markers
            .get(index)
            .filter(|marker| marker.at_ms == at);
        let next = index + usize::from(current.is_some());
        let mut jump = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(index > 0, egui::Button::new("Previous marker"))
                .clicked()
            {
                jump = Some(capture.markers[index - 1].at_ms);
            }
            egui::ComboBox::from_id_salt("recording_markers")
                .selected_text(if current.is_some() {
                    format!("Marker {} of {}", index + 1, capture.markers.len())
                } else {
                    format!("Markers ({})", capture.markers.len())
                })
                .width(135.0)
                .height(220.0)
                .show_ui(ui, |ui| {
                    ui.set_max_width(320.0);
                    for (index, marker) in capture.markers.iter().enumerate() {
                        let mut label = format!(
                            "{}. {}",
                            index + 1,
                            recordings::marker_position(marker.at_ms - capture.started_ms),
                        );
                        if !marker.note.is_empty() {
                            label.push_str(" · ");
                            label.push_str(&marker.note);
                        }
                        if ui
                            .add(
                                egui::Button::new(&label)
                                    .selected(marker.at_ms == at)
                                    .truncate(),
                            )
                            .on_hover_text(&label)
                            .clicked()
                        {
                            jump = Some(marker.at_ms);
                        }
                    }
                });
            if ui
                .add_enabled(
                    next < capture.markers.len(),
                    egui::Button::new("Next marker"),
                )
                .clicked()
            {
                jump = Some(capture.markers[next].at_ms);
            }
        });
        if let Some(marker) = current.filter(|marker| !marker.note.is_empty()) {
            caption(ui, &marker.note);
        }
        if let Some(at) = jump {
            self.replay_playing = false;
            self.seek_replay(at);
        }
    }

    fn recordings(&mut self, ui: &mut egui::Ui) {
        let directory = self.root.join("recordings");
        if self.unsaved.is_some() {
            ui.colored_label(AMBER, "A recording is waiting to be saved.");
            ui.horizontal_wrapped(|ui| {
                #[cfg(windows)]
                if ui.button("Save recording as…").clicked() {
                    match recordings::pick_recording(&directory, true) {
                        Ok(Some(path)) => self.save_recording_to(path),
                        Ok(None) => {}
                        Err(error) => self.status = error,
                    }
                }
                #[cfg(not(windows))]
                {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.capture_path)
                            .desired_width(420.0)
                            .hint_text("Writable full path for recording.json"),
                    );
                    if ui.button("Save recording as").clicked() {
                        self.save_recording_to(PathBuf::from(&self.capture_path));
                    }
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            let recording = self.recording.is_some();
            if action(
                ui,
                if recording {
                    "Save recording"
                } else {
                    "Start recording"
                },
                self.connected && self.unsaved.is_none() && (recording || !self.scanning_emissions),
            ) {
                if recording {
                    self.finish_recording();
                } else {
                    self.start_recording();
                }
            }
            if self.unsaved.is_some() && ui.button("Retry saving recording").clicked() {
                self.save_recording();
            }
            #[cfg(windows)]
            if ui.button("Open recording file…").clicked() {
                match recordings::pick_recording(&directory, false) {
                    Ok(Some(path)) => self.preview_recording(path),
                    Ok(None) => {}
                    Err(error) => self.status = error,
                }
            }
            if ui
                .add_enabled(
                    directory.is_dir(),
                    egui::Button::new("Open recordings folder"),
                )
                .clicked()
            {
                open_local(&directory);
            }
            if ui.button("Refresh").clicked() {
                self.refresh_files();
            }
        });
        if self.recording.is_some() {
            ui.colored_label(RED, format!("{} readings recorded", self.recording_rows));
        }
        self.recording_mark_controls(ui);
        if self.playback.is_some() {
            ui.add_space(12.0);
            let (close, export) = card(ui, |ui| {
                ui.set_min_width(ui.available_width());
                let actions = ui
                    .horizontal_wrapped(|ui| {
                        let title = self
                            .playback_path
                            .as_deref()
                            .map(recordings::recording_label)
                            .unwrap_or_else(|| "Recording replay".into());
                        ui.strong(title);
                        if ui.button("View data graphs").clicked() {
                            self.graphs = true;
                            self.page = Page::Live;
                        }
                        let export = ui.button("Export full CSV").clicked();
                        (ui.button("Close replay").clicked(), export)
                    })
                    .inner;
                self.replay_timeline(ui, "recording_replay");
                let capture = self.playback.as_ref().unwrap();
                caption(
                    ui,
                    &format!(
                        "{} samples / {} readings",
                        capture.samples.len(),
                        capture.readings()
                    ),
                );
                actions
            })
            .inner;
            if export {
                self.export_capture_csv(self.playback.as_ref().unwrap().capture());
            }
            if close {
                self.reset_results();
                self.demo = false;
                self.vehicle = Vehicle::default();
                self.status = "Replay closed.".into();
            }
        }
        let can_replay = self.recording.is_none() && self.unsaved.is_none();
        if let Some(preview) = &self.recording_preview {
            let mut play = false;
            let mut show_graphs = false;
            let mut export = false;
            let preview_card = card(ui, |ui| {
                ui.set_min_width(ui.available_width());
                let capture = &preview.capture;
                let seconds = capture.end_ms().saturating_sub(capture.started_ms) as f64 / 1000.0;
                ui.strong(if capture.demo {
                    "Practice recording".into()
                } else {
                    capture.vehicle.label()
                });
                caption(
                    ui,
                    &format!(
                        "{} · {seconds:.1} s · {} readings",
                        recordings::recording_date(capture.started_ms),
                        preview.readings,
                    ),
                );
                if preview.issues > 0 {
                    ui.colored_label(AMBER, format!("{} read issues", preview.issues));
                }
                let markers = capture.markers.len();
                if markers > 0 {
                    caption(
                        ui,
                        &format!(
                            "{markers} {}",
                            if markers == 1 { "marker" } else { "markers" }
                        ),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    play = action(ui, "Replay recording", can_replay);
                    show_graphs = ui
                        .add_enabled(
                            can_replay && preview.has_numeric_data,
                            egui::Button::new("Review graphs"),
                        )
                        .clicked();
                    export = ui
                        .add_enabled(
                            !capture.samples.is_empty() || markers > 0,
                            egui::Button::new("Export full CSV"),
                        )
                        .clicked();
                    if let Some(path) = self
                        .last_export
                        .as_ref()
                        .filter(|path| path.extension().is_some_and(|ext| ext == "csv"))
                    {
                        if ui.button("Open last CSV").clicked() {
                            open_local(path);
                        }
                    }
                });
            });
            if std::mem::take(&mut self.show_recording_preview) {
                ui.scroll_to_rect(preview_card.response.rect, Some(egui::Align::Min));
            }
            if play || show_graphs {
                if self.open_recording(preview.path.clone()) && show_graphs {
                    self.graphs = true;
                    self.page = Page::Live;
                }
            } else if export {
                self.export_capture_csv(preview.capture.clone());
            }
        }
        if let Some((path, error)) = &self.recording_preview_error {
            let message = ui.colored_label(
                AMBER,
                format!(
                    "Could not open {}. {error}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
            );
            if std::mem::take(&mut self.show_recording_preview) {
                message.scroll_to_me(Some(egui::Align::Min));
            }
        }
        #[cfg(not(windows))]
        {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.capture_path)
                        .desired_width(370.0)
                        .hint_text("Path to a recording JSON file"),
                );
                if ui.button("Open recording file").clicked() {
                    self.preview_recording(PathBuf::from(&self.capture_path));
                }
            });
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.strong("Saved recordings");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.recording_search)
                        .desired_width(180.0)
                        .hint_text("Filter dates or filenames"),
                )
                .changed()
            {
                self.recording_list_offset = 0;
            }
        });
        let search = self.recording_search.trim().to_lowercase();
        let paths: Vec<_> = self
            .saved_recordings
            .iter()
            .filter(|entry| entry.matches(&search))
            .collect();
        if paths.is_empty() {
            caption(
                ui,
                if search.is_empty() {
                    "No saved recordings."
                } else {
                    "No recordings match this filter."
                },
            );
        } else {
            const PAGE_SIZE: usize = 20;
            let first = self
                .recording_list_offset
                .min((paths.len() - 1) / PAGE_SIZE * PAGE_SIZE);
            let end = (first + PAGE_SIZE).min(paths.len());
            if paths.len() > PAGE_SIZE {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(first > 0, egui::Button::new("Previous"))
                        .clicked()
                    {
                        self.recording_list_offset = first.saturating_sub(PAGE_SIZE);
                    }
                    caption(ui, &format!("{}–{} of {}", first + 1, end, paths.len()));
                    if ui
                        .add_enabled(end < paths.len(), egui::Button::new("Next"))
                        .clicked()
                    {
                        self.recording_list_offset = end;
                    }
                });
            }
            let selected = self.recording_preview.as_ref().map(|p| p.path.as_path());
            let mut preview_path = None;
            for entry in &paths[first..end] {
                let path = &entry.path;
                let selected = selected == Some(path.as_path());
                if ui
                    .selectable_label(selected, &entry.label)
                    .on_hover_text(path.display().to_string())
                    .clicked()
                {
                    preview_path = Some(path.clone());
                }
            }
            if let Some(path) = preview_path {
                self.preview_recording(path);
            }
        }
    }
    fn reports(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open scan history").clicked() {
            self.page = Page::ScanHistory;
        }
        ui.label("Technician notes");
        ui.add(
            egui::TextEdit::multiline(&mut self.vehicle.notes)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal_wrapped(|ui| {
            if action(ui, "Save report", self.can_export_report()) {
                self.save_report();
            }
            if action(ui, "Print session report", self.can_export_report()) {
                if let Some(path) = self.export_report() {
                    open_local(&path);
                }
            }
            if let Some(path) = &self.last_export {
                if ui.button("Open last saved file").clicked() {
                    open_local(path);
                }
            }
        });
        ui.collapsing("Load a saved report", |ui| {
            let mut paths: Vec<_> = [self.root.join("reports"), self.root.join("baselines")]
                .into_iter()
                .flat_map(|directory| std::fs::read_dir(directory).into_iter().flatten())
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            paths.sort_unstable();
            for path in paths.iter().rev().take(50) {
                if ui
                    .button(path.file_name().unwrap_or_default().to_string_lossy())
                    .clicked()
                {
                    match load_json::<Snapshot>(path) {
                        Ok(b) => {
                            if b.vehicle.has_identity()
                                && b.vehicle.identity() == self.vehicle.identity()
                                && b.demo == self.demo
                            {
                                self.baseline = Some(b);
                                self.status = "Report loaded for comparison.".into();
                            } else {
                                self.status =
                                    "Report belongs to another vehicle or data source.".into();
                            }
                        }
                        Err(e) => self.status = e,
                    }
                }
            }
        });
        if let Some(before) = &self.baseline {
            card(ui, |ui| {
                ui.label(
                    RichText::new("Before / after comparison")
                        .size(19.0)
                        .strong(),
                );
                match comparison(before, &self.snapshot()) {
                    Ok(rows) => {
                        egui::Grid::new("comparison_grid")
                            .num_columns(4)
                            .striped(true)
                            .spacing(Vec2::new(18.0, 10.0))
                            .show(ui, |ui| {
                                for heading in ["Item", "Before", "After", "Assessment"] {
                                    ui.strong(heading);
                                }
                                ui.end_row();
                                for (name, a, b, state) in rows.iter().take(150) {
                                    ui.add(egui::Label::new(name).wrap());
                                    ui.label(a);
                                    ui.label(b);
                                    ui.add(egui::Label::new(state).wrap());
                                    ui.end_row();
                                }
                            });
                    }
                    Err(e) => {
                        ui.colored_label(AMBER, e);
                    }
                }
            });
        } else {
            ui.add_space(16.0);
            caption(
                ui,
                "Save or load a report to compare it with later readings.",
            );
        }
        caption(
            ui,
            "Open the saved report in a browser to print or save it as PDF.",
        );
    }

    fn scan_history(&mut self, ui: &mut egui::Ui) {
        let dir = self.root.join("scans");
        #[cfg(not(windows))]
        ui.add(
            egui::TextEdit::singleline(&mut self.scan_transfer_path)
                .desired_width(300.0)
                .hint_text("Path to import or export a scan file"),
        );
        #[cfg(not(windows))]
        let transfer_path = (!self.scan_transfer_path.trim().is_empty())
            .then(|| PathBuf::from(self.scan_transfer_path.trim()));
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.scan_history.search)
                    .desired_width(300.0)
                    .hint_text("Filter vehicle, VIN, plate, date or work order"),
            );
            if self.scan_history.entries.is_none() || ui.button("Refresh scans").clicked() {
                self.scan_history.refresh(&dir);
            }
            #[cfg(windows)]
            if ui.button("Import scan…").clicked() {
                match super::file_picker::pick(&self.root, scan_file::FILTER, "", None) {
                    Ok(Some(path)) => self.import_scan_file(&path),
                    Ok(None) => {}
                    Err(error) => self.status = error,
                }
            }
            #[cfg(not(windows))]
            if ui
                .add_enabled(transfer_path.is_some(), egui::Button::new("Import scan…"))
                .clicked()
                && let Some(path) = transfer_path.as_deref()
            {
                self.import_scan_file(path);
            }
        });
        if let Some(error) = &self.scan_history.problem {
            ui.colored_label(AMBER, error);
        }
        let pending = self.scan_history.pending_count();
        if pending > 0 {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    AMBER,
                    format!(
                        "{pending} {} waiting to be saved",
                        if pending == 1 { "scan" } else { "scans" }
                    ),
                )
                .on_hover_text(self.scan_history.save_error.as_deref().unwrap_or_default());
                if ui.button("Retry save").clicked() {
                    self.scan_history.retry_saves();
                    if self.scan_history.pending_count() == 0 {
                        self.status = "Pending scans saved to scan history.".into();
                    }
                }
            });
            if self.scan_history.pending_count() > 0 {
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.scan_recovery_dir)
                            .hint_text("Full path to another folder")
                            .desired_width(300.0),
                    );
                    let folder = PathBuf::from(self.scan_recovery_dir.trim());
                    if ui
                        .add_enabled(
                            folder.is_absolute(),
                            egui::Button::new("Save scans to folder"),
                        )
                        .clicked()
                    {
                        let saved = self.scan_history.save_pending_as(&folder);
                        let pending = self.scan_history.pending_count();
                        self.status = if pending == 0 {
                            format!("Saved scans to {}.", folder.display())
                        } else {
                            let detail =
                                self.scan_history.save_error.as_deref().unwrap_or_default();
                            format!("Saved {saved} of {} scans. {detail}", saved + pending)
                        };
                    }
                });
            }
        }

        let search = self.scan_history.search.trim().to_lowercase();
        let entries: Vec<_> = self
            .scan_history
            .entries
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|entry| entry.matches(&search))
            .collect();
        let selected = self.scan_history.selected.as_ref().map(|(path, _)| path);
        let mut next = None;
        if entries.is_empty() {
            caption(
                ui,
                if search.is_empty() {
                    "Completed and interrupted emissions scans appear here."
                } else {
                    "No scans match this filter."
                },
            );
        } else {
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .column(egui_extras::Column::initial(175.0))
                .column(egui_extras::Column::remainder().at_least(145.0))
                .column(egui_extras::Column::initial(201.0))
                .max_scroll_height(245.0)
                .header(27.0, |mut row| {
                    for label in ["Scan date", "Vehicle", "Status"] {
                        row.col(|ui| {
                            ui.strong(label);
                        });
                    }
                })
                .body(|body| {
                    body.rows(31.0, entries.len(), |mut row| {
                        let entry = entries[row.index()];
                        let is_selected = selected == Some(&entry.path);
                        row.col(|ui| {
                            if ui.selectable_label(is_selected, &entry.date).clicked() {
                                next = Some(entry.path.clone());
                            }
                        });
                        row.col(|ui| {
                            let identity = entry.vehicle_id();
                            let title = if identity.is_empty() {
                                entry.title.clone()
                            } else {
                                format!("{} · {identity}", entry.title)
                            };
                            if ui
                                .add(
                                    egui::Label::new(&title)
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_text(title)
                                .clicked()
                            {
                                next = Some(entry.path.clone());
                            }
                        });
                        row.col(|ui| {
                            let label = entry.clear_stage.map_or_else(
                                || entry.state.label().to_owned(),
                                |stage| format!("{} · {}", stage.label(), entry.state.label()),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(&label).color(
                                    if entry.state == emissions_scan::State::Interrupted {
                                        AMBER
                                    } else {
                                        MUTED
                                    },
                                ))
                                .truncate(),
                            )
                            .on_hover_text(label);
                        });
                    });
                });
        }
        if let Some(path) = next {
            if let Err(error) = self.scan_history.select(path) {
                self.status = format!("Could not open saved scan. {error}");
            }
        }
        self.saved_scan_comparison(ui);
        let Some((path, snapshot)) = &self.scan_history.selected else {
            return;
        };
        let path = path.clone();
        let snapshot = snapshot.clone();
        let Some(scan) = &snapshot.emissions_scan else {
            return;
        };
        ui.add_space(8.0);
        let (review, print, export) = card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let actions = ui
                .horizontal_wrapped(|ui| {
                    ui.strong(if snapshot.demo {
                        "Practice scan".into()
                    } else {
                        snapshot.vehicle.label()
                    });
                    caption(ui, &recordings::recording_date(scan.started_ms));
                    if let Some(stage) = scan.clear_stage {
                        caption(ui, stage.label());
                    }
                    (
                        action(ui, "Review scan", true),
                        ui.button("Print saved scan").clicked(),
                        ui.button("Export scan…")
                            .on_hover_text(
                                "Save a scan file with its vehicle details and saved notes.",
                            )
                            .clicked(),
                    )
                })
                .inner;
            ui.horizontal_wrapped(|ui| {
                if valid_vin(&snapshot.vehicle.vin) {
                    ui.monospace(&snapshot.vehicle.vin);
                }
                if !snapshot.vehicle.registration.is_empty() {
                    ui.label(&snapshot.vehicle.registration);
                }
            });
            egui::Grid::new("saved_emissions_checks")
                .striped(true)
                .spacing(Vec2::new(18.0, 5.0))
                .show(ui, |ui| {
                    for (check, result) in scan.checks() {
                        ui.label(check.label());
                        ui.colored_label(
                            if result.is_some_and(|result| result.attention()) {
                                AMBER
                            } else {
                                INK
                            },
                            scan.check_label(result),
                        );
                        ui.end_row();
                    }
                });
            actions
        })
        .inner;
        if review {
            self.open_scan_review(snapshot, Some(path.clone()), Page::ScanHistory);
        }
        if print {
            if let Some(path) = self.export_saved_scan() {
                open_local(&path);
            }
        }
        if export {
            #[cfg(windows)]
            {
                let name = path.with_extension(scan_file::EXTENSION);
                match super::file_picker::pick(
                    &self.root,
                    scan_file::SAVE_FILTER,
                    "",
                    name.file_name(),
                ) {
                    Ok(Some(path)) => self.export_scan_file(path, true),
                    Ok(None) => {}
                    Err(error) => self.status = error,
                }
            }
            #[cfg(not(windows))]
            if let Some(path) = transfer_path {
                self.export_scan_file(path, false);
            } else {
                self.status = "Enter a file path for the exported scan.".into();
            }
        }
    }

    fn saved_scan_comparison(&mut self, ui: &mut egui::Ui) {
        use super::scan_comparison::Assessment;
        let Some((_, snapshot)) = &self.scan_history.selected else {
            return;
        };
        let candidates: Vec<_> = self
            .scan_history
            .entries
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|entry| entry.precedes(snapshot))
            .collect();
        let current = self.scan_history.comparison.as_ref();
        let mut compare = None;
        let mut print = false;
        ui.add_space(10.0);
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.strong("Before / after");
                if candidates.is_empty() && current.is_none() {
                    caption(
                        ui,
                        "Select a later scan with an earlier scan for the same VIN or plate.",
                    );
                } else {
                    let previous = current.map_or_else(
                        || "Choose an earlier scan".to_owned(),
                        |(_, comparison)| recordings::recording_date(comparison.before_ms),
                    );
                    ui.add_enabled_ui(!candidates.is_empty(), |ui| {
                        egui::ComboBox::from_id_salt("comparison_before_scan")
                            .selected_text(previous)
                            .show_ui(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(230.0)
                                    .show(ui, |ui| {
                                        for entry in &candidates {
                                            let selected = current
                                                .is_some_and(|(path, _)| *path == entry.path);
                                            if ui
                                                .selectable_label(
                                                    selected,
                                                    format!(
                                                        "{} · {}",
                                                        entry.date,
                                                        entry.state.label()
                                                    ),
                                                )
                                                .clicked()
                                            {
                                                compare = Some(entry.path.clone());
                                            }
                                        }
                                    });
                            });
                    });
                    print = ui
                        .add_enabled(current.is_some(), egui::Button::new("Print comparison"))
                        .clicked();
                }
            });
        });
        if let Some(path) = compare {
            self.status = match self.scan_history.compare_with(path) {
                Ok(()) => "Saved scan comparison ready.".into(),
                Err(error) => error,
            };
        }
        if print {
            if let Some(path) = self.export_scan_comparison() {
                open_local(&path);
            }
        }
        let Some((_, comparison)) = &self.scan_history.comparison else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            caption(
                ui,
                &format!(
                    "Before · {} · {}",
                    recordings::recording_date(comparison.before_ms),
                    comparison.before_state.label()
                ),
            );
            caption(
                ui,
                &format!(
                    "After · {} · {}",
                    recordings::recording_date(comparison.after_ms),
                    comparison.after_state.label()
                ),
            );
        });
        for (index, section) in comparison.sections.iter().enumerate() {
            egui::CollapsingHeader::new(section.title)
                .id_salt(("saved_scan_comparison", index))
                .default_open(index == 0)
                .show(ui, |ui| {
                    egui_extras::TableBuilder::new(ui)
                        .striped(true)
                        .column(egui_extras::Column::initial(84.0))
                        .column(egui_extras::Column::remainder().at_least(150.0))
                        .column(egui_extras::Column::initial(124.0))
                        .column(egui_extras::Column::initial(124.0))
                        .column(egui_extras::Column::initial(146.0))
                        .max_scroll_height(228.0)
                        .header(25.0, |mut header| {
                            for title in ["ECU", "Item", "Before", "After", "Assessment"] {
                                header.col(|ui| {
                                    ui.strong(title);
                                });
                            }
                        })
                        .body(|body| {
                            body.rows(32.0, section.changes.len(), |mut row| {
                                let change = &section.changes[row.index()];
                                row.col(|ui| {
                                    ui.monospace(ecu_label(change.source));
                                });
                                for text in [&change.item, &change.before, &change.after] {
                                    row.col(|ui| {
                                        ui.add(egui::Label::new(text).truncate())
                                            .on_hover_text(text);
                                    });
                                }
                                row.col(|ui| {
                                    let color = match change.assessment {
                                        Assessment::NewCode | Assessment::Unverified => AMBER,
                                        Assessment::NoLongerReported | Assessment::Changed => {
                                            ACCENT
                                        }
                                        _ => MUTED,
                                    };
                                    let label = change.assessment.label();
                                    ui.add(
                                        egui::Label::new(RichText::new(label).color(color))
                                            .truncate(),
                                    )
                                    .on_hover_text(label);
                                });
                            });
                        });
                });
        }
    }
    fn help(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Request bytes");
            ui.add(
                egui::TextEdit::singleline(&mut self.manual)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(165.0),
            );
            if action(ui, "Run request", self.connected && !self.busy) {
                match self.manual_query() {
                    Ok(q) => self.once(vec![q]),
                    Err(e) => self.status = e,
                }
            }
        });
        caption(ui, "Read services 01, 02, 05, 06, 09 and 0A.");
        let values: Vec<_> = self
            .results
            .rows
            .values()
            .filter(|v| v.latest.reading.name == "Manual diagnostic request")
            .collect();
        self.value_table(ui, &values, ValueTable::Data);
        self.failures(ui, |r| r.name == "Manual diagnostic request");
    }
    fn log_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.collapsing("Diagnostic log", |ui| {
            if ui.button("Save diagnostic log").clicked() {
                let path = self.root.join(format!("diagnostic-log-{}.txt", self.now()));
                let data = self.logs.iter().cloned().collect::<Vec<_>>().join("\n");
                match std::fs::create_dir_all(&self.root).and_then(|_| std::fs::write(&path, data))
                {
                    Ok(()) => {
                        self.status = format!("Saved {}", path.display());
                        self.last_export = Some(path);
                    }
                    Err(e) => self.status = e.to_string(),
                }
            }
            egui::ScrollArea::vertical()
                .id_salt("diagnostic_log")
                .max_height(220.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.logs {
                        ui.add(egui::Label::new(RichText::new(line).monospace().size(12.0)).wrap());
                    }
                });
        });
    }
    fn value_table(&self, ui: &mut egui::Ui, values: &[&ValueState], kind: ValueTable) {
        if values.is_empty() {
            return;
        }
        let stats = matches!(kind, ValueTable::Live);
        let freeze_frame = matches!(kind, ValueTable::FreezeFrame { .. });
        let vehicle_info = values[0].latest.reading.request.starts_with("09");
        ui.push_id(
            match kind {
                ValueTable::Live => "live_values",
                ValueTable::FreezeFrame { .. } => "frame_values",
                ValueTable::Data => values[0].latest.reading.request.as_str(),
            },
            |ui| {
                let mut table = egui_extras::TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                if !freeze_frame {
                    table = table.column(egui_extras::Column::initial(90.0));
                }
                table = table
                    .column(
                        egui_extras::Column::initial(if stats { 180.0 } else { 245.0 })
                            .at_least(110.0),
                    )
                    .column(egui_extras::Column::initial(match kind {
                        ValueTable::FreezeFrame { .. } => 144.0,
                        _ if vehicle_info => 180.0,
                        _ => 100.0,
                    }));
                if !vehicle_info {
                    table = table.column(egui_extras::Column::initial(if stats {
                        64.0
                    } else {
                        85.0
                    }));
                }
                table = table
                    .column(egui_extras::Column::remainder().at_least(120.0))
                    .max_scroll_height(300.0);
                if stats {
                    table = table.column(egui_extras::Column::initial(100.0));
                }
                table
                    .header(27.0, |mut row| {
                        if !freeze_frame {
                            row.col(|ui| {
                                ui.strong("ECU");
                            });
                        }
                        let titles: &[&str] = if freeze_frame {
                            &["Data item", "Captured value", "Unit", "State"]
                        } else if vehicle_info {
                            &["Data item", "Value", "State / age"]
                        } else {
                            &["Data item", "Value", "Unit", "State / age"]
                        };
                        for &title in titles {
                            row.col(|ui| {
                                ui.strong(title);
                            });
                        }
                        if stats {
                            row.col(|ui| {
                                ui.strong("Min / max");
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(36.0, values.len(), |mut row| {
                            let value = &values[row.index()];
                            let r = &value.latest.reading;
                            let age =
                                self.view_time().saturating_sub(value.latest.at_ms) as f64 / 1000.0;
                            let historical = self.results.predates_clear(&value.latest);
                            let failed = self.results.refresh_failed(&value.latest);
                            let before_trigger = matches!(
                                kind,
                                ValueTable::FreezeFrame { trigger_at, .. }
                                    if freeze_frames::predates_trigger(&value.latest, trigger_at)
                            );
                            let trigger_unverified = matches!(
                                kind,
                                ValueTable::FreezeFrame {
                                    trigger_unverified: true,
                                    ..
                                }
                            );
                            if !freeze_frame {
                                row.col(|ui| {
                                    ui.monospace(ecu_label(r.source));
                                });
                            }
                            row.col(|ui| {
                                ui.add(egui::Label::new(reading_label(r)).truncate())
                                    .on_hover_text(format!(
                                        "ECU {} / {}\n{}",
                                        ecu_label(r.source),
                                        r.request,
                                        r.raw
                                    ));
                            });
                            row.col(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(display_value(r)).strong().color(
                                            if historical
                                                || failed
                                                || before_trigger
                                                || trigger_unverified
                                                || r.value == "Not ready"
                                            {
                                                AMBER
                                            } else {
                                                ACCENT
                                            },
                                        ),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&r.value);
                            });
                            if !vehicle_info {
                                row.col(|ui| {
                                    ui.add(egui::Label::new(unit_label(&r.unit)).truncate())
                                        .on_hover_text(&r.unit);
                                });
                            }
                            row.col(|ui| {
                                let (state, color) = if historical {
                                    ("Before clear", AMBER)
                                } else if failed {
                                    ("Refresh failed", AMBER)
                                } else if before_trigger {
                                    ("Before latest trigger", AMBER)
                                } else if trigger_unverified {
                                    ("Trigger unverified", AMBER)
                                } else if r.status == "FAIL" {
                                    ("FAIL", RED)
                                } else if r.status == "PASS" {
                                    ("PASS", ACCENT)
                                } else if freeze_frame {
                                    ("Captured", MUTED)
                                } else if age > 10.0 {
                                    ("Aged", AMBER)
                                } else {
                                    ("Received", MUTED)
                                };
                                let label = if freeze_frame {
                                    state.into()
                                } else {
                                    format!("{state} · {age:.1} s")
                                };
                                ui.colored_label(color, label).on_hover_text(if historical {
                                    "Reading from before the clear request. Reread to verify."
                                } else if before_trigger {
                                    "This value was captured before the latest trigger read."
                                } else if trigger_unverified {
                                    "The trigger could not be verified for this frame."
                                } else {
                                    &r.status
                                });
                            });
                            if stats {
                                row.col(|ui| {
                                    ui.small(match (value.minimum, value.maximum) {
                                        (Some(a), Some(b)) => format!("{a:.2} / {b:.2}"),
                                        _ => "--".into(),
                                    });
                                });
                            }
                        });
                    });
            },
        );
    }
    fn failures(&self, ui: &mut egui::Ui, predicate: impl Fn(&Reading) -> bool) {
        for value in self
            .results
            .failures
            .values()
            .filter(|v| predicate(&v.reading))
        {
            reading_issue(ui, &value.reading);
        }
    }
}
fn reading_issue(ui: &mut egui::Ui, reading: &Reading) {
    ui.colored_label(
        AMBER,
        format!(
            "{} / ECU {} / {}: {}",
            reading.request,
            ecu_label(reading.source),
            reading_label(reading),
            reading.status
        ),
    );
}
fn is_code(request: &str) -> bool {
    codes::Category::from_request(request).is_some() || request == "1902FF"
}
fn graph_selection<'a, Label: AsRef<str>>(
    ui: &mut egui::Ui,
    choices: &[(&str, Label)],
    saved: &mut Option<Vec<String>>,
    window: &mut graphs::Window,
    automatic: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let automatic: Vec<_> = automatic
        .take(graphs::MAX_GRAPHS)
        .map(str::to_owned)
        .collect();
    let mut selected = saved.clone().unwrap_or_else(|| automatic.clone());
    selected.retain(|key| choices.iter().any(|(choice, _)| key == choice));
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("graph_signals")
            .selected_text(format!(
                "Signals · {}/{}",
                selected.len(),
                graphs::MAX_GRAPHS
            ))
            .width(116.0)
            .height(240.0)
            .show_ui(ui, |ui| {
                for (key, title) in choices {
                    let mut checked = selected.iter().any(|item| item == key);
                    if ui
                        .add_enabled(
                            checked || selected.len() < graphs::MAX_GRAPHS,
                            egui::Checkbox::new(&mut checked, title.as_ref()),
                        )
                        .changed()
                    {
                        if checked {
                            selected.push((*key).to_owned());
                        } else {
                            selected.retain(|item| item != key);
                        }
                        changed = true;
                    }
                }
            });
        if saved.is_some() && ui.button("Reset graphs").clicked() {
            *saved = None;
            selected = automatic;
            changed = false;
        }
        egui::ComboBox::from_id_salt("graph_window")
            .selected_text(window.label())
            .width(70.0)
            .show_ui(ui, |ui| {
                for choice in graphs::Window::ALL {
                    ui.selectable_value(window, choice, choice.label());
                }
            });
    });
    if changed {
        *saved = Some(selected.clone());
    }
    selected
}

fn graph_current(ui: &mut egui::Ui, value: Option<&ValueState>, failed: bool) {
    let Some(value) = value else {
        caption(ui, "No reading at the replay position");
        return;
    };
    let reading = &value.latest.reading;
    let unit = unit_label(&reading.unit);
    ui.horizontal_wrapped(|ui| {
        let current = format!("{} {unit}", display_value(reading));
        ui.label(RichText::new(current.trim_end()).color(if failed { AMBER } else { ACCENT }));
        if failed {
            ui.colored_label(AMBER, "Refresh failed");
        } else if value
            .history
            .back()
            .is_some_and(|point| point.value.is_none())
        {
            caption(ui, "Last reading");
        }
    });
}

fn plot(
    ui: &mut egui::Ui,
    trace: &graphs::Trace,
    unit: &str,
    axis: graphs::TimeAxis,
    replay: Option<graphs::ReplayAxis<'_>>,
) -> Option<u64> {
    let clickable = replay.is_some() && axis.end_ms > axis.start_ms;
    let (area, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(100.0), 108.0),
        if clickable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let response = if clickable {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    };
    if clickable {
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Seek replay chart")
        });
    }
    let painter = ui.painter_at(area);
    let rect = area.shrink(4.0);
    let x = |at_ms| egui::lerp(rect.x_range(), axis.fraction(at_ms));
    painter.rect_filled(area, 4.0, CANVAS);
    for i in 0..=4 {
        let fraction = i as f32 / 4.0;
        let x = egui::lerp(rect.x_range(), fraction);
        painter.vline(x, rect.y_range(), Stroke::new(1.0_f32, BORDER));
    }
    for i in 0..=2 {
        let y = egui::lerp(rect.y_range(), i as f32 / 2.0);
        painter.hline(rect.x_range(), y, Stroke::new(1.0_f32, BORDER));
    }
    let pointer = response.hover_pos();
    let time_at = |pointer: egui::Pos2| axis.time_at((pointer.x - rect.left()) / rect.width());
    let tolerance = |pixels: f32| (pixels * axis.duration() as f32 / rect.width()) as u64;
    let marker_at = |pointer: egui::Pos2| {
        replay.and_then(|replay| replay.marker_near(time_at(pointer), tolerance(5.0)))
    };
    let nearest_marker = pointer.and_then(marker_at);
    if let Some(replay) = replay {
        let mut last_x = f32::NEG_INFINITY;
        for marker in replay.markers {
            let marker_x = x(marker.at_ms);
            if marker_x - last_x < 2.0 {
                continue;
            }
            painter.vline(marker_x, rect.y_range(), Stroke::new(1.0_f32, AMBER));
            painter.circle_filled(egui::pos2(marker_x, rect.top() + 4.0), 3.0, AMBER);
            last_x = marker_x;
        }
        painter.vline(
            x(replay.cursor_ms),
            rect.y_range(),
            Stroke::new(1.0_f32, INK),
        );
    }
    let position_at = |at_ms| {
        replay.map_or_else(
            || {
                format!(
                    "{:.2} s ago",
                    axis.end_ms.saturating_sub(at_ms) as f64 / 1000.0
                )
            },
            |replay| recordings::marker_position(at_ms.saturating_sub(replay.started_ms)),
        )
    };
    let mut tooltip = None;
    if let Some((low, high)) = trace.range {
        // Scale before subtracting so finite input never produces an infinite span.
        let scale = high.abs().max(low.abs()).max(1.0);
        let lo = low / scale;
        let span = high / scale - lo;
        let position = |point: GraphPoint| {
            let y = point.value.map_or(0.5, |n| {
                if span > 0.0 {
                    ((n / scale - lo) / span) as f32
                } else {
                    0.5
                }
            });
            egui::pos2(x(point.at_ms), egui::lerp(rect.y_range(), 1.0 - y))
        };
        for run in &trace.runs {
            if let [point] = run.as_slice() {
                painter.circle_filled(position(*point), 3.0, ACCENT);
            } else {
                painter.add(egui::Shape::line(
                    run.iter().copied().map(position).collect(),
                    Stroke::new(2.0_f32, ACCENT),
                ));
            }
        }
        if let Some(pointer) = pointer.filter(|_| nearest_marker.is_none()) {
            let cursor = time_at(pointer);
            painter.vline(
                pointer.x.clamp(rect.left(), rect.right()),
                rect.y_range(),
                Stroke::new(1.0_f32, MUTED),
            );
            if let Some(point) = trace.point_near(cursor, tolerance(6.0)) {
                painter.circle_filled(position(point), 4.0, INK);
                tooltip = Some(format!(
                    "{:.3} {unit} · {}",
                    point.value.unwrap(),
                    position_at(point.at_ms)
                ));
            } else {
                tooltip = Some("No sample at this time".to_owned());
            }
        }
        let range = format!("{low:.2} to {high:.2} {unit}");
        if replay.is_some() {
            caption(ui, range.trim_end());
        } else {
            caption(
                ui,
                &format!("{} · last {} s", range.trim_end(), axis.duration() / 1000),
            );
        }
    } else {
        caption(ui, "No readings in this window");
    }
    if replay.is_some() {
        caption(
            ui,
            &format!(
                "{} – {}",
                position_at(axis.start_ms),
                position_at(axis.end_ms)
            ),
        );
    }
    let jump = if clickable && response.clicked() {
        Some(response.interact_pointer_pos().map_or_else(
            || axis.time_at(0.5),
            |pointer| marker_at(pointer).map_or_else(|| time_at(pointer), |marker| marker.at_ms),
        ))
    } else {
        None
    };
    if let Some(marker) = nearest_marker {
        let label = if marker.note.is_empty() {
            "Event marker"
        } else {
            &marker.note
        };
        response.on_hover_text_at_pointer(format!("{} · {label}", position_at(marker.at_ms)));
    } else if let Some(tooltip) = tooltip {
        response.on_hover_text_at_pointer(tooltip);
    } else if clickable {
        response.on_hover_text_at_pointer("Click to move the replay cursor");
    }
    jump
}
