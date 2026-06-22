# Architecture Decision Records (ADR)

このディレクトリは tsm の設計判断記録（ADR）を保存する。
過去の意思決定の **背景・選択肢・理由** を後から辿るための一次資料であり、
コードや README が答えない「なぜこの設計か」をここで答える。

## Index

| # | タイトル | Status | Date |
|---|---|---|---|
| [0001](./0001-process-roles-and-responsibilities.md) | プロセスの役割と責務分担 | Deprecated（→ 0002） | 2026-04-01 |
| [0002](./0002-watcher-thread-integration.md) | プロセスの役割と責務分担（watcher スレッド化） | Accepted | 2026-04-03 |
| [0003](./0003-config-via-resolved-config.md) | 設定値は ResolvedConfig シングルトンで管理する | Accepted | 2026-04-03 |
| [0004](./0004-user-dictionary-design.md) | ユーザー辞書の設計方針 | Accepted（2026-04-14 改訂） | 2026-04-28 |
| [0005](./0005-embedder-binary-consolidation.md) | tsm-embedder 統合と 2 バイナリ構成 | Accepted | 2026-04-28 |
| [0006](./0006-cli-option-design.md) | CLI オプション設計の判断基準 | Accepted | 2026-04-28 |
| [0007](./0007-pipeline-stages.md) | tsm の処理を Index / Search の 2 パイプラインに分解 | Accepted | 2026-05-07 / 2026-06-23 |
| [0008](./0008-setup-init-separation.md) | tsm setup と tsm init の責務分離 | Accepted（一部 → 0009） | 2026-05-08 |
| [0009](./0009-workspace-and-content-model.md) | プロジェクトとコンテンツ参照モデル | Proposed | 2026-05-19 |
| [0010](./0010-per-project-daemon.md) | tsmd の per-project identity と socket 競合解決 | Accepted | 2026-05-19 / 2026-06-22 |
| [0011](./0011-uninitialized-db-failfast-and-readonly-doctor.md) | 未初期化 DB での fail-fast と doctor の read-only 化 | Accepted | 2026-06-22 |
| [0012](./0012-output-channel-model.md) | 出力チャネルモデル（ユーザー出力 / ログ / エラーの分離） | Accepted | 2026-06-22 |
| [0013](./0013-lua-metadata-scoring-hooks.md) | メタデータ抽出とスコアリングを Lua フックで拡張可能にする | Accepted | 2026-06-22 |

新規 ADR 追加時は上記表にも 1 行追加すること。

## ADR の方針

### ADR は「あるべき状態」を定義する

ADR は **target state（到達したい状態）** の定義に専念する。
現状からのマイグレーション手順は ADR の対象外として扱う。

- **書く**: 目指す設計、選択肢、根拠、影響範囲（Positive / Negative）
- **書かない**:
  - 現状から target state への移行手順
  - 移行用の一時的なロジック（実装後に削除する種類のコード）
  - 個別 PR の作業ログ

理由: 移行手順は実装と同時に陳腐化するが、ADR は何年も参照され続ける。
混在すると ADR の長期参照価値が下がる。
**移行手順は `CHANGELOG.md`、リリースノート、PR 説明文側に書く。**

### 1 ADR = 1 決定

複数の独立した決定を 1 ファイルにまとめない。
新しい決定は新しい番号で立てる（例: 0008 で setup/init の責務分離、
0009 でその後のプラグイン API、…）。

判断が変わった場合は:

- 旧 ADR の Status を `Deprecated` にし、置換先 ADR を Related で明示
- 新 ADR でその経緯を Context に記述

例: ADR-0001 → ADR-0002（watcher スレッド化への変更）

### Status の遷移

| Status | 意味 |
|---|---|
| `Proposed` | 議論中。実装前。マージ前のドラフト |
| `Accepted` | 採用決定。実装は別 PR で進める |
| `Deprecated` | 後続の ADR で置換された。Related で置換先を示す |
| `Superseded by ADR-NNNN` | 部分的に上書きされた場合 |

ドラフト中の PR は `Proposed`。**マージするときは Status を `Accepted` に変更する**
（マージ＝決定確定）。マージ前の最終コミットで frontmatter の `status` と `updated` を
更新し、上記索引行の Status 列も `Accepted` に合わせる。

### 番号付け

- 連番 4 桁: `0001` から始まる
- ファイル名: `NNNN-kebab-case-title.md`
- 番号は再利用しない（Deprecated になっても番号は維持）

### フォーマット

各 ADR は以下のセクションを持つ:

```markdown
---
status: proposed | accepted | deprecated
created: YYYY-MM-DD
updated: YYYY-MM-DD
superseded_by: NNNN-filename.md   # deprecated の場合のみ
---

# ADR-NNNN: タイトル

- **Deciders**: 名前
- **Related**: 関連 ADR や Issue へのリンク

## Context

なぜこの判断が必要になったか。背景・前提・問題点。

## Decision

採用する設計の中身。表・コードブロック・図を使って具体的に。

## Rationale

なぜ他の選択肢ではなくこの設計か。代替案も簡潔に列挙し、
却下理由を残す（後から「なぜ X じゃないのか」を辿れるように）。

## Consequences

### Positive

- 期待される利点

### Negative

- 受け入れるトレードオフ
```

ADR は target state を定義するものであり、タスクリストではない。
実装 PR の分割案・後続 ADR の予告・運用 TODO のような**時間とともに変化し
陳腐化する内容は ADR に書かない**（`### Follow-ups` のような節は設けない）。
これらは PR 説明文・issue・`CHANGELOG.md` 側に置く。

### 言語

- 本文は **日本語**（プロジェクトのチャット運用言語に合わせる）
- 用語・コード片・関数名・パス名は英語のまま

### コミット粒度

- ADR の追加 / Status 変更は **単独コミット** が望ましい
- ブランチ名: `docs/adr-NNNN-<short-name>`
- コミットメッセージ:
  - 提案時: `docs(adr): propose ADR-NNNN <title>`
  - 採用時: `docs(adr): accept ADR-NNNN <title>`
  - 廃止時: `docs(adr): deprecate ADR-NNNN <title>`

### レビュー観点

ADR をレビューするときに見るべき点:

1. **target state のみが書かれているか**（マイグレーション手順が混入していないか）
2. **代替案の却下理由が明確か**（「なぜ X じゃないのか」が答えられるか）
3. **影響範囲が誠実に書かれているか**（Negative を書ききっているか）
4. **既存 ADR との整合性**（矛盾するなら旧 ADR の Status を更新するか）
5. **粒度は適切か**（複数の独立した決定が混ざっていないか）
