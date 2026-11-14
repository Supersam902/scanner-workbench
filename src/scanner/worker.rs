use super::codes;
use super::connection_report::CANCELLED;
use super::driver::{DriverProcess, Request, Value};
use super::elm::{ElmConnection, ElmEndpoint};
use super::live_reads;
use super::model::{ecu_label, load_json, save_json, unavailable};
use crate::adapter::{DiagnosticAdapter, DiagnosticResponse, DiagnosticTarget, encode_hex};
use crate::diagnostics::{self, Job, PidSupport, Query, Reading, SupportScan, Update};
use crate::elm327::{ConnectionInfo, Elm327Error, block_on};
use crate::j2534::{
    CanProtocol, DriverInfo, KLINE_SESSION_IDLE_MS, ProbePhase, discover_drivers, format_voltage,
    is_pid00_reply, same_driver_path,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[path = "oxygen_tests.rs"]
mod oxygen_tests;
pub use oxygen_tests::Coverage as OxygenCoverage;
use oxygen_tests::Session as OxygenSession;

const VOLTAGE_INTERVAL: Duration = Duration::from_secs(2);
const VOLTAGE_BACKOFF: Duration = Duration::from_secs(10);
const AUTOMATIC_PID_TIMEOUT_MS: u64 = 800;
const EXPECTED_READ_MISS_LIMIT: u8 = 2;
const AUTOMATIC_PROTOCOLS: [CanProtocol; 9] = [
    CanProtocol::Can11Bit500K,
    CanProtocol::Can29Bit500K,
    CanProtocol::Can11Bit250K,
    CanProtocol::Can29Bit250K,
    CanProtocol::KwpFast,
    CanProtocol::Iso9141,
    CanProtocol::KwpSlow,
    CanProtocol::J1850Vpw,
    CanProtocol::J1850Pwm,
];

pub enum Command {
    Connect {
        id: u64,
        protocol: Option<CanProtocol>,
        ecu: Option<u8>,
        cancel: Arc<AtomicBool>,
    },
    SelectDriver(Option<PathBuf>),
    SelectElm(Option<ElmConnection>),
    ConnectElm {
        id: u64,
        connection: ElmConnection,
        cancel: Arc<AtomicBool>,
    },
    Demo {
        id: u64,
    },
    Disconnect {
        id: u64,
    },
    Read {
        id: u64,
        queries: Vec<Query>,
        discovery: Option<(u8, u8)>,
        mode: ReadMode,
        cancel: Arc<AtomicBool>,
    },
    Shutdown,
}

#[derive(Clone, Copy)]
pub enum ReadMode {
    Diagnostic,
    Identify,
    Scan,
    EmissionsScan,
    CheckVehicleReply,
    Live { group: bool },
    Poll { group: bool },
    FreezeFrame(u8),
    MonitorSweep,
    OxygenSweep,
    OxygenTest { sensor: u8, test: u8 },
    VehicleInformation,
    Control,
}

impl ReadMode {
    pub(super) fn is_poll(self) -> bool {
        matches!(self, Self::Poll { .. })
    }

    fn live_grouping(self) -> Option<bool> {
        match self {
            Self::Live { group } | Self::Poll { group } => Some(group),
            _ => None,
        }
    }
}

pub enum Event {
    ConnectProgress(u64, String),
    ConnectAttempt(u64, ConnectionAttempt),
    Connected {
        id: u64,
        driver: Option<String>,
        protocol: String,
        demo: bool,
        pid_support: PidSupport,
    },
    Disconnected(u64),
    LinkLost(u64, String),
    VehicleReply(u64, VehicleReply),
    VehicleReplyChecked {
        connection: u64,
        operation: u64,
        check: ReplyCheck,
    },
    Voltage(Option<VoltageSample>),
    Failed(u64, String),
    Log(u64, String),
    Update(Update),
    Rows(u64, Vec<diagnostics::Reading>),
    InitialVin(u64, Vec<diagnostics::Reading>),
    LiveRows(u64, Instant, Vec<diagnostics::Reading>),
    Support(u64, (u8, u8), Vec<u8>),
    MonitorSupport(u64, PidSupport),
    OxygenSupport(u64, OxygenCoverage),
    VehicleInfoSupport(u64, PidSupport),
    LivePidSupport(u64, PidSupport),
    LiveGroupingFallback(u64),
    Progress(u64, usize, usize, String),
    Finished(u64, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Opened,
    OpenFailed,
    ProbeFailed(ProbePhase),
    ElmSetupFailed,
    ElmProtocolFailed,
    DriverFailed,
    ElmTransportFailed,
    Verified,
    Cancelled,
}

impl AttemptOutcome {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Opened => "Cable opened",
            Self::OpenFailed => "Cable open failed",
            Self::ElmSetupFailed => "ELM setup failed",
            Self::ElmProtocolFailed => "ELM protocol identification failed",
            Self::ProbeFailed(phase) => match phase {
                ProbePhase::Channel => "Channel open failed",
                ProbePhase::Setup => "Request setup failed",
                ProbePhase::Initialization => "K-line initialization failed",
                ProbePhase::Transmit => "Request transmission failed",
                ProbePhase::Reply => "No verified ECU reply",
                ProbePhase::Keepalive => "K-line keepalive failed",
                ProbePhase::Cleanup => "Channel close failed",
            },
            Self::DriverFailed => "Driver process failed",
            Self::ElmTransportFailed => "ELM transport failed",
            Self::Verified => "Verified ECU reply",
            Self::Cancelled => "Cancelled",
        }
    }
}

#[derive(Debug)]
pub struct ConnectionAttempt {
    pub driver: String,
    pub protocol: Option<CanProtocol>,
    pub remembered_protocol: bool,
    pub outcome: AttemptOutcome,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LastConnection {
    driver: PathBuf,
    protocol: CanProtocol,
}

#[derive(Clone, Copy, Debug)]
pub struct VehicleReply {
    pub last_reply: Instant,
    pub missed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyCheck {
    pub outcome: ReplyCheckOutcome,
    pub protocol: CanProtocol,
    pub responses: Vec<ReplyEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyCheckOutcome {
    Verified,
    Partial,
    Rejected,
    Incomplete,
    Silent,
    Failed,
}

impl ReplyCheckOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Verified => "Vehicle replied. Support bitmap verified.",
            Self::Partial => "Vehicle replied. Some ECU results are unverified.",
            Self::Rejected => "ECU rejected the support request.",
            Self::Incomplete => "No complete support bitmap received.",
            Self::Silent => "No vehicle reply received.",
            Self::Failed => "Vehicle reply check failed.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyEvidence {
    pub source: Option<u32>,
    pub description: String,
    pub raw: String,
}

impl ReplyEvidence {
    pub(crate) fn label(&self) -> String {
        let description = if let Some(source) = self.source {
            format!("ECU {} · {}", ecu_label(Some(source)), self.description)
        } else {
            self.description.clone()
        };
        if self.raw.is_empty() {
            description
        } else {
            format!("{description} · {}", self.raw)
        }
    }
}

impl ReplyCheck {
    fn from_result(
        result: Result<Vec<DiagnosticResponse>, Elm327Error>,
        expected: &BTreeSet<Option<u32>>,
        protocol: CanProtocol,
    ) -> Self {
        let replies = match result {
            Ok(replies) => replies,
            Err(error) => {
                let outcome = if matches!(error, Elm327Error::Timeout(_)) {
                    ReplyCheckOutcome::Silent
                } else {
                    ReplyCheckOutcome::Failed
                };
                return Self {
                    outcome,
                    protocol,
                    responses: vec![ReplyEvidence {
                        source: None,
                        description: error.to_string(),
                        raw: String::new(),
                    }],
                };
            }
        };
        Self::inspect(&replies, expected.iter().flatten().copied(), protocol)
    }

    fn inspect(
        replies: &[DiagnosticResponse],
        expected: impl IntoIterator<Item = u32>,
        protocol: CanProtocol,
    ) -> Self {
        let mut verified = 0;
        let mut rejected = 0;
        let mut responses: Vec<_> = replies
            .iter()
            .map(|reply| {
                let description = if is_pid00_reply(reply) {
                    verified += 1;
                    "Support bitmap verified".into()
                } else if let Some(error) = &reply.transport_error {
                    error.clone()
                } else {
                    match reply.payload.as_slice() {
                        [0x7f, 1, 0x78] => "Support reply remained pending".into(),
                        [0x7f, 1, code] => {
                            rejected += 1;
                            format!("Support request rejected (NRC {code:02X})")
                        }
                        [0x41, 0, ..] => "Support bitmap is truncated".into(),
                        _ => "Unexpected support reply".into(),
                    }
                };
                ReplyEvidence {
                    source: reply.source,
                    description,
                    raw: encode_hex(&reply.payload),
                }
            })
            .collect();
        let mut outcome = match responses.len() {
            0 => ReplyCheckOutcome::Silent,
            count if verified == count => ReplyCheckOutcome::Verified,
            _ if verified > 0 => ReplyCheckOutcome::Partial,
            count if rejected == count => ReplyCheckOutcome::Rejected,
            _ => ReplyCheckOutcome::Incomplete,
        };
        for source in expected {
            if replies.iter().any(|reply| reply.source == Some(source)) {
                continue;
            }
            if outcome == ReplyCheckOutcome::Verified {
                outcome = ReplyCheckOutcome::Partial;
            }
            responses.push(ReplyEvidence {
                source: Some(source),
                description: "No reply from this previously seen ECU".into(),
                raw: String::new(),
            });
        }
        Self {
            outcome,
            protocol,
            responses,
        }
    }

    pub(super) fn summary(&self) -> String {
        let mut evidence = self.outcome.label().to_owned();
        for response in &self.responses {
            evidence.push('\n');
            evidence.push_str(&response.label());
        }
        evidence
    }
}

#[derive(Clone)]
pub struct VoltageSample {
    pub adapter: VoltageAdapter,
    pub millivolts: u32,
    pub sampled_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoltageAdapter {
    J2534(DriverInfo),
    Elm(ElmConnection),
}

impl VoltageAdapter {
    pub(super) fn label(&self) -> String {
        match self {
            Self::J2534(driver) => driver.name.clone(),
            Self::Elm(connection) => connection.label(),
        }
    }
}

impl VoltageSample {
    fn new(adapter: VoltageAdapter, millivolts: u32) -> Self {
        Self {
            adapter,
            millivolts,
            sampled_at: Instant::now(),
        }
    }
}

pub struct CommandSender {
    sender: mpsc::SyncSender<Command>,
    activity: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    #[cfg(windows)]
    thread: std::thread::JoinHandle<()>,
}

impl CommandSender {
    #[cfg(windows)]
    pub(super) fn is_stopped(&self) -> bool {
        self.thread.is_finished()
    }

