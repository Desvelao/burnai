# Architecture

```
brand.toml → core/build.rs → core::brand   (name, slug, version, binary names; see decision 18)

crates/core  (library, no UI)
 ├ brand      identity injected from brand.toml (build.rs)
 ├ db, models, settings, audit, error
 ├ accounts   providers → accounts
 ├ usage      snapshots + check_capacity
 ├ safety     billing guard, env scrub
 ├ provider   adapter trait + registry
 ├ claude     Claude Code CLI adapter (+ claude_usage: optional usage polling)
 ├ custom     Custom provider: script protocol, CustomAdapter (`probe` = live status check)
 ├ tasks      queue, deps, ordering, recovery
 ├ engine     preflight() + run_process()
 ├ runner     sequential batch executor
 ├ scheduler  gate() + plan()
 ├ approval   single-use batch approvals
 ├ control    pause / STOP state + cancel flag
 ├ notify     in-app, desktop, Telegram
 ├ analytics  history, estimates, forecast, export, templates
 ├ icon       state-aware app/tray icon (code-drawn RGBA/ARGB/PNG)
 ├ import     task import (JSON/Markdown)
 ├ ipc        control socket: the daemon serves, clients forward
 ├ launch     start sibling binaries (daemon, GUI), no shell
 ├ service    orchestration used by the daemon and every client
 └ session    glue: data dir, open_driver_service (daemon), open_client_service (clients), snapshot, driver lock

crates/app  (eframe/egui, no business rules)
 ├ main.rs          App, top bar (STOP/pause/automation), tick loop
 ├ dashboard.rs     per-account cards
 ├ queue.rs         task list, editors, result viewer
 ├ accounts_view.rs account list and add/edit form
 ├ other.rs         approvals, history, settings
 ├ ui.rs            shared widgets (cards, badges, buttons)
 └ theme.rs         colours and egui style

crates/tui  (ratatui)
 ├ main.rs   App state, key handling, event loop
 └ ui.rs     rendering only

crates/daemon (headless scheduler: the only process that owns the driver lock)
 └ main.rs   open_driver_service, poll/tick loop, signal shutdown

crates/tray (ksni tray, pure client: menu → Service)
 └ main.rs   tray menu (enable, pause/resume, STOP, open GUI, start scheduler, quit)

crates/cli (scripting client over control.sock: status, tasks list/add/import/action/reorder)
 └ main.rs
```

UI styling (`theme.rs`, `ui.rs`) follows the mockup canvas https://claude.ai/artifact/VB24nhQmt318WEcfnSEYkU; views contain no business rules.

## Data model
`providers` (adapter type) 1—n `accounts` (plan, billing mode, CLI profile dir, policy). `usage_snapshots` and `executions` reference an account. `tasks` carry a *target*: `provider_type` (NULL = any provider) and `account_id` (NULL = any account of the provider; set = pinned), plus `allow_fallback`. Also: `task_dependencies`, `automation_settings` (single row), `notifications`, `audit_log`, `task_templates`. Migrations are embedded SQL applied via `PRAGMA user_version`.

## Execution flow
`Service::tick` (every 5 min, 2 min while tasks wait) → `scheduler::gate` (enabled, not paused/stopped, not busy, allowed hours, idle) → `scheduler::plan` (per account: availability switch/schedule, billing/CLI detection, fresh usage, capacity incl. reserve and consumption since the reading, batch limits, targeting) → either an approval request (`ApprovalManager`) or, only with Automatic policy on both global and account level and no per-task approval flag, a direct start. `runner::run_batch` re-runs `engine::preflight` (including account availability) before **each** task, resolves the run mode (`engine::effective_run_mode`), builds the command through the provider adapter, then `engine::run_process`.

## Processes
The scheduler daemon (`plan-burner-daemon`, normally a systemd user unit) is the only process that takes `driver.lock`, runs the scheduler and serves `control.sock`. The GUI, TUI, tray and CLI are clients: they never lock or tick. They read the shared SQLite DB directly, mirror the daemon's in-memory state (run state, pending approvals, STOP report) with `Service::refresh_remote` (once a second from `poll`) and forward every write and action over the socket. If no daemon runs, `Service::scheduler_status` reports `Offline` (or `Unreachable` when the socket accepts but does not answer) and each client offers **Start scheduler** (`core::launch::spawn_daemon`). Clients never start it on their own and never take over. The tray opens only the GUI; the TUI and CLI are started from a terminal.

## Concurrency
One batch thread at a time (atomic flag), own SQLite connection (WAL). Events flow back over an mpsc channel to the UI thread. STOP sets a shared atomic cancel flag that `run_process` polls.
