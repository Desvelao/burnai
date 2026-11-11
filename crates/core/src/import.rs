//! Bulk task import from JSON or Markdown text. Pure parsing: the result is a list of
//! [`TaskForm`]s that `Service::import_tasks` stores through the same validation as the
//! task editor (nothing is ever started by an import).

use crate::forms::TaskForm;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportFormat {
    /// Pick by content: JSON when the text starts with `[` or `{`, Markdown otherwise.
    Auto,
    Json,
    Markdown,
}

impl ImportFormat {
    /// Guess from a file name (`.json`, `.md`/`.markdown`); anything else is `Auto`.
    pub fn from_path(path: &str) -> Self {
        let p = path.to_ascii_lowercase();
        if p.ends_with(".json") {
            ImportFormat::Json
        } else if p.ends_with(".md") || p.ends_with(".markdown") {
            ImportFormat::Markdown
        } else {
            ImportFormat::Auto
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(ImportFormat::Auto),
            "json" => Some(ImportFormat::Json),
            "md" | "markdown" => Some(ImportFormat::Markdown),
            _ => None,
        }
    }
}

/// One parsed record: the form plus the dependency references (task ids or names).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportItem {
    pub form: TaskForm,
    pub depends_on: Vec<String>,
}

/// Outcome of an import. `index` is the 1-based record number in the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportReport {
    pub dry_run: bool,
    /// (record number, task id) - ids are 0 for a dry run.
    pub created: Vec<(usize, i64)>,
    pub errors: Vec<(usize, String)>,
}

impl ImportReport {
    pub fn summary(&self) -> String {
        let verb = if self.dry_run {
            "would import"
        } else {
            "imported"
        };
        format!(
            "{verb} {} task(s), {} error(s)",
            self.created.len(),
            self.errors.len()
        )
    }
}

impl ImportReport {
    /// Multi-line text for a client's result view (what was created, what was skipped and why).
    pub fn render(&self) -> String {
        let mut out = format!("{}\n", self.summary());
        for (n, id) in &self.created {
            if self.dry_run {
                out.push_str(&format!("\n#{n}: ok"));
            } else {
                out.push_str(&format!("\n#{n}: created task {id}"));
            }
        }
        for (n, e) in &self.errors {
            out.push_str(&format!("\n#{n}: {e}"));
        }
        out
    }
}

/// Largest import file read from disk (kept under the control-socket request limit).
pub const MAX_FILE_BYTES: u64 = 1_500_000;

/// Reads an import file (UTF-8, size-capped) and picks the format from its extension unless
/// `format` is explicit.
pub fn read_file(path: &str, format: ImportFormat) -> Result<(String, ImportFormat)> {
    let path = path.trim();
    if path.is_empty() {
        return Err(Error::Invalid(
            "enter the path of the file to import".into(),
        ));
    }
    let meta =
        std::fs::metadata(path).map_err(|e| Error::Invalid(format!("cannot read {path}: {e}")))?;
    if !meta.is_file() {
        return Err(Error::Invalid(format!("{path} is not a file")));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(Error::Invalid(format!(
            "{path} is larger than {} KB; split it into several files",
            MAX_FILE_BYTES / 1000
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("cannot read {path}: {e}")))?;
    let format = if format == ImportFormat::Auto {
        ImportFormat::from_path(path)
    } else {
        format
    };
    Ok((text, format))
}

/// Lenient record shared by the JSON and Markdown parsers. Unknown JSON keys are rejected so
/// typos do not silently fall back to defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    #[serde(default)]
    name: String,
    #[serde(default)]
    prompt: String,
    description: Option<String>,
    #[serde(alias = "provider_type")]
    provider: Option<String>,
    account_id: Option<i64>,
    model: Option<String>,
    priority: Option<Num>,
    estimated_consumption: Option<f64>,
    estimated_minutes: Option<i64>,
    /// Hard cap in percent (blank = none).
    maximum_budget: Option<f64>,
    timeout_minutes: Option<f64>,
    retry_max: Option<i64>,
    working_dir: Option<String>,
    requires_approval: Option<bool>,
    allow_fallback: Option<bool>,
    schedule: Option<String>,
    expires: Option<String>,
    repeat: Option<String>,
    #[serde(default)]
    depends_on: Vec<Num>,
}

/// A number or a word (priority `high`, dependency name `build`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Num {
    Int(i64),
    Text(String),
}

impl Num {
    fn text(&self) -> String {
        match self {
            Num::Int(i) => i.to_string(),
            Num::Text(s) => s.trim().to_string(),
        }
    }
}

