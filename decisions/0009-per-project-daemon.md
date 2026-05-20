# ADR-0009: workspace 探索の walk-up 化と tsmd の per-project identity

- **Status**: **Proposed**
- **Date**: 2026-05-19
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0003](./0003-config-via-resolved-config.md),
  [ADR-0008](./0008-setup-init-separation.md)（partially supersedes）

## Context

[ADR-0008](./0008-setup-init-separation.md) により workspace state は
`<workspace>/.tsm/` に集約され、`tsm.db` / `daemon.sock` / `tsmd.pid` / 各種
scaffold ファイル / cache へのリンクが配置される構造になった。
しかし「どこを workspace と認識するか」「複数 workspace を同時に動かすときの
プロセス境界をどう区別するか」は ADR-0008 のスコープ外で、現状以下の不都合が
残っている。

### 課題

1. **workspace 探索が CWD のみ**
   `config::state_dir()` は `DEFAULT_STATE_DIR = ".tsm"` を CWD 相対で返す。
   workspace のサブディレクトリから `tsm` を叩くと workspace を見失い、
   そこに `.tsm/` を作ろうとする（または XDG global にフォールバックする）。
   git / mise / cargo はいずれも上方向への探索を行うのが業界慣習であり、
   tsm だけがこの慣習に反している

2. **`tsm.toml` の置き場所が見えにくい**
   ADR-0008 line 96 では `tsm.toml (任意)` を `.tsm/` の中に置く設計だが、
   `.tsm/` は機械的な state ディレクトリであり、人が日常的に編集する設定
   ファイルを混在させるとユーザーから可視性が著しく下がる。
   mise（`.mise.toml` / `mise.toml`）/ cargo（`Cargo.toml`）/
   pyproject.toml がいずれも workspace 直下に置く慣習に揃えるべき

3. **複数 workspace の並列稼働ができない**
   1 マシン 1 daemon を前提としており、複数 Claude Code セッションを
   別々の workspace で同時に開くと検索対象が混線する。
   workspace 単位で独立した daemon を持てる構造が無い

