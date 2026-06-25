---
status: accepted
created: 2026-06-25
updated: 2026-06-25
---

# ADR-0017: source_file の絶対パス保存と --path フィルタの意味論

- **Deciders**: key
- **Related**:
  [ADR-0009](./0009-workspace-and-content-model.md)（§4 の source_file 保存方式・
  表示・`--path` マッチ規則を本 ADR で supersede する。§4 の gitignore /
  `.tsmignore` の扱い、および §1〜3,5,6 は ADR-0009 のまま有効）,
  [ADR-0007](./0007-pipeline-stages.md)（Search パイプラインの段構成を前提とする）

## Context

[ADR-0009](./0009-workspace-and-content-model.md) §3 で `index_root` を廃止し
（実装は #229）、コンテンツは `project_root` + `content_dirs` で参照する。
このとき §4（`source_file` を絶対パスで保存する）は deferred とされ、
`documents.file_path` は現在 **`project_root` 相対**で保存されている
（`indexer/mod.rs` が `strip_prefix(project_root)` する）。`project_root` の外を
指す `..` / 絶対 `content_dir` のファイルは strip に失敗し、**絶対パスのまま**
保存される。結果として同一カラムに「相対」と「絶対」の 2 種類の意味が混在する。

この混在は `--path` を multi-context host（オーケストレーション: 1 つの索引ツリーの
下に複数 repo / worktree が並び、各呼び出しコンテキストがサブツリーへ結果を絞りたい）
向けの信頼できるフィルタにするうえで支障になる。

- **同一性が可変なコンテキストに縛られる**。`file_path` はファイルの唯一の同定キー
  なのに、`project_root`（CWD 直下の `tsm.toml`、ADR-0009 §2）に依存する。
  `tsm.toml` の位置・起動 CWD・`content_dir` 境界が変わると、同じ実ファイルが別の
  `file_path` になる。
- **`--path` の基準が定義できない**。相対と絶対が混在する以上、両方に一致する
  正規化を定義できない。呼び出し側の `../sibling` は `LIKE '../sibling%'` に束縛され、
  DB の何にも一致しない。
- **狭い scope が痩せる**。path フィルタが最終 rank JOIN でしか効かず
  （`searcher/mod.rs` は `path_prefixes` を `rank` にのみ渡し `retrieve` には渡さない）、
  各 retrieval source（FTS5 / vector / entity）が scope 非対応のまま `top_k*3` 候補を
  集めるため、scope 外チャンクが rank 枠を消費し、scope 内に十分な候補があっても
  `top_k` 未満しか返らないことがある。フィルタが正確であるほど痩せが悪化する。

ADR-0009 §4 は絶対パス保存を提案していたが、その正規化方式（canonical 化）・表示
（`~` 短縮）・`--path` マッチ規則（floating component match）は、オーケストレーション
用途を主眼に再検討した結果いずれも見直す。本 ADR がその確定版を定義する。

## Decision

### 1. source_file は lexical な絶対パスで保存する（symlink 非解決）

`documents.file_path` を絶対パスで保存する。絶対化は **lexical**（字句的）に行い、
**symlink は実体へ解決しない**。

- 相対入力は CWD を前置し、`.` / `..` を字句的に解決する。`fs::canonicalize` は
  使わない。`current_dir()` は OS が既に正規化して返すため、これで基準点は安定する。
- `content_dir` の root は基準を安定させるため一度だけ canonical 化してよいが、その
  配下のファイルは lexical に join する。
- `indexer/mod.rs` の `strip_prefix(project_root)` は撤廃する。絶対化は常に絶対パスを
  返すため、相対 / 絶対が混在する状態は解消する。

### 2. 保存は絶対、表示はフォーマットで分ける

保存値は絶対パスとし、出力時に畳む。

- **text 出力**: CWD 相対に畳む（人間が今いる場所から読む）。
- **JSON 出力**: 絶対パスのまま（機械・オーケストレーションが曖昧さなく解決できる。
  受け手は送り手の CWD を知らないため、相対では復元できない）。

### 3. --path は CWD アンカーの境界一致フィルタ

- **入力**: 絶対パス **または** CWD 相対パスのいずれも受ける。
- **正規化**: CWD 相対は CWD を前置して絶対化し、`.` / `..` を字句的に解決する
  （§1 と同じ規則）。末尾 `/` を除去し、複数指定は重複排除する。
