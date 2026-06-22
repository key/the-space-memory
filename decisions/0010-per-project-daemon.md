# ADR-0010: tsmd の per-project identity と socket 競合解決

- **Status**: **Proposed**
- **Date**: 2026-05-19
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0003](./0003-config-via-resolved-config.md),
  [ADR-0009](./0009-workspace-and-content-model.md)（`project_root` の定義元）,
  [#200](https://github.com/key/the-space-memory/issues/200)

## Context

[ADR-0009](./0009-workspace-and-content-model.md) はプロジェクトの境界（CWD 直下
`tsm.toml` → `--project-root` で確定する `project_root`）を定義した。

socket と DB はプロジェクトごとに `<project_root>/.tsm/` 配下で分離されているため、
複数のプロジェクトで daemon を並列稼働させること自体は既に可能である。しかし、
稼働中の daemon の所属が外から判別できず、起動時の socket 競合にも穴がある。

本 ADR は、その `project_root` を前提にプロセスの境界を定義する。具体的には、どの
`tsmd` がどのプロジェクト用かを `ps` で識別できるようにする方法と、起動時の socket
競合を防ぐ方法を扱う。現状、以下の不都合が残っている。

### 課題

1. **`ps -ef` で daemon の所属が判別できない**
   `tsmd` / `tsmd --embedder` / `tsmd --fs-watcher` がどのプロジェクト用か、argv からも
   env からも分からない。複数プロジェクトを並列稼働させたとき、どの daemon がどの
   プロジェクトを担当しているかを追えず、トラブルシュートが困難である。

2. **`tsmd` 直接起動時の silent socket clobber**（[#200](https://github.com/key/the-space-memory/issues/200)）
   `src/bin/tsmd/daemon_mode.rs:33-36` が既存 socket を無条件に削除している。
   `tsm start` 経由（`src/main.rs:489-498`）の Ping だけがガードであり、バイナリ
   直接起動の経路では同じ state_dir に対し silent な上書きが発生する。

## Decision

### 1. tsmd の per-project identity（`--project-root` 引数）

`tsmd` は常に `tsm` から起動される（`src/main.rs` の `cmd_start` が `tsmd` を spawn
する）。そこで `tsm start` は、起動する `tsmd`（および `--embedder` / `--fs-watcher`
子プロセス）の argv に必ず `--project-root <canonical_abs_path>` を付与する。
`tsm start` は ADR-0009 §2 で確定済みの `project_root` を持つため、これを明示的に渡す。

`tsmd` 自身はプロジェクトルートの探索を行わない。受け取った `--project-root` を
`project_root` として用いる。引数は canonical 化済みの絶対パスのみとし、相対パスや
`~` 短縮形は渡さない。これにより、稼働中の `tsmd` は `ps` や `pgrep -af tsmd` で必ず
フルパスの所属プロジェクトを表示する。

```text
tsmd --project-root /Users/key/work/proj-a
tsmd --project-root /Users/key/work/proj-a --embedder --no-idle-timeout
tsmd --project-root /Users/key/work/proj-a --fs-watcher
```

env での暗黙伝達は使わない（`ps` 可視性のため）。デバッグ目的で `tsmd` を直接起動する
場合は、ADR-0009 §2 の解決規則（CWD 直下 `tsm.toml` → `--project-root` → 失敗）に
従う。

### 2. 起動直後の処理と socket 競合検知

`tsmd` は起動直後に次の処理を行う。

1. `chdir(project_root)` を呼ぶ。daemonize 慣習の `chdir("/")` は採用しない
   （`/proc/<pid>/cwd` でプロジェクトを識別可能にするため）。
2. state_dir を `<project_root>/.tsm/` に固定する（env による override は ADR-0009 §6
   の escape hatch として残す）。
3. `<project_root>/.tsm/daemon.sock` の競合を、Ping と `tsmd.pid` の PID 生存で判定する。

| `daemon.sock` への Ping | `tsmd.pid` の PID | 判定 |
|---|---|---|
| 応答あり | — | 同一プロジェクトで daemon が稼働中 → 非零終了 |
| 応答なし | 生存 | 応答しない daemon が存在 → socket を消さず非零終了 |
| 応答なし | 不在 / 死亡 | stale と判断 → socket を削除して bind |

判定の補足：

- 「応答あり」の場合は「同一プロジェクトで daemon が既に稼働中」とログを出して終了する。
- 「応答なし + PID 生存」の場合は socket を削除せず、「既存 tsmd (pid N) が応答しない」
  とログを出して終了し、利用者に調査を促す。
- socket を無条件に削除しないことで、[#200](https://github.com/key/the-space-memory/issues/200)
  の silent clobber を構造的に解決する。PID が生きているのに応答しない（hung または
  起動途中の）daemon の socket を奪わないのが要点である。

### 3. CLI 側の起動・接続フロー

`tsm start`（`<project-root>/.tsm/` は `tsm init` が作成済みである前提。未初期化なら
`tsmd` が fail-fast する）:

```text
1. project_root 確定（ADR-0009 §2: CWD 直下 tsm.toml → --project-root → 失敗）
2. 起動: tsmd --project-root <abs_path> [--no-watcher]（--project-root を常に付与）
3. <project-root>/.tsm/daemon.sock の出現を待つ（既存ロジック流用）
```

`tsm search` / `status` / `doctor` 等:

```text
1. project_root 確定（ADR-0009 §2: CWD 直下 tsm.toml → --project-root → 失敗）
2. <project-root>/.tsm/daemon.sock に接続
3. 不在の場合は `tsm start` の実行を促すエラーで終了する
```

### 4. `ps` 可視性

```text
USER  PID    PPID   CMD
key   12345  1      tsmd --project-root /Users/key/work/proj-a
key   12346  12345  tsmd --project-root /Users/key/work/proj-a --embedder --no-idle-timeout
key   12347  12345  tsmd --project-root /Users/key/work/proj-a --fs-watcher
key   12350  1      tsmd --project-root /Users/key/work/proj-b
key   12351  12350  tsmd --project-root /Users/key/work/proj-b --embedder --no-idle-timeout
```

加えて daemon が `chdir(project_root)` するため、`ls -l /proc/<pid>/cwd` や
`pwdx <pid>` でも判別できる。

argv 書換系（`prctl(PR_SET_NAME)` や `setproctitle` クレート）は採用しない。
platform 差・16 文字制限・実装の複雑さがあり、`Command::arg()` で argv を組み立てる
方が単純かつポータブルだからである。

## Rationale

**`--project-root` を argv に明示する理由**:
`ps` で識別できることが運用上の最大の要望である。argv は `execve(2)` 時にカーネルへ
渡され `/proc/<pid>/cmdline` で観測されるため、追加コスト無く可視化できる。

env で伝達すると `cat /proc/<pid>/environ` が必要で日常運用に向かない。argv 書換系
（`setproctitle` 等）は platform 差が大きい。したがって `Command::arg()` で argv を
組み立てる方式を採る。

**フルパス（絶対パス）で渡す理由**:
`ps` での識別はフルパスで初めて一意になる。相対パスでは「どの CWD から見た相対か」が
`ps` から読めず、識別目的を果たさない。`~` 短縮も shell 依存で曖昧である。daemon は
`chdir(project_root)` する以上、起点は確定した絶対パスでなければ整合しない。

**daemon が `chdir(project_root)` する理由**:
2 段目の判別経路として `/proc/<pid>/cwd` を活用するためである。子プロセスにも CWD が
継承され、相対パスの解決基準が一意になる副次効果もある。伝統的な `chdir("/")` は
「カレントディレクトリのアンマウントを妨げない」ためだが、`project_root` が削除される
運用は想定しないため不要である。

**socket を無条件に削除しない理由**:
[#200](https://github.com/key/the-space-memory/issues/200) の核心は「応答する、または
PID が生きている daemon の socket を奪うと、稼働中の別 daemon を黙って壊す」点にある。
Ping と PID 生存の二段で「本当に死んでいる」場合のみ stale として削除すれば、重複起動を
確実に検知でき、生存 daemon を保護できる。

**env を escape hatch に格下げする理由**:
ADR-0001 の「プロセスの責務を明示する」方針と一貫する。親から子へのプロセス依存は
宣言として argv で渡し、env は環境（利用者が意図的に override する場面）に限定するのが
筋である。

**ADR-0009 と分ける理由**:
ADR-0009 はプロジェクトの境界とコンテンツ参照を扱い、本 ADR はプロセス境界の識別と
競合検知を扱う。本 ADR は ADR-0009 の `project_root` を消費する依存関係にあるが、判断の
中身（daemon の重複検知・`ps` 可視化・socket clobber の解決）は独立してレビュー・実装
できる小さな決定であり、1 決定 = 1 ADR の方針に沿って分離する。

## Consequences

### Positive

- `ps -ef` や `pgrep -af tsmd` で、どの daemon がどのプロジェクト用かを即座に判別できる。
  複数プロジェクトの並列稼働は元々可能だが、本 ADR により稼働中の daemon の所属が
  外から追えるようになる。Claude Code プラグイン（別 repo
  [`key/claude-code-plugins`](https://github.com/key/claude-code-plugins) で管理）の
  hook はプロジェクトルートに cd 済みのため、追加変更は不要である。
- [#200](https://github.com/key/the-space-memory/issues/200) の silent socket clobber が
  構造的に解決し、生存 daemon を誤って壊さない。
- DB・socket・log の境界がプロジェクト単位で完全に分離され、片方の障害が他方に波及
  しない（SQLite WAL のロック競合も発生しない）。

### Negative

- `tsmd` を直接起動する場合、CWD 直下に `tsm.toml` が無ければ `--project-root` が必要に
  なる。どちらも無い場合は起動失敗する。通常運用は `tsm start` 経由のため影響は限定的
  だが、デバッグ用の直接起動手順は更新が必要である。
- env を escape hatch に格下げするため、グローバル `TSM_STATE_DIR` を前提にしていた
  既存スクリプトは挙動が変わる可能性がある。
- 「応答なし + PID 生存」を非零終了にするため、PID ファイルが古い、または不正な状態に
  なっている（別プロセスが同 PID を再利用した）稀なケースでは手動介入が要る。誤って
  生存 daemon を壊すより安全側に倒す判断である。

### Follow-ups

- **Umbrella issue を作成**し、以下のタスク粒度でサブ issue 化する。
  1. `tsmd` に `--project-root <PATH>` 引数（canonical 絶対パス）を追加し、それを
     `project_root` として用いる（`tsmd` 自身は探索しない。直接起動時のみ ADR-0009 §2 の
     `resolve_project_root` を適用）。`chdir` の実装と state_dir の固定も行う。
  2. `tsm start` が解決した `project_root` を、起動時に argv へ `--project-root` として
     注入する（CWD に `tsm.toml` があっても `ps` 可視性のため常に渡す）。
  3. 子プロセスの起動（`child::spawn_child`）にも `--project-root` を継承する。
  4. `daemon_mode.rs` の socket clobber を「Ping + PID alive チェック」の三分岐に
     置き換える（[#200](https://github.com/key/the-space-memory/issues/200) を解決）。
  5. ドキュメントを更新する（README / README.ja / CLAUDE.md。`tsmd` 直接起動手順に
     `project_root` の解決順を明記する）。
- **ADR-0009 との順序**: 本 ADR は ADR-0009 の `project_root` 決定
  （`resolve_project_root`）を前提とするため、ADR-0009 のプロジェクトルート探索を先行
  実装する。
