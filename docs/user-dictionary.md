# ユーザー辞書

## デザインコンセプト

tsm の全文検索（FTS5）は lindera による形態素解析で日本語を分かち書きする。
lindera の内蔵辞書（IPAdic）は一般的な日本語をカバーするが、
技術用語・固有名詞・プロジェクト固有の語彙は未登録のため、
検索でヒットしなかったり、誤った位置で分割されたりする。

ユーザー辞書はこの問題を補う仕組みで、
**辞書の役割は「lindera が正しくトークン分割する」こと**に限定される。
表記ゆれの吸収（`ローラ` → `LoRa` など）はシノニム展開やベクトル検索の役割であり、
辞書では扱わない（読みフィールドからのシノニム自動生成は #115 で検討中）。

以下の設計方針をとっている。

- **収集は自動、適用は明示的** ---
  インデックス・検索・セッション取り込み時に未知語を自動収集するが、
  辞書への追加は `tsm dict update` コマンドで人間が確認してから行う
- **reject で品質を担保** ---
  候補テーブルに溜まった不要語（ストップワード等）は `rejected` にマークすることで、
  以後カウントされなくなる。`reject_words.txt` で一括管理できる
- **辞書変更は FTS 再構築とセット** ---
  FTS5 トークナイザーが辞書を読むため、
  辞書を変更したら FTS インデックスを再構築する必要がある。
  `tsm dict update --apply` は自動で FTS 再構築を行う

## ファイル構成

```text
.tsm/
├── tsm.db                    # dictionary_candidates テーブル（候補の蓄積・ステータス管理）
├── user_dict.simpledic       # lindera に読ませる辞書ファイル（simpledic 形式）
└── reject_words.txt          # リジェクト対象の単語リスト（1行1単語、# でコメント）
```

### user_dict.simpledic

lindera が形態素解析時に参照する辞書ファイル。simpledic 形式（3フィールド、カンマ区切り）。

```csv
tsmd,名詞,tsmd
LoRa,名詞,LoRa
ドッグトラッカー,名詞,ドッグトラッカー
```

各フィールド: `表層形,品詞,読み`

- 品詞は `名詞`（`user_dict::USER_DICT_POS` 定数で定義）
- 表層形は元のケースを保持する（`LoRa` は `LoRa` のまま登録）
- lindera の Aho-Corasick マッチは大文字小文字を区別するため、
  表層形のケースが原文と一致しないとマッチしない
- `#` で始まる行はコメント、空行は無視される（tsm がロード時に除去してから lindera に渡す）
- 読みフィールドは lindera の内部データとして保持されるが、
  現在 FTS 検索には使用されない（将来的にシノニム生成に活用予定: #115）

### reject_words.txt

辞書に登録したくない単語のリスト。`tsm dict reject --apply` でDBに反映する。
`rebuild --force` でDB がリセットされても、このファイルから再適用できる。

```text
# English stop words
the
and
to
in

# Generic terms
status
process
```

- 1行1単語、`#` でコメント、空行は無視
- `tsm dict reject --apply` で pending 候補を `rejected` にマーク

### dictionary_candidates テーブル（tsm.db 内）

候補の蓄積とステータス管理を行う DB テーブル。

| カラム | 型 | 説明 |
|---|---|---|
| surface | TEXT (PK) | 単語の表層形（小文字正規化済み） |
| frequency | INTEGER | 出現回数 |
| pos | TEXT | 品詞推定（`proper_noun` / `katakana` / `ascii`） |
| source | TEXT | 収集元（`document` / `query` / `session`） |
| first_seen | TEXT | 初出日時（RFC3339） |
| last_seen | TEXT | 最終出現日時（RFC3339） |
| status | TEXT | `pending` / `accepted` / `rejected` |

ステータスの意味:

- **pending** --- 候補として蓄積中。出現するたびに frequency が加算される
- **accepted** --- `tsm dict update --apply` で辞書ファイルに追加済み
- **rejected** --- 不要と判断された語。frequency の加算がスキップされる

