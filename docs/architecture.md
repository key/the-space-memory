# Architecture

## Process Architecture

The `tsmd` daemon spawns `tsmd --embedder` and `tsmd --fs-watcher` as child
processes. The daemon is the sole DB owner; child processes are stateless.

```text
  tsmd (daemon main process)
  ┌──────────────────────────────────────────────────────┐
  │  daemon.sock ◄── tsm CLI                             │
  │     │                                                │
  │  accept loop ──► handle_request ──► DB read/write    │
  │                                                      │
  │  backfill threads ──► embedder.sock ──► chunks_vec   │
  └──────────────────────────────────────────────────────┘
        │ spawn                      │ spawn
        ▼                            ▼
  ┌──────────────────┐    ┌─────────────────────────┐
  │ tsmd --embedder  │    │ tsmd --fs-watcher       │
  │ (pure inference) │    │ (file change → Index)   │
  │ embedder.sock    │    │ daemon.sock client      │
  │ no DB access     │    │ no DB access            │
  └──────────────────┘    └─────────────────────────┘
```

## Component Responsibilities

| Component | Role |
|---|---|
| `tsmd` (daemon) | Sole DB owner. Accept loop, client handling, index queue, backfill coordination |
| `tsmd --embedder` (child) | Text → vector inference via UNIX socket. Stateless, no DB access. Crash-isolated |
| `tsmd --fs-watcher` (child) | File change monitoring via inotify/FSEvents. Sends Index requests to daemon via daemon.sock |

## Design Decisions

**The embedder does not auto-restart**: to prevent OOM restart loops, a crashed
child process remains stopped. Use `tsm doctor` to check its status.

For detailed design decision records, see [decisions/](../decisions/).