    pub fn try_send(&self, command: Command) -> Result<(), mpsc::TrySendError<Command>> {
        if matches!(&command, Command::Shutdown) {
            self.shutdown.store(true, Ordering::Relaxed);
        }
        self.activity.fetch_add(1, Ordering::Relaxed);
        self.sender.try_send(command)
    }
}

impl Drop for CommandSender {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

struct DriverRuntime {
    executable: Option<PathBuf>,
    connection_cache: Option<PathBuf>,
    activity: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
}

struct VehicleSession {
    pid_scan: SupportScan,
    monitor_scan: SupportScan,
    oxygen: OxygenSession,
    info_scan: SupportScan,
    scan_responders: ScanResponders,
    grouping_allowed: bool,
}

impl Default for VehicleSession {
    fn default() -> Self {
        Self {
            pid_scan: SupportScan::new(1, 0),
            monitor_scan: SupportScan::new(6, 0),
            oxygen: OxygenSession::default(),
            info_scan: SupportScan::new(9, 0),
            scan_responders: ScanResponders::new(),
            grouping_allowed: true,
        }
    }
}

impl VehicleSession {
    fn connected(
        &mut self,
        mut physical: PhysicalLink,
        replies: Vec<DiagnosticResponse>,
        tx: &mpsc::Sender<Event>,
    ) -> Link {
        *self = Self::default();
        self.pid_scan.accept(Ok(replies));
        physical
            .expected_pids
            .extend(self.pid_scan.support.reported.iter().copied());
        let _ = tx.send(Event::Connected {
            id: physical.id,
            driver: Some(physical.info.port.clone()),
            protocol: physical.info.protocol.clone(),
            demo: false,
            pid_support: self.pid_scan.support.clone(),
        });
        let _ = tx.send(Event::VehicleReply(physical.id, physical.vehicle_reply));
        Link::Physical(Box::new(physical))
    }
}

impl DriverRuntime {
    fn launch(&self) -> Result<DriverProcess, String> {
        let exe = self
            .executable
            .as_deref()
            .ok_or("Scanner executable is unavailable")?;
        DriverProcess::start(exe, self.shutdown.clone())
            .map_err(|e| format!("Cannot start the adapter process: {e}"))
    }
}

pub(super) fn spawn(
    executable: Option<PathBuf>,
    data_dir: &Path,
    selected_driver: Option<PathBuf>,
    selected_elm: Option<ElmConnection>,
) -> (CommandSender, mpsc::Receiver<Event>) {
    spawn_worker(
        executable,
        Some(data_dir.to_owned()),
        selected_driver,
        selected_elm,
        discover_drivers,
    )
}

#[doc(hidden)]
pub fn spawn_with_driver(
    executable: Option<PathBuf>,
    discover: impl Fn() -> Vec<DriverInfo> + Send + 'static,
) -> (CommandSender, mpsc::Receiver<Event>) {
    spawn_with_driver_state(executable, None, None, discover)
}

#[doc(hidden)]
pub fn spawn_with_driver_state(
    executable: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    selected_driver: Option<PathBuf>,
    discover: impl Fn() -> Vec<DriverInfo> + Send + 'static,
) -> (CommandSender, mpsc::Receiver<Event>) {
    spawn_worker(executable, data_dir, selected_driver, None, discover)
}

fn spawn_worker(
    executable: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    selected_driver: Option<PathBuf>,
    selected_elm: Option<ElmConnection>,
    discover: impl Fn() -> Vec<DriverInfo> + Send + 'static,
) -> (CommandSender, mpsc::Receiver<Event>) {
    let (sender, rx) = mpsc::sync_channel(8);
    let (events, out) = mpsc::channel();
    let activity = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let runtime = DriverRuntime {
        executable,
        connection_cache: data_dir.map(|dir| dir.join("last-connection.json")),
        activity: activity.clone(),
        shutdown: shutdown.clone(),
    };
    let _thread = std::thread::spawn(move || {
        run(rx, events, discover, runtime, selected_driver, selected_elm)
    });
    let tx = CommandSender {
        sender,
        activity,
        shutdown,
        #[cfg(windows)]
        thread: _thread,
    };
    (tx, out)
}
enum Link {
    Physical(Box<PhysicalLink>),
    Demo(DemoAdapter),
}

fn close_link(link: &mut Option<Link>, kline_retry_at: &mut Instant) {
    let was_kline = matches!(link, Some(Link::Physical(physical)) if physical.protocol.is_kline());
    *link = None;
    if was_kline {
        *kline_retry_at = Instant::now() + Duration::from_millis(u64::from(KLINE_SESSION_IDLE_MS));
    }
}

struct PhysicalLink {
    driver: DriverProcess,
    adapter: VoltageAdapter,
    info: ConnectionInfo,
    protocol: CanProtocol,
    id: u64,
    next_voltage: Instant,
    vehicle_reply: VehicleReply,
    expected_read_misses: u8,
    expected_pids: BTreeSet<u8>,
    cancel: Option<Arc<AtomicBool>>,
    events: mpsc::Sender<Event>,
}

fn is_vehicle_reply(reply: &DiagnosticResponse) -> bool {
    reply.transport_error.is_none() && !reply.payload.is_empty()
}

impl PhysicalLink {
    fn read_millivolts(&mut self) -> Result<u32, Elm327Error> {
        let cancel = &self.cancel;
        match self.driver.call(Request::Voltage, || {
            cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
        }) {
            Ok(Value::Voltage(value)) => Ok(value),
            Err(message) => Err(Elm327Error::ProtocolError(message)),
            _ => unreachable!("the driver response type was checked"),
        }
    }

    fn refresh_voltage(&mut self) {
        if Instant::now() >= self.next_voltage
            && self.driver.failure().is_none()
            && !self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            let value = self
                .read_millivolts()
                .ok()
                .map(|millivolts| VoltageSample::new(self.adapter.clone(), millivolts));
            self.next_voltage = Instant::now() + VOLTAGE_INTERVAL;
            let _ = self.events.send(Event::Voltage(value));
        }
    }

    fn read(
        &mut self,
        target: Option<DiagnosticTarget>,
        bytes: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        let cancel = &self.cancel;
        let result = self.driver.call(
            Request::Read {
                bytes: bytes.to_vec(),
                target,
                timeout_ms,
            },
            || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)),
        );
        let completed_at = Instant::now();
        self.refresh_voltage();
        if self.driver.failure().is_none()
            && !self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            self.observe_vehicle_reply(target.is_none(), bytes, &result, completed_at);
        }
        match result {
            Ok(Value::Replies(replies)) => Ok(replies),
            Ok(Value::ReadTimeout(message)) => Err(Elm327Error::Timeout(message)),
            Err(message) => Err(Elm327Error::ProtocolError(message)),
            _ => unreachable!("the driver response type was checked"),
        }
    }

    fn observe_vehicle_reply(
        &mut self,
        functional: bool,
        bytes: &[u8],
        result: &Result<Value, String>,
        completed_at: Instant,
    ) {
        match result {
            Ok(Value::Replies(replies)) => {
                let mut answered = replies.iter().filter(|reply| {
                    if bytes == [1, 0] {
                        is_pid00_reply(reply)
                    } else {
                        is_vehicle_reply(reply)
                    }
                });
                let Some(first) = answered.next() else {
                    return;
                };
                // Remember PIDs that replied even when the support scan was incomplete.
                if let (true, [1, pid]) = (functional, bytes) {
                    if std::iter::once(first).chain(answered).any(|reply| {
                        matches!(reply.payload.as_slice(), [0x41, echoed, _, ..] if echoed == pid)
                    }) {
                        self.expected_pids.insert(*pid);
                    }
                }
                self.expected_read_misses = 0;
                self.vehicle_reply = VehicleReply {
                    last_reply: completed_at,
                    missed: false,
                };
            }
            Ok(Value::ReadTimeout(_)) if functional && !self.vehicle_reply.missed => {
                if bytes != [1, 0] {
                    let [1, pids @ ..] = bytes else {
                        return;
                    };
                    if !pids.iter().any(|pid| self.expected_pids.contains(pid)) {
                        return;
                    }
                    self.expected_read_misses += 1;
                    if self.expected_read_misses < EXPECTED_READ_MISS_LIMIT {
                        return;
                    }
                }
                self.vehicle_reply.missed = true;
            }
            _ => return,
        }
        let _ = self
            .events
            .send(Event::VehicleReply(self.id, self.vehicle_reply));
    }
}

impl Link {
    fn include_reported_pids(&mut self, support: &PidSupport) {
        if let Self::Physical(link) = self {
            // Later incomplete support scans cannot undo PIDs already seen on this session.
            link.expected_pids.extend(support.reported.iter().copied());
        }
    }

