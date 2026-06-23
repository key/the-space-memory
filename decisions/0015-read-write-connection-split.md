---
status: proposed
created: 2026-06-23
updated: 2026-06-23
---

# ADR-0015: 読み取り / 書き込み DB 接続の分離

- **Deciders**: key
- **Related**: ADR-0001（プロセス役割、deprecated）, ADR-0002（watcher 統合）, ADR-0007（パイプライン段）

## Context

`tsmd` は単一の `Arc<Mutex<Connection>>` で DB へアクセスし、`handle_client` は
Reindex / Reload 以外の全リクエスト（読み取りの `Status` / `Doctor` / `Search` を含む）で
この mutex を取得する。reindex / backfill は同じ mutex をバッチごとに取り直すため、
`Status` / `Doctor` が長時間応答できなくなる（mutex は unfair、かつ
`yield_to_search` は `Search` にしか譲らない）。さらに `DELETE FROM chunks_vec` や
FTS 初回 rebuild は 1 ロックで長時間保持する。

WAL は既に有効で、SQLite は「複数リーダー + 単一ライター」を並行で扱える。
ボトルネックは DB ではなく in-process の単一接続 mutex である。

## Decision

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

書き込みは依然 SQLite の単一ライター制約により直列。reader pool は書き込みを一切担わない
（ADR-0007 の Persist トランザクション境界・Embed serial contract を侵さない）。

## Rationale

- **代替案: `yield_to_search` を汎用カウンタに拡張** — 単一接続のまま。
  `DELETE FROM chunks_vec` や FTS 初回 rebuild の長時間ロックを解消できず、却下。
- **代替案: リクエストごとに接続を open** — 毎回 shm-map + PRAGMA コスト。固定 pool を採用。
- **代替案: 書き込みの並列化（DB 分割 / 別エンジン）** — SQLite は単一ファイルで
  並行書き込み不可。DB 分割はファイル跨ぎ原子性を失う。組込み方針に反するため却下。
- reader を read-only にするのは `query_only`。これで書き込みリクエストの誤ルートを即検出できる。

## Consequences

### Positive

- reindex / backfill 実行中も `Status` / `Doctor` / `Search` が即応答する。
- 読み取りが N 本で真に並行する。

### Negative

- 接続が writer 1 + reader N 本に増え、各 reader が WAL の `-shm` をマップする。
- 重い `Search` が pool を占有すると軽い `Status` が短時間待つ（N > 1 で緩和）。
