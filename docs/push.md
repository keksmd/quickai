# `quickai push` — daily analytics to a remote hub

Each machine (laptop, VPS with Hermes, ...) indexes its own agents and once a day POSTs
day-level aggregates to one receiver (agent-sync-hub). The receiver merges hosts and
harnesses and produces recommendations.

```bash
export QUICKAI_PUSH_URL=https://agent-sync-hub.dada-tuda.ru/api/quickai/v1/ingest
export QUICKAI_PUSH_TOKEN=...            # env, not --token: argv is visible in `ps`
quickai push --index
quickai push --dry-run --days 7          # print the document, send nothing
```

| Flag / env | Meaning |
|---|---|
| `--url` / `QUICKAI_PUSH_URL` | receiver endpoint |
| `--token` / `QUICKAI_PUSH_TOKEN` | sent as `Authorization: Bearer <token>` |
| `--host` / `QUICKAI_HOST` | machine name in analytics (default: `hostname`); **must be unique per machine** — the receiver replaces by host |
| `--days N` | window of N UTC days incl. today (default 7) |
| `--index` | refresh the index over every source with a default root first |
| `--with-text` | include prompt text in `tasks` (default: metadata only) |

Non-2xx response → non-zero exit with the response body.

## Daily schedule

```bash
QUICKAI_PUSH_TOKEN=... scripts/install-push-schedule.sh --url https://agent-sync-hub.dada-tuda.ru/api/quickai/v1/ingest --host macbook --at 09:00
```

macOS → LaunchAgent `dev.quickai.push`; Linux → one crontab line (re-running replaces it).
Secrets go to `~/.quickai/push.env` (mode 600), output to `~/.quickai/push.log`.
Run it on every machine with its own `--host`.

## Contract `quickai.push/v1`

`POST <url>`, `Content-Type: application/json`:

```json
{
  "schema": "quickai.push/v1",
  "host": "macbook",
  "quickai_version": "0.4.0",
  "generated_at": "2026-10-21T09:00:03.120Z",
  "window": { "since_day": "2026-10-15", "until_day": "2026-10-21", "days": 7 },
  "full_history_sources": ["hermes"],
  "daily": [
    { "day": "2026-10-20", "source": "zcode", "project": "/Users/alex/IdeaProjects/x", "model": "GLM-5.3",
      "turns": 412, "sidechain_turns": 37, "failed_turns": 3,
      "input_tokens": 1200345, "output_tokens": 90211, "cache_read_tokens": 50123456, "cache_write_tokens": 0,
      "cost_usd": 12.34 }
  ],
  "tools":  [ { "source": "zcode", "name": "bash", "calls": 812, "errors": 41 } ],
  "agents": [ { "source": "zcode", "agent_type": "Explore", "runs": 7, "turns": 136, "output_tokens": 67636, "cost_usd": 2.13 } ],
  "tasks":  [ { "id": "turn_8eeb...", "source": "zcode", "project": "/Users/alex/x", "day": "2026-10-20",
                "wall_ms": 498254, "cost_usd": 3.1, "output_tokens": 6681, "total_tokens": 4285787,
                "agent_count": 0, "turns": 126, "idle_ms": 301000, "max_gap_ms": 184000, "text": null } ],
  "stop_reasons": [ { "source": "zcode", "model": "GLM-5.3", "reason": "stop", "turns": 389 },
                    { "source": "zcode", "model": "GLM-5.3", "reason": "(null)", "turns": 20 } ],
  "latency": [ { "source": "zcode", "model": "GLM-5.3", "samples": 380, "avg_gap_ms": 8120 } ]
}
```

Semantics the receiver must follow:

- **Idempotent replace.** For a `host`, rows of `daily` for every day in
  `[window.since_day, window.until_day]` replace what was stored for that host and day
  (delete + insert in one transaction). Days outside the window are never touched —
  **except** for sources listed in `full_history_sources`: their `daily` rows are sent for the
  whole history, and the receiver replaces ALL stored rows of that `(host, source)`.
  `tools`, `agents`, `tasks`, `stop_reasons`, `latency` are snapshots: replace the host's
  previous snapshot.
  Re-sending must never double anything.
- **`source`** = harness: `claude`, `codex`, `opencode`, `hermes`, `zcode`, `proxy`
  (for `proxy`, `project` holds the `--tag` of the agent). Unknown values must be accepted.
- **Tokens.** `input_tokens` is uncached input; `cache_read_tokens` / `cache_write_tokens`
  are separate; `output_tokens` includes reasoning. Total = sum of the four.
- **`cost_usd`** is an API-list-price equivalent (subscriptions included), not a bill.
- **`failed_turns`** = calls that ended with `error` / `cancelled`.
- **Granularity.** Hermes stores only cumulative per-session x model aggregates, so its rows
  are bucketed by the session's last activity day (and move when a session resumes) — hence
  `full_history_sources`.
- **Snapshot scope.** `tools` = all-time tool calls of sessions active in the window (tool
  calls carry no timestamp); `agents` / `tasks` = runs / tasks whose last activity is in the
  window, with their full totals. Do not add them to `daily` sums.
- `tasks` is the de-duplicated union of the top 50 by cost, top 20 by `wall_ms` and top 20 by
  `turns` among tasks active in the window; `text` is `null` unless `--with-text`.
  `turns` = number of turns of the task; `idle_ms` = sum of gaps between consecutive turns of
  the task that are longer than 60 s (the harness was waiting); `max_gap_ms` = the largest
  single gap.
- `stop_reasons` = turns with `ts` in the window grouped by `source`, `model`,
  `reason` (a missing stop reason is `"(null)"`); `model` is `""` when unknown. Snapshot of the
  window: do not add to `daily` sums.
- `latency` = gaps between consecutive turns of the same session (turn timestamps in the
  window) per `source` x `model`; only gaps with 0 < gap < 120000 ms are counted (longer ones
  are user idle, not model time). `avg_gap_ms` is an integer, `samples` the number of gaps.
- Additive fields may appear in `v1`; a breaking change bumps `schema`.

Expected response: `2xx` with any JSON body (e.g. `{"accepted": true, "rows": 42}`).
