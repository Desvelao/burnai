# Client feature parity (GUI vs TUI)

AGENTS.md rule 9: every client exposes the same features, and a feature missing from a client must be
listed here with a reason. Business rules, form mapping and writes live in `core` (`core::forms`,
`core::present`, `Service`). The tray is a deliberately minimal control surface (enable, pause/resume, STOP/clear STOP,
open GUI, start scheduler, quit) and is exempt. The CLI (`crates/cli`) is a scripting subset (status, tasks
list/add/import/action/reorder) and is exempt: it has no accounts, settings, burn, approvals or pause/STOP commands.

| Feature | GUI (`crates/app`) | TUI (`crates/tui`) |
|---|---|---|
| Automation on/off, policy/state badges | top bar checkbox | `E`, header |
| Pause / resume / pause until next reset | top bar | `p` / `T` |
| Start the scheduler daemon when it is not running (`launch::spawn_daemon`, `Service::scheduler_status`) | red banner with button under the top bar | `R`, status line and `[scheduler not running]` tab marker |
| Emergency STOP / clear (with confirmation in TUI) | top bar | `S` (asks `y`) / `C` |
| Dashboard: meters, billing, CLI, reasons, stale badge, provenance, usage errors | account cards | dashboard page |
| Dashboard: queue by target, forecast, notifications, working-root banner | cards | sections |
| Refresh usage / manual usage entry | buttons / inline form | `u` / `U` form |
| Run tasks here (propose batch) | button | `g` |
| Account options: confirm subscription, available, run mode, policy, ignore limits | checkboxes/combos | `c` `v` `m` `P` `i` |
| Account add / edit (all fields, schedule): grouped sections (Identity, Connection, Usage tracking, Availability, Eligibility limits), modified-field dots + unsaved count (`AccountForm::changed_fields`) | editor page; Add: found logins scanned on open, secondary sections collapsed with a one-line summary; Edit: all sections open | `a` / `e` editor page with section headings, `●` after modified fields and `n unsaved` in the title; logins scanned with Ctrl-D; sections are never collapsed (a flat scrolling list is the terminal equivalent) |
| Per-account limits and 5 h window rules | editor page | same form (override fields) |
| Why a queued task is not running (`Service::queue_status`) | amber line on each queue row + banner; dashboard 'Run tasks here…' failure lists reasons | line under the queue table, 'holding:' in the header |
| Run-now eligibility per account (`Service::run_now_blockers`) | disabled entries with the reason as tooltip in 'Run now on…' | 'not eligible: …' suffix in the account picker |
| Burn buttons: on/off (general + per account, defaults 1 h / 24 h) and Burn now with / without approvals (`Service::set_burn_before_reset`, `set_account_burn`, `burn_now`) | top bar, dashboard cards, Accounts rows; `Burn now` menu, inline confirm for 'without approvals' | `b`/`B` toggle (account/general), `w`/`O` burn now with approvals, `W`/`X` without (asks y); Burn column. Tray: not offered (minimal menu) |
| Burn before reset (global + per-account, 5 h and weekly) | Settings Limits grid, account override rows | settings and account editor fields (blank = off / inherit) |
| Discover Claude logins | button | Ctrl-D in the account form |
| Delete account (confirmation) | inline confirm | `D` asks `y` |
| Queue: filter, new/edit task (all fields, deps, expiry, schedule) | chips, editor page | `f`, editor page |
| Queue: pause/resume/cancel/stop running/retry/delete (confirm)/move | buttons, drag | `x c t d J K` |
| Queue: duplicate a task (copies deps and repeat rule) | Duplicate button | `y` |
| Queue: import tasks from a JSON / Markdown file, with check-only mode (`Service::import_file`, `core::import`) | “Import…” panel on the queue page (path, format, check only, result) | `i` (path form, result in the text viewer). Scripts: the CLI (`crates/cli`) and the control socket use the same `Service::import_tasks` |
| Task repeat rule (`every:`/`daily:`/`weekly:`), shown in the row/detail | editor field, ↻ badge | editor field, detail line |
| Queue: run now on a chosen account | menu | `r` (+ account picker) |
| Task detail, estimate suggestion | selected card, button | `Enter`, Ctrl-L |
| Load a template when creating a task (always visible, also when none exist) | “Load template” dropdown + “Manage templates…” | first field “Load template” (←/→) |
| Save the task form as a template (full defaults, same name replaces) | “Save as template” | Ctrl-O |
| Templates page: list, create, edit/rename, duplicate, delete (confirm), new task from template | Templates page + editor page | page `6`: `n` `e` `y` `D` `t` |
| Templates page: queue a template as a task now (no editor) | Queue now button | `Q` |
| Result viewer (copy, export) | inline card | `v` (scroll, `y` copy via OSC 52, `e` export) |
| Approvals: per-task selection, approve selected/all, reject | checkboxes, buttons | `[ ]` `Space` `Enter` `a` `x` |
| History: record measured usage, view result | per-row | `a`, `v` |
| History export | CSV and JSON buttons | `e` writes both |
| Audit log | collapsing section | `A` |
| Settings (all fields, validation, discard) | settings page | editor page (`7`) |
| Browse… for path fields (working dir, allowed root, results folder, profile dir, script, import file) with the system dialog; typing a path still works (`core::pick`) | “Browse…” button next to the field | Ctrl-B on the focused path field |
| Help | – | `?` |

Intentional interaction differences (same capability): drag-and-drop reordering is GUI-only
(TUI has `J`/`K`); notification "mark read" exists in `Service` but neither client shows it.
