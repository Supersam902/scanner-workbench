use crate::adapter::{decode_hex, encode_hex};
use crate::diagnostics::{MONITOR_NOT_COMPLETED, MONITOR_UNKNOWN_SCALING, Query, Reading};
use crate::j2534::DriverInfo;
pub use crate::obd::valid_vin;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub use super::elm::ElmConnection;
pub use super::emissions_scan::Scan as EmissionsScan;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanEvent {
    Started,
    Finished,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearScanStage {
    Before,
    After,
}

impl ClearScanStage {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Before => "Before clear",
            Self::After => "After clear request",
        }
    }
}

pub const MAX_CAPTURE_ROWS: usize = 50_000;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CAPTURE_DURATION_MS: u64 = 7 * 24 * 3600 * 1000;
pub(super) const MAX_RECORDING_MARKERS: usize = 500;
pub(super) const MAX_MARKER_NOTE_CHARS: usize = 120;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vehicle {
    pub make: String,
    pub model: String,
    pub year: String,
    pub vin: String,
    pub registration: String,
    pub odometer: String,
    pub notes: String,
    pub experimental: bool,
}
impl Vehicle {
    pub fn label(&self) -> String {
        let label = format!("{} {} {}", self.year, self.make, self.model)
            .trim()
            .to_owned();
        if label.is_empty() {
            "Unidentified vehicle".into()
        } else {
            label
        }
    }
    pub fn has_identity(&self) -> bool {
        valid_vin(&self.vin) || !self.registration.trim().is_empty()
    }
    pub fn identity(&self) -> String {
        if valid_vin(&self.vin) {
            self.vin.trim().to_ascii_uppercase()
        } else {
            format!(
                "{}|{}",
                self.label(),
                self.registration.trim().to_uppercase()
            )
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        let year = self
            .year
            .parse::<u16>()
            .map_err(|_| "Enter the vehicle model year")?;
        let current: u16 = chrono::Local::now()
            .format("%Y")
            .to_string()
            .parse()
            .unwrap_or(2026);
        let minimum = 1996;
        if !(minimum..=current + 1).contains(&year) {
            return Err(format!("Model year must be {minimum}-{}", current + 1));
        }
        if self.model.trim().is_empty() {
            return Err("Select a model or Generic OBD-II".into());
        }
        if !self.vin.trim().is_empty() && !valid_vin(&self.vin) {
            return Err("VIN must contain 17 letters/digits, excluding I, O and Q".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub garage: Vec<Vehicle>,
    pub favorites: BTreeSet<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub live_presets: BTreeMap<String, BTreeSet<String>>,
    pub last_vehicle: Option<Vehicle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_driver: Option<DriverInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elm_connection: Option<ElmConnection>,
}

impl Settings {
    pub(super) fn load(root: &Path) -> Result<Self, String> {
        match read_json(&root.join("settings.json")) {
            Ok(settings) => Ok(settings),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub(super) fn edit(root: &Path, edit: impl FnOnce(&mut Self)) -> Result<Self, String> {
        Self::try_edit(root, |settings| {
            edit(settings);
            Ok(())
        })
    }

    pub(super) fn try_edit(
        root: &Path,
        edit: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
        let _lock = lock_file(&root.join("settings.lock")).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                "Another scanner is saving preferences. Try again.".into()
            } else {
                error.to_string()
            }
        })?;
        let mut settings = Self::load(root)?;
        edit(&mut settings)?;
        save_json(&root.join("settings.json"), &settings)?;
        Ok(settings)
    }
}

#[cfg(windows)]
pub(super) fn lock_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) {
                io::ErrorKind::WouldBlock.into()
            } else {
                error
            }
        })
}

#[cfg(unix)]
pub(super) fn lock_file(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // The descriptor remains valid and the OS releases the lock when it closes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(file)
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> io::Result<T> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    if size > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "File exceeds 64 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "File exceeds 64 MiB",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn load_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, String> {
    read_json(path).map_err(|error| error.to_string())
}
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    write_json(path, value, true).map_err(|error| error.to_string())
}

pub(super) fn write_json<T: Serialize>(path: &Path, value: &T, replace: bool) -> io::Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Save exceeds 64 MiB",
        ));
    }
    write_file_with(path, replace, |file| file.write_all(&data))
}

pub(super) fn write_file(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    write_file_with(path, true, write)
}

