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
`tsmd` がどのプロジェクト用かを `ps` で識別できるようにする方法と、起動時の競合
（直接起動による silent clobber と、並行起動による多重起動レース）を防ぐ方法を扱う。
現状、以下の不都合が残っている。

### 課題

1. **`ps -ef` で daemon の所属が判別できない**
   `tsmd` / `tsmd --embedder` / `tsmd --fs-watcher` がどのプロジェクト用か、argv からも
   env からも分からない。複数プロジェクトを並列稼働させたとき、どの daemon がどの
   プロジェクトを担当しているかを追えず、トラブルシュートが困難である。

2. **socket 競合の検知が check-then-act で、silent clobber と多重起動レースを許す**
   （[#200](https://github.com/key/the-space-memory/issues/200)）
   `src/bin/tsmd/daemon_mode.rs:44-50` は既存 socket を無条件に削除してから bind する。
   `tsm start`（`src/main.rs` の `cmd_start`）側にも spawn 前の socket 削除経路があり、
   いずれも「確認してから削除して bind する」という非アトミックな手順である。結果、
   (a) バイナリ直接起動では生存 daemon の socket を黙って奪い、(b) 並行 `tsm start` が
   双方とも「socket は不在 / stale」と判定して双方 bind し、daemon が多重起動しうる。

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

env での暗黙伝達は使わない（`ps` 可視性のため）。通常運用では `tsmd` は常に `tsm start`
経由で起動され、`--project-root` を必ず受け取る。`tsmd` の直接起動はデバッグ用の例外
経路であり、その場合のみ ADR-0009 §2 の解決規則（CWD 直下 `tsm.toml` → `--project-root`
→ 失敗）を `tsmd` 自身が適用する。

### 2. 起動シーケンスの排他と socket 競合解決（flock）

`tsmd` は起動直後に次の処理を行う。

1. `chdir(project_root)` を呼ぶ。daemonize 慣習の `chdir("/")` は採用しない。
2. state_dir を `<project_root>/.tsm/` に固定する（env による override は ADR-0009 §6
   の escape hatch として残す）。
3. **`<project_root>/.tsm/tsmd.lock` に対し、プロセス生存中ずっと保持する排他 advisory
   ロック `flock(LOCK_EX | LOCK_NB)` を取得する。** これを daemon の所有権の唯一の根拠と
   する。

以下で `<project_root>/.tsm/` と書く箇所は既定の state_dir を指す。`TSM_STATE_DIR`
（ADR-0009 §6 の escape hatch）で override した場合は、lock・socket・DB がまとめて
その state_dir 配下へ移る（共有時の注意は Consequences を参照）。

ロック取得の結果で分岐する。

| `flock(LOCK_EX\|LOCK_NB)` | 判定 |
|---|---|
| 取得失敗（`EWOULDBLOCK`） | 同一プロジェクトで別の生存 daemon が所有中 → socket に触れず非零終了 |
| 取得成功 | 生存 daemon は存在しない → 既存 socket は定義上 stale。unlink して bind し、ロックを生存中保持 |

要点：

- ロックを取得できた時点で「生存している別 daemon は存在しない」ことがカーネルにより
  保証される（後述）。したがって既存の `daemon.sock` は必ず stale であり、安全に unlink
  して bind できる。「Ping して PID を確認して…」という check-then-act は不要になり、
  起動シーケンス全体が単一の不可分なゲートで直列化される。
- 取得失敗時は socket を一切触らない。これにより
  [#200](https://github.com/key/the-space-memory/issues/200) の silent clobber と、並行
  `tsm start` による多重起動レースの双方が、同じ 1 つの仕組みで構造的に解決する。
- `tsmd.lock` は**ロック保持中に unlink しない**。`flock` は open file description に紐づく
  ため、ファイルを消して別プロセスが再生成すると inode が分かれ、別々の対象をロックして
  排他が崩れる。stale な lock ファイルが残ること自体は無害（次回 `flock` で取得できる）
  なので削除しない。
- ロックファイルは **close-on-exec** で開く。`flock` は open file description に紐づき、
  `fork()` で共有され `execve()` でも fd が `O_CLOEXEC` でない限り保持される。`tsmd` は
  embedder / watcher を `fork`+`exec`（`std::process::Command`）で起動するため、ロック
  fd が子へ継承されると、`--no-idle-timeout` の embedder が親死亡後も孤児としてロックを
  握り続け、新しい daemon が永久にロックを取得できず起動デッドロックする。これを避ける
  ため、ロックは標準ライブラリの `File`（`O_CLOEXEC` が既定で付く）で開き、その fd に
  `flock` を掛ける。`O_CLOEXEC` を伴わない低レベル open は使わない。
- `tsmd.pid` は引き続き書き出すが、役割は診断情報に格下げする。所有権・生存判定は
  flock を唯一の根拠とし、PID ファイルの内容には依存しない。

### 3. CLI 側の起動・接続フロー

`tsm start`（`<project_root>/.tsm/` は `tsm init` が作成済みである前提。未初期化なら
`tsmd` が fail-fast する）:

```text
1. project_root 確定（ADR-0009 §2: CWD 直下 tsm.toml → --project-root → 失敗）
2. 起動: tsmd --project-root <abs_path> [--no-watcher]（--project-root を常に付与）
3. <project_root>/.tsm/daemon.sock の出現を待つ（既存ロジック流用）
```

`tsm start` は**起動前に socket を削除しない**。stale 判定と削除は §2 のとおり `tsmd`
側がロック取得後に行う。CLI が spawn 前に socket を消すと、ロック判定をすり抜けて
生存 daemon の socket を奪う経路が残るためである（既存 `cmd_start` の pre-spawn 削除は
この理由で廃止する）。既に稼働中かどうかを UX 上知りたい場合は Ping で確認して
メッセージを出すに留め、socket は削除しない。

`tsm search` / `status` / `doctor` 等:

```text
1. project_root 確定（ADR-0009 §2: CWD 直下 tsm.toml → --project-root → 失敗）
2. <project_root>/.tsm/daemon.sock に接続
3. 不在の場合は `tsm start` の実行を促すエラーで終了する
```

### 4. `ps` 可視性とプラットフォーム別の観測経路

argv（`ps -ef` / `pgrep -af tsmd`）を**唯一の可搬な観測経路**とする。argv は
`execve(2)` 時にカーネルへ渡され、Linux / macOS の双方で `ps` から読めるため、追加
コスト無く所属プロジェクトを可視化できる。

```text
USER  PID    PPID   CMD
key   12345  1      tsmd --project-root /Users/key/work/proj-a
key   12346  12345  tsmd --project-root /Users/key/work/proj-a --embedder --no-idle-timeout
key   12347  12345  tsmd --project-root /Users/key/work/proj-a --fs-watcher
key   12350  1      tsmd --project-root /Users/key/work/proj-b
key   12351  12350  tsmd --project-root /Users/key/work/proj-b --embedder --no-idle-timeout
```

補助的な観測経路はプラットフォーム依存であり、可搬性は argv に一本化する。

| 経路 | 可搬性 |
|---|---|
| `ps -ef` / `pgrep -af tsmd`（argv） | Linux / macOS 共通 |
| `/proc/<pid>/cwd`, `pwdx` | Linux 限定 |
| `lsof -p <pid>` | macOS（Linux でも可） |

`/proc/<pid>/cmdline` も Linux 限定であり、可搬な識別は `ps` / `pgrep` 経由の argv に
依拠する。argv 書換系（`prctl(PR_SET_NAME)` や `setproctitle` クレート）は採用しない。
platform 差・16 文字制限・実装の複雑さがあり、`Command::arg()` で argv を組み立てる方が
単純かつポータブルだからである。

## Rationale

**`--project-root` を argv に明示する理由**:
`ps` で識別できることが運用上の最大の要望である。argv は `execve(2)` 時にカーネルへ
渡され `ps` / `pgrep` で観測されるため、追加コスト無く可視化できる。env で伝達すると
`cat /proc/<pid>/environ`（Linux 限定）が必要で日常運用に向かない。argv 書換系
（`setproctitle` 等）は platform 差が大きい。したがって `Command::arg()` で argv を
組み立てる方式を採る。

**フルパス（絶対パス）で渡す理由**:
`ps` での識別はフルパスで初めて一意になる。相対パスでは「どの CWD から見た相対か」が
`ps` から読めず、識別目的を果たさない。`~` 短縮も shell 依存で曖昧である。daemon は
`chdir(project_root)` する以上、起点は確定した絶対パスでなければ整合しない。

**flock を所有権の唯一の根拠にする理由**:
従来案の「Ping + PID 生存」の三分岐は check-then-act であり、判定とロック取得の間に
他プロセスが割り込める。実際、並行 `tsm start` が双方とも「socket 応答なし + PID 不在」
と判定して双方 bind し、daemon が多重起動しうる。`flock(LOCK_EX|LOCK_NB)` は取得自体が
カーネルレベルで不可分なため、起動シーケンス全体を 1 つのゲートで直列化でき、この種の
レースを原理的に排除できる。さらに flock は**プロセス死で自動解放される**ため、PID
ファイルの陳腐化や PID 再利用に伴う誤判定・手動介入の問題（check-then-act 案が抱えて
いた）が消える。`rustix` / `libc` は既に依存ツリーにあり、追加依存は不要である。

**socket を CLI 側で消さない理由**:
[#200](https://github.com/key/the-space-memory/issues/200) の核心は「生存 daemon の
socket を奪うと、別 daemon を黙って壊す」点にある。stale 判定を `tsmd` 側のロック取得後
に一本化し、`cmd_start` と `tsmd` の双方にあった socket 削除経路を `tsmd` のロック後の
一箇所へ集約することで、ロック判定をすり抜けて socket を奪う経路を無くす。

**daemon が `chdir(project_root)` する理由**:
主目的は子プロセスへ継承される CWD を一意化し、相対パス解決の基準を確定させることで
ある。これは Linux / macOS 共通に効く。`/proc/<pid>/cwd` による識別は Linux 限定の
副次効果であり、観測性の主たる根拠は argv（§4）に置く。伝統的な `chdir("/")` は
「カレントディレクトリのアンマウントを妨げない」ためだが、`project_root` が削除される
運用は想定しないため不要である。

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
- 起動シーケンスが単一の `flock` ゲートで直列化され、並行 `tsm start` による daemon の
  多重起動が原理的に起きない。
- [#200](https://github.com/key/the-space-memory/issues/200) の silent socket clobber が
  構造的に解決し、生存 daemon を誤って壊さない。
- `flock` がプロセス死でカーネルにより自動解放されるため、stale PID / PID 再利用に伴う
  誤判定や手動介入が不要になる。
- DB・socket・log の境界がプロジェクト単位で完全に分離され、片方の障害が他方に波及
  しない（SQLite WAL のロック競合も発生しない）。

### Negative

- `tsmd` を直接起動する場合、CWD 直下に `tsm.toml` が無ければ `--project-root` が必要に
  なる。どちらも無い場合は起動失敗する。通常運用は `tsm start` 経由のため影響は限定的
  だが、デバッグ用の直接起動手順は更新が必要である。
- env を escape hatch に格下げするため、グローバル `TSM_STATE_DIR` を前提にしていた
  既存スクリプトは挙動が変わる可能性がある。
- `flock` は **advisory lock** のため、`.tsm/` を共有するすべての `tsmd` 起動経路が
  ロックに参加して初めて排他が成立する。ロックを取らない外部プロセスが socket を squat
  する場合までは守れない。
- `flock` は一部のネットワークファイルシステム（NFS 等）では正しく機能しない。`.tsm/`
  はローカルディスク上にある前提とする。
- 同一の `TSM_STATE_DIR`（ADR-0009 §6 の escape hatch）を 2 つのプロジェクトで共有する
  と、lock・socket・DB の境界も共有され、per-project 分離が黙って壊れる。escape hatch を
  使う場合はプロジェクトごとに別ディレクトリを指す必要がある。
- 親 daemon が異常終了すると、`--no-idle-timeout` の embedder 子プロセスが孤児として
  残存しうる。ロック fd を close-on-exec にすることで孤児は新 daemon の起動をブロック
  しないが（ロックは継承されない）、`embedder.pid` / `embedder.sock` 等の状態を残し、
  次の daemon の子プロセス起動チェックに影響しうる。孤児の刈り取り（親死亡検知）は
  プロセス lifecycle の領域（[ADR-0001](./0001-process-roles-and-responsibilities.md)）で
  あり、`PR_SET_PDEATHSIG` が Linux 限定・macOS は kqueue / `getppid` 監視が要るなど
  クロスプラットフォームで非自明なため、本 ADR のスコープ外の別決定とする。本 ADR が
  確立する per-project な argv identity は、将来その孤児をプロジェクトへ紐付けて刈り取る
  前提を与える。
