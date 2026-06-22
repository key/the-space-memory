# Embedder/Daemon Blocking-Accept Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the macOS-only failure where `tsmd` IPC servers (embedder and daemon) reject client requests with `Resource temporarily unavailable (os error 35 / EAGAIN)` because a socket accepted from a non-blocking listener inherits `O_NONBLOCK`.

**Architecture:** Both `tsmd --embedder` and the daemon set their `UnixListener` non-blocking so the accept loop can poll and check the shutdown flag. On BSD/macOS the stream returned by `accept()` inherits the listener's non-blocking flag, so the subsequent length-prefixed `read_message` returns `EAGAIN` instead of waiting for the request. Add a shared `ipc::accept_blocking()` helper that accepts and resets the stream to blocking, and use it in both accept loops. Linux is unaffected (it does not inherit the flag) — the fix makes both platforms behave identically.

**Tech Stack:** Rust (std `UnixListener`/`UnixStream`), `libc` (already a dependency), `cargo test`.

## Global Constraints

- `cargo test` passes (all existing + new). Verbatim DoD from `CLAUDE.md`.
- `cargo clippy -- -D warnings` clean.
- `cargo fmt --check` clean.
- Coverage ≥ 90% on covered modules (`cargo llvm-cov`); `src/ipc.rs` is a covered module, so the new pub fn needs a unit test.
- `npx jscpd` duplication ≤ 5%; `lizard src/ --language rust -Tcyclomatic_complexity=15 -w` no new warnings.
- `bash tests/e2e.sh` passes (IPC changed → required).
- Branch name follows `<type>/<description>`: `fix/embedder-blocking-accept`.
- MIT license; no new dependencies (`libc` already present, exact-pinned).
- Root cause is confirmed by runtime evidence (`[embedder_mode] Client error: Resource temporarily unavailable (os error 35)`) plus code (`embedder_mode.rs` and `daemon_mode.rs` set the listener non-blocking and pass the accepted stream straight to `read_message`). The `is_ignored` hypothesis (in `src/indexer/walker.rs`) was considered and ruled out: it is in the file-walk path and cannot affect socket I/O blocking mode.

---

### Task 1: Add `ipc::accept_blocking` helper

**Files:**
- Modify: `src/ipc.rs`
- Test: `src/ipc.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub fn accept_blocking(listener: &std::os::unix::net::UnixListener) -> std::io::Result<(std::os::unix::net::UnixStream, std::os::unix::net::SocketAddr)>` — accepts one connection and returns a **blocking** stream. Propagates `ErrorKind::WouldBlock` from a non-blocking listener unchanged so callers' poll loops keep working.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/ipc.rs`:

```rust
#[test]
fn accept_blocking_clears_nonblocking_inherited_from_listener() {
    use super::accept_blocking;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.sock");

    // Listener is non-blocking, exactly like the embedder/daemon accept loops.
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();

    // A connected client makes accept() return a stream.
    let _client = UnixStream::connect(&path).unwrap();

    let (stream, _addr) = loop {
        match accept_blocking(&listener) {
            Ok(pair) => break pair,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("accept_blocking failed: {e}"),
        }
    };

    // The accepted stream must be blocking on every platform, even though the
    // listener is non-blocking (on BSD/macOS the flag is otherwise inherited).
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL failed");
    assert_eq!(
        flags & libc::O_NONBLOCK,
        0,
        "accepted stream must be blocking"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ipc::tests::accept_blocking_clears_nonblocking_inherited_from_listener`
Expected: FAIL to compile — `cannot find function accept_blocking in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/ipc.rs` (top-level, after the existing `use` and before/after `read_message`). Add the import line `use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};` at the top of the file:

```rust
/// Accept one connection and hand back a BLOCKING stream.
///
/// Callers keep the listener non-blocking so the accept loop can poll and check
/// the shutdown flag. On BSD/macOS a socket accepted from a non-blocking
/// listener inherits `O_NONBLOCK`, so a length-prefixed [`read_message`] then
/// fails with `EAGAIN` ("Resource temporarily unavailable") instead of waiting
/// for the request. Reset the accepted stream to blocking so reads behave
/// identically on Linux and macOS. A `WouldBlock` error from the listener (no
/// pending connection) is propagated unchanged for the caller's poll loop.
pub fn accept_blocking(listener: &UnixListener) -> std::io::Result<(UnixStream, SocketAddr)> {
    let (stream, addr) = listener.accept()?;
    stream.set_nonblocking(false)?;
    Ok((stream, addr))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ipc::tests::accept_blocking_clears_nonblocking_inherited_from_listener`