fn priority(n: &Num) -> Result<i64> {
    match n.text().to_ascii_lowercase().as_str() {
        "low" | "0" => Ok(0),
        "normal" | "medium" | "1" => Ok(1),
        "high" | "2" => Ok(2),
        o => Err(Error::Invalid(format!(
            "priority must be low, normal or high (got \"{o}\")"
        ))),
    }
}

impl Record {
    fn into_item(self) -> Result<ImportItem> {
        let mut f = TaskForm::new_task();
        f.t.name = self.name;
        f.t.prompt = self.prompt;
        if let Some(v) = self.description {
            f.t.description = v;
        }
        if let Some(p) = self.provider {
            let p = p.trim();
            if p.is_empty() || p.eq_ignore_ascii_case("any") {
                f.any_provider = true;
                f.t.provider_type = None;
            } else {
                f.t.provider_type = Some(p.to_string());
            }
        }
        if let Some(a) = self.account_id {
            f.pinned = Some(a);
        }
        if let Some(v) = self.model {
            f.t.model = v;
        }
        if let Some(p) = &self.priority {
            f.t.priority = priority(p)?;
        }
        if let Some(v) = self.estimated_consumption {
            f.t.estimated_consumption = v;
        }
        if let Some(v) = self.estimated_minutes {
            f.t.estimated_minutes = v;
        }
        if let Some(v) = self.maximum_budget {
            f.budget_on = true;
            f.budget = v;
        }
        if let Some(v) = self.timeout_minutes {
            f.timeout_min = v;
        }
        if let Some(v) = self.retry_max {
            f.t.retry_max = v;
        }
        if let Some(v) = self.working_dir {
            f.t.working_dir = v;
        }
        if let Some(v) = self.requires_approval {
            f.t.requires_approval = v;
        }
        if let Some(v) = self.allow_fallback {
            f.t.allow_fallback = v;
        }
        if let Some(v) = self.schedule.filter(|s| !s.trim().is_empty()) {
            f.t.schedule = Some(v);
        }
        if let Some(v) = self.expires {
            f.expires = v;
        }
        if let Some(v) = self.repeat {
            f.repeat = v;
        }
        let depends_on = self
            .depends_on
            .iter()
            .map(Num::text)
            .filter(|s| !s.is_empty())
            .collect();
        Ok(ImportItem {
            form: f,
            depends_on,
        })
    }

    /// Sets one `key: value` pair from Markdown metadata.
    fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let v = value.trim();
        let num = |what: &str| -> Result<f64> {
            v.parse::<f64>()
                .map_err(|_| Error::Invalid(format!("{what} must be a number (got \"{v}\")")))
        };
        let flag = |what: &str| -> Result<bool> {
            match v.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" => Ok(true),
                "false" | "no" | "off" => Ok(false),
                _ => Err(Error::Invalid(format!(
                    "{what} must be true or false (got \"{v}\")"
                ))),
            }
        };
        match key {
            "description" => self.description = Some(v.into()),
            "provider" => self.provider = Some(v.into()),
            "account" | "account_id" => {
                self.account_id = Some(num("account")? as i64);
            }
            "model" => self.model = Some(v.into()),
            "priority" => self.priority = Some(Num::Text(v.into())),
            "consumption" | "estimated_consumption" => {
                self.estimated_consumption = Some(num("consumption")?)
            }
            "minutes" | "estimated_minutes" => {
                self.estimated_minutes = Some(num("minutes")? as i64)
            }
            "budget" | "maximum_budget" => self.maximum_budget = Some(num("budget")?),
            "timeout" | "timeout_minutes" => self.timeout_minutes = Some(num("timeout")?),
            "retries" | "retry_max" => self.retry_max = Some(num("retries")? as i64),
            "dir" | "working_dir" => self.working_dir = Some(v.into()),
            "approval" | "requires_approval" => self.requires_approval = Some(flag("approval")?),
            "fallback" | "allow_fallback" => self.allow_fallback = Some(flag("fallback")?),
            "schedule" => self.schedule = Some(v.into()),
            "expires" => self.expires = Some(v.into()),
            "repeat" => self.repeat = Some(v.into()),
            "depends_on" | "after" => {
                self.depends_on = v
                    .split(',')
                    .map(|s| Num::Text(s.trim().into()))
                    .filter(|n| !n.text().is_empty())
                    .collect()
            }
            _ => return Err(Error::Invalid(format!("unknown key \"{key}\""))),
        }
        Ok(())
    }
}

const KEYS: &[&str] = &[
    "description",
    "provider",
    "account",
    "account_id",
    "model",
    "priority",
    "consumption",
    "estimated_consumption",
    "minutes",
    "estimated_minutes",
    "budget",
    "maximum_budget",
    "timeout",
    "timeout_minutes",
    "retries",
    "retry_max",
    "dir",
    "working_dir",
    "approval",
    "requires_approval",
    "fallback",
    "allow_fallback",
    "schedule",
    "expires",
    "repeat",
    "depends_on",
    "after",
];

