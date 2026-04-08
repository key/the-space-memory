# ADR-0006: CLI オプション設計の判断基準

- **Status**: **Accepted（確定）**
- **Date**: 2026-04-08
- **Deciders**: key
- **Related**:
  [Issue #114](https://github.com/key/the-space-memory/issues/114)

## Context

`tsm rebuild` に `--force`, `--fts-only` などフラグが増え、
用途が重複・混乱していた。
`tsm reindex` の新設にあたり、サブコマンドとフラグの使い分けを明文化する。

## Decision

### サブコマンド = "what"（操作対象が質的に異なる場合）

操作対象やモードが質的に異なるときはサブコマンドにする。

```bash
tsm reindex all       # FTS + vectors
tsm reindex fts       # FTS のみ
tsm reindex vectors   # vectors のみ
```

`fts` と `vectors` は処理パスもデータ構造も異なるため、
フラグ（`--fts-only`）ではなくサブコマンドで表現する。

### フラグ = "whether / how"（確認ゲート、パラメータ調整）

同一操作の実行制御やパラメータ調整にはフラグを使う。

```bash
tsm rebuild           # dry run（サマリー表示のみ）
tsm rebuild --apply   # 実際に実行
```

`--apply` は「実行するかどうか」の確認ゲートであり、
操作内容は変わらない。

## Consequences

- `--force` は廃止。確認ゲートは `--apply` に統一
- `--fts-only` は廃止。`tsm reindex fts` に移行
- 今後のコマンド追加時もこの基準に従う
