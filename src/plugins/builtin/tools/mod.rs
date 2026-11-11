//! Built-in tool plugins: the shell tool and the file tools
//! (`read`/`write`/`edit`).
//!
//! Two plugins rather than one, so the existing profile machinery's
//! plugin-id filter already answers the coarse question (a future
//! read-only profile can enable `fs` without the shell); the session-level
//! `/tools` switch does the fine-grained work per tool. Shapes follow the
//! references distilled in each module: pi for schemas/truncation, dsh for
//! the edit semantics, codex for collection caps and the kill-on-timeout
//! posture.
//!
//! The shell tool is the platform's own — `bash` on unix hosts, `pwsh` on
//! Windows — selected at compile time below. Both are thin shims over the
//! shared engine in [`shell`], so only the command line and the wording
//! differ: a Windows host gets a PowerShell tool and a schema that says so,
//! instead of a `bash` tool that cannot spawn.
//!
//! These are the first tools in the tree, so they also carry the working
//! conventions the model needs: relative paths resolve against the process
//! working directory (the workspace codest was started in), and the system
//! prompt sections below teach read-before-edit and edit-over-write.

mod fs;
mod shell;
mod truncate;

#[cfg(not(windows))]
mod bash;
#[cfg(windows)]
mod pwsh;

/// The host's shell tool module: everything platform-specific (name, schema,
/// prompt section, argument shim) comes from this one alias, so nothing else
/// in the tree has to branch on the platform.
#[cfg(not(windows))]
use self::bash as shell_tool;
#[cfg(windows)]
use self::pwsh as shell_tool;

use crate::core::tools::ToolCall;
use crate::core::tools::ToolSpec;
use crate::plugins::Plugin;
use crate::plugins::PluginError;

pub static SHELL: ShellPlugin = ShellPlugin;
pub static FS: FsPlugin = FsPlugin;

pub struct ShellPlugin;
pub struct FsPlugin;

/// The enabled shell tool's name (`bash` / `pwsh`) — for messages elsewhere
/// that have to point the model at it.
pub(crate) use shell_tool::TOOL_NAME as SHELL_TOOL_NAME;

/// Resolve a tool path against the working directory (absolute paths pass
/// through). The process working directory is the workspace codest was
/// started in and never changes mid-run, so no per-call context is needed.
pub(crate) fn workdir() -> Result<std::path::PathBuf, String> {
    std::env::current_dir().map_err(|error| format!("cannot resolve the working directory: {error}"))
}

static READ_SPEC: ToolSpec = ToolSpec {
    name: "read",
    description: "Read a text file with line numbers. Returns up to 2000 lines \
(50 KB) from `offset` (1-based line, default 1); the result says how to \
continue when truncated. Binary files are refused — read them with the shell \
tool instead.",
    parameters_json: r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "File path, absolute or relative to the working directory."
    },
    "offset": {
      "type": "integer",
      "description": "1-based line number to start from."
    },
    "limit": {
      "type": "integer",
      "description": "Maximum number of lines to return (default 2000)."
    }
  },
  "required": ["path"]
}"#,
    read_only: true,
};

static WRITE_SPEC: ToolSpec = ToolSpec {
    name: "write",
    description: "Create or overwrite a file with complete content, creating \
parent directories as needed. For existing files prefer `edit` — a full \
overwrite loses anything you did not retype.",
    parameters_json: r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "File path, absolute or relative to the working directory."
    },
    "content": {
      "type": "string",
      "description": "The complete file content to write."
    }
  },
  "required": ["path", "content"]
}"#,
    read_only: false,
};

static EDIT_SPEC: ToolSpec = ToolSpec {
    name: "edit",
    description: "Replace exact text in a file. `old_string` must match exactly \
once — include enough surrounding lines to make it unique — or set \
`replace_all` to replace every occurrence. Read the file first.",
    parameters_json: r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "File path, absolute or relative to the working directory."
    },
    "old_string": {
      "type": "string",
      "description": "The exact text to replace."
    },
    "new_string": {
      "type": "string",
      "description": "The replacement text (empty string deletes)."
    },
    "replace_all": {
      "type": "boolean",
      "description": "Replace every occurrence instead of requiring a unique match (default false)."
    }
  },
  "required": ["path", "old_string", "new_string"]
}"#,
    read_only: false,
};