fn write_file_with(
    path: &Path,
    replace: bool,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    #[cfg(windows)]
    let (parent, path) = {
        // Canonical paths keep the Win32 move used by the temporary file compatible with long paths.
        let parent = std::fs::canonicalize(parent)?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Expected a file name"))?;
        let path = parent.join(name);
        (parent, path)
    };
    #[cfg(windows)]
    let parent = parent.as_path();
    let (file, temporary) = tempfile::Builder::new()
        .prefix(".scanner-")
        .suffix(".tmp")
        .tempfile_in(parent)?
        .into_parts();
    let mut writer = BufWriter::new(file);
    write(&mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if replace {
        temporary.persist(path)?;
    } else {
        temporary.persist_noclobber(path)?;
    }
    Ok(())
}
pub fn data_directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ScannerWorkbench")
}
pub fn query_key(q: &Query) -> String {
    format!(
        "{}:{}",
        q.target
            .map(|t| format!("{:X}-{:X}", t.request, t.response))
            .unwrap_or_else(|| "OBD".into()),
        encode_hex(&q.bytes)
    )
}
pub fn scope(source: Option<u32>, request: &str) -> String {
    format!("{:X?}:{request}", source)
}
pub fn ecu_label(source: Option<u32>) -> String {
    source
        .map(|s| format!("{s:X}"))
        .unwrap_or_else(|| "Unspecified".into())
}
pub(super) fn unit_label(unit: &str) -> &str {
    match unit {
        "celsius" | "C" => "°C",
        "fahrenheit" => "°F",
        "percent" => "%",
        "gramsPerSecond" => "g/s",
        "kilometersPerHour" => "km/h",
        "kilopascal" => "kPa",
        "volts" => "V",
        "seconds" => "s",
        "liters" => "L",
        "rpm" => "RPM",
        _ => unit,
    }
}
pub fn row_key(r: &Reading) -> String {
    let request = scope(r.source, &r.request);
    // An emissions code list replaces the previous list for that ECU and service.
    // Its name can differ between a code scan and a manual request.
    if matches!(
        r.request.as_str(),
        "03" | "07" | "0A" | "0902" | "0904" | "0906" | "090A" | "090D" | "090F"
    ) {
        request
    } else {
        format!("{request}:{}", r.name)
    }
}
pub fn unavailable(r: &Reading) -> bool {
    matches!(r.value.as_str(), "Unavailable" | "Blocked")
}

pub(super) fn monitor_id(r: &Reading) -> Option<u8> {
    let id = r.request.strip_prefix("06").filter(|id| id.len() == 2)?;
    u8::from_str_radix(id, 16).ok().filter(|id| id % 32 != 0)
}

pub(super) fn is_monitor_result(r: &Reading) -> bool {
    monitor_id(r).is_some()
        || crate::diagnostics::oxygen_monitor::request_parts(&r.request).is_some()
}

pub(super) fn correct_recorded_monitor(reading: &Reading) -> Option<Reading> {
    // Older recordings classified all-zero CAN test records after scaling.
    if !matches!(reading.status.as_str(), "PASS" | MONITOR_UNKNOWN_SCALING)
        || !reading.raw.contains("000000000000")
    {
        return None;
    }
    let mid = monitor_id(reading)?;
    let (recorded_mid, test) = reading.name.strip_prefix("MID ")?.split_once(" / TID ")?;
    if u8::from_str_radix(recorded_mid, 16).ok()? != mid {
        return None;
    }
    let test = u8::from_str_radix(test, 16).ok()?;
    let payload = decode_hex(&reading.raw).ok()?;
    let records = payload.strip_prefix(&[0x46])?;
    if records.len() % 9 != 0
        || !records
            .chunks_exact(9)
            .any(|r| r[0] == mid && r[1] == test && r[3..] == [0; 6])
    {
        return None;
    }
    let mut corrected = reading.clone();
    corrected.value = MONITOR_NOT_COMPLETED.into();
    corrected.unit.clear();
    corrected.numeric = None;
    corrected.status = MONITOR_NOT_COMPLETED.into();
    Some(corrected)
}

fn failure_key(r: &Reading) -> String {
    if r.is_signal_failure() {
        row_key(r)
    } else {
        scope(r.source, &r.request)
    }
}

fn clear_outcome(r: &Reading) -> bool {
    r.request == "04" && r.value != "Blocked"
}

