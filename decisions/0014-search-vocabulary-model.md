---
status: accepted
created: 2026-06-23
updated: 2026-06-23
---

# ADR-0014: 検索語彙リソースの管理モデル（判定 / synonym 軸分離 / 読み）

- **Deciders**: key
- **Related**:
  [ADR-0004](./0004-user-dictionary-design.md)（本 ADR が見直す候補ライフサイクルと reject のファイル管理）,
  [ADR-0006](./0006-cli-option-design.md)（CLI オプション設計の判断基準）,
  [ADR-0012](./0012-output-channel-model.md)（コマンド出力の取り回し）
- **Issues**: #239（`dict add` / `reject` / `rm`）, #240（`synonym` のトップレベル化）

## Context

tsm は検索語彙を 3 つのテキストファイルで管理する。

- `user_dict.simpledic` — lindera の利用者辞書。語を 1 トークンとして扱わせる。
- `reject_words.txt` — 候補の reject リスト。辞書化しない語。
- `synonyms.csv` — シノニムのペア。検索時のクエリ拡張に使う。

[ADR-0004](./0004-user-dictionary-design.md) が候補のライフサイクルと reject の
ファイル管理を定義したが、運用で複数の穴が露呈した。

- **1 語ずつ追加する手段がない**。`dict update --apply` は全候補を一括で扱い、読みも
  持てない。lindera が誤分割する複合語（`ハンドロード`→`ハンド`+`ロード`、
  `クラウド`→`クラ`+`ウド`）は単一の未知語として候補に現れないため、候補ベースの方式では
  直せない。唯一の手段である `user_dict.simpledic` の直接編集は、アプリ内部の書式
  （`surface,名詞,reading`）の知識を利用者・エージェントに強いる。
- **矛盾状態を防げない**。同じ語が `user_dict.simpledic` と `reject_words.txt` に
  同時に存在しうる。`add` と `reject` は反対の判定なので、これは矛盾である。
- **軸の混在**。synonym が `dict synonym` 配下にあり、トークン化と検索時クエリ拡張という
  別サブシステムが 1 つの名詞にぶら下がる。`dict add`（語を追加）と `dict synonym add`
  （関係を追加）で動詞 `add` が別物を指す。
- **読みの置き場がない**。simpledic は読みの欄を持つが、現状の検索は表層形のみを使い
  （トークナイザは表層形を返す）読みは未使用。将来の読みベース照合をどう収容するかが未定義。
- **表記ゆれ機構の重複**。同一語の別表記を解く手段が Unicode 正規化・読み照合・synonym で
  重なり、責務分担が未定義。

## Decision

### 1. 判定を状態機械にし 1 語ずつのコマンドで遷移させる

ある語の辞書上の状態を **accepted（辞書）・rejected・pending（どちらでもない）** の
3 状態とし、相互排他を不変条件とする。遷移は 1 語ずつのコマンドが担い、反対側から
自動で外す。

```text
tsm dict add    <surface> [<yomi>]   # -> accepted （reject から除去）
tsm dict reject <word>               # -> rejected （辞書から除去）
tsm dict rm     <word>               # -> pending  （在る方から除去）
```

1 回 1 語とする。`add` が位置引数 `<yomi>` を取るため、可変長の surface 列とは
両立しない。一括投入は別コマンド（後述の `export` / `import`）で扱う。

### 2. DB を権威とし、ファイルは export / import で往復する

判定状態は DB（`dictionary_candidates`。候補が生まれ、状態を持つ場所）を権威とする。
1 語ずつのコマンドは DB を更新する。

git 管理のテキストファイルは、DB との間を**明示的な方向のコマンドで往復**する
可搬・永続な記録とする。

```text
tsm dict export   # DB -> reject_words.txt / user_dict.simpledic
tsm dict import   # ファイル -> DB
```

- `export` は DB の判定をファイルへ書き出す（git でレビュー・共有・退避するため）。
- `import` はファイルを DB へ取り込む（別マシンからの取得、手編集の反映、rebuild で
  空になった DB の復旧）。
- 方向を明示するため、従来の曖昧な `sync` と `--apply` は廃止し、`export` / `import` に
  置き換える。往復で語がそのまま一致する。

[ADR-0004](./0004-user-dictionary-design.md) が「ファイルを正」とした狙い（git 管理・
rebuild 後の復旧）は、この往復で保つ。本 ADR は「ファイルが唯一の正」という位置づけを
「DB が権威・ファイルは可搬な記録」に改める。

実装上の注意: `user_dict.simpledic` は lindera が起動時に読む実行時入力でもある。
したがって判定変更は simpledic を再生成し、トークナイザの再読込（`restart`）を要する。
`reject_words.txt` と `synonyms.csv` は純粋に往復用。

### 3. synonym を別軸としてトップレベルに分離する

トークン化（index 経路）と検索時クエリ拡張（search 経路）は直交する軸である。
両者は共存しうる。ある語が辞書語であり同時にシノニムを持つこともあるし、ある語を
辞書候補としては reject しつつ synonym で代表表記へ寄せることもある（これは正当な
組み合わせ）。したがって synonym を判定の相互排他に含めず、独立したトップレベルコマンドに置く。

```text
tsm synonym add <a> <b>
tsm synonym rm  <a> [b]
tsm synonym export   # DB -> stdout（既定）。--file <path> でファイルへ
tsm synonym import   # stdin（既定）-> DB。--file <path> でファイルから
```

