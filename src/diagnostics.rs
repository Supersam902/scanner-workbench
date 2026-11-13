//! Generic J1979 services and read-only Toyota signal profiles.
//! Signal definitions: OBDb, CC BY-SA 4.0; see data/obdb/ATTRIBUTION.txt.
use crate::adapter::{
    DiagnosticAdapter, DiagnosticResponse, DiagnosticTarget, decode_hex, encode_hex,
};
use crate::elm327::Elm327Error;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[path = "mercedes_profiles.rs"]
mod mercedes_profiles;
#[path = "oxygen_monitor.rs"]
pub(crate) mod oxygen_monitor;
#[path = "toyota_profiles.rs"]
mod toyota_profiles;
#[path = "toyota_workspace.rs"]
mod toyota_workspace;
#[path = "vehicle_information.rs"]
mod vehicle_information;
pub(crate) use vehicle_information::information_name;

#[derive(Debug, Clone)]
pub struct Query {
    pub bytes: Vec<u8>,
    pub target: Option<DiagnosticTarget>,
    pub name: String,
    pub signals: Vec<Value>,
    #[allow(dead_code)] // Capture-coverage metadata retained for regression checks.
    pub verified: bool,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub queries: Vec<Query>,
    pub discovery: Option<(u8, u8)>,
    pub cancelled: Arc<AtomicBool>,
    pub allow_control: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Reading {
    pub source: Option<u32>,
    pub request: String,
    pub name: String,
    pub value: String,
    pub unit: String,
    pub raw: String,
    pub numeric: Option<f64>,
    pub status: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signal_failure: bool,
}

const NO_FREEZE_FRAME: &str = "No freeze frame stored";
pub(crate) const MONITOR_NOT_COMPLETED: &str = "Not completed";
pub(crate) const MONITOR_UNKNOWN_SCALING: &str = "Scaling/limits unverified; pass/fail unavailable";
pub(crate) const MONITOR_INVALID_LIMITS: &str = "Invalid limits; pass/fail unavailable";
pub(crate) const UNSUPPORTED_REQUEST: &str = "ECU reports this request is unsupported";
pub(crate) const LEGACY_MONITOR_RAW: &str =
    "Legacy monitor data; OEM scaling and limits require a matching definition";

#[cfg(not(target_arch = "wasm32"))]
impl Reading {
    pub(crate) fn is_signal_failure(&self) -> bool {
        // Recognize decoder errors in recordings written before signal_failure was stored.
        self.signal_failure
            || self.value == "Unavailable"
                && matches!(
                    self.status.as_str(),
                    "Invalid bit index"
                        | "Missing signal width"
                        | "Unsupported signal width"
                        | "Response too short or signal format unsupported"
                        | "Unaligned little-endian signal is unsupported"
                        | "Invalid zero divisor"
                        | "Invalid numeric value"
                        | "ECU returned a not-available sentinel"
                )
    }

    pub(crate) fn freeze_frame_number(&self) -> Option<u8> {
        if self.request.len() != 6 || !self.request.starts_with("02") {
            return None;
        }
        u8::from_str_radix(self.request.get(4..)?, 16).ok()
    }

