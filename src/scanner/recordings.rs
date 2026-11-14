use super::model::{
    Capture, RecordingMarker, correct_recorded_monitor, graph_request, load_json, unavailable,
    write_file,
};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) struct Preview {
    pub(super) path: PathBuf,
    pub(super) capture: Arc<Capture>,
    pub(super) readings: usize,
    pub(super) issues: usize,
    pub(super) has_numeric_data: bool,
}

impl Preview {
    pub(super) fn open(path: PathBuf) -> Result<Self, String> {
        let capture = load_json::<Capture>(&path)?;
        capture.validate()?;
        Ok(Self::new(path, capture))
    }

    pub(super) fn new(path: PathBuf, capture: Capture) -> Self {
        let mut readings = 0;
        let mut issues = 0;
        let mut has_numeric_data = false;
        for row in capture.samples.iter().flat_map(|sample| &sample.rows) {
            let failed = unavailable(row);
            readings += 1;
            issues += usize::from(failed);
            has_numeric_data |= !failed && graph_request(&row.request) && row.numeric.is_some();
        }
        Self {
            path,
            capture: Arc::new(capture),
            readings,
            issues,
            has_numeric_data,
        }
    }
}

pub(super) struct SavedRecording {
    pub(super) path: PathBuf,
    pub(super) label: String,
    search_text: String,
}

impl SavedRecording {
    pub(super) fn matches(&self, search: &str) -> bool {
        self.search_text.contains(search)
    }
}

pub(super) fn saved_recordings(directory: &Path) -> Vec<SavedRecording> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            let name = path.file_name()?.to_string_lossy();
            let stamp = recording_stamp(&path);
            let sort_at = stamp
                .map(|(_, ms)| ms)
                .or_else(|| {
                    metadata
                        .modified()
                        .ok()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_millis()
                        .try_into()
                        .ok()
                })
                .unwrap_or(0);
            let label = recording_label(&path);
            let search_text = format!("{label} {name}").to_lowercase();
            Some((
                sort_at,
                SavedRecording {
                    path,
                    label,
                    search_text,
                },
            ))
        })
        .collect();
    entries.sort_unstable_by(|(a_time, a), (b_time, b)| {
        b_time.cmp(a_time).then_with(|| a.path.cmp(&b.path))
    });
    entries.into_iter().map(|(_, entry)| entry).collect()
}

fn recording_stamp(path: &Path) -> Option<(&'static str, u64)> {
    let (kind, millis) = path.file_stem()?.to_str()?.split_once('-')?;
    let kind = match kind {
        "vehicle" => "Vehicle",
        "practice" => "Practice",
        _ => return None,
    };
    Some((kind, millis.parse().ok()?))
}

pub(super) fn recording_date(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "Date unavailable".into(),
            |date| {
                date.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d  %I:%M:%S %p")
                    .to_string()
            },
        )
}

pub(super) fn marker_position(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}.{:03}", seconds / 60, seconds % 60, ms % 1000)
}

pub(super) fn recording_label(path: &Path) -> String {
    recording_stamp(path).map_or_else(
        || {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        },
        |(kind, ms)| format!("{} · {kind}", recording_date(ms)),
    )
}

#[cfg(windows)]
pub(super) fn pick_recording(directory: &Path, save: bool) -> Result<Option<PathBuf>, String> {
    super::file_picker::pick(
        directory,
        "Scanner recordings (*.json)\0*.json\0\0",
        "json",
        save.then_some(std::ffi::OsStr::new("")),
    )
}

pub(super) fn save_csv(path: &Path, capture: &Capture) -> io::Result<()> {
    write_file(path, |out| write_csv(out, capture))
}

