//! The `bash` tool (unix hosts): run one shell command and return its
//! combined output.
//!
//! This module is only the command line and the model-facing text; the spawn,
//! watchdog, process-tree kill, and output caps live in [`super::shell`].
//! The command is handed over as `$SHELL -c <command>` (falling back to
//! `/bin/sh`), and the child is put in its own process group at spawn so the
//! timeout can take its children down with it.
//!
//! Windows uses [`super::pwsh`] instead; `mod.rs` picks one at compile time,
//! so `/tools`, `profile.tools`, and the tool schema all follow the host.

use crate::plugins::ToolSpec;

use super::shell;

/// The tool's name — also the plugin id, so `/tools`, `profile.tools`, and
/// the transcript's card header all use one word.
pub(crate) const TOOL_NAME: &str = "bash";

/// The model-facing schema. The `pwsh` twin differs in wording (the two
/// shells do not take the same commands) but keeps the same argument shape,
/// so a client that learned `command`/`timeout` here can call either.
pub(crate) const SPEC: ToolSpec = ToolSpec {
    name: TOOL_NAME,
    description: "Run one shell command in the working directory and return its \
combined output (stdout + stderr), with an exit-code footer. Use for anything \
not covered by the other tools. Output is truncated; narrow the command if you \
need more. A `timeout` (seconds, default 300, max 600) kills the command tree \
on expiry.",
    parameters_json: r#"{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "The shell command to run."
    },
    "timeout": {
      "type": "integer",
      "description": "Seconds to wait before killing the command tree (default 300, max 600)."
    }
  },
  "required": ["command"]
}"#,
    read_only: false,
};

/// The shell plugin's `## Tools` section. The trailing `## Files` bullets come
/// from the `fs` plugin, which owns those tools.
pub(crate) const PROMPT: &str = r#"## Tools
Work through the tools; do not ask the user to run commands for you.
- `bash` runs one shell command per call in the working directory. Chain with `&&` / `;` when one shell session is needed; there is no persistent shell, so nothing survives between calls but the files on disk.
- Prefer the file tools over their shell equivalents: `read` instead of `cat`, `edit` instead of `sed`/`awk`, `write` instead of `>` redirection.
- Relative paths resolve against the working directory. Tool output is truncated; narrow the command instead of paging everything."#;

/// Run the request through the user's shell. Argument errors and a shell that
/// will not spawn are `Err`; a non-zero exit is normal output.
pub(crate) fn run(arguments: &str) -> Result<String, String> {
    let request = shell::parse_request(arguments)?;
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = std::process::Command::new(&program);
    cmd.arg("-c").arg(&request.command);
    // No spawn hint: `/bin/sh` exists on every unix host, so a failure here
    // is an environment problem the raw OS error already describes.
    shell::run(cmd, &program, "", request.timeout_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_json(arguments: &str) -> Result<String, String> {
        run(arguments)
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
