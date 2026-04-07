# Architecture

## Process Architecture

The `tsmd` daemon manages `tsmd --embedder` as a child process and runs the file
watcher (`tsmd --fs-watcher`) as a separate child process.

```text
┌─────────────────────────────────────────────────────────┐
│  tsmd (main daemon)                                     │
│                                                         │
│  - DB owner (all reads & writes)                        │
│  - Client response (UNIX socket)                        │
│  - Indexer queue / vector queue                          │
│  - Backfill management                                  │
│  - Spawns child processes                                │
│                                                         │
│   ┌──────────────────┐    ┌──────────────────┐          │
│   │ tsmd --embedder  │    │ tsmd --fs-watcher│          │
│   │                  │    │                  │          │
│   │ Model inference  │    │ File monitoring  │          │
│   │ Stateless        │    │ Stateless        │          │
│   │ No DB access     │    │ No DB access     │          │
│   └──────────────────┘    └──────────────────┘          │
│          ↑ text                    │ file path           │
│          ↓ vector                  ↓ index request       │
└─────────────────────────────────────────────────────────┘
```

## Component Responsibilities

| Component | Role |
|---|---|
| `tsmd` | DB management, client response, index queue processing |
| `tsmd --embedder` (child process) | Text → vector conversion (model inference). Runs as a separate process for crash isolation |
| `tsmd --fs-watcher` (child process) | Monitors file changes via inotify/FSEvents, sends index requests to daemon |

## Design Decisions

**The embedder does not auto-restart**: to prevent OOM restart loops, a crashed
child process remains stopped. Use `tsm doctor` to check its status.

For detailed design decision records, see [decisions/](../decisions/).