- **マッチ**: アンカーされたディレクトリ境界一致。
  `d.file_path = <abs> OR d.file_path LIKE <abs> || '/%'`。
  `--path <abs>/daily` は `<abs>/daily/notes/x.md` に一致し、`<abs>/daily-report/...`
  には一致しない。LIKE メタ文字（`%` `_` `\`）はエスケープする。
- **範囲チェックは設けない**。`index_root` という囲いが無くなったため、どの
  `content_dir` にも属さないパスや、何にも一致しないパスは **エラーではなく 0 件**を
  返す。複数 `content_dir` を跨いだ絞り込みも自然に成立する。
- **残すエラーは空文字のみ**。bare `.` は CWD に解決される（≒ フィルタなし相当）。

### 4. --path を retrieval 段の各 source に適用する

`searcher/mod.rs` は `path_prefixes` を `retrieve` にも渡す。FTS5・vector・entity の
各候補取得に §3 の境界一致を適用し（entity の 2nd-hop 展開を含む）、各 source が
scope 内で `top_k*3` 候補を確保する。最終 JOIN のフィルタは二重の保険として残す。

### 5. content_dir の weight / half_life マッチも絶対基準・境界一致に揃える

`config.rs` の `directory_weight` / `half_life_days` のマッチ
（現状 `file_path.starts_with(dir.path)`）は、`dir.path` を §1 の規則で絶対化し、
境界一致（`= dir_abs OR starts_with(dir_abs + "/")`）に変更する。単純な
`starts_with` では `/x/daily` が `/x/daily-report` に一致する漏れがあるため、
§3 と同じ境界規則に揃える。

### 6. canonical dedup は行わない

lexical 絶対化（§1）の帰結として、字句的に同一の絶対パスのみを UNIQUE 制約で 1
ドキュメントに収束させる。同じ実ファイルへ symlink 経由の別パスから到達した場合は
別ドキュメントとして扱い、検出しない。複数ルートの重複回避は `content_dir` 設定の
責務とする。

## Rationale

**なぜ canonical でなく lexical（§1）か**:
オーケストレーションで効くのは「呼び出し側が書くパス表記（bind mount 下の
`/workspaces/...` 等）と、保存値・`--path` が文字列として一致すること」である。
`canonicalize` は symlink を実体（`/private/var/...` 等）へ解決してしまい、ユーザーが
書く `--path /workspaces/...` とズレて一致しなくなる。実ファイル I/O も増える。
ADR-0009 §4 が挙げた「symlink 重複の dedup」はレアケースであり、その利得のために
canonical 化のコスト（表記の不一致・I/O）を払う価値は薄い。基準点（CWD・content_dir
root）は OS 正規化または一度の canonical で十分安定する。

**なぜ CWD アンカーで、floating component match（ADR-0009 §4 旧案）でないか**:
§4 旧案は「`--path daily` がパス中のどこに `daily` が現れても一致」する CWD 非依存の
モデルだった。これは「どこの `daily` でも拾う」には便利だが、オーケストレーションの
主要素である「**今いる場所**を基準に絞る」「**隣の repo**（`../sibling`）を指す」を
表現できない。各エージェントの CWD が異なる前提では、CWD アンカーの方が「ここ」と
「ここの隣」を素直に書ける。`--path` は CLI 側で CWD を使って絶対化してから daemon に
渡すため、daemon が CWD を知らなくても成立する。

**なぜ retrieval 段に押し下げるか（§4）**:
最終 JOIN だけでフィルタすると、各 source が scope 非対応で `top_k*3` を埋め、scope
内候補が枠を奪われて結果が痩せる。`--path` を正確に効かせる（§3 の境界一致）ほど
症状は悪化し、オーケストレーションの常用パターン（狭い scope）を直撃する。各 source
が scope 内で候補を確保すれば、絞り込みと件数を両立できる。

**なぜ ADR-0009 §4 を全面採用せず supersede するか**:
§4 の「絶対パス保存」「`rebuild` 必須」「複数ルートで規則一様」という骨子は維持する。
変更するのは正規化方式（canonical → lexical）・表示（`~` 短縮 → text 相対 / JSON
絶対）・`--path` マッチ（floating → CWD アンカー境界一致）と、retrieval 段への適用
（§4 には無い）である。§4 の gitignore / `.tsmignore` の扱いは本 ADR の対象外として
ADR-0009 に残す。

## Consequences

### Positive

- `file_path` が単一の意味（絶対パス）になり、ファイルの同定が `project_root` という
  可変コンテキストから独立する。`tsm.toml` の移動・起動 CWD・content_dir 境界に
  左右されない。
- `--path` が絶対 / CWD 相対の両方を受け、CWD アンカーで境界一致するため、
  オーケストレーションの各コンテキストが「ここ」「隣の repo」を確実に scope できる。
  複数 `content_dir` 跨ぎの絞り込みも自然に成立する。
- path フィルタを retrieval 段で効かせるため、狭い scope でも各 source から `top_k`
  分の scope 内候補が確保され、絞るほど結果が痩せる問題が解消する。
- text は CWD 相対で可読、JSON は絶対で機械が曖昧さなく解決できる。

### Negative

- **breaking change**: `file_path` の意味が変わるため `rebuild` が必須になる。絶対パス
  保存は DB がマシン依存になり、プロジェクトを別パスへ移すと既存行が stale になる
  （`rebuild` で再生成する。DB は元々ローカルな再生成可能アーティファクト）。
- symlink 経由の同一ファイルは別ドキュメントとして重複しうる（§6）。重複回避は
  `content_dir` 設定の責務に倒れる。ADR-0009 §4 の canonical dedup は行わない。
- `--path` が CWD 依存になるため、同じ `--path daily` でも実行場所により結果集合が
  変わる。CWD 非依存の floating match（§4 旧案）の挙動とは異なる。
- `--path` のマッチがコンポーネント境界一致に変わるため、従来の素朴な前方一致
  （`daily` が `daily-report` に一致）とはヒット集合が変わる。
