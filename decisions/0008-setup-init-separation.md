# ADR-0008: tsm setup と tsm init の責務分離（system-wide cache と workspace scaffold）

- **Status**: Accepted
- **Date**: 2026-05-08 (Proposed) / 2026-05-08 (Accepted)
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0003](./0003-config-via-resolved-config.md)

## Context

`tsm setup` と `tsm init` の責務分離が現状の実装で破綻している。

### 仕様の意図（`docs/command-reference.md`）

- `tsm setup`: System-wide; no workspace DB writes. Run once per machine.
  外部リソース（ruri-v3-30m モデル、Japanese WordNet DB）の取得層
- `tsm init`: ワークスペース固有の初期化（DB 作成、scaffold、WordNet → DB import）

### 実装の現状

`tsm setup` のコピー先がワークスペース内（CWD 相対）になっている。

| 種別 | 仕様の意図 | 実装の現状 | サイズ |
|---|---|---|---|
| ruri-v3-30m モデル | system-wide cache | `./.tsm/models/ruri-v3-30m/` | ~147MB |
| WordNet DB | system-wide cache | `./.tsm/wnjpn.db` | ~200MB |

`config::state_dir()` は `DEFAULT_STATE_DIR = ".tsm"`（CWD 相対）を返し、
`models_dir()` / `wordnet_db_path()` がこれに依存している。

### 問題点

1. "once per machine" が成立しない
   ワークスペースを変えるたびに ~347MB（モデル + WordNet）の
   再ダウンロード／重複保存
2. 仕様書と実装が矛盾
   `docs/command-reference.md` の "System-wide" 記述と実装が乖離
3. embedder の前提が曖昧
   embedder はワークスペースの `.tsm/models/` を読む。複数ワークスペースで
   モデルファイルがバラバラに存在する状態を許容している
4. doctor の責務が混在
   現在の doctor は workspace 内のモデル整合性しか見ない。machine-wide な
   セットアップ状態（モデル取得済みか）の概念が無い

## Decision

`tsm` のリソースを以下 2 層に明確に分離し、各層に独立した link_mode を持たせる。

### 第 1 層: system-wide cache（machine-global）

machine 全体で共有する immutable な外部リソース。
ディレクトリ位置は `cache_dir`。

```text
$cache_dir/
├── manifest.json
├── models/
│   └── ruri-v3-30m   →  ~/.cache/huggingface/hub/models--cl-nagoya--ruri-v3-30m/snapshots/<commit-hash>
│                        （[setup].link_mode = copy なら物理ディレクトリ）
├── wnjpn.db          →  sources/wnjpn-v1.1.db
│                        （[setup].link_mode = copy なら物理ファイル）
└── sources/
    └── wnjpn-v1.1.db （物理 ~200MB、tsm が所有）
```

- ruri モデルは HuggingFace Hub の cache snapshot ディレクトリへ
  ディレクトリ単位の symlink （or 物理コピー）
- WordNet は `sources/wnjpn-<version>.db` を tsm 自身が所有し、
  `wnjpn.db` をそこへの symlink （or 物理コピー）にする
- `manifest.json` は取得方式・ソース・サイズ・取得日時を記録
  （doctor の整合性チェックの根拠）

`cache_dir` の解決順:

1. 環境変数 `TSM_CACHE_DIR`
2. `tsm.toml` の `cache_dir = "..."` 設定
3. デフォルト: `$XDG_CACHE_HOME/tsm/`（未設定時は `$HOME/.cache/tsm/`）

Linux / macOS とも `$HOME/.cache/tsm/` を採用する。
macOS で `~/Library/Caches` を採らないのは、rustup / uv / cargo 等の
Rust エコシステムが XDG 系統を採用している慣例に揃えるため。

### 第 2 層: workspace state（CWD 相対 `.tsm/`）

ワークスペースごとに独立して持つ mutable な状態 + cache へのリソース参照。