fn predates_clear(reading: &Stamped, clear_at: Option<u64>) -> bool {
    let request = reading.reading.request.as_str();
    let reset_sensitive = matches!(
        request,
        "03" | "07" | "0101" | "0102" | "0121" | "0130" | "0131" | "0141" | "014D" | "014E"
    ) || matches!(request.get(..2), Some("02" | "05" | "06"));
    reset_sensitive && clear_at.is_some_and(|at| reading.at_ms <= at)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sample {
    pub at_ms: u64,
    pub rows: Vec<Reading>,
    pub replace: Option<(Option<u32>, String)>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_requested: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub graph_break: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emissions_scan: Option<ScanEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_scan_stage: Option<ClearScanStage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingMarker {
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capture {
    pub version: u8,
    pub vehicle: Vehicle,
    pub demo: bool,
    pub started_ms: u64,
    pub samples: Vec<Sample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<RecordingMarker>,
}
impl Capture {
    pub fn new(vehicle: Vehicle, demo: bool, now: u64) -> Self {
        Self {
            version: 1,
            vehicle,
            demo,
            started_ms: now,
            samples: vec![],
            markers: vec![],
        }
    }
    pub fn row_count(&self) -> usize {
        self.samples.iter().map(|s| s.rows.len()).sum()
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.row_count() > MAX_CAPTURE_ROWS
            || self.samples.len() > MAX_CAPTURE_ROWS
            || self.markers.len() > MAX_RECORDING_MARKERS
        {
            return Err("Unsupported or oversized recording".into());
        }
        let mut previous = self.started_ms;
        let mut scanning = false;
        for sample in &self.samples {
            if sample.at_ms < previous
                || sample.at_ms.saturating_sub(self.started_ms) > MAX_CAPTURE_DURATION_MS
            {
                return Err("Recording timestamps are invalid".into());
            }
            previous = sample.at_ms;
            if sample.clear_scan_stage.is_some()
                && !matches!(sample.emissions_scan, Some(ScanEvent::Started))
            {
                return Err("Clear scan stage must be recorded at the start of a scan".into());
            }
            match sample.emissions_scan {
                Some(ScanEvent::Started) if !scanning && sample.rows.is_empty() => {
                    scanning = true;
                }
                Some(ScanEvent::Finished | ScanEvent::Interrupted)
                    if scanning && sample.rows.is_empty() =>
                {
                    scanning = false;
                }
                Some(_) => return Err("Invalid emissions scan sequence".into()),
                None => {}
            }
            if let Some((source, req)) = &sample.replace {
                if source.is_some_and(|s| s > 0x1fffffff)
                    || req != "1902FF"
                    || sample.rows.is_empty()
                    || sample
                        .rows
                        .iter()
                        .any(|r| r.source != *source || r.request != *req || unavailable(r))
                {
                    return Err("Invalid replacement batch".into());
                }
            }
            if sample.rows.iter().any(|row| !valid_recorded_reading(row)) {
                return Err("Invalid recording field".into());
            }
        }
        let mut previous = None;
        for marker in &self.markers {
            if marker.at_ms < self.started_ms
                || previous.is_some_and(|last| marker.at_ms <= last)
                || marker.at_ms - self.started_ms > MAX_CAPTURE_DURATION_MS
                || marker.note.chars().count() > MAX_MARKER_NOTE_CHARS
                || marker.note.chars().any(char::is_control)
            {
                return Err("Invalid recording marker".into());
            }
            previous = Some(marker.at_ms);
        }
        Ok(())
    }
    pub(super) fn add_marker(&mut self, at_ms: u64, note: &str) -> Result<(), &'static str> {
        if self.markers.len() >= MAX_RECORDING_MARKERS {
            return Err("Recording marker limit reached.");
        }
        let mut at_ms = at_ms.max(self.end_ms());
        if self
            .markers
            .last()
            .is_some_and(|marker| marker.at_ms == at_ms)
        {
            at_ms = at_ms
                .checked_add(1)
                .ok_or("Recording timeline limit reached.")?;
        }
        if at_ms - self.started_ms > MAX_CAPTURE_DURATION_MS {
            return Err("Recording timeline limit reached.");
        }
        let note = note
            .trim()
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_MARKER_NOTE_CHARS)
            .collect();
        self.markers.push(RecordingMarker { at_ms, note });
        Ok(())
    }
    pub fn replay(&self, until_ms: u64) -> Results {
        let mut results = Results::default();
        for sample in self.samples.iter().take_while(|s| s.at_ms <= until_ms) {
            results.apply(sample);
        }
        results
    }
    pub fn end_ms(&self) -> u64 {
        let last_sample = self.samples.last().map_or(self.started_ms, |s| s.at_ms);
        self.markers
            .last()
            .map_or(last_sample, |marker| marker.at_ms.max(last_sample))
    }
}

pub(super) fn valid_recorded_reading(row: &Reading) -> bool {
    row.source.is_none_or(|source| source <= 0x1fff_ffff)
        && row.name.len() <= 32768
        && row.value.len() <= 8192
        && row.raw.len() <= 32768
        && row.status.len() <= 8192
        && row.unit.len() <= 256
        && (2..=32).contains(&row.request.len())
        && row.request.len() % 2 == 0
        && row.request.bytes().all(|byte| byte.is_ascii_hexdigit())
        && row.numeric.is_none_or(f64::is_finite)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub reading: Reading,
    pub at_ms: u64,
}
#[derive(Clone, Debug)]
pub struct ValueState {
    pub latest: Stamped,
    pub history: VecDeque<GraphPoint>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphPoint {
    pub at_ms: u64,
    pub value: Option<f64>,
}

pub(super) type GraphObserver<'a> = dyn FnMut(&str, &Reading, GraphPoint) + 'a;

impl ValueState {
    fn push_point(&mut self, at_ms: u64, value: Option<f64>) -> Option<GraphPoint> {
        if value.is_none()
            && !self
                .history
                .back()
                .is_some_and(|point| point.value.is_some())
        {
            return None;
        }
        let point = GraphPoint { at_ms, value };
        self.history.push_back(point);
        if self.history.len() > 1200 {
            self.history.pop_front();
        }
        Some(point)
    }
}

pub(super) fn graph_request(request: &str) -> bool {
    matches!(request.get(..2), Some("01" | "21" | "22"))
}
#[derive(Clone, Debug, Default)]
pub struct Results {
    pub rows: BTreeMap<String, ValueState>,
    pub failures: BTreeMap<String, Stamped>,
    last_clear: Option<ClearAttempt>,
    last_sample_ms: Option<u64>,
    pub(super) emissions_scan: Option<EmissionsScan>,
}

#[derive(Clone, Debug)]
struct ClearAttempt {
    at_ms: u64,
    has_outcome: bool,
}

fn refresh_failure_applies(failure: &Stamped, value: &Stamped) -> bool {
    let failed = &failure.reading;
    let reading = &value.reading;
    if failed.request != reading.request {
        return false;
    }
    if failed.is_signal_failure() {
        return failed.source == reading.source
            && failed.name == reading.name
            && failure.at_ms >= value.at_ms;
    }
    (failed.source == reading.source && failure.at_ms >= value.at_ms)
        || (failed.source.is_none() && failure.at_ms > value.at_ms)
}

#[derive(Clone, Copy)]
pub(super) enum EvidenceIssue {
    BeforeClear,
    RefreshFailed,
}

fn matching_failures<'a>(
    failures: &'a BTreeMap<String, Stamped>,
    key: &str,
    reading: &Reading,
) -> impl Iterator<Item = &'a Stamped> {
    [
        failures.get(key),
        failures.get(&scope(reading.source, &reading.request)),
        failures.get(&scope(None, &reading.request)),
    ]
    .into_iter()
    .flatten()
}

