use std::path::Path;

/// Run `claude <args>` with env `CLAUDE_CONFIG_DIR=<config_base>`, no shell, stdin null.
/// Kills the child after `timeout_secs` seconds using kill -9 on the pid.
/// Returns `Ok(stdout)` on zero exit or `Err(stderr/reason)` otherwise.
pub fn run_plugin_command(config_base: &Path, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let child = Command::new("claude")
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    let pid = child.id();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<std::process::Output>>();

    // Move the child into a thread for waiting; the main thread enforces the wall-clock timeout.
    std::thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        }
        Ok(Err(e)) => Err(format!("process error: {e}")),
        Err(_) => {
            // Timeout: best-effort kill by pid so the process doesn't linger.
            #[cfg(unix)]
            { let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status(); }
            #[cfg(windows)]
            { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).status(); }
            Err(format!("plugin command timed out after {timeout_secs}s"))
        }
    }
}

/// Install a plugin: `claude plugin install <id>`, 180s timeout.
pub fn install_plugin(config_base: &Path, id: &str) -> Result<String, String> {
    run_plugin_command(config_base, &["plugin", "install", id], 180)
}

/// Uninstall a plugin: `claude plugin uninstall <id> -y`, 60s timeout.
pub fn uninstall_plugin(config_base: &Path, id: &str) -> Result<String, String> {
    run_plugin_command(config_base, &["plugin", "uninstall", id, "-y"], 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    // run_plugin_command / install_plugin / uninstall_plugin are NOT tested with a real
    // 'claude' binary (not available in CI). The id validator is tested at the API layer.
    // Manual verification: install_plugin / uninstall_plugin require 'claude' to be present.

    #[test]
    fn run_plugin_command_returns_quickly_when_binary_missing() {
        // On systems without 'claude', spawn fails immediately (NotFound) — never hangs.
        let tmp = std::env::temp_dir().join(format!("agentic-pltest-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let start = std::time::Instant::now();
        let result = run_plugin_command(&tmp, &["--version"], 30);
        let elapsed = start.elapsed();
        // Whether claude is installed or not, the function must return in well under 5s.
        // If claude is NOT installed: immediate NotFound error.
        // If claude IS installed: --version returns quickly.
        assert!(elapsed.as_secs() < 5, "must not hang: took {elapsed:?}");
        // We just check it returns something (Ok or Err), no assertion on the value.
        drop(result);
    }
}
