# Importing tasks

GUI: Queue → **Import…**. TUI: Queue view → `i`. Scripts: `<cli> tasks import FILE` (see below).
Imported tasks are queued as **pending** and keep their approval setting (default: approval
required). Nothing is started by an import. Records that fail validation are reported and
skipped; the others are stored. "Check only" validates without creating anything.

## JSON

An array of task objects, or `{"tasks": [...]}`. Unknown keys are errors.

```json
[
  {"name": "build", "prompt": "Build the project", "priority": "high"},
  {"name": "test", "prompt": "Run the tests", "depends_on": ["build"], "provider": "claude",
   "estimated_consumption": 8, "schedule": null, "requires_approval": true}
]
```

Keys: `name`, `prompt` (required); `description`, `provider` (`any` = any provider), `account_id`,
`model`, `priority` (`low|normal|high`), `estimated_consumption` (%), `estimated_minutes`,
`maximum_budget` (%), `timeout_minutes`, `retry_max`, `working_dir`, `requires_approval`,
`allow_fallback`, `schedule`, `expires` (RFC 3339), `repeat`, `depends_on` (task ids or names,
existing tasks or others in the same file).

## Markdown

With headings, each heading is a task. Leading `key: value` lines are settings (same keys as
above; also `consumption`, `minutes`, `budget`, `timeout`, `retries`, `dir`, `approval`,
`fallback`, `after`), the rest is the prompt:

```markdown
## Fix tests
priority: high
depends_on: Build

Run the suite and fix failures.
```

Without headings, each top-level `- name: prompt` bullet is a task; indented lines continue the prompt.

## Command line

`<cli>` is the `cli` binary from `brand.toml` (`make dist`). It talks to the running GUI/TUI/tray
over the control socket and never opens the database.

```
<cli> tasks import tasks.json [--format auto|json|markdown] [--dry-run]    # FILE or - for stdin
<cli> tasks add --name N --prompt "..." [--priority high] [--depends-on a,b]
<cli> tasks list [--json]
<cli> tasks action ID pause|resume|cancel|retry|duplicate|delete
<cli> tasks reorder ID...
```

Exit codes: 0 ok, 1 error (e.g. no scheduler running), 2 usage, 3 some import records rejected.
The same requests can be sent as JSON lines to `control.sock` (`list_tasks`, `import_tasks`,
`reorder_tasks`; see `core::ipc`).
