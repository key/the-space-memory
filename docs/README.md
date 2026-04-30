# The Space Memory Documentation

Reference documentation for
[The Space Memory](https://github.com/key/the-space-memory) — a
cross-workspace knowledge search engine combining FTS5 and vector
retrieval. The CLI is installed as the `tsm` binary.

## Install

`tsm` is distributed as prebuilt binaries on the
[Releases page](https://github.com/key/the-space-memory/releases).
The recommended install path is:

```bash
curl -fsSL https://key.github.io/the-space-memory/install.sh | bash
```

The script picks the right archive for your platform, verifies the
checksum, and copies `tsm` and `tsmd` into `~/.local/bin/`.

After install, run the one-time setup and the per-workspace
initialization:

```bash
tsm setup        # download embedding model + WordNet DB (system-wide)
cd ~/my-notes
export TSM_INDEX_ROOT=$PWD
tsm init         # workspace bootstrap: schema, scaffold, synonym import
tsm start        # start the daemon (embedder + file watcher)
tsm index        # index your documents
tsm search -q "query" -k 5
```

For the full quick-start tour, see the project
[README](https://github.com/key/the-space-memory#readme)
([日本語版](https://github.com/key/the-space-memory/blob/main/README.ja.md)).

## Contents

| Document | Description |
|---|---|
| [Architecture](architecture.md) | Daemon / embedder / watcher topology and IPC layout |
| [Data Flow](data-flow.md) | Indexing and search pipelines end-to-end |
| [Configuration](configuration.md) | `tsm.toml` and environment variable reference |
| [Command Reference](command-reference.md) | Every `tsm` subcommand, flags, and examples |
| [User Dictionary](user-dictionary.md) | Custom terminology for the lindera tokenizer |
| [Claude Code prompt format](claude-code/claude-code-prompt-format.md) | Output format used by the auto-search hook |
