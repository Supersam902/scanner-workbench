use super::model::{Capture, correct_recorded_monitor, load_json, unavailable, write_file};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) struct Preview {
    pub(super) path: PathBuf,
    pub(super) capture: Arc<Capture>,
    pub(super) readings: usize,
    pub(super) issues: usize,
}

impl Preview {
    pub(super) fn open(path: PathBuf) -> Result<Self, String> {
        let capture = load_json::<Capture>(&path)?;
        capture.validate()?;
        Ok(Self::new(path, capture))
    }

    pub(super) fn new(path: PathBuf, capture: Capture) -> Self {
        let readings = capture.row_count();
        let issues = capture
            .samples
            .iter()
            .flat_map(|sample| &sample.rows)
            .filter(|row| unavailable(row))
            .count();
        Self {
            path,
            capture: Arc::new(capture),
            readings,
            issues,
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
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::ptr::null;
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    let mut file = vec![0u16; 32768];
    let current_dir = std::env::current_dir().map_err(|error| error.to_string())?;
    let filter: Vec<_> = "Scanner recordings (*.json)\0*.json\0\0"
        .encode_utf16()
        .collect();
    let extension: Vec<_> = "json\0".encode_utf16().collect();
    let initial: Vec<_> = directory.as_os_str().encode_wide().chain([0]).collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        // The button is handled on the scanner window's UI thread.
        hwndOwner: unsafe { GetActiveWindow() },
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: file.as_mut_ptr(),
        nMaxFile: file.len() as u32,
        lpstrInitialDir: if directory.is_dir() {
            initial.as_ptr()
        } else {
            null()
        },
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_EXPLORER
            | OFN_NOCHANGEDIR
            | OFN_DONTADDTORECENT
            | OFN_PATHMUSTEXIST
            | if save {
                OFN_OVERWRITEPROMPT
            } else {
                OFN_FILEMUSTEXIST | OFN_HIDEREADONLY
            },
        ..Default::default()
    };
    // The strings and writable file buffer remain alive until the dialog returns.
    let selected = unsafe {
        if save {
            GetSaveFileNameW(&mut dialog)
        } else {
            GetOpenFileNameW(&mut dialog)
        }
    };
    let error = if selected == 0 {
        unsafe { CommDlgExtendedError() }
    } else {
        0
    };
    // GetOpenFileName can change the process directory despite OFN_NOCHANGEDIR.
    if std::env::current_dir().ok().as_ref() != Some(&current_dir) {
        std::env::set_current_dir(current_dir).map_err(|error| error.to_string())?;
    }
    if selected != 0 {
        let end = file.iter().position(|&c| c == 0).unwrap_or(file.len());
        return Ok(Some(PathBuf::from(OsString::from_wide(&file[..end]))));
    }
    if error == 0 {
        Ok(None)
    } else {
        Err(format!(
            "Windows could not open the file picker ({error:#X})."
        ))
    }
}

pub(super) fn save_csv(path: &Path, capture: &Capture) -> io::Result<()> {
    write_file(path, |out| write_csv(out, capture))
}

fn write_csv(out: &mut impl Write, capture: &Capture) -> io::Result<()> {
    // A UTF-8 BOM lets Excel identify the encoding when opening the file directly.
    out.write_all(b"\xEF\xBB\xBF")?;
    out.write_all(
        b"elapsed_ms,timestamp_utc,session,event,ecu,request,item,value,numeric,unit,status,raw_reply\r\n",
    )?;
    let session = if capture.demo { "Practice" } else { "Vehicle" };
    for sample in &capture.samples {
        let elapsed = sample.at_ms.saturating_sub(capture.started_ms).to_string();
        let timestamp = i64::try_from(sample.at_ms)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map_or_else(String::new, |at| {
                at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            });
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
            csv_row(
                out,
                [
                    &elapsed, &timestamp, session, event, "", "", "", "", "", "", "", "",
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
