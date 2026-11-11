//! The one-shot process engine behind the built-in shell tool.
//!
//! The tool's *face* is platform-specific — `bash` on unix hosts, `pwsh` on
//! Windows (see `bash.rs` / `pwsh.rs`) — but the machinery is one shape for
//! both: spawn the shell once per call (codest has no per-session PTY to host
//! a persistent one), drain both pipes on threads, watch the clock, kill the
//! whole process tree on expiry, and return one combined transcript with an
//! exit-code footer.
//!
//! The process tree is what the watchdog kills, not just the direct child:
//! without that a `npm run dev &` (or a PowerShell job) would outlive the
//! timeout and hold the pipes open. Unix gets its own process group at spawn;
//! Windows has no such group to signal, so it walks the tree with `taskkill
//! /T` instead.
//!
//! Output is drained on threads (so a chatty command cannot fill its pipe
//! buffers and deadlock the wait loop), capped per stream at
//! [`truncate::COLLECT_MAX_BYTES`], then tail-truncated to the shared caps
//! by [`truncate::truncate_tail`] before it reaches the model.

use std::io::Read;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use super::truncate;
use super::workdir;

/// Kill the command after this long without an explicit `timeout`.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Hard ceiling for the `timeout` argument.
pub(crate) const MAX_TIMEOUT_SECS: u64 = 600;
/// How often the watchdog polls for exit between `try_wait`s.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The `command` / `timeout` arguments both shell tools accept.
///
/// Parsed here rather than in each platform module so `bash` and `pwsh`
/// reject the same malformed call with the same words — the model should not
/// have to learn two error dialects for one argument shape.
pub(crate) struct Request {
    /// The command text, trimmed; never empty.
    pub(crate) command: String,
    /// Already clamped to [`MAX_TIMEOUT_SECS`].
    pub(crate) timeout_secs: u64,
}

pub(crate) fn parse_request(arguments: &str) -> Result<Request, String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let command = parsed
        .get("command")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing required argument `command` (a shell command string)")?
        .trim()
        .to_string();
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
    Ok(Request {
        command,
        timeout_secs,
    })
}

/// Run `cmd` — already carrying its program and the arguments that precede
/// the command text — and return the combined output.
///
/// The result carries stdout followed by stderr, an exit-code footer, and
/// truncation notices; the `Err` half is reserved for tool-level failures
/// (bad arguments, a shell that will not spawn), never for non-zero exits,
/// which the model must see as normal output.
///
/// `display` names the program in error messages: `$SHELL` resolves at
/// runtime, so the unix shim cannot assume it spawned `bash`. `spawn_hint` is
/// an extra sentence for the model when the spawn itself fails — a missing
/// `pwsh` is the common Windows case, and it needs to hear about the install
/// rather than the raw OS error alone.
pub(crate) fn run(
    mut cmd: Command,
    display: &str,
    spawn_hint: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    cmd.current_dir(workdir()?);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|error| {
        let mut message = format!("failed to spawn `{display}`: {error}");
        if !spawn_hint.is_empty() {
            message.push(' ');
            message.push_str(spawn_hint);
        }
        message
    })?;

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

/// Kill the command and its children.
fn kill_process_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child was given its own process group at spawn, so a
        // negative-pid `kill` takes the whole tree down.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // Windows has no process group to signal: `taskkill /T` walks the
        // child's tree for us. It needs the pid while the process still
        // exists, hence the order; the plain `kill` afterwards covers a
        // `taskkill` that was missing from `PATH` (an already-dead child is
        // simply an error we ignore).
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_or_empty_command() {
        assert!(parse_request("{}").is_err());
        assert!(parse_request(r#"{"command": ""}"#).is_err());
        assert!(parse_request(r#"{"command": "   "}"#).is_err());
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"command": 3}"#).is_err());
    }

    #[test]
    fn rejects_bad_timeouts() {
        assert!(parse_request(r#"{"command": "true", "timeout": 0}"#).is_err());
        assert!(parse_request(r#"{"command": "true", "timeout": "ten"}"#).is_err());
    }

    #[test]
    fn defaults_and_clamps_the_timeout() {
        let request = parse_request(r#"{"command": "true"}"#).expect("parse");
        assert_eq!(request.command, "true");
        assert_eq!(request.timeout_secs, DEFAULT_TIMEOUT_SECS);

        // A `timeout` above the ceiling is clamped, not rejected.
        let request = parse_request(r#"{"command": "true", "timeout": 100000}"#).expect("parse");
        assert_eq!(request.timeout_secs, MAX_TIMEOUT_SECS);
    }
}
