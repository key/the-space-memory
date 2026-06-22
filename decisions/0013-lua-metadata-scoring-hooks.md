# ADR-0013: メタデータ抽出とスコアリングを Lua フックで拡張可能にする

- **Status**: **Proposed**
- **Date**: 2026-06-22 (Proposed)
- **Deciders**: key
- **Related**:
  [ADR-0007](./0007-pipeline-stages.md)（パイプライン段の確定）,
  [ADR-0011](./0011-uninitialized-db-failfast-and-readonly-doctor.md)（fail-fast 哲学）,
  [ADR-0012](./0012-output-channel-model.md)（出力チャネルモデル）

## Context

ADR-0007 で Index / Search のパイプライン段を確定させ、プラグインのフック点を
段の境界に揃えることを決めた。ただしフック仕様そのもの（実行環境・before/around/after）は
別 ADR 送りとした。本 ADR はその第一弾として、`metadata`（Prepare 段）と
スコアリング（Rank 段）のフック機構を定義する。

駆動要因は再スコアリング精度の問題である。

- **語彙のハードコード**: 現状 `config::status_penalty()` が
  `superseded=0.2 / rejected=0.3 / outdated=0.4` と語彙を core にベタ書きしている。
  `company`（`current/adopted/active/deprecated`）と ADR（`Proposed/Accepted/Deprecated/Superseded`）で
  語彙が食い違い、`Deprecated` のように core が知らない値は素通し（係数 1.0）になる。
- **抽出箇所の固定**: `frontmatter.rs` は `---` の YAML ブロックしか見ない。
  ADR は Status / Date を `- **Status**: **Accepted**` のように本文の箇条書きで持つため、
  status も基準日も拾えず、再スコアの恩恵をほぼ受けていない。
- **拡張の置き場所がない**: ソースごとに抽出ルールもスコア方針も異なるが、
  これを core に足し続けると語彙と分岐が無限に増える。設定ファイル（`tsm.toml`）に
  ロジックを書くのは「設定の肥大化」と「ユーザーに正規表現を書かせる」という
  二重の悪手であり、採らない。

ユーザーが編集可能なスクリプトで、メタデータの抽出とスコアリングを記述できる
拡張面が必要である。

## Decision

### 全体方針: core を語彙非依存にし、ポリシーを Lua フックへ追い出す

core は「ベクトル検索・RRF・フックを呼ぶ器」に徹し、メタデータの**意味付け**
（どの status をどれだけ減点するか等）を一切持たない。`status_penalty()` の
ハードコード語彙は廃止する。現状の `time_decay` / `status_penalty` の挙動は
**デフォルト同梱スクリプトとして移植**し、ユーザー未設定時は現状の挙動を
完全に再現する（リグレッションなし）。

### スクリプトランタイム: 埋め込み Lua（mlua, vendored Lua 5.4）

- **Lua を採用**。一般的で知名度が高く、ソースを直接編集するだけでよい
  （別途のビルド工程・ツールチェーンを必要としない）ため「ユーザー編集可能な
  スクリプト」要件に合致する。反映には `tsm restart` が要る（後述のライフサイクル）が、
  WASM のようなコンパイル工程は不要。
- `mlua` の vendored Lua 5.4 を使う。C コンパイルを伴うが、本プロジェクトは既に
  `rusqlite` の bundled feature で C ビルドを要求しているため、新たなツールチェーン
  要件は増えない。
- ライセンスは Lua / mlua とも MIT で、本プロジェクト（MIT）に適合。

### 2 つのフックと段への対応

| フック | 段 | 実行頻度 | 入力 | 出力 |
|---|---|---|---|---|
| `extract` | Prepare | index 時（ファイル変更時） | path, body, 解析済み frontmatter, source_type | 任意キーのメタデータ map |
| `score` | Rank | クエリ時 × ヒット件数 | 抽出済みメタデータ map, rrf, source_type, path | 係数（数値） |

抽出（重くてよい・index 時）とスコアリング（ホットパス・クエリ時）は性質が
正反対のため、1 スクリプトに混ぜず別フックに分離する。これにより、ユーザー
スクリプトが検索ホットパスで生テキストを触ることを構造的に防ぐ。

#### `extract` フック（Prepare）

```lua
-- ctx.frontmatter は core が YAML をパース済みで table として渡す
-- (サンドボックス Lua に YAML パーサを持たせないため)
-- ctx = { path, body, frontmatter = {...}, source_type, metadata = {累積} }
function extract(ctx)
  return { status = "deprecated", effective_date = "2026-05-07" }
end
```

- 出力は**任意キーの map**。core は中身の意味を知らず、1 ドキュメント 1 レコードの
  JSON として DB に保存する。
- チェーン内では後段スクリプトが前段までの累積 map を `ctx.metadata` で参照できる。
  キー衝突は**後勝ち**（明示的・単純）。
