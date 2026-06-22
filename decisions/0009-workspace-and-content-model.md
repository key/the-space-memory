# ADR-0009: workspace とコンテンツ参照モデル

- **Status**: **Proposed**
- **Date**: 2026-05-19
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0003](./0003-config-via-resolved-config.md),
  [ADR-0008](./0008-setup-init-separation.md)（partially supersedes）,
  [ADR-0010](./0010-per-project-daemon.md)（per-project daemon — 本 ADR の
  `project_root` を前提とする）

## Context

[ADR-0008](./0008-setup-init-separation.md) により workspace state は
`<workspace>/.tsm/` に集約され、`tsm.db` / `daemon.sock` / `tsmd.pid` / 各種
scaffold ファイル / cache へのリンクが配置される構造になった。
しかし「どこを workspace と認識するか」、そして「workspace のどこを索引対象と
するか（コンテンツの指定方法）」は ADR-0008 のスコープ外で、現状以下の不都合が
残っている。

本 ADR はこの 2 軸 ——
**workspace の境界（どこを workspace とみなすか）**、
**コンテンツの参照（workspace 内外のどこを索引するか）**——
を 1 つの一貫したモデルとして定義する。2 軸はいずれも
「CWD 直下の `tsm.toml` または `--project-root` で確定する `project_root`」を
共通の基準点として共有しており、分離するとかえって基準点の二重定義を招くため、
1 ADR にまとめる。

3 軸目の **プロセスの境界（どの daemon がどの workspace 用か）** は
[ADR-0010](./0010-per-project-daemon.md) で扱う。ADR-0010 は本 ADR が定義する
`project_root` を `tsmd --project-root` として受け取る関係にあり、本 ADR が先に
確定する前提。

加えて、コンテンツ参照の再設計に付随して `content_dirs` の設定スキーマと
DB の `source_file` 表現を破壊変更するため（§3〜§4）、同じ `content_dirs`
スキーマに属する `half_life_days` の意味変更（§5）も同一 migration window
（`rebuild --apply` 必須）に束ねる。`half_life_days = 0` 単体は `project_root`
とは独立した小さな決定だが、別 ADR にすると同じスキーマ・同じ再構築を
2 回に分けることになり、利用者・実装の双方で非効率なため同梱する。

### 課題

1. **workspace の決定ルールが不明確**
   `config::state_dir()` は `DEFAULT_STATE_DIR = ".tsm"` を CWD 相対で返すだけで、
   「どこを workspace とみなすか」「複数 workspace をどう区別するか」の明示的な
   ルールが無い。CWD 次第で `.tsm/` を作る場所が変わり（または XDG global に
   フォールバックし）、どの DB を見ているのかが利用者から追いにくい。
   workspace を一意に確定する明示的な決定ルールが要る

2. **`tsm.toml` の置き場所が見えにくい**
   ADR-0008 line 96 では `tsm.toml (任意)` を `.tsm/` の中に置く設計だが、
   `.tsm/` は機械的な state ディレクトリであり、人が日常的に編集する設定
   ファイルを混在させるとユーザーから可視性が著しく下がる。
   mise（`.mise.toml` / `mise.toml`）/ cargo（`Cargo.toml`）/
   pyproject.toml がいずれも workspace 直下に置く慣習に揃えるべき

3. **索引対象の指定が `index_root` と `content_dirs` の二層で直感的でない**
   `index_root`（既定 `/workspaces`）が索引のルートを定め、`content_dirs` が
   その相対サブディレクトリを列挙する、という二段構えになっている。問題は 3 点：
   - 既定値 `/workspaces` は devcontainer 専用の前提で、ホストや通常環境では
     存在せず、未設定のまま `tsm start` すると**黙って空振り**する
   - 「索引したい場所」を指定するのに `index_root` を変えるのか
     `content_dirs` を足すのか、利用者が迷う（2 経路の意味の重なり）
   - `content_dirs` は `index_root` 相対のみで絶対パス禁止
     （`src/config.rs:392`）。`../shared-notes` のような workspace 外の
     ディレクトリを素直に足せない。一方 `tsm.toml.example` は
     `/workspaces/notes` という絶対パス例を載せており、実装と矛盾している

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