```text
<workspace>/.tsm/
├── tsm.db (+ -shm, -wal)        FTS5 + vector DB（物理）
├── synonyms.csv                  ユーザーシノニム CSV（物理）
├── stopwords.txt                 ストップワード（物理）
├── reject_words.txt              辞書 reject リスト（物理）
├── custom_terms.toml             カスタム用語設定（物理）
├── user_dict.csv                 形態素辞書 simpledic（物理）
├── tsm.toml (任意)               ワークスペース固有設定（物理）
├── logs/                         ログ（物理）
├── {daemon,embedder}.sock        IPC ソケット（物理）
├── {tsmd,embedder}.pid           PID ファイル（物理）
├── tsm-status.json               デーモン状態（物理）
├── models/
│   └── ruri-v3-30m   →  $cache_dir/models/ruri-v3-30m
│                        （[init].link_mode = copy なら物理ディレクトリ）
└── wnjpn.db          →  $cache_dir/wnjpn.db
                         （[init].link_mode = copy なら物理ファイル）
```

embedder は workspace 側 `state_dir/models/ruri-v3-30m/` を最優先で見る。
これは workspace ごとに別モデル / 別 WordNet を置くための override 経路として機能する。

### リンク戦略（2 層独立）

cache 層と workspace 層、それぞれで `link_mode` を選べる。
モードは `symlink`（default）と `copy` の 2 択。

| mode | 動作 | ディスク使用 | 弱点 |
|---|---|---|---|
| `symlink` (default) | 上流の実体を symlink で指す | 重複なし | 上流が削除されるとリンク切れ |
| `copy` | 物理コピー | 重複あり | ストレージ消費が増える |

`hardlink` モードは採用しない[^no-hardlink]。

#### cache 層 (`[setup].link_mode`)

`tsm setup` が cache を構築するときの戦略。
"上流" は ruri モデルの場合 HF cache、WordNet の場合 `sources/wnjpn-<ver>.db`。

```bash
tsm setup --link-mode symlink|copy
```

```toml
[setup]
link_mode = "symlink"
```

#### workspace 層 (`[init].link_mode`)

`tsm init` が `.tsm/` を構築するときの戦略。"上流" は cache。

```bash
tsm init --link-mode symlink|copy
```

```toml
[init]
link_mode = "symlink"
```

#### ユースケース別の選び方

| シナリオ | `[setup].link_mode` | `[init].link_mode` | workspace 単位の重複 |
|---|---|---|---|
| ホスト単独運用 | `symlink` | `symlink` | 無し |
| DevContainer 単独運用 | `symlink` | `symlink` | 無し |
| ホスト ↔ DevContainer 併用 | `symlink` | `copy` | workspace ごとに ~347MB |
| 完全自己完結（移植性最大） | `copy` | `copy` | cache + workspace ごとに ~347MB |

優先順位（各層独立）: CLI フラグ > `tsm.toml` > デフォルト (`symlink`)

#### mode 切り替え時の挙動

- `tsm setup --link-mode <new>`: cache 直下のエントリ
  （`models/ruri-v3-30m/`, `wnjpn.db`）を作り直す。`sources/` は touch しない
- `tsm init --link-mode <new>`: `.tsm/models/`, `.tsm/wnjpn.db` のみ作り直す。
  `tsm.db` を含む他の workspace state は触らない

### manifest.json（cache のメタ情報）

cache_dir 直下に `manifest.json` を置き、各リソースの取得方式・ソース・
サイズ・取得日時を記録する。

```json
{
  "version": 1,
  "resources": {
    "models/ruri-v3-30m": {
      "mode": "symlink",
      "target": "/Users/key/.cache/huggingface/hub/models--cl-nagoya--ruri-v3-30m/snapshots/abc123",
      "model_id": "cl-nagoya/ruri-v3-30m",
      "fetched_at": "2026-05-08T09:18:00+09:00"
    },
    "wnjpn.db": {
      "mode": "symlink",
      "target": "sources/wnjpn-v1.1.db",
      "source_url": "https://github.com/bond-lab/wnja/releases/download/v1.1/wnjpn.db.gz",
      "version": "v1.1",
      "size": 203110400,
      "fetched_at": "2026-05-08T09:18:00+09:00"
    }
  }
}
```

