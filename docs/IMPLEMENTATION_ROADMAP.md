# Implementation roadmap and status

Status reflects what is **tested in the Docker container** (`make check`: fmt, clippy `-D warnings` and all tests pass).

| Phase | Scope | Status |
|---|---|---|
| 0 | Docker env, docs, AGENTS.md, workspace | Done |
| 1 | DB + migrations, settings, logging, egui shell + navigation + dashboard | Done (GUI compiles; not launched in this environment, see limitations) |
| 2 | Provider trait, Claude adapter, billing detection, manual usage, confidence/staleness | Done |
| 3 | Task CRUD, priority, dependencies, ordering, retry/cancel/pause, restart recovery | Done |
| 4 | Sequential engine, preflight, timeout, cancellation, result files, rate-limit stop | Done (tested against a fake `claude` script, not the real CLI) |
| 5 | Scheduler gates, approvals, automatic policy, pause modes, emergency STOP | Done |
| 6 | In-app / desktop / Telegram notifications, quiet hours, dedup, redaction | Done (desktop + Telegram delivery only unit-tested with mocks) |
| 7 | History, measured vs estimated, forecast, CSV/JSON export, templates | Done |
| 8 | Scenario tests (PLAN §10), billing-safety tests | Done for all 18 scenarios at core level; packaging = release build in container only |
| 9 | Custom provider (script protocol v1), account management, automatic Claude usage | Done (tested with local test scripts / stub server) |

## Next
Real Claude CLI verification, drag-and-drop ordering, additional provider adapters, email notifications, native packaging (AppImage/deb).