- YAML frontmatter のパースは core が担い、結果を table で渡す。Lua 側で YAML を
  パースさせない。

#### `score` フック（Rank）

```lua
-- ctx = { metadata = {...}, rrf, source_type, path }
-- 組み込み関数: decay(date, half_life_days), today()
-- 以下は company 語彙 (deprecated) に合わせたユーザーカスタム例。
-- デフォルト同梱の score/10-default.lua とは語彙が異なる（下記参照）。
function score(ctx)
  local p = ({ deprecated = 0.4, superseded = 0.2, rejected = 0.3 })[ctx.metadata.status] or 1.0
  return p * decay(ctx.metadata.effective_date, 90)
end
```

- 各 `score` スクリプトは**係数（数値）**を返し、チェーンは**積**で合成する。
  戻り値は**有限かつ非負の数値**でなければならない（バリデーションは後述）。
- core の最終スコアは次に置き換える:

  ```text
  score = rrf * weight * Π(score フックチェーンの係数)
  ```

  `rrf` と source `weight` は構造として core に残し、`decay * penalty` 相当を
  フックが供給する。デフォルト同梱の `score/10-default.lua` が現状の
  `time_decay` × `status_penalty` を再現する（語彙は現状どおり
  `superseded=0.2 / rejected=dropped=0.3 / outdated=0.4 / その他=1.0`）。
- core が Lua へ渡すプリミティブは `decay()` / `today()` 程度に絞る。
  DB / FS / embedder / ネットワークへのアクセスは遮断する（ADR-0007 の embed 直列契約と
  daemon の DB 所有権を侵さない）。

### 出力のバリデーション

フックは「エラーを出さないが不正な値」を返しうるため、戻り値は型で検証する。
不正値はランタイムエラーと同じく fail-safe で扱う（後述の失敗時セマンティクス）。

- **`score` の戻り値**: **有限かつ非負の数値**のみ許容する。非数値・`NaN`・`±Inf`・
  負値はランキングを破壊するため、不正値として **中立 1.0 にフォールバック + warn**。
  これはエラーを発生させない合法的な Lua 値でも適用する（codex review 指摘）。
- **`extract` の出力**: **JSON シリアライズ可能なスカラ値の map**のみ許容する。
  循環参照・関数値・シリアライズ不能な値を含む場合はその寄与を破棄 + warn。
  値型・サイズ/深さの具体上限はサンドボックス上限とあわせて別途定める（Follow-ups）。
- **ロード時の関数存在チェック**: 構文/コンパイルが通っても、期待する
  エントリポイント関数（`extract` / `score`）が未定義ならロード時に fail-fast する。
  戻り値の形状は動的なため実行時バリデーション（上記）で担保する。

### 発見と順序: ディレクトリ規約 + 数字プレフィックス

```text
.tsm/hooks/
  extract/
    10-frontmatter.lua
    20-adr.lua
  score/
    10-default.lua   # 同梱。status_penalty + time_decay の移植
```

- 実行順 = ファイル名昇順。`tsm.toml` には一行も足さない。
- 認識する拡張子は **`.lua` のみ**。一時的に無効化したい場合は
  `20-adr.lua-orig` のように拡張子を外す（削除不要）。

### 並列度の不変条件

- **`extract`**: ファイル across で並列、ファイル内のチェーンは順序実行。
  mlua の `Lua` VM はスレッド間共有を前提にしない設計（既定では `!Send`）のため、
  並列は**ワーカースレッドごとに専用 Lua VM を 1 個持つ**形で実現する。
- **`score`**: Rank は直列（ADR-0007）かつホットパスのため、**1 VM・ヒットごとに
  逐次評価**とする。スレッドへ撒かない（スレッド生成コストが Lua eval を上回る）。

### ライフサイクル: 起動時ロード・以降不変

- `tsm start` 時に全 `.lua` を**一度だけ**読み込み、コンパイルして構文エラーを
  即検出し、フックチェーンをメモリにキャッシュする。以降ディスクは読み直さない。
- ワーカー VM は、ディスクではなくメモリ上のキャッシュ（ソース / バイトコード）から
  ロードする。ディスク I/O は起動時の 1 回のみ。
- 帰結として **Lua を編集したら `tsm restart` が必要**（既存の segmenter / tokenizer
  キャッシュと同じ手触り）。

### 失敗時セマンティクス（2 層）

| いつ | 挙動 |
|---|---|
| **ロード時（`tsm start`）の構文 / コンパイルエラー / エントリポイント関数の不在** | **fail-fast**。daemon を起動させず、該当ファイル名とエラーを表示（ADR-0011 の哲学、ADR-0012 のチャネルで surface） |
| **実行時（特定ファイルの `extract` / 特定ヒットの `score`）のエラーまたは不正な戻り値** | **fail-safe**。`log::warn!` で記録し（ADR-0012: daemon は `tsmd.log`、捕捉される raw stderr は `tsmd-stderr.log`）、`extract` はその寄与を捨て、`score` は中立の係数 1.0 にフォールバックする。daemon は落とさない |

