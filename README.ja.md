# The Space Memory

![The Space Memory](docs/assets/cover.png)

[English](README.md) · [ドキュメント](https://key.github.io/the-space-memory/)

## 概要

Rustで構築されたクロスワークスペース・ナレッジ検索エンジン。
複数ワークスペースのMarkdownドキュメントをインデックス化し、
FTS5全文検索とベクトルセマンティック検索（ruri-v3-30m, 256次元）のハイブリッド検索を提供する。

## コンセプト

- **ワークスペース横断検索** — 複数のリポジトリ（個人メモ、業務プロジェクト、テックノート等）をオーケストレーションリポジトリから一括検索
- **100ms未満のローカル検索** — インデックス作成も検索もローカルで完結し、ネットワーク遅延なしに100ms未満で応答
- **Claude Codeとの透過的連携** — hookがプロンプトを読み取り、ナレッジベースを検索し、関連するコンテキストを自動的にインジェクト

## 機能

- **ハイブリッド検索** — FTS5 + ベクトル検索をRRF（Reciprocal Rank Fusion）で統合
- **形態素解析** — lindera（IPADIC）による日本語トークナイズ
- **セマンティック検索** — ruri-v3-30mの埋め込みをcandleでローカル推論（ONNX Runtime不要）
- **エンティティグラフ** — 自動エンティティ抽出とリンク推論
- **同義語展開** — WordNet + ユーザー定義CSVによるクエリ展開
- **セッション取り込み** — Claude Codeのセッション記録を検索可能なナレッジとしてインデックス化
- **シングルバイナリ** — Python不要、外部ランタイム依存なし

## はじめに

### 動作プラットフォーム

| プラットフォーム | 状態 |
|---|---|
| Linux x86_64 | メインターゲット、CI テスト済み |
| Linux arm64 | サポート、CI ビルドチェック済み |
| macOS Apple Silicon | サポート |
| macOS x86_64 | サポート |

ファイル監視は inotify（Linux）/ FSEvents（macOS）を使用。

### セットアップ

```bash
# 1. ビルド
cargo build --release

# 2. 外部リソースのダウンロード（ruri モデル + 日本語 WordNet DB）。
#    マシン共有のためセットアップは 1 回だけでよい。
tsm setup

# 3. ドキュメントのルートディレクトリを設定
export TSM_INDEX_ROOT=~/my-notes

# 4. ワークスペース初期化：DB スキーマ、デフォルト設定ファイル
#    （tsm.toml、.tsmignore、.tsm/{user_dict.simpledic,custom_terms.toml,
#    synonyms.csv}）の配置、WordNet/シノニムのインポートまで実施。
#    冪等で、ユーザがカスタマイズしたファイルは絶対に上書きしない。
tsm init

# 5. デーモンの起動（embedder + ファイル監視）
tsm start

# 6. ドキュメントのインデックス
tsm index

# 7. 検索
tsm search -q "クエリ" -k 5
```

`tsm setup` は `HF_HUB_CACHE` を自動設定する。Hugging Face のモデルキャッシュを
別の場所に向けたい場合は明示的に上書きする。

### インデックス対象

tsmは `TSM_INDEX_ROOT` 配下の `.md` ファイルを再帰的にスキャンする。
典型的なディレクトリ構成：

```text
~/my-notes/              ← TSM_INDEX_ROOT
├── projects/
│   ├── project-a.md
│   └── project-b.md
├── research/
│   └── notes.md
└── journal/
    └── 2026-04.md
```

`TSM_INDEX_ROOT` 配下のすべてのMarkdownファイルが自動的にインデックスされる。
ファイル監視により、追加・変更・削除をリアルタイムに検知する。

### メンテナンス

```bash
# デーモン稼働中に再インデックス（非破壊、バックグラウンド）
tsm reindex all       # FTS + ベクター
tsm reindex fts       # FTS のみ（辞書変更後）
tsm reindex vectors   # ベクターのみ（モデル変更後）

# ゼロから再構築（破壊的、デーモン停止が必要）
tsm rebuild           # ドライラン（DB情報を表示）
tsm rebuild --apply   # DB削除して再構築
```

`tsm doctor` でシステムの状態とデーモンのステータスを確認できる。

## 環境変数

以下の設定はいずれも `tsm.toml`（`tsm.toml キー` 列）でも指定でき、環境変数が
優先される。全変数の詳細は [docs/configuration.md](docs/configuration.md) を参照。

| 変数 | デフォルト | `tsm.toml` キー | 説明 |
|---|---|---|---|
| `TSM_CONFIG` | _(自動探索)_ | _(なし)_ | 設定ファイルのパス。その親ディレクトリがプロジェクトルートになる |
| `TSM_STATE_DIR` | `.tsm` | `state_dir` | tsm の状態一式（DB・ソケット・PID・ログ・ユーザー辞書）のルートディレクトリ |
| `TSM_CACHE_DIR` | `$XDG_CACHE_HOME/tsm`（無ければ `$HOME/.cache/tsm`） | `cache_dir` | モデルと WordNet DB のキャッシュディレクトリ |
| `TSM_INDEX_ROOT` | `/workspaces` | `index_root` | インデックス対象を含むルートディレクトリ |
| `TSM_EMBEDDER_SOCKET` | `{state_dir}/embedder.sock` | `embedder_socket_path` | embedder 子プロセスの UNIX ソケット |
| `TSM_DAEMON_SOCKET` | `{state_dir}/daemon.sock` | `daemon_socket_path` | `tsmd` デーモンの UNIX ソケット |
| `TSM_LOG_DIR` | `{state_dir}/logs` | `log_dir` | デーモンログの出力ディレクトリ |
| `TSM_EMBEDDER_IDLE_TIMEOUT` | `600` | `embedder_idle_timeout_secs` | embedder が自動停止するまでのアイドル秒数（`0` = 無効）。`tsmd` は `--no-idle-timeout` で起動するため、これは単体起動時のみ有効 |
| `TSM_EMBEDDER_BACKFILL_INTERVAL` | `300` | `embedder_backfill_interval_secs` | ベクター backfill の定期チェック間隔（秒、`0` = 無効） |
| `TSM_SEARCH_FALLBACK` | `error` | `search_fallback` | embedder 停止時の挙動: `error` または `fts_only` |
| `TSM_USER_DICT` | `{state_dir}/user_dict.simpledic` | `user_dict_path` | lindera ユーザー辞書のパス |
| `TSM_SETUP_LINK_MODE` | `symlink` | `[setup].link_mode` | `tsm setup` がキャッシュ資源を配置する方式: `symlink` または `copy` |
| `TSM_INIT_LINK_MODE` | `symlink` | `[init].link_mode` | `tsm init` がワークスペース資源をキャッシュへ紐付ける方式: `symlink` または `copy` |

このほか `RUST_LOG`（ログレベル、デフォルト `info`）と `NO_COLOR`（カラー出力の
無効化）を尊重する。

## ベンチマーク

検索・インデックスパイプラインの性能ベンチ。現状は検索レイテンシのベンチのみ
実装済みで、インデックスベンチと CI のリグレッションゲートは後続 PR で追加する
（[#181](https://github.com/key/the-space-memory/issues/181) 参照）。

### 前提条件

- `tsmd` が embedder 準備完了で稼働中（`tsm start` 後 `tsm status` で確認）
- 標準の testdata コーパスがインデックス済み:

  ```bash
  export TSM_INDEX_ROOT=$(pwd)/tests/e2e/testdata
  tsm init && tsm index
  ```

### 実行

```bash
# 検索レイテンシ（ハイブリッド: FTS5 + ベクター + エンティティ）
cargo bench --bench search_latency
```

### Embedder 呼び出しカウンタ

embedder の呼び出し回数を検証したいベンチでは、`bench-counters` フィーチャを
有効にしてビルドする。デフォルトでは無効で、リリースビルドではカウンタが
完全にコンパイルアウトされる。

```bash
cargo build --features bench-counters
```

```rust
use the_space_memory::embedder::counters;

counters::reset_embedder_calls();
// ... embed_via_socket_at を呼ぶコードを実行 ...
println!("calls: {}", counters::embedder_call_count());
```

## ドキュメント

- [コマンドリファレンス](docs/command-reference.md) — CLIコマンド、フラグ、使用例
- [アーキテクチャ](docs/architecture.md) — プロセス構成とコンポーネントの責務
- [データフロー](docs/data-flow.md) — インデックスと検索のフロー図
- [設定リファレンス](docs/configuration.md) — 環境変数と設定ファイルのリファレンス
- [ユーザー辞書](docs/user-dictionary.md) — カスタム辞書の管理
- [設計判断](decisions/) — ADR（アーキテクチャ決定記録）

## 背景

The Space Memoryは[sui-memory](https://zenn.dev/noprogllama/articles/7c24b2c2410213)に
インスパイアされた。sui-memoryはClaude Codeのセッション記録を検索可能なデータベースとして
インデックス化するアイデアを提示した。tsmはこのコンセプトをセッション記録から
ドキュメントリポジトリ全域に拡張し、ワークスペース横断のナレッジ検索を実現する。

### なぜ自作したのか

既存のツールにはそれぞれ決定的な欠点があった：

- **Notion / GitHub検索** — ネットワーク経由のため、リアルタイムのプロンプトインジェクションには速度が不足
- **grep** — シーケンシャルスキャンで、検索語間のセマンティックな相関がない
- **Obsidian** — Markdownエディタとしては優秀だが、AIエージェントとの連携には不向き

tsmはこれらのギャップを埋めるために構築された。ローカルファーストで100ms未満の
ハイブリッド検索エンジンであり、Claude Codeとhookを通じて透過的に連携する。
FTS5とベクトル検索の組み合わせは語彙のギャップを埋め
（例：「射撃」⇔「銃砲」のマッチング）、lindera/IPADICによる日本語トークナイズは
英語圏向けツールの流用ではなく自作した主な理由である。

### 名前の由来

命名はsui-memoryのパターン（prefix + memory）に倣い、複数リポジトリを統一的な
検索空間として扱うことから "space" を冠した。
カバービジュアルは『ハイドライド3』（サブタイトル：*The Space Memories*）のオマージュ。

## ライセンス

[MIT](LICENSE)
