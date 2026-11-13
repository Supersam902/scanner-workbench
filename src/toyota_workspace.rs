//! Toyota/Lexus/Mercedes vehicle -> system -> function workflow.
//! No module is marked present merely because it appears in a profile.
use super::*;
use egui::{Color32, RichText};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    Vehicle,
    Systems,
    Data,
    Codes,
    Utilities,
    Information,
}

pub(super) struct Workspace {
    page: Page,
    make: String,
    model: Option<usize>,
    year: String,
    model_search: String,
    applied: Option<String>,
    applied_make: Option<String>,
    system: Option<String>,
    system_names: BTreeMap<String, String>,
    experimental: bool,
    omitted: usize,
    pub(super) choose_data: bool,
    graphs: bool,
    details: bool,
    import_path: String,
    pub(super) confirm_clear: bool,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            page: Page::Vehicle,
            make: "Toyota".into(),
            model: None,
            year: String::new(),
            model_search: String::new(),
            applied: None,
            applied_make: None,
            system: None,
            system_names: BTreeMap::new(),
            experimental: false,
            omitted: 0,
            choose_data: false,
            graphs: false,
            details: false,
            import_path: String::new(),
            confirm_clear: false,
        }
    }
}

fn profiles() -> &'static [(&'static str, &'static str)] {
    static ALL: std::sync::OnceLock<Vec<(&'static str, &'static str)>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        toyota_profiles::PROFILES
            .iter()
            .chain(mercedes_profiles::PROFILES.iter())
            .copied()
            .collect()
    })
    .as_slice()
}
fn source_count(name: &str) -> usize {
    static COUNTS: std::sync::OnceLock<BTreeMap<String, usize>> = std::sync::OnceLock::new();
    let counts = COUNTS.get_or_init(|| {
        let mut counts = BTreeMap::new();
        for json in [
            include_str!("../data/obdb/toyota-family/manifest.json"),
            include_str!("../data/obdb/mercedes-family/manifest.json"),
        ] {
            if let Ok(root) = serde_json::from_str::<Value>(json) {
                if let Some(entries) = root["profiles"].as_array() {
                    for entry in entries {
                        if let (Some(name), Some(commands)) =
                            (entry["name"].as_str(), entry["commands"].as_u64())
                        {
                            counts.insert(name.to_string(), commands as usize);
                        }
                    }
                }
            }
        }
        counts
    });
    counts.get(name).copied().unwrap_or(0)
}
fn current_year() -> u16 {
    chrono::Local::now()
        .format("%Y")
        .to_string()
        .parse()
        .unwrap_or(2026)
}
fn normalize_model(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
pub(super) fn profile_name(index: usize) -> String {
    let name = profiles()[index].0;
    if let Some(model) = name.strip_prefix("Mercedes-Benz-") {
        format!("Mercedes-Benz {model}")
    } else {
        name.replace('-', " ")
    }
}

/// OBDb contains experimental and transport-specific definitions. Keep them
/// separate from definitions the native adapter can actually transmit.
pub(super) fn load_profile(
    text: &str,
    year: u16,
    experimental: bool,
) -> Result<(Vec<Query>, BTreeMap<String, String>, usize), String> {
    if text.len() > 4 * 1024 * 1024 {
        return Err("Profile exceeds 4 MiB".into());
    }
    let mut root: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut types = BTreeMap::new();
    if let Some(ecus) = root["ecu"].as_array() {
        for ecu in ecus {
            if ecu["eax"].is_null() {
                if let (Some(hdr), Some(kind)) = (ecu["hdr"].as_str(), ecu["type"].as_str()) {
                    let name = match kind {
                        "engine" => "Engine / powertrain",
                        "transmission" => "Transmission",
                        "hybridControl" => "Hybrid control",
                        "battery" => "Traction battery",
                        "tpms" => "Tire pressure",
                        _ => kind,
                    };
                    types.insert(hdr.to_ascii_uppercase(), name.to_string());
                }
            }
        }
    }
    let commands = root["commands"]
        .as_array_mut()
        .ok_or("Missing commands array")?;
    let before = commands.len();
    commands.retain(|c| {
        let mode = c["cmd"]
            .as_object()
            .and_then(|m| m.keys().next())
            .map(String::as_str);
        let readable = matches!(mode, Some("01" | "21" | "22"));
        let transport = [
            "eax", "fcm1", "proto", "baud", "init", "din", "dout", "pri", "tst",
        ]
        .iter()
        .all(|key| c[*key].is_null());
        let addressed = mode == Some("01") || (c["hdr"].is_string() && c["rax"].is_string());
        readable && transport && addressed && (experimental || c["dbg"] != true)
    });
    let omitted = before - commands.len();
    let queries = catalog(&root.to_string(), Some(year))?;
    Ok((queries, types, omitted))
}

fn system_key(query: &Query) -> String {
    query
        .target
        .map(|t| format!("{:X}:{:X}", t.request, t.response))
        .unwrap_or_else(|| "generic".into())
}

fn system_name(key: &str, queries: &[Query], declared: &BTreeMap<String, String>) -> String {
    if key == "generic" {
        return "OBD-II Engine".into();
    }
    let header = key.split(':').next().unwrap_or("");
    if let Some(name) = declared.get(header) {
        return name.clone();
    }
    // These labels describe the profile's data, not a verified ECU identity.
    let text = queries
        .iter()
        .filter(|q| system_key(q) == key)
        .map(|q| q.name.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if text.contains("engine oil") || text.contains("misfire") {
        "Engine / powertrain data".into()
    } else if text.contains("transmission") {
        "Transmission data".into()
    } else if text.contains("wheel speed") || text.contains("brake warning") {
        "Brake / chassis data".into()
    } else if text.contains("evaporator") || text.contains("cabin temperature") {
        "Climate control data".into()
    } else if text.contains("buckle switch") {
        "Occupant restraint data".into()
    } else if text.contains("seatbelt")
        || text.contains("seat belt")
        || text.contains("instrument")
        || text.contains("fuel level")
    {
        "Instrument / body data".into()
    } else {
        format!("Additional system {header}")
    }
}

impl Panel {
    fn is_mercedes(&self) -> bool {
        self.workspace.applied_make.as_deref() == Some("Mercedes-Benz")
    }
    /// Only available in development builds for layout inspection. Never opens
    /// an adapter or transmits a vehicle request.
    #[cfg(debug_assertions)]
    pub fn configure_toyota_preview(&mut self, step: &str) {
        let mercedes = step.starts_with("mercedes-");
        let step = step.strip_prefix("mercedes-").unwrap_or(step);
        if mercedes {
            self.workspace.make = "Mercedes-Benz".into();
        }
        if step == "vehicle" {
            return;
        }
        self.toyota_active = true;
        self.workspace.year = "2020".into();
        let index = profiles()
            .iter()
            .position(|(name, _)| {
                *name
                    == if mercedes {
                        "Mercedes-Benz-E-Class"
                    } else {
                        "Toyota-RAV4"
                    }
            })
            .unwrap();
        self.apply_workspace_profile(profile_name(index), profiles()[index].1);
        if step == "systems" {
            return;
        }
        let target = self
            .toyota
            .iter()
            .find(|q| q.target.is_some_and(|t| t.request == 0x7e0))
            .or_else(|| self.toyota.first())
            .map(system_key);
        if let Some(target) = target {
            self.select_workspace_system(target);
        }
        if step == "codes" {
            self.workspace.page = Page::Codes;
            return;
        }
        let queries = self.workspace_queries();
        for (index, q) in queries.iter().take(3).enumerate() {
            self.selected.insert(index);
            for signal in q.signals.iter().filter(|s| s["hidden"] != true).take(3) {
                let value = Reading {
                    source: q.target.map(|t| t.response),
                    request: encode_hex(&q.bytes),
                    name: signal["name"].as_str().unwrap_or("Preview item").into(),
                    value: "--".into(),
                    unit: signal["fmt"]["unit"].as_str().unwrap_or("").into(),
                    raw: String::new(),
                    numeric: None,
                    status: "UI PREVIEW - no vehicle data".into(),
                    signal_failure: false,
                };
                self.update(Update::Row(self.id, value));
            }
        }
        self.status = "UI PREVIEW - no vehicle connection or readings".into();
    }
    fn stop_workspace(&mut self) {
        self.cancel();
        self.repeat = false;
        self.workspace.confirm_clear = false;
    }
    fn workspace_context_changed(&mut self) {
        self.reset();
        self.selected.clear();
        self.filter.clear();
        self.workspace.confirm_clear = false;
    }
    fn workspace_queries(&self) -> Vec<Query> {
        if self.workspace.system.as_deref() == Some("generic") {
            self.generic_queries()
        } else {
            self.toyota
                .iter()
                .filter(|q| Some(system_key(q).as_str()) == self.workspace.system.as_deref())
                .cloned()
                .collect()
        }
    }
    fn select_workspace_system(&mut self, system: String) {
        self.workspace_context_changed();
        self.workspace.system = Some(system);
        self.service = 1;
        self.workspace.page = Page::Data;
    }
    fn apply_workspace_profile(&mut self, name: String, text: &str) {
        let year = self.workspace.year.trim().parse::<u16>();
        let Ok(year) = year else {
            self.status = "Enter the vehicle's model year".into();
            return;
        };
        let make = if name.starts_with("Mercedes-Benz") {
            "Mercedes-Benz"
        } else if name.starts_with("Toyota") {
            "Toyota"
        } else if name.starts_with("Lexus") {
            "Lexus"
        } else {
            self.workspace.make.as_str()
        }
        .to_string();
        let (min_year, max_year) = if make == "Mercedes-Benz" {
            (2005, current_year())
        } else {
            (1996, current_year() + 1)
        };
        if !(min_year..=max_year).contains(&year) {
            self.status = format!("Choose a {make} model year from {min_year} to {max_year}");
            return;
        }
        match load_profile(text, year, self.workspace.experimental) {
            Ok((queries, types, omitted)) => {
                self.workspace_context_changed();
                self.toyota = queries;
                self.workspace.system_names = types;
                self.workspace.omitted = omitted;
                self.workspace.applied = Some(format!("{year} {name}"));
                self.workspace.applied_make = Some(make);
                self.workspace.system = None;
                self.workspace.page = Page::Systems;
                self.status =
                    "Vehicle profile loaded. Select a system; module presence is not yet verified."
                        .into();
            }
            Err(e) => self.status = e,
        }
    }
    pub(super) fn show_toyota_workspace(
        &mut self,
        ui: &mut egui::Ui,
        connected: bool,
        can: bool,
    ) -> Option<Job> {
        if !self.toyota_active {
            self.reset();
            self.service = 1;
            self.selected.clear();
            self.toyota_active = true;
            self.status = if self.workspace.applied.is_none() {
                "Select a vehicle, then connect the VCI."
            } else {
                "Vehicle workspace selected. Choose data items to begin."
            }
            .into();
        }
        let mut outgoing = None;
        ui.horizontal(|ui| {
            ui.heading("VEHICLE DIAGNOSTICS");
            ui.label(RichText::new("TOYOTA / LEXUS / MERCEDES").color(Color32::LIGHT_BLUE));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(if connected {
                    RichText::new("VCI CONNECTED").color(Color32::LIGHT_GREEN)
                } else {
                    RichText::new("VCI DISCONNECTED").color(Color32::from_rgb(240, 170, 80))
                });
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.strong(
                self.workspace
                    .applied
                    .as_deref()
                    .unwrap_or("Select vehicle"),
            );
            if let Some(system) = &self.workspace.system {
                ui.label("/ ");
                ui.label(system_name(
                    system,
                    &self.toyota,
                    &self.workspace.system_names,
                ));
            }
            if self.busy {
                ui.spinner();
                ui.label("Reading...");
            }
        });
        if self.is_mercedes() && self.workspace.page != Page::Vehicle {
            ui.small("Mercedes: year selection does not establish complete coverage. CAN/K-line only; DoIP, SCN coding and security access are unavailable.");
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            for (page, label) in [
                (Page::Vehicle, "1  Vehicle"),
                (Page::Systems, "2  Systems"),
                (Page::Data, "Data List"),
                (Page::Codes, "Trouble Codes"),
                (Page::Utilities, "Utilities"),
                (Page::Information, "Coverage / Info"),
            ] {
                let enabled = page == Page::Vehicle
                    || self.workspace.applied.is_some()
                        && (page == Page::Systems
                            || page == Page::Information
                            || self.workspace.system.is_some());
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(label).selected(self.workspace.page == page),
                    )
                    .clicked()
                {
                    self.stop_workspace();
                    self.workspace.page = page;
                }
            }
        });
        ui.separator();
        match self.workspace.page {
            Page::Vehicle => self.vehicle_page(ui),
            Page::Systems => self.systems_page(ui),
            Page::Data => {
                outgoing = self.data_page(ui, connected, can);
            }
            Page::Codes => {
                outgoing = self.codes_page(ui, connected, can);
            }
            Page::Utilities => {
                ui.heading("System functions");
                ui.label("Data List uses the loaded profile. Trouble Codes provides generic engine codes or read-only Mercedes UDS codes on supported CAN ECUs.");
                ui.add_space(12.0);
                for feature in [
                    "OEM active tests",
                    "Coding and customization",
                    "Adaptations / calibrations",
                    "Immobilizer registration",
                    "ECU programming",
                ] {
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            false,
                            egui::Button::new(feature).min_size(egui::vec2(240.0, 32.0)),
                        );
                        ui.label("Not implemented");
                    });
                }
            }
            Page::Information => {
                egui::ScrollArea::vertical().max_height((ui.available_height()-50.0).max(100.0)).id_salt("toyota_coverage").show(ui,|ui|{
                    ui.heading("Profile coverage");
                    ui.label(format!("{} local model entries: 75 Toyota/Lexus (61 with definitions), 36 Mercedes-Benz (19 with definitions).",profiles().len()));
                    ui.label(format!("{} commands loaded for the selected year; {} experimental or unsupported transport definitions omitted.",self.toyota.len(),self.workspace.omitted));
                    ui.label("Profiles describe available definitions, not a completed module scan. Equipment, engine, market and ECU variants can differ.");
                    ui.label("System names come from profile metadata where provided; other labels describe the kind of data in that group. Addresses are available in Details.");
                    ui.label("Extended addressing, custom flow-control settings and custom initialization definitions are excluded. Imports accept only supported data reads.");
                    ui.label("Toyota module DTCs and OEM utilities are unavailable. Mercedes CAN systems can attempt read-only UDS DTCs if the ECU implements service 19. Raw identifiers have no OEM fault descriptions.");
                    ui.label("Mercedes selection covers 2005 through the current year; untagged definitions are not proven for every selected year. Newer DoIP-only vehicles/systems are outside this CAN/K-line backend. Its connection handshake requires an OBD supported-PID response, so some EVs may not connect.");
                    ui.hyperlink_to("Mercedes connection limitations", "https://b2bconnect.mercedes-benz.com/ie/help/faq/software/xentry-pass-thru-eu");
                    ui.hyperlink_to("OBDb data sources", "https://github.com/OBDb");
                    ui.small("Original data: CC BY-SA 4.0, OBDb contributors. Attribution and source manifest accompany the app.");
                });
            }
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if self.busy {
                if ui.button("Stop operation").clicked() {
                    self.stop_workspace();
                }
            }
            ui.label(&self.status);
        });
        outgoing
    }
    fn vehicle_page(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().max_height((ui.available_height()-50.0).max(100.0)).id_salt("vehicle_selection").show(ui,|ui|{
            ui.add_space(8.0); ui.heading("Vehicle selection");
            ui.label("Choose the vehicle you are working on. No model is selected automatically.");
            ui.add_space(12.0);
            ui.horizontal(|ui|{
                for make in ["Toyota","Lexus","Mercedes-Benz"] { if ui.selectable_value(&mut self.workspace.make,make.into(),RichText::new(make).size(18.0)).changed(){self.workspace.model=None;self.workspace.model_search.clear();} }
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui|{
                ui.label("Find model"); ui.add(egui::TextEdit::singleline(&mut self.workspace.model_search).desired_width(180.0).hint_text("RAV4, E-Class, C-Class..."));
                egui::ComboBox::from_id_salt("toyota_model").width(230.0).selected_text(self.workspace.model.map(profile_name).unwrap_or_else(||"Select a model".into())).show_ui(ui,|ui|{
                    for (index,(name,_)) in profiles().iter().enumerate(){
                        if name.starts_with(&format!("{}-",self.workspace.make)) && normalize_model(name).contains(&normalize_model(&self.workspace.model_search)) {
                            let label=if source_count(name)==0{format!("{} (generic OBD-II only)",profile_name(index))}else{profile_name(index)};ui.selectable_value(&mut self.workspace.model,Some(index),label);
                        }
                    }
                });
                ui.label("Model year"); ui.add(egui::TextEdit::singleline(&mut self.workspace.year).desired_width(65.0).hint_text("YYYY"));
            });
            ui.add_space(10.0);
            if self.workspace.make=="Mercedes-Benz" {
                ui.label(format!("Mercedes model years: 2005-{}. Most enhanced definitions are experimental; some newer systems need DoIP, which this app does not support.",current_year()));
                if ui.add_enabled(!self.busy,egui::Button::new("Use generic OBD-II for another Mercedes model")).clicked(){self.apply_workspace_profile("Mercedes-Benz (generic OBD-II)".into(),r#"{"commands":[]}"#);}
            }
            ui.checkbox(&mut self.workspace.experimental,"Include experimental community definitions").on_hover_text("Experimental definitions have not been established for every ECU variant. Unsupported transport definitions remain excluded.");
            if ui.add_enabled(!self.busy&&self.workspace.model.is_some(),egui::Button::new("Continue to system selection").min_size(egui::vec2(260.0,38.0))).clicked(){
                let index=self.workspace.model.unwrap(); self.apply_workspace_profile(profile_name(index),profiles()[index].1);
            }
            ui.add_space(14.0);
            ui.group(|ui|{
                ui.strong("Vehicle setup");
                ui.label("Engine, transmission, hybrid/battery, chassis and body data are organized by the selected model's definitions. An empty profile provides no enhanced commands; generic OBD-II is available only when the vehicle supports it.");
                ui.label("Use the correct CAN or older ISO 9141/KWP protocol in the connection bar. Vehicle selection does not change the adapter protocol automatically.");
            });
            ui.add_space(12.0);
            ui.collapsing("Import a local vehicle profile",|ui|{
                ui.horizontal_wrapped(|ui|{ui.add(egui::TextEdit::singleline(&mut self.workspace.import_path).desired_width(380.0).hint_text("OBDb JSON file path"));
                    #[cfg(not(target_arch="wasm32"))]
                    if ui.add_enabled(!self.busy,egui::Button::new("Import and use")).clicked(){
                        let path=self.workspace.import_path.clone();
                        let text=std::fs::metadata(&path).map_err(|e|e.to_string()).and_then(|m|if m.len()>4*1024*1024{Err("Profile exceeds 4 MiB".into())}else{std::fs::read_to_string(&path).map_err(|e|e.to_string())});
                        match text {Ok(text)=>self.apply_workspace_profile("Imported profile".into(),&text),Err(e)=>self.status=e}
                    }
                });
                ui.small("Enter the model year above before importing. Transport-specific and experimental exclusions apply to imports too.");
            });
        });
    }
    fn systems_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("System selection");
        ui.label("Profile systems - not yet a vehicle health scan. Select a system to open its data list.");
        let mut systems = BTreeSet::from(["generic".to_string()]);
        systems.extend(self.toyota.iter().map(system_key));
        let cards = systems
            .into_iter()
            .map(|key| {
                let name = system_name(&key, &self.toyota, &self.workspace.system_names);
                let count = if key == "generic" {
                    self.generic.len()
                } else {
                    self.toyota.iter().filter(|q| system_key(q) == key).count()
                };
                (key, name, count)
            })
            .collect::<Vec<_>>();
        let columns = if ui.available_width() > 780.0 { 3 } else { 2 };
        let width = ((ui.available_width() - 20.0) / columns as f32).max(180.0);
        egui::ScrollArea::vertical().max_height((ui.available_height()-50.0).max(100.0)).id_salt("toyota_systems").show(ui,|ui|{
            egui::Grid::new("system_tiles").num_columns(columns).spacing(egui::vec2(8.0,8.0)).show(ui,|ui|{
                for (index,(key,name,count)) in cards.iter().enumerate(){
                    let response=ui.add_sized([width,86.0],egui::Button::new(RichText::new(format!("{name}\n{count} data commands\nNot scanned")).size(15.0)));
                    if response.on_hover_text(if key=="generic"{"Generic emissions services".into()}else{format!("Profile addresses: {}",key.replace(':'," / "))}).clicked(){self.select_workspace_system(key.clone());}
                    if (index+1)%columns==0 {ui.end_row();}
                }
            });
            if self.toyota.is_empty(){ui.colored_label(Color32::from_rgb(240,170,80),"No enhanced definitions are available with the current year and filters. Generic OBD-II is available; inspect Coverage / Info or another matching profile.");}
        });
    }
    fn data_page(&mut self, ui: &mut egui::Ui, connected: bool, can: bool) -> Option<Job> {
        let mut outgoing = None;
        let generic = self.workspace.system.as_deref() == Some("generic");
        ui.horizontal_wrapped(|ui| {
            ui.heading("Data List");
            if ui.button("Select data items").clicked() {
                self.workspace.choose_data = true;
            }
            let can_read = connected && !self.busy && !self.selected.is_empty();
            if ui
                .add_enabled(can_read, egui::Button::new("Read once"))
                .clicked()
            {
                self.repeat = false;
                let queries = self.workspace_queries();
                let selected = self
                    .selected
                    .iter()
                    .filter_map(|i| queries.get(*i).cloned())
                    .collect();
                outgoing = Some(self.job(selected, None, false));
            }
            if ui
                .add_enabled(can_read && !self.repeat, egui::Button::new("Start live"))
                .clicked()
            {
                self.repeat = true;
            }
            if ui
                .add_enabled(self.repeat || self.busy, egui::Button::new("Stop"))
                .clicked()
            {
                self.stop_workspace();
            }
            if generic
                && ui
                    .add_enabled(
                        connected && !self.busy,
                        egui::Button::new("Discover supported items"),
                    )
                    .clicked()
            {
                self.repeat = false;
                outgoing = Some(self.job(Vec::new(), Some((1, 0)), false));
            }
            ui.checkbox(&mut self.workspace.graphs, "Graphs");
            ui.checkbox(&mut self.workspace.details, "Details");
        });
        if !connected {
            ui.label("Connect the VCI before reading vehicle data.");
        }
        if connected && !can && !generic {
            ui.colored_label(Color32::from_rgb(240,170,80),"This profile uses CAN ECU addresses. For K-line use a matching profile with one-byte addresses; advanced requests are available below.");
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} commands selected", self.selected.len()));
            ui.label("Refresh delay");
            ui.add(
                egui::DragValue::new(&mut self.interval)
                    .range(0.5..=60.0)
                    .suffix(" s"),
            );
            ui.checkbox(&mut self.record, "Record CSV");
            if self.record {
                ui.add(egui::TextEdit::singleline(&mut self.record_path).desired_width(200.0));
            }
        });
        let queries = self.workspace_queries();
        if self.repeat
            && connected
            && !self.busy
            && outgoing.is_none()
            && self.last_run.elapsed() >= Duration::from_secs_f32(self.interval)
        {
            let selected = self
                .selected
                .iter()
                .filter_map(|i| queries.get(*i).cloned())
                .filter(|q| read_service(q.bytes[0]))
                .collect::<Vec<_>>();
            if !selected.is_empty() {
                outgoing = Some(self.job(selected, None, false));
            } else {
                self.repeat = false;
            }
        }
        if self.workspace.choose_data {
            let mut open = true;
            egui::Window::new("Select data items").open(&mut open).default_size([660.0,440.0]).collapsible(false).show(ui.ctx(),|ui|{
                ui.horizontal(|ui|{ui.label("Search");ui.text_edit_singleline(&mut self.filter);if ui.button("Clear selection").clicked(){self.selected.clear();}});
                ui.small("Items are grouped by one ECU request. Selecting a group reads every signal in that group.");
                let supported=self.support.get(&1);
                let visible=queries.iter().enumerate().filter(|(_,q)|(!generic||supported.is_none_or(|p|q.bytes.get(1).is_some_and(|pid|p.contains(pid)))) && (self.filter.is_empty() || q.name.to_lowercase().contains(&self.filter.to_lowercase()))).map(|(i,_)|i).collect::<Vec<_>>();
                ui.horizontal(|ui|{if ui.button("Select visible").clicked(){self.selected.extend(visible.iter());}ui.label(format!("{} available groups",visible.len()));});
                egui::ScrollArea::vertical().id_salt("toyota_data_picker").show(ui,|ui|{
                    for index in visible {
                        let q=&queries[index];let mut selected=self.selected.contains(&index);
                        let label=if q.name.is_empty(){"Unnamed data group"}else{&q.name};
                        if ui.checkbox(&mut selected,label).on_hover_text(format!("Request: {} | Addresses: {}",encode_hex(&q.bytes),system_key(q).replace(':'," / "))).changed(){if selected{self.selected.insert(index);}else{self.selected.remove(&index);}}
                    }
                });
            });
            self.workspace.choose_data = open;
        }
        ui.add_space(6.0);
        let rows = self
            .rows
            .values()
            .filter(|r| {
                queries.iter().enumerate().any(|(index, q)| {
                    self.selected.contains(&index)
                        && encode_hex(&q.bytes) == r.reading.request
                        && (q.target.is_none() || q.target.map(|t| t.response) == r.reading.source)
                })
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            ui.group(|ui|{ui.add_space(16.0);ui.heading(if self.selected.is_empty(){"Choose data items to begin"}else{"Waiting for readings"});ui.label("Use Select data items, choose a small group, then Read once or Start live. Values appear here after the ECU responds.");ui.add_space(16.0);});
        } else {
            let height = (ui.available_height() - 85.0).max(100.0);
            egui::ScrollArea::both()
                .id_salt("toyota_live_results")
                .max_height(height)
                .show(ui, |ui| {
                    egui::Grid::new("toyota_values")
                        .striped(true)
                        .spacing(egui::vec2(18.0, 8.0))
                        .min_col_width(65.0)
                        .show(ui, |ui| {
                            for label in ["Data item", "Value", "Unit", "State / age"] {
                                ui.strong(label);
                            }
                            if self.workspace.graphs {
                                ui.strong("Trend");
                            }
                            if self.workspace.details {
                                ui.strong("ECU / Request / Raw response");
                            }
                            ui.end_row();
                            for display in rows {
                                let r = &display.reading;
                                ui.add_sized(
                                    [280.0, 24.0],
                                    egui::Label::new(&r.name).wrap().halign(egui::Align::LEFT),
                                )
                                .on_hover_text(&r.name);
                                ui.strong(&r.value);
                                ui.label(&r.unit);
                                let age = display.received.elapsed().as_secs_f32();
                                let state = if matches!(r.value.as_str(), "Unavailable" | "Blocked")
                                {
                                    "Unavailable"
                                } else if age > 10.0 {
                                    "Snapshot"
                                } else {
                                    "Updated"
                                };
                                ui.label(format!("{state} | {age:.1}s"))
                                    .on_hover_text(&r.status);
                                if self.workspace.graphs {
                                    sparkline(ui, &display.history);
                                }
                                if self.workspace.details {
                                    ui.monospace(format!(
                                        "{} / {} / {}",
                                        r.source.map(|s| format!("{s:X}")).unwrap_or_default(),
                                        r.request,
                                        r.raw
                                    ));
                                }
                                ui.end_row();
                            }
                        });
                });
        }
        ui.collapsing("Advanced diagnostic request",|ui|{
            ui.small("Data-read services 21/22 only. Enter documented ECU addresses; these are not data-item numbers.");
            ui.horizontal_wrapped(|ui|{ui.label("Hex request");ui.add(egui::TextEdit::singleline(&mut self.manual).desired_width(110.0));ui.label("Request ECU");ui.add(egui::TextEdit::singleline(&mut self.request_header).desired_width(80.0));ui.label("Response ECU");ui.add(egui::TextEdit::singleline(&mut self.response_header).desired_width(80.0));
                if ui.add_enabled(connected&&!self.busy,egui::Button::new("Send once")).clicked(){
                    let bytes=decode_hex(&self.manual).map_err(|e|e.to_string());
                    let target=u32::from_str_radix(self.request_header.trim(),16).and_then(|request|u32::from_str_radix(self.response_header.trim(),16).map(|response|DiagnosticTarget{request,response})).map_err(|e|e.to_string());
                    match (bytes,target) {
                        (Ok(bytes),Ok(target)) if matches!(bytes[0],0x21|0x22)=>{self.repeat=false;self.selected.clear();self.toyota.push(Query{bytes:bytes.clone(),target:Some(target),name:"Manual data request".into(),signals:Vec::new(),verified:false});self.workspace.system=Some(system_key(self.toyota.last().unwrap()));let queries=self.workspace_queries();self.selected.insert(queries.len()-1);outgoing=Some(self.job(vec![queries.last().unwrap().clone()],None,false));},
                        (Err(e),_)|(_,Err(e))=>self.status=e,
                        _=>self.status="Only read services 21 and 22 are accepted here".into(),
                    }
                }
            });
        });
        outgoing
    }
    fn codes_page(&mut self, ui: &mut egui::Ui, connected: bool, can: bool) -> Option<Job> {
        ui.heading("Trouble Codes");
        let generic = self.workspace.system.as_deref() == Some("generic");
        if !generic {
            if self.is_mercedes() {
                return self.mercedes_codes_page(ui, connected, can);
            }
            ui.colored_label(Color32::from_rgb(240,170,80),"Enhanced Toyota DTC reading is not implemented for this system. Select OBD-II Engine for generic emissions codes.");
            return None;
        }
        let mut outgoing = None;
        ui.label("Engine emissions only: stored, pending and permanent codes. This is not an all-system scan.");
        ui.small("Previous code readings remain visible with their age until refreshed. Re-read after a clear attempt.");
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    connected && !self.busy,
                    egui::Button::new("Read trouble codes"),
                )
                .clicked()
            {
                let queries = [3, 7, 10]
                    .iter()
                    .map(|mode| Query {
                        bytes: vec![*mode],
                        target: None,
                        name: match mode {
                            3 => "Stored DTCs",
                            7 => "Pending DTCs",
                            _ => "Permanent DTCs",
                        }
                        .into(),
                        signals: Vec::new(),
                        verified: false,
                    })
                    .collect();
                outgoing = Some(self.job(queries, None, false));
            }
            if ui
                .add_enabled(
                    connected && !self.busy,
                    egui::Button::new("Clear emissions codes..."),
                )
                .clicked()
            {
                self.workspace.confirm_clear = true;
            }
        });
        if self.workspace.confirm_clear {
            ui.group(|ui|{ui.colored_label(Color32::from_rgb(240,170,80),"This erases emissions codes, freeze-frame data and readiness status. Permanent codes cannot be erased this way.");ui.horizontal(|ui|{if ui.add_enabled(connected&&!self.busy,egui::Button::new("Confirm clear")).clicked(){self.workspace.confirm_clear=false;outgoing=Some(self.job(vec![Query{bytes:vec![4],target:None,name:"Clear emissions data".into(),signals:Vec::new(),verified:false}],None,true));}if ui.button("Cancel").clicked(){self.workspace.confirm_clear=false;}});});
        }
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - 50.0).max(100.0))
            .id_salt("toyota_generic_codes")
            .show(ui, |ui| {
                for display in self
                    .rows
                    .values()
                    .filter(|r| ["03", "04", "07", "0A"].contains(&r.reading.request.as_str()))
                {
                    ui.group(|ui| {
                        ui.strong(&display.reading.name);
                        ui.label(&display.reading.value);
                        ui.small(format!(
                            "ECU {} | {} | Read {:.1}s ago",
                            display
                                .reading
                                .source
                                .map(|s| format!("{s:X}"))
                                .unwrap_or_default(),
                            display.reading.status,
                            display.received.elapsed().as_secs_f32()
                        ));
                    });
                }
            });
        outgoing
    }
    fn mercedes_codes_page(
        &mut self,
        ui: &mut egui::Ui,
        connected: bool,
        can: bool,
    ) -> Option<Job> {
        ui.label("Selected Mercedes ECU: read-only UDS DTC information. The ECU must support service 19; older KWP-specific fault services are unavailable.");
        ui.small("DTCs are raw 24-bit identifiers with ECU status flags, without Mercedes fault descriptions. A rejected/timed-out read preserves the previous results and their age.");
        let target = self.workspace_queries().iter().find_map(|q| q.target);
        let mut outgoing = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    connected && can && !self.busy && target.is_some(),
                    egui::Button::new("Read ECU DTCs (UDS)"),
                )
                .clicked()
            {
                self.repeat = false;
                outgoing = Some(self.job(
                    vec![Query {
                        bytes: vec![0x19, 2, 0xff],
                        target,
                        name: "UDS DTC scan".into(),
                        signals: Vec::new(),
                        verified: false,
                    }],
                    None,
                    false,
                ));
            }
            ui.add_enabled(false, egui::Button::new("Clear ECU fault memory"));
            ui.label("Read only - no OEM clearing, sessions or security access");
        });
        if !can {
            ui.colored_label(
                Color32::from_rgb(240, 170, 80),
                "This UDS function is available only on the CAN connection path.",
            );
        }
        egui::ScrollArea::vertical()
            .id_salt("mercedes_uds_codes")
            .max_height((ui.available_height() - 50.0).max(100.0))
            .show(ui, |ui| {
                for display in self.rows.values().filter(|display| {
                    display.reading.request == "1902FF"
                        && display.reading.source == target.map(|t| t.response)
                }) {
                    let r = &display.reading;
                    ui.group(|ui| {
                        ui.strong(&r.name);
                        ui.label(&r.value);
                        ui.label(&r.status);
                        ui.small(format!(
                            "ECU {} | {:.1}s ago",
                            r.source.map(|s| format!("{s:X}")).unwrap_or_default(),
                            display.received.elapsed().as_secs_f32()
                        ));
                        ui.collapsing("Raw response", |ui| {
                            ui.monospace(&r.raw);
                        });
                    });
                }
            });
        outgoing
    }
}

