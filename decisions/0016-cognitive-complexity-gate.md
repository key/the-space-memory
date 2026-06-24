---
status: accepted
created: 2026-06-24
updated: 2026-06-24
---

# ADR-0016: 認知的複雑度ゲート（clippy グローバル閾値 + 属性免除）

- **Deciders**: key
- **Related**:
  [ADR-0007](./0007-pipeline-stages.md)

## Context

現状のコード複雑度ゲートは lizard の **cyclomatic complexity**（CCN）のみ
（`metrics.yml` の complexity job、`lizard -Tcyclomatic_complexity=15`、
baseline 13 を超えたら fail）。CCN は分岐の数を数えるが、**ネストの深さに
ペナルティを与えない**。このため次の盲点がある。

1. **ネスト由来の「隠れ複雑度」を取りこぼす**
   分岐数が少なくても、深い `match` / `if` / ループのネストで人間の読みやすさが
   著しく落ちる関数を CCN は低く評価する。実例として `run@watcher_mode.rs` は
   認知的複雑度 21 だが CCN は 15 未満で、CCN ゲートをすり抜けていた。
   backfill_vectors の分解（#267）も同型で、CCN は元から低かったが、巨大な
   `match` のネストで認知的負荷が高かった。

2. **可読性の指標として CCN は人間の体感とずれる**
   cyclomatic は「テストに必要な経路数」に近く、保守時の「読み下しやすさ」とは
   別物。フラットな 10 分岐より、3 段ネストした 4 分岐の方が読みにくい。

**Cognitive complexity**（Sonar / G. Ann Campbell の定義）はネストにペナルティを
与える指標で、人間が読み下す際の負荷に近い。CCN ゲートを置き換えるのではなく、
**補完**するために認知的複雑度ゲートを導入する。

## Decision

clippy の `clippy::cognitive_complexity` lint（nursery）を採用し、
**単一のグローバル閾値 + 属性による明示免除**で運用する。

### 構成

| 要素 | 設定 |
|---|---|
| 閾値 | `clippy.toml` の `cognitive-complexity-threshold = 15`（cyclomatic ゲートの CCN15 と対称に置く） |
| 有効化 | `src/lib.rs` に `#![warn(clippy::cognitive_complexity)]`（nursery は既定 allow のため明示的に有効化が必要） |
| 免除 | 閾値超過の関数は (a) リファクタで閾値以下に下げる、または (b) `#[allow(clippy::cognitive_complexity)]` + 理由コメントで明示免除する |

CI の `cargo clippy -- -D warnings`（ci.yml）が既に warning を deny しているため、
`#![warn(...)]` で有効化した時点で、閾値超過の関数は **免除属性を付けない限り
ビルドを止める**。すなわちゲートは「閾値超過ゼロ、ただし明示免除は許す」という
ハードゲートになる。

### 免除リスト＝可視化された複雑度負債

`#[allow(clippy::cognitive_complexity)]` を付けた関数の集合が、そのまま
「意図的に複雑なまま残している関数」の一覧になる。各免除に理由コメントを必須と
することで、

- `git grep "allow(clippy::cognitive_complexity)"` で複雑度負債を一覧できる
- 各関数が「なぜ複雑なままで良いか」を**コードの隣に**説明として持つ

免除の対象は、entry point / mode dispatch / CLI ディスパッチのように
**本質的に分岐が多く、分解するとかえって追いにくくなる**関数に限る。
ロジックの複雑さ（分解可能なもの）は免除せずリファクタする。

### 観測の二層モデル

複雑度の観測は目的に応じて 2 つのスコープを使い分ける。

| スコープ | いつ | 何を見る |
|---|---|---|
| **diff スコープ** | 各 PR の CI | 変更で閾値を超えた関数のみ。clippy ゲート（`-D warnings`）が自動で止める |
| **whole-repo** | リリース PR 作成時 | リポジトリ全体の認知的複雑度ランキング。負債の総量と分布を俯瞰し、閾値や免除リストを見直す |

