//! The `pwsh` tool (Windows hosts): run one PowerShell command and return its
//! combined output.
//!
//! This is the Windows half of the built-in shell tool — [`super::bash`] is
//! the unix half — and like it, only the command line and the model-facing
//! text live here; the spawn, watchdog, process-tree kill, and output caps
//! come from [`super::shell`]. `mod.rs` picks one at compile time, so
//! `/tools`, `profile.tools`, and the tool schema all follow the host.
//!
//! The command runs as `pwsh -NoLogo -NoProfile -NonInteractive -Command`
//! with one wrapped script argument: no profile (a user's `$PROFILE` would
//! otherwise decide how every call behaves), no interactive prompts (the
//! tool's stdin is closed; a `Read-Host` must fail fast rather than hang
//! until the timeout), and the wrapper because a bare `-Command` reports the
//! exit code of an `exit` statement only, so a failing cmdlet would reach the
//! model as a success. [`PREAMBLE`] and [`EPILOGUE`] close that gap.

use crate::plugins::ToolSpec;

use super::shell;

/// The tool's name — also the plugin id, so `/tools`, `profile.tools`, and
/// the transcript's card header all use one word.
pub(crate) const TOOL_NAME: &str = "pwsh";

/// The interpreter to spawn. PowerShell 7 is a separate install on Windows;
/// when it is missing the tool fails with an actionable message rather than
/// silently falling back to the older `powershell.exe`, whose wording and
/// encoding rules would make the tool behave differently per machine.
const PROGRAM: &str = "pwsh";

/// Appended to a spawn failure — a `pwsh`-less machine is the one common
/// Windows failure, and the model should hear the fix, not just "not found".
const SPAWN_HINT: &str = "PowerShell 7 is a separate install: `winget install \
Microsoft.PowerShell`, or put `pwsh` on `PATH`.";

/// Everything before the model's command text.
///
/// * `$ErrorActionPreference = 'Continue'` — a failing cmdlet writes stderr
///   and lets the script continue, matching how a shell script behaves; the
///   epilogue turns the failure into a non-zero exit code.
/// * `[Console]::OutputEncoding` — PowerShell decodes a native command's
///   output (and writes its own) with this encoding, which can still follow
///   the console's legacy code page. Pinning UTF-8 keeps `cargo build` and
///   friends readable; the `try` covers a host that refuses the assignment.
const PREAMBLE: &str = r#"$ErrorActionPreference = 'Continue'
try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}
try {
"#;

/// Everything after the model's command text: exception reporting plus the
/// exit code the footer reports.
///
/// `$?` and `$LASTEXITCODE` are read into variables *first* — the `if`
/// statements below would otherwise overwrite `$?` before it is consulted.
/// A native command's code wins (that is what a build failure looks like); a
/// failing cmdlet, which only sets `$?`, becomes `1`.
const EPILOGUE: &str = r#"
} catch {
  [Console]::Error.WriteLine($_)
  exit 1
}
$__codest_ok = $?
$__codest_code = $LASTEXITCODE
if ($null -ne $__codest_code -and $__codest_code -ne 0) { exit $__codest_code }
if (-not $__codest_ok) { exit 1 }
exit 0
"#;

