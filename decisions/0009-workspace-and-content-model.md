# ADR-0009: プロジェクトとコンテンツ参照モデル

- **Status**: **Proposed**
- **Date**: 2026-05-19
- **Deciders**: key
- **Related**:
  [ADR-0001](./0001-process-roles-and-responsibilities.md),
  [ADR-0003](./0003-config-via-resolved-config.md),
  [ADR-0008](./0008-setup-init-separation.md)（partially supersedes）,
  [ADR-0010](./0010-per-project-daemon.md)（本 ADR の `project_root` を前提とする）

## Context

ここで言う**プロジェクト**とは、`tsm.toml` を持ち `.tsm/` 配下に DB などの
状態を置く 1 つのまとまりを指す。その最上位ディレクトリを**プロジェクトルート**
（コード上は `project_root`）と呼ぶ。

[ADR-0008](./0008-setup-init-separation.md) により、プロジェクトの状態は
`<project-root>/.tsm/` に集約され、`tsm.db` / `daemon.sock` / `tsmd.pid` /
各種 scaffold ファイル / cache へのリンクが配置される構造になった。
ただし「どこをプロジェクトルートと認識するか」と「プロジェクト内外のどこを
索引対象とするか」は ADR-0008 のスコープ外であり、以下の不都合が残っている。

本 ADR は次の 2 軸を 1 つの一貫したモデルとして定義する。

1. **プロジェクトの境界** — どのディレクトリをプロジェクトルートとみなすか
2. **コンテンツの参照** — プロジェクト内外のどこを索引するか

この 2 軸は「CWD 直下の `tsm.toml` または `--project-root` で確定する
`project_root`」を共通の基準点として共有する。分離すると基準点を二重に定義する
ことになるため、1 ADR にまとめる。

3 軸目の**プロセスの境界**（どの daemon がどのプロジェクト用か）は
[ADR-0010](./0010-per-project-daemon.md) で扱う。ADR-0010 は本 ADR が定義する
`project_root` を `tsmd --project-root` として受け取るため、本 ADR が先に確定する。

また §3〜§4 では、`content_dirs` の設定スキーマと DB の `source_file` 表現を
破壊変更する。このため、同じ `content_dirs` スキーマに属する `half_life_days` の
意味変更（§5）も同じ migration window に含める。`half_life_days = 0` は
`project_root` とは独立した小さな決定だが、別 ADR に分けると同じスキーマ変更と
`rebuild --apply` を二度扱うことになり、利用者・実装の双方で非効率なため同梱する。

### 課題

1. **プロジェクトルートの決定ルールが不明確**
   `config::state_dir()` は `DEFAULT_STATE_DIR = ".tsm"` を CWD 相対で返すだけで、
   「どこをプロジェクトルートとみなすか」「複数のプロジェクトをどう区別するか」の
   明示的なルールが無い。CWD 次第で `.tsm/` を作る場所が変わり（または XDG global
   にフォールバックし）、どの DB を見ているのかを利用者が追いにくい。

2. **`tsm.toml` の置き場所が見えにくい**
   ADR-0008 line 96 では `tsm.toml (任意)` を `.tsm/` 内に置く設計だが、`.tsm/` は
   機械的な状態ディレクトリである。人が日常的に編集する設定ファイルをそこに混在
   させると、可視性が著しく下がる。mise（`.mise.toml` / `mise.toml`）、
   cargo（`Cargo.toml`）、pyproject はいずれもプロジェクトルート直下に置く慣習で、
   これに揃えるべきである。

