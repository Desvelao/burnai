# Custom provider example

`agent.sh` is a skeleton of the script protocol described in [`docs/CUSTOM_PROVIDER.md`](../../docs/CUSTOM_PROVIDER.md).
It is **not** a working integration: edit `usage.json` (shipped all-null on purpose, so the `usage` call reports an error until you put real
readings in it) (or replace the `usage` branch) and point `AGENT_CMD` at a
command that reads a prompt on stdin and prints the result.

1. Copy the folder somewhere only you can write to; `chmod +x agent.sh`.
2. In the app: Accounts → Add account → provider `custom`, Script = absolute path to `agent.sh`.
3. Tick *Usage from script* if `usage.json`/your `usage` branch provides readings, then **Refresh now**.
4. Tick the subscription confirmation only if the agent really uses a subscription.

Try it by hand:

```sh
PLAN_BURNER_KIND=status ./agent.sh status
echo "say hi" | AGENT_CMD=cat PLAN_BURNER_KIND=run ./agent.sh run --task-id=1
```