### 2. workspace 探索（`project_root` の決定）

`project_root` は次の優先順で決定する。`tsm` CLI と `tsmd`（daemon / 子
プロセス）で共通のアルゴリズムを使う。

```text
resolve_project_root(cwd, project_root_arg):
    # 1. CWD 直下に tsm.toml があればそれを採用
    if (cwd / "tsm.toml").exists():
        return cwd
    # 2. なければ --project-root 引数を採用
    if project_root_arg is set:
        return project_root_arg     # その直下に tsm.toml がある前提
    # 3. どちらも無ければ起動失敗
    fail("no tsm.toml in CWD and no --project-root; run `tsm init`")
```

- **CWD 直下のみ**を見る（上方向への walk-up はしない）。明示的な CWD か
  `--project-root` のどちらかで workspace を一意に確定させ、「実行場所に
  よって暗黙に親へ遡る」曖昧さを排除する
- `--project-root` は `tsm` / `tsmd` 双方が受け付ける。`tsm start` は解決した
  workspace の絶対パスを `tsmd --project-root <abs>` として明示的に渡し、
  daemon 側でも同じアルゴリズムで `project_root` が確定する
  （[ADR-0010](./0010-per-project-daemon.md)）
- CWD 直下にも `--project-root` にも `tsm.toml` が無ければ **起動失敗**させる
  （黙ってフォールバックしない）

`.tsm/` ディレクトリの存在は marker として用いない（`.tsm/` は
state の置き場であり workspace の意味論的な境界ではないため）。

確定した workspace ディレクトリが、本 ADR を通じての `project_root` ——
`content_dirs` の相対パス解決（§3）と ADR-0010 の `--project-root` の基準点
—— になる。

#### コマンド別の未確定時挙動

`project_root` が確定できなかった（CWD 直下に `tsm.toml` 無し、かつ
`--project-root` 未指定）場合：

| コマンド | 挙動 |
|---|---|
| `tsm init` | CWD 直下に `tsm.toml` + `.tsm/` を作る（ADR-0008 の init 仕様に従う） |
| `tsm start` | エラー「`tsm init` を実行するか `--project-root` を指定してね」 |
| `tsm search` / `status` / `doctor` 等 | エラー「`tsm init` を実行するか `--project-root` を指定してね」 |
| `tsmd`（直接起動） | エラー終了（[ADR-0010](./0010-per-project-daemon.md)）|

### 3. コンテンツ参照は `content_dirs` に一本化（`index_root` 廃止）

`index_root`（設定キー / `TSM_INDEX_ROOT` env / `DEFAULT_INDEX_ROOT = "/workspaces"`）
を**全廃**し、索引対象は `content_dirs` だけで表現する。

```toml
# tsm.toml（<workspace>/ 直下）
[[content_dirs]]
path = "."               # project_root 配下（= workspace 全体）
weight = 1.0
half_life_days = 90

[[content_dirs]]
path = "../shared-notes" # workspace 外の参照ディレクトリも自然に書ける
weight = 0.8
half_life_days = 180
```

- `path` の解決規則：
  - **相対パス**（`.` / `../shared-notes` 等）は **`project_root`（§2 で
    確定する `tsm.toml` のディレクトリ）に join** してから
    canonical 化する。`..` を許可する
  - **絶対パス**は `project_root` に join せず、そのまま canonical 化する
  - これにより「workspace 外を索引したい」用途（オーケストレーションで
    参照する隣接ディレクトリ等）が第一級でサポートされる
