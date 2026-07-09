# NFC canonical normalization (#357) — Implementation Plan

Single-source-of-truth NFC normalization, applied at the two pipeline
entry points (Index/Prepare and Search/Plan) plus load-time auxiliary
points for user-edited files. Compatibility folding (NFKC) is explicitly
out of scope (see issue #357).

Precondition: #366 (session ingest unified into Prepare) is merged —
every input lane flows through `prepare_text()`, so the index-side
application is literally one call site.

## Task 1: `src/normalize.rs` (TDD)

- Add dependency `unicode-normalization` (MIT/Apache-2.0; version style
  matches existing Cargo.toml entries, exact pin lands in Cargo.lock)
- `pub fn nfc(text: &str) -> Cow<'_, str>`
  - Fast path via `is_nfc_quick` → `Cow::Borrowed` for already-NFC text
    (the overwhelming majority) with zero allocation
  - Slow path: `text.nfc().collect::<String>()` → `Cow::Owned`
- Tests: NFD input composes (e.g. `ワーカー` with decomposed dakuten →
  composed form), already-NFC borrows (assert `Cow::Borrowed`), ASCII
  passthrough, empty string, mixed content

## Task 2: Index side — `prepare_text()` head

- First statement: normalize the whole input text; everything downstream
  (frontmatter parse, chunking, lua extract, content hashes) sees NFC
- Safety net: `debug_assert!` NFC-ness where `ChunkInput.content` is built
- Test: a Markdown file containing NFD text indexes with NFC chunk
  content; content_hash equals that of the equivalent NFC source file

## Task 3: Search side — `plan()` head (`src/searcher/plan.rs:30`)

- Normalize `query` before `extract_search_keywords`; flows into FTS
  keywords, classification, synonym expansion, and query embedding
- Test: NFD query finds a document indexed from NFC source (in-memory DB,
  FTS path)

## Task 4: Auxiliary load-time points (same `nfc()`, never rewrite files)

- `user_dict`: dictionary CSV load, `reject_words.txt` load, and the
  `dict add` / `dict reject` CLI inputs
- `synonyms`: CSV load
- `entity`: `custom_terms.toml` load (compose NFC into the existing
  normalize step)
- User-edited files are normalized on load only — files stay as written
- Tests: per load path, an NFD entry matches NFC-stored data (the
  original `dict reject` failure mode from 2026-07-06)

## Task 5: Docs + invariant

- `docs/pipeline.md` invariants: add "Normalization consistency — Prepare
  and Plan apply the same canonical normalization (NFC); changing it
  requires a re-index" (mirrors invariant 3, tokenizer consistency)
- CLAUDE.md architecture tree: add `normalize.rs`
- ADR-0016 (company workspace) amendment is a follow-up, out of this repo

## Definition of Done

Per repo CLAUDE.md: cargo test / clippy -D warnings / fmt --check /
coverage ≥ 90% / jscpd ≤ 5% / lizard no new warnings / e2e via CI.
PR notes that pre-existing NFD rows in the DB are resolved operationally
by `tsm rebuild --apply` (data migration is not code scope).
