# ADR-0002: プロセスの役割と責務分担（watcher スレッド化）

- **Status**: **Accepted（確定）**
- **Date**: 2026-04-01
- **Deciders**: key
- **Supersedes**: [ADR-0001](./0001-process-roles-and-responsibilities.md)
- **Related**:
  [Issue #54](https://github.com/key/the-space-memory/issues/54),
  [Issue #45](https://github.com/key/the-space-memory/issues/45),
  [company ADR-0010](https://github.com/key/company/blob/main/decisions/0010-tsmd-process-separation.md)

## Context

ADR-0001 では tsm-watcher を独立プロセスとして定義していたが、
watcher は純粋な Rust コードで構成されておりクラッシュ隔離の必要がない。
プロセス間通信（UNIX ソケット経由 JSON）のオーバーヘッドと
PID ファイル管理の複雑さが不要なコストになっていた。

Issue #45 で embedder の backfill を tsmd 内スレッドに統合した実績があり、
同じパターンで watcher もスレッド化するのが自然な流れ。

## Decision

### プロセス一覧

| コンポーネント | 役割 | ライフサイクル |
|---|---|---|
| `tsm` | CLI フロントエンド | ユーザーが実行、都度起動・終了 |
| `tsmd` | デーモン本体（DB・IPC ハブ・watcher・backfill） | バックグラウンド常駐 |
| `tsm-embedder` | テキストのベクトル化（ONNX Runtime） | tsmd の子プロセス |

### tsm（CLI）

- ユーザーのエントリポイント。サブコマンドを受けて tsmd に IPC で転送する
- tsmd が起動していなければ自動で `tsmd` を spawn する
- `tsm.toml` / 環境変数の設定を解決し、fallback 等のポリシーを
  明示的に `DaemonRequest` に含めて送信する
- DB を直接開かない（daemon-routed コマンドの場合）

### tsmd（デーモン本体）

責務: DB アクセスの集約、クライアント応答、ファイル監視、backfill。

- UNIX ソケット (`daemon.sock`) でクライアント（`tsm`）からの
  リクエストを受ける
- SQLite DB (`tsm.db`) への全アクセスを担う。
  他プロセスは DB を直接参照しない
- 子プロセス (`tsm-embedder`) を spawn し、PID file で管理する
- **watcher スレッド**: `notify` crate でファイル変更を監視し、
  `Arc<Mutex<Connection>>` 経由で直接インデックスを実行する。
  DB lock は per-file で取得・解放し、クライアント応答をブロックしない
- **backfill スレッド**: 未ベクトル化チャンクを定期処理する
- 子プロセスがクラッシュしても tsmd 自体は生存し続ける
  （FTS5 検索は維持）
- 子プロセスを**自動リスタートしない**
  （OOM クラッシュループ防止、詳細は company ADR-0010）
- `--no-watcher` フラグで watcher スレッドの起動をスキップ可能

### tsm-embedder（ベクトル推論）

責務: テキストからベクトル埋め込みを生成する。

- ONNX Runtime でモデルをロードし、
  UNIX ソケット (`embedder.sock`) でエンコードリクエストを受ける
- ステートレス: DB にアクセスしない。
  入力テキストを受け取り、埋め込みベクトルを返すだけ
- idle timeout で自動停止可能
  （`embedder_idle_timeout_secs`、デフォルト無効）
- ONNX Runtime / ROCm のセグフォが発生しうるため、
  プロセス分離が必須

## IPC

```text
tsm ──(daemon.sock)──> tsmd ──(embedder.sock)──> tsm-embedder
                         │
                         ├── watcher thread (DB 直接アクセス)
                         └── backfill thread (DB 直接アクセス)
```

| 経路 | プロトコル | 用途 |
|---|---|---|
| tsm → tsmd | UNIX socket + length-prefix JSON | コマンド転送 |
| tsmd → tsm-embedder | UNIX socket + length-prefix JSON | エンコードリクエスト |

## リソース管理

| リソース | 管轄 |
|---|---|
| `tsm.db` (SQLite) | tsmd のみ（`Arc<Mutex<Connection>>` で全スレッド共有） |
| `daemon.sock` | tsmd が listen |
| `embedder.sock` | tsm-embedder が listen |
| `embedder.pid` | tsmd が子プロセス PID を書く |
| ONNX モデル | tsm-embedder がロード |

## 障害時の挙動

| 障害 | 影響 | 検知方法 | 復旧 |
|---|---|---|---|
| tsm-embedder クラッシュ | ベクトル検索不可、FTS5 のみ | `tsm doctor`、`tsm search` エラー | `tsm restart` |
| watcher スレッド停止 | 自動差分インデックス停止 | `tsm status` | `tsm restart` |
| tsmd クラッシュ | 全機能停止 | `tsm search` 接続失敗 | `tsm start` |

## ADR-0001 からの変更点

- `tsm-watcher` バイナリを廃止し、tsmd 内スレッドに統合
- watcher → tsmd の UNIX ソケット IPC を廃止（DB 直接アクセス）
- `watcher.pid` ファイル管理を廃止
- `WatcherStatus` から `pid` フィールドを削除
- `tsm start --no-watcher` オプションを追加

## Consequences

- バイナリが 4 → 3 に減り、ビルド・デプロイが簡素化
- watcher のインデックスが IPC オーバーヘッドなしで実行される
- DB lock を per-file で取得・解放するため、
  watcher のインデックス中もクライアント応答が可能
- embedder は引き続きプロセス分離（クラッシュ隔離が必要なため）