    pub(crate) fn no_freeze_frame(&self) -> bool {
        self.request.starts_with("0202") && self.value == NO_FREEZE_FRAME
    }
}

#[derive(Debug, Clone)]
pub enum Update {
    Row(u64, Reading),
    ReplaceRows(u64, Option<u32>, String, Vec<Reading>),
    Support(u64, u8, Vec<u8>),
    Done(u64, String),
}

#[derive(Debug, Clone, Default)]
pub struct PidSupport {
    pub reported: BTreeSet<u8>,
    /// Highest PID for which absence can be inferred from the received maps.
    pub checked_through: u8,
    pub next_range: Option<u8>,
    pub problem: Option<String>,
}

pub(crate) struct SupportScan {
    mode: u8,
    parameter: u8,
    expected: BTreeSet<Option<u32>>,
    known_sources: BTreeSet<Option<u32>>,
    sources_by_pid: BTreeMap<u8, BTreeSet<Option<u32>>>,
    pub(crate) support: PidSupport,
}

impl SupportScan {
    pub(crate) fn new(mode: u8, parameter: u8) -> Self {
        Self {
            mode,
            parameter,
            expected: BTreeSet::new(),
            known_sources: BTreeSet::new(),
            sources_by_pid: BTreeMap::new(),
            support: PidSupport {
                next_range: Some(0),
                ..Default::default()
            },
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn refresh(&self) -> Self {
        let mut scan = Self::new(self.mode, self.parameter);
        scan.expect_sources(self.known_sources.iter().copied());
        scan
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn expect_sources(&mut self, sources: impl IntoIterator<Item = Option<u32>>) {
        self.known_sources.extend(sources);
        self.expected = self.known_sources.clone();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn known_sources(&self) -> &BTreeSet<Option<u32>> {
        &self.known_sources
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn pid_sources(&self, pid: u8) -> Option<&BTreeSet<Option<u32>>> {
        self.sources_by_pid.get(&pid)
    }

    pub(crate) async fn step(&mut self, adapter: &mut impl DiagnosticAdapter, timeout_ms: u64) {
        let Some(base) = self.support.next_range else {
            return;
        };
        let mut bytes = vec![self.mode, base];
        if matches!(self.mode, 2 | 5) {
            bytes.push(self.parameter);
        }
        let can = adapter.connection_info().protocol.contains("CAN");
        let replies = adapter.request(&bytes, timeout_ms).await;
        if can {
            self.accept(replies);
        } else {
            self.accept_map(replies, false);
        }
    }

    pub(crate) fn accept(&mut self, replies: Result<Vec<DiagnosticResponse>, Elm327Error>) {
        self.accept_map(replies, true);
    }

    fn accept_map(&mut self, replies: Result<Vec<DiagnosticResponse>, Elm327Error>, can: bool) {
        let Some(base) = self.support.next_range.take() else {
            return;
        };
        let replies = match replies {
            Ok(replies) => replies,
            Err(error) => {
                self.support
                    .problem
                    .get_or_insert_with(|| error.to_string());
                return;
            }
        };
        let legacy_info = self.mode == 9 && !can;
        let response = [
            self.mode + 0x40,
            base,
            if legacy_info { 1 } else { self.parameter },
        ];
        let prefix = if matches!(self.mode, 2 | 5) || legacy_info {
            3
        } else {
            2
        };
        let mut sources = BTreeSet::new();
        let mut more = BTreeSet::new();
        let mut problems = Vec::new();
        let mut unexpected_support = false;
        for reply in replies {
            let source = || {
                reply
                    .source
                    .map_or_else(|| "unidentified ECU".into(), |id| format!("ECU {id:X}"))
            };
            if let Some(error) = reply.transport_error {
                unexpected_support |=
                    reply.source.is_some() && !self.known_sources.contains(&reply.source);
                if base == 0 && reply.source.is_some() {
                    self.known_sources.insert(reply.source);
                }
                problems.push(format!(
                    "{} could not complete its support reply. {error}",
                    source()
                ));
                continue;
            }
            if reply.payload.starts_with(&[0x7f, self.mode]) {
                match reply.payload.get(2) {
                    Some(0x11 | 0x12 | 0x31) if !self.expected.contains(&reply.source) => continue,
                    Some(0x78) => {
                        problems.push(format!("{} support reply remained pending", source()))
                    }
                    _ => problems.push(format!(
                        "{} rejected the support request ({})",
                        source(),
                        encode_hex(&reply.payload)
                    )),
                }
                unexpected_support |=
                    reply.source.is_some() && !self.known_sources.contains(&reply.source);
                if base == 0 && reply.source.is_some() {
                    self.known_sources.insert(reply.source);
                }
                continue;
            }
            let match_len = if legacy_info { 2 } else { prefix };
            if !reply.payload.starts_with(&response[..match_len]) {
                continue;
            }
            if legacy_info && reply.payload.get(2) != Some(&1) {
                problems.push(format!(
                    "{} sent an invalid support bitmap sequence",
                    source()
                ));
                unexpected_support |=
                    reply.source.is_some() && !self.known_sources.contains(&reply.source);
                if base == 0 {
                    self.known_sources.insert(reply.source);
                }
                continue;
            }
            let data = match (self.mode, can) {
                (6, false) => reply.payload.get(prefix..).and_then(legacy_monitor_bitmap),
                (5, _) => reply.payload.get(prefix..).filter(|data| data.len() == 4),
                _ => reply.payload.get(prefix..prefix + 4),
            };
            let Some(data) = data else {
                problems.push(format!("{} sent an invalid support bitmap", source()));
                unexpected_support |=
                    reply.source.is_some() && !self.known_sources.contains(&reply.source);
                if base == 0 {
                    self.known_sources.insert(reply.source);
                }
                continue;
            };
            sources.insert(reply.source);
            let bitmap = u32::from_be_bytes(data.try_into().unwrap());
            for offset in 1..=31 {
                if bitmap & (1 << (32 - offset)) != 0 {
                    let pid = base + offset;
                    self.support.reported.insert(pid);
                    if matches!(self.mode, 1 | 2 | 5 | 6 | 9) {
                        self.sources_by_pid
                            .entry(pid)
                            .or_default()
                            .insert(reply.source);
                    }
                }
            }
            if bitmap & 1 != 0 {
                more.insert(reply.source);
            }
            unexpected_support |= bitmap != 0 && !self.expected.contains(&reply.source);
        }
        if base != 0 && (unexpected_support || !sources.is_subset(&self.known_sources)) {
            self.support.checked_through = 0;
            problems.push("ECU responses changed between support ranges".into());
        }
        self.known_sources.extend(sources.iter().copied());
        if sources.is_empty() || !self.expected.is_subset(&sources) {
            problems.push(format!(
                "Missing support bitmap for {:02X}{base:02X}",
                self.mode
            ));
        }
        if !problems.is_empty() {
            self.support
                .problem
                .get_or_insert_with(|| problems.join(". "));
        }
        let continuation = base < 0xe0 && !more.is_empty();
        if self.support.problem.is_none() {
            self.support.checked_through = if continuation { base + 31 } else { u8::MAX };
        }
        // Keep the partial result and follow ranges announced by valid ECUs.
        if continuation && (self.support.problem.is_none() || matches!(self.mode, 2 | 5 | 6 | 9)) {
            self.support.next_range = Some(base + 32);
            self.expected = more;
        }
    }
}

fn legacy_monitor_bitmap(data: &[u8]) -> Option<&[u8]> {
    match data {
        [0xff, bitmap @ ..] if bitmap.len() == 4 => Some(bitmap),
        bitmap if bitmap.len() == 4 => Some(bitmap),
        _ => None,
    }
}

/// Original community sources shared by the classic UI and the new workstation.
pub fn bundled_profiles() -> Vec<(&'static str, &'static str)> {
    toyota_profiles::PROFILES
        .iter()
        .chain(mercedes_profiles::PROFILES.iter())
        .copied()
        .collect()
}

pub fn load_vehicle_profile(
    text: &str,
    year: u16,
    experimental: bool,
) -> Result<(Vec<Query>, BTreeMap<String, String>, usize), String> {
    toyota_workspace::load_profile(text, year, experimental)
}

pub const SERVICES: &[(u8, &str)] = &[
    (1, "01 · Live data / readiness"),
    (2, "02 · Freeze frame"),
    (3, "03 · Stored DTCs"),
    (4, "04 · Clear emissions data"),
    (5, "05 · Legacy oxygen tests"),
    (6, "06 · Monitor test results"),
    (7, "07 · Pending DTCs"),
    (8, "08 · Component controls"),
    (9, "09 · Vehicle information"),
    (10, "0A · Permanent DTCs"),
];

pub fn read_service(mode: u8) -> bool {
    matches!(mode, 1 | 2 | 3 | 5 | 6 | 7 | 9 | 10 | 0x19 | 0x21 | 0x22)
}

pub fn catalog(text: &str, year: Option<u16>) -> Result<Vec<Query>, String> {
    if text.len() > 4 * 1024 * 1024 {
        return Err("Profile exceeds 4 MiB".into());
    }
    let root: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let commands = root["commands"]
        .as_array()
        .ok_or("Missing commands array")?;
    let mut queries = Vec::new();
    for command in commands {
        let filter = &command["filter"];
        if let Some(year) = year {
            if filter["from"].as_u64().is_some_and(|n| year < n as u16)
                || filter["to"].as_u64().is_some_and(|n| year > n as u16)
                || filter["years"]
                    .as_array()
                    .is_some_and(|years| !years.iter().any(|n| n.as_u64() == Some(year as u64)))
            {
                continue;
            }
        }
        let Some(cmd) = command["cmd"].as_object() else {
            continue;
        };
        if cmd.len() != 1 {
            continue;
        }
        let (service, pid) = cmd.iter().next().unwrap();
        let Some(pid) = pid.as_str() else { continue };
        let bytes = decode_hex(&format!("{service}{pid}")).map_err(|e| e.to_string())?;
        if !matches!(bytes.first(), Some(1 | 0x21 | 0x22)) {
            return Err("Profiles may contain only services 01, 21, and 22 (data reads)".into());
        }
        if bytes.len() > 16 {
            return Err("Profile command is too long".into());
        }
        let mode = bytes[0];
        let target = if mode == 1 {
            None
        } else {
            let request = command["hdr"]
                .as_str()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .ok_or("Invalid request header")?;
            let response = command["rax"]
                .as_str()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .ok_or("Invalid response header")?;
            if request > 0x1fffffff || response > 0x1fffffff {
                return Err("Invalid ECU identifier".into());
            }
            Some(DiagnosticTarget { request, response })
        };
        let signals = command["signals"].as_array().cloned().unwrap_or_default();
        let name = signals
            .iter()
            .filter(|s| s["hidden"] != true)
            .filter_map(|s| s["name"].as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        let key = target
            .map(|t| format!("{:X}.{:X}.{}", t.request, t.response, encode_hex(&bytes)))
            .unwrap_or_else(|| encode_hex(&bytes));
        let verified = year == Some(2012)
            && include_str!("../data/obdb/corolla-2012-support.yaml")
                .split("unsupported_commands_by_ecu:")
                .next()
                .unwrap_or("")
                .lines()
                .any(|line| {
                    line.trim()
                        .strip_prefix("- ")
                        .is_some_and(|line| line.split([':', '|']).next() == Some(key.as_str()))
                });
        queries.push(Query {
            bytes,
            target,
            name,
            signals,
            verified,
        });
    }
    if queries.len() > 2048 {
        return Err("Too many profile commands".into());
    }
    Ok(queries)
}

/// OBDb bit indices count from the most-significant bit of the data payload.
/// OBDb null limits apply to scaled values; missing bytes stay unavailable.
pub fn signal_value(signal: &Value, data: &[u8]) -> Result<(String, Option<f64>, String), String> {
    let fmt = &signal["fmt"];
    let start =
        usize::try_from(fmt["bix"].as_u64().unwrap_or(0)).map_err(|_| "Invalid bit index")?;
    let width = fmt["len"].as_u64().ok_or("Missing signal width")?;
    if width == 0 || width > 64 {
        return Err("Unsupported signal width".into());
    }
    let len = width as usize;
    if len == 0
        || len > 64
        || start
            .checked_add(len)
            .is_none_or(|end| end > data.len() * 8)
    {
        return Err("Response too short or signal format unsupported".into());
    }
    let mut raw = 0u64;
    for bit in start..start + len {
        raw = (raw << 1) | u64::from((data[bit / 8] >> (7 - bit % 8)) & 1);
    }
    if fmt["blsb"] == true {
        if start % 8 != 0 || len % 8 != 0 {
            return Err("Unaligned little-endian signal is unsupported".into());
        }
        raw = raw.swap_bytes() >> (64 - len);
    }
    let unit = fmt["unit"].as_str().unwrap_or("").to_string();
    if let Some(entry) = fmt["map"].get(raw.to_string()) {
        let text = entry["description"]
            .as_str()
            .or(entry["value"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| entry.to_string());
        return Ok((text, None, unit));
    }
    if unit == "hex" {
        return Ok((format!("{raw:X}"), None, unit));
    }
    let signed = fmt["sign"].as_bool().unwrap_or(false);
    let base = if signed && len == 64 {
        (raw as i64) as f64
    } else if signed && raw & (1u64 << (len - 1)) != 0 {
        (raw as i128 - (1i128 << len)) as f64
    } else {
        raw as f64
    };
    let divisor = fmt["div"].as_f64().unwrap_or(1.0);
    if divisor == 0.0 {
        return Err("Invalid zero divisor".into());
    }
    let value =
        base * fmt["mul"].as_f64().unwrap_or(1.0) / divisor + fmt["add"].as_f64().unwrap_or(0.0);
    if !value.is_finite() {
        return Err("Invalid numeric value".into());
    }
    // Schema: nullmin is an inclusive lower invalid bound; nullmax is an
    // inclusive upper invalid bound, both in the scaled value's units.
    if fmt["nullmin"].as_f64().is_some_and(|min| value <= min)
        || fmt["nullmax"].as_f64().is_some_and(|max| value >= max)
    {
        return Err("ECU returned a not-available sentinel".into());
    }
    let text = match unit.as_str() {
        "offon" | "noyes" => {
            if raw == 0 {
                "No / Off".into()
            } else {
                "Yes / On".into()
            }
        }
        "yesno" => {
            if raw == 0 {
                "Yes / Ready".into()
            } else {
                "No / Not ready".into()
            }
        }
        _ => format!("{value:.4}"),
    };
    Ok((text, Some(value), unit))
}

fn row(
    query: &Query,
    reply: &DiagnosticResponse,
    name: String,
    value: String,
    unit: String,
    numeric: Option<f64>,
    status: &str,
) -> Reading {
    Reading {
        source: reply.source,
        request: encode_hex(&query.bytes),
        name,
        value,
        unit,
        numeric,
        raw: encode_hex(&reply.payload),
        status: status.into(),
        signal_failure: false,
    }
}

pub fn decode(
    query: &Query,
    reply: &DiagnosticResponse,
    can: bool,
) -> Result<Vec<Reading>, String> {
    if let Some(error) = &reply.transport_error {
        return Err(error.clone());
    }
    let mode = *query.bytes.first().ok_or("Empty request")?;
    if reply.payload == [0x7f, mode, 0x78] {
        return Err("ECU response remained pending until the request timed out".into());
    }
    if reply.payload.starts_with(&[0x7f, mode]) {
        if matches!(mode, 3 | 7 | 10) && reply.payload.get(2) == Some(&0x11) {
            return Err("ECU reports this code service is unsupported".into());
        }
        if (query.bytes == [1, 0x41] || mode == 2 && query.bytes.get(1) == Some(&2) || mode == 5)
            && matches!(reply.payload.get(2), Some(0x11 | 0x12 | 0x31))
        {
            return Err(UNSUPPORTED_REQUEST.into());
        }
        return Err(format!(
            "ECU rejected request: {}",
            encode_hex(&reply.payload)
        ));
    }
    let prefix_len = match mode {
        1 | 6 | 8 | 9 | 0x19 | 0x21 => 2,
        2 | 5 | 0x22 => 3,
        _ => 1,
    };
    if reply.payload.first() != Some(&mode.wrapping_add(0x40)) {
        return Err("Unrelated service response".into());
    }
    if can && mode == 1 && (3..=7).contains(&query.bytes.len()) && query.signals.is_empty() {
        // A manual group request shows the original response as raw bytes.
        if !reply
            .payload
            .get(1)
            .is_some_and(|pid| query.bytes[1..].contains(pid))
        {
            return Err("Response identifier does not match the request".into());
        }
        return Ok(vec![row(
            query,
            reply,
            query.name.clone(),
            encode_hex(&reply.payload[1..]),
            "raw".into(),
            None,
            "Raw response; no decoder definition",
        )]);
    } else if query.bytes.len() > 1
        && reply.payload.get(1..prefix_len) != query.bytes.get(1..prefix_len)
    {
        return Err("Response identifier does not match the request".into());
    }
    let data = reply
        .payload
        .get(prefix_len..)
        .ok_or("Truncated response header")?;
    if mode == 2 && data.is_empty() {
        return Err("Freeze-frame response has no data".into());
    }
    let mut rows = Vec::new();
    if mode == 0x19 {
        if query.bytes.len() != 3 || query.bytes[1] != 2 {
            return Err("Only UDS reportDTCByStatusMask is decoded".into());
        }
        let mask = *data.first().ok_or("Missing UDS status availability mask")?;
        let records = &data[1..];
        if records.len() % 4 != 0 {
            return Err("Truncated UDS DTC/status record".into());
        }
        let count = records.len() / 4;
        rows.push(row(
            query,
            reply,
            "ECU DTC summary".into(),
            format!(
                "{count} matching records | requested status {:02X}, available {mask:02X}",
                query.bytes[2]
            ),
            String::new(),
            None,
            if mask == 0 {
                "Status support unavailable; no all-clear conclusion"
            } else {
                "Read-only UDS result; raw DTC identifiers, no OEM descriptions"
            },
        ));
        for record in records.chunks_exact(4) {
            let code = encode_hex(&record[..3]);
            let status = record[3] & mask;
            let labels = [
                "Test failed",
                "Failed this cycle",
                "Pending",
                "Confirmed",
                "Not completed since clear",
                "Failed since clear",
                "Not completed this cycle",
                "Warning indicator requested",
            ];
            let flags = labels
                .iter()
                .enumerate()
                .filter(|(bit, _)| status & (1 << bit) != 0)
                .map(|(_, label)| *label)
                .collect::<Vec<_>>()
                .join(" | ");
            rows.push(row(
                query,
                reply,
                format!("DTC {code}"),
                code,
                "UDS identifier".into(),
                None,
                &format!(
                    "Status {:02X}, availability {mask:02X} | {flags}",
                    record[3]
                ),
            ));
        }
        return Ok(rows);
    }
    if matches!(mode, 3 | 7 | 10) {
        let code_bytes = if can {
            let (&count, payload) = data.split_first().ok_or("Missing CAN DTC count")?;
            let (codes, padding) = payload
                .split_at_checked(usize::from(count) * 2)
                .ok_or("Truncated CAN DTC list")?;
            if codes.chunks_exact(2).any(|pair| pair == [0, 0])
                || padding.iter().any(|&byte| byte != 0)
            {
                return Err("CAN DTC count does not match the response".into());
            }
            codes
        } else if !data.is_empty() && data.len() % 2 == 0 {
            data
        } else {
            return Err("Truncated legacy DTC pair".into());
        };
        let codes = crate::obd::decode_dtc_response(code_bytes);
        let value = if codes.is_empty() {
            "No codes reported by this ECU".into()
        } else {
            codes
                .iter()
                .map(|c| c.code.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        rows.push(row(
            query,
            reply,
            query.name.clone(),
            value,
            String::new(),
            None,
            "ECU responded",
        ));
    } else if mode == 4 {
        rows.push(row(
            query,
            reply,
            query.name.clone(),
            "Clear acknowledged by this ECU".into(),
            String::new(),
            None,
            "Read DTCs again to verify the current state",
        ));
    } else if matches!(mode, 1 | 2) && matches!(query.bytes.get(1), Some(1 | 0x41)) {
        if data.len() < 4 {
            return Err("Truncated readiness response".into());
        }
        let diesel = data[1] & 8 != 0;
        let this_drive_cycle = query.bytes[1] == 0x41;
        if !this_drive_cycle {
            rows.push(row(
                query,
                reply,
                "Malfunction indicator lamp".into(),
                if data[0] & 0x80 != 0 { "On" } else { "Off" }.into(),
                String::new(),
                Some(f64::from(data[0] >> 7)),
                "ECU responded",
            ));
            rows.push(row(
                query,
                reply,
                "Number of DTCs stored in this ECU".into(),
                (data[0] & 0x7f).to_string(),
                String::new(),
                Some(f64::from(data[0] & 0x7f)),
                "ECU responded",
            ));
        }
        rows.push(row(
            query,
            reply,
            "Ignition monitor type".into(),
            if diesel { "Compression" } else { "Spark" }.into(),
            String::new(),
            None,
            "ECU responded",
        ));
        let mut monitor = |name: &str, active: bool, incomplete: bool| {
            // PID 41 reports enable bits. Only PID 01 reports monitor support.
            let value = match (active, incomplete) {
                (false, _) if this_drive_cycle => "Disabled or unsupported",
                (false, _) => "Not supported",
                (true, true) => "Not ready",
                (true, false) => "Ready",
            };
            rows.push(row(
                query,
                reply,
                name.into(),
                value.into(),
                String::new(),
                None,
                "ECU responded",
            ));
        };
        for (bit, name) in [
            (0, "Misfire monitoring"),
            (1, "Fuel system monitoring"),
            (2, "Comprehensive component monitoring"),
        ] {
            monitor(
                name,
                data[1] & (1 << bit) != 0,
                data[1] & (1 << (bit + 4)) != 0,
            );
        }
        let spark = [
            (0, "Catalyst monitoring"),
            (1, "Heated catalyst monitoring"),
            (2, "Evaporative system monitoring"),
            (3, "Secondary air monitoring"),
            // Legacy SAE J1979 used bit 4 for A/C refrigerant monitoring.
            (4, "A/C refrigerant monitoring"),
            (5, "Oxygen sensor monitoring"),
            (6, "Oxygen sensor heater monitoring"),
            (7, "EGR/VVT monitoring"),
        ];
        let compression = [
            (0, "NMHC catalyst monitoring"),
            (1, "NOx/SCR monitoring"),
            (3, "Boost pressure monitoring"),
            (5, "Exhaust gas sensor monitoring"),
            (6, "Particulate filter monitoring"),
            (7, "EGR/VVT monitoring"),
        ];
        for (bit, name) in if diesel { &compression[..] } else { &spark[..] } {
            monitor(name, data[2] & (1 << bit) != 0, data[3] & (1 << bit) != 0);
        }
    } else if matches!(mode, 1 | 2) && query.bytes.get(1) == Some(&2) {
        let Some(&[high, low]) = data.get(..2) else {
            return Err("Truncated freeze-frame DTC".into());
        };
        let empty = [high, low] == [0, 0];
        rows.push(row(
            query,
            reply,
            "DTC that caused freeze-frame storage".into(),
            if empty {
                NO_FREEZE_FRAME.into()
            } else {
                crate::obd::decode_dtc_bytes(high, low)
            },
            String::new(),
            None,
            if empty {
                "ECU reports no stored freeze frame"
            } else {
                "ECU responded"
            },
        ));
    } else if mode == 5 && !can {
        return oxygen_monitor::decode(query, reply, data);
    } else if mode == 6 && query.bytes.get(1).is_some_and(|id| id % 32 == 0) {
        let bitmap = if can {
            data.get(..4)
        } else {
            legacy_monitor_bitmap(data)
        }
        .ok_or("Invalid Mode 06 support bitmap")?;
        rows.push(row(
            query,
            reply,
            query.name.clone(),
            encode_hex(bitmap),
            "raw".into(),
            None,
            "Monitor support bitmap",
        ));
    } else if mode == 6 && can {
        return monitor_rows(query, reply);
    } else if mode == 6 {
        let id = query.bytes.get(1).ok_or("Missing legacy monitor test ID")?;
        let (&component, result) = data
            .split_first()
            .filter(|(_, result)| result.len() >= 4)
            .ok_or("Truncated legacy monitor record")?;
        let (name, value) = match result {
            [v_hi, v_lo, lim_hi, lim_lo] => {
                let bound = if component & 0x80 == 0 { "max" } else { "min" };
                (
                    format!("TID {id:02X} / CID {:02X} ({bound})", component & 0x7f),
                    format!("0x{v_hi:02X}{v_lo:02X} ({bound} 0x{lim_hi:02X}{lim_lo:02X})"),
                )
            }
            _ => (
                format!("TID {id:02X} / CID {component:02X}"),
                encode_hex(result),
            ),
        };
        rows.push(row(
            query,
            reply,
            name,
            value,
            "raw".into(),
            None,
            LEGACY_MONITOR_RAW,
        ));
    } else if mode == 9 {
        return vehicle_information::decode(query, reply, data, can);
    } else if !query.signals.is_empty() {
        for signal in &query.signals {
            if signal["hidden"] == true {
                continue;
            }
            let name = signal["name"]
                .as_str()
                .unwrap_or("Unnamed signal")
                .to_string();
            match signal_value(signal, data) {
                Ok((text, number, unit)) => rows.push(row(
                    query,
                    reply,
                    name,
                    text,
                    unit,
                    number,
                    if query.target.is_some() {
                        "Read response; profile requires vehicle validation"
                    } else {
                        "ECU responded"
                    },
                )),
                Err(message) => {
                    let mut reading = row(
                        query,
                        reply,
                        name,
                        "Unavailable".into(),
                        String::new(),
                        None,
                        &message,
                    );
                    reading.signal_failure = true;
                    rows.push(reading);
                }
            }
        }
    } else {
        rows.push(row(
            query,
            reply,
            query.name.clone(),
            encode_hex(data),
            "raw".into(),
            None,
            "Raw response; no decoder definition",
        ));
    }
    Ok(rows)
}

fn decode_vin(data: &[u8], can: bool) -> Result<String, String> {
    let Some((&1, mut bytes)) = data.split_first() else {
        return Err("Missing or invalid VIN count or sequence byte".into());
    };
    if !can {
        bytes = bytes.strip_prefix(&[0, 0, 0]).unwrap_or(bytes);
    }
    if bytes.len() != 17 {
        return Err(format!(
            "VIN reply contained {} characters. Expected 17.",
            bytes.len()
        ));
    }
    let vin = std::str::from_utf8(bytes).map_err(|_| "VIN contains invalid characters")?;
    if !crate::obd::valid_vin(vin) {
        return Err("VIN contains invalid characters".into());
    }
    Ok(vin.to_ascii_uppercase())
}

fn scale(uas: u8, raw: u16) -> Option<(f64, &'static str)> {
    // SAE J1979 Appendix E. Unknown IDs stay in hexadecimal.
    let (signed, mul, add, unit) = match uas {
        1 => (false, 1.0, 0.0, ""),
        2 => (false, 0.1, 0.0, ""),
        3 => (false, 0.01, 0.0, ""),
        4 => (false, 0.001, 0.0, ""),
        7 => (false, 0.25, 0.0, "rpm"),
        8 => (false, 0.01, 0.0, "km/h"),
        9 => (false, 1.0, 0.0, "km/h"),
        0x0a => (false, 0.000122, 0.0, "V"),
        0x0b => (false, 0.001, 0.0, "V"),
        0x0c => (false, 0.01, 0.0, "V"),
        0x10 => (false, 0.001, 0.0, "s"),
        0x16 => (false, 0.1, -40.0, "C"),
        0x24 => (false, 1.0, 0.0, "count"),
        0x2f => (false, 0.01, 0.0, "%"),
        0x81 => (true, 1.0, 0.0, ""),
        0x82 => (true, 0.1, 0.0, ""),
        0x83 => (true, 0.01, 0.0, ""),
        0x84 => (true, 0.001, 0.0, ""),
        0x8b => (true, 0.001, 0.0, "V"),
        0x8c => (true, 0.01, 0.0, "V"),
        0x96 => (true, 0.1, 0.0, "C"),
        _ => return None,
    };
    Some((
        (if signed {
            (raw as i16) as f64
        } else {
            raw as f64
        }) * mul
            + add,
        unit,
    ))
}

pub fn monitor_rows(query: &Query, reply: &DiagnosticResponse) -> Result<Vec<Reading>, String> {
    // SAE J1979 §7.6. Each ISO 15765 record contains MID, TID, scaling,
    // test value, minimum and maximum.
    if reply.payload.first() != Some(&0x46) {
        return Err("Not a monitor response".into());
    }
    let &mid = query.bytes.get(1).ok_or("Missing monitor ID")?;
    let data = &reply.payload[1..];
    if data.is_empty() || data.len() % 9 != 0 {
        return Err("Incomplete Mode 06 record".into());
    }
    let mut tests = BTreeMap::new();
    let mut rows = Vec::new();
    for r in data.chunks_exact(9) {
        if r[0] != mid {
            return Err("Unexpected monitor ID in Mode 06 record".into());
        }
        if let Some(previous) = tests.insert(r[1], r) {
            if previous != r {
                return Err(format!("Conflicting records for test {:02X}", r[1]));
            }
            continue;
        }
        let value = u16::from_be_bytes([r[3], r[4]]);
        let min = u16::from_be_bytes([r[5], r[6]]);
        let max = u16::from_be_bytes([r[7], r[8]]);
        let (text, numeric, unit, status) = if (value, min, max) == (0, 0, 0) {
            (
                MONITOR_NOT_COMPLETED.into(),
                None,
                String::new(),
                MONITOR_NOT_COMPLETED,
            )
        } else {
            match (scale(r[2], value), scale(r[2], min), scale(r[2], max)) {
                (Some((v, u)), Some((lo, _)), Some((hi, _))) => (
                    format!("{v:.4} (min {lo:.4}, max {hi:.4})"),
                    Some(v),
                    u.to_string(),
                    if lo > hi {
                        MONITOR_INVALID_LIMITS
                    } else if v >= lo && v <= hi {
                        "PASS"
                    } else {
                        "FAIL"
                    },
                ),
                _ => (
                    format!(
                        "0x{value:04X} (min 0x{min:04X}, max 0x{max:04X}, scaling 0x{:02X})",
                        r[2]
                    ),
                    None,
                    "raw".into(),
                    MONITOR_UNKNOWN_SCALING,
                ),
            }
        };
        rows.push(row(
            query,
            reply,
            format!("MID {:02X} / TID {:02X}", r[0], r[1]),
            text,
            unit,
            numeric,
            status,
        ));
    }
    Ok(rows)
}

fn send(tx: &mpsc::Sender<crate::app::ObdEvent>, update: Update) {
    let _ = tx.send(crate::app::ObdEvent::Diagnostics(update));
}

/// Legacy protocols can return several application messages for one ECU.
/// Preserve complete code lists and reassemble sequenced vehicle information.
pub fn normalize_legacy(
    query: &Query,
    replies: Vec<DiagnosticResponse>,
    can: bool,
) -> Result<Vec<DiagnosticResponse>, String> {
    let mode = query.bytes.first().copied().unwrap_or(0);
    if can || !matches!(mode, 3 | 7 | 9 | 10) {
        return Ok(replies);
    }
    let info_pid = if mode == 9 {
        let pid = *query.bytes.get(1).ok_or("Missing vehicle-info PID")?;
        if information_name(pid).is_none() {
            return Ok(replies);
        }
        Some(pid)
    } else {
        None
    };
    let mut groups: BTreeMap<Option<u32>, Vec<DiagnosticResponse>> = BTreeMap::new();
    let mut other = Vec::new();
    for reply in replies {
        if reply.payload.first() == Some(&(mode + 0x40)) {
            groups.entry(reply.source).or_default().push(reply);
        } else {
            other.push(reply);
        }
    }
    for (source, mut group) in groups {
        if group.len() == 1 {
            other.push(group.remove(0));
            continue;
        }
        if info_pid.is_some() {
            group.sort_by_key(|reply| reply.payload.get(2).copied().unwrap_or(0));
            group.dedup_by(|later, earlier| later.payload == earlier.payload);
        }
        let assembled = assemble_legacy_group(mode, info_pid, &group);
        other.push(match assembled {
            Ok(payload) => DiagnosticResponse::new(source, payload),
            Err(error) => DiagnosticResponse::receive_failure(source, error.into()),
        });
    }
    Ok(other)
}

fn assemble_legacy_group(
    mode: u8,
    info_pid: Option<u8>,
    group: &[DiagnosticResponse],
) -> Result<Vec<u8>, &'static str> {
    if let Some(pid) = info_pid {
        let mut payload = vec![0x49, pid, 1];
        for (index, reply) in group.iter().enumerate() {
            if reply.payload.get(1) != Some(&pid)
                || reply.payload.get(2).copied().map(usize::from) != Some(index + 1)
                || reply.payload.len() <= 3
            {
                return Err("Incomplete or invalid legacy vehicle-information sequence");
            }
            payload.extend_from_slice(&reply.payload[3..]);
        }
        Ok(payload)
    } else {
        let mut payload = vec![mode + 0x40];
        for reply in group {
            let data = &reply.payload[1..];
            if data.len() % 2 != 0 {
                return Err("Malformed legacy DTC pairs");
            }
            payload.extend_from_slice(data);
        }
        Ok(payload)
    }
}

pub async fn execute<A: DiagnosticAdapter>(
    adapter: &mut A,
    job: Job,
    tx: &mpsc::Sender<crate::app::ObdEvent>,
) {
    let can = adapter.connection_info().protocol.contains("CAN");
    if let Some((mode, frame)) = job.discovery {
        let mut scan = SupportScan::new(mode, frame);
        while scan.support.next_range.is_some() && !job.cancelled.load(Ordering::Relaxed) {
            scan.step(adapter, 2000).await;
        }
        let cancelled = job.cancelled.load(Ordering::Relaxed);
        if scan.support.checked_through == u8::MAX && !cancelled {
            send(
                tx,
                Update::Support(job.id, mode, scan.support.reported.into_iter().collect()),
            );
        }
        send(
            tx,
            Update::Done(
                job.id,
                if cancelled {
                    "Cancelled".into()
                } else if let Some(problem) = scan.support.problem {
                    format!("Discovery incomplete. {problem}")
                } else {
                    "Support discovery completed".into()
                },
            ),
        );
        return;
    }
    for query in job.queries {
        if job.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let mode = query.bytes.first().copied().unwrap_or(0);
        if !read_service(mode) && !(job.allow_control && matches!(mode, 4 | 8)) {
            send(tx,Update::Row(job.id,Reading{source:None,request:encode_hex(&query.bytes),name:query.name,value:"Blocked".into(),unit:String::new(),raw:String::new(),numeric:None,status:"Control requires explicit confirmation; unsupported services are blocked".into(),signal_failure:false}));
            continue;
        }
        let replies = if let Some(target) = query.target {
            adapter.request_to(target, &query.bytes, 3000).await
        } else {
            adapter.request(&query.bytes, 3000).await
        };
        let replies = replies.and_then(|replies| {
            if can && mode == 9 {
                Ok(vehicle_information::normalize_can(replies))
            } else {
                normalize_legacy(&query, replies, can).map_err(Elm327Error::ProtocolError)
            }
        });
        match replies {
            Ok(replies) => {
                for reply in replies {
                    let decoded = decode(&query, &reply, can);
                    match decoded {
                        Ok(rows) => {
                            if mode == 0x19 {
                                send(
                                    tx,
                                    Update::ReplaceRows(
                                        job.id,
                                        reply.source,
                                        encode_hex(&query.bytes),
                                        rows,
                                    ),
                                );
                            } else {
                                for row in rows {
                                    send(tx, Update::Row(job.id, row));
                                }
                            }
                        }
                        Err(e) => send(
                            tx,
                            Update::Row(
                                job.id,
                                row(
                                    &query,
                                    &reply,
                                    query.name.clone(),
                                    "Unavailable".into(),
                                    String::new(),
                                    None,
                                    &e,
                                ),
                            ),
                        ),
                    }
                }
            }
            Err(e) => send(
                tx,
                Update::Row(
                    job.id,
                    Reading {
                        source: query.target.map(|t| t.response),
                        request: encode_hex(&query.bytes),
                        name: query.name,
                        value: "Unavailable".into(),
                        unit: String::new(),
                        raw: String::new(),
                        numeric: None,
                        status: e.to_string(),
                        signal_failure: false,
                    },
                ),
            ),
        }
        adapter.delay(if can { 20 } else { 60 }).await;
    }
    send(
        tx,
        Update::Done(
            job.id,
            if job.cancelled.load(Ordering::Relaxed) {
                "Cancelled".into()
            } else {
                "Requests completed; check each ECU result".into()
            },
        ),
    );
}

struct DisplayRow {
    reading: Reading,
    received: Instant,
    history: Vec<f64>,
}

pub struct Panel {
    workspace: toyota_workspace::Workspace,
    service: u8,
    frame: u8,
    selected: BTreeSet<usize>,
    support: BTreeMap<u8, Vec<u8>>,
    generic: Vec<Query>,
    toyota: Vec<Query>,
    rows: BTreeMap<String, DisplayRow>,
    filter: String,
    manual: String,
    request_header: String,
    response_header: String,
    confirm: bool,
    busy: bool,
    id: u64,
    cancel: Arc<AtomicBool>,
    status: String,
    repeat: bool,
    interval: f32,
    last_run: Instant,
    record: bool,
    record_path: String,
    toyota_active: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            workspace: toyota_workspace::Workspace::default(),
            service: 1,
            frame: 0,
            selected: BTreeSet::new(),
            support: BTreeMap::new(),
            generic: catalog(include_str!("../data/obdb/saej1979.json"), None)
                .expect("bundled generic definitions"),
            toyota: Vec::new(),
            rows: BTreeMap::new(),
            filter: String::new(),
            manual: "00".into(),
            request_header: String::new(),
            response_header: String::new(),
            confirm: false,
            busy: false,
            id: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            status: "Choose a service and discover what the ECU supports".into(),
            repeat: false,
            interval: 2.0,
            last_run: Instant::now(),
            record: false,
            record_path: "diagnostics.csv".into(),
            toyota_active: false,
        }
    }
}

impl Panel {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn reset(&mut self) {
        self.workspace.confirm_clear = false;
        self.workspace.choose_data = false;
        self.cancel.store(true, Ordering::Relaxed);
        self.id = self.id.wrapping_add(1);
        self.busy = false;
        self.repeat = false;
        self.rows.clear();
        self.support.clear();
        self.status = "Connection changed; discover ECU support again".into();
    }
    fn job(&mut self, queries: Vec<Query>, discovery: Option<(u8, u8)>, control: bool) -> Job {
        self.id = self.id.wrapping_add(1);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.busy = true;
        self.last_run = Instant::now();
        self.status = "Working… Stop cancels after the current request".into();
        Job {
            id: self.id,
            queries,
            discovery,
            cancelled: self.cancel.clone(),
            allow_control: control,
        }
    }
    pub fn update(&mut self, update: Update) {
        match update {
            Update::ReplaceRows(id, source, request, rows) if id == self.id => {
                self.rows.retain(|_, display| {
                    display.reading.source != source || display.reading.request != request
                });
                for row in rows {
                    self.update(Update::Row(id, row));
                }
            }
            Update::Row(id, reading) if id == self.id => {
                if self.record {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        use std::io::Write;
                        let exists = std::path::Path::new(&self.record_path).exists();
                        match std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&self.record_path)
                        {
                            Ok(mut f) => {
                                if !exists {
                                    let _ =
                                        writeln!(f, "time,ecu,request,name,value,unit,status,raw");
                                }
                                let fields = [
                                    chrono::Local::now().to_rfc3339(),
                                    reading.source.map(|s| format!("{s:X}")).unwrap_or_default(),
                                    reading.request.clone(),
                                    reading.name.clone(),
                                    reading.value.clone(),
                                    reading.unit.clone(),
                                    reading.status.clone(),
                                    reading.raw.clone(),
                                ];
                                if let Err(e) = writeln!(
                                    f,
                                    "{}",
                                    fields
                                        .iter()
                                        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
                                        .collect::<Vec<_>>()
                                        .join(",")
                                ) {
                                    self.status = format!("Recording failed: {e}");
                                    self.record = false;
                                }
                            }
                            Err(e) => {
                                self.status = format!("Recording failed: {e}");
                                self.record = false;
                            }
                        }
                    }
                }
                let key = format!("{:?}:{}:{}", reading.source, reading.request, reading.name);
                let row = self.rows.entry(key).or_insert_with(|| DisplayRow {
                    reading: reading.clone(),
                    received: Instant::now(),
                    history: Vec::new(),
                });
                if let Some(value) = reading.numeric {
                    row.history.push(value);
                    if row.history.len() > 120 {
                        row.history.remove(0);
                    }
                }
                row.reading = reading;
                row.received = Instant::now();
            }
            Update::Support(id, mode, pids) if id == self.id => {
                self.support.insert(mode, pids);
                self.selected.clear();
            }
            Update::Done(id, message) if id == self.id => {
                self.busy = false;
                self.status = message;
            }
            _ => {}
        }
    }
    fn generic_queries(&self) -> Vec<Query> {
        if matches!(self.service, 1 | 2) {
            let mut queries = self.generic.clone();
            if let Some(supported) = self.support.get(&self.service) {
                for pid in supported {
                    if !queries.iter().any(|q| q.bytes.get(1) == Some(pid)) {
                        queries.push(Query {
                            bytes: vec![1, *pid],
                            target: None,
                            name: format!("PID {pid:02X} (raw; no definition)"),
                            signals: Vec::new(),
                            verified: false,
                        });
                    }
                }
            }
            queries
                .into_iter()
                .map(|mut q| {
                    if self.service == 2 {
                        q.bytes[0] = 2;
                        q.bytes.push(self.frame);
                    }
                    q
                })
                .collect()
        } else if matches!(self.service, 3 | 4 | 7 | 10) {
            vec![Query {
                bytes: vec![self.service],
                target: None,
                name: SERVICES
                    .iter()
                    .find(|(s, _)| *s == self.service)
                    .unwrap()
                    .1
                    .into(),
                signals: Vec::new(),
                verified: false,
            }]
        } else if self.service == 9 {
            self.support
                .get(&9)
                .cloned()
                .unwrap_or_else(|| vec![2, 4, 6, 8, 10, 11, 13])
                .into_iter()
                .map(|pid| Query {
                    bytes: vec![9, pid],
                    target: None,
                    name: match pid {
                        2 => "VIN".into(),
                        4 => "Calibration IDs".into(),
                        6 => "Calibration verification numbers".into(),
                        8 => "In-use performance tracking (spark)".into(),
                        10 => "ECU name".into(),
                        11 => "In-use performance tracking (compression)".into(),
                        13 => "Engine serial number".into(),
                        _ => format!("Vehicle information PID {pid:02X}"),
                    },
                    signals: Vec::new(),
                    verified: false,
                })
                .collect()
        } else {
            self.support
                .get(&self.service)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|pid| Query {
                    bytes: vec![self.service, pid],
                    target: None,
                    name: format!("Service {:02X} / ID {pid:02X}", self.service),
                    signals: Vec::new(),
                    verified: false,
                })
                .collect()
        }
    }
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        toyota: bool,
        connected: bool,
        can: bool,
    ) -> Option<Job> {
        if toyota {
            return self.show_toyota_workspace(ui, connected, can);
        }
        if toyota != self.toyota_active {
            self.reset();
            self.selected.clear();
            self.repeat = false;
            self.confirm = false;
            self.toyota_active = toyota;
        }
        let mut outgoing = None;
        ui.heading(if toyota {
            "Toyota enhanced data"
        } else {
            "Generic OBD-II · Services 01–0A"
        });
        ui.label("Responses are per ECU. Unsupported, unavailable and undecoded values remain explicit. Discovery and polling do not run component controls.");
        let old = self.service;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("generic_service")
                .selected_text(SERVICES.iter().find(|(s, _)| *s == self.service).unwrap().1)
                .show_ui(ui, |ui| {
                    for (id, name) in SERVICES {
                        ui.selectable_value(&mut self.service, *id, *name);
                    }
                });
            if self.service == 2 {
                ui.label("Frame:");
                if ui
                    .add_enabled(!self.busy, egui::DragValue::new(&mut self.frame))
                    .changed()
                {
                    self.support.remove(&2);
                    self.selected.clear();
                    self.repeat = false;
                }
            }
            if ui.button("Reset discovery").clicked() {
                self.support.remove(&self.service);
            }
            if ui
                .add_enabled(
                    connected && !self.busy && matches!(self.service, 1 | 2 | 6 | 8 | 9),
                    egui::Button::new("Discover support"),
                )
                .clicked()
            {
                self.repeat = false;
                outgoing = Some(self.job(Vec::new(), Some((self.service, self.frame)), false));
            }
        });
        if old != self.service {
            self.selected.clear();
            self.repeat = false;
            self.confirm = false;
        }
        if self.service == 5 && can {
            ui.label("Mode 05 is for legacy oxygen-test results. On CAN vehicles, use Mode 06; a Mode 05 request may be unsupported.");
        }
        if self.service == 10 {
            ui.label("Permanent codes are read-only; Mode 04 cannot erase them. The ECU clears them after successful monitoring criteria are met.");
        }

        let queries = self.generic_queries();
        let supported = self.support.get(&self.service);
        let visible: Vec<usize> = queries
            .iter()
            .enumerate()
            .filter(|(_, q)| {
                supported.is_none_or(|p| q.bytes.len() < 2 || p.contains(&q.bytes[1]))
                    && (self.filter.is_empty()
                        || format!("{} {}", q.name, encode_hex(&q.bytes))
                            .to_lowercase()
                            .contains(&self.filter.to_lowercase()))
            })
            .map(|(i, _)| i)
            .collect();
        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.text_edit_singleline(&mut self.filter);
            ui.label(format!("{} commands", visible.len()));
            if ui.button("Select visible").clicked() {
                self.selected.extend(visible.iter());
            }
            if ui.button("Select none").clicked() {
                self.selected.clear();
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("command_picker")
            .max_height(170.0)
            .show(ui, |ui| {
                for index in visible {
                    let q = &queries[index];
                    let mut selected = self.selected.contains(&index);
                    let label = format!(
                        "{} {}{}",
                        q.target
                            .map(|t| format!("{:X} / {:X} ", t.request, t.response))
                            .unwrap_or_default(),
                        encode_hex(&q.bytes),
                        if q.name.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", q.name)
                        }
                    );
                    if ui.checkbox(&mut selected, label).changed() {
                        if selected {
                            self.selected.insert(index);
                        } else {
                            self.selected.remove(&index);
                        }
                    }
                }
            });
        let control = !toyota && matches!(self.service, 4 | 8);
        if control {
            self.repeat = false;
            ui.checkbox(
                &mut self.confirm,
                if self.service == 4 {
                    "Confirm clear: erase emissions codes/freeze frames and reset readiness"
                } else {
                    "Confirm execution of the selected emissions component test"
                },
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    connected && !self.busy && (!control || self.confirm),
                    egui::Button::new("Read / run selected"),
                )
                .clicked()
            {
                let selected = self
                    .selected
                    .iter()
                    .filter_map(|i| queries.get(*i).cloned())
                    .collect::<Vec<_>>();
                if !selected.is_empty() {
                    outgoing = Some(self.job(selected, None, control));
                    self.confirm = false;
                }
            }
            ui.add_enabled_ui(!control, |ui| {
                ui.checkbox(&mut self.repeat, "Repeat selected reads");
                ui.add(
                    egui::DragValue::new(&mut self.interval)
                        .range(0.5..=60.0)
                        .suffix(" s after batch"),
                );
            });
            if ui.button("Stop").clicked() {
                self.cancel.store(true, Ordering::Relaxed);
                self.repeat = false;
            }
            if ui.button("Clear results").clicked() {
                self.rows.clear();
            }
        });
        if self.repeat
            && !self.busy
            && connected
            && self.last_run.elapsed() >= Duration::from_secs_f32(self.interval)
            && outgoing.is_none()
        {
            let selected = self
                .selected
                .iter()
                .filter_map(|i| queries.get(*i).cloned())
                .filter(|q| read_service(q.bytes[0]))
                .collect::<Vec<_>>();
            if !selected.is_empty() {
                outgoing = Some(self.job(selected, None, false));
            }
        }
        ui.collapsing("Manual service request / ECU address",|ui|{
            ui.label("Suffix bytes after the selected service (e.g. 01: 0C, 02: 0C00, 05: 0101, 06: A1, 08: test ID and documented parameters). Toyota reads must begin with 21 or 22.");
            ui.horizontal(|ui|{ui.label("Hex:");ui.text_edit_singleline(&mut self.manual);ui.label("Request ECU (optional):");ui.text_edit_singleline(&mut self.request_header);ui.label("Response ECU:");ui.text_edit_singleline(&mut self.response_header);});
            if ui.add_enabled(connected&&!self.busy&&(!control||self.confirm),egui::Button::new("Send once")).clicked(){
                let bytes=decode_hex(&if toyota{self.manual.clone()}else{format!("{:02X}{}",self.service,self.manual)});
                let target=if self.request_header.trim().is_empty(){Ok(None)}else{u32::from_str_radix(self.request_header.trim(),16).and_then(|request|u32::from_str_radix(self.response_header.trim(),16).map(|response|Some(DiagnosticTarget{request,response}))).map_err(|e|e.to_string())};
                match (bytes,target) {
                    (Ok(bytes),Ok(target)) if !toyota||matches!(bytes[0],0x21|0x22)=>{outgoing=Some(self.job(vec![Query{bytes,target,name:"Manual request".into(),signals:Vec::new(),verified:false}],None,control));self.confirm=false;},
                    (Err(e),_)=>self.status=e.to_string(),(_,Err(e))=>self.status=e, _=>self.status="Toyota manual requests are limited to reads (21/22)".into(),
                }
            }
        });
        #[cfg(not(target_arch = "wasm32"))]
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.record, "Record incoming results to CSV");
            ui.text_edit_singleline(&mut self.record_path);
        });
        ui.label(&self.status);
        ui.separator();
        egui::ScrollArea::both()
            .id_salt("diagnostic_results")
            .show(ui, |ui| {
                egui::Grid::new("diagnostic_grid")
                    .striped(true)
                    .show(ui, |ui| {
                        for h in [
                            "ECU / Request",
                            "Signal",
                            "Value",
                            "Unit",
                            "Age / Status",
                            "Trend",
                            "Raw reply",
                        ] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for display in self.rows.values() {
                            let r = &display.reading;
                            ui.monospace(format!(
                                "{} / {}",
                                r.source
                                    .map(|s| format!("{s:X}"))
                                    .unwrap_or_else(|| "?".into()),
                                r.request
                            ));
                            ui.label(&r.name);
                            ui.label(&r.value);
                            ui.label(&r.unit);
                            let age = display.received.elapsed().as_secs_f32();
                            ui.label(format!(
                                "{age:.1}s · {}{}",
                                if age > 10.0 {
                                    "Snapshot / out of date · "
                                } else {
                                    ""
                                },
                                r.status
                            ));
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(100.0, 24.0), egui::Sense::hover());
                            if display.history.len() > 1 {
                                let lo = display
                                    .history
                                    .iter()
                                    .copied()
                                    .fold(f64::INFINITY, f64::min);
                                let hi = display
                                    .history
                                    .iter()
                                    .copied()
                                    .fold(f64::NEG_INFINITY, f64::max);
                                let span = (hi - lo).max(0.001);
                                let points = display
                                    .history
                                    .iter()
                                    .enumerate()
                                    .map(|(i, v)| {
                                        egui::pos2(
                                            rect.left()
                                                + rect.width() * i as f32
                                                    / (display.history.len() - 1) as f32,
                                            rect.bottom()
                                                - rect.height() * ((v - lo) / span) as f32,
                                        )
                                    })
                                    .collect();
                                ui.painter().add(egui::Shape::line(
                                    points,
                                    egui::Stroke::new(1.5_f32, egui::Color32::LIGHT_BLUE),
                                ));
                            }
                            ui.monospace(&r.raw);
                            ui.end_row();
                        }
                    });
            });
        ui.small("Definitions: OBDb SAEJ1979 | CC BY-SA 4.0. Unknown monitor scalings retain raw values.");
        outgoing
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.cancel();
    }
}