fn sparkline(ui: &mut egui::Ui, history: &[f64]) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(110.0, 24.0), egui::Sense::hover());
    if history.len() < 2 {
        return;
    }
    let lo = history.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = history.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (hi - lo).max(0.001);
    let points = history
        .iter()
        .enumerate()
        .map(|(i, v)| {
            egui::pos2(
                rect.left() + rect.width() * i as f32 / (history.len() - 1) as f32,
                rect.bottom() - rect.height() * ((v - lo) / span) as f32,
            )
        })
        .collect();
    ui.painter().add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5_f32, Color32::LIGHT_BLUE),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mercedes_sources_obey_transport_and_year_limits_and_preserve_toyota() {
        assert_eq!(mercedes_profiles::PROFILES.len(), 36);
        assert_eq!(profiles().len(), 111);
        assert_eq!(
            mercedes_profiles::PROFILES
                .iter()
                .filter(|(n, _)| source_count(n) > 0)
                .count(),
            19
        );
        for (name, text) in mercedes_profiles::PROFILES {
            for year in [2005, current_year()] {
                for experimental in [false, true] {
                    let (queries, _, _) = load_profile(text, year, experimental)
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                    assert!(
                        queries
                            .iter()
                            .all(|q| matches!(q.bytes[0], 1 | 0x21 | 0x22))
                    );
                }
            }
        }
        let text = mercedes_profiles::PROFILES
            .iter()
            .find(|(n, _)| *n == "Mercedes-Benz-E-Class")
            .unwrap()
            .1;
        let mut panel = Panel::default();
        for year in [2005, current_year()] {
            panel.workspace.year = year.to_string();
            panel.apply_workspace_profile("Mercedes-Benz E-Class".into(), text);
            assert!(panel.is_mercedes());
            assert!(!panel.toyota.is_empty());
        }
        let previous = panel.workspace.applied.clone();
        for year in [2004, current_year() + 1] {
            panel.workspace.year = year.to_string();
            panel.apply_workspace_profile("Mercedes-Benz E-Class".into(), text);
            assert_eq!(panel.workspace.applied, previous);
            assert!(panel.status.contains("Choose a Mercedes-Benz model year"));
        }
        panel.workspace.year = "2004".into();
        panel.apply_workspace_profile("Toyota Camry".into(), r#"{"commands":[]}"#);
        assert!(!panel.is_mercedes());
        assert_eq!(
            normalize_model("Mercedes-Benz E-Class"),
            normalize_model("Mercedes Benz E Class")
        );
    }
    #[test]
    fn mercedes_data_scaling_uses_original_definitions_and_checks_identifier() {
        let text = mercedes_profiles::PROFILES
            .iter()
            .find(|(n, _)| *n == "Mercedes-Benz-E-Class")
            .unwrap()
            .1;
        let (queries, _, _) = load_profile(text, 2020, false).unwrap();
        let radiator = queries
            .iter()
            .find(|q| q.bytes == [0x22, 0x20, 0x17])
            .unwrap();
        assert_eq!(radiator.target.unwrap().request, 0x7e0);
        let reply = DiagnosticResponse::new(Some(0x7e8), vec![0x62, 0x20, 0x17, 0, 80]);
        assert_eq!(
            decode(radiator, &reply, true).unwrap()[0].numeric,
            Some(99.5)
        );
        let wrong = DiagnosticResponse {
            payload: vec![0x62, 0x20, 0x18, 0, 80],
            ..reply
        };
        assert!(decode(radiator, &wrong, true).is_err());
        let transmission = queries.iter().find(|q| q.bytes == [0x22, 2, 0x34]).unwrap();
        let reply = DiagnosticResponse::new(Some(0x7e9), vec![0x62, 2, 0x34, 0, 0, 0, 100]);
        assert_eq!(
            decode(transmission, &reply, true).unwrap()[0].numeric,
            Some(50.0)
        );
    }
    #[test]
    fn uds_code_refresh_replaces_only_successful_ecu_results_and_rejects_stale_events() {
        let mut panel = Panel::default();
        let query = Query {
            bytes: vec![0x19, 2, 0xff],
            target: None,
            name: "UDS DTC scan".into(),
            signals: vec![],
            verified: false,
        };
        let job = panel.job(vec![query.clone()], None, false);
        let reply = DiagnosticResponse::new(Some(0x7e8), vec![0x59, 2, 0xff, 0x12, 0x34, 0x56, 9]);
        let rows = decode(&query, &reply, true).unwrap();
        panel.update(Update::ReplaceRows(
            job.id,
            reply.source,
            "1902FF".into(),
            rows.clone(),
        ));
        assert_eq!(panel.rows.len(), 2);
        let mut other = rows[1].clone();
        other.source = Some(0x7e9);
        panel.update(Update::Row(job.id, other));
        // Failed reads are ordinary status rows and retain the prior DTC.
        panel.update(Update::Row(
            job.id,
            row(
                &query,
                &reply,
                query.name.clone(),
                "Unavailable".into(),
                String::new(),
                None,
                "Timeout",
            ),
        ));
        assert_eq!(panel.rows.len(), 4);
        let empty = DiagnosticResponse {
            payload: vec![0x59, 2, 0xff],
            ..reply
        };
        panel.update(Update::ReplaceRows(
            job.id,
            empty.source,
            "1902FF".into(),
            decode(&query, &empty, true).unwrap(),
        ));
        assert_eq!(panel.rows.len(), 2);
        assert!(
            panel
                .rows
                .values()
                .any(|d| d.reading.source == Some(0x7e9) && d.reading.name == "DTC 123456")
        );
        panel.reset();
        panel.update(Update::ReplaceRows(
            job.id,
            Some(0x7e8),
            "1902FF".into(),
            rows,
        ));
        assert!(panel.rows.is_empty());
    }
    #[test]
    fn uds_raw_identifiers_status_masks_and_malformed_replies() {
        let query = Query {
            bytes: vec![0x19, 2, 0xff],
            target: None,
            name: "UDS".into(),
            signals: vec![],
            verified: false,
        };
        let response = |payload: &[u8]| DiagnosticResponse::new(Some(0x7e8), payload.to_vec());
        let rows = decode(
            &query,
            &response(&[0x59, 2, 9, 0x12, 0x34, 0x56, 0xff]),
            true,
        )
        .unwrap();
        assert_eq!(rows[1].value, "123456");
        assert!(rows[1].status.contains("Test failed | Confirmed"));
        assert!(!rows[1].status.contains("Pending"));
        assert_eq!(rows[1].numeric, None);
        assert!(
            decode(&query, &response(&[0x59, 2, 0]), true).unwrap()[0]
                .status
                .contains("no all-clear")
        );
        for bad in [
            &[0x59, 2][..],
            &[0x59, 2, 0xff, 0x12],
            &[0x59, 3, 0xff],
            &[0x7f, 0x19, 0x11],
        ] {
            assert!(decode(&query, &response(bad), true).is_err());
        }
    }
    #[test]
    fn mercedes_workflow_renders_and_uds_is_disabled_without_can_or_connection() {
        for size in [
            egui::vec2(800.0, 500.0),
            egui::vec2(1114.0, 650.0),
            egui::vec2(1400.0, 900.0),
        ] {
            let mut panel = Panel::default();
            panel.toyota_active = true;
            panel.workspace.year = "2020".into();
            let model = profiles()
                .iter()
                .position(|(name, _)| *name == "Mercedes-Benz-E-Class")
                .unwrap();
            panel.apply_workspace_profile(profile_name(model), profiles()[model].1);
            panel.select_workspace_system("7E0:7E8".into());
            let ctx = egui::Context::default();
            for page in [
                Page::Vehicle,
                Page::Systems,
                Page::Data,
                Page::Codes,
                Page::Utilities,
                Page::Information,
            ] {
                panel.workspace.page = page;
                for (connected, can) in [(false, true), (true, false)] {
                    let output = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                assert!(panel.show(ui, true, connected, can).is_none());
                            });
                        },
                    );
                    assert!(!output.shapes.is_empty());
                    if page == Page::Codes {
                        assert!(text_shapes(&output.shapes).contains("Read ECU DTCs (UDS)"));
                    }
                }
            }
        }
    }
    fn text_shapes(shapes: &[egui::epaint::ClippedShape]) -> String {
        fn add(shape: &egui::Shape, out: &mut String) {
            match shape {
                egui::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::Shape::Vec(v) => {
                    for s in v {
                        add(s, out)
                    }
                }
                _ => {}
            }
        }
        let mut out = String::new();
        for s in shapes {
            add(&s.shape, &mut out);
        }
        out
    }
    #[test]
    fn all_family_profiles_load_without_inventing_transport_support() {
        assert_eq!(toyota_profiles::PROFILES.len(), 75);
        for (name, text) in toyota_profiles::PROFILES {
            let (queries, _, _) =
                load_profile(text, 2020, true).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                queries
                    .iter()
                    .all(|q| matches!(q.bytes[0], 1 | 0x21 | 0x22))
            );
        }
        let text = r#"{"commands":[{"hdr":"750","rax":"758","eax":"2A","cmd":{"21":"85"},"signals":[]},{"hdr":"7E0","rax":"7E8","cmd":{"21":"01"},"signals":[]},{"hdr":"7E0","rax":"7E8","cmd":{"04":""},"signals":[]}]}"#;
        let (queries, _, omitted) = load_profile(text, 2020, false).unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(omitted, 2);
    }
    #[test]
    fn vehicle_system_changes_cancel_and_clear_previous_vehicle_values() {
        let mut panel = Panel::default();
        assert!(panel.workspace.model.is_none());
        assert!(panel.toyota.is_empty());
        panel.workspace.year = "2020".into();
        let index = toyota_profiles::PROFILES
            .iter()
            .position(|(n, _)| *n == "Toyota-Camry")
            .unwrap();
        panel.apply_workspace_profile(profile_name(index), toyota_profiles::PROFILES[index].1);
        assert!(!panel.toyota.is_empty());
        assert_eq!(panel.workspace.page, Page::Systems);
        let job = panel.job(vec![], None, false);
        panel.select_workspace_system("generic".into());
        assert!(job.cancelled.load(Ordering::Relaxed));
        assert!(panel.rows.is_empty());
        panel.workspace.confirm_clear = true;
        panel.service = 8;
        panel.toyota_active = false;
        let context = egui::Context::default();
        let _ = context.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(panel.show(ui, true, false, true).is_none());
            });
        });
        assert_eq!(panel.service, 1);
        assert!(!panel.workspace.confirm_clear);
        assert!(!panel.repeat);
        panel.update(Update::Row(
            job.id,
            Reading {
                source: None,
                request: "010C".into(),
                name: "Old vehicle".into(),
                value: "1".into(),
                unit: String::new(),
                raw: String::new(),
                numeric: Some(1.0),
                status: String::new(),
                signal_failure: false,
            },
        ));
        assert!(panel.rows.is_empty());
    }
    #[test]
    fn workflow_renders_small_and_large_without_corolla_default_or_broken_arrow() {
        for size in [
            egui::vec2(800.0, 500.0),
            egui::vec2(1114.0, 650.0),
            egui::vec2(1400.0, 900.0),
        ] {
            let mut panel = Panel::default();
            let ctx = egui::Context::default();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        assert!(panel.show(ui, true, false, true).is_none());
                    });
                },
            );
            let text = text_shapes(&output.shapes);
            assert!(text.contains("Vehicle selection"));
            assert!(!text.contains("2012"));
            assert!(!text.contains('→'));
            assert!(!text.contains("K-line engine address"));
            panel.workspace.year = "2020".into();
            let model = toyota_profiles::PROFILES
                .iter()
                .position(|(name, _)| *name == "Toyota-RAV4")
                .unwrap();
            panel.apply_workspace_profile(profile_name(model), toyota_profiles::PROFILES[model].1);
            for page in [
                Page::Systems,
                Page::Data,
                Page::Codes,
                Page::Utilities,
                Page::Information,
            ] {
                panel.workspace.system = Some("generic".into());
                panel.workspace.page = page;
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            assert!(panel.show(ui, true, false, true).is_none());
                        });
                    },
                );
            }
        }
    }
}