## 辞書操作ガイド

### 初回セットアップ

```bash
# 1. インデックスして候補を蓄積
tsm index

# 2. 候補の確認（ドライラン）
tsm dict update

# 3. reject_words.txt を作成してストップワードを除外
vi .tsm/reject_words.txt

# 4. リジェクトを適用
tsm dict reject --apply

# 5. 残った候補を確認して辞書に追加
tsm dict update          # ドライラン — 候補を確認
tsm stop                 # デーモン停止が必要
tsm dict update --apply  # 辞書追加 + FTS 再構築
tsm start
```

### 定期メンテナンス

`tsm doctor` が `N candidates ready` と報告したら候補が溜まっている。

```bash
# 1. 候補を確認
tsm dict update

# 2. 不要な語があれば reject_words.txt に追記して適用
echo "newstopword" >> .tsm/reject_words.txt
tsm dict reject --apply

# 3. 辞書に追加
tsm stop
tsm dict update --apply
tsm start
```

### rebuild 後のリジェクト再適用

`tsm rebuild --force` は DB を削除・再作成するため、
`dictionary_candidates` テーブルの rejected ステータスが失われる。
`reject_words.txt` から再適用する:

```bash
tsm dict reject --apply
```

### 現在の状態を確認する

```bash
# doctor で辞書候補の概要を確認
tsm doctor

# 辞書の単語数
wc -l .tsm/user_dict.simpledic

# リジェクト済み単語の一覧
tsm dict reject --all
```

## 候補収集の仕組み

### 収集元

| 収集タイミング | source 値 | 説明 |
|---|---|---|
| `tsm index` | `document` | ドキュメントのインデックス時 |
| `tsm search` | `query` | 検索クエリの処理時 |
| `tsm ingest-session` | `session` | セッション取り込み時 |

### 収集される単語の種類

| 種類 | 条件 | 例 |
|---|---|---|
| 固有名詞 | IPADIC が `名詞-固有名詞` と判定 | 東京、田中 |
| カタカナ語 | 全文字カタカナ、2文字以上 | リンデラ、プロセス |
| ASCII 語 | 英数字+記号、2文字以上、英字を含む | tsm, embedder, LoRa |

**注意**: 英語のストップワード（the, and, is...）も ASCII 語として収集されるため、
`reject_words.txt` での除外が重要。

### FTS と品詞の関係

FTS5 インデックスは品詞を理解しない。`wakachi()` が全トークンの表層形を
スペース区切りで FTS5 に登録する（助詞・動詞も含む）。
品詞によるフィルタリングは検索クエリ側（`extract_search_keywords`）でのみ行われ、
名詞以外のトークンはクエリキーワードから除外される。

ユーザー辞書の品詞を `名詞` に統一しているのはこのためで、
名詞フィルタを自然に通過させるための設計判断である。

### 候補収集の流れ

1. `indexer::index_file()` がドキュメントをインデックスする際、テキストを `user_dict::collect_from_text()` に渡す
2. lindera で形態素解析し、固有名詞・カタカナ語・ASCII 用語を候補として抽出
3. 既に `user_dict.simpledic` に存在する語はスキップ
4. 1文字・数字のみ・記号のみの語もスキップ
5. `dictionary_candidates` テーブルに UPSERT（既存なら frequency を +1、rejected なら加算しない）

## 実装ファイル

| ファイル | 役割 |
|---|---|
| `src/user_dict.rs` | 候補収集（`collect_from_text`）、辞書エクスポート、accept/reject 操作、`USER_DICT_POS` 定数 |
| `src/tokenizer.rs` | lindera の初期化。`user_dict.simpledic` を `load_user_dictionary_from_csv()` で読み込み、`Segmenter` に適用 |
| `src/cli.rs` | `tsm dict update` / `tsm dict reject` コマンドの実装 |
| `src/db.rs` | `dictionary_candidates` テーブルのスキーマ定義 |