`tsm doctor` がこれを読み、エントリの存在・リンク先生存・サイズ整合を確認する。

### コマンドの責務

#### `tsm setup`（machine-wide cache のみ）

1. 必要なら `cache_dir/` と `cache_dir/sources/` を作成
2. ruri モデル取得:
   - HuggingFace Hub から `cl-nagoya/ruri-v3-30m` の snapshot を確保
     （`hf_hub` crate が HF cache に配置）
   - `[setup].link_mode` に従い `cache_dir/models/ruri-v3-30m` を生成
3. WordNet 取得:
   - `cache_dir/sources/wnjpn-v1.1.db` が無ければ GitHub から DL→展開して配置
   - `[setup].link_mode` に従い `cache_dir/wnjpn.db` を生成
4. `manifest.json` を更新
5. DB に一切触れない。`.tsm/` ディレクトリも作らない

既に揃っているリソースは skip（idempotent）。

#### `tsm init`（workspace 固有のみ）

1. `.tsm/` を作成
2. `.tsm/tsm.db` を作成しスキーマ初期化
3. scaffold ファイル群を作成（`synonyms.csv`, `stopwords.txt` 等）
4. `[init].link_mode` に従い `.tsm/models/ruri-v3-30m`, `.tsm/wnjpn.db`
   を作成（cache を上流として参照）
   - cache に対応リソースが無ければ `tsm setup` を促す警告を出して continue
5. `.tsm/wnjpn.db` から WordNet シノニムを `.tsm/tsm.db` に import
6. `.tsm/synonyms.csv` からユーザーシノニムを sync

#### `tsmd --embedder`

探索順は以下のとおり:

1. `state_dir/models/ruri-v3-30m/` （workspace override / link 経由）
2. `cache_dir/models/ruri-v3-30m/`
3. どちらにも無ければ起動失敗 → `tsm setup` または `tsm init` を促すエラー

symlink モードでリンク切れを検出した場合も同様にエラー終了する。

#### `tsm doctor`（多層チェック）

```text
✔ Workspace (.tsm/)
  ✔ models/ruri-v3-30m  →  cache (link alive)
  ✔ wnjpn.db            →  cache (link alive)
  ✔ tsm.db: 1,234 chunks, 1,200 vectors
✔ System cache (~/.cache/tsm/)
  ✔ models/ruri-v3-30m  →  HF cache (link alive)
  ✔ wnjpn.db            →  sources/wnjpn-v1.1.db (link alive)
  ✔ sources/wnjpn-v1.1.db: 203MB (matches manifest)
  ✔ manifest.json: 2 entries, all valid
```

リンク切れがどの層で起きているか正確に診断できる。

### 設定の整理

`config.rs` に新たに追加する関数:

```rust
pub fn cache_dir() -> PathBuf;              // 解決後の cache 起点
pub fn cache_models_dir() -> PathBuf;       // cache_dir + "models/ruri-v3-30m"
pub fn cache_wordnet_db_path() -> PathBuf;  // cache_dir + "wnjpn.db"
pub fn cache_sources_dir() -> PathBuf;      // cache_dir + "sources"
pub fn cache_manifest_path() -> PathBuf;    // cache_dir + "manifest.json"

pub fn setup_link_mode() -> LinkMode;       // cache 層
pub fn init_link_mode() -> LinkMode;        // workspace 層
```

既存の `models_dir()` / `wordnet_db_path()` は残す。
これらは `state_dir` 起点のパスを返し、workspace ごとに別モデル / 別
WordNet ファイルを置きたいとき（fine-tune 版モデル、別バージョン WordNet
の検証など）の override 経路として機能する。embedder はこの経路を最優先で
見たうえで cache_dir 起点のパスにフォールバックする。

