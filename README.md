# The Space Memory

![The Space Memory](docs/assets/cover.png)

A cross-workspace knowledge search engine built in Rust.

Indexes Markdown documents across multiple workspaces and provides hybrid search
combining FTS5 full-text search with vector semantic search (ruri-v3-30m, 256-dim).

## Features

- **Hybrid search** — FTS5 + vector search fused via Reciprocal Rank Fusion (RRF)
- **Morphological analysis** — Japanese tokenization via lindera (IPADIC)
- **Semantic search** — ruri-v3-30m embeddings computed locally with candle (no ONNX Runtime)
- **Entity graph** — Automatic entity extraction and link inference
- **Synonym expansion** — WordNet-based query expansion
- **Session ingestion** — Index Claude Code session transcripts as searchable knowledge
- **Single binary** — No Python, no external runtime dependencies

## Usage

```bash
# Search
tsm search -q "query" -k 5

# Index all documents
tsm index

# Health check
tsm doctor

# Rebuild database
tsm rebuild --force
```

## Architecture

### プロセス構成

`tsmd` デーモンは `tsm-embedder` を子プロセスとして管理し、ファイル監視（watcher）は内部スレッドとして動作する。

```text
┌─────────────────────────────────────────────────────────┐
│  tsmd（メインデーモン）                                   │
│                                                         │
│  ・DB 所有者（全読み書き）                                │
│  ・クライアント応答（UNIX ソケット）                      │
│  ・インデクサーキュー / ベクターキュー                    │
│  ・backfill 管理                                        │
│  ・ファイル監視（watcher スレッド）                       │
│                                                         │
│   ┌──────────────────┐                                  │
│   │  tsm-embedder    │                                  │
│   │                  │                                  │
│   │  モデル推論のみ   │                                  │
│   │  ステートレス     │                                  │
│   │  DB アクセスなし  │                                  │
│   └──────────────────┘                                  │
│          ↑ text                                         │
│          ↓ vector                                       │
└─────────────────────────────────────────────────────────┘
```

| コンポーネント | 役割 |
|---|---|
| `tsmd` | DB 管理・クライアント応答・インデックスキュー処理 |
| `tsm-embedder`（子プロセス） | テキスト → ベクトル変換（モデル推論）。クラッシュ隔離のため独立プロセス |
| watcher スレッド | ファイル変更を inotify/FSEvents で監視し、DB に直接インデックス |

**embedder は自動リスタートしない設計**:
OOM リスタートループ防止のため、クラッシュした子プロセスは
停止したままになる。`tsm doctor` で状態を確認できる。

### Data Flow

```mermaid
flowchart TB
    subgraph External["External Sources"]
        IF["index-file / ingest-session"]
        WD["watcher daemon<br/><i>file change notify</i>"]
    end

    subgraph Main["main process (sole DB owner)"]
        IQ["indexer queue"]
        CH["chunking"]
        FTS["FTS5 write"]
        VQ["vector queue"]
        VW["receive vector → chunks_vec write"]
        BF["backfill<br/><i>enqueue missing</i>"]
    end

    subgraph Embedder["embedder daemon"]
        INF["inference<br/><i>text → vector</i><br/>no DB access"]
    end

    subgraph DB["SQLite DB"]
        FDB["chunks_fts"]
        VDB["chunks_vec"]
    end

    IF --> IQ
    WD --> IQ
    IQ --> CH --> FTS --> FDB
    CH --> VQ
    BF --> VQ
    VQ -->|"socket request"| INF
    INF -->|"socket response"| VW
    VW --> VDB
```

### Component Responsibilities

```mermaid
graph LR
    subgraph Main["main process"]
        direction TB
        M1["DB ownership<br/>All reads & writes"]
        M2["Indexer queue"]
        M3["Vector queue"]
        M4["Backfill coordination"]
    end

    subgraph Embedder["embedder daemon"]
        direction TB
        E1["Model inference only"]
        E2["Stateless"]
        E3["No DB access"]
    end

    subgraph Watcher["watcher daemon"]
        direction TB
        W1["File system monitoring"]
        W2["inotify / FSEvents"]
        W3["Stateless"]
        W4["No DB access"]
    end

    Watcher -->|"file path"| Main
    Main -->|"text"| Embedder
    Embedder -->|"vector"| Main
```

### Search Flow

```mermaid
flowchart LR
    Q["query"] --> QP["query preprocessing<br/><i>keyword extraction</i>"]
    QP --> CL["classifier"]
    CL --> FTS["FTS5 search"]
    CL --> VEC["vector search<br/><i>read from chunks_vec</i>"]
    CL --> ENT["entity search"]
    FTS --> RRF["RRF fusion<br/><i>+ time decay</i><br/><i>+ status penalty</i>"]
    VEC --> RRF
    ENT --> RRF
    RRF --> R["ranked results"]
```

## License

[MIT](LICENSE)
