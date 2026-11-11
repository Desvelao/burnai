#!/bin/sh
# Minimal example of the Custom provider protocol v1 (docs/CUSTOM_PROVIDER.md).
# Not a real integration: usage comes from usage.json next to this script, and `run` calls $AGENT_CMD.
# Usage: agent.sh [fixed args] <usage|status|run> [--task-id=N] [--model=NAME]
DIR="$(cd "$(dirname "$0")" && pwd)"
KIND="$PLAN_BURNER_KIND"

case "$KIND" in
  usage)
    # Replace with a call to your vendor's usage source. Must print JSON; null = unknown.
    # NOTE: the shipped usage.json is all-null on purpose (no invented numbers), so `usage` fails
    # ("both ... unknown") until you edit it with real readings or replace this branch.
    cat "$DIR/usage.json"
    ;;
  status)
    # Report what you can verify. If unsure, say "unverified" (the app then refuses to run tasks).
    printf '{"version":1,"billing_mode":"subscription","authenticated":true,"supports":["usage","run"],"message":"example"}\n'
    ;;
  run)
    PROMPT="$(cat)"                      # prompt arrives on stdin, never in argv
    # $PLAN_BURNER_MODEL and $PLAN_BURNER_TASK_ID are available (also as --model= / --task-id=).
    if [ -z "$AGENT_CMD" ]; then echo "set AGENT_CMD to your agent command" >&2; exit 1; fi
    # Exit 75 if the agent reports a usage limit so the app stops the batch. Exit codes are 0-255, so an
    # HTTP-style 429 can't be returned: either have your agent exit 75 itself (passed through below), or
    # detect its rate-limit message in the output and map it to 75 (adapt the pattern to your agent).
    OUT="$(printf '%s' "$PROMPT" | "$AGENT_CMD" 2>&1)"
    RC=$?
    printf '%s\n' "$OUT"
    if [ "$RC" -ne 0 ] && printf '%s' "$OUT" | grep -qiE 'rate.?limit|usage limit|too many requests|429'; then
      exit 75
    fi
    exit "$RC"
    ;;
  *)
    exit 64
    ;;
esac