fn csv_time(at_ms: u64, start_ms: u64) -> (String, String) {
    let elapsed = at_ms.saturating_sub(start_ms).to_string();
    let timestamp = i64::try_from(at_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(String::new, |at| {
            at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });
    (elapsed, timestamp)
}

fn csv_marker(
    out: &mut impl Write,
    marker: &RecordingMarker,
    start_ms: u64,
    session: &str,
) -> io::Result<()> {
    let (elapsed, timestamp) = csv_time(marker.at_ms, start_ms);
    csv_row(
        out,
        [
            &elapsed,
            &timestamp,
            session,
            "Marker",
            "",
            "",
            "Event marker",
            &marker.note,
            "",
            "",
            "",
            "",
        ],
    )
}

fn write_csv(out: &mut impl Write, capture: &Capture) -> io::Result<()> {
    // A UTF-8 BOM lets Excel identify the encoding when opening the file directly.
    out.write_all(b"\xEF\xBB\xBF")?;
    out.write_all(
        b"elapsed_ms,timestamp_utc,session,event,ecu,request,item,value,numeric,unit,status,raw_reply\r\n",
    )?;
    let session = if capture.demo { "Practice" } else { "Vehicle" };
    let mut markers = capture.markers.iter().peekable();
    for sample in &capture.samples {
        while let Some(marker) = markers.next_if(|marker| marker.at_ms < sample.at_ms) {
            csv_marker(out, marker, capture.started_ms, session)?;
        }
        let (elapsed, timestamp) = csv_time(sample.at_ms, capture.started_ms);
        if sample.clear_requested {
            csv_row(
                out,
                [
                    &elapsed,
                    &timestamp,
                    session,
                    "Clear requested",
                    "",
                    "04",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                ],
            )?;
        }
        if sample.graph_break {
            csv_row(
                out,
                [
                    &elapsed,
                    &timestamp,
                    session,
                    "Graph break",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                ],
            )?;
        }
        if let Some(event) = sample.emissions_scan {
            use super::model::ScanEvent;
            let event = match event {
                ScanEvent::Started => "Emissions scan started",
                ScanEvent::Finished => "Emissions scan finished",
                ScanEvent::Interrupted => "Emissions scan interrupted",
            };
            let stage = sample.clear_scan_stage.map_or("", |stage| stage.label());
            csv_row(
                out,
                [
                    &elapsed, &timestamp, session, event, "", "", "", stage, "", "", "", "",
                ],
            )?;
        }
        for row in &sample.rows {
            let corrected = correct_recorded_monitor(row);
            let row = corrected.as_ref().unwrap_or(row);
            let ecu = row
                .source
                .map_or_else(String::new, |source| format!("{source:X}"));
            let numeric = row
                .numeric
                .filter(|n| n.is_finite() && !unavailable(row))
                .map_or_else(String::new, |n| n.to_string());
            csv_row(
                out,
                [
                    &elapsed,
                    &timestamp,
                    session,
                    if unavailable(row) {
                        "Read issue"
                    } else {
                        "Reading"
                    },
                    &ecu,
                    &row.request,
                    &row.name,
                    &row.value,
                    &numeric,
                    &row.unit,
                    &row.status,
                    &row.raw,
                ],
            )?;
        }
    }
    for marker in markers {
        csv_marker(out, marker, capture.started_ms, session)?;
    }
    Ok(())
}

fn csv_row(out: &mut impl Write, fields: [&str; 12]) -> io::Result<()> {
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            out.write_all(b",")?;
        }
        if index == 0 || index == 8 {
            // The elapsed and numeric fields are formatted numbers from the model.
            out.write_all(field.as_bytes())?;
            continue;
        }
        out.write_all(b"\"")?;
        let start = field.trim_start_matches(|c: char| c.is_whitespace() || c.is_control());
        if start.starts_with(['=', '+', '-', '@', '＝', '＋', '－', '＠'])
            || field.starts_with(['\t', '\r', '\n'])
        {
            out.write_all(b"'")?;
        }
        for (segment, text) in field.split('"').enumerate() {
            if segment > 0 {
                out.write_all(b"\"\"")?;
            }
            out.write_all(text.as_bytes())?;
        }
        out.write_all(b"\"")?;
    }
    out.write_all(b"\r\n")
}
