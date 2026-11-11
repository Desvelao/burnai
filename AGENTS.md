# AGENTS.md

Guidance for AI coding agents (and humans) working on **Burnai**: a local-first desktop app (Rust + egui) that watches AI subscription usage and runs a queued task list on otherwise-unused capacity. The product spec is `PLAN.md`; deviations from it are listed in `docs/TECHNICAL_DECISIONS.md`.

## Hard rules

1. **Docker only.** Never run `cargo`, `rustc`, or any build/test tool on the host. Use the `make` targets (they wrap `docker compose run --rm dev …`). Editing files on the host is fine.
2. **egui (eframe) GUI and ratatui TUI, not Tauri.** No web frontend, no Node.
   Only the daemon may drive the scheduler (`core::session::open_driver_service` / `acquire_driver_lock`); every other binary uses `open_client_service` and never locks, ticks or runs tasks.
3. **Subscription safety is the core invariant.** Never add a code path that can start a task using API-key / pay-as-you-go billing, switch auth methods, enable paid credits, or run when usage is stale/unknown. Billing-safety tests must stay green.
4. Credentials: the only permitted token access is `core::claude_usage` reading the account's own Claude Code credentials file (read-only, fresh each poll) to call the unofficial usage endpoint the user asked for. Never log, store, display, refresh or write tokens; never expose them to the `app` crate, DB, audit log or notifications. No other private/undocumented endpoints, no session cookies.
5. Never use simulated usage data in production code paths. Test fixtures live under `#[cfg(test)]` or `tests/`.
6. Custom-provider scripts are run only via `custom::call_pull` / `CustomAdapter::command` (no shell, absolute path, prompt on stdin); the runner must call `ProviderAdapter::probe` before every task.
7. Child processes use structured args (no shell); prompts go over stdin, never into a command line.
8. **No hardcoded product name, slug, version or binary names** in source: use `core::brand` (values come from `brand.toml`; `make brand-test`). The control socket must only call existing `Service` methods and never carry credentials. Exemption: the `PLAN_BURNER_*` environment variable names (data dir, TUI log, Telegram token, custom-provider protocol) are a fixed external interface, not product identity, so they stay literal (see `docs/TECHNICAL_DECISIONS.md` #18 and the README table); do not derive them from `brand`.

9. **Client parity.** Every client (GUI `crates/app`, TUI `crates/tui`, and any future one) must expose the same features. A feature added or changed in one client is added to all of them in the same change. Business rules, form↔model mapping, validation and every write live in `core` (`core::forms`, `Service`), never in a client, so clients stay thin renderers. Keep `docs/FEATURE_PARITY.md` current; a feature may be missing from a client only if that file lists it with a reason. The tray is a deliberately minimal control surface and is exempt except for what it lists.

## Commands

| Task | Command |
|---|---|
| Build image | `make image` |
| Build | `make build` |
| Test | `make test` |
| Lint (warnings are errors) | `make clippy` |
| Format / check | `make fmt` / `make fmt-check` |
| All checks | `make check` |
| Release build | `make release` |
| Host binaries in `dist/` (branded names, units, `MANIFEST`) | `make dist` |
| Test `install.sh` against `dist/` | `make install-test` |
| Run GUI (X11 forwarded) | `make gui` |
| Run scheduler daemon (in container) | `make daemon` |
| Run TUI (in container) | `make tui` |
| Shell in container | `make shell` |

## Layout

- `crates/core` – UI-free library: `db` (SQLite + migrations in `migrations/`), `models`, `settings`, `accounts`, `usage` (capacity/freshness), `safety` (billing guard), `provider` + `claude` / `custom` (adapters; see docs/CUSTOM_PROVIDER.md), `tasks` (queue), `session` (front-end glue: data dir, bootstrap, snapshot, driver lock), `forms` (client-independent form models + mapping), `present` (shared texts/severities/counts), `engine` (preflight + process runner), `runner` (sequential batches), `scheduler`, `approval`, `control` (pause/STOP), `notify`, `analytics`, `audit`, `ipc` (control socket), `brand` (identity injected from `brand.toml`).
- `crates/daemon` – headless scheduler (`daemon` bin, `[bins].daemon`): the **only** process that owns the driver lock, runs the scheduler and serves the control socket.
- `crates/tray` – ksni tray (`tray` bin, `[bins].tray`): pure client, minimal menu (opens the GUI, starts the daemon), no business rules.
- `crates/cli` – scripting client (`cli` bin, `[bins].cli`): talks to the daemon over the control socket only.
- `crates/tui` – ratatui terminal client (`burnai-tui`). Same rule: no business rules; shares `core::session` with the GUI.
- `crates/app` – eframe GUI. Contains no business rules; it calls into `core`.
- `install.sh` + `docs/INSTALL.md` – per-user installer and install guide. Changing a binary, unit file or profile means updating both and `make install-test`. `install.sh` takes every name from the tarball's `MANIFEST` (rule 8).
- `docs/` – architecture, roadmap, decisions, known limitations.

## Conventions

- Model providers/accounts generically: a **provider** (adapter type) has many **accounts**; usage, billing mode, safeguards and approvals are **per account**. Tasks target `provider_type` + optional `account_id`.
- Percentages are 0–100 "remaining". Unknown is `None` and is *never* treated as available.
- Estimates and provider/manual measurements are distinct types in the UI and DB (`confidence`, `source`).
- Add a test with each safety-relevant rule. Keep dependencies minimal.
- Update docs when behaviour changes; do not claim a feature is done unless tested in the container.
