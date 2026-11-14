use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::null;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

pub(super) fn pick(
    directory: &Path,
    filter: &str,
    extension: &str,
    save_name: Option<&OsStr>,
) -> Result<Option<PathBuf>, String> {
    let mut file = vec![0u16; 32768];
    if let Some(name) = save_name {
        for (slot, character) in file.iter_mut().take(32767).zip(name.encode_wide()) {
            *slot = character;
        }
    }
    let current_dir = std::env::current_dir().map_err(|error| error.to_string())?;
    let filter: Vec<_> = filter.encode_utf16().collect();
    let extension: Vec<_> = extension.encode_utf16().chain([0]).collect();
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
        lpstrDefExt: if extension.len() > 1 {
            extension.as_ptr()
        } else {
            null()
        },
        Flags: OFN_EXPLORER
            | OFN_NOCHANGEDIR
            | OFN_DONTADDTORECENT
            | OFN_PATHMUSTEXIST
            | if save_name.is_some() {
                OFN_OVERWRITEPROMPT
            } else {
                OFN_FILEMUSTEXIST | OFN_HIDEREADONLY
            },
        ..Default::default()
    };
    // The strings and writable file buffer remain alive until the dialog returns.
    let selected = unsafe {
        if save_name.is_some() {
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
    if std::env::current_dir().ok().as_ref() != Some(&current_dir) {
        std::env::set_current_dir(current_dir).map_err(|error| error.to_string())?;
    }
    if selected != 0 {
        let end = file.iter().position(|&c| c == 0).unwrap_or(file.len());
        Ok(Some(PathBuf::from(OsString::from_wide(&file[..end]))))
    } else if error == 0 {
        Ok(None)
    } else {
        Err(format!(
            "Windows could not open the file picker ({error:#X})."
        ))
    }
}
