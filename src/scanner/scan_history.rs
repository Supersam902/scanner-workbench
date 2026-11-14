use super::emissions_scan::{Scan, State};
use super::model::{
    ClearScanStage, MAX_CAPTURE_ROWS, Snapshot, Vehicle, load_json, lock_file, save_json,
    valid_recorded_reading,
};
use super::recordings::recording_date;
use super::scan_comparison::{Comparison, same_vehicle};
use super::{scan_file, scan_notes};
use serde::Deserialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Deserialize)]
struct Metadata {
    vehicle: Vehicle,
    demo: bool,
    at_ms: u64,
    emissions_scan: Scan,
}

pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) started_ms: u64,
    pub(super) date: String,
    pub(super) title: String,
    pub(super) state: State,
    pub(super) clear_stage: Option<ClearScanStage>,
    vehicle: Vehicle,
    demo: bool,
    ended_ms: u64,
    search_text: String,
    work_order: String,
}

impl Entry {
    fn new(
        path: PathBuf,
        vehicle: Vehicle,
        demo: bool,
        ended_ms: u64,
        scan: &Scan,
        work_order: &str,
    ) -> Self {
        let date = recording_date(scan.started_ms);
        let title = if demo {
            "Practice scan".into()
        } else {
            vehicle.label()
        };
        let stage = scan.clear_stage.map_or("", ClearScanStage::label);
        let search_text = format!(
            "{date} {title} {stage} {} {}",
            vehicle.vin, vehicle.registration
        )
        .to_lowercase();
        Self {
            path,
            started_ms: scan.started_ms,
            date,
            title,
            state: scan.state,
            clear_stage: scan.clear_stage,
            vehicle,
            demo,
            ended_ms,
            search_text,
            work_order: work_order.to_lowercase(),
        }
    }

    pub(super) fn matches(&self, search: &str) -> bool {
        self.search_text.contains(search) || self.work_order.contains(search)
    }

    pub(super) fn precedes(&self, after: &Snapshot) -> bool {
        self.demo == after.demo
            && after.emissions_scan.as_ref().is_some_and(|scan| {
                self.started_ms < scan.started_ms && self.ended_ms <= scan.started_ms
            })
            && same_vehicle(&self.vehicle, &after.vehicle)
    }

    pub(super) fn vehicle_id(&self) -> &str {
        if !self.vehicle.registration.trim().is_empty() {
            self.vehicle.registration.trim()
        } else {
            self.vehicle.vin.trim()
        }
    }
}

struct Pending {
    path: PathBuf,
    snapshot: Arc<Snapshot>,
}

#[derive(Default)]
pub(super) struct History {
    pub(super) entries: Option<Vec<Entry>>,
    pub(super) search: String,
    pub(super) problem: Option<String>,
    pub(super) save_error: Option<String>,
    pub(super) selected: Option<(PathBuf, Arc<Snapshot>)>,
    pub(super) comparison: Option<(PathBuf, Comparison)>,
    pending: Vec<Pending>,
}

fn checked_scan(scan: Option<&Scan>, at_ms: u64) -> Result<&Scan, String> {
    scan.filter(|scan| scan.state != State::Running && scan.started_ms <= at_ms)
        .ok_or_else(|| "Invalid saved scan timeline.".into())
}

fn sort(entries: &mut [Entry]) {
    entries.sort_unstable_by(|a, b| {
        b.started_ms
            .cmp(&a.started_ms)
            .then_with(|| b.path.cmp(&a.path))
    });
}

fn available_path(dir: &Path, started_ms: u64, extension: &str) -> PathBuf {
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
    loop {
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "scan-{started_ms}-{}-{sequence}.{extension}",
            std::process::id()
        ));
        if !path.exists() && !path.with_extension("notes").exists() {
            return path;
        }
    }
}