/// The model-facing schema. Identical in shape to [`super::bash`]'s — same
/// `command`/`timeout` arguments — because the two are one tool that swaps
/// its shell with the host.
pub(crate) const SPEC: ToolSpec = ToolSpec {
    name: TOOL_NAME,
    description: "Run one PowerShell 7 command in the working directory and \
return its combined output (stdout + stderr), with an exit-code footer. Use for \
anything not covered by the other tools. Output is truncated; narrow the \
command if you need more. A `timeout` (seconds, default 300, max 600) kills the \
command tree on expiry.",
    parameters_json: r#"{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "The PowerShell command to run."
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
- `pwsh` runs one PowerShell 7 script per call in the working directory. Chain with `;` or pipelines when one session is needed; there is no persistent session, so variables and functions do not survive between calls — only the files on disk.
- Prefer the file tools over their PowerShell equivalents: `read` instead of `Get-Content`, `edit` instead of `-replace`, `write` instead of `Set-Content` / `>`.
- Relative paths resolve against the working directory. Tool output is truncated; narrow the command instead of paging everything.
- The footer is the script's exit code: a failing cmdlet, a non-zero `$LASTEXITCODE` from a native command, and a caught exception all report non-zero. Output is UTF-8."#;

/// Run the request through PowerShell. Argument errors and a missing `pwsh`
/// are `Err`; a non-zero exit is normal output.
pub(crate) fn run(arguments: &str) -> Result<String, String> {
    let request = shell::parse_request(arguments)?;
    let mut cmd = std::process::Command::new(PROGRAM);
    cmd.arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(script(&request.command));
    shell::run(cmd, PROGRAM, SPAWN_HINT, request.timeout_secs)
}

/// The full `-Command` text: preamble, the model's command, epilogue.
///
/// Assembled by concatenation rather than one `format!` so the PowerShell
/// braces need no `{{`/`}}` escaping — the script stays readable next to the
/// PowerShell it produces.
fn script(command: &str) -> String {
    let mut script = String::with_capacity(PREAMBLE.len() + command.len() + EPILOGUE.len() + 1);
    script.push_str(PREAMBLE);
    script.push_str(command);
    script.push_str(EPILOGUE);
    script
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
    fn propagates_a_native_commands_exit_code() {
        // `$LASTEXITCODE` is how a failed build/toolchain shows up; without
        // the epilogue the footer would say 0.
        let out = run_json(r#"{"command": "cmd /c exit 5"}"#).expect("run");
        assert!(out.contains("[exit code: 5]"), "{out}");
    }

    #[test]
    fn a_failing_cmdlet_becomes_a_non_zero_exit() {
        let out = run_json(
            r#"{"command": "Get-Item C:\\codest-no-such-path\\file.txt"}"#,
        )
        .expect("run");
        assert!(out.contains("[exit code: 1]"), "{out}");
    }

    #[test]
    fn captures_stdout_and_stderr() {
        let out = run_json(
            r#"{"command": "Write-Output out-line; [Console]::Error.WriteLine('err-line')"}"#,
        )
        .expect("run");
        assert!(out.contains("out-line"), "{out}");
        assert!(out.contains("err-line"), "{out}");
        assert!(out.contains("[exit code: 0]"), "{out}");
    }

    #[test]
    fn keeps_non_ascii_output_intact() {
        // The UTF-8 preamble is what keeps a native command's output from
        // arriving as code-page mojibake.
        let out = run_json(r#"{"command": "Write-Output '中文 ✓'"}"#).expect("run");
        assert!(out.contains("中文 ✓"), "{out}");
    }

    #[test]
    fn runs_in_the_current_working_directory() {
        let out = run_json(r#"{"command": "(Get-Location).Path"}"#).expect("run");
        let cwd = std::env::current_dir().expect("cwd");
        assert!(
            out.contains(cwd.to_string_lossy().as_ref()),
            "{out} vs {cwd:?}"
        );
    }

    #[test]
    fn timeout_kills_a_sleeping_command() {
        let out = run_json(
            r#"{"command": "Start-Sleep -Seconds 30; Write-Output survived", "timeout": 1}"#,
        )
        .expect("run");
        assert!(out.contains("timed out after 1s"), "{out}");
        assert!(!out.contains("survived"), "{out}");
        // Completes well under the 30s the child was asked to sleep.
    }

    #[test]
    fn timeout_caps_at_the_maximum() {
        let out = run_json(r#"{"command": "Write-Output ok", "timeout": 100000}"#).expect("run");
        assert!(out.contains("[exit code: 0]"), "{out}");
    }
}
