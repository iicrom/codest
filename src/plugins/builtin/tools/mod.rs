//! Built-in tool plugins: `bash` and the file tools (`read`/`write`/`edit`).
//!
//! Two plugins rather than one, so the existing profile machinery's
//! plugin-id filter already answers the coarse question (a future
//! read-only profile can enable `fs` without `bash`); the session-level
//! `/tools` switch does the fine-grained work per tool. Shapes follow the
//! references distilled in each module: pi for schemas/truncation, dsh for
//! the edit semantics, codex for collection caps and the kill-on-timeout
//! posture.
//!
//! These are the first tools in the tree, so they also carry the working
//! conventions the model needs: relative paths resolve against the process
//! working directory (the workspace codest was started in), and the system
//! prompt section below teaches read-before-edit and edit-over-write.

mod bash;
mod fs;
mod truncate;

use crate::core::tools::ToolCall;
use crate::core::tools::ToolSpec;
use crate::plugins::Plugin;
use crate::plugins::PluginError;

pub static BASH: BashPlugin = BashPlugin;
pub static FS: FsPlugin = FsPlugin;

pub struct BashPlugin;
pub struct FsPlugin;

/// Resolve a tool path against the working directory (absolute paths pass
/// through). The process working directory is the workspace codest was
/// started in and never changes mid-run, so no per-call context is needed.
pub(crate) fn workdir() -> Result<std::path::PathBuf, String> {
    std::env::current_dir().map_err(|error| format!("cannot resolve the working directory: {error}"))
}

static BASH_SPEC: ToolSpec = ToolSpec {
    name: "bash",
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

static READ_SPEC: ToolSpec = ToolSpec {
    name: "read",
    description: "Read a text file with line numbers. Returns up to 2000 lines \
(50 KB) from `offset` (1-based line, default 1); the result says how to \
continue when truncated. Binary files are refused — use `bash` for those.",
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

static BASH_PROMPT: &str = r#"## Tools
Work through the tools; do not ask the user to run commands for you.
- `bash` runs one shell command per call in the working directory. Chain with `&&` / `;` when one shell session is needed; there is no persistent shell.
- `read` shows a numbered window of a text file; continue large files with `offset`.
- `edit` replaces exact text; `old_string` must be unique (or pass `replace_all`). `write` overwrites whole files.
- Read a file before editing or overwriting it. Prefer `edit` over `write` for existing files.
- Relative paths resolve against the working directory. Tool output is truncated; narrow commands instead of paging everything."#;

static BASH_TOOLS: [ToolSpec; 1] = [BASH_SPEC];
static FS_TOOLS: [ToolSpec; 3] = [READ_SPEC, WRITE_SPEC, EDIT_SPEC];

impl Plugin for BashPlugin {
    fn id(&self) -> &'static str {
        "bash"
    }

    fn system_prompt(&self) -> Option<&'static str> {
        Some(BASH_PROMPT)
    }

    fn tools(&self) -> &'static [ToolSpec] {
        &BASH_TOOLS
    }

    fn run_tool(&self, call: &ToolCall) -> Result<String, PluginError> {
        match call.name.as_str() {
            "bash" => bash::run_bash(&call.arguments).map_err(PluginError::new),
            other => Err(PluginError::new(format!(
                "plugin `bash` does not implement tool `{other}`"
            ))),
        }
    }
}

impl Plugin for FsPlugin {
    fn id(&self) -> &'static str {
        "fs"
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
        for spec in [&BASH_SPEC, &READ_SPEC, &WRITE_SPEC, &EDIT_SPEC] {
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
        assert!(!BASH_SPEC.read_only);
        assert!(!WRITE_SPEC.read_only);
        assert!(!EDIT_SPEC.read_only);
    }

    #[test]
    fn executors_route_by_tool_name() {
        assert!(BASH.run_tool(&ToolCall {
            id: "c1".to_string(),
            name: "not-bash".to_string(),
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
