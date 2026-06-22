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

[ADR-0009](./0009-workspace-and-content-model.md) が **workspace の境界**
（CWD 直下 `tsm.toml` → `--project-root` で確定する `project_root`）を定義した。
本 ADR はその `project_root` を
受けて、**プロセスの境界 ——どの `tsmd` がどの workspace 用か—— をどう識別し、
競合をどう防ぐか** を扱う。現状以下の不都合が残っている。

### 課題

1. **複数 workspace の並列稼働ができない**
   1 マシン 1 daemon を前提としており、複数 Claude Code セッションを
   別々の workspace で同時に開くと検索対象が混線する。
   workspace 単位で独立した daemon を持てる構造が無い

2. **`tsmd` 直接起動時の silent socket clobber**（[#200](https://github.com/key/the-space-memory/issues/200)）
   `src/bin/tsmd/daemon_mode.rs:33-36` で既存 socket を無条件削除している。
   `tsm start` 経由（`src/main.rs:489-498`）の Ping だけがガードで、
   バイナリ直接起動経路では同じ state_dir に対し silent な上書きが発生する

3. **`ps -ef` で daemon の所属が判別不能**
   `tsmd` / `tsmd --embedder` / `tsmd --fs-watcher` がどの workspace 用か
   argv からも env からも分からない。複数 workspace 並列時のトラブルシュート
   が困難

## Decision

### 1. tsmd の per-project identity（`--project-root` 引数）

`tsmd` は `project_root` を **ADR-0009 §2 の共通アルゴリズム**で決定する：
**CWD 直下に `tsm.toml` があればそれ、無ければ `--project-root` 引数、
どちらも無ければ起動失敗**。

**`tsm start` が spawn する `tsmd`（および `--embedder` / `--fs-watcher`
子プロセス）には、起動後の argv に必ず `--project-root <canonical abs_path>` が
付与される。** `tsm start` は確定済みの `project_root` を持っているため、CWD に
`tsm.toml` があるか否かに関わらず常に明示注入する。これにより、稼働中の
`tsmd` は `ps` / `pgrep -af tsmd` で必ずフルパスの所属 workspace を表示する。
値は **常に canonical 化されたフルパス（絶対パス）** とし、相対パスや `~`
短縮形は argv に渡さない。

加えて `tsm start` は子プロセスの初期 CWD を `project_root` に設定して spawn
する。これにより §2 の「CWD 直下 `tsm.toml` 優先」ルールと注入した
`--project-root` が必ず一致し、別の `tsm.toml` を誤って拾う余地が無い。

`--project-root` 自体は必須引数ではない（直接起動で CWD 直下に `tsm.toml` が
あれば省略可）。ただし `tsm start` 経由では上記のとおり常に付与される。

```text
tsmd --project-root /workspaces/proj-a
tsmd --project-root /workspaces/proj-a --embedder --no-idle-timeout
tsmd --project-root /workspaces/proj-a --fs-watcher
```

子プロセス（`--embedder` / `--fs-watcher`）も argv に `--project-root <abs_path>`
を継承する。env での暗黙伝達は使わない（`ps` 可視性のため）。

### 2. 起動直後の処理と socket 競合検知

`tsmd` は起動直後に以下を行う：

1. `chdir(project_root)` を呼ぶ
   - daemonize 慣習の `chdir("/")` は採用しない（`/proc/<pid>/cwd` で
     workspace を識別可能にするため）
2. state_dir を `<project_root>/.tsm/` に固定（env による override は
   ADR-0009 §6 の escape hatch として残す）
3. `<project_root>/.tsm/daemon.sock` の競合を、Ping と `tsmd.pid` の
   PID 生存で判定する：

| `daemon.sock` への Ping | `tsmd.pid` の PID | 判定 |
|---|---|---|
| 応答あり | — | 同一 workspace で daemon が稼働中 → ログを出して**非零終了** |
| 応答なし | 生存している | 応答しない daemon が存在 → **socket を消さず**「既存 tsmd (pid N) が応答しない」とログを出して**非零終了**（調査を促す） |
| 応答なし | 不在 / 死亡 | stale と判断 → socket を削除して bind |

**socket を無条件削除しない**ことで [#200](https://github.com/key/the-space-memory/issues/200)
の silent clobber を構造的に解決する。PID が生きているのに応答しない
（hung / 起動途中）daemon の socket を奪わないのが要点。

### 3. CLI 側の起動・接続フロー

`tsm start`:

```text
1. project_root 確定（ADR-0009 §2: CWD 直下 tsm.toml → --project-root → 失敗）
2. <workspace>/.tsm/ を必要なら作る
3. spawn:  tsmd --project-root <abs_path> [--no-watcher]
           （子プロセスの初期 CWD = project_root。--project-root は常に付与）
4. <workspace>/.tsm/daemon.sock の出現を待つ（既存ロジック流用）
```

`tsm search` / `status` / `doctor` 等:

```text
1. project_root 確定（ADR-0009 §2: CWD 直下 tsm.toml → --project-root → 失敗）
2. <workspace>/.tsm/daemon.sock に接続
3. 不在 → エラー「tsm start してね」
```

### 4. `ps` 可視性

```text
USER  PID    PPID   CMD
key   12345  1      tsmd --project-root /workspaces/proj-a
key   12346  12345  tsmd --project-root /workspaces/proj-a --embedder --no-idle-timeout
key   12347  12345  tsmd --project-root /workspaces/proj-a --fs-watcher
key   12350  1      tsmd --project-root /workspaces/proj-b
key   12351  12350  tsmd --project-root /workspaces/proj-b --embedder --no-idle-timeout
```

加えて daemon が `chdir(project_root)` するため
`ls -l /proc/<pid>/cwd` / `pwdx <pid>` でも判別可能。

argv 書換系（`prctl(PR_SET_NAME)` / `setproctitle` クレート）は
**採用しない**。理由は platform 差・16 文字制限・実装複雑性で、
`Command::arg()` で argv を組み立てる方が単純かつポータブルだから。

## Rationale

**なぜ `--project-root` を argv に明示するか**:
`ps` で識別できることが運用上の最大の要望。argv は `execve(2)` 時に
カーネルに渡され `/proc/<pid>/cmdline` で観測されるため、追加コスト無く
可視化できる。env で伝達すると `cat /proc/<pid>/environ` が必要で
日常運用には向かない。argv 書換系（`setproctitle` 等）は platform 差が
大きく、`Command::arg()` で argv を組み立てる方が単純。

**なぜフルパス（絶対パス）で渡すか**:
`ps` での識別はフルパスで初めて一意になる。相対パスでは「どの CWD から見た
相対か」が `ps` から読めず識別目的を果たさない。`~` 短縮も shell 依存で
曖昧。daemon は `chdir(project_root)` する以上、起点は確定した絶対パスで
なければ整合しない。

**なぜ daemon が `chdir(project_root)` するか**:
2 段目の判別経路として `/proc/<pid>/cwd` を活用するため。
子プロセスにも CWD が継承され、相対パスの解決基準が一意になる副次効果あり。
伝統的な `chdir("/")` は「カレントディレクトリのアンマウントを邪魔しない」
ためだが、project_root が削除される運用は想定しないため不要。

**なぜ socket を無条件削除しないか**:
[#200](https://github.com/key/the-space-memory/issues/200) の核心は
「応答する / PID が生きている daemon の socket を奪うと、稼働中の別 daemon を
黙って壊す」点にある。Ping と PID 生存の二段で「本当に死んでいる」場合のみ
stale 削除すれば、重複起動を確実に検知でき、生存 daemon を保護できる。

**なぜ env を escape hatch に格下げするか**:
ADR-0001 の「プロセスの責務を明示する」方針と一貫する。
親 → 子のプロセス依存は宣言として argv で渡し、env は環境（ユーザーが
意図的に override する場面）に限定するのが筋。

**なぜ ADR-0009 と分けるか**:
ADR-0009 は **workspace の境界** と **コンテンツ参照** を扱い、本 ADR は
**プロセス境界の識別と競合検知** を扱う。本 ADR は ADR-0009 の `project_root`
を消費する依存関係にあるが、判断の中身（daemon 重複検知・`ps` 可視化・
socket clobber 解決）は独立してレビュー / 実装できる小さな決定であり、
1 決定 = 1 ADR の方針に沿って分離する。

## Consequences

### Positive

- 複数 workspace の並列稼働が自然に動く。Claude Code プラグイン
  （別 repo [`key/claude-code-plugins`](https://github.com/key/claude-code-plugins) で管理）
  の hook は workspace に cd 済みのため追加変更不要
- `ps -ef` / `pgrep -af tsmd` でどの daemon がどの workspace 用か即判別可能
- [#200](https://github.com/key/the-space-memory/issues/200) の silent socket
  clobber が構造的に解決し、生存 daemon を誤って壊さない
- DB / socket / log の境界が workspace 単位で完全分離され、片方の障害が
  他方に波及しない（SQLite WAL ロック競合も発生しない）

### Negative

- `tsmd` を直接起動する際、CWD 直下に `tsm.toml` が無ければ `--project-root`
  が必要になる（CWD に `tsm.toml` も `--project-root` も無いと起動失敗）。
  通常運用は `tsm start` 経由のため影響は限定的だが、デバッグ用の直接起動
  手順は更新が必要
- env vars を escape hatch に格下げしたことで、グローバル `TSM_STATE_DIR`
  を前提にしていた既存スクリプトは挙動が変わる可能性
- 「応答なし + PID 生存」を非零終了にするため、PID ファイルが腐っている
  （別プロセスが同 PID を再利用）稀なケースでは手動介入が要る。
  誤って生存 daemon を壊すより安全側に倒す判断

### Follow-ups

- **Umbrella issue を作成**、以下のタスク粒度でサブ issue 化：
  1. `tsmd` に `--project-root <PATH>` 引数追加（canonical 絶対パス）、
     ADR-0009 §2 の `resolve_project_root` を適用、`chdir` 実装、state_dir 固定
  2. `tsm start` が解決した project_root を spawn 時に argv へ
     `--project-root` 注入（CWD に tsm.toml があっても ps 可視性のため常に渡す）
  3. 子プロセス spawn (`child::spawn_child`) にも `--project-root` 継承
  4. `daemon_mode.rs` の socket clobber を「Ping + PID alive チェック」の
     三分岐に置き換え（[#200](https://github.com/key/the-space-memory/issues/200) 解決）
  5. ドキュメント更新（README / README.ja / CLAUDE.md。
     `tsmd` 直接起動手順に project_root 解決順を明記）
- **ADR-0009 との順序**: 本 ADR は ADR-0009 の `project_root` 決定
  （`resolve_project_root`）を前提とするため、ADR-0009 の workspace 探索を
  先行実装する
