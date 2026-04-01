# ADR-0001: プロセスの役割と責務分担

- **Status**: **Accepted（確定）**
- **Date**: 2026-04-01
- **Deciders**: key
- **Related**:
  [company ADR-0010](https://github.com/key/company/blob/main/decisions/0010-tsmd-process-separation.md),
  [Issue #44](https://github.com/key/the-space-memory/issues/44),
  [Issue #45](https://github.com/key/the-space-memory/issues/45)

## Context

tsm は 4 つのバイナリで構成される。
それぞれの役割・IPC 手段・リソース管理を明確にしておくことで、
改修時の影響範囲を限定し、障害時の切り分けを容易にする。

## Decision

### プロセス一覧

| プロセス | 役割 | ライフサイクル |
|---|---|---|
| `tsm` | CLI フロントエンド | ユーザーが実行、都度起動・終了 |
| `tsmd` | デーモン本体（DB・IPC ハブ） | バックグラウンド常駐 |
| `tsm-embedder` | テキストのベクトル化（ONNX Runtime） | tsmd の子プロセス |
| `tsm-watcher` | ファイル変更監視・差分インデックス | tsmd の子プロセス |

### tsm（CLI）

- ユーザーのエントリポイント。サブコマンドを受けて tsmd に IPC で転送する
- tsmd が起動していなければ自動で `tsmd` を spawn する
- `tsm.toml` / 環境変数の設定を解決し、fallback 等のポリシーを
  明示的に `DaemonRequest` に含めて送信する
- DB を直接開かない（daemon-routed コマンドの場合）

### tsmd（デーモン本体）

責務: DB アクセスの集約とクライアント応答。

- UNIX ソケット (`daemon.sock`) でクライアント（`tsm`）からの
  リクエストを受ける
- SQLite DB (`tsm.db`) への全アクセスを担う。
  他プロセスは DB を直接参照しない
- 子プロセス (`tsm-embedder`, `tsm-watcher`) を spawn し、
  PID file で管理する
- 子プロセスがクラッシュしても tsmd 自体は生存し続ける
  （FTS5 検索は維持）
- 子プロセスを**自動リスタートしない**
  （OOM クラッシュループ防止、詳細は company ADR-0010）
- `tsm doctor` で子プロセスの健全性を報告する

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
- バックフィルワーカー (`tsm backfill-worker`) を
  子プロセスとして spawn し、未ベクトル化チャンクを定期処理する

### tsm-watcher（ファイル監視）

責務: コンテンツファイルの変更を検知し、差分インデックスを発火する。

- `notify` crate でファイルシステムイベントを監視する
- 変更を検知したら tsmd に `DaemonRequest::Index` を送信し、
  差分インデックスを実行させる
- DB にアクセスしない（インデックス処理は tsmd 側で行う）

## IPC

```text
tsm ──(daemon.sock)──> tsmd ──(embedder.sock)──> tsm-embedder
                         │
                         └──(spawn)──> tsm-watcher ──(daemon.sock)──> tsmd
```

| 経路 | プロトコル | 用途 |
|---|---|---|
| tsm → tsmd | UNIX socket + length-prefix JSON | コマンド転送 |
| tsmd → tsm-embedder | UNIX socket + length-prefix JSON | エンコードリクエスト |
| tsm-watcher → tsmd | UNIX socket + length-prefix JSON | 差分インデックス通知 |

## リソース管理

| リソース | 管轄プロセス |
|---|---|
| `tsm.db` (SQLite) | tsmd のみ |
| `daemon.sock` | tsmd が listen |
| `embedder.sock` | tsm-embedder が listen |
| `*.pid` | 各プロセスが自分の PID を書く |
| ONNX モデル | tsm-embedder がロード |

## 障害時の挙動

| 障害 | 影響 | 検知方法 | 復旧 |
|---|---|---|---|
| tsm-embedder クラッシュ | ベクトル検索不可、FTS5 のみ | `tsm doctor`、`tsm search` エラー | `tsm restart` |
| tsm-watcher クラッシュ | 自動差分インデックス停止 | `tsm doctor` | `tsm restart` |
| tsmd クラッシュ | 全機能停止 | `tsm search` 接続失敗 | `tsm start` |

## Consequences

- DB アクセスを tsmd に集約することで、
  SQLite の single-writer 制約と自然に整合する
- embedder は DB 非依存のステートレスサーバーとして設計されており、
  将来的にリモート化・スケールアウトが可能（Issue #45）
- 子プロセスの自動リスタートがないため、
  embedder クラッシュ後は手動介入が必要
  （`tsm doctor` で検知 → `tsm restart` で復旧）