impl Results {
    pub(super) fn reported_vin(&self) -> (Option<String>, bool) {
        let rows = self.rows.values().map(|value| &value.latest);
        let (vin, conflict) = vehicle_vins(rows.clone());
        let fresh = rows
            .filter(|row| row.reading.request == "0902")
            .any(|row| valid_vin(&row.reading.value) && !self.refresh_failed(row));
        (vin.filter(|_| fresh), conflict)
    }

    pub fn clear_attempt_ms(&self) -> Option<u64> {
        self.last_clear.as_ref().map(|clear| clear.at_ms)
    }

    pub fn predates_clear(&self, value: &Stamped) -> bool {
        predates_clear(value, self.clear_attempt_ms())
    }

    pub fn refresh_failed(&self, value: &Stamped) -> bool {
        let r = &value.reading;
        matching_failures(&self.failures, &row_key(r), r)
            .any(|failure| refresh_failure_applies(failure, value))
    }

    pub(super) fn evidence_issue(&self, value: &Stamped) -> Option<EvidenceIssue> {
        evidence_issue(value, self.predates_clear(value), || {
            self.refresh_failed(value)
        })
    }

    pub fn next_sample_ms(&self, now: u64) -> u64 {
        self.last_sample_ms
            .map_or(now, |last| now.max(last.saturating_add(1)))
    }

    pub fn apply(&mut self, sample: &Sample) {
        self.apply_with_graphs(sample, None);
    }

