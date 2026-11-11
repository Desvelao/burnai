//! System file/folder chooser shared by every client.
//!
//! The dialog only ever *fills a text field*; all validation stays in the
//! existing core rules. When no portal is available (or the user cancels)
//! this returns `None` and typing a path remains the fallback.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    Dir,
    File,
}

/// Directory the dialog should open in for the current text value: the path
/// itself if it is an existing directory, else its nearest existing parent.
pub fn start_dir(current: &str) -> Option<PathBuf> {
    let t = current.trim();
    if t.is_empty() {
        return None;
    }
    let mut p = Path::new(t);
    loop {
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        p = p.parent()?;
        if p.as_os_str().is_empty() {
            return None;
        }
    }
}

/// Opens the system chooser (blocking) and returns the chosen path.
pub fn pick_path(kind: PickKind, current: &str) -> Option<String> {
    let mut d = rfd::FileDialog::new();
    if let Some(dir) = start_dir(current) {
        d = d.set_directory(dir);
    }
    let chosen = match kind {
        PickKind::Dir => d.pick_folder(),
        PickKind::File => d.pick_file(),
    }?;
    Some(chosen.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_dir_uses_existing_dir_or_parent() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().to_str().unwrap();
        assert_eq!(start_dir(d), Some(t.path().to_path_buf()));
        let missing = format!("{d}/nope/deeper");
        assert_eq!(start_dir(&missing), Some(t.path().to_path_buf()));
        assert_eq!(start_dir("  "), None);
    }
}
