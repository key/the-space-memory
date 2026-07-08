use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

/// Spawn the current executable with additional arguments.
fn start_child_self(extra_args: &[&str]) -> Result<Child> {
    let exe = std::env::current_exe().context("cannot determine own executable path")?;
    // The daemon has already chdir'd to the project root, so its CWD
    // is the canonical project root. Pass it to every child as `--project-root`
    // so `ps`/`pgrep` reveal the owning project and the child resolves its own
    // relative state paths under the same root.
    let project_root = std::env::current_dir().context("cannot determine current dir")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--project-root").arg(&project_root);
    // Keep stderr inherited so pre-logger startup errors are visible
    cmd.args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd.spawn()
        .context(format!("Failed to spawn self with {:?}", extra_args))
}

/// Spawn a child process and write its PID file.
/// On PID file write failure, kills the child to prevent orphaned processes.
pub fn spawn_child(label: &str, extra_args: &[&str], pid_path: &Path) -> Option<Child> {
    match start_child_self(extra_args) {
        Ok(mut child) => {
            let child_pid = child.id();
            log::info!("{label} started (PID {child_pid})");
            if let Err(e) = std::fs::write(pid_path, child_pid.to_string()) {
                log::error!(
                    "failed to write {label} PID file: {e}; \
                     killing child to prevent unguarded spawn"
                );
                let _ = child.kill();
                let _ = child.wait();
                None
            } else {
                Some(child)
            }
        }
        Err(e) => {
            log::error!("failed to start {label}: {e}");
            None
        }
    }
}

/// Detect child exit. Returns `true` if the child exited.
/// The child is NOT restarted — this only logs and cleans up the PID file.
pub fn reap_child(label: &str, child: &mut Option<Child>, pid_path: &Path) -> bool {
    let Some(c) = child else { return false };
    match c.try_wait() {
        Ok(Some(exit_status)) => {
            if exit_status.success() {
                log::info!("{label} exited normally");
            } else {
                log::warn!("{label} exited with {exit_status}, not restarting");
            }
            *child = None;
            let _ = std::fs::remove_file(pid_path);
            true
        }
        Ok(None) => false,
        Err(e) => {
            log::warn!("error checking {label}: {e}");
            false
        }
    }
}

/// Stop a child process: SIGTERM -> wait (2s grace) -> SIGKILL. Removes PID file.
pub fn stop_child(label: &str, child: Option<Child>, pid_path: &Path) {
    if let Some(mut child) = child {
        let pid = child.id();
        log::info!("stopping {label} (PID {pid})...");

        // Send SIGTERM for graceful shutdown
        let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error();
            log::warn!("SIGTERM to {label} (PID {pid}) failed: {errno}");
        }

        // Wait up to 2 seconds for graceful exit
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                let _ = std::fs::remove_file(pid_path);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // Force kill if still running
        if let Err(e) = child.kill() {
            log::warn!("failed to kill {label} (PID {pid}): {e}");
        }
        if let Err(e) = child.wait() {
            log::warn!("failed to wait for {label} (PID {pid}): {e}");
        }
    }
    let _ = std::fs::remove_file(pid_path);
}

/// Remove a stale UNIX socket if it exists.
pub fn remove_stale_socket(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("could not remove stale socket {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reap_child_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("test.pid");
        let mut child: Option<Child> = None;
        assert!(!reap_child("test", &mut child, &pid_path));
    }

    #[test]
    fn test_remove_stale_socket_nonexistent() {
        // Should not panic on missing file
        remove_stale_socket(Path::new("/tmp/nonexistent.sock"));
    }

    #[test]
    fn test_remove_stale_socket_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");
        std::fs::write(&sock_path, "").unwrap();
        assert!(sock_path.exists());
        remove_stale_socket(&sock_path);
        assert!(!sock_path.exists());
    }
}
