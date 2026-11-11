# Burnai

A local-first desktop app (Rust + [egui](https://github.com/emilk/egui)) that tracks your AI **subscription** usage per account and runs a queue of tasks on otherwise-unused capacity, with approvals, an emergency STOP, and hard safeguards against accidental pay-as-you-go API billing.

Providers: **Claude** (Pro / Max) via the user-installed Claude Code CLI, and **Custom**: any agent behind a user script that implements a small pull/push protocol ([`docs/CUSTOM_PROVIDER.md`](docs/CUSTOM_PROVIDER.md)). Multiple providers and multiple accounts per provider are supported by design (provider adapters; usage, billing checks and approvals are per account). Specification: [`PLAN.md`](PLAN.md).

## Development (Docker only)

Nothing is built or run on the host. Requires Docker + Docker Compose.

```sh
make image    # build the dev image
make test     # unit + integration tests
make clippy   # lint (warnings are errors)
make check    # fmt-check + clippy + test
make release  # release build (in the container)
make brand-test  # proves name/version/bin names come from brand.toml
make gui      # run the GUI with the host X11 display forwarded
```

For `make gui` the container needs permission to use your X server; grant the minimum yourself, e.g. `xhost +SI:localuser:$(whoami)`. Data lives in the `app-data` Docker volume (`~/.local/share/burnai` inside the container; override with `PLAN_BURNER_DATA`).

## UI

The egui theme (dark flat cards, outlined status badges, per-account cards with a provider stripe, usage meters) follows the design mockups: <https://claude.ai/artifact/VB24nhQmt318WEcfnSEYkU>. Styling lives in `crates/app/src/theme.rs` and `ui.rs`.

## Install

Per-user, no root, Linux x86_64. Pick a profile (the daemon that runs tasks and the tray are always included):

```sh
curl -fsSL https://raw.githubusercontent.com/Desvelao/burnai/main/install.sh | sh -s -- --profile gui   # or tui, cli, all
```

Profiles, manual steps, upgrade, uninstall and troubleshooting: [`docs/INSTALL.md`](docs/INSTALL.md). To build the binaries yourself, `make dist` (inside Docker) writes them to `dist/` under the names in `brand.toml` (`[bins]`), plus the systemd units, the `.desktop` file, the icon and a `MANIFEST`.

Tasks can be bulk-imported from JSON/Markdown (GUI **Import…**, TUI `i`) or scripted through the CLI over the control socket: [`docs/IMPORT_FORMAT.md`](docs/IMPORT_FORMAT.md).

The GUI and TUI offer the same features (matrix: `docs/FEATURE_PARITY.md`; press `?` in the TUI for its key bindings). Only the scheduler daemon runs tasks; the GUI, TUI, tray and CLI are clients that never drive it. The `.desktop` entry launches the GUI (with "Start scheduler" / "Start tray" actions; the TUI is started from a shell). Check the daemon with `systemctl --user status burnai-daemon.service` and `journalctl --user -u burnai-daemon.service`.

- Data lives in `${XDG_DATA_HOME:-~/.local/share}/burnai` (override with `PLAN_BURNER_DATA`); credentials are your real `~/.claude`, and `claude` is taken from your `PATH` by default (each Claude account has a "CLI command" setting: a bare program name or absolute path). Nothing is shared with the Docker volume used by `make gui`.
- The binary links only glibc (needs >= 2.35: the GUI references `GLIBC_2.35`, the TUI and tray `GLIBC_2.34`; measured with `objdump -T` on `make dist` output); X11/Wayland, OpenGL/Vulkan, libxkbcommon and fontconfig are loaded at runtime from your desktop. Optional: `xprintidle` for the idle requirement.
- Linux x86_64 only. Install with `install.sh` or by hand: [`docs/INSTALL.md`](docs/INSTALL.md).

## Releases

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`: it checks the tag equals the `brand.toml` version, runs `make check` and `make brand-test`, builds with `make dist` and publishes a GitHub Release with `<slug>-<version>-linux-x86_64.tar.gz` and its `.sha256`. To release: bump `version` in `brand.toml` and `Cargo.toml` (they must match), commit, then `git tag vX.Y.Z && git push origin vX.Y.Z`. To test packaging without publishing, run the workflow manually from the Actions tab (**Run workflow**); it also runs `make install-test` and uploads the tarball as a workflow artifact instead of creating a release.

## Environment variables

| Variable | Read by | Meaning |
|---|---|---|
| `PLAN_BURNER_DATA` | daemon, GUI, TUI, tray, CLI | Data directory (default `${XDG_DATA_HOME:-~/.local/share}/burnai`) |
| `PLAN_BURNER_TUI_LOG` | TUI | Write TUI logs to this file (off by default, logs on stderr would corrupt the screen) |
| `PLAN_BURNER_TELEGRAM_TOKEN` | daemon | Telegram bot token for notifications; never stored in the DB or logged |
| `RUST_LOG` | daemon, tray, GUI, TUI | Log filter (`tracing` `EnvFilter`, default `info`) |
| `BRAND_FILE` | build only | Alternative `brand.toml` for `cargo build` (used by `make brand-test`) |
| `PLAN_BURNER_PROTOCOL`, `_KIND`, `_ACCOUNT`, `_<PARAM>` | custom-provider scripts | Set *for* your script on every call, see [`docs/CUSTOM_PROVIDER.md`](docs/CUSTOM_PROVIDER.md) |

These names are fixed on purpose (stable external interface, not product identity; see [`docs/TECHNICAL_DECISIONS.md`](docs/TECHNICAL_DECISIONS.md) #18).

## How it works

1. Add an account (Dashboard → *Add account*). Optionally give it its own CLI profile dir (`CLAUDE_CONFIG_DIR`).
2. Tick *I confirm this account uses a subscription login*. The app additionally requires a local login to exist and **no** API/cloud-billing env vars (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, Bedrock/Vertex flags…). Otherwise the account is `Unverified`/`API` and nothing runs.
3. Usage: on the *Accounts* page use *Discover logins* (finds `~/.claude*/.credentials.json`) and enable *Automatic usage* to poll the unofficial claude.ai usage endpoint with that login's token (read-only; add/edit/delete accounts there). In Docker, mount the credentials read-only (see the commented line in `docker-compose.yml`). Or enter readings manually (*Update usage*). Readings expire after the configured max age (default 30 min); unknown/stale values are never treated as available.
4. Set the allowed working directory in Settings. Tasks can only run inside it.
5. Optional: *Burn before reset* (top bar, per account, or `b`/`B` in the TUI) spends what is left in a window's last hour (5 h) / last day (weekly) instead of keeping the reserve; *Burn now* does it immediately for one batch, either via *Approvals* or, after a confirmation, without. Stale usage and billing checks still apply.
6. Create tasks, tagged for a provider and optionally pinned to an account (or "any provider"). Review proposed batches under *Approvals*.

## Default settings

Fresh installs are conservative: nothing runs until you enable it and every batch needs your approval.

| Setting | Default |
|---|---|
| Automation enabled | off |
| Execution policy / approval | Approval / always require approval |
| Start when capacity above | 65 % remaining |
| Reserved capacity | 20 % |
| Max tasks per batch / max minutes per batch | 3 / 60 |
| Max usage age (older readings block execution) | 30 min |
| Check interval, normal / waiting | 5 / 2 min |
| Keep history | 90 days |
| Allowed working directory root | empty (no task can run until set) |
| Desktop notifications / Telegram | on / off |
| Burn before reset | off |

All of these are editable under Settings; accounts can override the limits (see `docs/TECHNICAL_DECISIONS.md` #22).

## Troubleshooting

Start with the grey/amber "why not running" line under a queued task (GUI queue row, TUI line under the queue table); it uses the same checks the scheduler uses.

- **Usage is stale or "no usage reading yet".** Automatic usage polls an unofficial endpoint that can answer 403 (Cloudflare) or change without notice. Use *Refresh now*, or enter the usage by hand (*Update usage* in the GUI, `U` in the TUI). A stale reading always blocks execution.
- **"Credentials expired" / token problems.** The app never refreshes tokens: run `claude` once to log in again.
- **"Billing not verified".** Tick *I confirm this account uses a subscription*, make sure the login file exists, and remove API-key / cloud-billing environment variables; API billing is never used.
- **"No working root is set".** Choose the allowed working directory under Settings.
- **Nothing starts although everything looks fine.** Automation is off by default and approval is required: enable *Automation* and approve the proposed batch under *Approvals*.
- **"The scheduler is not running".** Clients never run tasks themselves. Use **Start scheduler** (GUI banner, TUI `R`, tray menu) or `systemctl --user start burnai-daemon.service`. "Scheduler not responding" means the daemon is hung: restart it.

More: [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md).

## Safety model (summary)

- Subscription-only: re-checked before **every** task; child processes get a scrubbed environment with all billing variables removed.
- Stop when usage is unknown, stale, a reset has passed since the reading, a rate limit is reported, or billing can't be verified.
- Approval is single-use per batch; it never authorizes later batches.
- Emergency STOP blocks launches, drops approval requests, terminates the active process (SIGTERM → SIGKILL on its process group) and only reports success once the OS confirms.
- Prompts go to the CLI over stdin (never on the command line); external notifications never include prompts (task names only by opt-in).

See [`docs/CUSTOM_PROVIDER.md`](docs/CUSTOM_PROVIDER.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/TECHNICAL_DECISIONS.md`](docs/TECHNICAL_DECISIONS.md), [`docs/IMPLEMENTATION_ROADMAP.md`](docs/IMPLEMENTATION_ROADMAP.md) and [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md).
