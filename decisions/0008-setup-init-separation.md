# ADR-0008: tsm setup と tsm init の責務分離（system-wide cache と workspace scaffold）

- **Status**: **Proposed**
- **Date**: 2026-05-08 (Proposed)
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0003](./0003-config-via-resolved-config.md)

## Context

`tsm setup` と `tsm init` の責務分離が現状の実装で破綻している。

### 仕様の意図（`docs/command-reference.md`）

- `tsm setup`: **System-wide; no workspace DB writes. Run once per machine.**
  外部リソース（ruri-v3-30m モデル、Japanese WordNet DB）の取得層
- `tsm init`: ワークスペース固有の初期化（DB 作成、scaffold、WordNet → DB import）

### 実装の現状

`tsm setup` のコピー先がワークスペース内（CWD 相対）になっている:

| 種別 | 仕様の意図 | 実装の現状 |
|---|---|---|
| ruri-v3-30m モデル | system-wide cache | `./.tsm/models/ruri-v3-30m/` |
| WordNet DB | system-wide cache | `./.tsm/wnjpn.db` |

`config::state_dir()` は `DEFAULT_STATE_DIR = ".tsm"`（CWD 相対）を返し、
`models_dir()` / `wordnet_db_path()` がこれに依存している。

### 問題点

1. **"once per machine" が成立しない**
   ワークスペースを変えるたびに ~250MB の再ダウンロード／重複保存
2. **仕様書と実装が矛盾**
   `docs/command-reference.md` の "System-wide" 記述と実装が乖離
3. **embedder の前提が曖昧**
   embedder はワークスペースの `.tsm/models/` を読む。複数ワークスペースで
   モデルファイルがバラバラに存在する状態を許容している
4. **doctor の責務が混在**
   現在の doctor は workspace 内のモデル整合性しか見ない。machine-wide な
   セットアップ状態（モデル取得済みか）の概念が無い

## Decision

`tsm` のリソースを以下 2 層に明確に分離する。

### 第 1 層: system-wide cache（machine-global）

machine 全体で共有する **immutable な外部リソース**。

| ファイル | 目的 |
|---|---|
| `cache_dir/models/ruri-v3-30m/config.json` | embedder モデル設定 |
| `cache_dir/models/ruri-v3-30m/tokenizer.json` | embedder トークナイザ |
| `cache_dir/models/ruri-v3-30m/model.safetensors` | embedder 重み（~250MB） |
| `cache_dir/wnjpn.db` | Japanese WordNet DB（~5MB） |

`cache_dir` の解決順:

1. 環境変数 `TSM_CACHE_DIR`
2. `tsm.toml` の `cache_dir = "..."` 設定
3. デフォルト: `$XDG_CACHE_HOME/tsm/`（未設定時は `$HOME/.cache/tsm/`）

Linux / macOS とも `$HOME/.cache/tsm/` を採用する。
macOS で `~/Library/Caches` を採らないのは、rustup / uv / cargo 等の
Rust エコシステムが XDG 系統を採用している慣例に揃えるため
（ユーザーが手で触りやすい・ドキュメント可搬性が高い）。

`cache_dir` は **書き換え対象外**。
ワークスペース DB の状態に依らず machine 全体で共有される。

### 第 2 層: workspace state（CWD 相対 `.tsm/`）

ワークスペースごとに独立して持つ **mutable な状態**。

| ファイル | 目的 |
|---|---|
| `.tsm/tsm.db` (+ `-shm`, `-wal`) | FTS5 + vector DB |
| `.tsm/synonyms.csv` | ユーザーシノニム（CSV）|
| `.tsm/stopwords.txt` | ストップワード |
| `.tsm/reject_words.txt` | 辞書 reject リスト |
| `.tsm/custom_terms.toml` | カスタム用語設定 |
| `.tsm/user_dict.csv` | 形態素辞書（simpledic）|
| `.tsm/tsm.toml` (任意) | ワークスペース固有設定 |
| `.tsm/logs/` | ログ |
| `.tsm/{daemon,embedder}.sock` | IPC ソケット |
| `.tsm/{tsmd,embedder}.pid` | PID ファイル |
| `.tsm/tsm-status.json` | デーモン状態 |

### コマンドの責務

#### `tsm setup`（system-wide のみ。ワークスペースに触れない）

1. `cache_dir/models/ruri-v3-30m/` に必要ファイル全てが揃っているか確認
   - 揃っていれば skip（idempotent）
   - 欠けていれば HuggingFace Hub から取得して配置
2. `cache_dir/wnjpn.db` の存在確認
   - 存在すれば skip
   - 無ければ GitHub から `wnjpn.db.gz` を取得し展開
3. **DB に一切触れない**。`.tsm/` ディレクトリも作らない

#### `tsm init`（workspace 固有のみ。cache を読むだけ）

