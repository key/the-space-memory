---
name: doctor
description: Run health checks on The Space Memory daemon, embedder, and database.
user-invocable: true
---

# Doctor — Health Check

Run `tsm doctor` to check daemon, embedder, database, and vector integrity.

## Usage

```bash
cd "$CLAUDE_PROJECT_DIR" && "${CLAUDE_PLUGIN_ROOT}bin/tsm" doctor
```

## What it checks

- Daemon process status (tsmd)
- Embedder child process status
- Database integrity (FTS5 + vector tables)
- Vector backfill queue status
- Socket connectivity (daemon.sock, embedder.sock)

## Troubleshooting

| Symptom | Action |
|---|---|
| Daemon not running | `tsm daemon start` |
| Embedder down | Check logs in `{state_dir}/logs/` |
| Vectors stale | `tsm backfill` to re-queue |
| DB corrupt | `tsm rebuild --force` |