## Rationale

### なぜ system-wide / workspace を 2 層に分けるか

- リソースの性質が異なる: モデルと WordNet DB は immutable / global、
  DB と CSV は mutable / workspace-local
- 同一マシン上で複数ワークスペースを持つ運用が前提
  （CLAUDE.md にもある通り、ナレッジは複数の作業ディレクトリにまたがる）。
  モデルが共有されないと UX として致命的

### なぜ XDG_CACHE_HOME を採るか（macOS でも）

- Rust エコシステム（rustup, uv, cargo）が XDG 系統を採用しており、
  この延長で扱えるユーザーが多い
- macOS の `~/Library/Caches` は GUI アプリ向けで、CLI からの可視性が低い
- 環境変数で上書き可能なので、Apple 流に拘りたいユーザーは
  `TSM_CACHE_DIR=$HOME/Library/Caches/tsm` を設定すれば良い

### なぜ link_mode を 2 層独立にするか

- ホスト ↔ DevContainer の往復で `~/.cache/tsm/` の絶対パスが
  環境間で異なる。workspace 層を `copy` にすれば workspace が自己完結し、
  環境間移動でも壊れない
- cache 層は machine 固有なので link_mode の選択は環境依存しない。
  逆に workspace 層は portability を要求されるシーンがある
- 「常に symlink」「常に copy」は両極端で、現実のユースケースを
  カバーしきれない

### なぜ `symlink` をデフォルトとするか

- 上流（HF cache, sources/）の実体は外部から見て immutable に近い扱い。
  symlink で参照すれば hardlink のような FS 制約（cross-device の EXDEV）
  を回避できる
- リンク切れは embedder 起動時の `stat` チェックと `tsm doctor` の両方で
  検出するため、症状が見えないまま壊れることを防げる
- ディスク重複ゼロが標準で得られる方がユーザー体験として優れる
- `copy` は明示選択（DevContainer 共有・移植性重視）でのみ採用

### なぜ `hardlink` モードを採用しないか

- ディスク節約（重複ゼロ）は `symlink` で達成できる
- 上流削除耐性は `copy` で達成できる
- 「重複ゼロ + 削除耐性」のニッチケースだけが hardlink の固有メリットだが、
  ユースケースが限定的
- 一方で実装コストは大きい:
  - cross-device 環境（コンテナマウント）での EXDEV フォールバック処理
  - ディレクトリには hardlink 不可なため、ruri モデルだけはファイル単位の
    特殊処理が必要
  - mode の数が増えるとテスト・ドキュメントの組み合わせが膨らむ
- 実装単純化を優先し採用しない

### なぜ ruri モデルは「ディレクトリ単位の symlink」にするか

- HF cache の snapshot ディレクトリ（`.../snapshots/<commit-hash>/`）は
  リビジョン固定で中身不変
- ファイル単位で 3 つの symlink を作るより、ディレクトリ 1 つの symlink で
  済ませる方がシンプル
- HF Hub 側で新しい revision が出ても、tsm cache は古い hash を指したまま
  安定する。新 revision に切り替えたい場合は `tsm setup` 系の再実行で対応

### なぜ WordNet に `sources/` を導入するか

- WordNet は GitHub release から直接 DL→展開で、HF cache のような
  中間ストアが存在しない
- `sources/wnjpn-<version>.db` を tsm 自身が所有し、`wnjpn.db` をそこへの
  symlink にすることで、cache_dir 直下のリソースをすべて symlink で
  統一できる（構造的整合性）
- バージョン並存が可能（v1.1 と v1.2 を `sources/` に置いて、`wnjpn.db`
  のリンク先を切り替えれば即時ロールバック）
- copy モード時は cache_dir 直下に直接展開してもよいが、`sources/` を
  常に持つ方が manifest.json のスキーマが対称的になり実装が単純

### なぜ workspace 内のパスを override 経路として残すか