1. `.tsm/` を作成
2. `.tsm/tsm.db` を作成しスキーマ初期化
3. scaffold（`synonyms.csv`, `stopwords.txt`, `reject_words.txt`）
4. `cache_dir/wnjpn.db` から WordNet シノニムを `.tsm/tsm.db` に import
   - cache が無ければ `tsm setup` を促す警告を出して continue
5. `.tsm/synonyms.csv` からユーザーシノニムを sync

#### `tsmd --embedder`（cache から model をロード）

- `cache_dir/models/ruri-v3-30m/` を参照
- 不在なら起動失敗 → `tsm setup` を促すエラー

#### `tsm doctor`（両層を独立してレポート）

セクションを分ける:

- **System cache**: モデル取得状態、WordNet DB 取得状態
- **Workspace**: DB 整合性、vector backfill 状態、辞書状態

### 設定の整理

`config.rs` に新たに追加:

```rust
pub fn cache_dir() -> PathBuf;          // 解決後の cache 起点
pub fn cache_models_dir() -> PathBuf;   // cache_dir + "models/ruri-v3-30m"
pub fn cache_wordnet_db_path() -> PathBuf; // cache_dir + "wnjpn.db"
```

既存の `models_dir()` / `wordnet_db_path()` は **削除**。
state_dir 起点の同名関数は混乱の温床になるため、cache 系は明示的に
`cache_*` プレフィックスを付ける。

## Rationale

**なぜ system-wide / workspace を 2 層に分けるか**

- リソースの**性質が異なる**: モデルと WordNet DB は immutable / global、
  DB と CSV は mutable / workspace-local。一緒くたに扱うと、
  ワークスペース複製時にコピーする・しないの判断が個別に必要になる
- **同一マシン上で複数ワークスペースを持つ運用が前提**
  （CLAUDE.md にもある通り、ナレッジは複数の作業ディレクトリにまたがる）。
  モデルが共有されないと UX として致命的

**なぜ XDG_CACHE_HOME を採るか（macOS でも）**

- Rust エコシステム（rustup, uv, cargo）が XDG 系統を採用しており、
  この延長で扱えるユーザーが多い
- macOS の `~/Library/Caches` は GUI アプリ向けで、CLI からの可視性が低い
- 環境変数で上書き可能なので、Apple 流に拘りたいユーザーは
  `TSM_CACHE_DIR=$HOME/Library/Caches/tsm` を設定すれば良い

**なぜ `models_dir()` を消して `cache_models_dir()` にするか**

- 名前空間の汚染回避。`state_dir` 系と `cache_dir` 系を関数名から
  即座に区別できるようにすることで、将来「どっちに書くべきか」の
  判断ミスを防ぐ
- breaking change だが、tsm は単一バイナリ製品で外部 API は持たない。
  影響は内部コードのみ

**なぜ embedder は cache 専一の前提を持つか**

- embedder は **stateless / cache 専一** であるべき（ADR-0001 の延長）。
  ワークスペース内パスを fallback として参照すると、cache が正であるという
  前提が崩れ、責務分離の効果が薄れる
- 「cache に無ければ起動失敗」という単純な契約を維持することで、
  setup の実行漏れを早期に検出できる

## Consequences

### Positive

- ワークスペースサイズが最大 ~250MB 縮小（モデル分）
- "Run once per machine" が文字通り成立する
- doctor のレポート構造が責務単位で明確になる
- embedder の前提（cache 専一）が明文化され、将来のプラグイン作者が
  workspace 内モデルを期待しなくなる
- 仕様書（`docs/command-reference.md`）と実装の乖離が解消される

### Negative

- breaking change: 既存の `models_dir()` / `wordnet_db_path()` 参照箇所の
  リファクタコストが発生する
- doctor の出力フォーマットが変わる（JSON 構造を見ている自動化があれば破壊）
- ワークスペースを別マシンに丸ごとコピーした場合、別マシン側で
  `tsm setup` が必要になる（モデルがついてこない）

### Follow-ups

- **実装 PR の分割案**:
  1. `cache_dir` 解決ロジックの追加（`config.rs`）
  2. `cache_models_dir()` / `cache_wordnet_db_path()` 追加と embedder の参照差し替え
  3. `tsm setup` の動作変更（cache 専一化、ワークスペースに触れない）
  4. `tsm init` の動作変更（cache から WordNet を読む）
  5. doctor のセクション分割
  6. 旧 `models_dir()` / `wordnet_db_path()` の削除
  7. `docs/command-reference.md` の記述を実装と一致させる
- **テスト**:
  - cache_dir の解決優先順位（env > toml > default）の単体テスト
  - doctor の system / workspace 分離出力の JSON テスト
- **設定ドキュメント**:
  - `tsm.toml.example` に `cache_dir` 設定例を追加
  - `docs/configuration.md` に第 1 / 第 2 層の概念を記述
- **README**:
  - `tsm setup` が machine-global であることを明記
  - 別マシンへの引っ越し時の手順