4. **`tsmd` 直接起動時の silent socket clobber**（[#200](https://github.com/key/the-space-memory/issues/200)）
   `src/bin/tsmd/daemon_mode.rs:33-36` で既存 socket を無条件削除している。
   `tsm start` 経由（`src/main.rs:489-498`）の Ping だけがガードで、
   バイナリ直接起動経路では同じ state_dir に対し silent な上書きが発生する

5. **`ps -ef` で daemon の所属が判別不能**
   `tsmd` / `tsmd --embedder` / `tsmd --fs-watcher` がどの workspace 用か
   argv からも env からも分からない。複数 workspace 並列時のトラブルシュート
   が困難

## Decision

### 1. workspace marker は `<workspace>/tsm.toml`（root に置く）

**ADR-0008 line 96 の `.tsm/tsm.toml (任意)` 記述を本 ADR で supersede する。**

設定ファイル `tsm.toml` は workspace 直下に置く。`.tsm/` の中には置かない。

```text
<workspace>/
├── tsm.toml          ← workspace marker、人が編集、git に入れる
└── .tsm/             ← ADR-0008 で定義された state 一式（git ignore）
    ├── tsm.db (+ -shm, -wal)
    ├── synonyms.csv / stopwords.txt / reject_words.txt / custom_terms.toml / user_dict.csv
    ├── logs/
    ├── {daemon,embedder}.sock
    ├── {tsmd,embedder}.pid
    ├── tsm-status.json
    ├── models/ruri-v3-30m → $cache_dir/models/ruri-v3-30m
    └── wnjpn.db          → $cache_dir/wnjpn.db
```

`.tsm/` の内部構造は ADR-0008 に従う。本 ADR は marker の位置のみを変更する。

### 2. workspace 探索は walk-up 方式

`tsm` / `tsmd` は **CWD から上方向に辿って最初に見つかった `tsm.toml`** を
workspace marker として採用する。それを含むディレクトリが workspace。

```text
discover_workspace(cwd):
    for d in cwd.ancestors():
        if (d / "tsm.toml").exists():
            return d
    return None
```

`.tsm/` ディレクトリの存在は marker として用いない（`.tsm/` は
state の置き場であり workspace の意味論的な境界ではないため）。

#### コマンド別の未発見時挙動

| コマンド | `tsm.toml` 未発見時 |
|---|---|
| `tsm init` | CWD 直下に `tsm.toml` + `.tsm/` を作る（ADR-0008 の init 仕様に従う） |
| `tsm start` | エラー「`tsm init` を実行してね」 |
| `tsm search` / `status` / `doctor` 等 | エラー「`tsm init` を実行してね」 |

### 3. tsmd の per-project identity（`--project-root` 引数）

`tsmd` は **workspace の絶対パスを必須引数として受ける**。

```text
tsmd --project-root /workspaces/proj-a
tsmd --project-root /workspaces/proj-a --embedder --no-idle-timeout
tsmd --project-root /workspaces/proj-a --fs-watcher
```

起動直後の処理：

1. `chdir(project_root)` を呼ぶ
   - daemonize 慣習の `chdir("/")` は採用しない（`/proc/<pid>/cwd` で workspace を識別可能にするため）
2. state_dir を `<project_root>/.tsm/` に固定（env による override は escape hatch として残す）
3. 既存 `daemon.sock` があれば Ping
4. Ping 応答あり → 「同じ workspace で daemon が既に動いている」とログを出して非零終了
5. Ping 応答なし + `tsmd.pid` の PID も生きていない → stale と判断、socket 削除して bind

これにより [#200](https://github.com/key/the-space-memory/issues/200) の
silent clobber は構造的に解決する（同一 workspace への重複起動を確実に検知）。

子プロセス（`--embedder` / `--fs-watcher`）も argv に `--project-root <abs_path>`
を継承する。env での暗黙伝達は使わない（`ps` 可視性のため）。

### 4. CLI 側の探索フロー

`tsm start`:

```text
1. CWD から walk-up → workspace 確定
2. <workspace>/.tsm/ を必要なら作る
3. spawn:  tsmd --project-root <abs_path> [--no-watcher]
4. <workspace>/.tsm/daemon.sock の出現を待つ（既存ロジック流用）
```

`tsm search` / `status` / `doctor` 等:

```text
1. CWD から walk-up → workspace 確定
2. <workspace>/.tsm/daemon.sock に接続
3. 不在 → エラー「tsm start してね」
```

### 5. `ps` 可視性

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

### 6. escape hatch（環境変数の扱い）

| 変数 | 扱い |
|---|---|
| `TSM_CONFIG` | 指定されていれば walk-up をスキップしてそのファイルを採用 |
| `TSM_STATE_DIR` | 指定されていれば `<workspace>/.tsm/` を上書き |
| `TSM_INDEX_ROOT` / `TSM_DAEMON_SOCKET` / `TSM_EMBEDDER_SOCKET` 等 | 既存どおり個別パスを上書き |

CI / テスト / 一時的な実験のための escape hatch として残す。
通常運用では env を使わない前提に倒す。

## Rationale

**なぜ `tsm.toml` を root に置く（ADR-0008 を supersede してまで）**:
ADR-0008 の `.tsm/tsm.toml (任意)` は機械的に整合は取れるが、
**人が編集する設定ファイル**を機械的 state と同じ場所に置くと
ユーザーが「設定がそこにある」と気付けない。
mise / cargo / pyproject はいずれも workspace 直下に置く慣習で、
ADR-0008 task #190（`tsm init` 書き換え）が未実装の今なら破壊コストは小さい。

**なぜ walk-up を入れるか**:
CLI を打つ場所＝必ずしも workspace root とは限らない。サブディレクトリで
`tsm search` を叩いても自 workspace の daemon に繋がるのが直感的。
実装も `current_dir()` から `ancestors()` を回すだけで複雑性は低い。
git / mise / cargo / npm すべて同方式を採用している。

**なぜ `.tsm/` の存在を marker にしないか**:
`.tsm/` は state の置き場であり、空でも `mkdir` 一発で作れる。
意図的でない `.tsm/` が混入したサブディレクトリで誤検知する余地がある。
`tsm.toml` は明示的な設定ファイルであり、誤検知の余地が極めて少ない。
git の `.git/` は単一の管理ディレクトリで重複しないため安全だが、
tsm の `.tsm/` は cache link や DB を持つため作りやすく、同じ感覚で扱えない。

**なぜ `--project-root` を argv に明示するか**:
`ps` で識別できることが運用上の最大の要望。argv は `execve(2)` 時に
カーネルに渡され `/proc/<pid>/cmdline` で観測されるため、追加コスト無く
可視化できる。env で伝達すると `cat /proc/<pid>/environ` が必要で
日常運用には向かない。argv 書換系（`setproctitle` 等）は platform 差が
大きく、`Command::arg()` で argv を組み立てる方が単純。

**なぜ daemon が `chdir(project_root)` するか**:
2 段目の判別経路として `/proc/<pid>/cwd` を活用するため。
子プロセスにも CWD が継承され、相対パスの解決基準が一意になる副次効果あり。
伝統的な `chdir("/")` は「カレントディレクトリのアンマウントを邪魔しない」
ためだが、project_root が削除される運用は想定しないため不要。

**なぜ env を escape hatch に格下げするか**:
ADR-0001 の「プロセスの責務を明示する」方針と一貫する。
親 → 子のプロセス依存は宣言として argv で渡し、env は環境（ユーザーが
意図的に override する場面）に限定するのが筋。

**なぜ ADR-0008 と分けるか**:
ADR-0008 は `.tsm/` の **内部構造** と cache / workspace の責務分離を扱う。
本 ADR は **workspace の境界の決め方** と **プロセス境界の識別** を扱う。
スコープが直交しており、独立して意思決定 / レビュー / 実装できる。

## Consequences

### Positive

- 複数 workspace の並列稼働が自然に動く。Claude Code プラグイン
  （別 repo [`key/claude-code-plugins`](https://github.com/key/claude-code-plugins) で管理）
  の hook は workspace に cd 済みのため追加変更不要
- `ps -ef` / `pgrep -af tsmd` でどの daemon がどの workspace 用か即判別可能
- [#200](https://github.com/key/the-space-memory/issues/200) の silent socket clobber が構造的に解決
- 設定ファイル探索が CWD-only から walk-up になり、サブディレクトリでの
  CLI 利用が自 workspace の設定に追従する
- DB / socket / log の境界が workspace 単位で完全分離され、片方の障害が
  他方に波及しない（SQLite WAL ロック競合も発生しない）
- `tsm.toml` が workspace 直下に出ることで、ユーザーが設定の存在を
  視認しやすくなる

### Negative

- **breaking change**: 既存の XDG `state_dir` 配下に DB を持つユーザーは
  `tsm init` で workspace 直下に移行する必要がある。
  自動マイグレーションは提供しない方針（ADR-0008 と同じ思想）
- ADR-0008 の `.tsm/tsm.toml (任意)` 仕様を破壊変更で supersede するため、
  `decisions/0008-setup-init-separation.md` 側にも追記が必要
- `tsm init` が必須ステップとして増える。`cd /path && tsm start` 一発では
  動かなくなる（`tsm init` を先に挟む必要）
- walk-up のコストが各 CLI 呼び出しに加わる（ただし定数回の stat、
  実測で問題になるレベルではない）
- env vars を escape hatch に格下げしたことで、グローバル `TSM_STATE_DIR`
  を前提にしていた既存スクリプトは挙動が変わる可能性

### Follow-ups

- **ADR-0008 への追記**: `decisions/0008-setup-init-separation.md` の
  workspace state 構造図と `tsm init` 仕様で `tsm.toml` の位置を更新する
  PR を ADR-0009 と同時にマージ。ADR-0008 task #190 がまだ未実装のため
  実装コンフリクトは無い
- **Umbrella issue を作成**、以下のタスク粒度でサブ issue 化：
  1. `config.rs` に `find_workspace()`（walk-up 探索）を実装
  2. `tsm init` 拡張：`<workspace>/tsm.toml` 雛形生成、`.tsm/.gitignore`
     （`*` のみ）作成（ADR-0008 task #190 の中で対応するか別タスクにするか調整）
  3. `tsmd` に `--project-root <PATH>` 引数追加、`chdir` 実装、state_dir 固定
  4. `tsm start` で walk-up → spawn 時に argv へ `--project-root` 注入
  5. 子プロセス spawn (`child::spawn_child`) にも `--project-root` 継承
  6. `daemon_mode.rs` の socket clobber を「Ping + PID alive チェック」に
     置き換え（[#200](https://github.com/key/the-space-memory/issues/200) 解決）
  7. ドキュメント更新（README / CLAUDE.md / `docs/configuration.md` /
     `tsm.toml.example`）
- **ADR-0008 タスクとの順序調整**: task #190（`tsm init` 書き換え）と
  本 ADR の task 2 が同一コマンドを触るため、どちらかの ADR の中で
  まとめて実装するか、順序を明示する
- **`.gitignore` への `.tsm/` 自動追加**: `tsm init` 実行時に workspace
  直下の `.gitignore` へ `.tsm/` を追記するか否か。デフォルト on で
  `--no-gitignore` フラグで opt-out が妥当か、別途検討