diff スコープは通常開発の安全網、whole-repo はリリースごとの健康診断。
whole-repo レポートの生成手段（スクリプト化するか、手動 clippy 実行か）は
本 ADR では定めない（対象外）。

## Rationale

**なぜ clippy か。**
`cargo clippy -- -D warnings` は既に CI（ci.yml）にあり、**新規依存ゼロ**で
認知的複雑度ゲートを追加できる。ツール・CI ジョブ・学習コストの追加がない。

**なぜグローバル閾値 + 属性免除か（per-file 数値閾値ではなく）。**
clippy は per-file / per-module の数値閾値を持たず、単一のグローバル閾値のみ。
per-file の細かい調整は属性免除で代替する。属性免除はバイナリ（on/off）だが、
理由コメントを必須にすることで「なぜこの関数だけ高くて良いか」を残せる。
これは lizard の baseline カウント（`13`）のような**不透明な数値**より
自己文書的で、grep 可能。

**なぜ閾値 15 か。**
cyclomatic ゲート（lizard）の CCN15 と同じ数値に揃え、「両軸とも 15」という
覚えやすい基準にする。認知的複雑度と cyclomatic は別指標で値は直接比較できないが、
ゲートの運用基準として同一の数値を置くことで、開発者が 2 つの閾値を別々に
記憶する負担を避ける。15 は entry point / mode dispatch など本質的に複雑な少数の
関数のみを捕捉し、それらは属性免除へ回す。具体的な免除対象の選定は本 ADR では定めない。

なお、認知的複雑度は同一コードでも cyclomatic より高く出る（ネストペナルティを
加算するため）。したがって「閾値 15」は CCN15 と数値が同じでも**相対的にはより
緩い**ゲートになる。より厳しい値（例: 12）も選択肢だが、初期導入では覚えやすさと
免除対象の少なさを優先して 15 を置く。

検討した代替案と却下理由は以下のとおり。

- **rust-code-analysis（Mozilla）**: per-function で cognitive / cyclomatic /
  halstead / MI を JSON 出力でき、スクリプトで完全カスタムな per-file 閾値が
  組める。だが**新規ツール + 新規 CI ジョブ依存**が増える。per-file 閾値の
  柔軟性は、現状の規模では属性免除で十分代替でき、依存追加に見合わない。
  将来 per-file 閾値が本当に必要になれば再検討する。
- **lizard の閾値を流用 / 拡張**: lizard は cyclomatic complexity のみで
  cognitive complexity を算出しない。補完目的を果たせない。
- **CCN ゲートの置き換え**: cyclomatic と cognitive は別の側面（経路数 vs
  ネスト負荷）を測る。片方で他方を代替できないため、CCN ゲート（lizard）は
  残し、認知的複雑度ゲートを**並置**する。

## Consequences

### Positive

- ネスト由来の隠れ複雑度を検出し、CCN ゲートの盲点（`run@watcher_mode.rs` 型）を
  補完する。
- `#[allow]` 免除リストが複雑度負債の**単一観測点**になり、各負債が理由コメントを
  伴う。
- 既存ツールチェーン（clippy）に乗るため新規依存・新規 CI ジョブがない。

### Negative

- clippy の `cognitive_complexity` は **nursery** lint。アルゴリズムが将来変わる
  可能性があり、値が安定とは限らない（閾値の再調整が必要になりうる）。
- 単一グローバル閾値のみで、per-module の細かい調整はできない（属性免除で対処）。
- 有効化時点で閾値超過の既存関数すべてに `#[allow]` + 理由を付ける**初期コスト**が
  かかる。
- clippy が算出する認知的複雑度の値は実装依存で、Sonar の定義と完全一致しない
  （絶対値ではなく相対比較・閾値運用の道具として扱う）。
