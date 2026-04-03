# ユーザー辞書

## デザインコンセプト

tsm の全文検索（FTS5）は lindera による形態素解析で日本語を分かち書きする。
lindera の内蔵辞書（IPAdic）は一般的な日本語をカバーするが、
技術用語・固有名詞・プロジェクト固有の語彙は未登録のため、
検索でヒットしなかったり、誤った位置で分割されたりする。

ユーザー辞書はこの問題を補う仕組みで、以下の設計方針をとっている。

- **収集は自動、適用は明示的** ---
  インデックス・検索・セッション取り込み時に未知語を自動収集するが、
  辞書への追加は `tsm dict update` コマンドで人間が確認してから行う
- **reject で品質を担保** ---
  候補テーブルに溜まった不要語（ストップワード等）は `rejected` にマークすることで、
  以後カウントされなくなる。辞書ファイル には影響しない
- **辞書変更は rebuild とセット** ---
  FTS5 トークナイザーが辞書を読むため、
  辞書を変更したら `tsm rebuild --force` で FTS インデックスを再構築する必要がある

## ファイル構成

```text
.tsm/
├── tsm.db                  # dictionary_candidates テーブル（候補の蓄積・ステータス管理）
└── user_dict.simpledic           # lindera に読ませる辞書ファイル（simpledic 形式）
```

### user_dict.simpledic

lindera が形態素解析時に参照する辞書ファイル。simpledic 形式（11フィールド、カンマ区切り）。

```csv
tsmd,0,0,0,カスタム名詞,*,*,*,tsmd,tsmd,tsmd
lora,0,0,0,カスタム名詞,*,*,*,lora,lora,lora
ドッグトラッカー,0,0,0,カスタム名詞,*,*,*,ドッグトラッカー,ドッグトラッカー,ドッグトラッカー
```

各フィールド: `表層形,左文脈ID,右文脈ID,コスト,品詞,品詞細分類1,品詞細分類2,品詞細分類3,活用型,活用形,原形`

tsm では全エントリを `カスタム名詞` として登録し、コスト 0 で最優先マッチさせる。

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
- **accepted** --- `tsm dict update` で辞書ファイル に追加済み
- **rejected** --- 不要と判断された語。frequency の加算がスキップされる

## 実装ファイル

| ファイル | 役割 |
|---|---|
| `src/user_dict.rs` | 候補収集（`collect_from_text`）、辞書エクスポート、accept/reject 操作 |
| `src/tokenizer.rs` | lindera の初期化。`user_dict.simpledic` を `load_user_dictionary_from_csv()` で読み込み、`Segmenter` に適用 |
| `src/cli.rs` | `tsm dict update` コマンドの実装（`cmd_dict_update`） |
| `src/db.rs` | `dictionary_candidates` テーブルのスキーマ定義 |

### 候補収集の流れ

1. `indexer::index_file()` がドキュメントをインデックスする際、テキストを `user_dict::collect_from_text()` に渡す
2. lindera で形態素解析し、固有名詞・カタカナ語・ASCII 用語を候補として抽出
3. 既に `user_dict.simpledic` に存在する語はスキップ
4. 1文字・数字のみ・記号のみの語もスキップ
5. `dictionary_candidates` テーブルに UPSERT（既存なら frequency を +1、rejected なら加算しない）

## 管理方法

### 辞書に候補を追加する

```bash
# 1. デーモンを停止（tsm dict update は DB を直接操作するため）
tsm stop

# 2. 閾値（5回）以上の候補を辞書ファイル に追加
tsm dict update --apply

# 3. FTS インデックスを再構築（辞書変更を反映）
tsm rebuild --force
```

`tsm dict update` は以下を行う:

- frequency >= 5 かつ status = pending の候補を取得
- 既に CSV にある語は `accepted` にマークしてスキップ
- 新規の語を `user_dict.simpledic` に追記し、`accepted` にマーク

### 候補を reject する

不要な候補は rejected にすることで、doctor の報告から消え、今後 frequency も加算されなくなる。

```bash
# 現時点では CLI コマンドがないため、SQL で直接操作する
sqlite3 .tsm/tsm.db "UPDATE dictionary_candidates SET status = 'rejected' WHERE surface = 'ストップワード'"
```

### 辞書から単語を手動削除する

`user_dict.simpledic` をテキストエディタで編集し、不要な行を削除する。
変更後は `tsm rebuild --force` が必要。

### 現在の状態を確認する

```bash
# doctor で辞書候補の概要を確認
tsm doctor

# 候補テーブルの統計
sqlite3 .tsm/tsm.db "SELECT status, COUNT(*) FROM dictionary_candidates GROUP BY status"

# 閾値以上の pending 候補を表示
sqlite3 .tsm/tsm.db \
  "SELECT surface, frequency, pos FROM dictionary_candidates \
   WHERE status = 'pending' AND frequency >= 5 \
   ORDER BY frequency DESC LIMIT 20"

# 辞書の単語数
wc -l .tsm/user_dict.simpledic
```
