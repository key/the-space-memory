---
name: the-space-memory:doctor
description: Run health checks on The Space Memory daemon, embedder, and database.
user-invocable: true
---

# Doctor — Health Check

Run `tsm doctor` to check daemon, embedder, database, and vector integrity.

## Usage

```bash
cd "$CLAUDE_PROJECT_DIR" && "${CLAUDE_PLUGIN_ROOT}/bin/tsm" doctor -f json
```

## What it checks

- Daemon process status (tsmd)
- Embedder child process status
- Database integrity (FTS5 + vector tables)
- Vector backfill queue status
- Socket connectivity (daemon.sock, embedder.sock)

## Output Format

Parse the JSON and present like this:

```text
### The Space Memory — Doctor

✔ Daemon: running (pid 1234)
✔ Embedder: running (pid 5678)
✔ Database: 1,234 chunks, 1,200 vectors
⚠ Backfill: 34 chunks pending (hint: run `tsm backfill`)
✘ Socket: embedder.sock not found (hint: restart embedder)

All good. / N issue(s) found.
```

- status "ok" → ✔
- status "warning" → ⚠ (show hint)
- status "error" → ✘ (show hint)

## Troubleshooting

| Symptom | Action |
|---|---|
| Daemon not running | `tsm daemon start` |
| Embedder down | Check logs in `{state_dir}/logs/` |
| Vectors stale | `tsm backfill` to re-queue |
| DB corrupt | `tsm rebuild --force` |