impl History {
    pub(super) fn refresh(&mut self, dir: &Path) {
        self.problem = None;
        let paths = match fs::read_dir(dir) {
            Ok(paths) => paths,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    self.problem = Some(format!("Could not read scan history. {error}"));
                }
                self.entries = Some(Vec::new());
                return;
            }
        };
        let mut entries = Vec::new();
        let mut issues = 0;
        for entry in paths {
            let Ok(entry) = entry else {
                issues += 1;
                continue;
            };
            let path = entry.path();
            let portable = scan_file::is_scan_file(&path);
            if !(portable || legacy_scan_file(&path))
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            let item = (|| {
                let (data, notes) = if portable {
                    let file = scan_file::File::<Metadata>::load(&path)?;
                    (file.scan, file.notes)
                } else {
                    let notes = scan_notes::load(&path).unwrap_or_default();
                    (load_json::<Metadata>(&path)?, notes)
                };
                let scan = checked_scan(Some(&data.emissions_scan), data.at_ms)?;
                Ok::<_, String>(Entry::new(
                    path,
                    data.vehicle,
                    data.demo,
                    data.at_ms,
                    scan,
                    &notes.work_order,
                ))
            })();
            match item {
                Ok(entry) => entries.push(entry),
                Err(_) => issues += 1,
            }
        }
        sort(&mut entries);
        self.entries = Some(entries);
        if issues > 0 {
            self.problem = Some(format!("Could not read {issues} scan history files."));
        }
    }

    pub(super) fn select(&mut self, path: PathBuf) -> Result<(), String> {
        let snapshot = read_snapshot(&path)?;
        self.comparison = None;
        self.selected = Some((path, Arc::new(snapshot)));
        Ok(())
    }

    pub(super) fn import(&mut self, source: &Path, dir: &Path) -> Result<bool, String> {
        let (snapshot, notes) = read_import(source)?;
        let scan = validate_snapshot(&snapshot)?;
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        let _lock = lock_file(&dir.join("import.lock")).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                "Another Workbench window is importing a scan. Try again.".into()
            } else {
                error.to_string()
            }
        })?;
        self.refresh(dir);
        let existing_path = self
            .entries
            .iter()
            .flatten()
            .filter(|entry| {
                entry.started_ms == scan.started_ms
                    && entry.ended_ms == snapshot.at_ms
                    && entry.demo == snapshot.demo
            })
            .find(|entry| {
                read_import(&entry.path).is_ok_and(|(existing, existing_notes)| {
                    existing == snapshot && existing_notes == notes
                })
            })
            .map(|entry| entry.path.clone());
        let added = existing_path.is_none();
        let path = match existing_path {
            Some(path) => path,
            None => {
                let path = available_path(dir, scan.started_ms, scan_file::EXTENSION);
                scan_file::write(&path, &snapshot, &notes, false)
                    .map_err(|error| error.to_string())?;
                if let Some(entries) = &mut self.entries {
                    entries.push(Entry::new(
                        path.clone(),
                        snapshot.vehicle.clone(),
                        snapshot.demo,
                        snapshot.at_ms,
                        scan,
                        &notes.work_order,
                    ));
                    sort(entries);
                }
                path
            }
        };
        let search = self.search.trim().to_lowercase();
        if !self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.path == path && entry.matches(&search))
        {
            self.search.clear();
        }
        self.comparison = None;
        self.selected = Some((path, Arc::new(snapshot)));
        Ok(added)
    }

    pub(super) fn export(
        &self,
        mut destination: PathBuf,
        replace: bool,
        directory: &Path,
    ) -> Result<PathBuf, String> {
        let missing_extension = destination.extension().is_none();
        if missing_extension {
            destination.set_extension(scan_file::EXTENSION);
        } else if !scan_file::is_scan_file(&destination) {
            return Err("Use an .obdscan extension for the exported scan.".into());
        }
        let (source, snapshot) = self.selected.as_ref().ok_or("Choose a saved scan first.")?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let inside_history = parent == directory
            || fs::canonicalize(directory)
                .is_ok_and(|scans| fs::canonicalize(parent).is_ok_and(|parent| parent == scans));
        // The Save As prompt confirms only the chosen path. Appending an extension may target a different file.
        let replace = replace && !missing_extension && !inside_history;
        let notes = scan_notes::load(source)?;
        scan_file::write(&destination, snapshot, &notes, replace).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                "A file already exists at that path. Choose a new name.".into()
            } else {
                error.to_string()
            }
        })?;
        Ok(destination)
    }

    pub(super) fn record_work_order(&mut self, path: &Path, work_order: &str) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.path == path)
        {
            entry.work_order = work_order.to_lowercase();
        }
    }

    pub(super) fn compare_with(&mut self, path: PathBuf) -> Result<(), String> {
        self.comparison = None;
        let after = &self
            .selected
            .as_ref()
            .ok_or("Choose the later scan first.")?
            .1;
        let before = read_snapshot(&path)?;
        let comparison = Comparison::new(&before, after)?;
        self.comparison = Some((path, comparison));
        Ok(())
    }

    pub(super) fn archive(&mut self, dir: &Path, snapshot: Arc<Snapshot>) -> (PathBuf, bool) {
        let scan = snapshot
            .emissions_scan
            .as_ref()
            .expect("only captured emissions scans are archived");
        let path = available_path(dir, scan.started_ms, "json");
        self.pending.push(Pending {
            path: path.clone(),
            snapshot,
        });
        self.retry_saves();
        let saved = !self.pending.iter().any(|item| item.path == path);
        (path, saved)
    }

    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn save_pending_as(&mut self, directory: &Path) -> usize {
        let before = self.pending.len();
        self.save_pending(Some(directory));
        before - self.pending.len()
    }

    pub(super) fn retry_saves(&mut self) {
        self.save_pending(None);
    }

    fn save_pending(&mut self, directory: Option<&Path>) {
        self.save_error = None;
        let mut failed = Vec::new();
        for pending in self.pending.drain(..) {
            let recovery_path = directory
                .map(|dir| dir.join(pending.path.file_name().expect("generated scan filename")));
            let path = recovery_path.as_deref().unwrap_or(&pending.path);
            let result = if recovery_path.is_some() && path.exists() {
                Err(format!(
                    "{} already exists. Choose another folder.",
                    path.display()
                ))
            } else {
                save_json(path, pending.snapshot.as_ref())
            };
            match result {
                Ok(()) => {
                    if path == pending.path
                        && let (Some(entries), Some(scan)) =
                            (&mut self.entries, &pending.snapshot.emissions_scan)
                    {
                        entries.push(Entry::new(
                            pending.path,
                            pending.snapshot.vehicle.clone(),
                            pending.snapshot.demo,
                            pending.snapshot.at_ms,
                            scan,
                            "",
                        ));
                    }
                }
                Err(error) => {
                    self.save_error.get_or_insert(error);
                    failed.push(pending);
                }
            }
        }
        self.pending = failed;
        if let Some(entries) = &mut self.entries {
            sort(entries);
        }
    }
}