    fn failure(&self) -> Option<(u64, String)> {
        match self {
            Self::Physical(link) => link
                .driver
                .failure()
                .map(|reason| (link.id, reason.to_owned())),
            _ => None,
        }
    }
}
impl DiagnosticAdapter for Link {
    async fn request(
        &mut self,
        bytes: &[u8],
        timeout: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        match self {
            Self::Physical(link) => link.read(None, bytes, timeout),
            Self::Demo(a) => a.request(bytes, timeout).await,
        }
    }
    async fn request_to(
        &mut self,
        target: DiagnosticTarget,
        bytes: &[u8],
        timeout: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        match self {
            Self::Physical(link) => link.read(Some(target), bytes, timeout),
            Self::Demo(a) => a.request_to(target, bytes, timeout).await,
        }
    }
    fn connection_info(&self) -> &ConnectionInfo {
        match self {
            Self::Physical(link) => &link.info,
            Self::Demo(a) => a.connection_info(),
        }
    }
    async fn voltage(&mut self) -> Result<String, Elm327Error> {
        match self {
            Self::Physical(link) => link.read_millivolts().map(format_voltage),
            Self::Demo(a) => a.voltage().await,
        }
    }
    async fn delay(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
fn driver_candidates(
    mut drivers: Vec<DriverInfo>,
    selected: Option<&Path>,
    preferred: Option<&Path>,
) -> Vec<DriverInfo> {
    fn rank(driver: &DriverInfo) -> u8 {
        let name = driver.name.to_ascii_lowercase();
        let file = driver
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if name.contains("mfc3") || file.starts_with("mongoose") && file.contains("mfc3") {
            0
        } else if name.contains("mongoose") || file.starts_with("mongoose") {
            1
        } else {
            2
        }
    }
    if let Some(path) = selected {
        let selected = drivers
            .iter()
            .position(|driver| driver.path == path)
            .or_else(|| {
                drivers
                    .iter()
                    .position(|driver| same_driver_path(&driver.path, path))
            });
        return selected
            .map(|index| drivers.swap_remove(index))
            .into_iter()
            .collect();
    }
    // Keep the original cable priority while allowing other registered drivers.
    drivers.sort_by_key(|driver| {
        let remembered = preferred.is_some_and(|path| same_driver_path(&driver.path, path));
        (rank(driver), !remembered)
    });
    let mut seen = BTreeSet::new();
    drivers.retain(|driver| seen.insert(driver.path.to_string_lossy().to_ascii_lowercase()));
    drivers
}

#[allow(clippy::too_many_arguments)] // The worker owns the connection state and its retry deadlines.
fn connect_vehicle(
    id: u64,
    drivers: Vec<DriverInfo>,
    override_protocol: Option<CanProtocol>,
    ecu: Option<u8>,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<Event>,
    runtime: &DriverRuntime,
    voltage_driver: &mut Option<PathBuf>,
    kline_retry_at: &mut Instant,
    last_connection: Option<&LastConnection>,
) -> Result<(PhysicalLink, Vec<DiagnosticResponse>), String> {
    let cancelled = || cancel.load(Ordering::Relaxed) || runtime.shutdown.load(Ordering::Relaxed);
    let log = |message| {
        let _ = tx.send(Event::Log(id, message));
    };
    let progress = |message| {
        let _ = tx.send(Event::ConnectProgress(id, message));
    };
    if cancelled() {
        return Err(CANCELLED.into());
    }
    let automatic = override_protocol.is_none() && ecu.is_none();
    let default_protocols: &[CanProtocol] = match override_protocol {
        Some(ref protocol) => std::slice::from_ref(protocol),
        None if ecu.is_some() => &[CanProtocol::Can29Bit500K, CanProtocol::Can29Bit250K],
        None => &AUTOMATIC_PROTOCOLS,
    };
    let mut last_open_error = None;
    let mut cable_opened = false;
    let mut furthest_phase = None;
    'drivers: for driver in drivers {
        if cancelled() {
            break;
        }
        let mut protocols = default_protocols.to_vec();
        let remembered = last_connection
            .filter(|connection| automatic && same_driver_path(&connection.driver, &driver.path))
            .map(|connection| connection.protocol);
        if let Some(index) = protocols
            .iter()
            .position(|&protocol| Some(protocol) == remembered)
        {
            protocols[..=index].rotate_right(1);
        }
        let attempt = |protocol, outcome, detail| {
            let _ = tx.send(Event::ConnectAttempt(
                id,
                ConnectionAttempt {
                    driver: driver.name.clone(),
                    protocol,
                    remembered_protocol: remembered.is_some() && protocol == remembered,
                    outcome,
                    detail,
                },
            ));
        };
        progress(format!("Opening {}", driver.name));
        log(format!(
            "Opening {} ({})",
            driver.name,
            driver.path.display()
        ));
        let mut process = match runtime.launch() {
            Ok(process) => process,
            Err(error) => {
                attempt(None, AttemptOutcome::DriverFailed, Some(error.clone()));
                return Err(error);
            }
        };
        let opened = process.call(Request::Open(driver.path.clone()), cancelled);
        match opened {
            Ok(Value::Done) => attempt(None, AttemptOutcome::Opened, None),
            Err(_) if cancelled() => {
                attempt(None, AttemptOutcome::Cancelled, None);
                break;
            }
            Err(error) => {
                let failure = format!("Could not open {}. {error}", driver.name);
                log(failure.clone());
                last_open_error = Some(failure);
                let outcome = if process.failure().is_some() {
                    AttemptOutcome::DriverFailed
                } else {
                    AttemptOutcome::OpenFailed
                };
                attempt(None, outcome, Some(error));
                continue;
            }
            Ok(_) => unreachable!("the driver response type was checked"),
        }
        cable_opened = true;
        log(format!(
            "{} opened. Checking for an OBD-II response.",
            driver.name
        ));
        let mut voltage_at = Instant::now();
        let mut sample_voltage = |process: &mut DriverProcess| {
            if Instant::now() >= voltage_at {
                let value = match process.call(Request::Voltage, cancelled) {
                    Ok(Value::Voltage(millivolts)) => Some(VoltageSample::new(
                        VoltageAdapter::J2534(driver.clone()),
                        millivolts,
                    )),
                    Ok(_) => unreachable!("the driver response type was checked"),
                    Err(_) => None,
                };
                voltage_at = Instant::now() + VOLTAGE_INTERVAL;
                if !cancelled() {
                    if value.is_some() {
                        *voltage_driver = Some(driver.path.clone());
                    }
                    let _ = tx.send(Event::Voltage(value));
                }
            }
        };
        for (index, &protocol) in protocols.iter().enumerate() {
            if cancelled() {
                break 'drivers;
            }
            if protocol.is_kline() && Instant::now() < *kline_retry_at {
                progress(format!(
                    "Waiting for K-line to settle before {}",
                    protocol.short_label()
                ));
                while !cancelled()
                    && Instant::now() < *kline_retry_at
                    && process.failure().is_none()
                {
                    sample_voltage(&mut process);
                    std::thread::sleep(
                        kline_retry_at
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(25)),
                    );
                }
                if cancelled() {
                    break 'drivers;
                }
                if let Some(error) = process.failure() {
                    log(format!("{}: {error}", driver.name));
                    attempt(None, AttemptOutcome::DriverFailed, Some(error.into()));
                    continue 'drivers;
                }
            }
            progress(format!(
                "Checking {} ({}/{})",
                protocol.short_label(),
                index + 1,
                protocols.len()
            ));
            let note = if Some(protocol) == remembered {
                " (last verified)"
            } else {
                ""
            };
            log(format!(
                "Trying {} with {}{note}",
                protocol.label(),
                driver.name
            ));
            let probe = process.call(Request::Probe(protocol, ecu), cancelled);
            if protocol.is_kline() {
                // A successful or interrupted init can leave an ECU awake.
                let retry = match &probe {
                    Ok(Value::ProbeFailed { kline_retry_ms, .. })
                        if process.failure().is_none() =>
                    {
                        *kline_retry_ms
                    }
                    _ => KLINE_SESSION_IDLE_MS,
                };
                *kline_retry_at = Instant::now() + Duration::from_millis(u64::from(retry));
            }
            if cancelled() {
                attempt(Some(protocol), AttemptOutcome::Cancelled, None);
                if protocol.is_kline() && matches!(probe, Ok(Value::Replies(_))) {
                    // Release periodic K-line traffic before starting the idle interval.
                    drop(process);
                    *kline_retry_at =
                        Instant::now() + Duration::from_millis(u64::from(KLINE_SESSION_IDLE_MS));
                }
                break 'drivers;
            }
            let (outcome, probe_error) = match probe {
                Ok(Value::Replies(replies)) => {
                    let check = ReplyCheck::inspect(&replies, [], protocol);
                    attempt(
                        Some(protocol),
                        AttemptOutcome::Verified,
                        Some(check.summary()),
                    );
                    let info = ConnectionInfo {
                        port: driver.name.clone(),
                        protocol: protocol.label().into(),
                        baud: 0,
                        elm_version: "J2534 04.04".into(),
                        voltage: None,
                    };
                    return Ok((
                        PhysicalLink {
                            driver: process,
                            adapter: VoltageAdapter::J2534(driver),
                            info,
                            protocol,
                            id,
                            next_voltage: Instant::now(),
                            vehicle_reply: VehicleReply {
                                last_reply: Instant::now(),
                                missed: false,
                            },
                            expected_read_misses: 0,
                            expected_pids: BTreeSet::new(),
                            cancel: None,
                            events: tx.clone(),
                        },
                        replies,
                    ));
                }
                Ok(Value::ProbeFailed { phase, message, .. }) => {
                    furthest_phase = furthest_phase.max(Some(phase));
                    (AttemptOutcome::ProbeFailed(phase), message)
                }
                Ok(_) => unreachable!("the driver response type was checked"),
                Err(error) => (AttemptOutcome::DriverFailed, error),
            };
            log(format!("{} failed. {probe_error}", protocol.label()));
            attempt(Some(protocol), outcome, Some(probe_error));
            if process.failure().is_some() {
                continue 'drivers;
            }
        }
        log(format!(
            "{} opened without a verified OBD-II reply.",
            driver.name
        ));
    }
    if cancelled() {
        Err(CANCELLED.into())
    } else if cable_opened {
        let result = match furthest_phase {
            Some(ProbePhase::Transmit) => {
                "The adapter opened. Diagnostic request transmission failed."
            }
            Some(ProbePhase::Reply) => "The adapter sent a request. No OBD-II reply was verified.",
            Some(ProbePhase::Keepalive) => {
                "An ECU replied. The scanner could not start the K-line keepalive."
            }
            Some(ProbePhase::Cleanup) => {
                "An ECU answered CAN discovery. The driver could not release the discovery channel."
            }
            Some(_) => "The adapter opened. The scanner could not start a diagnostic session.",
            None => "The adapter opened. The driver process failed during the connection check.",
        };
        Err(format!("{result} See Connection for details."))
    } else {
        Err(format!(
            "{}. See Connection for the attempt log.",
            last_open_error
                .as_deref()
                .unwrap_or("Could not open a J2534 adapter")
                .trim_end_matches('.')
        ))
    }
}

