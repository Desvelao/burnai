//! Injects the product identity from `brand.toml` (repo root, or the file named by `BRAND_FILE`) as
//! compile-time constants. A missing or invalid file is a build error: there is no hardcoded fallback.
use std::{collections::HashMap, env, fs, path::PathBuf};

fn main() {
    let path = match env::var_os("BRAND_FILE") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../brand.toml"),
    };
    println!("cargo:rerun-if-env-changed=BRAND_FILE");
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read brand file {}: {e}", path.display()));
    let map = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let get = |k: &str| -> String {
        map.get(k)
            .cloned()
            .unwrap_or_else(|| panic!("{}: missing `{k}`", path.display()))
    };
    let slug = get("app.slug");
    if slug.is_empty()
        || !slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        panic!("app.slug must be non-empty lowercase [a-z0-9-], got {slug:?}");
    }
    let version = get("app.version");
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        panic!("app.version must be MAJOR.MINOR.PATCH, got {version:?}");
    }
    let pkg = env::var("CARGO_PKG_VERSION").unwrap();
    if pkg != version {
        panic!(
            "brand.toml version {version} != Cargo.toml [workspace.package] version {pkg}; keep them in sync"
        );
    }
    for k in [
        "app.name",
        "app.db_file",
        "bins.gui",
        "bins.tui",
        "bins.tray",
        "bins.cli",
        "bins.daemon",
    ] {
        let v = get(k);
        if v.trim().is_empty() || v.contains('\n') {
            panic!("{k} must be a non-empty single line");
        }
    }
    for (key, var) in [
        ("app.name", "BRAND_NAME"),
        ("app.slug", "BRAND_SLUG"),
        ("app.version", "BRAND_VERSION"),
        ("app.db_file", "BRAND_DB_FILE"),
        ("bins.gui", "BRAND_BIN_GUI"),
        ("bins.tui", "BRAND_BIN_TUI"),
        ("bins.tray", "BRAND_BIN_TRAY"),
        ("bins.cli", "BRAND_BIN_CLI"),
        ("bins.daemon", "BRAND_BIN_DAEMON"),
    ] {
        println!("cargo:rustc-env={var}={}", get(key));
    }
}

/// Minimal TOML subset: `[section]` headers and `key = "string"` lines, `#` comments.
fn parse(text: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let mut section = String::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.trim().to_string();
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or(format!("line {}: expected key = \"value\"", i + 1))?;
        let v = v.trim();
        let val = if let Some(rest) = v.strip_prefix('"') {
            let end = rest
                .find('"')
                .ok_or(format!("line {}: unterminated string", i + 1))?;
            if !rest[end + 1..].trim().is_empty() && !rest[end + 1..].trim().starts_with('#') {
                return Err(format!("line {}: trailing characters", i + 1));
            }
            rest[..end].to_string()
        } else {
            return Err(format!("line {}: value must be a quoted string", i + 1));
        };
        out.insert(format!("{section}.{}", k.trim()), val);
    }
    Ok(out)
}
