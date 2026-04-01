# ADR-0004: ユーザー辞書の設計方針

- **Status**: **Accepted**
- **Date**: 2026-04-01
- **Deciders**: key
- **Related**:
  [Issue #59](https://github.com/key/the-space-memory/issues/59),
  [PR #63](https://github.com/key/the-space-memory/pull/63)

## Context

tsm の全文検索（FTS5）は lindera による形態素解析で日本語を分かち書きする。
lindera の内蔵辞書（IPAdic）は一般的な日本語をカバーするが、
技術用語・固有名詞・プロジェクト固有の語彙は未登録のため、
検索でヒットしなかったり、誤った位置で分割されたりする。

ユーザー辞書はこの問題を補う仕組みだが、以下の設計判断が必要だった。

1. 候補の収集タイミングと辞書への適用タイミングをどう分離するか
2. 不要な候補（ストップワード等）の管理方法
3. 辞書変更後の再構築コスト

## Decision

### 方針 1: 収集は自動、適用は明示的

インデックス・検索・セッション取り込み時に未知語を `dictionary_candidates` テーブルへ自動収集する。
ただし辞書への追加は `tsm dict update --apply` で人間が確認してから行う。

**理由**: 自動収集した語には、コード片（`cfg`, `mut`）、URL の一部（`https`, `www`）、
セッション固有の文字列など、辞書に入れるべきでないものが大量に含まれる。
自動で辞書に入れると FTS の精度が下がる。

### 方針 2: reject リストはテキストファイルで管理

不要な候補は `.tsm/reject_words.txt` で管理し、`tsm dict reject --apply` で DB に同期する。

**理由**:

- DB 内の `status = 'rejected'` は `rebuild --force` で消失する
- テキストファイルなら git 管理可能、エディタで編集可能、DB 再作成後も復元可能
- `reject_words.txt` が真実の源泉、DB は同期先という関係

### 方針 3: 辞書変更時は FTS のみ再構築

辞書変更後は `rebuild --fts-only` で FTS5 テーブルのみ再構築する（`rebuild --force` は不要）。

**理由**: ベクトル埋め込みは元テキストから生成するため辞書に依存しない。
7000 チャンクのベクトル再計算に数分かかるが、FTS の再構築だけなら 1 秒で済む。

### CLI 設計

```bash
tsm dict update             # 候補一覧（ドライラン）
tsm dict update --apply     # 辞書に追加 + FTS rebuild + git commit & PR 作成
tsm dict reject             # reject 候補一覧
tsm dict reject --apply     # reject_words.txt → DB 同期
tsm dict reject --all       # rejected 全件表示
```

- `--apply` なし = 見るだけ、`--apply` あり = 実行
- `dict update` と `dict reject` で対称的な構造
- `dict reject` はデーモン稼働中でも実行可能（FTS インデックスを変更せず rebuild 不要。rejected 候補は辞書ファイルに存在しないため検索精度にも影響しない）
- `dict update --apply` はデーモン停止が必要（rebuild のため）

### データ配置

| パス | 内容 |
|---|---|
| `.tsm/tsm.db` | `dictionary_candidates` テーブル（候補の蓄積・ステータス管理） |
| `.tsm/user_dict.simpledic` | lindera に読ませる辞書ファイル（`TSM_USER_DICT` で変更可） |
| `.tsm/reject_words.txt` | reject リスト（1行1語、`#` コメント対応） |

### 候補のライフサイクル

```text
テキスト（インデックス・セッション）  クエリ（検索）
  │ collect_from_text()              │ collect_from_query()
  └──────────────┬───────────────────┘
                 ▼
dictionary_candidates (status = 'pending')
  │
  ├─→ tsm dict update --apply → user_dict.simpledic (status = 'accepted')
  │
  └─→ tsm dict reject --apply → status = 'rejected'（frequency 加算停止）
```

## Consequences

- 辞書更新の心理的ハードルが下がる（FTS のみ再構築で 1 秒）
- reject リストが git 管理可能になり、チーム間で共有・レビュー可能
- `rebuild --force` 後も `dict reject --apply` で reject 状態を復元可能
- 候補の品質は人間の判断に依存する（自動フィルタリングは Issue #10 で検討中）
