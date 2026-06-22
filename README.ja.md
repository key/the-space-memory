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
#    synonyms.csv, hooks/extract/10-md_frontmatter.lua,
#    hooks/score/10-default.lua}）の配置、WordNet/シノニムのインポートまで実施。
#    冪等で、ユーザがカスタマイズしたファイルは絶対に上書きしない。
tsm init

# 5. デーモンの起動（embedder + ファイル監視）
tsm start

# 6. ドキュメントのインデックス
tsm index

# 7. 検索
tsm search -q "クエリ" -k 5
```

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