fn sample_idle_voltage(
    runtime: &DriverRuntime,
    discover: &impl Fn() -> Vec<DriverInfo>,
    selected: Option<&Path>,
    preferred: Option<&Path>,
    interrupted: impl Fn() -> bool,
) -> Result<Option<VoltageSample>, String> {
    let mut drivers = driver_candidates(discover(), selected, None);
    if let Some(path) = preferred {
        drivers.sort_by_key(|driver| !same_driver_path(&driver.path, path));
    }
    let mut zero_sample = None;
    let mut driver_failure = None;
    for driver in drivers {
        if interrupted() {
            return Ok(None);
        }
        let mut process = runtime.launch()?;
        let result = process
            .call(Request::Open(driver.path.clone()), &interrupted)
            .and_then(|_| process.call(Request::Voltage, &interrupted));
        match result {
            Ok(Value::Voltage(millivolts)) => {
                let sample = VoltageSample::new(VoltageAdapter::J2534(driver), millivolts);
                if millivolts != 0 {
                    return Ok(Some(sample));
                }
                zero_sample.get_or_insert(sample);
            }
            Ok(_) => unreachable!("the driver response type was checked"),
            Err(e) if process.failure().is_some() => driver_failure = Some(e),
            Err(_) => {}
        }
        // Drop releases this device before another library is opened.
    }
    if zero_sample.is_some() {
        Ok(zero_sample)
    } else {
        driver_failure.map_or(Ok(None), Err)
    }
}

#[allow(clippy::too_many_arguments)] // The worker owns the adapter handle and its operation deadline.
fn connect_elm(
    id: u64,
    connection: &ElmConnection,
    last_baud: Option<u32>,
    idle: Option<(DriverProcess, ElmEndpoint)>,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<Event>,
    runtime: &DriverRuntime,
) -> Result<(PhysicalLink, Vec<DiagnosticResponse>, Option<u32>), String> {
    let cancelled = || cancel.load(Ordering::Relaxed) || runtime.shutdown.load(Ordering::Relaxed);
    let mut idle = idle;
    let origin = VoltageAdapter::Elm(connection.clone());
    let mut last_error = String::from("Could not open an ELM adapter");
    for endpoint in connection.endpoints(last_baud) {
        if cancelled() {
            break;
        }
        let baud = endpoint.baud();
        let name = baud.map_or_else(
            || connection.label(),
            |baud| format!("{} · {baud} baud", connection.label()),
        );
        let _ = tx.send(Event::ConnectProgress(id, format!("Checking {name}")));
        let attempt = |protocol, outcome, detail| {
            let _ = tx.send(Event::ConnectAttempt(
                id,
                ConnectionAttempt {
                    driver: name.clone(),
                    protocol,
                    remembered_protocol: false,
                    outcome,
                    detail,
                },
            ));
        };
        let mut adapter = match idle.take() {
            Some((adapter, open_endpoint)) if open_endpoint == endpoint => adapter,
            _ => {
                let mut adapter = match runtime.launch() {
                    Ok(adapter) => adapter,
                    Err(error) => {
                        attempt(None, AttemptOutcome::DriverFailed, Some(error.clone()));
                        return Err(error);
                    }
                };
                let opened = adapter.call(Request::OpenElm(endpoint), cancelled);
                if cancelled() {
                    attempt(None, AttemptOutcome::Cancelled, None);
                    break;
                }
                match opened {
                    Ok(Value::Done) => adapter,
                    Err(error) => {
                        let outcome = if adapter.failure().is_some() {
                            AttemptOutcome::DriverFailed
                        } else {
                            AttemptOutcome::OpenFailed
                        };
                        attempt(None, outcome, Some(error.clone()));
                        last_error = error;
                        // An unavailable port cannot be opened at any baud rate.
                        break;
                    }
                    Ok(_) => unreachable!("the driver response type was checked"),
                }
            }
        };
        attempt(None, AttemptOutcome::Opened, None);
        let probe = adapter.call(Request::ProbeElm, cancelled);
        if cancelled() {
            attempt(None, AttemptOutcome::Cancelled, None);
            break;
        }
        let (protocol, outcome, message) = match probe {
            Ok(Value::ElmProbed {
                protocol,
                identity,
                replies,
            }) => {
                let check = ReplyCheck::inspect(&replies, [], protocol);
                attempt(
                    Some(protocol),
                    AttemptOutcome::Verified,
                    Some(check.summary()),
                );
                let driver = if identity.is_empty() {
                    name
                } else {
                    format!("{name} · {identity}")
                };
                let link = PhysicalLink {
                    driver: adapter,
                    adapter: origin,
                    info: ConnectionInfo {
                        port: driver,
                        baud: baud.unwrap_or(0),
                        protocol: protocol.label().into(),
                        elm_version: identity,
                        voltage: None,
                    },
                    protocol,
                    id,
                    next_voltage: Instant::now(),
                    vehicle_reply: VehicleReply {
                        last_reply: Instant::now(),
                        missed: false,
                    },
                    expected_read_misses: 0,
                    expected_pids: BTreeSet::new(),
                    cancel: None,
                    events: tx.clone(),
                };
                return Ok((link, replies, baud));
            }
            Ok(Value::ElmSetupFailed(message)) => (None, AttemptOutcome::ElmSetupFailed, message),
            Ok(Value::ElmTransportFailed(message)) => {
                (None, AttemptOutcome::ElmTransportFailed, message)
            }
            Ok(Value::ElmProtocolFailed(message)) => {
                (None, AttemptOutcome::ElmProtocolFailed, message)
            }
            Ok(Value::ElmReplyFailed { protocol, message }) => (
                protocol,
                AttemptOutcome::ProbeFailed(ProbePhase::Reply),
                message,
            ),
            Ok(Value::ElmUnverified { protocol, replies }) => (
                Some(protocol),
                AttemptOutcome::ProbeFailed(ProbePhase::Reply),
                ReplyCheck::inspect(&replies, [], protocol).summary(),
            ),
            Err(error) => (None, AttemptOutcome::DriverFailed, error),
            Ok(_) => unreachable!("the driver response type was checked"),
        };
        attempt(protocol, outcome, Some(message.clone()));
        last_error = format!("{}. {message}", outcome.label());
    }
    if cancelled() {
        Err(CANCELLED.into())
    } else {
        Err(format!(
            "{} did not connect. {last_error}",
            connection.label()
        ))
    }
}

fn sample_idle_elm(
    runtime: &DriverRuntime,
    connection: &ElmConnection,
    idle: &mut Option<(DriverProcess, ElmEndpoint)>,
    last_baud: &mut Option<u32>,
    interrupted: impl Fn() -> bool,
) -> Result<Option<VoltageSample>, String> {
    let origin = VoltageAdapter::Elm(connection.clone());
    if let Some((adapter, _)) = idle {
        match adapter.call(Request::Voltage, &interrupted) {
            Ok(Value::Voltage(value)) => return Ok(Some(VoltageSample::new(origin, value))),
            Err(_) => *idle = None,
            Ok(_) => unreachable!("the driver response type was checked"),
        }
        return Ok(None);
    }
    for endpoint in connection.endpoints(*last_baud) {
        if interrupted() {
            return Ok(None);
        }
        let mut adapter = runtime.launch()?;
        adapter.call(Request::OpenElm(endpoint.clone()), &interrupted)?;
        match adapter.call(Request::Voltage, &interrupted) {
            Ok(Value::Voltage(value)) => {
                *last_baud = endpoint.baud();
                *idle = Some((adapter, endpoint));
                return Ok(Some(VoltageSample::new(origin, value)));
            }
            Err(_) => {}
            Ok(_) => unreachable!("the driver response type was checked"),
        }
    }
    Ok(None)
}

fn read_one(
    adapter: &mut Link,
    query: &Query,
    id: u64,
    control: bool,
    cancel: &Arc<AtomicBool>,
    tx: &mpsc::Sender<Event>,
) -> Vec<Reading> {
    let key = super::model::query_key(query);
    let _ = tx.send(Event::Log(id, format!("Request {key} / {}", query.name)));
    let (inner_tx, inner_rx) = mpsc::channel();
    block_on(diagnostics::execute(
        adapter,
        Job {
            id,
            queries: vec![query.clone()],
            discovery: None,
            cancelled: cancel.clone(),
            allow_control: control,
        },
        &inner_tx,
    ));
    let mut rows = Vec::new();
    for update in inner_rx.try_iter() {
        if let crate::app::ObdEvent::Diagnostics(update) = update {
            match update {
                Update::Row(_, reading) => rows.push(reading),
                Update::Done(_, _) => {}
                other => {
                    let _ = tx.send(Event::Update(other));
                }
            }
        }
    }
    rows
}

fn publish_rows(tx: &mpsc::Sender<Event>, id: u64, rows: Vec<Reading>) {
    publish_read(tx, id, rows, None);
}

fn publish_read(tx: &mpsc::Sender<Event>, id: u64, rows: Vec<Reading>, live_at: Option<Instant>) {
    for row in &rows {
        let _ = tx.send(Event::Log(
            id,
            format!(
                "ECU {:X?} {}: {} [{}]",
                row.source, row.name, row.value, row.status
            ),
        ));
    }
    if !rows.is_empty() {
        let event = match live_at {
            Some(at) => Event::LiveRows(id, at, rows),
            None => Event::Rows(id, rows),
        };
        let _ = tx.send(event);
    }
}

type ScanResponders = BTreeMap<[u8; 2], BTreeSet<Option<u32>>>;

fn observe_scan_reply<'a>(
    query: &Query,
    rows: &[Reading],
    responders: &'a mut ScanResponders,
) -> Option<&'a BTreeSet<Option<u32>>> {
    let key = match (query.bytes.as_slice(), query.target) {
        ([1, pid], None) => [1, *pid],
        // For Mode 02 we track the trigger request by frame number.
        ([2, 2, frame], None) => [2, *frame],
        ([mode @ (3 | 7 | 10)], None) => [*mode, 0],
        ([6, mid], None) if mid % 32 != 0 => [6, *mid],
        ([9, pid], None) if diagnostics::information_name(*pid).is_some() => [9, *pid],
        _ => return None,
    };
    let observed = responders.entry(key).or_default();
    observed.extend(
        rows.iter()
            .filter(|row| row.source.is_some() && !unavailable(row))
            .map(|row| row.source),
    );
    Some(observed)
}

fn prepare_legacy_code_scan(queries: &mut Vec<Query>, can: bool) -> bool {
    if can || !queries.iter().any(|q| q.bytes == [3] && q.target.is_none()) {
        return false;
    }
    // Read the per-ECU confirmed-code count before Mode 03. Reuse the
    // readiness request if this is the post-clear scan.
    let query = queries
        .iter()
        .position(|q| q.bytes == [1, 1] && q.target.is_none())
        .map(|index| queries.remove(index))
        .unwrap_or_else(|| Query {
            bytes: vec![1, 1],
            target: None,
            name: "Readiness since clear".into(),
            signals: vec![],
            verified: false,
        });
    queries.insert(0, query);
    true
}

