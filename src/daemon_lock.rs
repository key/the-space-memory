//! Exclusive, lifetime-held advisory lock guarding per-project daemon startup.
//!
//! `tsmd` acquires this lock (`flock`) on `<state_dir>/tsmd.lock` before binding
//! its socket and holds it for the whole process lifetime. The lock — not the
//! PID file — is the sole ownership oracle: the kernel releases it automatically
//! on process death, so a leftover socket can be reclaimed without PID-liveness
//! guesswork, and concurrent `tsm start` invocations serialize on a single
//! atomic gate (ADR-0010).
//!
//! The lock file is opened close-on-exec (the std `File` default) so spawned
//! children never inherit the lock fd. An orphaned `--no-idle-timeout` embedder
//! that kept the lock held would otherwise deadlock the next daemon's startup.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Outcome of a non-blocking attempt to acquire the daemon startup lock.
pub enum LockOutcome {
    /// Lock acquired. Hold the returned guard for the daemon's lifetime;
    /// dropping it (or the process dying) releases the lock.
    Acquired(LockGuard),
    /// Another live process already holds the lock for this project.
    Held,
}

/// RAII guard owning the open lock file. The `flock` is released when this is
/// dropped or the process exits. The lock file itself is intentionally NOT
/// unlinked: removing it while held would let a re-creating process lock a
/// different inode, splitting ownership.
pub struct LockGuard {
    _file: std::fs::File,
}

/// Try to acquire the exclusive daemon lock at `path` without blocking.
///
/// Returns [`LockOutcome::Held`] when a live process already owns the lock, and
/// an error only for unexpected I/O failures (e.g. the parent directory is
/// missing). The lock file is created if absent.
pub fn try_acquire(path: &Path) -> io::Result<LockOutcome> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid descriptor owned by `file` for the duration of
    // this call. `flock` only consults the descriptor.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(LockOutcome::Acquired(LockGuard { _file: file }));
    }

    let err = io::Error::last_os_error();
    // LOCK_NB reports a conflicting lock via EWOULDBLOCK (== EAGAIN on the
    // platforms in play). Anything else is a genuine failure.
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK => Ok(LockOutcome::Held),
        _ => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    fn lock_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("tsmd.lock")
    }

    #[test]
    fn first_acquire_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        match try_acquire(&path).unwrap() {
            LockOutcome::Acquired(_guard) => {}
            LockOutcome::Held => panic!("first acquire must succeed"),
        }
        assert!(path.exists(), "lock file should be created");
    }

    #[test]
    fn second_concurrent_acquire_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        // Hold the first lock for the duration of the second attempt.
        let _held = match try_acquire(&path).unwrap() {
            LockOutcome::Acquired(g) => g,
            LockOutcome::Held => panic!("first acquire must succeed"),
        };

        // A second, independent open of the same path conflicts even within the
        // same process: flock locks attach to the open file description.
        match try_acquire(&path).unwrap() {
            LockOutcome::Held => {}
            LockOutcome::Acquired(_) => panic!("second acquire must observe Held"),
        }
    }

    #[test]
    fn lock_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        {
            let _g = match try_acquire(&path).unwrap() {
                LockOutcome::Acquired(g) => g,
                LockOutcome::Held => panic!("first acquire must succeed"),
            };
        } // guard dropped here → flock released

        match try_acquire(&path).unwrap() {
            LockOutcome::Acquired(_) => {}
            LockOutcome::Held => panic!("acquire after drop must succeed"),
        }
    }

    #[test]
    fn lock_fd_is_close_on_exec() {
        // The orphan-deadlock guard hinges on the lock fd being CLOEXEC so
        // children spawned via fork+exec never inherit it. Pin that premise.
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        let guard = match try_acquire(&path).unwrap() {
            LockOutcome::Acquired(g) => g,
            LockOutcome::Held => panic!("first acquire must succeed"),
        };

        let fd = guard._file.as_raw_fd();
        // SAFETY: querying descriptor flags on a live, owned fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "lock fd must be close-on-exec"
        );
    }
}
