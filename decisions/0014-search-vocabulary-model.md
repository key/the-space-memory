---
status: proposed
created: 2026-06-23
updated: 2026-06-23
---

# ADR-0014: 検索語彙リソースの管理モデル（dict 判定 / synonym 軸分離 / reading）

- **Deciders**: key
- **Related**:
  [ADR-0004](./0004-user-dictionary-design.md)（本 ADR が拡張する候補ライフサイクルと reject の file 管理）,
  [ADR-0006](./0006-cli-option-design.md)（CLI オプション設計の判断基準）,
  [ADR-0012](./0012-output-channel-model.md)（コマンド出力の pipe 可能性）
- **Issues**: #239（`dict add` / `reject` / `rm`）, #240（`synonym` の top-level 化）

## Context

tsm は検索語彙を 3 つのテキストファイルで管理する。

- `user_dict.simpledic` — tokenizer（lindera）の user 辞書。語を 1 トークンとして扱わせる。
- `reject_words.txt` — 候補 reject リスト。辞書化しない語。
- `synonyms.csv` — シノニムのペア。検索時のクエリ拡張に使う。

[ADR-0004](./0004-user-dictionary-design.md) が候補ライフサイクルと reject の file 管理を
定義したが、運用で複数の穴が露呈した。

- **per-word 追加手段がない**。`dict update --apply` は全候補一括で、読みも持てない。
  lindera が誤分割する複合語（`ハンドロード`→`ハンド`+`ロード`、`クラウド`→`クラ`+`ウド`）は
  単一の未知語として候補に現れないため、候補ベースの flow では直せない。唯一の手段である
  `user_dict.simpledic` 直編集は、アプリ内部 format（`surface,名詞,reading`）の知識を
  ユーザー / エージェントに強いる。
- **矛盾状態を防げない**。同じ語が `user_dict.simpledic` と `reject_words.txt` に
  同時に存在しうる。add と reject は反対の判定なので、これは矛盾である。
- **軸の混在**。synonym が `dict synonym` 配下にあり、tokenization（index 経路）と
  query expansion（search 経路）という別サブシステムが 1 つの名詞にぶら下がる。
  `dict add`（token を追加）と `dict synonym add`（関係を追加）で動詞 `add` が
  別物を指す（verb overload）。
- **reading の置き場がない**。simpledic は reading フィールドを持つが、現状 FTS は
  surface 専用（tokenizer は `token.surface` を返す）で reading は未使用。将来の
  読みベース照合をどう収容するかが未定義。
- **表記ゆれ機構の重複**。「同一語の別表記」を解く手段が NFKC 正規化・reading 照合・
  synonym で重なり、責務分担が未定義。

## Decision

### 1. 判定を状態機械にし per-word verb で遷移させる

ある語の辞書上の状態を **accepted（`user_dict.simpledic`）⊕ rejected
（`reject_words.txt`）⊕ pending（どちらにも無い）** の 3 状態とし、相互排他を
不変条件とする。遷移は per-word のコマンドが担い、反対側のリストから自動で外す。

```text
tsm dict add    <surface> [<yomi>]   # -> accepted （reject から除去）
tsm dict reject <word>               # -> rejected （user_dict から除去）
tsm dict rm     <word>               # -> pending  （在る方から除去）
```

1 回 1 語とする。`add` が位置引数 `<yomi>` を取るため、可変長の surface 列とは
両立しない。bulk 投入は対象外（将来 `dict import` で扱う）。

### 2. テキストファイルを source of truth とする

3 ファイルが正であり、コマンドは「ファイルを編集して DB へ同期する」薄いラッパに
徹する（[ADR-0004](./0004-user-dictionary-design.md) の git 管理・レビュー可能性・
DB 再作成後の復元可能性を維持）。

### 3. synonym を別軸として top-level に分離する

tokenization（index 経路）と query expansion（search 経路）は直交する軸である。
両者は共存しうる（ある語が辞書語かつシノニムを持つ／ある語を辞書候補としては reject
しつつ synonym で canonical に寄せる、は正当な組み合わせ）。したがって synonym を
判定の相互排他に含めず、独立した top-level コマンドに置く。

```text
tsm synonym add <a> <b>
tsm synonym rm  <a> [b]
tsm synonym sync
```

`dict` の名は「tokenizer user dictionary」として維持し、その旨を doc で明示する。

### 4. reading は位置引数の任意とする

`tsm dict add <surface> [<yomi>]`。per-word の add だけが正しい読みを供給できる
入口である（bulk 候補 flow は構造的に読みを持てない）。

- 全カナ surface で `<yomi>` 省略時は `reading = surface`（正しい）。
- 漢字を含む surface で `<yomi>` 省略時は **warn**（surface で代用した旨を出し、
  データ負債を可視化）。ハードブロックはしない。
- **auto-yomi は採らない**。追加対象は lindera が知らない語＝自動読み付与が最も
  外す集合である。
- reading は現状 **格納のみ・FTS 未使用**。読みベース照合の導入時に意味を持つ。

### 5. 表記ゆれの責務を 3 機構で分担する

「同一語の別表記」を解く手段の責務を分離する。

| 機構 | 守備範囲 |
|---|---|
| NFKC 正規化（pipeline） | 半角 / 全角・互換文字 |
| reading 照合（将来） | 表記体系違い・同音 |
| synonym | 別語・略語・意味的等価 |

**reading = 音的正規化、synonym = 意味的等価** と定義する。読みベース照合を実装する
前に、この分担を前提とする。

## Rationale

- **synonym を dict 配下に残さない理由**: verb overload（`dict add` と
  `dict synonym add`）と mixed nesting（名詞の下に別の名詞）。tsm の top-level は
  サブシステム単位で切られており（[ADR-0006](./0006-cli-option-design.md)）、別軸は
  別 top-level が整合する。
- **DB でなく file を正とする理由**: [ADR-0004](./0004-user-dictionary-design.md) の
  git 管理・rebuild 後の復元可能性。
- **auto-yomi を採らない理由**: 追加対象は未知語であり、自動読みが最も信頼できない集合。
- **reading を今から任意で受ける理由**: per-word add が唯一の正しい読みの入口であり、
  フィールドを今開けておけば、読み照合導入時に全エントリを backfill する移行を避けられる。
  コストは位置引数 1 個で安い。

### 却下した代替案

- **傘コマンド `tsm lexicon {dict,synonym}`**: ネストが戻り、`lexicon` が抽象的すぎる。
  flat に 2 名詞の方が素直。
- **`dict` を `userdict` / `token` に改名**: 概念は明確になるが
  [ADR-0004](./0004-user-dictionary-design.md) の既定名を壊す価値はない。doc で
  定義を締める方が安い。

## Consequences

### Positive

- 誤分割複合語を正規手段で追加でき、simpledic 直編集と内部 format の露出が解消する。
- dict ↔ reject の矛盾状態をコマンドが構造的に排除する。
- CLI が「判定軸（dict）」と「関係軸（synonym）」で素直に分離し、動詞が一貫する（add / rm）。
- reading の置き場が定義され、読みベース照合へ前方互換になる。
- 表記ゆれ 3 機構の責務が明確になり、重複実装を予防する。

### Negative

- 既存 `tsm dict synonym sync` は破壊的変更になる（top-level 移行）。alias 維持か破棄かの
  移行判断が要る（target state の対象外。実装側で扱う）。
- top-level コマンドが 1 つ増える（synonym）。
- reading を任意で受けても、FTS が未使用の間は検証されない格納データになる（読み照合
  実装時の再検証を前提とする）。
- bulk 追加は当面コマンド化されない（1 回 1 語。将来 `dict import`）。