fn freeze_frame_query(catalog: &[Query], pid: u8, frame: u8) -> Query {
    let bytes = vec![2, pid, frame];
    if let Some(base) = catalog.iter().find(|q| {
        q.bytes == [1, pid]
            && q.target.is_none()
            && q.signals.iter().any(|signal| signal["hidden"] != true)
    }) {
        let mut query = base.clone();
        query.bytes = bytes;
        query
    } else {
        Query {
            bytes,
            target: None,
            name: format!("PID {pid:02X}"),
            signals: vec![],
            verified: false,
        }
    }
}

#[allow(clippy::too_many_arguments)] // Receives the frame, query catalog, and current session state.
fn read_freeze_frame(
    adapter: &mut Link,
    id: u64,
    frame: u8,
    catalog: &[Query],
    cancel: &Arc<AtomicBool>,
    tx: &mpsc::Sender<Event>,
    report_scan: bool,
    responders: &mut ScanResponders,
) -> String {
    let stopped = |adapter: &Link| cancel.load(Ordering::Relaxed) || adapter.failure().is_some();
    let mut trigger = freeze_frame_query(catalog, 2, frame);
    trigger.name = "DTC that caused freeze-frame storage".into();
    let mut triggers = read_one(adapter, &trigger, id, false, cancel, tx);
    super::freeze_frames::flag_conflicting_triggers(&mut triggers);
    if !stopped(adapter) {
        if let Some(expected) = observe_scan_reply(&trigger, &triggers, responders) {
            triggers = live_reads::individual_rows(&trigger, expected, triggers);
        }
    }
    let data_sources: BTreeSet<_> = triggers
        .iter()
        .filter(|r| !unavailable(r) && !r.no_freeze_frame())
        .map(|r| r.source)
        .collect();
    let empty_sources: BTreeSet<_> = triggers
        .iter()
        .filter(|r| r.source.is_some() && r.no_freeze_frame())
        .filter(|r| {
            triggers
                .iter()
                .all(|other| other.source != r.source || other.no_freeze_frame())
        })
        .map(|r| r.source)
        .collect();
    let mut scan = SupportScan::new(2, frame);
    if report_scan {
        scan.expect_sources(data_sources.iter().copied());
    } else {
        scan.expect_sources(
            triggers
                .iter()
                .filter(|r| {
                    r.source.is_some() && !matches!(r.raw.as_str(), "7F0211" | "7F0212" | "7F0231")
                })
                .map(|r| r.source),
        );
    }
    let trigger_problem = triggers
        .iter()
        .any(|r| unavailable(r) && r.status != diagnostics::UNSUPPORTED_REQUEST);
    publish_rows(tx, id, triggers);
    if report_scan && data_sources.is_empty() {
        return if trigger_problem {
            "Freeze frame could not be verified.".into()
        } else {
            "No stored freeze frame was reported.".into()
        };
    }

    while let Some(base) = scan.support.next_range {
        if stopped(adapter) {
            return String::new();
        }
        let _ = tx.send(Event::Log(
            id,
            format!("Request 02{base:02X}{frame:02X} / Freeze-frame PID support"),
        ));
        block_on(scan.step(adapter, 2000));
    }
    if stopped(adapter) {
        return String::new();
    }
    let incomplete = scan.support.problem.is_some();
    if let Some(problem) = &scan.support.problem {
        let _ = tx.send(Event::Log(
            id,
            format!("Frame {frame} PID support incomplete. {problem}"),
        ));
        publish_rows(
            tx,
            id,
            vec![Reading {
                source: None,
                request: format!("0200{frame:02X}"),
                name: super::freeze_frames::DISCOVERY.into(),
                value: "Unavailable".into(),
                unit: String::new(),
                raw: String::new(),
                numeric: None,
                status: format!("PID discovery incomplete. {problem}"),
                signal_failure: false,
            }],
        );
    }
    if !empty_sources.is_empty() && scan.known_sources().is_subset(&empty_sources) {
        return if incomplete {
            format!("ECU reported no freeze frame for frame {frame}. PID support check incomplete.")
        } else {
            format!("No freeze frame stored for frame {frame}.")
        };
    }

    let mut pids = scan.support.reported.clone();
    pids.retain(|pid| {
        *pid != 2
            && (!report_scan
                || scan
                    .pid_sources(*pid)
                    .is_some_and(|sources| !sources.is_disjoint(&data_sources)))
            && !(*pid <= scan.support.checked_through
                && scan
                    .pid_sources(*pid)
                    .is_some_and(|sources| sources.is_subset(&empty_sources)))
    });
    if incomplete {
        // Some ECUs answer common snapshot reads without supplying a usable bitmap.
        let fallback: &[u8] = if report_scan {
            &[5, 12, 13]
        } else {
            &[4, 5, 6, 7, 11, 12, 13, 15, 16, 17]
        };
        pids.extend(
            fallback
                .iter()
                .copied()
                .filter(|&pid| pid > scan.support.checked_through),
        );
    }

    let total = pids.len();
    for (index, pid) in pids.into_iter().enumerate() {
        if stopped(adapter) {
            break;
        }
        let query = freeze_frame_query(catalog, pid, frame);
        let _ = tx.send(Event::Progress(id, index, total, query.name.clone()));
        let mut rows = read_one(adapter, &query, id, false, cancel, tx);
        rows.retain(|r| {
            !empty_sources.contains(&r.source)
                && (!report_scan || r.source.is_none() || data_sources.contains(&r.source))
        });
        if !cancel.load(Ordering::Relaxed) {
            if let Some(sources) = scan.pid_sources(pid) {
                let expected: BTreeSet<_> = sources
                    .difference(&empty_sources)
                    .copied()
                    .filter(|source| {
                        source.is_some() && (!report_scan || data_sources.contains(source))
                    })
                    .collect();
                rows = live_reads::individual_rows(&query, &expected, rows);
            }
        }
        publish_rows(tx, id, rows);
        let _ = tx.send(Event::Progress(id, index + 1, total, String::new()));
    }
    if incomplete {
        format!("Frame {frame} PID support incomplete. See freeze-frame results.")
    } else if total == 0 {
        format!("No data PIDs were reported for frame {frame}. Check the trigger reply.")
    } else {
        format!("Freeze frame {frame} read. Check the ECU results.")
    }
}

fn complete_advertised_read(
    query: &Query,
    rows: Vec<Reading>,
    support: &SupportScan,
    observed: &BTreeSet<Option<u32>>,
) -> Vec<Reading> {
    let mut expected = observed.clone();
    if let Some(sources) = support.pid_sources(query.bytes[1]) {
        expected.extend(sources.iter().copied().filter(Option::is_some));
    }
    live_reads::individual_rows(query, &expected, rows)
}

fn read_monitors(
    adapter: &mut Link,
    id: u64,
    support: &mut SupportScan,
    responders: &mut ScanResponders,
    cancel: &Arc<AtomicBool>,
    tx: &mpsc::Sender<Event>,
) -> String {
    let stopped = |adapter: &Link| cancel.load(Ordering::Relaxed) || adapter.failure().is_some();
    let mut scan = support.refresh();
    while let Some(base) = scan.support.next_range {
        if stopped(adapter) {
            return String::new();
        }
        let _ = tx.send(Event::Log(
            id,
            format!("Request 06{base:02X} / Monitor support"),
        ));
        block_on(scan.step(adapter, 2000));
    }
    if stopped(adapter) {
        return String::new();
    }
    *support = scan;
    let _ = tx.send(Event::MonitorSupport(id, support.support.clone()));

    let total = support.support.reported.len();
    let mut failures = 0;
    for (index, &mid) in support.support.reported.iter().enumerate() {
        if stopped(adapter) {
            break;
        }
        let query = Query {
            bytes: vec![6, mid],
            target: None,
            name: format!("Monitor {mid:02X}"),
            signals: vec![],
            verified: false,
        };
        let _ = tx.send(Event::Progress(id, index, total, query.name.clone()));
        let mut rows = read_one(adapter, &query, id, false, cancel, tx);
        if stopped(adapter) {
            break;
        }
        if let Some(observed) = observe_scan_reply(&query, &rows, responders) {
            rows = complete_advertised_read(&query, rows, support, observed);
        }
        failures += rows.iter().filter(|row| unavailable(row)).count();
        publish_rows(tx, id, rows);
        let _ = tx.send(Event::Progress(id, index + 1, total, String::new()));
    }

    if !stopped(adapter) {
        let status = if support.support.problem.is_some() {
            "Monitor discovery incomplete"
        } else {
            "ECU did not advertise this monitor"
        };
        let mut skipped = Vec::new();
        for (&[mode, mid], sources) in responders.iter() {
            if mode != 6 || support.support.reported.contains(&mid) {
                continue;
            }
            for &source in sources {
                skipped.push(Reading {
                    source,
                    request: format!("06{mid:02X}"),
                    name: format!("Monitor {mid:02X}"),
                    value: "Unavailable".into(),
                    unit: String::new(),
                    raw: String::new(),
                    numeric: None,
                    status: status.into(),
                    signal_failure: false,
                });
            }
        }
        failures += skipped.len();
        publish_rows(tx, id, skipped);
    }
    if let Some(problem) = &support.support.problem {
        let _ = tx.send(Event::Log(
            id,
            format!("Monitor discovery incomplete. {problem}"),
        ));
    }
    match (total, support.support.problem.is_some(), failures) {
        (0, true, _) => {
            "Could not discover monitor IDs. Enter a known ID to read it directly.".into()
        }
        (0, false, _) => "No monitor IDs were reported.".into(),
        (_, true, _) => {
            format!("Requested {total} reported monitor IDs. Discovery was incomplete.")
        }
        (_, false, 0) => format!("Read {total} monitor IDs. Review the ECU results."),
        (_, false, _) => format!("Requested {total} monitor IDs. Check ECU read issues."),
    }
}