- 解決した `path` が存在しない / ディレクトリでない / canonical 化に失敗した
  場合は、**警告ログを出して当該ルートを skip**（起動自体は継続）する。
  現状 `walker.rs` の content_dir 不在時挙動（warn + skip）を踏襲する
- `content_dirs` が**空、または `tsm.toml` に未記載**のときは
  **`[{ path = ".", weight = 1.0, half_life_days = 90 }]`** とみなす
  （= `project_root` 配下を再帰索引）。`/workspaces` 既定による空振りは消える
- 同一ファイルが複数ルートに重複して含まれる場合（例 `.` と `./sub`）は、
  実体パスを canonical 化して**重複排除**し、weight / half_life_days は
  **最長一致するルート**の設定を採用する（最も具体的なルールが勝つ）。
  「最長一致」は**パスコンポーネント境界**で判定する
  （`/foo/bar` は `/foo/barista` のルートに一致しない）
- 起動時に解決後の絶対ルート一覧をログへ出力し、`tsm doctor` でも表示する
  （「黙って空振り」を観測可能にする）

`index_root` の役割はすべて吸収される：

| `index_root` の旧役割 | 引き受け先 |
|---|---|
| `content_dirs` 相対パスの基準点 | `project_root`（§2） |
| 未設定時の auto-discover 起点 | 既定 `content_dirs = [{ path = "." }]` |
| `source_file` の相対保存基準 | §4（絶対パス保存）で不要化 |
| `.gitignore` のアンカー | §4（各 content_dir ルート直下） |
| 「`tsm.toml` と別の場所を索引」用途 | `content_dirs` の絶対 / `..` パス |

中間概念 `index_root` を残すと「`index_root` と `content_dirs` の
どちらで指定するのか」という今回潰す混乱を再導入するため、廃止する。

`tsm.toml` に残存する `index_root` キーは**未知キーとして loud に拒否**する
（serde の `deny_unknown_fields` 相当）。黙殺すると「設定したのに効かない」
という分かりにくい失敗を生むため、起動時に明示エラーで気付かせる。

### 4. `source_file` は正規化済み絶対パスで保存

複数ルートでは単一の相対基準が存在しないため、`source_file` は
**canonical 化した絶対パス**で DB に保存する。

- 検索結果の表示では `$HOME` を `~` に短縮し、可読性を保つ。
  symlink 経由で索引したファイルも canonical 化された実体パスで保存・表示
  される（symlink 名は保持されない）点を許容する
- `tsm search --path <q>` のマッチ規則：
  - 格納された絶対パスを `/` 区切りのコンポーネント列とみなす
  - `q` も `/` で分割し、**連続するコンポーネント列がパスのどこかに
    コンポーネント境界で出現すれば一致**とする（任意位置・部分文字列ではない）
  - 例: `--path daily` も `--path daily/` も
    `/Users/key/work/notes/daily/foo.md` に一致。
    `--path notes/daily` も一致。`--path ily` は**不一致**（境界非整合）
  - 絶対パス（`--path /Users/key/work/notes`）は先頭から一致する特殊形として
    同じ規則で成立する
  - 末尾 slash は有無を問わず同義（区切りとして正規化）
  - 複数 `content_dirs` があっても規則は同一（格納絶対パスにのみ依存し、
    どのルート由来かは問わない）
  - これにより、絶対パス保存にしても従来の相対プレフィックス絞り込み体験を
    維持する
- `.gitignore` は**各 content_dir ルート直下**のものをそれぞれ読む。
  `.tsmignore` は `project_root` 同階層の単一ファイルを維持し、パターンは
  各ルート相対でマッチする
- スキーマ意味が変わるため `rebuild --apply` を必須とする
  （既存の「DB 構造変更は rebuild」運用と同じ）

### 5. `half_life_days = 0` は time decay 無効（無期限）

`half_life_days = 0` を「**時間減衰を適用しない**（decay = 1.0 固定）」の
意味とする。`tsm.toml` の各 `content_dirs` と `claude_session` の両方に適用する。

