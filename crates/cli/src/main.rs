//! Scriptable command line for the running scheduler. It never opens the database: every
//! command is one request over the control socket (`core::ipc`) to the instance that drives the
//! scheduler, so all validation and safety gates are the same as in the GUI/TUI. No credentials
//! are read or sent.
use plan_burner_core::import::{self, ImportFormat, ImportReport};
use plan_burner_core::ipc::{self, Request, Response, TaskAction};
use std::io::Read;

const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
/// Some records of an import were rejected (the others were stored).
const EXIT_PARTIAL: i32 = 3;

fn usage() -> String {
    use plan_burner_core::brand::{BIN_CLI as B, NAME};
    format!(
        "{NAME} command line (talks to the running scheduler over its control socket)

USAGE
  {B} status
  {B} tasks list [--json]
  {B} tasks add --name NAME (--prompt TEXT | --prompt-stdin) [--description TEXT]
                [--priority low|normal|high] [--provider ID|any] [--depends-on A,B]
                [--approval true|false] [--schedule RULE] [--repeat RULE]
  {B} tasks import FILE|- [--format auto|json|markdown] [--dry-run]
  {B} tasks action ID pause|resume|cancel|retry|duplicate|delete
  {B} tasks reorder ID...
  {B} --version | --help

Data directory: $PLAN_BURNER_DATA (default as the apps). A scheduler instance (GUI, TUI or
tray) must be running. Exit codes: 0 ok, 1 error, 2 usage, 3 some import records rejected."
    )
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    Version,
    Status,
    List {
        json: bool,
    },
    /// Single task as an import record.
    Add {
        record: serde_json::Value,
    },
    Import {
        file: String,
        format: ImportFormat,
        dry_run: bool,
    },
    Action {
        id: i64,
        action: TaskAction,
    },
    Reorder {
        ids: Vec<i64>,
    },
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] | ["help"] | ["--help"] | ["-h"] => Ok(Cmd::Help),
        ["--version"] | ["-V"] => Ok(Cmd::Version),
        ["status"] => Ok(Cmd::Status),
        ["tasks", "list"] => Ok(Cmd::List { json: false }),
        ["tasks", "list", "--json"] => Ok(Cmd::List { json: true }),
        ["tasks", "add", rest @ ..] => parse_add(rest),
        ["tasks", "import", rest @ ..] => parse_import(rest),
        ["tasks", "action", id, act] => Ok(Cmd::Action {
            id: id
                .parse()
                .map_err(|_| format!("\"{id}\" is not a task id"))?,
            action: match *act {
                "pause" => TaskAction::Pause,
                "resume" => TaskAction::Resume,
                "cancel" => TaskAction::Cancel,
                "retry" => TaskAction::Retry,
                "duplicate" => TaskAction::Duplicate,
                "delete" => TaskAction::Delete,
                o => return Err(format!("unknown action \"{o}\"")),
            },
        }),
        ["tasks", "reorder", ids @ ..] if !ids.is_empty() => Ok(Cmd::Reorder {
            ids: ids
                .iter()
                .map(|i| i.parse().map_err(|_| format!("\"{i}\" is not a task id")))
                .collect::<Result<_, _>>()?,
        }),
        _ => Err("unknown command".into()),
    }
}

fn parse_import(rest: &[&str]) -> Result<Cmd, String> {
    let (mut file, mut format, mut dry_run) = (None, ImportFormat::Auto, false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match *a {
            "--dry-run" => dry_run = true,
            "--format" => {
                let v = it.next().ok_or("--format needs a value")?;
                format = ImportFormat::parse(v).ok_or(format!("unknown format \"{v}\""))?;
            }
            o if o.starts_with("--") => return Err(format!("unknown option {o}")),
            o if file.is_none() => file = Some(o.to_string()),
            _ => return Err("only one file can be imported at a time".into()),
        }
    }
    Ok(Cmd::Import {
        file: file.ok_or("tasks import needs a FILE (or - for stdin)")?,
        format,
        dry_run,
    })
}

fn parse_add(rest: &[&str]) -> Result<Cmd, String> {
    let mut rec = serde_json::Map::new();
    let mut it = rest.iter();
    let mut stdin_prompt = false;
    while let Some(a) = it.next() {
        let mut val = |what: &str| -> Result<String, String> {
            it.next()
                .map(|s| s.to_string())
                .ok_or(format!("{what} needs a value"))
        };
        match *a {
            "--name" => {
                rec.insert("name".into(), val("--name")?.into());
            }
            "--prompt" => {
                rec.insert("prompt".into(), val("--prompt")?.into());
            }
            "--prompt-stdin" => stdin_prompt = true,
            "--description" => {
                rec.insert("description".into(), val("--description")?.into());
            }
            "--priority" => {
                rec.insert("priority".into(), val("--priority")?.into());
            }
            "--provider" => {
                rec.insert("provider".into(), val("--provider")?.into());
            }
            "--schedule" => {
                rec.insert("schedule".into(), val("--schedule")?.into());
            }
            "--repeat" => {
                rec.insert("repeat".into(), val("--repeat")?.into());
            }
            "--depends-on" => {
                let v: Vec<String> = val("--depends-on")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                rec.insert("depends_on".into(), v.into());
            }
            "--approval" => {
                let b = match val("--approval")?.as_str() {
                    "true" => true,
                    "false" => false,
                    o => return Err(format!("--approval must be true or false (got \"{o}\")")),
                };
                rec.insert("requires_approval".into(), b.into());
            }
            o => return Err(format!("unknown option {o}")),
        }
    }
    if !rec.contains_key("name") {
        return Err("tasks add needs --name".into());
    }
    if stdin_prompt {
        if rec.contains_key("prompt") {
            return Err("use either --prompt or --prompt-stdin".into());
        }
        // Filled in by `run` (reads stdin); marked with a null so the shape is checked here.
        rec.insert("prompt".into(), serde_json::Value::Null);
    } else if !rec.contains_key("prompt") {
        return Err("tasks add needs --prompt TEXT or --prompt-stdin".into());
    }
    Ok(Cmd::Add {
        record: serde_json::Value::Object(rec),
    })
}