fn read_vehicle_information(
    adapter: &mut Link,
    id: u64,
    support: &mut SupportScan,
    responders: &mut ScanResponders,
    cancel: &Arc<AtomicBool>,
    tx: &mpsc::Sender<Event>,
) -> String {
    let stopped = |adapter: &Link| cancel.load(Ordering::Relaxed) || adapter.failure().is_some();
    let can = adapter.connection_info().protocol.contains("CAN");
    let mut scan = support.refresh();
    while let Some(base) = scan.support.next_range {
        if stopped(adapter) {
            return String::new();
        }
        let _ = tx.send(Event::Log(
            id,
            format!("Request 09{base:02X} / Vehicle information support"),
        ));
        block_on(scan.step(adapter, 2000));
    }
    if stopped(adapter) {
        return String::new();
    }
    *support = scan;
    let _ = tx.send(Event::VehicleInfoSupport(id, support.support.clone()));
    if let Some(problem) = &support.support.problem {
        let _ = tx.send(Event::Log(
            id,
            format!("Vehicle information discovery incomplete. {problem}"),
        ));
    }
    let mut pids = support.support.reported.clone();
    pids.extend(
        responders.iter().filter_map(|(&[mode, pid], sources)| {
            (mode == 9 && !sources.is_empty()).then_some(pid)
        }),
    );
    if support.support.problem.is_some() {
        // Common identifiers can still reply when an ECU supplies no usable map.
        pids.extend([2, 4, 6, 10]);
    }
    pids.retain(|&pid| diagnostics::information_name(pid).is_some());

    let total = pids.len();
    let mut failures = 0;
    for (index, pid) in pids.into_iter().enumerate() {
        if stopped(adapter) {
            break;
        }
        let name = diagnostics::information_name(pid).unwrap();
        let _ = tx.send(Event::Progress(id, index, total, name.into()));
        let count_pid = match (can, pid) {
            (false, 4 | 6 | 8) => Some(pid - 1),
            (false, 0x0b) => Some(7),
            _ => None,
        };
        let count_rows = if let Some(count_pid) = count_pid {
            // A legacy reply can end on a valid boundary after dropping
            // some of the data the ECU advertised.
            let count_query = vehicle_info_query(count_pid, format!("{name} message count"));
            read_one(adapter, &count_query, id, false, cancel, tx)
        } else {
            Vec::new()
        };
        if stopped(adapter) {
            break;
        }
        let query = vehicle_info_query(pid, name.into());
        let mut rows = read_one(adapter, &query, id, false, cancel, tx);
        if !stopped(adapter) {
            if count_pid.is_some() {
                validate_legacy_information(&mut rows, &count_rows);
            }
            if let Some(observed) = observe_scan_reply(&query, &rows, responders) {
                rows = complete_advertised_read(&query, rows, support, observed);
            }
        }
        failures += rows.iter().filter(|row| unavailable(row)).count();
        publish_rows(tx, id, rows);
        let _ = tx.send(Event::Progress(id, index + 1, total, String::new()));
    }
    match (total, failures) {
        (0, _) => {
            "No recognized vehicle information was reported. You can request the VIN directly."
                .into()
        }
        (_, 0) => "Vehicle information read.".into(),
        _ => "Vehicle information read. Check ECU read issues.".into(),
    }
}

fn vehicle_info_query(pid: u8, name: String) -> Query {
    Query {
        bytes: vec![9, pid],
        target: None,
        name,
        signals: vec![],
        verified: false,
    }
}

fn validate_legacy_information(rows: &mut [Reading], counts: &[Reading]) {
    for row in rows.iter_mut().filter(|row| !unavailable(row)) {
        let announced = counts
            .iter()
            .find(|count| row.source == count.source && !unavailable(count))
            .and_then(|count| count.value.parse::<usize>().ok());
        if let Some(announced) = announced {
            // The normalized payload holds a three-byte header and four bytes per packet.
            let received = row.raw.len().saturating_sub(6) / 8;
            if announced != received {
                row.value = "Unavailable".into();
                row.numeric = None;
                row.status = format!("ECU announced {announced} messages. Received {received}.");
            }
        } else {
            row.status = if matches!(row.request.as_str(), "0908" | "090B") {
                "Completions / conditions encountered. Legacy message count unavailable."
            } else {
                "Legacy message count unavailable"
            }
            .into();
        }
    }
}

