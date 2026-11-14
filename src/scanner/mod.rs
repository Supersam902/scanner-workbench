//! Scanner Workbench: an independent application shell over the tested transports.
mod appearance;
mod codes;
#[doc(hidden)]
pub mod driver;
mod elm;
mod emissions_scan;
mod freeze_frames;
mod graphs;
mod live_presets;
mod live_reads;
pub mod model;
mod readiness;
mod recordings;
mod replay_graphs;
mod report_sections;
mod scan_comparison;
mod scan_history;
mod scan_review;
#[cfg(test)]
mod tests;
#[cfg(windows)]
#[doc(hidden)]
pub mod updates;
mod views;
pub mod worker;

use crate::adapter::{decode_hex, encode_hex};
use crate::diagnostics::{self, PidSupport, Query, Reading, Update};
use crate::j2534::{CanProtocol, DriverInfo, discover_drivers, same_driver_path};
pub use appearance::icon_data as workbench_icon;
use elm::AdapterKind;
use model::*;
use readiness::Period as ReadinessPeriod;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use worker::{
    AttemptOutcome, Command, CommandSender, ConnectionAttempt, Event, ReadMode, ReplyCheck,
    VehicleReply, VoltageAdapter, VoltageSample,
};

pub const VERSION: &str = env!("SCANNER_WORKBENCH_VERSION");
const NO_REPORTED_SELECTION: &str = "The vehicle did not report the selected PIDs. Try another preset or turn off Skip unreported PIDs.";

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Page {
    Home,
    Connection,
    VehicleInfo,
    Garage,
    Systems,
    Codes,
    Live,
    Emissions,
    FreezeFrame,
    MonitorTests,
    Recordings,
    ScanHistory,
    ScanReview,
    Reports,
    Help,
    Updates,
}
impl Page {
    const NAVIGATION: [Self; 7] = [
        Self::Home,
        Self::Codes,
        Self::Live,
        Self::Emissions,
        Self::VehicleInfo,
        Self::Recordings,
        Self::Connection,
    ];

    #[cfg(test)]
    const ALL: [Self; 16] = [
        Self::Home,
        Self::Codes,
        Self::Live,
        Self::Emissions,
        Self::FreezeFrame,
        Self::MonitorTests,
        Self::VehicleInfo,
        Self::Garage,
        Self::Systems,
        Self::Recordings,
        Self::ScanHistory,
        Self::ScanReview,
        Self::Reports,
        Self::Connection,
        Self::Help,
        Self::Updates,
    ];

    fn section(self) -> Self {
        match self {
            Self::FreezeFrame | Self::MonitorTests => Self::Emissions,
            Self::Garage | Self::Systems => Self::VehicleInfo,
            Self::Reports | Self::ScanHistory | Self::ScanReview => Self::Recordings,
            Self::Help | Self::Updates => Self::Connection,
            page => page,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Home => "Overview",
            Self::Codes => "Trouble codes",
            Self::Live => "Live data",
            Self::Emissions | Self::FreezeFrame | Self::MonitorTests => "Emissions",
            Self::VehicleInfo | Self::Garage | Self::Systems => "Vehicle",
            Self::Recordings | Self::Reports | Self::ScanHistory | Self::ScanReview => "Saved work",
            Self::Connection | Self::Help | Self::Updates => "Connection",
        }
    }

    fn tabs(self) -> &'static [(Self, &'static str)] {
        match self.section() {
            Self::Emissions => &[
                (Self::Emissions, "Readiness"),
                (Self::FreezeFrame, "Freeze frame"),
                (Self::MonitorTests, "Monitor tests"),
            ],
            Self::VehicleInfo => &[
                (Self::VehicleInfo, "Information"),
                (Self::Garage, "Garage"),
                (Self::Systems, "ECUs"),
            ],
            Self::Recordings => &[
                (Self::Recordings, "Recordings"),
                (Self::ScanHistory, "Scan history"),
                (Self::Reports, "Reports"),
            ],
            Self::Connection => &[
                (Self::Connection, "Adapter"),
                (Self::Help, "Manual requests"),
                (Self::Updates, "Updates"),
            ],
            _ => &[],
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PidAvailability {
    Reported,
    Unreported,
    Unchecked,
}

#[derive(Clone, Copy)]
enum StartupSplash {
    Hidden,
    Pending,
    VisibleUntil(Instant),
}

impl StartupSplash {
    fn visible(&mut self, now: Instant) -> bool {
        match *self {
            Self::Hidden => false,
            Self::Pending => {
                *self = Self::VisibleUntil(now + Duration::from_millis(850));
                true
            }
            Self::VisibleUntil(end) if now < end => true,
            Self::VisibleUntil(_) => {
                *self = Self::Hidden;
                false
            }
        }
    }
}

pub struct Workbench {
    tx: CommandSender,
    rx: mpsc::Receiver<Event>,
    root: PathBuf,
    settings: Settings,
    #[cfg(windows)]
    updates: Option<updates::Updates>,
    #[cfg(windows)]
    restarting_for_update: Option<updates::ReadyUpdate>,
    splash: StartupSplash,
    driver: Option<String>,
    driver_override: Option<DriverInfo>,
    driver_save_error: Option<String>,
    installed_drivers: Option<Vec<DriverInfo>>,
    elm_connection: Option<ElmConnection>,
    adapter_kind: AdapterKind,
    serial_port: String,
    serial_baud: String,
    serial_ports: Option<Vec<String>>,
    tcp_address: String,
    adapter_problem: Option<String>,
    protocol_override: Option<CanProtocol>,
    ecu_source: Option<u8>,
    vehicle: Vehicle,
    draft: Vehicle,
    observed_vin: Option<String>,
    vin_conflict: bool,
    vin_request: Option<Arc<AtomicBool>>,
    vin_copied_until: Option<Instant>,
    generic: &'static [Query],
    page: Page,
    connected: bool,
    connecting: bool,
    demo: bool,
    protocol: String,
    voltage: Option<VoltageSample>,
    connection_id: Option<u64>,
    vehicle_reply: Option<VehicleReply>,
    vehicle_reply_check: Option<(Instant, ReplyCheck)>,
    connection_attempts: Vec<ConnectionAttempt>,
    connection_error: Option<String>,
    adapter_connect: Option<u64>,
    epoch: u64,
    cancel: Arc<AtomicBool>,
    busy: bool,
    live: bool,
    poll_in_flight: bool,
    next_live: Instant,
    status: String,
    progress: (usize, usize, String),
    logs: VecDeque<String>,
    results: Results,
    scanning_emissions: bool,
    latest_scan: Option<Arc<Snapshot>>,
    before_clear_scan: Option<(PathBuf, Arc<Snapshot>)>,
    clear_baseline: Option<PathBuf>,
    scan_history: scan_history::History,
    scan_review: Option<scan_review::Review>,
    scan_recovery_dir: String,
    monitor_support: Option<PidSupport>,
    oxygen_support: worker::OxygenCoverage,
    vehicle_info_support: Option<PidSupport>,
    pid_support: PidSupport,
    selected: BTreeSet<String>,
    live_preset_dialog: Option<live_presets::Dialog>,
    search: String,
    favorites_only: bool,
    skip_unreported: bool,
    group_live: bool,
    live_grouping_fell_back: bool,
    graphs: bool,
    graph_signals: Option<Vec<String>>,
    graph_window: graphs::Window,
    details: bool,
    interval: f32,
    base_ms: u64,
    clock: Instant,
    recording: Option<Capture>,
    marker_note: String,
    unsaved: Option<Capture>,
    recording_rows: usize,
    playback: Option<replay_graphs::ReplayCapture>,
    replay_at: u64,
    replay_index: usize,
    replay_playing: bool,
    replay_tick: Instant,
    #[cfg(not(windows))]
    capture_path: String,
    recording_preview: Option<recordings::Preview>,
    recording_preview_error: Option<(PathBuf, String)>,
    recording_search: String,
    recording_list_offset: usize,
    show_recording_preview: bool,
    playback_path: Option<PathBuf>,
    baseline: Option<Snapshot>,
    saved_recordings: Vec<recordings::SavedRecording>,
    last_export: Option<PathBuf>,
    confirm_clear: bool,
    clear_ready: bool,
    clearing: bool,
    readiness_period: ReadinessPeriod,
    code_category: Option<codes::Category>,
    show_unsupported_monitors: bool,
    frame: u8,
    frame_ecu: Option<u32>,
    monitor: u8,
    oxygen_sensor: u8,
    oxygen_test: u8,
    manual: String,
    lookup: String,
    db: crate::dtc_database::DtcDatabase,
}

fn generic_catalog() -> &'static [Query] {
    static CATALOG: std::sync::OnceLock<Vec<Query>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        diagnostics::catalog(include_str!("../../data/obdb/saej1979.json"), None)
            .expect("bundled SAE catalog")
    })
}

