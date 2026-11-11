//! Product identity injected at build time from `brand.toml` (see `build.rs`). Never hardcode these
//! strings elsewhere.

/// Display name (window/TUI titles, notifications, tray tooltip, messages).
pub const NAME: &str = env!("BRAND_NAME");
/// Machine id: data directory, database file, service/desktop file ids.
pub const SLUG: &str = env!("BRAND_SLUG");
/// Database file name inside the data directory.
pub const DB_FILE: &str = env!("BRAND_DB_FILE");
pub const VERSION: &str = env!("BRAND_VERSION");
/// Installed binary names.
pub const BIN_GUI: &str = env!("BRAND_BIN_GUI");
pub const BIN_TUI: &str = env!("BRAND_BIN_TUI");
pub const BIN_TRAY: &str = env!("BRAND_BIN_TRAY");
pub const BIN_CLI: &str = env!("BRAND_BIN_CLI");
pub const BIN_DAEMON: &str = env!("BRAND_BIN_DAEMON");

/// `"<name> <version>"`, for `--version` and About text.
pub fn name_version() -> String {
    format!("{NAME} {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_values_are_present_and_consistent() {
        assert!(!NAME.is_empty() && !SLUG.is_empty());
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(BIN_GUI != BIN_TUI && BIN_TUI != BIN_TRAY && BIN_GUI != BIN_TRAY);
        assert!(BIN_CLI != BIN_GUI && BIN_CLI != BIN_TUI && BIN_CLI != BIN_TRAY);
        assert!(BIN_DAEMON != BIN_GUI && BIN_DAEMON != BIN_TUI && BIN_DAEMON != BIN_TRAY);
        assert!(BIN_DAEMON != BIN_CLI);
        assert!(DB_FILE.ends_with(".db"));
    }
}