/// Parses `text`. Whole-file problems (not JSON, wrong shape) are `Err`; per-record problems
/// are `Err` entries in the returned list so the other records can still be imported.
pub fn parse(text: &str, format: ImportFormat) -> Result<Vec<Result<ImportItem>>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let format = match format {
        ImportFormat::Auto => {
            if matches!(text.trim_start().chars().next(), Some('[') | Some('{')) {
                ImportFormat::Json
            } else {
                ImportFormat::Markdown
            }
        }
        f => f,
    };
    let items = match format {
        ImportFormat::Json => parse_json(text)?,
        _ => parse_markdown(text),
    };
    if items.is_empty() {
        return Err(Error::Invalid("no tasks found in the input".into()));
    }
    Ok(items)
}

fn parse_json(text: &str) -> Result<Vec<Result<ImportItem>>> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let arr = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(mut o) if o.contains_key("tasks") => match o.remove("tasks") {
            Some(serde_json::Value::Array(a)) => a,
            _ => return Err(Error::Invalid("\"tasks\" must be an array".into())),
        },
        serde_json::Value::Object(o) => vec![serde_json::Value::Object(o)],
        _ => {
            return Err(Error::Invalid(
                "expected a JSON array of tasks (or {\"tasks\": [...]})".into(),
            ))
        }
    };
    Ok(arr
        .into_iter()
        .map(|v| {
            serde_json::from_value::<Record>(v)
                .map_err(|e| Error::Invalid(e.to_string()))
                .and_then(Record::into_item)
        })
        .collect())
}

/// Heading mode when the text has any `#` heading (each heading = one task: leading
/// `key: value` lines are settings, the rest is the prompt); otherwise bullet mode
/// (`- name: prompt`, indented continuation lines extend the prompt).
fn parse_markdown(text: &str) -> Vec<Result<ImportItem>> {
    let is_heading = |l: &str| {
        let t = l.trim_start_matches('#');
        l.starts_with('#') && l.len() - t.len() <= 6 && t.starts_with(' ')
    };
    let mut blocks: Vec<(String, Vec<&str>)> = Vec::new();
    if text.lines().any(is_heading) {
        let mut fence = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                fence = !fence;
            }
            if !fence && is_heading(line) {
                blocks.push((line.trim_start_matches('#').trim().to_string(), Vec::new()));
            } else if let Some((_, body)) = blocks.last_mut() {
                body.push(line);
            }
        }
        blocks
            .into_iter()
            .map(|(name, body)| heading_block(name, &body))
            .collect()
    } else {
        for line in text.lines() {
            let t = line.trim_start();
            let bullet = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "));
            match bullet {
                Some(b) if line.len() == t.len() => {
                    blocks.push((b.trim().to_string(), Vec::new()));
                }
                _ => {
                    if let Some((_, body)) = blocks.last_mut() {
                        body.push(line);
                    }
                }
            }
        }
        blocks
            .into_iter()
            .map(|(first, body)| {
                let (name, prompt) = match first.split_once(": ") {
                    Some((n, p)) => (n.trim().to_string(), p.trim().to_string()),
                    None => (first.clone(), String::new()),
                };
                let mut lines = vec![prompt];
                lines.extend(body.iter().map(|l| l.trim().to_string()));
                let prompt = lines.join("\n").trim().to_string();
                let prompt = if prompt.is_empty() { first } else { prompt };
                Record {
                    name,
                    prompt,
                    ..Default::default()
                }
                .into_item()
            })
            .collect()
    }
}