    pub(super) fn apply_with_graphs(
        &mut self,
        sample: &Sample,
        mut observe: Option<&mut GraphObserver<'_>>,
    ) {
        self.last_sample_ms = Some(sample.at_ms);
        EmissionsScan::apply(&mut self.emissions_scan, sample);
        if sample.graph_break {
            for (key, row) in &mut self.rows {
                let reading = &row.latest.reading;
                if graph_request(&reading.request)
                    && let Some(point) = row.push_point(sample.at_ms, None)
                    && let Some(observe) = observe.as_mut()
                {
                    observe(key, &row.latest.reading, point);
                }
            }
        }
        let has_outcome = sample.rows.iter().any(clear_outcome);
        // Older recordings contain only the Mode 04 reply batch.
        if sample.clear_requested
            || (has_outcome
                && self
                    .last_clear
                    .as_ref()
                    .is_none_or(|clear| clear.has_outcome))
        {
            self.last_clear = Some(ClearAttempt {
                at_ms: sample.at_ms,
                has_outcome,
            });
            self.rows
                .retain(|_, value| value.latest.reading.request != "04");
            self.failures
                .retain(|_, value| value.reading.request != "04");
        } else if has_outcome {
            if let Some(clear) = &mut self.last_clear {
                clear.has_outcome = true;
            }
        }

        if let Some((source, request)) = &sample.replace {
            self.rows.retain(|_, v| {
                v.latest.reading.source != *source || v.latest.reading.request != *request
            });
        }
        for trigger in sample.rows.iter().filter(|r| r.request.starts_with("0202")) {
            if let Some(frame) = trigger.freeze_frame_number() {
                self.failures
                    .remove(&scope(None, &format!("0200{frame:02X}")));
            }
        }
        for trigger in sample
            .rows
            .iter()
            .filter(|r| r.request.starts_with("0202") && !unavailable(r))
        {
            let Some(frame) = trigger.freeze_frame_number() else {
                continue;
            };
            let same_frame =
                |r: &Reading| r.source == trigger.source && r.freeze_frame_number() == Some(frame);
            let changed = self.rows.values().any(|v| {
                let r = &v.latest.reading;
                same_frame(r) && r.request == trigger.request && r.raw != trigger.raw
            });
            if trigger.no_freeze_frame() || changed {
                self.rows.retain(|_, v| !same_frame(&v.latest.reading));
                self.failures.retain(|_, v| !same_frame(&v.reading));
            }
        }
        let failed: BTreeSet<_> = sample
            .rows
            .iter()
            .filter(|r| unavailable(r))
            .map(failure_key)
            .collect();
        let mut replacement_batches = BTreeSet::new();
        let mut replacement_rows = BTreeSet::new();
        for row in sample.rows.iter().filter(|r| {
            (is_monitor_result(r) || matches!(r.request.as_str(), "0908" | "090B"))
                && !unavailable(r)
        }) {
            let batch = (row.source, row.request.as_str());
            if !failed.contains(&scope(row.source, &row.request)) {
                replacement_batches.insert(batch);
                replacement_rows.insert(row_key(row));
            }
        }
        if !replacement_batches.is_empty() {
            // A successful response replaces the test or counter set for this ECU and ID.
            // Keep earlier rows when the ECU's latest reply was incomplete.
            self.rows.retain(|key, value| {
                let r = &value.latest.reading;
                !replacement_batches.contains(&(r.source, r.request.as_str()))
                    || replacement_rows.contains(key)
            });
        }
        for r in &sample.rows {
            let corrected = correct_recorded_monitor(r);
            let r = corrected.as_ref().unwrap_or(r);
            let is_failure = unavailable(r);
            if !is_failure || r.is_signal_failure() {
                let request = scope(r.source, &r.request);
                if !failed.contains(&request) {
                    self.failures.remove(&request);
                }
                if r.source.is_some() {
                    let unknown = scope(None, &r.request);
                    if !failed.contains(&unknown) {
                        self.failures.remove(&unknown);
                    }
                }
            }
            if is_failure {
                self.failures.insert(
                    failure_key(r),
                    Stamped {
                        reading: r.clone(),
                        at_ms: sample.at_ms,
                    },
                );
                continue;
            }
            let key = row_key(r);
            if !failed.contains(&key) {
                self.failures.remove(&key);
            }
            let refresh_failed =
                failed.contains(&key) || failed.contains(&scope(r.source, &r.request));
            let observed_key =
                (observe.is_some() && graph_request(&r.request)).then(|| key.clone());
            let row = self.rows.entry(key).or_insert_with(|| ValueState {
                latest: Stamped {
                    reading: r.clone(),
                    at_ms: sample.at_ms,
                },
                history: VecDeque::new(),
                minimum: None,
                maximum: None,
            });
            let unit_changed = row.latest.reading.unit != r.unit && r.numeric.is_some();
            if unit_changed {
                row.minimum = None;
                row.maximum = None;
                row.history.clear();
            }
            row.latest = Stamped {
                reading: r.clone(),
                at_ms: sample.at_ms,
            };
            let number = r.numeric.filter(|n| n.is_finite() && !refresh_failed);
            if let Some(n) = number {
                row.minimum = Some(row.minimum.map_or(n, |v| v.min(n)));
                row.maximum = Some(row.maximum.map_or(n, |v| v.max(n)));
            }
            if graph_request(&r.request)
                && let Some(point) = row.push_point(sample.at_ms, number)
                && let Some((observe, key)) = observe.as_mut().zip(observed_key.as_deref())
            {
                if unit_changed {
                    observe(
                        key,
                        r,
                        GraphPoint {
                            at_ms: sample.at_ms,
                            value: None,
                        },
                    );
                }
                observe(key, r, point);
            }
        }
        if !failed.is_empty() {
            for (key, row) in &mut self.rows {
                if graph_request(&row.latest.reading.request)
                    && matching_failures(&self.failures, key, &row.latest.reading).any(|failure| {
                        failure.at_ms == sample.at_ms
                            && refresh_failure_applies(failure, &row.latest)
                    })
                    && let Some(point) = row.push_point(sample.at_ms, None)
                    && let Some(observe) = observe.as_mut()
                {
                    observe(key, &row.latest.reading, point);
                }
            }
        }
    }
}