- 現状 `src/config.rs:405` は `0` を不正値として弾き既定 90 に差し替えるが、
  これを「decay 無効」の sentinel として受理するよう変更する
- `src/searcher.rs:282` の `0.5.powf(days / half_life)` は `half_life == 0`
  だと `days / 0` が `inf`（`days > 0`）または `NaN`（`days == 0`）になり、
  decay が 0 や NaN に化ける（いずれも意図と逆）。これを避けるため、
  `half_life == 0.0` のとき decay = 1.0 を返す**早期 return** を入れる。
  `0` は数学的極限ではなく sentinel として扱う
- 負値 / `inf` / `NaN` は従来どおり不正値として既定 90 に差し替える

「実質無期限にしたければ巨大な有限値を入れる」という非直感的な裏技を
不要にし、`0` で素直に表現できるようにする。

### 6. escape hatch（環境変数の扱い）

| 変数 | 扱い |
|---|---|
| `TSM_CONFIG` | 指定されていれば §2 の CWD / `--project-root` 解決をスキップしてそのファイルを採用。このとき `project_root`（= `content_dirs` 相対解決と ADR-0010 の `--project-root` の基準）は **`TSM_CONFIG` の親ディレクトリ**とする |
| `TSM_STATE_DIR` | 指定されていれば `<workspace>/.tsm/` を上書き |
| `TSM_DAEMON_SOCKET` / `TSM_EMBEDDER_SOCKET` 等 | 既存どおり個別パスを上書き |

`TSM_INDEX_ROOT` は §3 の `index_root` 廃止に伴い**削除**する（escape hatch
としても残さない）。CI / テスト / 一時的な実験のための escape hatch は上記に
限定し、通常運用では env を使わない前提に倒す。

## Rationale

**なぜ `tsm.toml` を root に置く（ADR-0008 を supersede してまで）**:
ADR-0008 の `.tsm/tsm.toml (任意)` は機械的に整合は取れるが、
**人が編集する設定ファイル**を機械的 state と同じ場所に置くと
ユーザーが「設定がそこにある」と気付けない。
mise / cargo / pyproject はいずれも workspace 直下に置く慣習で、
ADR-0008 task #190（`tsm init` 書き換え）が未実装の今なら破壊コストは小さい。

**なぜ walk-up ではなく「CWD 直下 + `--project-root`」にするか**:
walk-up（git/mise 流の祖先探索）は便利だが、「実行場所によって暗黙に
どこまで親へ遡るか」が見えにくく、意図しない上位 `tsm.toml` を拾う事故や、
どの workspace を見ているのか追いにくい曖昧さを生む。明示的に「CWD 直下に
`tsm.toml` があるか、無ければ `--project-root` を渡す」の二択に倒すことで、
workspace が常に一意に確定し、`tsmd` に渡す `project_root`（ADR-0010）とも
同じアルゴリズムで一貫する。サブディレクトリからの利用は `--project-root`
明示で対応でき、暗黙探索による曖昧さを避けられる。

**なぜ `.tsm/` の存在を marker にしないか**:
`.tsm/` は state の置き場であり、空でも `mkdir` 一発で作れる。
意図的でない `.tsm/` が混入したサブディレクトリで誤検知する余地がある。
`tsm.toml` は明示的な設定ファイルであり、誤検知の余地が極めて少ない。
git の `.git/` は単一の管理ディレクトリで重複しないため安全だが、
tsm の `.tsm/` は cache link や DB を持つため作りやすく、同じ感覚で扱えない。

