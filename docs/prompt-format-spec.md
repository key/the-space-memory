# プロンプトフォーマット仕様

Claude Code プラグインの UserPromptSubmit フックが出力する
ナレッジ検索結果の XML フォーマット仕様。

関連: [ADR-0011](https://github.com/KenosInc/company/blob/main/decisions/0011-tsm-output-layer-separation.md)、[Issue #128](https://github.com/key/the-space-memory/issues/128)

## 設計方針

- [Anthropic プロンプティングベストプラクティス][prompting]の XML 構造化に準拠する
- snake_case タグ名（公式パターン: `document_content`, `frontend_aesthetics` 等）
- 公式の `<documents>` → `<document index="n">` パターンを踏襲する
- 注入はトリガー（きっかけ）として機能する。
  詳細が必要なら Claude が `tsm search` スキルや `Read` で深掘りする
- snippet は「関連ありそう」と判断できる程度に短くてよい

[prompting]: https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices

## 出力フォーマット

### 通常（結果あり）

```xml
<knowledge_search query="LoRa モジュール" count="5" total="12">
<result index="1" score="0.018">
<source type="daily">daily/daily/research/vhf-tracker-radio-options.md</source>
<section>VHFドッグトラッカー 無線方式調査シート > 比較マトリクス</section>
<snippet>
| 項目 | T99 (150MHz) | 429MHz LoRa | ...
</snippet>
<related>daily/daily/intel/2026-02-24.md, daily/daily/intel/2026-03-15.md</related>
</result>
<result index="2" score="0.016">
<source type="knowledge" status="current">company/knowledge/lora/dog-tracker-lora-competitors.md</source>
<section>LoRaベース ドッグトラッカー 競合調査 > keyさんのVHFドッグトラッカーとの比較</section>
<snippet>
| 項目 | LoRa製品 | LTE-M製品 | keyさんのVHF |
</snippet>
</result>
</knowledge_search>
```

### 結果なし

```xml
<knowledge_search query="存在しないトピック" count="0" total="0"/>
```

### トークン予算超過時（下位の snippet を省略）

```xml
<knowledge_search query="LoRa" count="5" total="20">
<result index="1" score="0.025">
<source type="knowledge" status="current">company/knowledge/lora/lora-module-guide.md</source>
<section>LoRa通信モジュール選定ガイド > 概要</section>
<snippet>
LoRa通信モジュールの選定基準と各製品の比較...
</snippet>
</result>
<!-- ... 中間の result ... -->
<result index="5" score="0.008">
<source type="daily">daily/daily/intel/2026-03-09.md</source>
<section>情報収集ログ > LoRa関連</section>
<snippet/>
</result>
</knowledge_search>
```

## 公式パターンとの対応

| 公式パターン | 本仕様 | 対応 |
|---|---|---|
| `<documents>` | `<knowledge_search>` | コンテナ要素 |
| `<document index="n">` | `<result index="n">` | アイテム（index 属性） |
| `<source>` | `<source type="..." status="...">` | ファイルパス + メタデータ |
| `<document_content>` | `<snippet>` | 内容プレビュー |
| snake_case タグ名 | snake_case タグ名 | 命名規則統一 |

## フィールド設計

### コンテナ属性（`<knowledge_search>`）

| 属性 | 説明 |
|---|---|
| `query` | 検索クエリ文字列 |
| `count` | 表示件数 |
| `total` | 全ヒット数（トークン予算で絞った透明性を確保） |

### アイテム属性（`<result>`）

| 属性 | 説明 |
|---|---|
| `index` | 順位（1-based、公式 `<document index>` に準拠） |
| `score` | RRF スコア（関連度の判断材料） |

### ソース属性（`<source>`）

| 属性 | 説明 | 省略条件 |
|---|---|---|
| `type` | daily / knowledge / session 等 | 常に出力 |
| `status` | current / draft 等 | null なら省略 |

### 子要素

| 要素 | 説明 | 省略条件 |
|---|---|---|
| `<source>` | ファイルパス（Read で直接読める導線、最重要） | 常に出力 |
| `<section>` | セクションパス | 常に出力 |
| `<snippet>` | 短いプレビュー（判断材料） | 予算超過時は `<snippet/>` |
| `<related>` | 関連ファイルのカンマ区切り | 0 件なら省略 |

## トークン予算

snippet 合計の上限は環境変数 `TSM_SNIPPET_BUDGET` で制御する。

```bash
TSM_SNIPPET_BUDGET=1000  # デフォルト: 1000 文字
```

- search.sh 内で `${TSM_SNIPPET_BUDGET:-1000}` として参照する
- 上限超過時は下位の result から snippet を省略し `<snippet/>` とする
- プラグイン本体に手を入れずに、ユーザーごとの調整やモデルごとの最適化が可能

## tsm CLI との責務分離

tsm CLI は人間向けテキストと構造化 JSON の出力に責任を持つ（ADR-0011）。
本仕様のフォーマット変換は Claude Code プラグイン側（`hooks/scripts/search.sh`）で行う。

- tsm に `--format claude` のような LLM 固有オプションは追加しない
- search.sh が `tsm search --format json` の出力を受け取り、本仕様の XML に変換する
- tsm の JSON スキーマ変更時はプラグイン側も追従が必要