`score` を fail-closed にしないのは、これが関連度の調整でありセキュリティ境界では
ないため。ADR-0007 の fail-closed 原則は visibility フック限定である。

### ストレージ

- ドキュメントに任意キーのメタデータを保持する `metadata` JSON カラムを追加する。
- スキーマ変更を伴うため、導入時は `rebuild --apply` が必要（既存の運用方針どおり）。

## Rationale

**なぜ宣言的ルール（`tsm.toml` 内の regex マップ）ではないか**:
設定ファイルにロジックを足すと肥大化し、かつ「ユーザーに正規表現を書かせる」
実質的なミニ言語の押し付けになる。スクリプトという第一級の拡張面のほうが、
抽出の自由度と可読性で勝る。

**なぜ Rhai ではないか**: Rhai は埋め込み専用で世間に知られていない。
「一般的な言語を使いたい」という要件を満たさない。

**なぜ WASM ではないか**: WASM はコンパイル済みバイナリであり、編集のたびに
ソースのコンパイル + ツールチェーンを要する。Lua はソースを直接編集し
`tsm restart` でリロードするだけ（別途ビルド工程が不要）で済むため、編集の
手数が構造的に少ない。WASM はランタイムも重く、
データ受け渡しに ABI / シリアライズのコストが乗る。信頼できない第三者の
コンパイル済みプラグイン（filter / source など）を sandbox 実行する世界では
WASM が適するが、それは本フックとは別レイヤとして将来必要になったときに足す。

**なぜ in-process Rust トレジストリではないか**: 抽出器を足すたびに再コンパイルが
必要で、ユーザー編集の要件を満たさない。

**なぜ外部サブプロセス（任意言語）ではないか**: `score` はクエリ時にヒットごと
評価されるため、プロセス起動を都度行うのは非現実的。段ごとにランタイムを分けると
保守対象が増える。1 ランタイム（Lua）で両フックを賄う。

**なぜ抽出とスコアを 1 スクリプトにまとめないか**: Prepare（cold path）と
Rank（hot path）の境界を曖昧にし、ユーザースクリプトを検索ホットパスで
生テキストごと走らせることになる。ADR-0007 が段を分けた意図に反する。

## Consequences

### Positive

- メタデータ語彙が core から消え、`company` / ADR など語彙差を core 改修なしに吸収できる。
- ADR 本文（箇条書きの Status / Date）からメタデータを抽出できるようになる。
- 抽出済みメタデータが DB にあるため、`score` フックのみ再評価する運用
  （再インデックス・再埋め込み不要の「再スコアリング」）が可能になる。
  ただしこれは **`extract` の出力スキーマを変えない範囲に限る**。`score` が
  新規・改名したキーを要求するのに既存行が古いメタデータのままだと、サイレントに
  誤ったランキングになるため、`extract` を変更したら再インデックスが必要（下記 Negative）。
- フックランナーが段の境界に統一され、将来 filter / mask / output などを
  同じ仕組みで段違いに足せる（本 ADR では実装しない）。
- デフォルト同梱スクリプトにより、未設定時の挙動は現状と同一でリグレッションがない。

### Negative

- `mlua` 依存とワーカーごとの Lua VM メモリが増える。
- `metadata` カラム追加によりスキーマが変わり、導入に `rebuild --apply` が要る。
- フック編集は `tsm restart` が必要（起動時キャッシュのため）。
- ユーザーは Lua とメタデータ契約（入力 ctx / 出力形式）を学ぶ必要がある。
- `score` がヒットごとに Lua eval を行うため、クエリ時コストが増える
  （サンドボックスの operation 上限とヒット件数の小ささで抑制する）。
- 現状の既定挙動が同梱スクリプトに移るため、その配置・scaffold が新たな
  インストール上の関心事になる。
- `extract` を変更すると DB 内の既存メタデータが古いままになり、`score` のみ
  再評価する再スコアリングではサイレントな不整合を招く。`extract` 変更時は
  再インデックスが必要という運用制約を負う。

### Follow-ups

- 実装 PR の分割案（ランタイム導入 → extract → score → デフォルト移植 → schema）。
- 再スコアリング運用コマンド（`tsm rescore` 相当）の設計。
- Lua へ渡す ctx フィールドと組み込み関数セットの厳密仕様。
- デフォルトフックの同梱場所と `tsm init` / `setup` での scaffold 方法。
- `tsm doctor` でのフックのロード状態 / エラーの可視化。
- サンドボックス上限（max_operations / 呼び出し深さ / メモリ）の既定値。
- 信頼できない第三者プラグイン向けの将来的な WASM レイヤ。
- filter / mask / output フックへの拡張（別 ADR）。