pub(super) fn vehicle_vins<'a>(rows: impl Iterator<Item = &'a Stamped>) -> (Option<String>, bool) {
    let mut vins = rows
        .map(|row| &row.reading)
        .filter(|r| r.request == "0902" && valid_vin(&r.value))
        .map(|r| r.value.trim().to_ascii_uppercase());
    let first = vins.next();
    if vins.any(|vin| Some(&vin) != first.as_ref()) {
        (None, true)
    } else {
        (first, false)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub vehicle: Vehicle,
    pub demo: bool,
    pub at_ms: u64,
    pub rows: Vec<Stamped>,
    pub failures: Vec<Stamped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_requested_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emissions_scan: Option<EmissionsScan>,
}
impl Snapshot {
    pub fn from_results(vehicle: &Vehicle, demo: bool, at_ms: u64, results: &Results) -> Self {
        Self {
            vehicle: vehicle.clone(),
            demo,
            at_ms: at_ms.max(results.last_sample_ms.unwrap_or(0)),
            rows: results.rows.values().map(|v| v.latest.clone()).collect(),
            failures: results.failures.values().cloned().collect(),
            clear_requested_at_ms: results.clear_attempt_ms(),
            emissions_scan: results.emissions_scan.clone(),
        }
    }

    pub(super) fn from_emissions_scan(
        vehicle: &Vehicle,
        demo: bool,
        results: &Results,
    ) -> Option<Self> {
        let scan = results.emissions_scan.as_ref()?;
        if scan.state == super::emissions_scan::State::Running {
            return None;
        }
        let at_ms = scan.ended_ms?;
        let from_scan = |row: &&Stamped| (scan.started_ms..=at_ms).contains(&row.at_ms);
        let (vin, conflict) = vehicle_vins(
            results
                .rows
                .values()
                .map(|row| &row.latest)
                .filter(from_scan),
        );
        let mut vehicle = vehicle.clone();
        if conflict {
            vehicle.vin.clear();
        } else if let Some(vin) = vin {
            vehicle.vin = vin;
        }
        Some(Self {
            vehicle,
            demo,
            at_ms,
            rows: results
                .rows
                .values()
                .map(|v| &v.latest)
                .filter(from_scan)
                .cloned()
                .collect(),
            failures: results
                .failures
                .values()
                .filter(from_scan)
                .cloned()
                .collect(),
            clear_requested_at_ms: None,
            emissions_scan: Some(scan.clone()),
        })
    }

    pub(super) fn clear_attempt_ms(&self) -> Option<u64> {
        self.clear_requested_at_ms.or_else(|| {
            self.rows
                .iter()
                .chain(&self.failures)
                .filter(|s| clear_outcome(&s.reading))
                .map(|s| s.at_ms)
                .max()
        })
    }

    pub(super) fn predates_clear(&self, value: &Stamped) -> bool {
        predates_clear(value, self.clear_attempt_ms())
    }

    pub(super) fn refresh_failed(&self, value: &Stamped) -> bool {
        self.failures
            .iter()
            .any(|failure| refresh_failure_applies(failure, value))
    }

    pub(super) fn evidence_issue(&self, value: &Stamped) -> Option<EvidenceIssue> {
        evidence_issue(value, self.predates_clear(value), || {
            self.refresh_failed(value)
        })
    }

    pub(super) fn predates_scan(&self, value: &Stamped) -> bool {
        self.emissions_scan
            .as_ref()
            .is_some_and(|scan| value.at_ms < scan.started_ms)
    }
}

fn evidence_issue(
    value: &Stamped,
    before_clear: bool,
    refresh_failed: impl FnOnce() -> bool,
) -> Option<EvidenceIssue> {
    if before_clear {
        Some(EvidenceIssue::BeforeClear)
    } else if !unavailable(&value.reading) && refresh_failed() {
        Some(EvidenceIssue::RefreshFailed)
    } else {
        None
    }
}

fn append_scan_summary(html: &mut String, snapshot: &Snapshot) {
    let Some(scan) = &snapshot.emissions_scan else {
        return;
    };
    let date = i64::try_from(scan.started_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(String::new, |at| {
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        });
    let state = if scan.state == super::emissions_scan::State::Running {
        "In progress at capture time"
    } else {
        scan.state.label()
    };
    html.push_str(&format!(
        "<details><summary><strong>Emissions scan</strong> · {} · {}</summary><table><thead><tr><th>Check</th><th>Result at scan</th></tr></thead><tbody>",
        state, date,
    ));
    for (check, result) in scan.checks() {
        html.push_str(&format!(
            "<tr><th scope=\"row\">{}</th><td>{}</td></tr>",
            check.label(),
            html_escape(&scan.check_label(result))
        ));
    }
    html.push_str("</tbody></table></details>");
}
fn latest_rows(rows: &[Stamped]) -> BTreeMap<String, &Stamped> {
    let mut latest: BTreeMap<String, &Stamped> = BTreeMap::new();
    for row in rows {
        latest
            .entry(row_key(&row.reading))
            .and_modify(|current| {
                if row.at_ms >= current.at_ms {
                    *current = row;
                }
            })
            .or_insert(row);
    }
    latest
}

pub fn comparison(
    before: &Snapshot,
    after: &Snapshot,
) -> Result<Vec<(String, String, String, String)>, String> {
    if !before.vehicle.has_identity()
        || before.vehicle.identity() != after.vehicle.identity()
        || before.demo != after.demo
    {
        return Err("Comparison requires the same vehicle and data source".into());
    }
    let current = latest_rows(&after.rows);
    let old = latest_rows(&before.rows);
    let clear_at = after.clear_attempt_ms();
    let mut changes = vec![];
    for (key, &prev) in &old {
        let corrected_old = correct_recorded_monitor(&prev.reading);
        let r = corrected_old.as_ref().unwrap_or(&prev.reading);
        let new = current.get(key).copied();
        let corrected_new = new.and_then(|value| correct_recorded_monitor(&value.reading));
        let new_reading = corrected_new.as_ref().or_else(|| new.map(|s| &s.reading));
        let failed = after
            .failures
            .iter()
            .any(|f| f.at_ms > before.at_ms && refresh_failure_applies(f, new.unwrap_or(prev)));
        let status = if new.is_some_and(|value| predates_clear(value, clear_at)) {
            "Reread needed after clear request".into()
        } else if failed {
            "Read failed / not verified".into()
        } else if let Some(n) = new_reading.filter(|_| new.is_some_and(|s| s.at_ms > before.at_ms))
        {
            match (r.numeric, n.numeric) {
                (Some(a), Some(b)) if r.unit == n.unit => {
                    format!("Change {:+.3} {}", b - a, r.unit)
                }
                _ if r.value == n.value => "Unchanged after reread".into(),
                _ => "Changed after reread".into(),
            }
        } else if new.is_none()
            && after.rows.iter().any(|s| {
                s.reading.source == r.source
                    && s.reading.request == r.request
                    && s.at_ms > before.at_ms
            })
        {
            "Not reported after rescan".into()
        } else {
            "Not retested".into()
        };
        changes.push((
            format!("{} / ECU {}", r.name, ecu_label(r.source)),
            r.value.clone(),
            new_reading.map_or("--", |r| r.value.as_str()).into(),
            status,
        ));
    }
    for (_, n) in current.iter().filter(|(key, s)| {
        !old.contains_key(*key) && s.at_ms > before.at_ms && !predates_clear(s, clear_at)
    }) {
        let corrected = correct_recorded_monitor(&n.reading);
        let reading = corrected.as_ref().unwrap_or(&n.reading);
        changes.push((
            reading.name.clone(),
            "--".into(),
            reading.value.clone(),
            "Newly observed".into(),
        ));
    }
    Ok(changes)
}
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
pub fn report(snapshot: &Snapshot, baseline: Option<&Snapshot>) -> String {
    report_with_descriptions(snapshot, baseline, &|code| {
        crate::dtc_descriptions::describe(code).into()
    })
}

pub(super) fn printable_header(title: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>{}</title>
<style>
* {{ box-sizing: border-box; }}
body {{ font: 15px/1.45 system-ui, sans-serif; max-width: 900px; margin: 32px auto; padding: 0 24px; color: #25221e; overflow-wrap: break-word; }}
button {{ font: inherit; font-weight: 650; color: #fff; background: #312b25; border: 0; border-radius: 5px; padding: 8px 14px; cursor: pointer; }}
a {{ color: #813a19; }}
header {{ border-top: 4px solid #312b25; margin-top: 18px; padding-top: 10px; }}
h1 {{ margin: 8px 0 4px; font-size: 29px; letter-spacing: -.02em; }}
h2 {{ margin: 19px 0 8px; font-size: 19px; }}
section > h2 {{ border-bottom: 1px solid #aaa29a; padding-bottom: 5px; }}
h3 {{ margin: 13px 0 4px; font-size: 15px; }}
p {{ margin: 4px 0 8px; }}
table {{ border-collapse: collapse; width: 100%; margin: 6px 0 12px; font-variant-numeric: tabular-nums; }}
td, th {{ padding: 5px 7px; border-bottom: 1px solid #ddd6cd; text-align: left; vertical-align: top; overflow-wrap: anywhere; }}
tbody th {{ font-weight: 550; }}
thead th {{ background: #f2eee8; border-bottom-color: #aaa29a; }}
.codes td:nth-child(-n+2), .codes thead th:nth-child(-n+3), .codes th[scope=row] {{ white-space: nowrap; }}
.codes th:nth-child(5) {{ width: 21%; }}
.readiness thead th:nth-child(n+2) {{ width: 27%; }}
.read-status th, .read-status td {{ border-bottom-color: #aaa29a; }}
.frame-values th:first-child {{ width: 63%; }}
.result-note {{ display: block; margin-top: 2px; font-size: .86em; color: #5c5045; }}
.vehicle-meta {{ display: flex; gap: 4px 22px; flex-wrap: wrap; margin: 6px 0 10px; font-size: .92em; }}
.vehicle-meta div {{ display: flex; gap: 5px; }}
.vehicle-meta dt {{ color: #5c5045; }}
.vehicle-meta dd {{ margin: 0; font-weight: 600; }}
.comparison td:first-child {{ white-space: nowrap; }}
.note {{ white-space: pre-wrap; }}
summary {{ cursor: pointer; margin-top: 10px; font-weight: 600; }}
@media print {{
    @page {{
        margin: 13mm;
        @bottom-right {{ content: counter(page) " / " counter(pages); color: #5c5045; font: 8pt system-ui, sans-serif; }}
    }}
    button, details:not([open]) {{ display: none; }}
    body {{ margin: 0; padding: 0; font-size: 10pt; line-height: 1.32; }}
    header {{ margin-top: 0; }}
    h1, h2, h3, thead {{ break-after: avoid; }}
    h1 {{ font-size: 20pt; }}
    h2 {{ font-size: 14pt; }}
    h3 {{ font-size: 11pt; }}
    p {{ orphans: 2; widows: 2; }}
    thead {{ display: table-header-group; }}
    tr {{ break-inside: avoid; }}
    td, th {{ padding: 3px 6px; }}
    thead th {{ padding-block: 4px; }}
}}
</style></head><body>
<button onclick="window.print()">Print / Save as PDF</button>"#,
        html_escape(title)
    )
}

pub(super) fn report_with_descriptions(
    snapshot: &Snapshot,
    baseline: Option<&Snapshot>,
    describe: &impl Fn(&str) -> String,
) -> String {
    report_with_notes(
        snapshot,
        baseline,
        &super::scan_notes::Notes::default(),
        describe,
    )
}

pub(super) fn report_with_notes(
    snapshot: &Snapshot,
    baseline: Option<&Snapshot>,
    notes: &super::scan_notes::Notes,
    describe: &impl Fn(&str) -> String,
) -> String {
    let mut html = printable_header("OBD-II diagnostic report");
    let work_order = if notes.work_order.is_empty() {
        String::new()
    } else {
        format!(
            "<div><dt>Work order</dt><dd>{}</dd></div>",
            html_escape(&notes.work_order)
        )
    };
    html.push_str(&format!(
        "<header><h1>OBD-II diagnostic report</h1>{}<h2>{}</h2><dl class=\"vehicle-meta\"><div><dt>VIN</dt><dd>{}</dd></div><div><dt>Registration</dt><dd>{}</dd></div><div><dt>Odometer</dt><dd>{}</dd></div><div><dt>Scan date</dt><dd>{}</dd></div>{work_order}</dl></header>",
        if snapshot.demo {
            "<p><strong>DEMONSTRATION DATA</strong></p>"
        } else {
            ""
        },
        html_escape(&snapshot.vehicle.label()),
        field_or_dash(&snapshot.vehicle.vin),
        field_or_dash(&snapshot.vehicle.registration),
        field_or_dash(&snapshot.vehicle.odometer),
        i64::try_from(snapshot.at_ms)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|t| t
                .with_timezone(&chrono::Local)
                .format("%b %e, %Y %I:%M %p")
                .to_string())
            .unwrap_or_default()
    ));
    if vehicle_vins(snapshot.rows.iter()).1 {
        html.push_str("<p><strong>ECUs reported different VINs.</strong></p>");
    }
    if let Some(scan) = &snapshot.emissions_scan {
        let status = match scan.state {
            super::emissions_scan::State::Running => "Scan in progress at capture",
            super::emissions_scan::State::Finished => "Emissions scan finished",
            super::emissions_scan::State::Interrupted => "Emissions scan interrupted",
        };
        let stage = scan.clear_stage.map_or("", ClearScanStage::label);
        html.push_str(&format!(
            "<p><strong>{status}</strong>{}{stage}</p>",
            if stage.is_empty() { "" } else { " · " }
        ));
    }
    if snapshot.clear_attempt_ms().is_some() {
        let acknowledgments = snapshot
            .rows
            .iter()
            .filter(|s| clear_outcome(&s.reading))
            .count();
        html.push_str(&format!(
            "<p><strong>Latest emissions clear request</strong> &middot; {acknowledgments} ECU acknowledgments. Affected readings from before the request are historical until reread.</p>"
        ));
    }
    if !notes.technician.is_empty() {
        append_report_note(&mut html, "Technician notes", &notes.technician);
    }
    super::report_sections::append(&mut html, snapshot, describe);
    if !snapshot.vehicle.notes.trim().is_empty() {
        let heading = if notes.technician.is_empty() {
            "Technician notes"
        } else {
            "Notes at capture"
        };
        append_report_note(&mut html, heading, &snapshot.vehicle.notes);
    }
    append_scan_summary(&mut html, snapshot);
    let mut other = snapshot
        .rows
        .iter()
        .chain(snapshot.failures.iter())
        .filter(|s| !super::report_sections::is_scan_reading(&s.reading))
        .peekable();
    if other.peek().is_some() {
        html.push_str("<details class=\"other\"><summary>Other diagnostic readings</summary><table><thead><tr><th>ECU / request</th><th>Parameter</th><th>Value</th><th>Result</th></tr></thead><tbody>");
        for s in other {
            let r = &s.reading;
            let state = super::report_sections::qualification(snapshot, s);
            html.push_str(&format!(
                "<tr><td>{} / {}</td><td>{}</td><td>{} {}</td><td>{}{}</td></tr>",
                ecu_label(r.source),
                html_escape(&r.request),
                html_escape(&r.name),
                html_escape(&r.value),
                html_escape(unit_label(&r.unit)),
                html_escape(&r.status),
                state.map_or_else(String::new, |text| format!(
                    "<small class=\"result-note\">{text}</small>"
                ))
            ));
        }
        html.push_str("</tbody></table></details>");
    }
    if let Some(before) = baseline {
        html.push_str("<details><summary>Before / after comparison</summary>");
        match comparison(before, snapshot) {
            Ok(rows) => {
                html.push_str(
                    "<table><thead><tr><th>Item</th><th>Before</th><th>After</th><th>Assessment</th></tr></thead><tbody>",
                );
                for (name, a, b, state) in rows {
                    html.push_str(&format!(
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        html_escape(&name),
                        html_escape(&a),
                        html_escape(&b),
                        html_escape(&state)
                    ));
                }
                html.push_str("</tbody></table>");
            }
            Err(e) => html.push_str(&format!("<p>{}</p>", html_escape(&e))),
        }
        html.push_str("</details>");
    }
    html.push_str("</body></html>");
    html
}

fn append_report_note(html: &mut String, heading: &str, note: &str) {
    html.push_str(&format!(
        "<section><h2>{heading}</h2><p class=\"note\">{}</p></section>",
        html_escape(note)
    ));
}

fn field_or_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "—".into()
    } else {
        html_escape(value)
    }
}