**なぜ `index_root` を廃止し `content_dirs` に一本化するか**:
`index_root` の全役割（相対基準 / auto-discover 起点 / 相対保存基準 /
gitignore アンカー / 別 location 指定）は、`project_root`（§2 で確定）と
`content_dirs`（絶対 / `..` 許可）に過不足なく吸収される。中間概念として
残すと「どちらで索引対象を指定するのか」という 2 経路の混乱が残り、
これは本 ADR が解こうとしている課題そのもの。単一経路に倒すのが直感的。
既定 `/workspaces` も devcontainer 専用の前提で、ホスト実行時の空振りの
原因だったため、既定を「`project_root` 配下（`.`）」に変えて解消する。
`index_root` を deprecated alias として残す案も検討したが、本 ADR は
ADR-0008 の breaking change（state レイアウト変更）と同じ migration window に
乗るため、互換コードを抱える利得が小さい。残すと「2 経路の混乱」も温存される
ので、loud に拒否して書き換えを促す方が結果的に親切と判断した。

**なぜ `source_file` を絶対パスで保存するか**:
複数ルートを許すと、単一の `index_root` 相対という同定基準が成立しない。
ラベル方式（`label/relative`）も検討したが、tsm の DB はそもそも
マシンローカルで `rebuild` により再生成可能な性質であり、移植性のために
ラベル管理（命名 / 一意性検証 / 改名・削除時の孤児処理）のコストを払う
価値は薄い。絶対パスは join 1 段で同定でき実装が単純。表示は `~` 短縮、
`--path` はコンポーネント境界マッチで従来の絞り込み体験を保つ。

**なぜ `half_life_days = 0` を「無期限」にするか**:
従来は `0` を不正値として弾いていたが、time decay を切りたい
（恒久的な参照資料など）ニーズは実在する。巨大な有限値を入れる裏技は
非直感的なので、`0 = decay 無効` という sentinel を正式化する。
`null` / キー省略 / `decay = false` も検討したが、既存の型
（`half_life_days: Option<f64>`）では省略は「既定 90 を使う」の意味で
既に埋まっており、新フラグ追加はスキーマを増やす。`0` は「半減期ゼロ＝
減衰しきる」と誤読される懸念はあるが、本来不正だった値の再利用で
スキーマを増やさず表現でき、`0 = 減衰なし` は時間の単位（日数）として
直感に合うと判断した。

**なぜ ADR-0008 / ADR-0010 と分けるか**:
ADR-0008 は `.tsm/` の **内部構造** と cache / workspace の責務分離を扱う。
ADR-0010 は **プロセス境界の識別**（per-project daemon identity）を扱う。
本 ADR は **workspace の境界の決め方** と **コンテンツの参照モデル** を扱い、
この 2 軸は共通基準点 `project_root` を共有するため 1 ADR に束ねる。
ADR-0010 は本 ADR の `project_root` を消費する依存関係にあるが、判断の中身
（daemon の重複検知・`ps` 可視化）は本 ADR と直交しており、独立して
レビュー / 実装できる。

## Consequences

### Positive

- workspace の決定が「CWD 直下の `tsm.toml` か `--project-root`」の明示的な
  二択に固定され、どの workspace / DB を見ているかが一意で追いやすくなる
  （暗黙の親探索による事故が起きない）
- `tsm.toml` が workspace 直下に出ることで、ユーザーが設定の存在を
  視認しやすくなる
- 索引対象が「`project_root`（境界）＋ `content_dirs`（実体）」の 2 層に
  整理され、`index_root` という中間概念が消えて直感的になる。
  既定 `/workspaces` によるホスト実行時の空振りも解消
- `content_dirs` が絶対 / `..` パスを許すことで、workspace 外の参照
  ディレクトリ（オーケストレーションで参照する隣接 repo 等）を素直に索引できる
- `half_life_days = 0` で time decay を明示的に無効化でき、恒久参照資料の
  スコアが古さで沈まなくなる

### Negative

- **breaking change**: `tsm.toml` を workspace 直下に置く前提に変わるため、
  既存の XDG `state_dir` 配下に DB を持つユーザーは `tsm init` で workspace
  直下に移行する必要がある。自動マイグレーションは提供しない方針
  （ADR-0008 と同じ思想）