static FS_PROMPT: &str = r#"## Files
- `read` shows a numbered window of a text file; continue large files with `offset`.
- `edit` replaces exact text; `old_string` must be unique (or pass `replace_all`). `write` overwrites whole files.
- Read a file before editing or overwriting it. Prefer `edit` over `write` for existing files."#;

static SHELL_TOOLS: [ToolSpec; 1] = [shell_tool::SPEC];
static FS_TOOLS: [ToolSpec; 3] = [READ_SPEC, WRITE_SPEC, EDIT_SPEC];

impl Plugin for ShellPlugin {
    fn id(&self) -> &'static str {
        shell_tool::TOOL_NAME
    }

    fn system_prompt(&self) -> Option<&'static str> {
        Some(shell_tool::PROMPT)
    }

    fn tools(&self) -> &'static [ToolSpec] {
        &SHELL_TOOLS
    }

    fn run_tool(&self, call: &ToolCall) -> Result<String, PluginError> {
        match call.name.as_str() {
            shell_tool::TOOL_NAME => shell_tool::run(&call.arguments).map_err(PluginError::new),
            other => Err(PluginError::new(format!(
                "plugin `{}` does not implement tool `{other}`",
                shell_tool::TOOL_NAME
            ))),
        }
    }
}

impl Plugin for FsPlugin {
    fn id(&self) -> &'static str {
        "fs"
    }

    fn system_prompt(&self) -> Option<&'static str> {
        Some(FS_PROMPT)
    }

    fn tools(&self) -> &'static [ToolSpec] {
        &FS_TOOLS
    }

    fn run_tool(&self, call: &ToolCall) -> Result<String, PluginError> {
        let outcome = match call.name.as_str() {
            "read" => fs::run_read(&call.arguments),
            "write" => fs::run_write(&call.arguments),
            "edit" => fs::run_edit(&call.arguments),
            other => {
                return Err(PluginError::new(format!(
                    "plugin `fs` does not implement tool `{other}`"
                )))
            }
        };
        outcome.map_err(PluginError::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_and_carry_the_read_only_flag() {
        for spec in [&SHELL_TOOLS[0], &READ_SPEC, &WRITE_SPEC, &EDIT_SPEC] {
            let function = spec.function_json();
            assert_eq!(function["type"], "function", "{}", spec.name);
            assert_eq!(
                function["function"]["parameters"]["type"],
                "object",
                "{}: parameters_json must be a schema object",
                spec.name
            );
        }
        assert!(READ_SPEC.read_only);
        assert!(!SHELL_TOOLS[0].read_only);
        assert!(!WRITE_SPEC.read_only);
        assert!(!EDIT_SPEC.read_only);
    }

    /// The tool is the host's own: same argument shape either way, but the
    /// name the model sees, the plugin id `profile.tools` filters on, and the
    /// prompt that teaches it must all agree with the platform.
    #[test]
    fn the_shell_tool_is_named_for_the_host() {
        let expected = if cfg!(windows) { "pwsh" } else { "bash" };
        assert_eq!(shell_tool::TOOL_NAME, expected);
        assert_eq!(SHELL_TOOLS[0].name, expected);
        assert_eq!(SHELL.id(), expected);
        assert!(
            shell_tool::PROMPT.contains(expected),
            "{}",
            shell_tool::PROMPT
        );
        // Both platforms declare the same arguments, so a cached schema or a
        // familiar model call keeps working across hosts.
        let parameters: serde_json::Value =
            serde_json::from_str(SHELL_TOOLS[0].parameters_json).expect("schema");
        assert_eq!(parameters["required"][0], "command");
        assert!(parameters["properties"]["timeout"].is_object());
    }

    #[test]
    fn executors_route_by_tool_name() {
        assert!(SHELL.run_tool(&ToolCall {
            id: "c1".to_string(),
            name: "not-the-shell".to_string(),
            arguments: "{}".to_string(),
        })
        .is_err());
        assert!(FS.run_tool(&ToolCall {
            id: "c2".to_string(),
            name: "not-fs".to_string(),
            arguments: "{}".to_string(),
        })
        .is_err());
    }
}
