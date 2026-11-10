//! The `bash` tool: run one shell command and return its combined output.
//!
//! A one-shot spawn rather than a persistent shell (codest has no per-session
//! PTY to host one): `$SHELL -c <command>` (falling back to `/bin/sh`), with
//! a watchdog that kills the whole process group on timeout — the default is
//! 300s like dsh's persistent bash, overridable per call up to 600s. The
//! process group (`process_group(0)`) is what makes the kill take the
//! command's children down with it; without it a `npm run dev &` would
//! outlive the timeout and hold the pipes open.
//!
//! Output is drained on threads (so a chatty command cannot fill its pipe
//! buffers and deadlock the wait loop), capped per stream at
//! [`truncate::COLLECT_MAX_BYTES`], then tail-truncated to the shared caps
//! by [`truncate::truncate_tail`] before it reaches the model.

use std::io::Read;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use super::truncate;
use super::workdir;

/// Kill the command after this long without an explicit `timeout`.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Hard ceiling for the `timeout` argument.
const MAX_TIMEOUT_SECS: u64 = 600;
/// How often the watchdog polls for exit between `try_wait`s.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Run `command` in the session's working directory. The result carries the
/// combined stdout/stderr, an exit-code footer, and truncation notices — the
/// `Err` half is reserved for tool-level failures (bad arguments, no shell),
/// not for non-zero exits, which the model must see as normal output.
pub(crate) fn run_bash(arguments: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let command = parsed
        .get("command")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing required argument `command` (a shell command string)")?
        .trim();
    if command.is_empty() {
        return Err("`command` is empty".to_string());
    }
    let timeout_secs = match parsed.get("timeout") {
        None | Some(serde_json::Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(value) => {
            let requested = value
                .as_u64()
                .ok_or("`timeout` must be an integer number of seconds")?;
            if requested == 0 {
                return Err("`timeout` must be at least 1 second".to_string());
            }
            requested.min(MAX_TIMEOUT_SECS)
        }
    };

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = std::process::Command::new(&shell);
    cmd.arg("-c").arg(command);
    cmd.current_dir(workdir()?);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("failed to spawn `{shell}`: {error}"))?;

    // Drain both pipes on threads from the start: a command that writes more
    // than the OS pipe buffer would otherwise block forever on a full pipe
    // while we sit in the wait loop. The handles are joined *after* the
    // watchdog, never before it — joining first would block until the
    // command closed its pipes, which is exactly what the timeout is for.
    let stdout = child.stdout.take().map(spawn_drainer);
    let stderr = child.stderr.take().map(spawn_drainer);

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= Duration::from_secs(timeout_secs) {
                    kill_process_tree(&mut child);
                    timed_out = true;
                    // The kill is asynchronous; wait for the exit so no
                    // zombie is left behind.
                    break child.wait().ok();
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => break None,
        }
    };

    let stdout_text = stdout
        .map(|handle| handle.join().unwrap_or_default())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stderr_text = stderr
        .map(|handle| handle.join().unwrap_or_default())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();

    let mut output = String::new();
    if !stdout_text.is_empty() {
        output.push_str(&stdout_text);
    }
    if !stderr_text.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&stderr_text);
    }
    if timed_out {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!(
            "[command timed out after {timeout_secs}s and was killed]"
        ));
    } else {
        let footer = match status {
            Some(status) if status.success() => "[exit code: 0]".to_string(),
            Some(status) => match status.code() {
                Some(code) => format!("[exit code: {code}]"),
                None => "[terminated by signal]".to_string(),
            },
            None => "[command could not be reaped]".to_string(),
        };
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&footer);
    }
    Ok(truncate::truncate_tail(&output))
}

/// Collect one pipe up to [`truncate::COLLECT_MAX_BYTES`], discarding the
/// rest; a drained pipe lets the command run to completion.
fn spawn_drainer<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut collected = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if collected.len() < truncate::COLLECT_MAX_BYTES {
                        let take = n.min(truncate::COLLECT_MAX_BYTES - collected.len());
                        collected.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        collected
    })
}

/// Kill the command and its children. On unix the child was given its own
/// process group at spawn, so a negative-pid `kill` takes the whole tree
/// down; elsewhere only the direct child can be killed.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_json(arguments: &str) -> Result<String, String> {
        run_bash(arguments)
    }

    #[test]
    fn rejects_missing_or_empty_command() {
        assert!(run_bash("{}").is_err());
        assert!(run_bash(r#"{"command": ""}"#).is_err());
        assert!(run_bash("not json").is_err());
        assert!(run_bash(r#"{"command": 3}"#).is_err());
    }

    #[test]
    fn rejects_bad_timeouts() {
        assert!(run_bash(r#"{"command": "true", "timeout": 0}"#).is_err());
        assert!(run_bash(r#"{"command": "true", "timeout": "ten"}"#).is_err());
    }

    #[test]
    fn reports_the_exit_code() {
        let out = run_json(r#"{"command": "exit 3"}"#).expect("run");
        assert!(out.contains("[exit code: 3]"), "{out}");
    }

    #[test]
    fn captures_stdout_and_stderr() {
        let out = run_json(
            r#"{"command": "echo out-line; echo err-line >&2"}"#,
        )
        .expect("run");
        assert!(out.contains("out-line"), "{out}");
        assert!(out.contains("err-line"), "{out}");
        assert!(out.contains("[exit code: 0]"), "{out}");
    }

    #[test]
    fn runs_in_the_current_working_directory() {
        let out = run_json(r#"{"command": "pwd"}"#).expect("run");
        let cwd = std::env::current_dir().expect("cwd");
        assert!(
            out.contains(cwd.to_string_lossy().as_ref()),
            "{out} vs {cwd:?}"
        );
    }

    #[test]
    fn timeout_kills_a_sleeping_command() {
        let out = run_json(
            r#"{"command": "sleep 30 && echo survived", "timeout": 1}"#,
        )
        .expect("run");
        assert!(out.contains("timed out after 1s"), "{out}");
        assert!(!out.contains("survived"), "{out}");
        // Completes well under the 30s the child was asked to sleep.
    }

    #[test]
    fn timeout_caps_at_the_maximum() {
        // A `timeout` above the ceiling is clamped, not rejected: the command
        // still runs (and here still exits immediately).
        let out = run_json(r#"{"command": "true", "timeout": 100000}"#).expect("run");
        assert!(out.contains("[exit code: 0]"), "{out}");
    }
}