- ADR-0008 の `.tsm/tsm.toml (任意)` 仕様を破壊変更で supersede するため、
  `decisions/0008-setup-init-separation.md` 側にも追記が必要
- `tsm init` が必須ステップとして増える。`cd /path && tsm start` 一発では
  動かなくなる（`tsm init` を先に挟む必要）
- walk-up を採用しないため、workspace のサブディレクトリから `tsm` を
  叩くと CWD 直下に `tsm.toml` が無く起動失敗する。サブディレクトリで
  使いたい場合は `--project-root` を明示する必要がある（git/mise 流の
  暗黙の親探索は行わない）
- **breaking change**: `source_file` を絶対パス保存に変えるためスキーマ意味が
  変わり、`rebuild --apply` が必須。絶対パス保存は DB がマシン依存になり、
  workspace を別パスへ移動すると既存行が stale になる（`rebuild` で再生成）
- `index_root` / `TSM_INDEX_ROOT` を撤廃するため、これらを設定していた
  既存ユーザーは `content_dirs` への書き換えが必要。残存 `index_root` キーは
  起動時エラーになる
- `--path` の一致規則がコンポーネント境界マッチに変わるため、ごく稀に
  従来と異なるヒット集合になる可能性（中間ディレクトリ名での一致など）

### Follow-ups

- **ADR-0008 への追記**: `decisions/0008-setup-init-separation.md` の
  workspace state 構造図と `tsm init` 仕様で `tsm.toml` の位置を更新する
  PR を本 ADR と同時にマージ。ADR-0008 task #190 がまだ未実装のため
  実装コンフリクトは無い
- **Umbrella issue を作成**、以下のタスク粒度でサブ issue 化：
  1. `config.rs` に `resolve_project_root(cwd, project_root_arg)`
     （CWD 直下 `tsm.toml` → `--project-root` → 失敗）を実装。
     `tsm` / `tsmd` 双方が `--project-root` 引数を受ける
  2. `tsm init` 拡張：`<workspace>/tsm.toml` 雛形生成、`.tsm/.gitignore`
     （`*` のみ）作成（ADR-0008 task #190 の中で対応するか別タスクにするか調整）
  3. `index_root` 撤廃：`config.rs` から `index_root` / `DEFAULT_INDEX_ROOT` /
     `TSM_INDEX_ROOT` を削除し、`content_dirs` 解決基準を `project_root` に
     変更。残存 `index_root` キーは未知キーとして拒否
  4. `content_dirs` の絶対 / `..` パス対応。`walker.rs` の
     `index_root.join()` 基準を `project_root` 基準へ。未設定時の既定
     `[{ path = "." }]`、重複ルートの canonical 重複排除と最長一致 weight 解決
  5. `source_file` 絶対パス保存：`walker.rs` の strip_prefix 撤廃、
     `searcher.rs` / `cli.rs` のパス復元を絶対パス前提に。表示の `~` 短縮、
     `--path` のコンポーネント境界マッチ実装
  6. gitignore を各 content_dir ルート直下から読むよう `walker.rs` 変更
  7. `half_life_days = 0` を decay 無効 sentinel として受理
     （`config.rs` 検証、`searcher.rs` 早期 return）
  8. ドキュメント更新（README / README.ja / CLAUDE.md /
     `docs/configuration.md` / `tsm.toml.example`。
     `tsm.toml.example` の絶対パス例の矛盾も修正）
- **ADR-0008 タスクとの順序調整**: task #190（`tsm init` 書き換え）と
  本 ADR の task 2 が同一コマンドを触るため、どちらかの ADR の中で
  まとめて実装するか、順序を明示する
- **`.gitignore` への `.tsm/` 自動追加**: `tsm init` 実行時に workspace
  直下の `.gitignore` へ `.tsm/` を追記するか否か。デフォルト on で
  `--no-gitignore` フラグで opt-out が妥当か、別途検討