3. **索引対象の指定が `index_root` と `content_dirs` の二層で直感的でない**
   `index_root`（既定 `/workspaces`）が索引のルートを定め、`content_dirs` がその
   相対サブディレクトリを列挙する二段構えになっている。問題は次の 3 点である。
   - 既定値 `/workspaces` は devcontainer 専用の前提であり、ホストや通常環境では
     存在しない。未設定のまま `tsm start` すると黙って空振りする。
   - 索引したい場所を指定するのに `index_root` を変えるのか `content_dirs` を足すのか、
     利用者が迷う（2 経路の意味が重なる）。
   - `content_dirs` は `index_root` 相対のみで絶対パスを許さない
     （`src/config.rs:392`）。`../shared-notes` のようなプロジェクト外のディレクトリを
     素直に足せない。一方 `tsm.toml.example` は `/workspaces/notes` という絶対パス例を
     載せており、実装と矛盾している。

## Decision

### 1. プロジェクトルートの marker は `<project-root>/tsm.toml`

**ADR-0008 line 96 の `.tsm/tsm.toml (任意)` 記述を本 ADR で supersede する。**

設定ファイル `tsm.toml` はプロジェクトルート直下に置く。`.tsm/` 内には置かない。

```text
<project-root>/
├── tsm.toml          ← プロジェクトルートの marker。人が編集し、git に入れる
└── .tsm/             ← ADR-0008 で定義された状態一式（git ignore）
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

### 2. プロジェクトルート（`project_root`）の決定

`project_root` は次の優先順で決定する。`tsm` CLI と `tsmd`（daemon・子プロセス）で
共通のアルゴリズムを使う。

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

- **CWD 直下のみ**を見る。上方向への walk-up は行わない。明示的な CWD か
  `--project-root` のいずれかでプロジェクトルートを一意に確定させ、実行場所に
  よって暗黙に親へ遡る曖昧さを排除する。
- `--project-root` は `tsm` と `tsmd` の双方が受け付ける。`tsm start` は解決した
  プロジェクトルートの絶対パスを `tsmd --project-root <abs>` として明示的に渡し、
  daemon 側でも同じアルゴリズムで `project_root` が確定する
  （[ADR-0010](./0010-per-project-daemon.md)）。
- CWD 直下にも `--project-root` にも `tsm.toml` が無ければ起動失敗とする
  （黙ってフォールバックしない）。

`.tsm/` ディレクトリの存在は marker として用いない。`.tsm/` は状態の置き場であり、
プロジェクトの意味論的な境界ではないためである。

#### `project_root` が確定できない場合の挙動

CWD 直下に `tsm.toml` が無く、かつ `--project-root` も未指定の場合：

| コマンド | 挙動 |
|---|---|
| `tsm init` | CWD 直下に `tsm.toml` と `.tsm/` を作成する（ADR-0008 の init 仕様に従う）|
| `tsm start` | `tsm init` の実行または `--project-root` の指定を促し、エラーで終了する |
| `tsm search` / `status` / `doctor` 等 | 同上のエラーで終了する |
| `tsmd`（直接起動）| エラーで終了する（[ADR-0010](./0010-per-project-daemon.md)）|

### 3. コンテンツ参照は `content_dirs` に一本化（`index_root` 廃止）

`index_root`（設定キー、`TSM_INDEX_ROOT` env、`DEFAULT_INDEX_ROOT = "/workspaces"`）
を全廃し、索引対象は `content_dirs` だけで表現する。

```toml
# tsm.toml（プロジェクトルート直下）
[[content_dirs]]
path = "."               # project_root 配下（プロジェクト全体）
weight = 1.0
half_life_days = 90

