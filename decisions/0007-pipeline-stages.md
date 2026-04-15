# ADR-0007: tsm の処理を Index / Search の 2 パイプラインに分解し段を明示する

- **Status**: **Proposed**
- **Date**: 2026-04-15
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0005](./0005-embedder-binary-consolidation.md)

## Context

tsm は段の境界が曖昧なまま機能追加が重ねられており、以下の問題が観測されている。

- `indexer/mod.rs` にチャンク分割・埋め込み・DB 書き込みが混在し、責務が不明瞭
- `tsmd` が接続ごとに naked thread を無制限に `spawn` しており、並列度が制御できない
- `embedder` はシングルスレッド accept で逐次処理するが、この「直列契約」が暗黙
- 将来のプラグイン機能のフック点が未設計

加えて、複数のプラグイン構想が「どの段のどこに挿すか」を個別に定義しようと
していて、共通の骨格がないままプラグイン種別だけが増えつつある。
パイプラインの段を先に確定させないと、プラグイン API が後から破綻する。

## Decision

tsm の処理を以下 2 つのパイプラインとして定義する。

### Index パイプライン（3段）

```text
Prepare（並列可）─▶ Embed（直列）─▶ Persist（直列）
```

| 段 | 性質 | 責務 |
|---|---|---|
| Prepare | IO/CPU bound、ファイル単位で並列 | Load / frontmatter / チャンク分割 / メタデータ抽出 |
| Embed | GPU bound、直列（embedder 契約） | embedder 呼び出し |
| Persist | DB Mutex bound、直列 | FTS5 / vector / metadata 書き込み（1 ファイル = 1 transaction） |

### Search パイプライン（4段）

```text
Plan ─▶ Retrieve（FTS5 / Vector 並列）─▶ Rank ─▶ Format
```

| 段 | 性質 | 責務 |
|---|---|---|
| Plan | 直列、軽量 | 形態素解析 / クエリ埋め込み |
| Retrieve | FTS5 と Vector を並列実行 | 2 系統の候補集合を得る |
| Rank | 直列 | RRF 統合 / 時間減衰 / ステータスペナルティ / filter |
| Format | 直列、純関数 | 出力形式への整形（テキスト / JSON / 将来の MCP 等） |

### 実装上の不変条件

段分解後も以下を維持する。破ると簡単に性能劣化する。

1. **バッチ粒度**: 段間を流れる最小単位は「チャンクの集合」。per-chunk に flatten しない
2. **Persist トランザクション境界**: 1 ファイル = 1 transaction を維持する
3. **Tokenizer 整合性**: Prepare と Plan で同一 tokenizer 実装を参照する。差し替え時は再インデックス必須
4. **Embed 直列契約**: embedder の呼び出しは Embed 段からのみ行い、他段・プラグインから直接叩かない

### プラグインのフック点

将来のプラグイン種別は以下の段に配置する。

| プラグイン | 段 |
|---|---|
| metadata | Prepare |
| indexer (transformer) | Prepare |
| embedder | Embed |
| filter（除外） | Rank |
| mask（伏字化） | Format |
| output（出力形式追加） | Format |
| tokenizer | Prepare + Plan（横断） |
| source（外部ソース） | pipeline 外（MD 化して Prepare に流す） |

### visibility の扱い（セキュリティ境界）

将来の visibility プラグインは **fail-closed** を原則とする。
プラグインが失敗したら検索結果を返さない方向に倒し、fail-open（素通し）は禁止する。
`.tsmignore` は Prepare 段の手前のゲートで適用し、そもそも DB に入れない。
監査ツールは検索パイプラインとは独立した運用コマンドとして実装する。

### 性能検証義務

段分解の PR では以下の指標をベースラインと比較する。5% を超える劣化が
あれば原因を特定してから merge する。

- フルインデックススループット
- 差分インデックスレイテンシ
- 検索レイテンシ（FTS5 only / ハイブリッド）
- embedder 呼び出し回数

## Rationale

**なぜパイプライン先、プラグイン後か**:
プラグインのフック点はパイプラインの段の境界と等しい。段を先に確定させると、
プラグイン API は「各段の入出力型」に従うだけで済む。順序を逆にすると、
プラグイン都合で段が歪み、責務と並列度の設計が崩れる。

**なぜ 3 段 + 4 段という粒度か**:
段の境界は「並列度とリソース制約が変わる点」で切る。Prepare（並列）/ Embed
（GPU 直列）/ Persist（DB Mutex 直列）は実際のリソース境界と一致する。
それ以上細かく切ると段間オーバーヘッドと型定義コストが増えるだけで、
責務分離の効果が薄れる。Search 側の Rank と Format の分離は
「順序決定」と「表示」の責務が明確に異なるため必要。

**なぜ直列段を残すか**:
Embed は embedder のシングルスレッド accept、Persist は `Arc<Mutex<Connection>>`
で既に直列。段分解は事実の明文化であり、新たな直列化ではない。
むしろ stage pipelining（A ファイルの Persist 中に B ファイルの Embed を
並行させる）の余地を作る方向に働く。

## Consequences

### Positive

- 段ごとの並列度・リソース制約が明示され、IO 戦略とリソース制御の設計が可能になる
- プラグインのフック点が段の境界として統一され、種別間で一貫する
- embedder 直列契約が明文化され、将来のプラグイン作者が誤って並列化しない
- visibility の fail-closed 原則が明記され、セキュリティ境界として扱える
- 責務分離により各段の単体テストが書きやすくなる

### Negative

- 既存コード（`indexer/mod.rs`, `searcher.rs`）のリファクタコストが発生する
- 段間のデータ受け渡しでチャンネル / struct passing のオーバーヘッドが生じる
  （Rust では通常無視できるレベルだが、バッチ粒度を誤ると劣化する）
- 段ごとに入出力型を分けると型定義が増え、デバッグ時の stack trace が
  段をまたいで読みにくくなる場合がある
- 段の追加・分割は ADR レベルの判断となり、意思決定のコストが上がる
  （過剰設計の抑止としては望ましい）

### Follow-ups

- **E2E テスト補強**: #149（並列投入 race）、#150（embedder クラッシュ時の search 挙動）、
  #151（reindex × search 競合）。failpoint 注入による途中失敗ロールバック検証は
  段分解リファクタと同時に `fail` crate 導入で別途対応
- **ベースラインベンチマーク整備**: 段分解前に criterion 等で計測基盤を作り、
  段分解 PR ごとに回帰チェック
- **段分解リファクタ PR の計画**: Prepare / Embed / Persist の順で段を切り出し、
  E2E グリーンを維持しながら分解
- **段間データ型の設計**: 共通 Envelope か段ごとの型か、別ドキュメントで詰める
- **段の手前の bounded queue 設計**: 受付層の背圧設計（busy / drop / block）
- **プラグインのフック仕様**（before / around / after、実行環境）は別 ADR で扱う
