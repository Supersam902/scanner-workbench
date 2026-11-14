use super::model::{lock_file, read_json, save_json};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

pub(super) const WORK_ORDER_LIMIT: usize = 100;
pub(super) const NOTES_LIMIT: usize = 4000;
pub(super) const UNSAVED_MESSAGE: &str = "Save or discard the edited scan notes before printing.";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub(super) struct Notes {
    pub(super) work_order: String,
    pub(super) technician: String,
}

impl Notes {
    fn validate(&self) -> Result<(), String> {
        if self.work_order.chars().count() > WORK_ORDER_LIMIT
            || self.work_order.chars().any(char::is_control)
        {
            return Err(format!(
                "Enter up to {WORK_ORDER_LIMIT} characters on one line for the work order."
            ));
        }
        if self.technician.chars().count() > NOTES_LIMIT
            || self
                .technician
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
        {
            return Err(format!(
                "Enter up to {NOTES_LIMIT} characters of technician notes."
            ));
        }
        Ok(())
    }
}

fn notes_path(scan: &Path) -> PathBuf {
    scan.with_extension("notes")
}

pub(super) fn load(scan: &Path) -> Result<Notes, String> {
    match read_json::<Notes>(&notes_path(scan)) {
        Ok(notes) => {
            notes.validate()?;
            Ok(notes)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Notes::default()),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn save(scan: &Path, expected: &Notes, draft: &Notes) -> Result<Notes, String> {
    let notes = Notes {
        work_order: draft.work_order.trim().to_owned(),
        technician: draft
            .technician
            .trim()
            .replace("\r\n", "\n")
            .replace('\r', "\n"),
    };
    notes.validate()?;
    if !scan.is_file() {
        return Err("The saved scan is unavailable. Check the scan file and try again.".into());
    }
    // Lock next to the notes so an atomic replacement can complete while the lock is held.
    let _lock = lock_file(&scan.with_extension("notes.lock")).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            "Another scanner is saving these notes. Try again.".into()
        } else {
            error.to_string()
        }
    })?;
    let current = load(scan)?;
    if &current != expected && current != notes {
        return Err("These notes changed in another window. Discard edits to reload the saved version. Copy any draft text you want to keep first.".into());
    }
    if current != notes {
        save_json(&notes_path(scan), &notes)?;
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_edit_and_failed_read_preserve_saved_scan_notes() {
        let root = std::env::temp_dir().join(format!(
            "scanner-scan-notes-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let scan = root.join("scan-1.json");
        std::fs::write(&scan, b"original scan evidence").unwrap();
        let original = load(&scan).unwrap();
        let draft = Notes {
            work_order: "  WO-17  ".into(),
            technician: "Checked at idle\r\nRepeat test".into(),
        };
        let saved = save(&scan, &original, &draft).unwrap();
        assert_eq!(saved.work_order, "WO-17");
        assert_eq!(saved.technician, "Checked at idle\nRepeat test");
        assert_eq!(save(&scan, &original, &draft).unwrap(), saved);
        assert!(
            save(&scan, &original, &Notes::default())
                .unwrap_err()
                .contains("changed in another window")
        );
        assert_eq!(load(&scan).unwrap(), saved);
        std::fs::write(notes_path(&scan), b"broken notes file").unwrap();
        assert!(save(&scan, &saved, &draft).is_err());
        assert_eq!(
            std::fs::read(notes_path(&scan)).unwrap(),
            b"broken notes file"
        );
        assert_eq!(std::fs::read(&scan).unwrap(), b"original scan evidence");
    }
}