impl Workbench {
    /// An offline workbench for the preview and tests.
    pub fn new(root: PathBuf) -> Self {
        Self::with_worker(root, |_, _| worker::spawn_with_driver(None, Vec::new))
    }

    /// The executable supplied here handles the private adapter-worker entry point.
    pub fn with_native_driver(root: PathBuf, executable: Option<PathBuf>) -> Self {
        let app = Self::with_worker(root, |root, settings| {
            worker::spawn(
                executable,
                root,
                settings
                    .preferred_driver
                    .as_ref()
                    .map(|driver| driver.path.clone()),
                settings
                    .elm_connection
                    .clone()
                    .filter(|connection| connection.validate().is_ok()),
            )
        });
        #[cfg(windows)]
        let app = {
            let mut app = app;
            if updates::Updates::available() {
                let mut updates = updates::Updates::new();
                updates.check();
                app.updates = Some(updates);
            }
            app
        };
        app
    }

    pub fn show_startup_splash(&mut self) {
        self.splash = StartupSplash::Pending;
    }

    fn with_worker(
        root: PathBuf,
        start_worker: impl FnOnce(&Path, &Settings) -> (CommandSender, mpsc::Receiver<Event>),
    ) -> Self {
        let (settings, notice) = match Settings::load(&root) {
            Ok(s) => (s, String::new()),
            Err(e) => (
                Settings::default(),
                format!("Settings could not be loaded: {e}"),
            ),
        };
        let driver_override = settings.preferred_driver.clone();
        let (tx, rx) = start_worker(&root, &settings);
        let elm_connection = settings
            .elm_connection
            .clone()
            .filter(|connection| connection.validate().is_ok());
        let adapter_kind = elm_connection
            .as_ref()
            .map_or(AdapterKind::J2534, ElmConnection::kind);
        let mut serial_port = String::new();
        let mut serial_baud = String::new();
        let mut tcp_address = String::new();
        match &settings.elm_connection {
            Some(ElmConnection::Serial { port, baud }) => {
                serial_port = port.clone();
                if let Some(baud) = baud {
                    serial_baud = baud.to_string();
                }
            }
            Some(ElmConnection::Tcp(address)) => tcp_address = address.clone(),
            None => {}
        }
        let draft = settings.last_vehicle.clone().unwrap_or_default();
        let db = crate::dtc_database::find_database_path()
            .map(|p| crate::dtc_database::DtcDatabase::load(&p))
            .unwrap_or_default();
        let mut app = Self {
            tx,
            rx,
            root,
            settings,
            #[cfg(windows)]
            updates: None,
            #[cfg(windows)]
            restarting_for_update: None,
            splash: StartupSplash::Hidden,
            driver: None,
            driver_override,
            driver_save_error: None,
            installed_drivers: None,
            elm_connection,
            adapter_kind,
            serial_port,
            serial_baud,
            serial_ports: None,
            tcp_address,
            adapter_problem: None,
            protocol_override: None,
            ecu_source: None,
            draft,
            vehicle: Vehicle::default(),
            observed_vin: None,
            vin_conflict: false,
            vin_request: None,
            vin_copied_until: None,
            generic: generic_catalog(),
            page: Page::Home,
            connected: false,
            connecting: false,
            demo: false,
            protocol: String::new(),
            voltage: None,
            connection_id: None,
            vehicle_reply: None,
            vehicle_reply_check: None,
            connection_attempts: Vec::new(),
            connection_error: None,
            adapter_connect: None,
            epoch: 1,
            cancel: Arc::new(AtomicBool::new(false)),
            busy: false,
            live: false,
            poll_in_flight: false,
            next_live: Instant::now(),
            status: if notice.is_empty() {
                "Ready to connect.".into()
            } else {
                notice
            },
            progress: (0, 0, String::new()),
            logs: VecDeque::new(),
            results: Results::default(),
            scanning_emissions: false,
            latest_scan: None,
            before_clear_scan: None,
            clear_baseline: None,
            scan_history: Default::default(),
            scan_review: None,
            scan_recovery_dir: String::new(),
            monitor_support: None,
            oxygen_support: Default::default(),
            vehicle_info_support: None,
            pid_support: PidSupport::default(),
            selected: BTreeSet::new(),
            live_preset_dialog: None,
            search: String::new(),
            favorites_only: false,
            skip_unreported: true,
            group_live: true,
            live_grouping_fell_back: false,
            graphs: false,
            graph_signals: None,
            graph_window: graphs::Window::Minute,
            details: false,
            interval: 1.0,
            base_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
            clock: Instant::now(),
            recording: None,
            marker_note: String::new(),
            unsaved: None,
            recording_rows: 0,
            playback: None,
            replay_at: 0,
            replay_index: 0,
            replay_playing: false,
            replay_tick: Instant::now(),
            #[cfg(not(windows))]
            capture_path: String::new(),
            recording_preview: None,
            recording_preview_error: None,
            recording_search: String::new(),
            recording_list_offset: 0,
            show_recording_preview: false,
            playback_path: None,
            baseline: None,
            saved_recordings: vec![],
            last_export: None,
            confirm_clear: false,
            clear_ready: false,
            clearing: false,
            readiness_period: ReadinessPeriod::SinceClear,
            code_category: None,
            show_unsupported_monitors: false,
            frame: 0,
            frame_ecu: None,
            monitor: 1,
            oxygen_sensor: 1,
            oxygen_test: 5,
            manual: "0902".into(),
            lookup: "P0133".into(),
            db,
        };
        app.preset("Engine");
        app.refresh_files();
        app
    }
    fn now(&self) -> u64 {
        self.sample_time(Instant::now())
    }
    fn sample_time(&self, at: Instant) -> u64 {
        self.base_ms + at.saturating_duration_since(self.clock).as_millis() as u64
    }
    fn view_time(&self) -> u64 {
        if self.playback.is_some() {
            self.replay_at
        } else {
            self.now()
        }
    }
    fn log(&mut self, text: String) {
        let line = format!("{}  {text}", chrono::Local::now().format("%H:%M:%S"));
        if text.starts_with("Request ") || text.starts_with("ECU ") {
            tracing::debug!("{line}");
        } else {
            tracing::info!("{line}");
        }
        self.logs.push_back(line);
        if self.logs.len() > 600 {
            self.logs.pop_front();
        }
    }
    fn edit_settings(&mut self, edit: impl FnOnce(&mut Settings)) -> Result<(), String> {
        self.settings = Settings::edit(&self.root, edit)?;
        Ok(())
    }
    fn refresh_files(&mut self) {
        self.saved_recordings = recordings::saved_recordings(&self.root.join("recordings"));
        self.recording_list_offset = 0;
    }
    fn cancel_operation(&mut self) {
        if self.adapter_connect.take().is_some() {
            self.connection_error = Some("Connection cancelled.".into());
        }
        let _ = self.finish_emissions_scan(true);
        self.stop_live();
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(cancel_vin) = self.vin_request.take() {
            cancel_vin.store(true, Ordering::Relaxed);
        }
        self.vin_copied_until = None;
        self.epoch += 1;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.busy = false;
        self.poll_in_flight = false;
        self.connecting = false;
        self.confirm_clear = false;
        self.clearing = false;
        self.progress = (0, 0, String::new());
    }
    fn stop_operation(&mut self) {
        let was_live = self.live;
        self.stop_live();
        if self.busy {
            self.cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping diagnostics…".into();
        } else if was_live {
            self.status = "Live data stopped.".into();
        }
    }
    fn start_live(&mut self) {
        if !self.live {
            self.break_graphs();
            self.live = true;
            self.next_live = Instant::now();
            self.status = "Polling live data.".into();
        }
    }
    fn stop_live(&mut self) {
        if self.live {
            self.live = false;
            if self.poll_in_flight {
                self.cancel.store(true, Ordering::Relaxed);
            }
            self.break_graphs();
        }
    }
    fn break_graphs(&mut self) {
        self.apply_sample(Sample {
            at_ms: self.now(),
            graph_break: true,
            ..Default::default()
        });
    }
    fn send(&mut self, command: Command) -> bool {
        match self.tx.try_send(command) {
            Ok(()) => true,
            Err(e) => {
                self.busy = false;
                self.connecting = false;
                self.stop_live();
                self.poll_in_flight = false;
                self.status = format!("Worker unavailable: {e}");
                false
            }
        }
    }
    fn disconnect(&mut self) {
        self.stop_live();
        self.cancel_operation();
        self.finish_recording();
        self.connected = false;
        self.driver = None;
        self.protocol.clear();
        self.connection_id = None;
        self.vehicle_reply = None;
        self.busy = true;
        if self.unsaved.is_none() {
            self.status = "Closing the adapter…".into();
        }
        self.send(Command::Disconnect { id: self.epoch });
    }
    fn reset_results(&mut self) {
        self.results = Results::default();
        self.frame_ecu = None;
        self.latest_scan = None;
        self.before_clear_scan = None;
        self.clear_baseline = None;
        self.graph_signals = None;
        self.monitor_support = None;
        self.oxygen_support = Default::default();
        self.vehicle_info_support = None;
        self.vin_conflict = false;
        self.vin_copied_until = None;
        self.pid_support = PidSupport::default();
        self.baseline = None;
        self.playback = None;
        self.playback_path = None;
        self.replay_playing = false;
    }