Expected: PASS (on macOS this fails without Step 3's `set_nonblocking(false)`; on Linux it documents the invariant).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt
cargo clippy -- -D warnings
git add src/ipc.rs
git commit -m "fix(ipc): add accept_blocking to reset accepted sockets to blocking

On BSD/macOS a stream accepted from a non-blocking UnixListener inherits
O_NONBLOCK, making length-prefixed read_message fail with EAGAIN. The new
helper resets the accepted stream to blocking; callers keep polling via the
propagated WouldBlock."
```

---

### Task 2: Use `accept_blocking` in the embedder accept loop

**Files:**
- Modify: `src/bin/tsmd/embedder_mode.rs` (import line near top; the `listener.accept()` call in the accept loop, ~line 86)

**Interfaces:**
- Consumes: `ipc::accept_blocking` from Task 1.

- [ ] **Step 1: Add the import**

In `src/bin/tsmd/embedder_mode.rs`, extend the existing IPC import:

```rust
use the_space_memory::ipc::{accept_blocking, read_message, write_message};
```

- [ ] **Step 2: Swap the accept call**

Replace the accept loop's `match listener.accept() {` with:

```rust
        match accept_blocking(&listener) {
```

Leave the `Ok((stream, _))`, `Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock`, and fatal `Err(e)` arms unchanged.

- [ ] **Step 3: Build + verify embedder serves a request (macOS reproduction)**

Run:
```bash
cargo build --release --bin tsm --bin tsmd
P=$(mktemp -d) && cd "$P"
"$OLDPWD/target/release/tsm" init >/dev/null
# Stage a model so the embedder actually serves (skip if no model available;
# rely on Task 4's workspace verification instead).
"$OLDPWD/target/release/tsm" setup >/dev/null 2>&1 || true
```
Expected: with a model present, `tsm start` then a search/backfill produces NO `Client error: Resource temporarily unavailable` lines in `.tsm/logs/tsmd-stderr.log`. (Definitive end-to-end check is Task 4.)

- [ ] **Step 4: Run the full test suite**

Run: `cargo test`
Expected: PASS (no regressions; Task 1 test green).

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings
git add src/bin/tsmd/embedder_mode.rs
git commit -m "fix(embedder): accept blocking client sockets (macOS EAGAIN)

Use ipc::accept_blocking so the embedder's accepted sockets are blocking;
fixes 'Client error: Resource temporarily unavailable (os error 35)' on
macOS that stalled vector backfill."
```

---

### Task 3: Use `accept_blocking` in the daemon accept loop

**Files:**
- Modify: `src/bin/tsmd/daemon_mode.rs` (import; the `listener.accept()` call in the accept loop, ~line 256)

**Interfaces:**
- Consumes: `ipc::accept_blocking` from Task 1.

- [ ] **Step 1: Add the import**

In `src/bin/tsmd/daemon_mode.rs`, add (or extend an existing `the_space_memory::ipc` use):

```rust
use the_space_memory::ipc::accept_blocking;
```

- [ ] **Step 2: Swap the accept call**

Replace `match listener.accept() {` (the main accept loop, ~line 256) with:

```rust
        match accept_blocking(&listener) {
```

Leave the `Ok((mut stream, _))`, `Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock`, and fatal `Err(e)` arms unchanged. (The daemon spawns a thread per client, so this bug was masked by timing — but blocking accepted sockets is correct regardless.)

- [ ] **Step 3: Run the full test suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings
git add src/bin/tsmd/daemon_mode.rs
git commit -m "fix(daemon): accept blocking client sockets

Use ipc::accept_blocking for the daemon accept loop too, so daemon.sock
client reads cannot intermittently EAGAIN on macOS."
```

---

### Task 4: End-to-end verification on macOS + e2e

**Files:** none (verification only)

- [ ] **Step 1: Build release**

Run: `cargo build --release --bin tsm --bin tsmd`
Expected: success.

- [ ] **Step 2: Verify the embedder serves against a real model**

Use a project that already has the model staged (the workspace `.tsm/models/ruri-v3-30m` exists). In a temp project with the model linked, or by pointing at an initialized project:

```bash
BIN=$(pwd)/target/release
P=$(mktemp -d) && cd "$P"
"$BIN/tsm" init >/dev/null
ln -s /Users/key/work/2026/key/workspace/.tsm/models ".tsm/models-src" 2>/dev/null || true
# Easiest: copy/symlink the model dir into .tsm/models/ruri-v3-30m, then:
"$BIN/tsm" start
echo "hello world" > note.md
echo "note.md" | "$BIN/tsm" index --files-from-stdin
sleep 5
grep -c "Resource temporarily unavailable" .tsm/logs/tsmd-stderr.log || echo "0 EAGAIN errors"
"$BIN/tsm" doctor -f json | head -c 200   # must return promptly, valid JSON
"$BIN/tsm" stop
```
Expected: `0 EAGAIN errors`, vectors backfill progresses (doctor shows Vectors matching chunks over time), and `tsm doctor` returns promptly (no hang) even while indexing.

- [ ] **Step 3: Run e2e (IPC changed)**

Run: `bash tests/e2e.sh`
Expected: PASS.

- [ ] **Step 4: Final gate**

```bash
cargo test && cargo clippy -- -D warnings && cargo fmt --check
npx jscpd
lizard src/ --language rust -Tcyclomatic_complexity=15 -w
```
Expected: all clean.

- [ ] **Step 5: Push + PR**

```bash
git push -u origin fix/embedder-blocking-accept
gh pr create --base main --title "fix: accept blocking client sockets (macOS EAGAIN)" --body "<summary + root cause + verification>"
```