[[content_dirs]]
path = "../shared-notes" # プロジェクト外の参照ディレクトリも自然に書ける
weight = 0.8
half_life_days = 180
```

`path` の解決規則は次のとおり。

- **相対パス**（`.` / `../shared-notes` 等）は `project_root`（§2 で確定する
  `tsm.toml` のディレクトリ）に join してから canonical 化する。`..` を許可する。
- **絶対パス**は `project_root` に join せず、そのまま canonical 化する。
- これにより、プロジェクト外のディレクトリ（オーケストレーションで参照する隣接
  ディレクトリ等）の索引が第一級でサポートされる。

その他の規則：

- 解決した `path` が存在しない、ディレクトリでない、または canonical 化に失敗した
  場合は、警告ログを出して当該ルートを skip する（起動自体は継続する）。現状
  `walker.rs` の content_dir 不在時の挙動（warn + skip）を踏襲する。
- `content_dirs` が空、または `tsm.toml` に未記載のときは
  `[{ path = ".", weight = 1.0, half_life_days = 90 }]` とみなす（`project_root`
  配下を再帰索引する）。`/workspaces` 既定による空振りはこれで解消する。
- 同一ファイルが複数ルートに重複して含まれる場合（例: `.` と `./sub`）は、実体
  パスを canonical 化して重複排除する。weight と half_life_days は最長一致する
  ルートの設定を採用する（最も具体的なルールが勝つ）。最長一致はパスコンポーネント
  境界で判定する（`/foo/bar` は `/foo/barista` のルートに一致しない）。
- 起動時に解決後の絶対ルート一覧をログへ出力し、`tsm doctor` でも表示する
  （空振りを観測可能にする）。

`index_root` の役割はすべて他の概念に吸収される。

| `index_root` の旧役割 | 引き受け先 |
|---|---|
| `content_dirs` 相対パスの基準点 | `project_root`（§2）|
| 未設定時の auto-discover 起点 | 既定 `content_dirs = [{ path = "." }]` |
| `source_file` の相対保存基準 | §4（絶対パス保存）で不要化 |
| `.gitignore` のアンカー | §4（各 content_dir ルート直下）|
| 「`tsm.toml` と別の場所を索引」する用途 | `content_dirs` の絶対 / `..` パス |

中間概念 `index_root` を残すと「`index_root` と `content_dirs` のどちらで指定するか」
という、本 ADR が解こうとしている混乱を再導入する。したがって廃止する。

`tsm.toml` に残存する `index_root` キーは、未知キーとして明示的に拒否する
（serde の `deny_unknown_fields` 相当）。黙殺すると「設定したのに効かない」という
分かりにくい失敗を生むため、起動時にエラーで気付かせる。

### 4. `source_file` は正規化済み絶対パスで保存

複数ルートでは単一の相対基準が存在しないため、`source_file` は canonical 化した
絶対パスで DB に保存する。

- 検索結果の表示では `$HOME` を `~` に短縮し、可読性を保つ。symlink 経由で索引した
  ファイルも canonical 化された実体パスで保存・表示される（symlink 名は保持しない）。
- `.gitignore` は各 content_dir ルート直下のものをそれぞれ読む。`.tsmignore` は
  `project_root` 同階層の単一ファイルを維持し、パターンは各ルート相対でマッチする。
- スキーマの意味が変わるため `rebuild --apply` を必須とする（既存の「DB 構造変更は
  rebuild」運用と同じ）。

`tsm search --path <q>` のマッチ規則は次のとおり。

- 格納された絶対パスを `/` 区切りのコンポーネント列とみなす。
- `q` も `/` で分割し、連続するコンポーネント列がパスのどこかにコンポーネント境界で
  出現すれば一致とする（任意位置の部分文字列一致ではない）。
- 例: `--path daily` と `--path daily/` はいずれも
  `/Users/key/work/notes/daily/foo.md` に一致する。`--path notes/daily` も一致する。
  `--path ily` は一致しない（境界が合わない）。
- 絶対パス（`--path /Users/key/work/notes`）は先頭から一致する特殊形として、同じ
  規則で成立する。
- 末尾 slash は有無を問わず同義とする（区切りとして正規化する）。
- 複数の `content_dirs` があっても規則は同一である（格納絶対パスにのみ依存し、どの
  ルート由来かは問わない）。

この規則により、絶対パス保存でも従来の相対プレフィックス絞り込みの体験を維持する。

### 5. `half_life_days = 0` は time decay 無効（無期限）

`half_life_days = 0` を「時間減衰を適用しない（decay を 1.0 に固定する）」の意味と
する。`tsm.toml` の各 `content_dirs` と `claude_session` の両方に適用する。

- `src/config.rs:405` は現状 `0` を不正値として弾き既定 90 に差し替えるが、これを
  「decay 無効」の sentinel として受理するよう変更する。
- `src/searcher.rs:282` には、`half_life == 0.0` のとき decay = 1.0 を返す早期 return
  を入れる。現状の `0.5.powf(days / half_life)` は `half_life == 0` だと `days / 0`
  が `inf`（`days > 0`）または `NaN`（`days == 0`）になり、decay が 0 や NaN に
  化けるためである（いずれも意図と逆）。`0` は数学的極限ではなく sentinel として扱う。
- 負値・`inf`・`NaN` は従来どおり不正値として既定 90 に差し替える。

これにより、「実質無期限にするために巨大な有限値を入れる」という非直感的な裏技が
不要になり、`0` で素直に表現できる。

### 6. escape hatch（環境変数の扱い）

| 変数 | 扱い |
|---|---|
| `TSM_CONFIG` | 指定時は §2 の CWD / `--project-root` 解決をスキップし、そのファイルを採用する |
| `TSM_STATE_DIR` | 指定時は `<project-root>/.tsm/` を上書きする |
| `TSM_DAEMON_SOCKET` / `TSM_EMBEDDER_SOCKET` 等 | 既存どおり個別パスを上書きする |

`TSM_CONFIG` を指定した場合、`project_root` は `TSM_CONFIG` の親ディレクトリとする
（`content_dirs` の相対解決と ADR-0010 の `--project-root` の基準点になる）。

`TSM_INDEX_ROOT` は §3 の `index_root` 廃止に伴い削除する（escape hatch としても
残さない）。escape hatch は上記に限定し、通常運用では env を使わない前提に倒す
（CI・テスト・一時的な実験のための逃げ道とする）。

## Rationale

**`tsm.toml` をプロジェクトルート直下に置く理由（ADR-0008 を supersede してまで）**:
ADR-0008 の `.tsm/tsm.toml (任意)` でも機械的な整合は取れるが、人が編集する設定
ファイルを機械的な状態と同じ場所に置くと、利用者が設定の存在に気付けない。
mise・cargo・pyproject はいずれもプロジェクトルート直下に置く慣習であり、ADR-0008
task #190（`tsm init` の書き換え）が未実装の今なら破壊コストは小さい。

**walk-up ではなく「CWD 直下 + `--project-root`」にする理由**:
walk-up（git / mise 流の祖先探索）は便利だが、実行場所によってどこまで親へ遡るかが
見えにくい。意図しない上位の `tsm.toml` を拾う事故や、どのプロジェクトを見ているか
追いにくい曖昧さを生む。

そこで「CWD 直下に `tsm.toml` があればそれ、無ければ `--project-root`」の二択に倒す。
これによりプロジェクトルートが常に一意に確定し、`tsmd` に渡す `project_root`
（ADR-0010）とも同じアルゴリズムで一貫する。

サブディレクトリからの利用は `--project-root` の明示で対応する。暗黙の親探索による
曖昧さを避けることを優先する。

**`index_root` を廃止し `content_dirs` に一本化する理由**:
`index_root` の役割は、`project_root`（§2 で確定）と `content_dirs`（絶対 / `..`
許可）に過不足なく吸収される。具体的には次のとおり。

- 相対パスの基準点 → `project_root`
- auto-discover の起点 → 既定 `content_dirs = [{ path = "." }]`
- 相対保存の基準 → §4 の絶対パス保存で不要化
- gitignore のアンカー → 各 content_dir ルート直下
- 別ロケーションの指定 → `content_dirs` の絶対 / `..` パス

中間概念として残すと「どちらで索引対象を指定するのか」という 2 経路の混乱が残り、
これは本 ADR が解こうとしている課題そのものである。単一経路に倒す方が直感的である。
既定 `/workspaces` も devcontainer 専用の前提でホスト実行時の空振りの原因だったため、
既定を `project_root` 配下（`.`）に変えて解消する。

`index_root` を deprecated alias として残す案も検討した。しかし本 ADR は ADR-0008 の
破壊変更（状態レイアウトの変更）と同じ migration window に乗るため、互換コードを
抱える利得が小さい。残すと 2 経路の混乱も温存される。明示的に拒否して書き換えを
促す方が結果的に親切と判断した。

**`source_file` を絶対パスで保存する理由**:
複数ルートを許すと、単一の `index_root` 相対という同定基準が成立しない。ラベル方式
（`label/relative`）も検討したが、tsm の DB はマシンローカルで `rebuild` により
再生成できる性質であり、移植性のためにラベル管理（命名・一意性検証・改名や削除時の
孤児処理）のコストを払う価値は薄い。絶対パスは join 1 段で同定でき、実装が単純である。
表示は `~` 短縮、`--path` はコンポーネント境界マッチで従来の絞り込み体験を保つ。

**`half_life_days = 0` を「無期限」にする理由**:
従来は `0` を不正値として弾いていたが、time decay を切りたいニーズ（恒久的な参照
資料など）は実在する。巨大な有限値を入れる裏技は非直感的なので、`0 = decay 無効`
という sentinel を正式化する。`null`・キー省略・`decay = false` も検討したが、既存の
型（`half_life_days: Option<f64>`）では省略は「既定 90 を使う」の意味で既に埋まって
おり、新フラグの追加はスキーマを増やす。`0` は「半減期ゼロ＝減衰しきる」と誤読される
懸念はあるが、本来不正だった値の再利用でスキーマを増やさず表現でき、時間の単位
（日数）として `0 = 減衰なし` は直感に合うと判断した。

**ADR-0008 / ADR-0010 と分ける理由**:
ADR-0008 は `.tsm/` の内部構造と cache・プロジェクトの責務分離を扱う。ADR-0010 は
プロセス境界の識別（per-project daemon identity）を扱う。本 ADR はプロジェクトの境界の
決め方とコンテンツの参照モデルを扱い、この 2 軸は共通基準点 `project_root` を共有する
ため 1 ADR に束ねる。ADR-0010 は本 ADR の `project_root` を消費する依存関係にあるが、
判断の中身（daemon の重複検知・`ps` 可視化）は本 ADR と直交しており、独立してレビュー・
実装できる。

## Consequences

### Positive

- プロジェクトルートの決定が「CWD 直下の `tsm.toml` か `--project-root`」の明示的な
  二択に固定され、どのプロジェクト・どの DB を見ているかが一意で追いやすくなる
  （暗黙の親探索による事故が起きない）。
- `tsm.toml` がプロジェクトルート直下に出ることで、利用者が設定の存在を視認しやすくなる。
- 索引対象が「`project_root`（境界）と `content_dirs`（実体）」の 2 層に整理され、
  `index_root` という中間概念が消えて直感的になる。既定 `/workspaces` によるホスト
  実行時の空振りも解消する。
- `content_dirs` が絶対 / `..` パスを許すことで、プロジェクト外の参照ディレクトリ
  （オーケストレーションで参照する隣接 repo 等）を素直に索引できる。
- `half_life_days = 0` で time decay を明示的に無効化でき、恒久参照資料のスコアが
  古さで沈まなくなる。

### Negative

- **breaking change**: `tsm.toml` をプロジェクトルート直下に置く前提に変わるため、
  既存の XDG `state_dir` 配下に DB を持つ利用者は `tsm init` でプロジェクトルート直下に
  移行する必要がある。自動マイグレーションは提供しない（ADR-0008 と同じ思想）。
- ADR-0008 の `.tsm/tsm.toml (任意)` 仕様を破壊変更で supersede するため、
  `decisions/0008-setup-init-separation.md` 側にも追記が必要になる。
- `tsm init` が必須ステップとして増える。`cd /path && tsm start` の一発では動かなく
  なり、先に `tsm init` を挟む必要がある。
- walk-up を採用しないため、プロジェクトのサブディレクトリから `tsm` を実行すると
  CWD 直下に `tsm.toml` が無く起動失敗する。サブディレクトリで使う場合は
  `--project-root` を明示する必要がある。
- **breaking change**: `source_file` を絶対パス保存に変えるためスキーマの意味が変わり、
  `rebuild --apply` が必須になる。絶対パス保存は DB がマシン依存になり、プロジェクトを
  別パスへ移動すると既存行が stale になる（`rebuild` で再生成する）。
- `index_root` / `TSM_INDEX_ROOT` を撤廃するため、これらを設定していた利用者は
  `content_dirs` への書き換えが必要になる。残存する `index_root` キーは起動時エラーに
  なる。
- `--path` の一致規則がコンポーネント境界マッチに変わるため、ごく稀に従来と異なる
  ヒット集合になる可能性がある（中間ディレクトリ名での一致など）。

### Follow-ups

- **ADR-0008 への追記**: `decisions/0008-setup-init-separation.md` のプロジェクト状態
  構造図と `tsm init` 仕様で `tsm.toml` の位置を更新する PR を本 ADR と同時にマージする。
  ADR-0008 task #190 が未実装のため実装コンフリクトは無い。
- **Umbrella issue を作成**し、以下のタスク粒度でサブ issue 化する。
  1. `config.rs` に `resolve_project_root(cwd, project_root_arg)`（CWD 直下 `tsm.toml`
     → `--project-root` → 失敗）を実装する。`tsm` / `tsmd` 双方が `--project-root`
     引数を受ける。
  2. `tsm init` を拡張し、`<project-root>/tsm.toml` の雛形生成と `.tsm/.gitignore`
     （`*` のみ）の作成を行う（ADR-0008 task #190 の中で対応するか別タスクにするかを調整）。
  3. `index_root` を撤廃する。`config.rs` から `index_root` / `DEFAULT_INDEX_ROOT` /
     `TSM_INDEX_ROOT` を削除し、`content_dirs` の解決基準を `project_root` に変更する。
     残存する `index_root` キーは未知キーとして拒否する。
  4. `content_dirs` の絶対 / `..` パスに対応する。`walker.rs` の `index_root.join()`
     基準を `project_root` 基準へ変更し、未設定時の既定 `[{ path = "." }]`、重複ルートの
     canonical 重複排除と最長一致 weight 解決を実装する。
  5. `source_file` を絶対パス保存にする。`walker.rs` の strip_prefix を撤廃し、
     `searcher.rs` / `cli.rs` のパス復元を絶対パス前提にする。表示の `~` 短縮と、
     `--path` のコンポーネント境界マッチを実装する。
  6. gitignore を各 content_dir ルート直下から読むよう `walker.rs` を変更する。
  7. `half_life_days = 0` を decay 無効の sentinel として受理する（`config.rs` の検証、
     `searcher.rs` の早期 return）。
  8. ドキュメントを更新する（README / README.ja / CLAUDE.md / `docs/configuration.md` /
     `tsm.toml.example`。`tsm.toml.example` の絶対パス例の矛盾も修正する）。
- **ADR-0008 タスクとの順序調整**: task #190（`tsm init` の書き換え）と本 ADR の
  task 2 は同一コマンドを触るため、どちらかの ADR の中でまとめて実装するか、順序を
  明示する。
- **`.gitignore` への `.tsm/` 自動追加**: `tsm init` 実行時にプロジェクトルート直下の
  `.gitignore` へ `.tsm/` を追記するか否かを別途検討する。デフォルト on で
  `--no-gitignore` による opt-out が妥当かを判断する。
