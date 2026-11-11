# Custom provider (script protocol v1)

The **Custom** provider lets any AI agent or service plug in through **one executable per account**. The app
never talks to the vendor itself: it calls your script, and the script decides how to get usage, how to verify
billing, and how to run a task.

* **Pull** kinds fetch data: `usage`, `status`.
* **Push** kind executes the AI agent embedded in the script: `run`.

All dispatch lives in the script. The app only passes the *kind* and its *parameters*.

## Configuration (Accounts page, provider "custom")

| Field | Meaning |
|---|---|
| Script | **Absolute** path of an executable file. Never run through a shell. |
| Fixed arguments | Leading arguments, one per line (e.g. `--profile=work`). |
| Pull timeout | 1–600 s for `usage`/`status` (default 30). `run` uses the task's timeout. |
| Usage from script | Poll `usage` periodically. Off = enter usage manually. |

`status` is always polled (every few minutes, and again before **every** task). Changing the script or its
arguments resets the account's subscription confirmation and cached status.

## Invocation

```
<script> [fixed args…] <kind> [--name=value …]
```

* The prompt (`run` only) is written to **stdin**; it is never placed in arguments.
* The same values are in the environment:

| Variable | Value |
|---|---|
| `PLAN_BURNER_PROTOCOL` | `1` |
| `PLAN_BURNER_KIND` | `usage`, `status` or `run` |
| `PLAN_BURNER_ACCOUNT` | the account label |
| `PLAN_BURNER_<PARAM>` | each parameter (`--task-id=7` → `PLAN_BURNER_TASK_ID=7`) |

* The environment is the app's, minus Anthropic billing variables (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
  `ANTHROPIC_BASE_URL`, Bedrock/Vertex/Foundry flags). Other credentials your script needs must come from its own
  config files.
* Working directory: the script's directory for pull kinds; the validated task working directory for `run`.
* A kind the script does not implement should exit **64**.

## Kinds

### `usage` (pull)
No parameters. Print one JSON object on stdout and exit 0:

```json
{ "version": 1,
  "session_remaining": 62.5,
  "weekly_remaining": 40,
  "session_reset_at": "2026-11-03T21:00:00Z",
  "weekly_reset_at": "2026-11-06T09:00:00Z",
  "confidence": "provider",
  "plan": "Team" }
```

* Percentages are **remaining**, 0–100, or `null`/absent when unknown. At least one must be known.
* `confidence`: `provider` (measured by the vendor), `estimated`, or `manual` (default).
* Recorded as a usage snapshot with source `custom_script`. Unknown values are never treated as available; readings
  expire after the configured maximum age; a reset time in the past invalidates the reading.

### `status` (pull)
No parameters. Print on stdout, exit 0:

```json
{ "version": 1,
  "billing_mode": "subscription",
  "authenticated": true,
  "supports": ["usage", "run"],
  "message": "logged in as work" }
```

* `billing_mode`: `subscription`, `api` or `unverified`.
* `supports` (optional): kinds the script implements. Without `run` the account is **data-only** and never launches tasks.
* **Fail-closed**: a non-zero exit, timeout, invalid JSON, wrong `version`, `authenticated: false`, a stale result
  (> 60 min) or `unverified` all mean *not verified* and block execution. `api` blocks execution as API billing.
  `subscription` is still not enough: the user must also tick the subscription confirmation on the account.

### `run` (push)
Parameters: `--task-id=<id>`, `--model=<name>` (omitted if the task has no model), `--mode=<plan|review|auto_edit>` (omitted unless a run mode is configured; also `PLAN_BURNER_MODE`). `mode` is the app's provider-neutral run mode: `plan` = read-only/planning, `review` = ask before changes, `auto_edit` = file edits without prompting. Translate it to your agent's own flags, or ignore it. Prompt on stdin.
stdout is the result text (saved like any other result; treated as untrusted text).

| Exit code | Meaning |
|---|---|
| 0 | completed |
| 75 | rate / usage limit reached: the batch stops and the task waits for capacity |
| 64 | `run` not supported |
| other | failed (last stderr line is shown) |

Timeouts and Emergency STOP send SIGTERM, then SIGKILL, to the script's process group; success is reported only after
the OS confirms it is gone. Spawned descendants that leave the process group are not terminated.

## Safety model

* The script is **arbitrary code running with your privileges** and is configured only by you in the UI. Use scripts
  you trust and keep them writable only by you.
* No shell, absolute path, structured arguments, prompt on stdin only, bounded pull output (1 MB), timeouts.
* The app cannot police what a script bills. It relies on `status` plus your confirmation, fail-closed, and re-runs
  `status` before every task: if it flips to `api` or fails mid-batch, the batch stops.
* Script output is never rendered as markup and is not included in error messages.

## Versioning
`"version": 1` is required in every JSON reply. A future protocol version will use a new number; replies with an
unknown version are rejected.

## Example
See [`examples/custom-provider`](../examples/custom-provider/README.md) for a minimal dispatcher script.