- 別のモデル（ruri 以外、fine-tune 版など）や別 WordNet バージョンを
  workspace 単位で差し替えたいケースが存在する
- workspace 内のファイルは元々 mutable / workspace-local であり、
  override 経路として使うのは設計上自然
- 削除すると override の手段がなくなり、設定柔軟性が下がる

### なぜ embedder は state_dir 優先 + cache_dir フォールバックの順にするか

- ADR-0001 の延長で stateless（読み取り専用）であることは維持
- 探索順を「workspace → cache」とすることで、
  workspace 内 override が即時に効く
- cache に無いケース（setup 未実行）も明示的なエラーで早期検出できる

## Consequences

### Positive

- ワークスペースサイズが最大 ~347MB 縮小（モデル + WordNet 合計、
  symlink モード時）
- "Run once per machine" が文字通り成立する
- doctor のレポート構造が層単位で明確になる
- embedder の前提（cache 専一にフォールバック）が明文化され、
  将来のプラグイン作者が workspace 内モデルを期待しなくなる
- 仕様書（`docs/command-reference.md`）と実装の乖離が解消される
- DevContainer ↔ ホスト併用ユースケースを `[init].link_mode = copy` で
  サポートできる
- `manifest.json` により cache の状態が機械可読になり、doctor の
  整合性チェックの根拠が明確化される
- 既存の `models_dir()` / `wordnet_db_path()` を残すことで、
  workspace 単位でのモデル / WordNet 差し替えが従来通り可能

### Negative

- doctor の出力フォーマットが変わる（JSON 構造を見ている自動化があれば破壊）
- ワークスペースを別マシンに丸ごとコピーした場合、別マシン側で
  `tsm setup` が必要になる（symlink モード時。`copy` モードならついてくる）
- mode の組み合わせが 2×2 = 4 通り発生し、テスト・ドキュメントが増える
- multi-stage symlink（workspace → cache → HF cache）はリンク切れ時の
  原因切り分けに doctor 出力を要する

### Follow-ups

- 実装 PR の分割案:
  1. `cache_dir` 解決ロジックの追加（`config.rs`）
  2. `LinkMode` enum と `[setup].link_mode` / `[init].link_mode` 設定の追加
  3. `cache_models_dir()` / `cache_wordnet_db_path()` / `cache_sources_dir()`
     / `cache_manifest_path()` 追加と embedder のフォールバック実装
  4. `tsm setup` の動作変更（cache 専一化 + link_mode 適用 + manifest 更新）
  5. `tsm init` の動作変更（cache から WordNet を読む + workspace の
     symlink/copy 作成）
  6. doctor の多層チェック実装
  7. `docs/command-reference.md` の記述を実装と一致させる
- テスト:
  - cache_dir の解決優先順位（env > toml > default）の単体テスト
  - link_mode 別の cache 構築テスト（symlink / copy）
  - link_mode 別の workspace 構築テスト（symlink / copy）
  - リンク切れ検出の doctor テスト（cache 層 / workspace 層それぞれ）
  - manifest.json の読み書きと整合性検証の単体テスト
  - workspace 内 override が cache よりも優先されることのテスト
- 設定ドキュメント:
  - `tsm.toml.example` に `cache_dir`, `[setup].link_mode`,
    `[init].link_mode` の例を追加
  - `docs/configuration.md` に第 1 / 第 2 層の概念と link_mode の
    使い分け、override の仕組みを記述
- README:
  - `tsm setup` が machine-global であることを明記
  - DevContainer / 別マシン併用時の `link_mode = copy` の使い方
  - workspace 内に別モデルを置いて差し替える方法

[^no-hardlink]: ディスク重複ゼロは `symlink` で、上流削除耐性は `copy` で
    達成できるため、`hardlink` 固有のメリット（重複ゼロ + 削除耐性）の
    ニッチケースが限定的。詳細は Rationale の
    「なぜ `hardlink` モードを採用しないか」を参照。
