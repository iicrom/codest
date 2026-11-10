//! The `/tools` command spec: session-level tool switches.
//!
//! Like the session and reasoning commands, the spec only gives every
//! front-end a shared, parseable shape (command menu, `/help`); the behavior
//! is a front-end concern. `/tools` with no argument opens a checkbox picker
//! over the declared tools; `/tools <name> [on|off]` flips or pins one tool
//! directly. The switch carries the whole enabled set to the worker
//! (`Op::SetToolsEnabled`), where disabled tools stop being declared to the
//! model and are rejected at execution time. The set is per session and is
//! recorded in the rollout, so a resumed session continues with the tools it
//! last ran with — the seam future read-only / normal profiles plug into.

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;

pub static TOOLS_COMMAND: CommandSpec = CommandSpec::new(
    "tools",
    &[],
    "/tools [name] [on|off]",
    "show or set the session's enabled tools (no args opens a picker)",
    ArgsPolicy::Optional,
);

/// Tool commands merged into the shipped command table; no plugin owns them,
/// so front-ends dispatch them before asking the plugin host.
pub static TOOLS_COMMANDS: &[CommandSpec] = &[TOOLS_COMMAND];