fn read_stdin() -> Result<String, String> {
    let mut s = String::new();
    std::io::stdin()
        .take(import::MAX_FILE_BYTES + 1)
        .read_to_string(&mut s)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    if s.len() as u64 > import::MAX_FILE_BYTES {
        return Err("input is too large; split it".into());
    }
    Ok(s)
}

fn call(req: &Request) -> Result<Response, String> {
    ipc::call_ok(&plan_burner_core::session::data_dir(), req).map_err(|e| e.to_string())
}

fn print_report(r: &ImportReport) -> i32 {
    println!("{}", r.render());
    if r.errors.is_empty() {
        0
    } else {
        EXIT_PARTIAL
    }
}

fn run(cmd: Cmd) -> Result<i32, String> {
    match cmd {
        Cmd::Help => {
            println!("{}", usage());
            Ok(0)
        }
        Cmd::Version => {
            println!(
                "{} {}",
                plan_burner_core::brand::NAME,
                plan_burner_core::brand::VERSION
            );
            Ok(0)
        }
        Cmd::Status => match call(&Request::Status)? {
            Response::Status { status } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status).map_err(|e| e.to_string())?
                );
                Ok(0)
            }
            _ => Err("unexpected reply".into()),
        },
        Cmd::List { json } => match call(&Request::ListTasks)? {
            Response::Tasks { tasks } => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&tasks).map_err(|e| e.to_string())?
                    );
                } else {
                    for t in tasks {
                        println!(
                            "{:>5}  {:<20} p{}  {}",
                            t.id,
                            t.status.as_str(),
                            t.priority,
                            t.name
                        );
                    }
                }
                Ok(0)
            }
            _ => Err("unexpected reply".into()),
        },
        Cmd::Add { mut record } => {
            if record["prompt"].is_null() {
                record["prompt"] = read_stdin()?.into();
            }
            import_text(
                &serde_json::to_string(&[record]).map_err(|e| e.to_string())?,
                ImportFormat::Json,
                false,
            )
        }
        Cmd::Import {
            file,
            format,
            dry_run,
        } => {
            let (text, format) = if file == "-" {
                (read_stdin()?, format)
            } else {
                import::read_file(&file, format).map_err(|e| e.to_string())?
            };
            import_text(&text, format, dry_run)
        }
        Cmd::Action { id, action } => {
            call(&Request::TaskAction { id, action })?;
            println!("ok");
            Ok(0)
        }
        Cmd::Reorder { ids } => {
            call(&Request::ReorderTasks { ids })?;
            println!("ok");
            Ok(0)
        }
    }
}

fn import_text(text: &str, format: ImportFormat, dry_run: bool) -> Result<i32, String> {
    match call(&Request::ImportTasks {
        text: text.to_string(),
        format,
        dry_run,
    })? {
        Response::Import { report } => Ok(print_report(&report)),
        _ => Err("unexpected reply".into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_args(&args) {
        Err(e) => {
            eprintln!("error: {e}\n\n{}", usage());
            EXIT_USAGE
        }
        Ok(cmd) => run(cmd).unwrap_or_else(|e| {
            eprintln!("error: {e}");
            EXIT_ERROR
        }),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cmd, String> {
        parse_args(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_simple_commands() {
        assert_eq!(p("").unwrap(), Cmd::Help);
        assert_eq!(p("status").unwrap(), Cmd::Status);
        assert_eq!(p("tasks list --json").unwrap(), Cmd::List { json: true });
        assert_eq!(
            p("tasks action 7 cancel").unwrap(),
            Cmd::Action {
                id: 7,
                action: TaskAction::Cancel
            }
        );
        assert_eq!(
            p("tasks reorder 3 1 2").unwrap(),
            Cmd::Reorder { ids: vec![3, 1, 2] }
        );
        assert!(p("tasks action x cancel").is_err());
        assert!(p("tasks action 1 explode").is_err());
        assert!(p("tasks reorder").is_err());
        assert!(p("frobnicate").is_err());
    }

    #[test]
    fn import_options() {
        assert_eq!(
            p("tasks import a.md --dry-run --format markdown").unwrap(),
            Cmd::Import {
                file: "a.md".into(),
                format: ImportFormat::Markdown,
                dry_run: true
            }
        );
        assert!(p("tasks import").is_err());
        assert!(p("tasks import a b").is_err());
        assert!(p("tasks import a --format yaml").is_err());
        assert!(p("tasks import a --bogus").is_err());
    }

    #[test]
    fn add_builds_an_import_record() {
        let Cmd::Add { record } = p(
            "tasks add --name build --prompt go --priority high --depends-on a,b --approval false",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(record["name"], "build");
        assert_eq!(record["priority"], "high");
        assert_eq!(record["depends_on"], serde_json::json!(["a", "b"]));
        assert_eq!(record["requires_approval"], false);
        assert!(p("tasks add --prompt x").is_err(), "name required");
        assert!(p("tasks add --name x").is_err(), "prompt required");
        assert!(p("tasks add --name x --prompt y --prompt-stdin").is_err());
        assert!(p("tasks add --name x --prompt y --approval maybe").is_err());
        assert!(p("tasks add --name x --prompt-stdin").is_ok());
    }
}