fn legacy_scan_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn read_snapshot(path: &Path) -> Result<Snapshot, String> {
    let snapshot = if scan_file::is_scan_file(path) {
        scan_file::File::<Snapshot>::load(path)?.scan
    } else {
        read_legacy_scan(path)?
    };
    check_timeline(&snapshot)?;
    Ok(snapshot)
}

fn read_import(path: &Path) -> Result<(Snapshot, scan_notes::Notes), String> {
    if scan_file::is_scan_file(path) {
        let file = scan_file::File::<Snapshot>::load(path)?;
        return Ok((file.scan, file.notes));
    }
    Ok((read_legacy_scan(path)?, scan_notes::load(path)?))
}

fn read_legacy_scan(path: &Path) -> Result<Snapshot, String> {
    if !legacy_scan_file(path) {
        return Err("Choose a Workbench .obdscan file or a saved scan .json file.".into());
    }
    load_json(path)
}

fn check_timeline(snapshot: &Snapshot) -> Result<&Scan, String> {
    let scan = checked_scan(snapshot.emissions_scan.as_ref(), snapshot.at_ms)?;
    if snapshot
        .rows
        .iter()
        .chain(&snapshot.failures)
        .any(|row| !(scan.started_ms..=snapshot.at_ms).contains(&row.at_ms))
    {
        return Err("A reading falls outside the saved scan timeline.".into());
    }
    Ok(scan)
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<&Scan, String> {
    let scan = check_timeline(snapshot)?;
    if scan.ended_ms.is_some_and(|end| end != snapshot.at_ms) {
        return Err("The saved scan and its readings have different end times.".into());
    }
    if snapshot.rows.len().saturating_add(snapshot.failures.len()) > MAX_CAPTURE_ROWS
        || snapshot
            .rows
            .iter()
            .chain(&snapshot.failures)
            .any(|row| !valid_recorded_reading(&row.reading))
    {
        return Err("The scan contains invalid or oversized readings.".into());
    }
    Ok(scan)
}
