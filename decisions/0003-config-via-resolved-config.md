# ADR-0003: 設定値は ResolvedConfig シングルトンで管理する

- **Status**: **Accepted（確定）**
- **Date**: 2026-04-01
- **Deciders**: key
- **Related**:
  [Issue #60](https://github.com/key/the-space-memory/issues/60)

## Context

`user_dict_path()` が `state_dir().join("user_dict.csv")` として
毎回 derived path を計算していた。
他のパス（`embedder_socket_path`, `daemon_socket_path`, `log_dir`）は
`ResolvedConfig` のフィールドとして起動時に一度だけ解決される設計だったが、
`user_dict_path` だけがこのパターンから外れていた。

## Decision

全てのユーザー設定（パス・パラメータ）は `ResolvedConfig` シングルトンで管理する。

### ルール

1. **`ConfigFile`** にオプショナルフィールドを追加する
   （TOML からのデシリアライズ用）
2. **`ResolvedConfig`** に確定値フィールドを追加する
3. **`from_config_file()`** で解決する
   （優先順位: 環境変数 > tsm.toml > デフォルト値）
4. **`load_config_from()`** の merge ロジックに追加する
5. **アクセサ関数** `pub fn xxx() -> T { resolved().xxx.clone() }` を提供する
6. **derived path**（`state_dir().join("xxx")` で毎回計算）は使わない

### 新しい設定項目を追加する際のチェックリスト

- [ ] `ConfigFile` にフィールド追加
- [ ] `ResolvedConfig` にフィールド追加（doc comment で Default/Env/Config を記載）
- [ ] `from_config_file()` に `env_or()` / `env_parse_*()` で解決ロジック追加
- [ ] `Self { ... }` にフィールド追加
- [ ] `load_config_from()` の merge ロジックに追加
- [ ] アクセサ関数を追加
- [ ] テスト（`test_resolved_defaults` 等）を更新

## Consequences

- 設定の解決が起動時1回に集約され、パフォーマンスとデバッグ性が向上する
- 全設定が環境変数・tsm.toml・デフォルト値の3段階で上書き可能になる
- 設定の追加手順が明確になり、パターンの逸脱を防げる
