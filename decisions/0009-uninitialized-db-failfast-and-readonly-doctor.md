# ADR-0009: 未初期化 DB での fail-fast と doctor の read-only 化（auto-start 境界の整理）

- **Status**: Accepted
- **Date**: 2026-06-22 (Proposed) / 2026-06-22 (Accepted)
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0008](./0008-setup-init-separation.md)

## Context

ADR-0008 で `tsm setup`（machine-global なリソース取得）と
`tsm init`（workspace 固有の DB 作成・scaffold）の責務を分離した。
init は **明示的な 1 ステップ** であり、暗黙に DB を作成しないことが前提である。

しかし daemon 起動・コマンド実行パスがこの前提と整合していなかった。

### 問題点

1. **未初期化 DB で daemon が起動できず、CLI が 30 秒ハングする**
   `db::get_connection` は旧 DB 移行用の `ensure_chunk_hash_column` を呼ぶが、
   これが `chunks` テーブル不在時に `ALTER TABLE chunks ...` を実行して失敗する。
   結果 daemon は socket bind 前に終了し、`cmd_start` の起動待ちループが
   30 秒タイムアウトするまでブロックする。`tsm doctor` を含む全
   daemon-routed コマンドが対象。

2. **`tsm doctor` が診断のために daemon を auto-start し、空の `tsm.db` を生成する**
   read-only であるべき健全性診断が副作用で workspace の状態を変える。
   ADR-0008 の「init は明示」原則とも矛盾する。

3. **auto-start した detached daemon の stderr がユーザーのターミナルへ漏れる**
   `cmd_start` が daemon を `Stdio::inherit()` で起動していたため、
   daemon ロガーが stderr へ複製する WARN/ERROR と、子プロセス
   （embedder / watcher）の anyhow エラーがシェルに溢れ続ける。

4. **CLI のログレベル（既定 info）と「ユーザー出力」が未分離**
   コマンドのフィードバック（`Database initialized` 等）が `log::info!`
   経由で、ログ抑制とユーザー出力の制御が同居していた。

## Decision

「init は明示」という ADR-0008 の原則を、起動・診断経路まで一貫させる。
出力・ログチャネルの全体方針は [ADR-0010](./0010-output-channel-model.md) で扱い、
ここではその上で「未初期化 DB の起動・診断」をどう振る舞わせるかを決める。

### 1. daemon は未初期化 DB で fail-fast する

`db::is_initialized`（core テーブル `documents` / `chunks` の存在で判定）を
daemon 起動時に確認し、未初期化なら socket bind 前に
`Run \`tsm init\` first` を示して終了する。**スキーマは自動作成しない。**

旧 DB 移行専用の `ensure_chunk_hash_column` は、`chunks` テーブル不在時に
no-op とする（新 DB は SCHEMA_SQL で `content_hash` を持ち、未初期化 DB は
移行対象が無い。これは latent bug の修正でもある）。

### 2. auto-start 境界を CLI 側で明示的に検知する

`cmd_start` は spawn した daemon の stderr を `tsmd-stderr.log`
（ADR-0010 で定義）へリダイレクトし捕捉する。起動待ちループで `try_wait` に
より daemon の早期終了を検知し、捕捉した stderr の末尾を添えて即座にエラーを
返す。30 秒の盲目的なポーリングは行わない。

### 3. `tsm doctor` は daemon を auto-start しない（read-only 診断）

daemon が起動済みならその in-process レポートを使い、未起動ならローカルの
`doctor_check` にフォールバックする。`doctor_check` は未初期化・不在 DB を
グレースフルに報告し（`Run \`init\``）、`tsm.db` を生成しない。

### 4. 出力・ログの方針は ADR-0010 に従う

本 ADR の挙動は [ADR-0010](./0010-output-channel-model.md)（出力チャネルモデル）
の上に乗る。関連する帰結:

- fail-fast の理由（`Run \`tsm init\` first`）は daemon の致命的エラー（③ エラー
  出力）として`tsmd-stderr.log`に出て、`cmd_start` が読み出し端末に表示する。
- `tsm doctor` のローカルレポートは ① ユーザー出力（stdout）として表示する。
- daemon の auto-start は暗黙処理なので静音（ADR-0010 の不変条件）。

## Consequences

### Positive

- 未初期化 DB での `tsm doctor` は 31 秒ハング → 約 1 秒で明確な
  `Run \`init\``表示に。`tsm.db` の副作用生成も無くなる。
- daemon-routed コマンドは未初期化時に 30 秒待たず、即座に理由付きで失敗する。
- daemon が「初期化済み DB のみを扱う」前提が明文化・強制される
  （ADR-0001 の DB 所有権・ADR-0008 の init 明示と整合）。

（出力・ログ集約に関する帰結は [ADR-0010](./0010-output-channel-model.md) を参照。）

### Negative

- `search` / `index` 等は未初期化時に auto-start を試みて空の `tsm.db` を
  生成する（`init_db` は `IF NOT EXISTS` なので後続の `tsm init` で正しく
  初期化され、実害は無い）。doctor は read-only なので対象外。
