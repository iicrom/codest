//! Built-in session commands.
//!
//! Multi-session support is a permanent part of the agent rather than a
//! plugin: `/new` archives the current conversation and starts a fresh one,
//! `/sessions` (alias `/resume`) opens the session picker menu, and `/rename`
//! names the current session. A session is named after its first user message
//! until renamed. The front-end owns the behavior — these specs only give
//! every front-end a shared, parseable shape (command menu, `/help`, aliases)
//! for the commands.

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;

pub static NEW_COMMAND: CommandSpec = CommandSpec::new(
    "new",
    &[],
    "/new",
    "archive the current conversation and start a fresh session",
    ArgsPolicy::None,
);

pub static RENAME_COMMAND: CommandSpec = CommandSpec::new(
    "rename",
    &[],
    "/rename <name>",
    "rename the current session (default: its first user message)",
    ArgsPolicy::Required,
);

pub static SESSIONS_COMMAND: CommandSpec = CommandSpec::new(
    "sessions",
    &["resume"],
    "/sessions",
    "open the session list to switch back and continue one",
    ArgsPolicy::None,
);

/// Session commands merged into the shipped command table; no plugin owns
/// them, so front-ends dispatch them before asking the plugin host.
pub static SESSION_COMMANDS: &[CommandSpec] = &[NEW_COMMAND, RENAME_COMMAND, SESSIONS_COMMAND];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::commands::CommandError;
    use crate::core::commands::CommandInput;
    use crate::core::commands::CommandTable;

    fn parse(input: &str) -> CommandInput {
        CommandTable::new(SESSION_COMMANDS.iter())
            .expect("session commands are valid")
            .parse(input)
    }

    #[test]
    fn new_and_sessions_parse_without_arguments() {
        for (input, name) in [
            ("/new", "new"),
            ("/sessions", "sessions"),
            ("/resume", "sessions"),
        ] {
            match parse(input) {
                CommandInput::Command(invocation) => {
                    assert_eq!(invocation.spec.name, name, "input {input}");
                    assert!(invocation.args.is_empty());
                }
                other => panic!("expected a command for {input}, got {other:?}"),
            }
        }
    }

    #[test]
    fn rename_requires_a_name() {
        assert!(matches!(
            parse("/rename"),
            CommandInput::Invalid(CommandError::MissingArguments { name: "rename", .. })
        ));
        match parse("/rename rust help") {
            CommandInput::Command(invocation) => {
                assert_eq!(invocation.spec.name, "rename");
                assert_eq!(invocation.args, "rust help");
            }
            other => panic!("expected a rename command, got {other:?}"),
        }
    }

    #[test]
    fn session_commands_reject_arguments() {
        assert!(matches!(
            parse("/new now"),
            CommandInput::Invalid(CommandError::UnexpectedArguments { name: "new", .. })
        ));
        assert!(matches!(
            parse("/sessions 2"),
            CommandInput::Invalid(CommandError::UnexpectedArguments {
                name: "sessions",
                ..
            })
        ));
    }
}