fn heading_block(name: String, body: &[&str]) -> Result<ImportItem> {
    let mut rec = Record {
        name,
        ..Default::default()
    };
    let mut i = 0;
    // Leading blank lines, then `key: value` settings until the first non-setting line.
    while i < body.len() && body[i].trim().is_empty() {
        i += 1;
    }
    while i < body.len() {
        let Some((k, v)) = body[i].split_once(':') else {
            break;
        };
        let k = k.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        if !KEYS.contains(&k.as_str()) {
            break;
        }
        rec.set(&k, v)?;
        i += 1;
    }
    rec.prompt = body[i..].join("\n").trim().to_string();
    rec.into_item()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(r: &Result<ImportItem>) -> &ImportItem {
        r.as_ref().unwrap()
    }

    #[test]
    fn json_array_and_wrapped_object_parse_with_defaults() {
        let a = parse(
            r#"[{"name":"a","prompt":"do a","priority":"high","depends_on":["b",3]}]"#,
            ImportFormat::Auto,
        )
        .unwrap();
        assert_eq!(a.len(), 1);
        let it = ok(&a[0]);
        assert_eq!(it.form.t.priority, 2);
        assert!(it.form.t.requires_approval, "approval stays on by default");
        assert_eq!(it.depends_on, vec!["b", "3"]);
        let w = parse(
            r#"{"tasks":[{"name":"a","prompt":"p","provider":"any"}]}"#,
            ImportFormat::Json,
        )
        .unwrap();
        assert!(ok(&w[0]).form.any_provider);
    }

    #[test]
    fn json_unknown_keys_and_bad_priority_are_per_record_errors() {
        let r = parse(
            r#"[{"name":"a","prompt":"p","priorty":1},{"name":"b","prompt":"p","priority":"urgent"},{"name":"c","prompt":"p"}]"#,
            ImportFormat::Json,
        )
        .unwrap();
        assert!(r[0].is_err() && r[1].is_err() && r[2].is_ok());
    }

    #[test]
    fn whole_file_problems_are_errors() {
        assert!(parse("{not json", ImportFormat::Json).is_err());
        assert!(parse("   \n", ImportFormat::Auto).is_err());
        assert!(parse("[]", ImportFormat::Json).is_err());
        assert!(parse("42", ImportFormat::Json).is_err());
    }

    #[test]
    fn markdown_headings_with_settings_and_prompt() {
        let md = "# Plan\n\n## Fix tests\npriority: high\ndepends_on: 4, Build\n\nRun the suite\nand fix failures.\n\n## Docs\nWrite docs\n";
        let r = parse(md, ImportFormat::Auto).unwrap();
        assert_eq!(r.len(), 3, "the top heading is a (promptless) task too");
        assert!(
            ok(&r[0]).form.t.prompt.is_empty(),
            "an empty prompt is rejected later by task validation"
        );
        let a = ok(&r[1]);
        assert_eq!(a.form.t.name, "Fix tests");
        assert_eq!(a.form.t.priority, 2);
        assert_eq!(a.depends_on, vec!["4", "Build"]);
        assert_eq!(a.form.t.prompt, "Run the suite\nand fix failures.");
        assert_eq!(ok(&r[2]).form.t.prompt, "Write docs");
    }

    #[test]
    fn markdown_bullets_with_continuation_lines() {
        let md = "- lint: run clippy\n  and fix it\n* docs: update README\n- bare\n";
        let r = parse(md, ImportFormat::Markdown).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(ok(&r[0]).form.t.name, "lint");
        assert_eq!(ok(&r[0]).form.t.prompt, "run clippy\nand fix it");
        assert_eq!(ok(&r[1]).form.t.name, "docs");
        assert_eq!(ok(&r[2]).form.t.name, "bare");
        assert_eq!(ok(&r[2]).form.t.prompt, "bare");
    }

    #[test]
    fn markdown_unknown_setting_value_is_an_error_not_prompt_text() {
        let r = parse("## t\npriority: urgent\nbody", ImportFormat::Markdown).unwrap();
        assert!(r[0].is_err());
        let r = parse(
            "## t\nnote: this is prompt text\nmore",
            ImportFormat::Markdown,
        )
        .unwrap();
        assert_eq!(ok(&r[0]).form.t.prompt, "note: this is prompt text\nmore");
    }

    #[test]
    fn read_file_checks_path_size_and_picks_the_format() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.md");
        std::fs::write(&p, "- a: b").unwrap();
        let (t, f) = read_file(p.to_str().unwrap(), ImportFormat::Auto).unwrap();
        assert_eq!((t.as_str(), f), ("- a: b", ImportFormat::Markdown));
        assert!(read_file("", ImportFormat::Auto).is_err());
        assert!(read_file(d.path().to_str().unwrap(), ImportFormat::Auto).is_err());
        assert!(read_file("/nonexistent/x.json", ImportFormat::Auto).is_err());
    }

    #[test]
    fn report_renders_created_and_errors() {
        let r = ImportReport {
            dry_run: false,
            created: vec![(1, 7)],
            errors: vec![(2, "bad".into())],
        };
        let t = r.render();
        assert!(
            t.contains("#1: created task 7") && t.contains("#2: bad"),
            "{t}"
        );
    }

    #[test]
    fn format_from_path() {
        assert_eq!(ImportFormat::from_path("x.JSON"), ImportFormat::Json);
        assert_eq!(ImportFormat::from_path("x.md"), ImportFormat::Markdown);
        assert_eq!(ImportFormat::from_path("x.txt"), ImportFormat::Auto);
    }
}
