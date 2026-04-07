use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

/// Spawn the current executable with additional arguments.
fn start_child_self(extra_args: &[&str]) -> Result<Child> {
    let exe = std::env::current_exe().context("cannot determine own executable path")?;
    let mut cmd = Command::new(&exe);
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

/// Check if a PID file points to a running process.
pub fn is_process_alive(pid_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<i32>() else {
        return false;
    };
    // kill(pid, 0) checks process existence without sending a signal.
    unsafe { libc::kill(pid, 0) == 0 }
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
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
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
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            log::warn!("could not remove stale socket {}: {e}", path.display());
        }
    }
}
