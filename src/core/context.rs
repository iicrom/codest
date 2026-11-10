//! Built-in `/context` command spec.
//!
//! The behavior is the front-end's, in two modes: before a session has any
//! transcript messages the command prints the prompt-debug report (the
//! assembled prompt, request config, tools, usage — what the old
//! `prompt_debug` plugin rendered); once the session has messages, the same
//! name opens the interactive context viewer (browse, delete, edit, undo).
//! No plugin owns the command — this spec only gives every front-end a
//! shared, parseable shape (command menu, `/help`, parsing).

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;

pub static CONTEXT_COMMAND: CommandSpec = CommandSpec::new(
    "context",
    &[],
    "/context",
    "before the first message: show the prompt report; in a session: browse, delete, and edit the context",
    ArgsPolicy::None,
);

/// Context commands merged into the shipped command table; no plugin owns
/// them, so front-ends dispatch them before asking the plugin host.
pub static CONTEXT_COMMANDS: &[CommandSpec] = &[CONTEXT_COMMAND];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::commands::CommandInput;
    use crate::core::commands::CommandTable;

    #[test]
    fn context_parses_without_arguments() {
        match CommandTable::new(CONTEXT_COMMANDS.iter())
            .expect("valid")
            .parse("/context")
        {
            CommandInput::Command(invocation) => {
                assert_eq!(invocation.spec.name, "context");
                assert!(invocation.args.is_empty());
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }
}