    fn select_driver(&mut self, driver: Option<DriverInfo>) {
        if self.busy || self.connected && !self.demo || self.driver_override == driver {
            return;
        }
        let path = driver.as_ref().map(|driver| driver.path.clone());
        match self.tx.try_send(Command::SelectDriver(path)) {
            Ok(()) => {
                self.driver_override = driver;
                self.voltage = None;
                self.connection_attempts.clear();
                self.connection_error = None;
                self.save_driver_preference();
            }
            Err(error) => self.status = format!("Could not change the driver. {error}"),
        }
    }

    fn save_driver_preference(&mut self) {
        let driver = self.driver_override.clone();
        self.status = match self.edit_settings(|settings| settings.preferred_driver = driver) {
            Ok(()) => {
                self.driver_save_error = None;
                "Driver preference saved.".into()
            }
            Err(error) => {
                self.driver_save_error = Some(error);
                "The driver choice is active for this session. Could not save the preference."
                    .into()
            }
        };
    }

    fn apply_adapter_choice(&mut self) -> bool {
        let connection = match self.elm_connection_draft() {
            Ok(connection) => connection,
            Err(error) => {
                self.adapter_problem = Some(error.into());
                self.status = error.into();
                return false;
            }
        };
        if connection == self.elm_connection && connection == self.settings.elm_connection {
            self.adapter_problem = None;
            return true;
        }
        if self.elm_connection != connection {
            if let Err(error) = self.tx.try_send(Command::SelectElm(connection.clone())) {
                self.adapter_problem = Some(format!("Could not change adapters. {error}"));
                return false;
            }
            self.voltage = None;
            self.connection_attempts.clear();
            self.connection_error = None;
            self.elm_connection = connection.clone();
        }
        self.adapter_problem = self
            .edit_settings(|settings| settings.elm_connection = connection)
            .err()
            .map(|error| {
                format!("Adapter choice is active for this session. Could not save it. {error}")
            });
        self.status = self
            .adapter_problem
            .clone()
            .unwrap_or_else(|| "Adapter choice saved.".into());
        true
    }

