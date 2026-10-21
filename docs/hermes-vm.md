# quickai + Hermes Agent on a VM

quickai reads Hermes usage straight from `state.db` (read-only) and serves it via CLI and MCP.
Hermes stores usage as aggregates per session x model, so granularity is "session x model",
not per API call. Cost comes from Hermes itself (`actual_cost_usd`, then `estimated_cost_usd`),
falling back to quickai's price table.

## 1. Install (Debian/Ubuntu)

Run everything as **the same Linux user that runs Hermes**: SQLite needs write access to
`state.db-shm` even for read-only WAL reads.

```bash
sudo apt-get update && sudo apt-get install -y build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo install --locked --git https://github.com/keksmd/quickai --branch feat/multi-agent-sources quickai
quickai --help
```

Update later: rerun the `cargo install` line with `--force`.

## 2. First index

```bash
quickai index --source hermes
quickai stats
quickai top --group model
quickai top --group project
```

Hermes not in `~/.hermes` (Docker volume, custom `HERMES_HOME`)? Point at it:

```bash
quickai index --source hermes --root /srv/hermes-data
```

Profiles (`~/.hermes/profiles/<name>/state.db`) are picked up automatically.
Codex / Claude Code / OpenCode on the same VM: `quickai index --source all`
(missing sources are skipped).

Index location: `~/.claude/quickai.db` (override with `QUICKAI_DB=/path/quickai.db`).

## 3. Keep the index fresh (cron)

The MCP server only reads the index; something has to refresh it.

```bash
( crontab -l 2>/dev/null; echo "*/10 * * * * $HOME/.cargo/bin/quickai index --source all >/dev/null 2>&1" ) | crontab -
```

If you use `--root` or `QUICKAI_DB`, put the same flags/env into the cron line.

## 3b. Send daily analytics to the hub

```bash
QUICKAI_PUSH_TOKEN=... bash install-push-schedule.sh --url https://agent-sync-hub.dada-tuda.ru/api/quickai/v1/ingest --host hermes-vps --at 03:30
```

On a VM without the repo: `curl -fsSLO https://raw.githubusercontent.com/keksmd/quickai/feat/multi-agent-sources/scripts/install-push-schedule.sh && bash install-push-schedule.sh ...`.
The job runs `quickai push --index` (re-index + send). Contract and flags: [push.md](push.md).

## 4. Ask Hermes about its own spend (MCP)

Add to `~/.hermes/config.yaml` (use the absolute path, `which quickai`):

```yaml
mcp_servers:
  quickai:
    command: /home/<user>/.cargo/bin/quickai
    args: ["mcp"]
```

Then `hermes gateway restart` (or restart the CLI session). Tools: `quickai_stats`,
`quickai_top`, `quickai_usage`, `quickai_task`, `quickai_cache`, ...
Ask in chat: "how much did I spend this week, by model?"

## 5. Other agents on the VM (aider, Cline, custom bots)

Anything speaking the OpenAI API can go through the capture proxy:

```bash
quickai proxy --port 8787 --upstream https://api.openai.com --tag my-bot
```

Point the agent's `base_url` at `http://127.0.0.1:8787/v1`, then
`quickai index --source proxy`. `--tag` becomes the project name.

## 6. Alternative: analyse on your laptop

Copy the DB together with its WAL and index the mirror:

```bash
rsync -a vm:.hermes/state.db vm:.hermes/state.db-wal ~/mirrors/vm-hermes/
quickai index --source hermes --root ~/mirrors/vm-hermes
```

Each root is replaced independently, so local and mirrored Hermes data coexist.
`--rebuild` drops the source on all roots; re-run `index` for each root afterwards.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `unable to open database file` on `state.db` | run as the Hermes user, or give it write access to `~/.hermes/state.db-shm` |
| `no such column` | Hermes too old: update Hermes (adapter expects `session_model_usage`, `sessions.cwd/title`) |
| `+0 turn'ов` but Hermes was used | check `sqlite3 ~/.hermes/state.db 'select count(*) from session_model_usage'` |
| `cargo install` fails on `rusqlite` | `build-essential` missing |
