# ADR-0012: 出力チャネルモデル（ユーザー出力 / ログ / エラーの分離）

- **Status**: Accepted
- **Date**: 2026-06-22 (Proposed) / 2026-06-22 (Accepted)
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0005](./0005-embedder-binary-consolidation.md),
  [ADR-0011](./0011-uninitialized-db-failfast-and-readonly-doctor.md)

## Context

`tsm` / `tsmd` は「コマンド結果」「動作ログ」「エラー」を区別せず、多くを
`log::*`（stderr / ファイル）に流していた。このため次の不整合が生じていた。

1. **結果がパイプできない**
   `tsm dict update` などの出力が `log::info!`（= stderr）で、
   `tsm dict update | head | pbcopy` のように stdout を加工・コピーできない。
   コマンドの「成果物」が標準出力に乗っていない。

2. **背景 daemon の警告が端末へ漏れる**
   detached daemon の stderr を端末が継承し、WARN/ERROR が溢れる
   （ADR-0011 の Bug1）。

3. **ログファイルがプロセス別に分散**
   daemon / embedder / watcher が各自ファイル＋日次 7 世代で最大 ~24 ファイル。
   12-factor（アプリは自前でログ管理しない）やローカル daemon の慣行
   （単一ストリーム or supervisor 委譲。例: Syncthing は stderr を 1 ファイル）
   からも乖離。

4. **抑制とユーザー出力が同居**
   既定ログレベルを上げ下げすると、コマンドの成果物まで消える／溢れる。
   「静かにする」ことと「結果を出す」ことが分離できていなかった。

根本原因は **3 種類の出力が 1 チャネルに混在**していたこと。これを分離する。

## Decision

出力を 3 チャネルに定義し、行き先・レベル・パイプ可否を固定する。

### 出力チャネルモデル

| # | チャネル | 内容 | 行き先 | レベル / 制御 | パイプ |
|---|---|---|---|---|---|
| ① | ユーザー出力（結果） | コマンドの成果物：検索結果、init/setup/dict/index の進捗と結果、status/doctor 表示、明示時の `tsmd started` / `stopped` | **stdout**（`println!`） | レベルに依らず常時。`--format json` で機械可読 | ✅ |
| ② | 通常ログ（診断） | 内部動作の記録。CLI の補助情報、daemon の運用ログ | logger。CLI=**stderr** / daemon=**`tsmd.log`**（いずれも既定 `info`） | `RUST_LOG` で上書き | ❌ |
| ③ | エラー出力 | 失敗・警告。致命的 anyhow エラー、`log::warn!` | **stderr**（CLI）/ daemon ツリーの raw stderr は detached 時 `tsmd-stderr.log` | 致命時は非ゼロ終了 | ❌ |

### 不変条件

- **stdout は「結果」専用**。診断・進捗ノイズ・ログを混ぜない。これにより
  `| head` / `| jq` / `| pbcopy` が常に成立する。
- **② ③ は stdout に出さない**（stderr / ファイル）。パイプライン出力を汚さない。
- **ログとユーザー出力は独立**。① は `println!` なのでログレベルに依らず必ず
  出る。`tsm dict update` のような成果物は、ログ設定に関わらず消えない。
- **暗黙の処理は静音**。daemon の auto-start（daemon-routed コマンド経由）は
  ① にも ② にも余計な行を出さない。明示操作（`tsm start` / `stop` / `restart`）
  のみ ① に確認を出す。

### プロセス別の写像

- **`tsm`（CLI）**: ① stdout / ②③ stderr。logger 既定 `info`
  （`logging::LogMode::Stderr`）。ユーザー向けの全コマンド出力は `log::info!`
  ではなく `println!` なので、② のログとは混ざらない。
- **`tsmd`（daemon・detached）**: stdout/stderr を端末から切り離す。
  ② は `tsmd.log`（`info`、日次ローテーション・3 世代）。
  ③ はツリーの raw stderr で `cmd_start` が `tsmd-stderr.log` に捕捉。
- **子（embedder / watcher）**: 独自ファイルを持たず、親の stderr を継承して
  `tsmd-stderr.log` へ。レベルは `info`（`LogMode::DaemonStderr`）。端末へは
  継承させず捕捉するため、`info` でも端末スパムにはならない。
- **foreground `tsmd`**: ②③ はそのまま端末 stderr。

### ログファイルは 2 つに集約

プロセス別 ~24 ファイルを 2 つに減らす。

- **`tsmd.log`** — daemon 本体の構造化ログ（`info`）。日次ローテーション・
  3 世代保持。daemon の通常ログ・警告は全てここに残る。
- **`tsmd-stderr.log`** — daemon ツリー全体の raw stderr。`cmd_start` が spawn 時に
  リダイレクトし、子はこれを継承。pre-logger の起動失敗・fail-fast の理由
  （ADR-0011、`cmd_start` がここを読んで表示）・致命的エラーが集まる。

端末スパムの防止はレベルではなく構造で担保する:

- 子の stderr は端末へ継承させず `tsmd-stderr.log` に捕捉する（端末に溢れない）。
- daemon は `tsmd.log` に構造化出力を持つため `duplicate_to_stderr` を廃止する
  （冗長かつ daemon の warn を端末へ複製していた原因）。

`tsmd-stderr.log` は起動ごとに truncate するため、複数起動を跨いでは蓄積しない。
ただし **rotate はされず**、子を `info` で書くため、1 つの長寿命 daemon
セッション中はサイズ上限が無い。サイズ上限 / rotation の付与は follow-up とする
（レベルを下げて抑制する案は、子のライフサイクルログが消えるため採らない）。

## Consequences

### Positive

- `tsm dict update | head | pbcopy` のようなパイプ加工が常に成立する。
- 背景 daemon の警告がシェルに溢れない（stderr を端末継承させず捕捉し、
  `duplicate_to_stderr` も廃止したため）。これがレベルに依らず成り立つ。
- 「結果を出す」と「ログ」が独立。① は `println!` なのでレベルに関わらず不変。
- ログファイルがプロセス別最大 ~24 → **2 ファイル**。調査時の参照先が明確
  （`tsmd.log` = 構造化、`tsmd-stderr.log` = ツリーの raw stderr）。
- ログレベルを下げないため、子のライフサイクルや診断 `info` が消えない。

### Negative

- CLI はデフォルト `info` のため、`tsm` 実行時に診断 `info` が stderr（端末）へ
  出る。結果（①）は `println!` で別系統だが、端末を完全に無音にしたい場合は
  `RUST_LOG=warn` で絞る。
- 子（embedder / watcher）のログは構造化ファイル・ローテーションを失い、
  現行 daemon セッションの `tsmd-stderr.log`（起動ごとに truncate）にのみ残る。
  rotate されないため長寿命セッションでは増加しうる（上限付与は follow-up）。
  長期保持が必要なら `tsmd.log` 側で拾うか、将来 supervisor（systemd/launchd）へ
  委譲する。複数プロセスが 1 つの継承 stderr に書くため行の interleave は
  起こりうる（共有 fd 追記のため破損はしない）。
- `tsm start` / `stop` の確認は明示実行時のみ。auto-start 経由は無音で、状態
  確認は `tsm doctor` / `tsm status` に一本化される。
