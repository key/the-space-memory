---
status: accepted
created: 2026-06-23
updated: 2026-06-23
---

# ADR-0015: DB 接続の読み書き分離と書き込み公平化

- **Deciders**: key
- **Related**: ADR-0001（プロセス役割、deprecated）, ADR-0002（watcher 統合）, ADR-0007（パイプライン段）

## Context

`tsmd` は単一の `Arc<Mutex<Connection>>` で DB へアクセスし、`handle_client` は
Reindex / Reload 以外の全リクエスト（読み取りの `Status` / `Doctor` / `Search` を含む）で
この mutex を取得する。結果として 2 つの問題がある。

1. **読み取りが書き込みにブロックされる。** reindex / backfill は同じ mutex を
   バッチごとに取り直すため、`Status` / `Doctor` が長時間応答できない（mutex は unfair、
   かつ既存の `yield_to_search` は `Search` にしか譲らない）。`DELETE FROM chunks_vec` や
   FTS 初回 rebuild は 1 ロックで長時間保持する。
2. **書き込み同士で reindex が file index を starve させる。** watcher 経由の `Index` は
   reindex と同じ writer を奪い合う。reindex の FTS バッチは 1000 チャンクと大きく、
   その 1 バッチ分のあいだ `Index` が待たされる。

WAL は既に有効で、SQLite は「複数リーダー + 単一ライター」を並行で扱える。
読み取り側のボトルネックは DB ではなく in-process の単一接続 mutex である。
書き込み側は SQLite の単一ライター制約により本質的に直列だが、公平性は改善できる。

## Decision

### 決定 1: 読み取り / 書き込み接続の分離

`tsmd`（単一 DB オーナーであることは不変）の内部接続を 2 種に分ける。

| 接続 | 用途 |
|---|---|
| writer: `Arc<Mutex<Connection>>` | 全書き込み（Index / IngestSession / VectorFill / ImportWordnet, reindex, backfill）を直列化 |
| reader pool: `query_only` 接続 N 本 | 読み取りリクエスト専用。WAL スナップショットを並行読み |

- 振り分けは `DaemonRequest::is_read_only()`（全変種を網羅する match）で決定する。
  読み取りは全て同一の reader pool へ流し、種別ごとの特別扱いはしない。
  網羅 match により、新リクエスト変種は分類しない限りコンパイルが通らず、
  読み取りが writer に誤ルートして再び固まる回帰を構造的に防ぐ。
- reader 接続は `READ_WRITE` で開き `PRAGMA query_only=ON` を適用する
  （`SQLITE_OPEN_READ_ONLY` は hot WAL で `SQLITE_READONLY_RECOVERY` になり起動を阻む）。
- writer / reader 双方に `busy_timeout` を設定する。
- pool サイズ N は既定で CPU コア数（config `reader_pool_size` で上書き可）。
  N が同時並行読みの上限。

読み取りが reader pool へ移ると、`Search` と reindex/backfill の DB ロック競合は消える。
よって既存の `search_active` / `yield_to_search`（DB ロック回避のための仕組み）は
役目を終え、撤去する。

### 決定 2: 書き込みの公平化（writer は依然単一・同期）

writer を並列化はしない（SQLite は単一ファイルで並行書き込み不可）。代わりに、
撤去する `yield_to_search` の**鏡像**を導入する。

- 書き込みリクエスト（Index 等）が writer mutex を待つ / 処理中であることを示す
  `writes_pending` カウンタを立てる（`Search` 用カウンタの裏返し）。
- reindex / backfill は次バッチを取得する前に `yield_to_pending_writes` を呼び、
  保留中の書き込みがあれば mutex を再取得せず譲る。これにより file / 対話的 `Index` は
  reindex を 1 バッチ以内で preempt する。reindex が自発的に退くため、`std::sync::Mutex`
  の unfairness に依存しない。
- 書き込みは同期のまま（呼び出し側は完了を待ち、ack を受け取る）。非同期キューや専用
  writer スレッドは導入しない。
- FTS reindex のバッチサイズを config `reindex_fts_batch_size` で上書き可能にする。
  バッチが小さいほど 1 回のロック保持が短く、preempt の粒度が細かくなる。既定は
  応答性とスループットの折衷（後述）。`DELETE FROM chunks_vec`（vectors reindex）は
  1 文・短時間保持のため対象外。

書き込みは依然 SQLite の単一ライター制約により直列。reader pool は書き込みを一切担わない
（ADR-0007 の Persist トランザクション境界・Embed serial contract を侵さない）。

## Rationale

- **読み取り: `yield_to_search` を汎用カウンタに拡張する案** — 単一接続のまま。
  長時間ロック（`DELETE FROM chunks_vec` / FTS 初回 rebuild）を解消できず、却下。reader 分離を採用。
- **読み取り: リクエストごとに接続を open する案** — 毎回 shm-map + PRAGMA コスト。固定 pool を採用。
- **書き込み: 専用 writer スレッド + ジョブキュー（FIFO 公平）案** — reindex / backfill も
  自スレッドでの mutex 取得をやめてジョブ投入に作り替える必要があり、単一ライターを保つための
  本格的な再設計になる。唯一の利点は非同期ハンドオフだが、「ブロッキングでも応答が良ければ可」
  という方針のもとでは不要。`yield_to_pending_writes`（最小版）でほぼ同等の体感を一桁小さい
  変更で得られるため却下。
- **書き込み: DB 分割 / 別エンジン案** — SQLite は単一ファイルで並行書き込み不可。DB 分割は
  ファイル跨ぎ原子性を失う。組込み方針に反するため却下。
- reader を read-only にするのは `query_only`。書き込みリクエストの誤ルートを即検出できる。

## Consequences

### Positive

- reindex / backfill 実行中も `Status` / `Doctor` / `Search` が即応答する。
- 読み取りが N 本で真に並行する。
- reindex 実行中でも file / 対話的 `Index` が 1 バッチ以内で割り込めるようになる。

### Negative

- 接続が writer 1 + reader N 本に増え、各 reader が WAL の `-shm` をマップする。
- 重い `Search` が pool を占有すると軽い `Status` が短時間待つ（N > 1 で緩和）。
- `reindex_fts_batch_size` を小さくすると per-batch の fsync が増え、フル reindex の
  スループットが落ちうる（ADR-0007 の ≤5% ゲートに対し計測が必要）。
- 書き込みは依然直列。writer のスループット自体は向上しない（公平性のみ改善）。
