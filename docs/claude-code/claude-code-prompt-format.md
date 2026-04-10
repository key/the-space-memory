# Prompt Format Specification

XML format specification for knowledge search results injected by the
Claude Code plugin's UserPromptSubmit hook.

Related: [ADR-0011](https://github.com/KenosInc/company/blob/main/decisions/0011-tsm-output-layer-separation.md), [Issue #128](https://github.com/key/the-space-memory/issues/128)

## Design Principles

- Follow [Anthropic Prompting Best Practices][prompting] XML structuring
- Use snake_case tag names (matching official patterns: `document_content`, `frontend_aesthetics`, etc.)
- Follow the official `<documents>` → `<document index="n">` pattern
- Injection acts as a **trigger** — Claude digs deeper via `tsm search` skill or `Read` when needed
- Snippets should be just enough to judge relevance

[prompting]: https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices

## Output Format

### Normal (with results)

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

### No results

```xml
<knowledge_search query="nonexistent topic" count="0" total="0"/>
```

### Token budget exceeded (lower-ranked snippets omitted)

```xml
<knowledge_search query="LoRa" count="5" total="20">
<result index="1" score="0.025">
<source type="knowledge" status="current">company/knowledge/lora/lora-module-guide.md</source>
<section>LoRa通信モジュール選定ガイド > 概要</section>
<snippet>
LoRa通信モジュールの選定基準と各製品の比較...
</snippet>
</result>
<!-- ... intermediate results ... -->
<result index="5" score="0.008">
<source type="daily">daily/daily/intel/2026-03-09.md</source>
<section>情報収集ログ > LoRa関連</section>
<snippet/>
</result>
</knowledge_search>
```

## Mapping to Official Patterns

| Official pattern | This spec | Role |
|---|---|---|
| `<documents>` | `<knowledge_search>` | Container element |
| `<document index="n">` | `<result index="n">` | Item (index attribute) |
| `<source>` | `<source type="..." status="...">` | File path + metadata |
| `<document_content>` | `<snippet>` | Content preview |
| snake_case tag names | snake_case tag names | Naming convention |

## Field Design

### Container attributes (`<knowledge_search>`)

| Attribute | Description |
|---|---|
| `query` | Search query string |
| `count` | Number of results displayed |
| `total` | Total hit count (transparency for token budget trimming) |

### Item attributes (`<result>`)

| Attribute | Description |
|---|---|
| `index` | Rank (1-based, follows official `<document index>`) |
| `score` | RRF score (relevance indicator) |

### Source attributes (`<source>`)

| Attribute | Description | Omission rule |
|---|---|---|
| `type` | daily / knowledge / session, etc. | Always present |
| `status` | current / draft, etc. | Omitted when null |

### Child elements

| Element | Description | Omission rule |
|---|---|---|
| `<source>` | File path (primary entry point for `Read`) | Always present |
| `<section>` | Section path | Always present |
| `<snippet>` | Short preview for relevance judgment | `<snippet/>` when budget exceeded |
| `<related>` | Comma-separated related file paths | Omitted when empty |

## Token Budget

The total snippet budget is controlled by the `TSM_SNIPPET_BUDGET` environment variable.

```bash
TSM_SNIPPET_BUDGET=1000  # Default: 1000 characters
```

- Referenced as `${TSM_SNIPPET_BUDGET:-1000}` in search.sh
- When exceeded, lower-ranked results get `<snippet/>` (self-closing, no content)
- Allows per-user or per-model tuning without modifying the plugin itself

## Separation of Concerns with tsm CLI

tsm CLI is responsible for human-readable text and structured JSON output (ADR-0011).
The format conversion defined here is handled by the Claude Code plugin (`hooks/scripts/search.sh`).

- No LLM-specific options (e.g., `--format claude`) will be added to tsm
- search.sh receives `tsm search --format json` output and converts it to this XML format
- Plugin must track tsm JSON schema changes
