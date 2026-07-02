# The Space Memory

![The Space Memory](docs/assets/cover.png)

[English](README.md) · [ドキュメント](https://key.github.io/the-space-memory/)

## 概要

Rustで構築されたクロスワークスペース・ナレッジ検索エンジン。
複数ワークスペースのMarkdownドキュメントをインデックス化し、
FTS5全文検索とベクトルセマンティック検索（ruri-v3-30m, 256次元）のハイブリッド検索を提供する。

## Claude Code プラグイン

本リポジトリが提供するのは `tsm` / `tsmd` バイナリのみ。Claude Code プラグイン
（スキル・エージェント・hook）は別リポジトリ
[`key/tsm-plugin-cc`](https://github.com/key/tsm-plugin-cc) にある
（リポジトリ単体がマーケットプレイスを兼ねる）。

```bash
/plugin marketplace add key/tsm-plugin-cc
/plugin install the-space-memory@tsm-plugin-cc
```

プラグインは `tsm` CLI を呼び出すため、`tsm` は別途インストールし（下記）、
`PATH` に通すこと。

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

# 3. ノートのディレクトリでワークスペース初期化：DB スキーマ、デフォルト設定ファイル
#    （tsm.toml、.tsmignore、.tsm/{user_dict.simpledic,custom_terms.toml,
#    synonyms.csv, hooks/extract/10-md_frontmatter.lua,
#    hooks/score/10-default.lua}）の配置、WordNet/シノニムのインポートまで実施。
#    冪等で、ユーザがカスタマイズしたファイルは絶対に上書きしない。
cd ~/my-notes
tsm init

# 5. デーモンの起動（embedder + ファイル監視）
tsm start

# 6. ドキュメントのインデックス
tsm index

# 7. 検索
tsm search -q "クエリ" -k 5

# ディレクトリで絞り込む（絶対パスまたはカレントディレクトリ相対）
tsm search -q "クエリ" --path notes/
```

`tsm setup` は `HF_HUB_CACHE` を自動設定する。Hugging Face のモデルキャッシュを
別の場所に向けたい場合は明示的に上書きする。

`tsm setup` はマシン共有のキャッシュ（`~/.cache/tsm`）をマシンごとに 1 回だけ
構築し、`tsm init` がそのモデルと WordNet DB をワークスペースの `.tsm/` に
symlink（既定）または copy で展開する。詳細は
[Resource Layers and `link_mode`](docs/configuration.md#resource-layers-and-link_mode) を参照。

### インデックス対象

tsmはプロジェクトルート（`tsm.toml` があるディレクトリ）配下の `.md` ファイルを再帰的にスキャンする。
典型的なディレクトリ構成：

```text
~/my-notes/              ← プロジェクトルート（tsm.toml を含む）
├── projects/
│   ├── project-a.md
│   └── project-b.md
├── research/
│   └── notes.md
└── journal/
    └── 2026-04.md
```

既定では（`content_dirs` 未設定）プロジェクトルート配下のすべての Markdown ファイルがインデックスされる。
`tsm.toml` に `content_dirs` を設定すると、対象を特定のディレクトリに絞り込める。
列挙した各ディレクトリは再帰的にスキャンされるが、その外側にあるものはインデックスされない。
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

## Lua フック

tsm は組み込み Lua（mlua, lua54）によるユーザー編集可能なメタデータ抽出と
結果スコアリングをサポートする。フックは `.tsm/hooks/` に配置し、`tsm init` で
雛形が生成される。

### フックディレクトリ構成

```text
.tsm/hooks/
├── extract/
│   └── 10-md_frontmatter.lua   ← extract フック（インデックス時）
└── score/
    └── 10-default.lua          ← score フック（検索時）
```

- `.lua` ファイルのみ読み込まれる。それ以外のファイルは無視される。
- 同一ディレクトリ内のフックはファイル名のソート順に実行される。
- フックを削除せず無効化するには `.lua` 以外に拡張子を変更する
  （例: `10-default.lua.disabled`）。
- ディレクトリが空または存在しない場合は組み込みデフォルトにフォールバックする。

### Extract フック

インデックス時に各ドキュメントチャンクへ呼び出される。受け取るコンテキスト：

```lua
-- ctx のフィールド: path, body, frontmatter（トップレベル YAML キー）, metadata（累積済み）
function extract(ctx)
  local fm = ctx.frontmatter or {}
  return { status = fm.status, effective_date = fm.updated }
end
```

`ctx.frontmatter` はトップレベルの YAML キーを公開する：スカラー（string/number/bool）と
シーケンス（`tags` など。Lua 配列として渡る）。ネストしたマップは渡されない。

スカラー値（string、number、boolean）のフラットなテーブルを返す。
全 extract フックの結果は浅くマージ（同一キーは後勝ち）され、
`documents.metadata`（JSON カラム）に保存される。

### Score フック

検索時に各結果へ呼び出される。受け取るコンテキスト：

```lua
-- ctx のフィールド: metadata（extract 結果）, rrf, source_type, path, half_life_days
-- 組み込み関数: decay(date, half_life_days), today()
function score(ctx)
  local m = ctx.metadata or {}
  local penalty = ({ outdated = 0.4 })[m.status] or 1.0
  return penalty * decay(m.effective_date, ctx.half_life_days)
end
```

上記は最小例。同梱デフォルト（`score/10-default.lua`）は完全な status→ペナルティ表を使う：
`superseded=0.2`、`rejected=0.3`、`dropped=0.3`、`deprecated=0.3`、
`outdated=0.4`、`proposed=0.7`、その他は `1.0`。

各フックは乗数（`>= 0`）を返す。最終スコアは
`rrf × weight × Π(score フック)` となる。無効な戻り値（負値、NaN、±Inf）は
警告を出して `1.0` として扱われる。

### サンドボックスとライフサイクル

- Lua VM はサンドボックス化されている。標準ライブラリ（`io`/`os`/`package`）なし、
  メモリ上限 64 MiB。フックはファイルシステム・ネットワーク・プロセスにアクセスできない。
- デーモンはすべてのフックを起動時に検証・ロードする（構文エラーやエントリポイント不在は fail-fast）。
  CLI は遅延プロセス内キャッシュを使用する。
- **フック編集後は `tsm restart` が必要** — フックは実行中に再読み込みされない。
- **`metadata` カラムは接続時に自動追加される**（冪等マイグレーション）。既存行は metadata が NULL のまま
  となり、スコアラーは `status`/`updated` カラムから合成するため、スコアリングへの影響はない。
  カスタム extract フック作成後に既存ドキュメントへ metadata を反映したい場合は `tsm reindex` を
  実行する — 破壊的な全再構築は不要。

## 環境変数

以下の設定はいずれも `tsm.toml`（`tsm.toml キー` 列）でも指定でき、環境変数が
優先される。全変数の詳細は [docs/configuration.md](docs/configuration.md) を参照。

| 変数 | デフォルト | `tsm.toml` キー | 説明 |
|---|---|---|---|
| `TSM_CONFIG` | _(自動探索)_ | _(なし)_ | 設定ファイルのパス。それが置かれているディレクトリがプロジェクトルートになる |
| `TSM_STATE_DIR` | `.tsm` | `state_dir` | tsm の状態一式（DB・ソケット・PID・ログ・ユーザー辞書）のルートディレクトリ |
| `TSM_CACHE_DIR` | `$XDG_CACHE_HOME/tsm`（無ければ `$HOME/.cache/tsm`） | `cache_dir` | モデルと WordNet DB のキャッシュディレクトリ |
| `TSM_EMBEDDER_SOCKET` | `{state_dir}/embedder.sock` | `embedder_socket_path` | embedder 子プロセスの UNIX ソケット |
| `TSM_DAEMON_SOCKET` | `{state_dir}/daemon.sock` | `daemon_socket_path` | `tsmd` デーモンの UNIX ソケット |
| `TSM_LOG_DIR` | `{state_dir}/logs` | `log_dir` | デーモンログの出力ディレクトリ |
| `TSM_EMBEDDER_IDLE_TIMEOUT` | `600` | `embedder_idle_timeout_secs` | embedder が自動停止するまでのアイドル秒数（`0` = 無効）。`tsmd` は `--no-idle-timeout` で起動するため、これは単体起動時のみ有効 |
| `TSM_EMBEDDER_BACKFILL_INTERVAL` | `300` | `embedder_backfill_interval_secs` | ベクター backfill の定期チェック間隔（秒、`0` = 無効） |
| `TSM_SEARCH_FALLBACK` | `error` | `search_fallback` | embedder 停止時の挙動: `error` または `fts_only` |
| `TSM_USER_DICT` | `{state_dir}/user_dict.simpledic` | `user_dict_path` | lindera ユーザー辞書のパス |
| `TSM_SETUP_LINK_MODE` | `symlink` | `[setup].link_mode` | `tsm setup` がキャッシュ資源を配置する方式: `symlink` または `copy` |
| `TSM_INIT_LINK_MODE` | `symlink` | `[init].link_mode` | `tsm init` がワークスペース資源をキャッシュへ紐付ける方式: `symlink` または `copy` |
| `TSM_READER_POOL_SIZE` | CPU コア数 | `reader_pool_size` | デーモンの `query_only` リーダープールの接続数。同時読み取り数の上限 |
| `TSM_REINDEX_FTS_BATCH_SIZE` | `200` | `reindex_fts_batch_size` | FTS reindex の 1 バッチあたりのドキュメント数。小さいほど割り込み粒度が細かく fsync が増加、大きいほど reindex スループットが向上 |

このほか `RUST_LOG`（ログレベル、デフォルト `info`）と `NO_COLOR`（カラー出力の
無効化）を尊重する。

## ベンチマーク

検索・インデックスパイプラインの性能ベンチと、`src/`・`benches/`・`Cargo.toml`
を変更する全 PR で走る CI リグレッションゲート（`.github/workflows/bench.yml`）。

**リグレッションゲートの対象は `embedder_calls`（フルインデックス時・単一クエリ
ハイブリッド検索時の呼び出し回数）のみ**で、完全一致で判定する。固定コーパス・
固定バッチ戦略のもとでは決定的な値になるため。インデックススループットと検索
レイテンシは CI のジョブサマリに「recorded, not gated（記録のみ・ゲート対象外）」
と明示した上で記録するだけに留める。現状の 5 ファイル testdata コーパスでは、
無変更のコード・アイドル状態のマシンでも連続 2 回の実行でハイブリッド検索
レイテンシが 15〜49% 振れ、単一ファイルの差分再インデックスも 5 回連続実行で
約 3.4 倍のウォームアップ変動を示した。このコーパス規模ではどんな割合閾値も
本物のリグレッションとノイズを区別できない。コーパスが十分に拡大した段階で
ゲート化を再検討する。

### 前提条件

- `tsmd` が embedder 準備完了で稼働中（`tsm start` 後 `tsm status` で確認）
- 標準の testdata コーパスがインデックス済み:

  ```bash
  cd tests/e2e/testdata
  tsm init && tsm index
  ```

### 実行

```bash
# 検索レイテンシ（ハイブリッド: FTS5 + ベクター + エンティティ）— 人間向けの
# criterion 探索ツール。統計分布をターミナルに表示する。
cargo bench --bench search_latency

# フルメトリクス記録（Prepare/Persist/Embed 段別内訳付きインデックス
# スループット、差分インデックスレイテンシ、ハイブリッド検索レイテンシ、
# embedder 呼び出し回数）— CI 向けツール。benches/baseline.json と同じ
# スキーマの JSON を stdout に1つ出力する。
cargo bench --features bench-counters --bench record_metrics
```

両ベンチのプローブクエリは必ず1件以上ヒットする必要がある。0件ヒットの
クエリは `searcher::search` が embedder や FTS5 に到達する前に早期リターン
するため、そのレイテンシは検索の実処理ではなく早期リターン経路を計測して
しまう（実測: 実クエリの数百ミリ秒に対し、0件ヒットクエリは数マイクロ秒）。

### リグレッションゲート

`tsm-bench-check` は純粋な diff/閾値判定ロジック（`src/bench_baseline.rs` で
ユニットテスト済み）を薄い CLI シェルでラップしたもの:

```bash
cargo run --bin tsm-bench-check -- benches/baseline.json current.json
```

終了コード: `0`（リグレッションなし、または `benches/baseline.json` が
まだ存在しない ── サイレントパスではなく `BOOTSTRAP` として明示的に報告）、
`1`（embedder 呼び出し回数がリグレッション）、`2`（使用法エラー、または
パース不能なファイル）。PR ラベル `bench-baseline-bump` を付けると意図的な
呼び出し回数・記録値の変更に対してゲートジョブ自体をスキップでき、次回の
main マージ後に新しい値がベースラインとして採用される。

`benches/baseline.json` はローカルで計測した値をコミットするのではなく、
最初の CI 実行でブートストラップする。ゲートは CI 環境（`ubuntu-latest`、
CPU 推論）に限定されており、macOS 開発機での値は比較対象にならないため。

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