fn run(
    rx: mpsc::Receiver<Command>,
    tx: mpsc::Sender<Event>,
    discover: impl Fn() -> Vec<DriverInfo>,
    runtime: DriverRuntime,
    mut selected_driver: Option<PathBuf>,
    mut selected_elm: Option<ElmConnection>,
) {
    let mut link: Option<Link> = None;
    let mut vehicle = VehicleSession::default();
    let mut idle_voltage = Instant::now() + VOLTAGE_INTERVAL;
    let mut voltage_driver = None::<PathBuf>;
    let mut elm_voltage = None::<(DriverProcess, ElmEndpoint)>;
    let mut last_elm_baud = None;
    let mut kline_retry_at = Instant::now();
    let mut last_connection = None::<LastConnection>;
    while !runtime.shutdown.load(Ordering::Relaxed) {
        let activity = runtime.activity.load(Ordering::Relaxed);
        let received = match &link {
            // Only start an advertised continuation when the command queue is empty.
            Some(Link::Physical(_)) if vehicle.pid_scan.support.next_range.is_some() => {
                rx.recv_timeout(Duration::ZERO)
            }
            Some(Link::Physical(physical)) => rx.recv_timeout(
                physical
                    .next_voltage
                    .saturating_duration_since(Instant::now()),
            ),
            Some(Link::Demo(_)) | None => {
                rx.recv_timeout(idle_voltage.saturating_duration_since(Instant::now()))
            }
        };
        let command = match received {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match &mut link {
                    Some(adapter @ Link::Physical(_)) => {
                        if let Link::Physical(physical) = adapter {
                            if Instant::now() >= physical.next_voltage {
                                physical.refresh_voltage();
                            } else if vehicle.pid_scan.support.next_range.is_some() {
                                let id = physical.id;
                                block_on(vehicle.pid_scan.step(adapter, AUTOMATIC_PID_TIMEOUT_MS));
                                adapter.include_reported_pids(&vehicle.pid_scan.support);
                                let _ = tx.send(Event::LivePidSupport(
                                    id,
                                    vehicle.pid_scan.support.clone(),
                                ));
                            }
                        }
                    }
                    Some(Link::Demo(_)) | None => {
                        // Prioritise an incoming Connect command over a stalled passive poll.
                        let interrupted = || runtime.activity.load(Ordering::Relaxed) != activity;
                        let result = if let Some(connection) = &selected_elm {
                            sample_idle_elm(
                                &runtime,
                                connection,
                                &mut elm_voltage,
                                &mut last_elm_baud,
                                interrupted,
                            )
                        } else {
                            sample_idle_voltage(
                                &runtime,
                                &discover,
                                selected_driver.as_deref(),
                                voltage_driver.as_deref(),
                                interrupted,
                            )
                        };
                        let delay = if result.is_err()
                            || selected_elm.is_some() && matches!(result, Ok(None))
                        {
                            VOLTAGE_BACKOFF
                        } else {
                            VOLTAGE_INTERVAL
                        };
                        idle_voltage = Instant::now() + delay;
                        if !interrupted() {
                            let value = result.ok().flatten();
                            if let Some(VoltageSample {
                                adapter: VoltageAdapter::J2534(driver),
                                ..
                            }) = &value
                            {
                                voltage_driver = Some(driver.path.clone());
                            }
                            let _ = tx.send(Event::Voltage(value));
                        }
                    }
                }
                if let Some((id, reason)) = link.as_ref().and_then(Link::failure) {
                    close_link(&mut link, &mut kline_retry_at);
                    idle_voltage = Instant::now() + VOLTAGE_INTERVAL;
                    let _ = tx.send(Event::LinkLost(id, reason));
                }
                continue;
            }
        };
        match command {
            Command::Shutdown => break,
            Command::SelectDriver(path) => {
                if !matches!(link, Some(Link::Physical(_))) {
                    selected_driver = path;
                    voltage_driver = None;
                    idle_voltage = Instant::now();
                    let _ = tx.send(Event::Voltage(None));
                }
            }
            Command::SelectElm(connection) => {
                if !matches!(link, Some(Link::Physical(_))) && selected_elm != connection {
                    selected_elm = connection;
                    elm_voltage = None;
                    last_elm_baud = None;
                    idle_voltage = Instant::now();
                    let _ = tx.send(Event::Voltage(None));
                }
            }
            Command::Disconnect { id } => {
                close_link(&mut link, &mut kline_retry_at);
                idle_voltage = Instant::now() + VOLTAGE_INTERVAL;
                let _ = tx.send(Event::Disconnected(id));
            }
            Command::Demo { id } => {
                close_link(&mut link, &mut kline_retry_at);
                let mut adapter = Link::Demo(DemoAdapter::new());
                vehicle = VehicleSession::default();
                while vehicle.pid_scan.support.next_range.is_some() {
                    block_on(
                        vehicle
                            .pid_scan
                            .step(&mut adapter, AUTOMATIC_PID_TIMEOUT_MS),
                    );
                }
                link = Some(adapter);
                let _ = tx.send(Event::Connected {
                    id,
                    driver: None,
                    protocol: "DEMO CAN 11-bit / 500 kbit/s".into(),
                    demo: true,
                    pid_support: vehicle.pid_scan.support.clone(),
                });
            }
            Command::Connect {
                id,
                protocol,
                ecu,
                cancel,
            } => {
                if selected_elm.take().is_some() {
                    last_elm_baud = None;
                    let _ = tx.send(Event::Voltage(None));
                }
                elm_voltage = None;
                close_link(&mut link, &mut kline_retry_at);
                let saved = runtime
                    .connection_cache
                    .as_deref()
                    .and_then(|path| load_json::<LastConnection>(path).ok());
                let remembered = saved.as_ref().or(last_connection.as_ref());
                let drivers = driver_candidates(
                    discover(),
                    selected_driver.as_deref(),
                    remembered.map(|connection| connection.driver.as_path()),
                );
                let result = if cancel.load(Ordering::Relaxed) {
                    Err(CANCELLED.into())
                } else if drivers.is_empty() {
                    Err(if selected_driver.is_some() {
                        "The selected J2534 driver is no longer listed. Choose an installed driver under Connection."
                            .into()
                    } else {
                        "No J2534 04.04 drivers found. Install a vendor driver that matches the app architecture."
                            .into()
                    })
                } else {
                    connect_vehicle(
                        id,
                        drivers,
                        protocol,
                        ecu,
                        &cancel,
                        &tx,
                        &runtime,
                        &mut voltage_driver,
                        &mut kline_retry_at,
                        remembered,
                    )
                };
                idle_voltage = Instant::now() + VOLTAGE_INTERVAL;
                if cancel.load(Ordering::Relaxed) {
                    if let Ok((physical, _)) = result {
                        link = Some(Link::Physical(Box::new(physical)));
                        close_link(&mut link, &mut kline_retry_at);
                    }
                    let _ = tx.send(Event::Failed(id, CANCELLED.into()));
                    continue;
                }
                match result {
                    Ok((physical, replies)) => {
                        let VoltageAdapter::J2534(driver) = &physical.adapter else {
                            unreachable!("a J2534 connection opened a J2534 driver");
                        };
                        let remember = ecu.is_none().then(|| LastConnection {
                            driver: driver.path.clone(),
                            protocol: physical.protocol,
                        });
                        if voltage_driver
                            .as_ref()
                            .is_some_and(|path| !same_driver_path(path, &driver.path))
                        {
                            let _ = tx.send(Event::Voltage(None));
                        }
                        voltage_driver = Some(driver.path.clone());
                        link = Some(vehicle.connected(physical, replies, &tx));
                        // Update the hint only after a newly verified session is accepted.
                        if let Some(remember) = remember {
                            if saved.as_ref() != Some(&remember) {
                                if let Some(path) = &runtime.connection_cache {
                                    if let Err(error) = save_json(path, &remember) {
                                        let _ = tx.send(Event::Log(
                                            id,
                                            format!(
                                                "Could not save the last verified protocol. {error}"
                                            ),
                                        ));
                                    }
                                }
                            }
                            last_connection = Some(remember);
                        }
                    }
                    Err(error) => {
                        // The next passive poll can still read pin 16 if the cable is present.
                        let _ = tx.send(Event::Failed(id, error));
                    }
                }
            }
            Command::ConnectElm {
                id,
                connection,
                cancel,
            } => {
                close_link(&mut link, &mut kline_retry_at);
                if selected_elm.as_ref() != Some(&connection) {
                    elm_voltage = None;
                    last_elm_baud = None;
                    let _ = tx.send(Event::Voltage(None));
                    selected_elm = Some(connection.clone());
                }
                let result = connect_elm(
                    id,
                    &connection,
                    last_elm_baud,
                    elm_voltage.take(),
                    &cancel,
                    &tx,
                    &runtime,
                );
                idle_voltage = Instant::now() + VOLTAGE_INTERVAL;
                if cancel.load(Ordering::Relaxed) {
                    if let Ok((physical, _, _)) = result {
                        link = Some(Link::Physical(Box::new(physical)));
                        close_link(&mut link, &mut kline_retry_at);
                    }
                    let _ = tx.send(Event::Failed(id, CANCELLED.into()));
                    continue;
                }
                match result {
                    Ok((physical, replies, baud)) => {
                        last_elm_baud = baud;
                        link = Some(vehicle.connected(physical, replies, &tx));
                    }
                    Err(error) => {
                        let _ = tx.send(Event::Failed(id, error));
                    }
                }
            }
            Command::Read {
                id,
                mut queries,
                discovery,
                mode,
                cancel,
            } => {
                let Some(adapter) = link.as_mut() else {
                    let _ = tx.send(Event::Failed(
                        id,
                        "Adapter disconnected. Click Connect to try again.".into(),
                    ));
                    continue;
                };
                let VehicleSession {
                    pid_scan,
                    monitor_scan,
                    oxygen,
                    info_scan,
                    scan_responders,
                    grouping_allowed,
                } = &mut vehicle;
                let identify = matches!(mode, ReadMode::Identify);
                if identify && !matches!(adapter, Link::Physical(physical) if physical.id == id) {
                    continue;
                }
                let polling = mode.is_poll();
                if let Link::Physical(physical) = adapter {
                    // A foreground action can stop polling between requests without
                    // interrupting a J2534 call and losing the diagnostic channel.
                    physical.cancel = (!polling).then(|| cancel.clone());
                }
                let control = matches!(mode, ReadMode::Control);
                let emissions_scan = matches!(mode, ReadMode::EmissionsScan);
                let mut completion = if matches!(mode, ReadMode::Live { .. }) {
                    "Live data read once.".to_owned()
                } else {
                    "Operation finished. Check the ECU results.".to_owned()
                };
                if identify {
                    let query = vehicle_info_query(2, "VIN".into());
                    let mut rows = read_one(adapter, &query, id, false, &cancel, &tx);
                    if !cancel.load(Ordering::Relaxed) && adapter.failure().is_none() {
                        if let Some(observed) = observe_scan_reply(&query, &rows, scan_responders) {
                            rows = complete_advertised_read(&query, rows, info_scan, observed);
                        }
                        let _ = tx.send(Event::InitialVin(id, rows));
                    }
                } else if matches!(mode, ReadMode::CheckVehicleReply) {
                    if let Link::Physical(physical) = adapter {
                        let protocol = physical.protocol;
                        let check = ReplyCheck::from_result(
                            physical.read(None, &[1, 0], 2000),
                            pid_scan.known_sources(),
                            protocol,
                        );
                        completion = check.outcome.label().into();
                        if !cancel.load(Ordering::Relaxed) && physical.driver.failure().is_none() {
                            let _ = tx.send(Event::VehicleReplyChecked {
                                connection: physical.id,
                                operation: id,
                                check,
                            });
                        }
                    } else {
                        completion = "Connect to a vehicle to check its reply.".into();
                    }
                } else if matches!(discovery, Some((1, _))) {
                    let mut refresh = pid_scan.refresh();
                    while refresh.support.next_range.is_some()
                        && !cancel.load(Ordering::Relaxed)
                        && adapter.failure().is_none()
                    {
                        block_on(refresh.step(adapter, 2000));
                    }
                    if !cancel.load(Ordering::Relaxed) {
                        *pid_scan = refresh;
                        adapter.include_reported_pids(&pid_scan.support);
                        completion = if let Some(problem) = &pid_scan.support.problem {
                            format!("PID support check incomplete. {problem}")
                        } else {
                            "PID support updated.".into()
                        };
                        let _ = tx.send(Event::LivePidSupport(id, pid_scan.support.clone()));
                    }
                } else if let Some(discovery) = discovery {
                    let (inner_tx, inner_rx) = mpsc::channel();
                    block_on(diagnostics::execute(
                        adapter,
                        Job {
                            id,
                            queries: vec![],
                            discovery: Some(discovery),
                            cancelled: cancel.clone(),
                            allow_control: false,
                        },
                        &inner_tx,
                    ));
                    for update in inner_rx.try_iter() {
                        if let crate::app::ObdEvent::Diagnostics(u) = update {
                            match u {
                                Update::Done(_, message) => completion = message,
                                Update::Support(_, _, pids) => {
                                    let _ = tx.send(Event::Support(id, discovery, pids));
                                }
                                other => {
                                    let _ = tx.send(Event::Update(other));
                                }
                            }
                        }
                    }
                } else if let ReadMode::FreezeFrame(frame) = mode {
                    let catalog = if queries.is_empty() {
                        super::generic_catalog()
                    } else {
                        &queries
                    };
                    completion = read_freeze_frame(
                        adapter,
                        id,
                        frame,
                        catalog,
                        &cancel,
                        &tx,
                        false,
                        scan_responders,
                    );
                } else if matches!(mode, ReadMode::MonitorSweep) {
                    completion =
                        read_monitors(adapter, id, monitor_scan, scan_responders, &cancel, &tx);
                } else if matches!(mode, ReadMode::OxygenSweep | ReadMode::OxygenTest { .. }) {
                    completion = oxygen.run(adapter, id, mode, pid_scan, &cancel, &tx);
                } else if matches!(mode, ReadMode::VehicleInformation) {
                    completion = read_vehicle_information(
                        adapter,
                        id,
                        info_scan,
                        scan_responders,
                        &cancel,
                        &tx,
                    );
                } else {
                    let can = adapter.connection_info().protocol.contains("CAN");
                    let legacy_codes = matches!(mode, ReadMode::Scan | ReadMode::EmissionsScan)
                        && prepare_legacy_code_scan(&mut queries, can);
                    let mut confirmed_counts = BTreeMap::new();
                    let total = queries.len();
                    let mut index = 0;
                    let live_grouping = mode.live_grouping();
                    let can_group = can && live_grouping == Some(true);
                    while index < total
                        && !cancel.load(Ordering::Relaxed)
                        && adapter.failure().is_none()
                    {
                        let group = if can_group && *grouping_allowed {
                            live_reads::Group::new(&queries[index..], pid_scan)
                        } else {
                            None
                        };
                        if let Some(mut group) = group {
                            group.include_observed(scan_responders);
                            let label = format!("Live data ({} PIDs)", group.pids.len());
                            if !polling {
                                let _ = tx.send(Event::Progress(id, index, total, label.clone()));
                            }
                            let _ = tx.send(Event::Log(
                                id,
                                format!("Request {} / {label}", encode_hex(&group.bytes)),
                            ));
                            if !block_on(group.read(adapter))
                                && !cancel.load(Ordering::Relaxed)
                                && adapter.failure().is_none()
                            {
                                *grouping_allowed = false;
                                let _ = tx.send(Event::LiveGroupingFallback(id));
                            }
                            let group_at = Instant::now();
                            for pid in group.pids {
                                let single_rows = (!pid.complete()
                                    && !cancel.load(Ordering::Relaxed)
                                    && adapter.failure().is_none())
                                .then(|| read_one(adapter, pid.query, id, false, &cancel, &tx));
                                let query = pid.query;
                                let sampled_at =
                                    single_rows.as_ref().map_or(group_at, |_| Instant::now());
                                let rows = pid.into_rows(single_rows);
                                let _ = observe_scan_reply(query, &rows, scan_responders);
                                publish_read(&tx, id, rows, Some(sampled_at));
                                index += 1;
                                if !polling {
                                    let _ =
                                        tx.send(Event::Progress(id, index, total, String::new()));
                                }
                            }
                        } else {
                            let query = &queries[index];
                            if !polling {
                                let _ =
                                    tx.send(Event::Progress(id, index, total, query.name.clone()));
                            }
                            let mut rows = if query.bytes.first() == Some(&5) && !can {
                                oxygen.read_request(adapter, id, query, &cancel, &tx, false)
                            } else {
                                read_one(adapter, query, id, control, &cancel, &tx)
                            };
                            let sampled_at = live_grouping.is_some().then(Instant::now);
                            let observed = observe_scan_reply(query, &rows, scan_responders);
                            if !cancel.load(Ordering::Relaxed) {
                                match mode {
                                    ReadMode::Diagnostic
                                        if matches!(query.bytes.as_slice(), [6 | 9, _]) =>
                                    {
                                        if let Some(observed) = observed {
                                            let support: &SupportScan = if query.bytes[0] == 6 {
                                                monitor_scan
                                            } else {
                                                info_scan
                                            };
                                            rows = complete_advertised_read(
                                                query, rows, support, observed,
                                            );
                                        }
                                    }
                                    ReadMode::Live { .. } | ReadMode::Poll { .. } => {
                                        if let Some(observed) = observed {
                                            rows = complete_advertised_read(
                                                query, rows, pid_scan, observed,
                                            );
                                        }
                                    }
                                    ReadMode::Scan | ReadMode::EmissionsScan => {
                                        if let Some(observed) = observed {
                                            match query.bytes.as_slice() {
                                                [1, pid] => {
                                                    if *pid == 1 {
                                                        confirmed_counts =
                                                            codes::confirmed_counts(&rows);
                                                    }
                                                    rows = complete_advertised_read(
                                                        query, rows, pid_scan, observed,
                                                    );
                                                }
                                                [9, _] => {
                                                    rows = complete_advertised_read(
                                                        query, rows, info_scan, observed,
                                                    );
                                                }
                                                [3] if adapter.failure().is_none() => {
                                                    let mut expected = observed.clone();
                                                    expected
                                                        .extend(confirmed_counts.keys().copied());
                                                    if legacy_codes {
                                                        codes::complete_legacy_stored(
                                                            query,
                                                            &confirmed_counts,
                                                            &expected,
                                                            &mut rows,
                                                        );
                                                    }
                                                    rows = live_reads::individual_rows(
                                                        query, &expected, rows,
                                                    );
                                                }
                                                _ => {
                                                    rows = live_reads::individual_rows(
                                                        query, observed, rows,
                                                    )
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            publish_read(&tx, id, rows, sampled_at);
                            index += 1;
                            if !polling {
                                let _ = tx.send(Event::Progress(id, index, total, String::new()));
                            }
                        }
                    }
                    if emissions_scan
                        && !cancel.load(Ordering::Relaxed)
                        && adapter.failure().is_none()
                    {
                        let _ = tx.send(Event::Progress(id, 0, 0, "Reading freeze frame".into()));
                        let freeze = read_freeze_frame(
                            adapter,
                            id,
                            0,
                            super::generic_catalog(),
                            &cancel,
                            &tx,
                            true,
                            scan_responders,
                        );
                        if !freeze.is_empty() {
                            let _ = tx.send(Event::Log(id, freeze));
                        }
                        completion =
                            "Emissions scan finished. Review the results or print the report."
                                .into();
                    }
                }
                if let Some((id, reason)) = adapter.failure() {
                    close_link(&mut link, &mut kline_retry_at);
                    idle_voltage = Instant::now() + VOLTAGE_INTERVAL;
                    let _ = tx.send(Event::Voltage(None));
                    let reason = if control {
                        format!(
                            "{reason}. The ECU may have acted. Read vehicle data again before repeating the command"
                        )
                    } else {
                        reason
                    };
                    let _ = tx.send(Event::LinkLost(id, reason));
                    continue;
                }
                if let Link::Physical(physical) = adapter {
                    physical.cancel = None;
                }
                if identify {
                    continue;
                }
                let _ = tx.send(Event::Finished(
                    id,
                    if cancel.load(Ordering::Relaxed) {
                        "Operation stopped.".into()
                    } else {
                        completion
                    },
                ));
            }
        }
    }
}

/// Deterministic local simulator: this type never opens a driver or socket.
pub struct DemoAdapter {
    info: ConnectionInfo,
    started: Instant,
    cleared: bool,
}

impl Default for DemoAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoAdapter {
    pub fn new() -> Self {
        Self {
            info: ConnectionInfo {
                port: "Offline training".into(),
                baud: 500000,
                protocol: "DEMO CAN".into(),
                elm_version: "Local simulator".into(),
                voltage: Some("13.8V".into()),
            },
            started: Instant::now(),
            cleared: false,
        }
    }
    fn reply(&mut self, bytes: &[u8], source: u32) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        if bytes.is_empty() {
            return Err(Elm327Error::ProtocolError("Empty request".into()));
        }
        let mode = bytes[0];
        if mode == 1 && (3..=7).contains(&bytes.len()) {
            let mut grouped = vec![0x41];
            for &pid in &bytes[1..] {
                for reply in self.reply(&[1, pid], source)? {
                    if let Some(record) = reply.payload.strip_prefix(&[0x41]) {
                        grouped.extend_from_slice(record);
                    }
                }
            }
            if grouped.len() == 1 {
                grouped = vec![0x7f, 1, 0x12];
            }
            return Ok(vec![DiagnosticResponse::new(Some(source), grouped)]);
        }
        let pid = bytes.get(1).copied().unwrap_or(0);
        let frame = bytes.get(2).copied().unwrap_or(0);
        let frame_empty = mode == 2 && (self.cleared || frame != 0);
        let mut p = vec![mode.wrapping_add(0x40)];
        let t = self.started.elapsed().as_secs_f64();
        if matches!(mode, 1 | 2 | 6 | 9) && bytes.len() > 1 && pid % 32 == 0 {
            let mut supported: Vec<u8> = match mode {
                1 | 2 => vec![
                    1, 4, 5, 6, 7, 11, 12, 13, 15, 16, 17, 31, 33, 47, 49, 66, 70, 92,
                ],
                6 => vec![1, 2, 0x21],
                9 => vec![2, 4, 6, 8, 10, 13],
                _ => vec![],
            };
            if mode == 1 {
                supported.push(0x41);
            } else if frame_empty {
                supported = vec![2];
            } else if mode == 2 {
                supported.extend([2, 0x6f]);
            }
            let mut bitmap = 0u32;
            for value in supported
                .iter()
                .copied()
                .filter(|v| *v > pid && u16::from(*v) <= u16::from(pid) + 31)
            {
                bitmap |= 1 << (32 - u32::from(value - pid));
            }
            if supported
                .iter()
                .any(|v| u16::from(*v) >= u16::from(pid) + 32)
            {
                bitmap |= 1;
            }
            p.push(pid);
            if mode == 2 {
                p.push(frame);
            }
            p.extend_from_slice(&bitmap.to_be_bytes());
        } else if matches!(mode, 1 | 2) {
            p.push(pid);
            if mode == 2 {
                p.push(frame);
                if frame_empty && pid != 2 {
                    return Err(Elm327Error::Timeout("No stored freeze frame".into()));
                }
            }
            let rpm = if mode == 2 {
                8320u16
            } else {
                ((780.0 + (t * 0.7).sin() * 110.0) * 4.0) as u16
            };
            let maf = if mode == 2 {
                375u16
            } else {
                ((3.0 + (t * 0.6).sin() * 0.4) * 100.0) as u16
            };
            let data = match pid {
                1 => vec![
                    if self.cleared { 0 } else { 0x81 },
                    7,
                    0x65,
                    if self.cleared { 0x65 } else { 4 },
                ],
                2 if self.cleared || frame_empty => vec![0, 0],
                2 => vec![1, 0x33],
                4 => vec![48],
                5 => vec![128],
                6 => vec![134],
                7 => vec![130],
                11 => vec![36],
                12 => rpm.to_be_bytes().to_vec(),
                13 => vec![0],
                15 => vec![66],
                16 => maf.to_be_bytes().to_vec(),
                17 => vec![29],
                31 => vec![1, 45],
                33 => vec![0, 20],
                47 => vec![165],
                49 => vec![0, 30],
                0x41 if mode == 1 => vec![0, 7, 0x21, if self.cleared { 0x21 } else { 0x20 }],
                66 => vec![0x35, 0xe8],
                70 => vec![61],
                92 => vec![133],
                0x6f if mode == 2 => vec![0x01, 0x80, 0x32],
                _ => {
                    return Ok(vec![DiagnosticResponse::new(
                        Some(source),
                        vec![0x7f, mode, 0x12],
                    )]);
                }
            };
            p.extend(data);
        } else if matches!(mode, 3 | 7 | 10) {
            if !self.cleared && mode == 3 || mode == 10 {
                p.extend([1, 1, 0x33]);
            } else {
                p.push(0);
            }
        } else if mode == 4 {
            self.cleared = true;
        } else if mode == 9 {
            p.extend([pid, 1]);
            match pid {
                2 => p.extend_from_slice(b"WDD00000000000000"),
                4 => p.extend_from_slice(b"DEMO-CALIBRATION"),
                6 => p.extend([0x12, 0x34, 0x56, 0x78]),
                8 => {
                    p[2] = 16;
                    for count in [86u16, 121, 42, 48, 0, 0, 56, 60, 0, 0, 71, 80, 0, 0, 23, 42] {
                        p.extend_from_slice(&count.to_be_bytes());
                    }
                }
                10 => p.extend_from_slice(b"ECM\0-EngineControl\0\0"),
                13 => p.extend_from_slice(b"\0\0\0\0\0\0\0\0DEMO12345"),
                _ => p = vec![0x7f, 9, 0x31],
            }
        } else if mode == 6 {
            if ![1, 2, 0x21].contains(&pid) {
                return Ok(vec![DiagnosticResponse::new(
                    Some(source),
                    vec![0x7f, 6, 0x31],
                )]);
            }
            p.extend([pid, 1, 1]);
            p.extend(match (pid, self.cleared) {
                (_, true) | (2, _) => [0; 6],
                (0x21, _) => [0, 220, 0, 0, 0, 200],
                _ => [0, 80, 0, 0, 0, 200],
            });
        } else {
            p = vec![0x7f, mode, 0x11];
        }
        Ok(vec![DiagnosticResponse::new(Some(source), p)])
    }
}
impl DiagnosticAdapter for DemoAdapter {
    async fn request(
        &mut self,
        bytes: &[u8],
        _: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        self.reply(bytes, 0x7e8)
    }
    async fn request_to(
        &mut self,
        target: DiagnosticTarget,
        bytes: &[u8],
        _: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        self.reply(bytes, target.response)
    }
    fn connection_info(&self) -> &ConnectionInfo {
        &self.info
    }
    async fn voltage(&mut self) -> Result<String, Elm327Error> {
        Ok("13.8 V (simulated)".into())
    }
    async fn delay(&mut self, _: u64) {}
}
