# ADR-0005: tsm-embedder 統合と 2 バイナリ構成

- **Status**: **Accepted（確定）**
- **Date**: 2026-04-07
- **Deciders**: key
- **Supersedes**: [ADR-0002](./0002-watcher-thread-integration.md)（watcher 部分のみ）
- **Related**:
  [Issue #82](https://github.com/key/the-space-memory/issues/82),
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0002](./0002-watcher-thread-integration.md)

## Context

ADR-0002 では 3 バイナリ構成（tsm, tsmd, tsm-embedder）を維持し、
watcher のみスレッド化していた。

問題点:

- macOS Gatekeeper で 3 バイナリそれぞれに許可が必要（UX が悪い）
- tsm-embedder はユーザーから見て tsmd の内部実装詳細
- docker/dockerd パターンの「CLI + daemon = 2 バイナリ」が自然

また ADR-0002 の watcher スレッド化は DB 直接アクセスの利点があったが、
プロセス分離（embedder と同じパターン）に揃えることで
tsmd のコードが対称的になり保守性が向上する。

## Decision

### バイナリ構成: 2 バイナリ

| バイナリ | 役割 |
|---|---|
| `tsm` | CLI フロントエンド |
| `tsmd` | デーモン本体 + `--embedder` / `--fs-watcher` モードで子プロセスとしても動作 |

### プロセス構成

```text
tsmd (メインプロセス)
├── tsmd --embedder --no-idle-timeout  … 推論子プロセス
└── tsmd --fs-watcher                  … ファイル監視子プロセス
```

### tsmd のモード分岐

`--embedder` と `--fs-watcher` は相互排他のフラグ。
どちらも指定しない場合はデーモンモードとして動作。

- `tsmd --embedder [--model <dir>] [--no-idle-timeout]`
  - embedder.sock でソケットサーバーを起動
  - `--model` でモデルディレクトリを明示指定可能
- `tsmd --fs-watcher`
  - ファイル変更を検知し、daemon.sock 経由で `DaemonRequest::Index` を送信
  - SIGHUP を受け取ると config を再読み込みして watch 対象を更新

### 子プロセス起動

`std::env::current_exe()` で自身のパスを取得し、モードフラグ付きで spawn。
旧 `sibling_binary("tsm-embedder")` パターンを廃止。

### watcher の IPC

ADR-0002 ではスレッド化して DB 直接アクセスとしていたが、
子プロセス化に伴い daemon.sock 経由の IPC に戻す。

watcher → tsmd: `DaemonRequest::Index { files }` で変更ファイルを通知。
tsmd → watcher: `SIGHUP` で config reload を通知。

### モジュール構成

`src/bin/tsmd.rs`（771 行）を `src/bin/tsmd/` ディレクトリに分割:

| ファイル | 責務 |
|---|---|
| `main.rs` | Args 定義、モード分岐 |
| `daemon_mode.rs` | デーモンモード（accept loop, handle_client） |
| `embedder_mode.rs` | embedder モード（ソケットサーバー、推論） |
| `watcher_mode.rs` | watcher モード（ファイル監視、Index IPC） |
| `child.rs` | 子プロセス管理（spawn, reap, stop） |
| `backfill.rs` | バックフィルオーケストレーション |

`src/embedder.rs` は純粋な推論ライブラリ（`Embedder` struct +
クライアント関数）に整理。デーモンコードは `embedder_mode.rs` に移動。

## ADR-0002 からの変更点

- `tsm-embedder` バイナリを廃止、`tsmd --embedder` に統合
- watcher をスレッドから子プロセス（`tsmd --fs-watcher`）に変更
- watcher の DB 直接アクセスを廃止、daemon.sock 経由 IPC に変更
- watcher への reload 通知を mpsc channel から SIGHUP に変更
- `WatcherStatus` に `pid` フィールドを追加
- `src/bin/tsmd.rs` を `src/bin/tsmd/` ディレクトリに分割

## Consequences

- バイナリが 3 → 2 に減り、Gatekeeper 問題が解消
- embedder と watcher が対称的なパターン（子プロセス + PID ファイル）で統一
- watcher の Index がソケット IPC 経由になるため、
  DB 直接アクセスと比べてわずかなオーバーヘッドがある
- `ps` での表示が `tsm-embedder` → `tsmd --embedder` に変わる