synonym は単一ストリーム（`synonyms.csv` 相当の 1 リソース）なので、export / import は
標準入出力を既定とし、出力先・入力元はユーザがリダイレクトや `--file <path>` で決める
（[ADR-0012](./0012-output-channel-model.md) の「ユーザ出力は stdout」と整合）。これは
2 ファイル（`reject_words.txt` と `user_dict.simpledic`）へ書き出す §2 の `dict export` が
ファイルベースを採るのと対照的で、リソース数の違いに由来する。補足として、stdout への
export 時は件数などの診断を stderr に出して CSV ストリームを汚さない。import は mirror
（入力に無い user ペアを削除）なので、対話 TTY からの読み取りは空入力での全削除事故を
防ぐためエラーとし、パイプか `--file` を要求する。

`dict` の名は「lindera 利用者辞書」として維持し、その旨をドキュメントで明示する。

### 4. 読みは位置引数の任意とする

`tsm dict add <surface> [<yomi>]`。1 語ずつの `add` だけが正しい読みを供給できる入口で
ある（一括候補の方式は構造的に読みを持てない）。

- 全カナの surface で `<yomi>` 省略時は読みを surface とする（正しい）。
- 漢字を含む surface で `<yomi>` 省略時は警告する（surface で代用した旨を出し、データ負債を
  可視化する）。処理は止めない。
- 自動の読み付与は採らない。追加対象は lindera が知らない語であり、自動付与が最も外す集合で
  ある。
- 読みは現状、格納のみで検索には未使用（検索は表層形を使う）。読みベース照合の導入時に意味を
  持つ。

### 5. 表記ゆれの責務を 3 機構で分担する

同一語の別表記を解く手段の責務を分離する。

| 機構 | 守備範囲 |
|---|---|
| Unicode 正規化（処理段） | 半角 / 全角・互換文字。macOS（APFS）由来の NFD（濁点・半濁点の分解。例: `か`+結合濁点 → `が`）を NFC へ合成する |
| 読み照合（将来） | 表記体系違い・同音 |
| synonym | 別語・略語・意味的等価 |

読みは音的正規化、synonym は意味的等価、と定義する。読みベース照合を実装する前に、この
分担を前提とする。

#### Unicode 正規化を必須とする理由

macOS のファイルシステムは名前・テキストを NFD（分解形）で持ちがちである。NFD の `が`
（`か` + 結合濁点 U+3099）は NFC の `が`（U+304C 単一）と見た目が同一だがバイト列が異なり、
FTS5 で一致しない。よって索引前・クエリ前に NFC への正準合成（濁点・半濁点の結合を含む）を
行う。互換文字（半角 / 全角等）は NFKC で吸収する。

## Rationale

- **synonym を dict 配下に残さない理由**: 動詞の意味が重なる（`dict add` と
  `dict synonym add`）うえ、名詞の下に別の名詞がぶら下がる。tsm のトップレベルは
  サブシステム単位で切られており（[ADR-0006](./0006-cli-option-design.md)）、別軸は
  別トップレベルが整合する。
- **DB を権威にする理由**: 候補は DB で生まれ状態を持つ。1 語ずつのコマンドを入れると、
  ファイルを正と言い張る限りコマンドは毎回ファイルの読み直し・追記・重複排除・書き戻しを
  強いられ、実体と乖離する。権威を DB に置き、ファイルは明示的な往復にすると矛盾が消える。
- **`sync` を廃し `export` / `import` にする理由**: `sync` は方向が曖昧。明示的な 2 コマンドに
  すると往復が一意になり、語がそのまま一致する。
- **自動の読み付与を採らない理由**: 追加対象は未知語であり、自動の読みが最も信頼できない集合。
- **読みを今から任意で受ける理由**: 1 語ずつの `add` が唯一の正しい読みの入口であり、欄を
  今開けておけば、読み照合の導入時に全エントリを後から埋め直す移行を避けられる。コストは
  位置引数 1 個で安い。

### 却下した代替案

- **傘コマンド `tsm lexicon {dict,synonym}`**: 名詞のネストが戻り、`lexicon` が抽象的すぎる。
  素直に 2 名詞を並べる方がよい。
- **`dict` の改名（`userdict` / `token`）**: 概念は明確になるが
  [ADR-0004](./0004-user-dictionary-design.md) の既定名を壊す価値はない。ドキュメントで
  定義を締める方が安い。

## Consequences

### Positive

- 誤分割の複合語を正規の手段で追加でき、simpledic の直接編集と内部書式の露出が解消する。
- 辞書と reject の矛盾状態をコマンドが構造的に排除する。
- CLI が判定軸（dict）と関係軸（synonym）で素直に分離し、動詞が一貫する（`add` / `rm`）。
- `export` / `import` で DB とファイルの往復が一意になり、git 運用と rebuild 復旧が明確になる。
- 読みの置き場が定義され、読みベース照合へ前方互換になる。
- 表記ゆれ 3 機構の責務が明確になり、重複実装を予防する。NFD 由来の不一致を正規化で解消する。

### Negative

- 既存の `tsm dict synonym sync` / `dict update --apply` / `dict reject --apply` は廃止・置換に
  なる破壊的変更（コマンド引数の整理）。本 ADR はこれを許容する。
- トップレベルコマンドが 1 つ増える（synonym）。
- 読みを任意で受けても、検索が未使用の間は検証されない格納データになる（読み照合の実装時に
  再検証を前提とする）。
- 判定変更のたびに simpledic 再生成とトークナイザ再読込が要る（lindera の実行時入力のため）。
