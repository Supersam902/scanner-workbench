use super::model::{Snapshot, load_json, write_json};
use super::scan_notes::Notes;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;
use std::path::Path;

pub(super) const EXTENSION: &str = "obdscan";
#[cfg(windows)]
pub(super) const FILTER: &str = "Scanner Workbench scans (*.obdscan;*.json)\0*.obdscan;*.json\0\0";
#[cfg(windows)]
pub(super) const SAVE_FILTER: &str = "Scanner Workbench scan (*.obdscan)\0*.obdscan\0\0";
const VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
pub(super) struct File<T, N = Notes> {
    version: u32,
    pub(super) scan: T,
    pub(super) notes: N,
}

impl<T: DeserializeOwned> File<T> {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let file: Self = load_json(path)?;
        if file.version != VERSION {
            return Err(format!("Unsupported scan file version {}.", file.version));
        }
        file.notes.validate()?;
        Ok(file)
    }
}

pub(super) fn is_scan_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(EXTENSION))
}

pub(super) fn write(path: &Path, scan: &Snapshot, notes: &Notes, replace: bool) -> io::Result<()> {
    write_json(
        path,
        &File {
            version: VERSION,
            scan,
            notes,
        },
        replace,
    )
}