    fn elm_connection_draft(&self) -> Result<Option<ElmConnection>, &'static str> {
        let connection = match self.adapter_kind {
            AdapterKind::J2534 => return Ok(None),
            AdapterKind::Serial => {
                let baud = self.serial_baud.trim();
                let baud = (!baud.is_empty())
                    .then(|| baud.parse::<u32>())
                    .transpose()
                    .map_err(
                        |_| "Enter a baud rate or leave the field blank for automatic detection.",
                    )?;
                ElmConnection::Serial {
                    port: self.serial_port.trim().into(),
                    baud,
                }
            }
            AdapterKind::Tcp => ElmConnection::Tcp(self.tcp_address.trim().into()),
        };
        connection.validate()?;
        Ok(Some(connection))
    }

    fn connect(&mut self) {
        if self.busy || self.connected && !self.demo {
            return;
        }
        self.finish_recording();
        if self.unsaved.is_some() {
            self.status = "Save the pending recording before changing connection.".into();
            return;
        }
        if !self.apply_adapter_choice() {
            self.page = Page::Connection;
            return;
        }
        self.cancel_operation();
        self.connected = false;
        self.driver = None;
        self.protocol.clear();
        self.connection_id = None;
        self.vehicle_reply = None;
        self.vehicle_reply_check = None;
        self.connecting = true;
        self.busy = true;
        self.connection_attempts.clear();
        self.connection_error = None;
        self.adapter_connect = Some(self.epoch);
        let command = if let Some(connection) = &self.elm_connection {
            self.status = format!("Connecting through {}…", connection.label());
            Command::ConnectElm {
                id: self.epoch,
                connection: connection.clone(),
                cancel: self.cancel.clone(),
            }
        } else {
            self.status = self.driver_override.as_ref().map_or_else(
                || "Finding the active J2534 adapter…".into(),
                |driver| format!("Connecting through {}…", driver.name),
            );
            Command::Connect {
                id: self.epoch,
                protocol: self.protocol_override,
                ecu: self
                    .ecu_source
                    .filter(|_| self.protocol_override.is_none_or(CanProtocol::extended)),
                cancel: self.cancel.clone(),
            }
        };
        if !self.send(command) {
            self.adapter_connect = None;
            self.connection_error = Some(self.status.clone());
        }
    }
    pub fn start_demo(&mut self) {
        if self.busy || self.connected {
            return;
        }
        self.finish_recording();
        if self.unsaved.is_some() {
            self.status = "Save the pending recording before entering practice mode.".into();
            return;
        }
        self.cancel_operation();
        self.reset_results();
        self.connected = false;
        self.demo = true;
        self.vehicle_reply = None;
        self.vehicle_reply_check = None;
        self.observed_vin = None;
        self.vehicle = Vehicle {
            model: "Practice vehicle".into(),
            vin: "WDD00000000000000".into(),
            notes: "Offline practice session".into(),
            ..Default::default()
        };
        self.busy = true;
        self.connecting = true;
        self.status = "Starting the local simulator...".into();
        self.send(Command::Demo { id: self.epoch });
    }
    fn apply_vehicle(&mut self) {
        if !self.connected || self.busy || self.demo {
            self.status = "Connect a vehicle before applying its details.".into();
            return;
        }
        if let Err(e) = self.draft.validate() {
            self.status = e;
            return;
        }
        if let Some(vin) = &self.observed_vin {
            if !self.draft.vin.trim().is_empty() && !self.draft.vin.trim().eq_ignore_ascii_case(vin)
            {
                self.status = format!(
                    "This vehicle reported VIN {vin}. Choose matching details in the garage."
                );
                return;
            }
            self.draft.vin = vin.clone();
        }
        self.vehicle = self.draft.clone();
        let vehicle = self.vehicle.clone();
        self.status = match self.edit_settings(|settings| settings.last_vehicle = Some(vehicle)) {
            Ok(()) => "Vehicle details applied to this session.".into(),
            Err(error) => format!(
                "Vehicle details applied for this session. Could not save preferences. {error}"
            ),
        };
    }
    fn save_garage(&mut self) {
        if let Err(e) = self.draft.validate() {
            self.status = e;
            return;
        }
        let vehicle = self.draft.clone();
        let identity = vehicle.identity();
        self.status = match self.edit_settings(|settings| {
            if let Some(saved) = settings
                .garage
                .iter_mut()
                .find(|v| v.identity() == identity)
            {
                *saved = vehicle;
            } else {
                settings.garage.push(vehicle);
            }
        }) {
            Ok(()) => "Vehicle saved to your garage.".into(),
            Err(error) => format!("Could not save vehicle. {error}"),
        };
    }
    fn base_query(&self, mode: u8, pid: Option<u8>) -> Query {
        let mut bytes = vec![mode];
        if let Some(pid) = pid {
            bytes.push(pid);
        }
        Query {
            bytes,
            target: None,
            name: match (mode, pid) {
                (1, Some(1)) => "Readiness since clear",
                (1, Some(0x41)) => "Readiness this drive cycle",
                (3, _) => "Stored emissions codes",
                (7, _) => "Pending emissions codes",
                (10, _) => "Permanent emissions codes",
                (4, _) => "Clear emissions codes",
                (9, Some(2)) => "VIN",
                (9, _) => "Vehicle identity",
                _ => "Diagnostic read",
            }
            .into(),
            signals: vec![],
            verified: false,
        }
    }
    fn dispatch(&mut self, queries: Vec<Query>, discovery: Option<(u8, u8)>, mode: ReadMode) {
        if !self.connected || self.busy || self.playback.is_some() {
            self.status =
                "Connect a vehicle or practice session and wait for the current operation.".into();
            return;
        }
        if queries.len() > 512 {
            self.status = "Choose no more than 512 request groups per operation.".into();
            self.stop_live();
            return;
        }
        let polling = mode.is_poll();
        if !polling {
            self.stop_live();
            self.confirm_clear = false;
            self.progress = (0, queries.len(), String::new());
        }
        self.epoch += 1;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.busy = !polling;
        self.poll_in_flight = polling;
        if let ReadMode::FreezeFrame(frame) = mode {
            self.progress.1 = 0;
            self.status = format!("Reading freeze frame {frame}…");
        } else if matches!(mode, ReadMode::MonitorSweep) {
            self.status = "Finding supported monitors…".into();
        } else if matches!(mode, ReadMode::OxygenSweep | ReadMode::OxygenTest { .. }) {
            self.status = "Reading stored oxygen-sensor tests…".into();
        } else if matches!(mode, ReadMode::VehicleInformation) {
            self.status = "Finding supported vehicle information…".into();
        } else if !polling {
            self.status = "Reading vehicle data…".into();
        }
        let clear_requested =
            matches!(mode, ReadMode::Control) && queries.iter().any(|q| q.bytes == [4]);
        let sent = self.send(Command::Read {
            id: self.epoch,
            queries,
            discovery,
            mode,
            cancel: self.cancel.clone(),
        });
        self.clearing = sent && clear_requested;
        if self.clearing {
            self.clear_baseline = self.before_clear_scan.take().map(|(path, _)| path);
            self.apply_sample(Sample {
                at_ms: self.now(),
                clear_requested: true,
                ..Default::default()
            });
        }
    }
    fn once(&mut self, q: Vec<Query>) {
        self.dispatch(q, None, ReadMode::Diagnostic);
    }
    fn can_check_vehicle_reply(&self) -> bool {
        self.connected
            && self.connection_id.is_some()
            && !self.demo
            && !self.busy
            && !self.live
            && self.playback.is_none()
    }
    fn check_vehicle_reply(&mut self) {
        if self.can_check_vehicle_reply() {
            self.dispatch(vec![], None, ReadMode::CheckVehicleReply);
            if self.busy {
                self.status = "Checking for a vehicle reply…".into();
            }
        }
    }
    fn read_live(&mut self, queries: Vec<Query>, continuous: bool) {
        let group = self.group_live;
        let mode = if continuous {
            ReadMode::Poll { group }
        } else {
            ReadMode::Live { group }
        };
        self.dispatch(queries, None, mode);
    }
    fn read_freeze_frame(&mut self) {
        self.dispatch(vec![], None, ReadMode::FreezeFrame(self.frame));
    }
    fn read_supported_monitors(&mut self) {
        self.dispatch(vec![], None, ReadMode::MonitorSweep);
    }
    fn read_oxygen_tests(&mut self) {
        self.dispatch(vec![], None, ReadMode::OxygenSweep);
    }
    fn read_oxygen_test(&mut self) {
        self.dispatch(
            vec![],
            None,
            ReadMode::OxygenTest {
                sensor: self.oxygen_sensor,
                test: self.oxygen_test,
            },
        );
    }
    fn read_vehicle_information(&mut self) {
        self.dispatch(vec![], None, ReadMode::VehicleInformation);
    }
    fn refresh_pid_support(&mut self) {
        self.dispatch(vec![], Some((1, 0)), ReadMode::Diagnostic);
    }
    fn code_queries(&self) -> Vec<Query> {
        vec![
            self.base_query(3, None),
            self.base_query(7, None),
            self.base_query(10, None),
        ]
    }
    fn read_codes(&mut self) {
        self.dispatch(self.code_queries(), None, ReadMode::Scan);
    }
    fn read_emissions_scan(&mut self) {
        self.capture_emissions_scan(None);
    }
    fn capture_emissions_scan(&mut self, stage: Option<ClearScanStage>) {
        if !self.connected || self.busy || self.playback.is_some() {
            return;
        }
        if stage == Some(ClearScanStage::Before) {
            self.before_clear_scan = None;
        }
        let mut queries = vec![self.base_query(1, Some(1)), self.base_query(1, Some(0x41))];
        queries.extend(self.code_queries());
        queries.push(self.base_query(9, Some(2)));
        self.dispatch(queries, None, ReadMode::EmissionsScan);
        if self.busy {
            self.latest_scan = None;
            self.scanning_emissions = true;
            self.status = "Scanning emissions…".into();
            self.apply_sample(Sample {
                at_ms: self.now(),
                emissions_scan: Some(ScanEvent::Started),
                clear_scan_stage: stage,
                ..Default::default()
            });
        }
    }
    fn can_export_report(&self) -> bool {
        !self.busy
            && (!self.results.rows.is_empty()
                || !self.results.failures.is_empty()
                || self.results.emissions_scan.is_some())
    }
    fn finish_emissions_scan(&mut self, interrupted: bool) -> Option<String> {
        if !std::mem::take(&mut self.scanning_emissions) {
            return None;
        }
        self.apply_sample(Sample {
            at_ms: self.now(),
            emissions_scan: Some(if interrupted {
                ScanEvent::Interrupted
            } else {
                ScanEvent::Finished
            }),
            ..Default::default()
        });
        let snapshot = Snapshot::from_emissions_scan(&self.vehicle, self.demo, &self.results)?;
        let stage = snapshot.emissions_scan.as_ref()?.clear_stage;
        let snapshot = Arc::new(snapshot);
        self.latest_scan = Some(snapshot.clone());
        let directory = self.root.join("scans");
        let path = self.scan_history.archive(&directory, snapshot.clone());
        if let Some(error) = &self.scan_history.save_error {
            self.log(format!("Could not save the emissions scan. {error}"));
        }
        if interrupted {
            return match (stage, path) {
                (Some(_), Some(_)) => Some("Scan interrupted. Partial results saved.".into()),
                (Some(_), None) => {
                    Some("Scan interrupted. Open Scan history to retry saving.".into())
                }
                _ => None,
            };
        }
        match (stage, path) {
            (Some(ClearScanStage::Before), Some(path)) => {
                self.before_clear_scan = Some((path, snapshot));
                Some(
                    "Before-clear scan saved. Review it or open Clear emissions codes again."
                        .into(),
                )
            }
            (Some(ClearScanStage::After), Some(path)) => {
                let result = self.scan_history.select(path).and_then(|()| {
                    if let Some(before) = &self.clear_baseline {
                        self.scan_history.compare_with(before.clone())
                    } else {
                        Ok(())
                    }
                });
                self.page = Page::ScanHistory;
                Some(match result {
                    Ok(()) if self.clear_baseline.is_some() => {
                        "Follow-up scan saved. Before / after comparison ready.".into()
                    }
                    Ok(()) => "Follow-up scan saved. Review the results in Scan history.".into(),
                    Err(error) => format!("Follow-up scan saved. {error}"),
                })
            }
            (Some(_), None) => Some("Scan could not be saved. Open Scan history to retry.".into()),
            _ => None,
        }
    }

    fn prepare_to_close(&mut self, ctx: &egui::Context) {
        if self.scanning_emissions {
            self.cancel.store(true, Ordering::Relaxed);
            let _ = self.finish_emissions_scan(true);
        }
        self.scan_history.retry_saves();
        self.finish_recording();
        let (page, message) = if self.unsaved.is_some() {
            (
                Page::Recordings,
                "Recording could not be saved. Retry or choose a writable location before closing.",
            )
        } else if self.scan_history.pending_count() > 0 {
            (
                Page::ScanHistory,
                "Scan history could not be saved. Retry or save the pending scans to another location before closing.",
            )
        } else {
            return;
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.page = page;
        self.status = message.into();
    }
    fn read_readiness(&mut self) {
        self.read_readiness_period(ReadinessPeriod::SinceClear);
    }
    fn read_readiness_period(&mut self, period: ReadinessPeriod) {
        let mut queries = vec![self.base_query(1, Some(1))];
        if period == ReadinessPeriod::DriveCycle {
            queries.push(self.base_query(1, Some(0x41)));
        }
        self.dispatch(queries, None, ReadMode::Scan);
    }
    fn recheck_after_clear(&mut self) {
        let mut queries = vec![self.base_query(1, Some(1))];
        queries.extend(self.code_queries());
        self.dispatch(queries, None, ReadMode::Scan);
    }
    fn capture_follow_up(&mut self) {
        if self.results.clear_attempt_ms().is_some() {
            self.capture_emissions_scan(Some(ClearScanStage::After));
        }
    }
    fn confirm_emissions_clear(&mut self) {
        if self.confirm_clear && self.clear_ready && self.connected && !self.busy {
            self.dispatch(vec![self.base_query(4, None)], None, ReadMode::Control);
        }
    }
    fn pid_availability(&self, q: &Query) -> PidAvailability {
        match (q.bytes.as_slice(), q.target) {
            ([1, pid], None) if self.pid_support.reported.contains(pid) => {
                PidAvailability::Reported
            }
            ([1, pid], None) if *pid % 32 != 0 && *pid <= self.pid_support.checked_through => {
                PidAvailability::Unreported
            }
            _ => PidAvailability::Unchecked,
        }
    }
    fn selected_live_queries(&self) -> impl Iterator<Item = &Query> {
        self.generic.iter().filter(|q| {
            self.selected.contains(&query_key(q))
                && (!self.skip_unreported
                    || self.pid_availability(q) != PidAvailability::Unreported)
        })
    }
    fn selected_queries(&self) -> Vec<Query> {
        let mut queries: Vec<_> = self.selected_live_queries().cloned().collect();
        queries.sort_by_key(|q| self.pid_availability(q) != PidAvailability::Reported);
        queries
    }
    fn apply_sample(&mut self, mut sample: Sample) {
        let received_at = sample.at_ms;
        sample.at_ms = self.results.next_sample_ms(sample.at_ms);
        if !self.live {
            for r in sample.rows.iter().filter(|r| {
                matches!(r.request.as_str(), "03" | "07" | "0A")
                    || diagnostics::oxygen_monitor::request_parts(&r.request).is_some()
                    || r.request.starts_with("0202")
                    || r.request == "0101"
                        && (unavailable(r)
                            || matches!(
                                r.name.as_str(),
                                "Malfunction indicator lamp" | "Number of DTCs stored in this ECU"
                            )
                            || r.value == "Not ready")
                    || r.request == "0141"
                        && (unavailable(r)
                            || matches!(r.value.as_str(), "Not ready" | "Disabled or unsupported"))
            }) {
                tracing::info!(
                    practice = self.demo,
                    request = %r.request,
                    ecu = %ecu_label(r.source),
                    item = %r.name,
                    value = %r.value,
                    status = %r.status,
                    "Scan result"
                );
            }
        }
        let read_vin = sample.rows.iter().any(|r| r.request == "0902");
        self.results.apply(&sample);
        if read_vin {
            self.update_session_vin();
        }
        if let Some(capture) = self.recording.as_mut() {
            if received_at < capture.started_ms {
                return;
            }
            if self.recording_rows + sample.rows.len() > MAX_CAPTURE_ROWS
                || capture.samples.len() >= MAX_CAPTURE_ROWS
            {
                self.status =
                    "Recording reached 50,000 readings and was saved automatically.".into();
                self.finish_recording();
            } else {
                self.recording_rows += sample.rows.len();
                capture.samples.push(sample);
            }
        }
    }
    fn update_session_vin(&mut self) {
        let (vin, conflict) = self.results.reported_vin();
        if conflict {
            self.observed_vin = None;
            self.vehicle.vin.clear();
            self.vin_copied_until = None;
            self.baseline = None;
            if !self.vin_conflict {
                self.log(
                    "ECUs reported different VINs. Review Vehicle Info before using a VIN.".into(),
                );
            }
        } else if let Some(vin) = vin {
            if !self.vehicle.vin.eq_ignore_ascii_case(&vin) {
                self.vin_copied_until = None;
            }
            if !self.demo {
                self.observed_vin = Some(vin.clone());
                if valid_vin(&self.vehicle.vin)
                    && !self.vehicle.vin.trim().eq_ignore_ascii_case(&vin)
                {
                    self.vehicle = Vehicle::default();
                    self.baseline = None;
                    self.log("The vehicle VIN differs from the applied garage entry. Its details were detached.".into());
                }
            }
            self.vehicle.vin = vin;
        }
        self.vin_conflict = conflict;
    }
    fn tick(&mut self) {
        #[cfg(windows)]
        if let Some(updates) = &mut self.updates {
            updates.tick();
        }
        let events: Vec<_> = self.rx.try_iter().collect();
        for event in events {
            match event {
                Event::ConnectProgress(id, text)
                    if self.adapter_connect == Some(id) && !self.cancel.load(Ordering::Relaxed) =>
                {
                    self.status = text;
                }
                Event::ConnectAttempt(id, attempt) if self.adapter_connect == Some(id) => {
                    self.connection_attempts.push(attempt);
                }
                Event::Connected {
                    id,
                    driver,
                    protocol,
                    demo,
                    pid_support,
                } if id == self.epoch => {
                    if self.cancel.load(Ordering::Relaxed) {
                        self.disconnect();
                        continue;
                    }
                    self.adapter_connect = None;
                    if !demo {
                        self.reset_results();
                        self.vehicle = Vehicle::default();
                        self.observed_vin = None;
                    }
                    self.connected = true;
                    self.connecting = false;
                    self.busy = false;
                    self.demo = demo;
                    self.driver = driver;
                    self.protocol = protocol;
                    self.pid_support = pid_support;
                    self.live_grouping_fell_back = false;
                    self.connection_id = (!demo).then_some(id);
                    self.vehicle_reply_check = None;
                    self.vehicle_reply = (!demo).then(|| VehicleReply {
                        last_reply: Instant::now(),
                        missed: false,
                    });
                    self.status = if demo {
                        "Practice mode. Diagnostic results are simulated."
                    } else {
                        "Vehicle connected. Read codes, open live data, or check readiness."
                    }
                    .into();
                    if !demo {
                        let cancel = Arc::new(AtomicBool::new(false));
                        if self
                            .tx
                            .try_send(Command::Read {
                                id,
                                queries: vec![],
                                discovery: None,
                                mode: ReadMode::Identify,
                                cancel: cancel.clone(),
                            })
                            .is_ok()
                        {
                            self.vin_request = Some(cancel);
                        }
                    }
                    self.log(self.status.clone());
                }
                Event::Voltage(value) => {
                    if value.as_ref().is_some_and(|sample| match &sample.adapter {
                        VoltageAdapter::J2534(driver) => {
                            self.elm_connection.is_some()
                                || self.driver_override.as_ref().is_some_and(|selected| {
                                    !same_driver_path(&driver.path, &selected.path)
                                })
                        }
                        VoltageAdapter::Elm(connection) => {
                            self.elm_connection.as_ref() != Some(connection)
                        }
                    }) {
                        continue;
                    }
                    self.voltage = value;
                }
                Event::VehicleReply(id, reply)
                    if self.connection_id == Some(id)
                        && self.connected
                        && !self.demo
                        && self.playback.is_none() =>
                {
                    let missed = self.vehicle_reply.is_some_and(|last| last.missed);
                    if missed != reply.missed {
                        self.log(
                            if reply.missed {
                                "No vehicle reply to an expected request. The adapter is still connected."
                            } else {
                                "Vehicle replies resumed."
                            }
                            .into(),
                        );
                    }
                    self.vehicle_reply = Some(reply);
                }
                Event::VehicleReplyChecked {
                    connection,
                    operation,
                    check,
                } if self.connection_id == Some(connection)
                    && operation == self.epoch
                    && !self.cancel.load(Ordering::Relaxed) =>
                {
                    self.vehicle_reply_check = Some((Instant::now(), check));
                }
                Event::LinkLost(id, reason) if self.connection_id == Some(id) => {
                    self.voltage = None;
                    let message = format!("{reason}. Click Connect to try again.");
                    self.connection_error = Some(message.clone());
                    self.connection_failed(message);
                }
                Event::Disconnected(id) if id == self.epoch => {
                    self.busy = false;
                    if self.playback.is_none() && self.unsaved.is_none() {
                        self.status = "Disconnected. You can still review captured results.".into();
                    }
                }
                Event::Failed(id, text) if id == self.epoch => {
                    if self.adapter_connect.take() == Some(id) {
                        self.connection_error = Some(text.clone());
                    }
                    self.connection_failed(text);
                }
                Event::Log(id, text) if id == self.epoch => self.log(text),
                Event::Progress(id, done, total, name)
                    if id == self.epoch && !self.poll_in_flight =>
                {
                    self.progress = (done, total, name)
                }
                Event::MonitorSupport(id, support) if id == self.epoch => {
                    if !self.cancel.load(Ordering::Relaxed) && !support.reported.is_empty() {
                        self.status = "Reading stored monitor results…".into();
                    }
                    self.monitor_support = Some(support);
                }
                Event::OxygenSupport(id, coverage) if id == self.epoch => {
                    self.oxygen_support = coverage;
                }
                Event::VehicleInfoSupport(id, support) if id == self.epoch => {
                    if !self.cancel.load(Ordering::Relaxed) {
                        self.status = "Reading vehicle information…".into();
                    }
                    self.vehicle_info_support = Some(support);
                }
                Event::LivePidSupport(id, support)
                    if self.connected && (id == self.epoch || self.connection_id == Some(id)) =>
                {
                    self.pid_support = support;
                }
                Event::LiveGroupingFallback(id) if id == self.epoch => {
                    self.live_grouping_fell_back = true;
                    self.log("Grouped replies differed from reported PID support. Using individual Live Data reads until reconnect.".into());
                }
                Event::Rows(id, rows) if id == self.epoch => self.apply_sample(Sample {
                    at_ms: self.now(),
                    rows,
                    ..Default::default()
                }),
                Event::InitialVin(id, rows) if self.connection_id == Some(id) && self.connected => {
                    self.vin_request = None;
                    if !rows.is_empty() {
                        let at_ms = self.now();
                        self.apply_sample(Sample {
                            at_ms,
                            rows,
                            ..Default::default()
                        });
                    }
                }
                Event::LiveRows(id, at, rows)
                    if id == self.epoch && !self.cancel.load(Ordering::Relaxed) =>
                {
                    self.apply_sample(Sample {
                        at_ms: self.sample_time(at),
                        rows,
                        ..Default::default()
                    });
                }
                Event::Update(update) => match update {
                    Update::Row(id, r) if id == self.epoch => self.apply_sample(Sample {
                        at_ms: self.now(),
                        rows: vec![r],
                        ..Default::default()
                    }),
                    Update::ReplaceRows(id, source, request, rows) if id == self.epoch => self
                        .apply_sample(Sample {
                            at_ms: self.now(),
                            rows,
                            replace: Some((source, request)),
                            ..Default::default()
                        }),
                    Update::Done(id, text) if id == self.epoch => self.status = text,
                    _ => {}
                },
                Event::Finished(id, text) if id == self.epoch => {
                    if std::mem::take(&mut self.poll_in_flight) {
                        self.next_live = Instant::now() + Duration::from_secs_f32(self.interval);
                        continue;
                    }
                    let text = self
                        .finish_emissions_scan(self.cancel.load(Ordering::Relaxed))
                        .unwrap_or(text);
                    self.busy = false;
                    self.clearing = false;
                    self.log(text.clone());
                    self.status = text;
                }
                _ => {}
            }
        }
        if self.live
            && !self.poll_in_flight
            && !self.busy
            && self.connected
            && Instant::now() >= self.next_live
        {
            let queries = self.selected_queries();
            if queries.is_empty() {
                self.stop_live();
                self.status = if self.selected.is_empty() {
                    "Select data items to start live data."
                } else {
                    NO_REPORTED_SELECTION
                }
                .into();
            } else {
                self.read_live(queries, true);
            }
        }
        if self.replay_playing
            && let Some(end) = self.playback.as_ref().map(|capture| capture.end_ms())
        {
            let advance = self.replay_tick.elapsed().as_millis() as u64;
            self.replay_tick = Instant::now();
            self.seek_replay(self.replay_at.saturating_add(advance));
            self.replay_playing = self.replay_at < end;
        }
    }
    fn connection_failed(&mut self, message: String) {
        self.cancel_operation();
        self.connected = false;
        self.driver = None;
        self.protocol.clear();
        self.connection_id = None;
        self.vehicle_reply = None;
        self.finish_recording();
        self.status = message.clone();
        if self.unsaved.is_some() {
            self.status.push_str(" A recording is waiting to be saved.");
        }
        self.log(message);
    }
    fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }
        if !self.connected || self.playback.is_some() {
            self.status = "Connect a vehicle or practice session before recording.".into();
            return;
        }
        if self.scanning_emissions {
            self.status =
                "Wait for the emissions scan to finish before starting a recording.".into();
            return;
        }
        if self.unsaved.is_some() {
            self.status = "Save the pending recording first.".into();
            return;
        }
        self.recording_rows = 0;
        self.marker_note.clear();
        self.recording = Some(Capture::new(self.vehicle.clone(), self.demo, self.now()));
        self.status =
            "Recording incoming readings. Start live data or run a diagnostic read.".into();
    }
    fn mark_recording(&mut self) {
        let now = self.now();
        if let Some(capture) = &mut self.recording {
            match capture.add_marker(now, &self.marker_note) {
                Ok(()) => {
                    self.marker_note.clear();
                    self.status = format!("Marker {} added.", capture.markers.len());
                }
                Err(message) => self.status = message.into(),
            }
        }
    }
    fn finish_recording(&mut self) {
        if let Some(mut capture) = self.recording.take() {
            if self.recording_rows == 0
                && capture.markers.is_empty()
                && !capture
                    .samples
                    .iter()
                    .any(|sample| sample.clear_requested || sample.emissions_scan.is_some())
            {
                self.status = "Recording stopped before any readings arrived.".into();
                return;
            }
            let scan_in_recording = capture
                .samples
                .iter()
                .rev()
                .find_map(|sample| sample.emissions_scan);
            if matches!(scan_in_recording, Some(ScanEvent::Started))
                && capture.samples.len() < MAX_CAPTURE_ROWS
            {
                capture.samples.push(Sample {
                    at_ms: self.results.next_sample_ms(self.now()),
                    emissions_scan: Some(ScanEvent::Interrupted),
                    ..Default::default()
                });
            }
            capture.vehicle = self.vehicle.clone();
            self.unsaved = Some(capture);
            self.save_recording();
        }
    }
    fn save_recording(&mut self) {
        if let Some(capture) = &self.unsaved {
            let path = self.root.join("recordings").join(format!(
                "{}-{}.json",
                if capture.demo { "practice" } else { "vehicle" },
                capture.started_ms
            ));
            self.save_recording_to(path);
        }
    }
    fn save_recording_to(&mut self, path: PathBuf) {
        let Some(capture) = &self.unsaved else { return };
        if !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            self.status = "Choose a .json destination filename.".into();
            return;
        }
        match save_json(&path, capture) {
            Ok(()) => {
                self.status = format!("Recording saved to {}", path.display());
                self.last_export = Some(path.clone());
                let capture = self.unsaved.take().unwrap();
                self.recording_preview = Some(recordings::Preview::new(path, capture));
                self.recording_preview_error = None;
                self.show_recording_preview = true;
                self.refresh_files();
            }
            Err(error) => self.status = format!("Recording save failed. Still in memory. {error}"),
        }
    }
    fn preview_recording(&mut self, path: PathBuf) {
        self.recording_preview = None;
        self.recording_preview_error = None;
        self.show_recording_preview = true;
        match recordings::Preview::open(path.clone()) {
            Ok(preview) => self.recording_preview = Some(preview),
            Err(error) => self.recording_preview_error = Some((path, error)),
        }
    }
    fn export_capture_csv(&mut self, capture: Arc<Capture>) {
        let path = self.root.join("exports").join(format!(
            "{}-{}.csv",
            if capture.demo { "practice" } else { "vehicle" },
            self.now()
        ));
        match recordings::save_csv(&path, &capture) {
            Ok(()) => {
                self.status = format!("CSV saved to {}", path.display());
                self.last_export = Some(path);
            }
            Err(error) => self.status = format!("Could not export recording. {error}"),
        }
    }
    fn open_recording(&mut self, path: PathBuf) -> bool {
        if self.recording.is_some() || self.unsaved.is_some() {
            self.status = "Stop and save the current recording before opening another.".into();
            return false;
        }
        let capture = self
            .recording_preview
            .as_ref()
            .filter(|preview| preview.path == path)
            .map(|preview| Ok(preview.capture.clone()))
            .unwrap_or_else(|| {
                let capture = load_json::<Capture>(&path)?;
                capture.validate()?;
                Ok::<_, String>(Arc::new(capture))
            });
        match capture {
            Ok(c) => {
                self.disconnect();
                self.reset_results();
                self.vehicle = c.vehicle.clone();
                self.observed_vin = None;
                self.demo = c.demo;
                self.replay_at = c.started_ms;
                self.results = c.replay(self.replay_at);
                self.replay_index = c.samples.partition_point(|s| s.at_ms <= self.replay_at);
                self.playback = Some(replay_graphs::ReplayCapture::new(c));
                self.playback_path = Some(path);
                self.page = Page::Recordings;
                self.status =
                    "Offline playback loaded. Vehicle communication is disconnected.".into();
                true
            }
            Err(e) => {
                self.status = format!("Cannot open recording: {e}");
                false
            }
        }
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot::from_results(&self.vehicle, self.demo, self.view_time(), &self.results)
    }
    fn seek_replay(&mut self, at: u64) {
        let Some(capture) = &self.playback else {
            return;
        };
        let at = at.clamp(capture.started_ms, capture.end_ms());
        let index = capture.samples.partition_point(|s| s.at_ms <= at);
        if index < self.replay_index {
            self.results = Results::default();
            self.replay_index = 0;
        }
        for sample in &capture.samples[self.replay_index..index] {
            self.results.apply(sample);
        }
        self.replay_index = index;
        self.replay_at = at;
    }
    fn save_baseline(&mut self) {
        if self.results.rows.is_empty() {
            self.status = "Read vehicle data or codes before saving a baseline.".into();
            return;
        }
        if !self.vehicle.has_identity() {
            self.status = "Read the VIN or apply a plate or fleet ID from the garage before saving a baseline.".into();
            return;
        }
        let baseline = self.snapshot();
        let path = self
            .root
            .join("baselines")
            .join(format!("baseline-{}.json", self.now()));
        match save_json(&path, &baseline) {
            Ok(()) => {
                self.baseline = Some(baseline);
                self.last_export = Some(path);
                self.status =
                    "Before baseline saved. Run the same reads again after the repair.".into();
            }
            Err(e) => self.status = e,
        }
    }
    fn export_report(&mut self) -> Option<PathBuf> {
        if self.busy {
            return None;
        }
        let snapshot = self.snapshot();
        if snapshot.rows.is_empty()
            && snapshot.failures.is_empty()
            && self.results.emissions_scan.is_none()
        {
            self.status = "Capture some results before exporting a report.".into();
            return None;
        }
        let result = self.save_report_html(&snapshot, self.baseline.as_ref(), "report");
        self.report_saved(result)
    }
    fn export_latest_scan(&mut self) -> Option<PathBuf> {
        if self.busy {
            return None;
        }
        let snapshot = self.latest_scan_snapshot()?;
        let result = self.save_report_html(&snapshot, None, "scan-report");
        self.report_saved(result)
    }
    fn latest_scan_snapshot(&self) -> Option<Arc<Snapshot>> {
        if let Some(snapshot) = &self.latest_scan {
            return Some(snapshot.clone());
        }
        let scan = self.results.emissions_scan.as_ref()?;
        let capture = self.playback.as_ref()?;
        let results = capture.replay(scan.ended_ms?);
        Snapshot::from_emissions_scan(&capture.vehicle, capture.demo, &results).map(Arc::new)
    }
    fn open_scan_review(&mut self, snapshot: Arc<Snapshot>, from: Page) {
        self.scan_review = Some(scan_review::Review::new(snapshot, from));
        self.page = Page::ScanReview;
    }
    fn export_saved_scan(&mut self) -> Option<PathBuf> {
        let snapshot = &self.scan_history.selected.as_ref()?.1;
        let result = self.save_report_html(snapshot, None, "scan-report");
        self.report_saved(result)
    }
    fn export_scan_comparison(&mut self) -> Option<PathBuf> {
        let snapshot = &self.scan_history.selected.as_ref()?.1;
        let comparison = &self.scan_history.comparison.as_ref()?.1;
        let html = comparison.html(&snapshot.vehicle, snapshot.demo);
        let result = self.write_report("scan-comparison", &html);
        self.report_saved(result)
    }
    fn save_report_html(
        &self,
        snapshot: &Snapshot,
        baseline: Option<&Snapshot>,
        prefix: &str,
    ) -> std::io::Result<PathBuf> {
        let html = model::report_with_descriptions(snapshot, baseline, &|code| {
            self.description_for(&snapshot.vehicle, code)
        });
        self.write_report(prefix, &html)
    }
    fn write_report(&self, prefix: &str, html: &str) -> std::io::Result<PathBuf> {
        use std::sync::atomic::AtomicU64;
        static NEXT_REPORT: AtomicU64 = AtomicU64::new(0);
        let stamp = self.now();
        let path = loop {
            let sequence = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
            let path = self.root.join("reports").join(format!(
                "{prefix}-{stamp}-{}-{sequence}.html",
                std::process::id()
            ));
            if !path.exists() {
                break path;
            }
        };
        write_file(&path, |out| std::io::Write::write_all(out, html.as_bytes()))?;
        Ok(path)
    }
    fn report_saved(&mut self, result: std::io::Result<PathBuf>) -> Option<PathBuf> {
        match result {
            Ok(path) => {
                self.status = format!("Report saved: {}", path.display());
                self.last_export = Some(path.clone());
                Some(path)
            }
            Err(e) => {
                self.status = e.to_string();
                None
            }
        }
    }
    fn manual_query(&self) -> Result<Query, String> {
        let bytes = decode_hex(&self.manual).map_err(|e| e.to_string())?;
        if bytes.len() > 16 || !matches!(bytes[0], 1 | 2 | 5 | 6 | 9 | 10) {
            return Err("Choose a read service: 01, 02, 05, 06, 09 or 0A.".into());
        }
        let minimum = match bytes[0] {
            2 | 5 => 3,
            10 => 1,
            _ => 2,
        };
        if bytes.len() < minimum {
            return Err("Request is missing an identifier, frame or test parameter".into());
        }
        if bytes[0] == 5 {
            if self.protocol.contains("CAN") {
                return Err("Use Mode 06 monitor results on a CAN connection.".into());
            }
            if bytes.len() != 3 || !bytes[2].is_power_of_two() {
                return Err("Enter one test ID and one sensor mask for Mode 05.".into());
            }
        }
        Ok(Query {
            bytes,
            target: None,
            name: "Manual diagnostic request".into(),
            signals: vec![],
            verified: false,
        })
    }
    /// Render-only harness uses this navigation entry without generating traffic.
    pub fn preview_page(&mut self, page: Page) {
        self.page = page;
    }
    #[cfg(debug_assertions)]
    pub fn prepare_preview(&mut self, page: Page) {
        // Layout fixtures never open a physical interface and remain visibly simulated.
        let _ = self.tx.try_send(Command::Demo { id: 0 });
        self.demo = true;
        self.vehicle = Vehicle {
            model: "Practice vehicle".into(),
            vin: "WDD00000000000000".into(),
            notes: "Offline practice / layout inspection".into(),
            ..Default::default()
        };
        self.draft = self.vehicle.clone();
        self.protocol = "DEMO CAN".into();
        self.preset("Engine");
        let now = self.now();
        let mut demo = worker::DemoAdapter::new();
        use crate::adapter::DiagnosticAdapter;
        let mut queries = self.selected_queries();
        queries.extend(self.code_queries());
        queries.push(
            self.generic
                .iter()
                .find(|q| q.bytes == [1, 1])
                .unwrap()
                .clone(),
        );
        for index in 0..40u64 {
            for q in &queries {
                let reply = crate::elm327::block_on(demo.request(&q.bytes, 10))
                    .unwrap()
                    .remove(0);
                if let Ok(mut rows) = diagnostics::decode(q, &reply, true) {
                    for r in &mut rows {
                        if r.request == "010C" {
                            let n = 800.0 + (index as f64 * 0.4).sin() * 100.0;
                            r.numeric = Some(n);
                            r.value = format!("{n:.1}");
                        }
                    }
                    self.results.apply(&Sample {
                        at_ms: now.saturating_sub(39_000) + index * 1000,
                        rows,
                        ..Default::default()
                    });
                }
            }
        }
        self.page = page;
        self.status = "OFFLINE PRACTICE PREVIEW - simulated readings, no adapter access".into();
    }

    #[cfg(windows)]
    fn update_restart_blocker(&self) -> Option<&'static str> {
        if self.busy {
            Some("Wait for the current diagnostic operation to finish.")
        } else if self.recording.is_some() || self.unsaved.is_some() {
            Some("Save the recording under Saved work before restarting.")
        } else if self.scan_history.pending_count() > 0 {
            Some("Save pending scans in Scan history before restarting.")
        } else if self.connected && self.demo {
            Some("End the practice session on Overview before restarting.")
        } else if self.connected {
            Some("Disconnect from the vehicle before restarting.")
        } else {
            None
        }
    }

    #[cfg(windows)]
    fn restart_for_update(&mut self, ctx: &egui::Context) {
        if self.update_restart_blocker().is_some() {
            return;
        }
        let Some(update) = self.updates.as_mut().and_then(updates::Updates::take_ready) else {
            return;
        };
        self.restarting_for_update = Some(update);
        let _ = self.tx.try_send(Command::Shutdown);
        ctx.request_repaint();
    }

    #[cfg(windows)]
    fn apply_update_after_shutdown(&mut self, ctx: &egui::Context) {
        let Some(update) = self.restarting_for_update.take() else {
            return;
        };
        let restart_args = [
            std::ffi::OsString::from("--data-dir"),
            self.root.as_os_str().to_owned(),
        ];
        match update.apply(restart_args) {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(err) => {
                tracing::warn!(%err, "Software update restart failed");
                self.voltage = None;
                (self.tx, self.rx) = worker::spawn(
                    std::env::current_exe().ok(),
                    &self.root,
                    self.driver_override
                        .as_ref()
                        .map(|driver| driver.path.clone()),
                    self.elm_connection.clone(),
                );
                if let Some(updates) = &mut self.updates {
                    updates.fail(format!("Could not restart to update. {err}"));
                }
            }
        }
    }
}
impl Drop for Workbench {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.finish_emissions_scan(true);
        self.scan_history.retry_saves();
        self.finish_recording();
        let _ = self.tx.try_send(Command::Shutdown);
    }
}
impl eframe::App for Workbench {
    fn raw_input_hook(&mut self, ctx: &egui::Context, _: &mut egui::RawInput) {
        // Load fonts before egui starts the pass so layout uses them from the first frame.
        appearance::configure(ctx);
    }

    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.tick();
        #[cfg(windows)]
        if self.restarting_for_update.is_some() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| ui.heading("Restarting to update"));
            });
            if self.tx.is_stopped() {
                self.apply_update_after_shutdown(ctx);
            } else {
                if ctx.input(|input| input.viewport().close_requested()) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                ctx.request_repaint_after(Duration::from_millis(40));
            }
            return;
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.prepare_to_close(ctx);
        }
        if self.splash.visible(Instant::now()) {
            appearance::splash(ctx);
            ctx.request_repaint_after(Duration::from_millis(40));
            return;
        }
        self.draw(ctx);
        ctx.request_repaint_after(Duration::from_millis(
            if self.connected || self.replay_playing {
                100
            } else {
                400
            },
        ));
    }
}
