//! Starting sibling executables (the GUI from the tray, the scheduler daemon from any client).
//! Structured arguments only: no shell, no prompt, no credentials on the command line.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where to find a sibling executable: the branded name next to this binary, else the generic cargo name
/// (running from `target/`), else the bare branded name for a `PATH` lookup.
pub fn resolve_sibling(exe_dir: Option<&Path>, branded: &str, generic: &str) -> PathBuf {
    if let Some(dir) = exe_dir {
        for name in [branded, generic] {
            let p = dir.join(name);
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from(branded)
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Run `cmd` detached from this process (no stdio) and reap it when it exits.
pub fn detach(mut cmd: Command) -> std::io::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Start the sibling named `branded` (cargo name `generic`) detached.
pub fn spawn_sibling(branded: &str, generic: &str) -> Result<(), String> {
    let prog = resolve_sibling(exe_dir().as_deref(), branded, generic);
    detach(Command::new(&prog)).map_err(|e| format!("cannot start {}: {e}", prog.display()))
}

/// Start the scheduler daemon detached. The daemon itself refuses to start when one already runs
/// (`session::acquire_driver_lock`), so a repeated click is harmless.
pub fn spawn_daemon() -> Result<(), String> {
    spawn_sibling(crate::brand::BIN_DAEMON, "daemon")
}

/// Start the GUI detached.
pub fn spawn_gui() -> Result<(), String> {
    spawn_sibling(crate::brand::BIN_GUI, "gui")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_prefers_branded_then_generic_then_path() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_sibling(Some(d.path()), "brand-daemon", "daemon"),
            PathBuf::from("brand-daemon")
        );
        std::fs::write(d.path().join("daemon"), "").unwrap();
        assert_eq!(
            resolve_sibling(Some(d.path()), "brand-daemon", "daemon"),
            d.path().join("daemon")
        );
        std::fs::write(d.path().join("brand-daemon"), "").unwrap();
        assert_eq!(
            resolve_sibling(Some(d.path()), "brand-daemon", "daemon"),
            d.path().join("brand-daemon")
        );
    }
}
