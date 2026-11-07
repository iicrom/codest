//! Built-in slash commands.
//!
//! Commands are parsed in the core so every front-end agrees on what counts as
//! a command. Only input whose first non-whitespace character is `/` is
//! considered; recognized commands are dispatched locally and never sent to
//! the model, unknown commands produce a [`CommandError`], and everything else
//! is a normal user message.
//!
//! The command table is intentionally a small `const` array so adding a
//! command is one entry in [`COMMANDS`] plus one arm in the front-end.

use std::fmt;
use std::fmt::Write as _;

/// A built-in command recognized at the start of a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Clear,
    Quit,
}

/// How a submitted line should be handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandInput {
    /// Not slash-prefixed: send it to the model as a normal user message.
    Message,
    /// A recognized command.
    Command(SlashCommand),
    /// Slash-prefixed input that could not be parsed.
    Invalid(CommandError),
}

/// Why a slash-prefixed submission was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// A bare `/` with no command name.
    MissingName,
    /// The name is not in the command table.
    Unknown { name: String },
    /// The built-in commands do not take arguments.
    UnexpectedArguments { name: &'static str, args: String },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => write!(f, "missing command name after `/`; try /help"),
            Self::Unknown { name } => write!(f, "unknown command `/{name}`; try /help"),
            Self::UnexpectedArguments { name, args } => {
                write!(f, "`/{name}` does not take arguments (got `{args}`)")
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// One row of the command table.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub command: SlashCommand,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub summary: &'static str,
}

impl CommandSpec {
    /// `" (also /h, /?)"`, or an empty string when there are no aliases.
    pub fn alias_hint(&self) -> String {
        if self.aliases.is_empty() {
            return String::new();
        }
        let names = self
            .aliases
            .iter()
            .map(|alias| format!("/{alias}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" (also {names})")
    }
}

/// Commands whose name or one of its aliases starts with `query`
/// (case-insensitive). An empty query returns every command in table order.
pub fn suggestions(query: &str) -> Vec<&'static CommandSpec> {
    let query = query.trim().to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| {
            spec.name.starts_with(&query)
                || spec.aliases.iter().any(|alias| alias.starts_with(&query))
        })
        .collect()
}

/// Every command the agent understands.
pub const COMMANDS: [CommandSpec; 3] = [
    CommandSpec {
        command: SlashCommand::Help,
        name: "help",
        aliases: &["h", "?"],
        usage: "/help",
        summary: "show this help",
    },
    CommandSpec {
        command: SlashCommand::Clear,
        name: "clear",
        aliases: &["reset"],
        usage: "/clear",
        summary: "clear the conversation context and transcript",
    },
    CommandSpec {
        command: SlashCommand::Quit,
        name: "quit",
        aliases: &["exit"],
        usage: "/quit",
        summary: "quit codest",
    },
];

impl SlashCommand {
    fn spec(self) -> &'static CommandSpec {
        COMMANDS
            .iter()
            .find(|spec| spec.command == self)
            .expect("every SlashCommand has a spec")
    }

    pub fn name(self) -> &'static str {
        self.spec().name
    }

    pub fn usage(self) -> &'static str {
        self.spec().usage
    }

    pub fn summary(self) -> &'static str {
        self.spec().summary
    }

    pub fn aliases(self) -> &'static [&'static str] {
        self.spec().aliases
    }

    /// The `/help` body: usage, summary, and aliases for every command.
    pub fn help_text() -> String {
        let mut text = String::from("slash commands:");
        for spec in &COMMANDS {
            // Writing into a String cannot fail.
            let _ = write!(
                text,
                "\n  {:<8} {}{}",
                spec.usage,
                spec.summary,
                spec.alias_hint()
            );
        }
        let _ = write!(text, "\nAnything else is sent to the model.");
        text
    }
}

impl CommandInput {
    /// Classify a submission. Leading/trailing whitespace is ignored, and
    /// command names are matched case-insensitively.
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();
        let Some(rest) = trimmed.strip_prefix('/') else {
            return Self::Message;
        };

        let mut words = rest.split_whitespace();
        let Some(raw_name) = words.next() else {
            return Self::Invalid(CommandError::MissingName);
        };
        let name = raw_name.to_ascii_lowercase();
        let Some(spec) = COMMANDS
            .iter()
            .find(|spec| spec.name == name || spec.aliases.contains(&name.as_str()))
        else {
            return Self::Invalid(CommandError::Unknown { name });
        };

        let args = words.collect::<Vec<_>>();
        if !args.is_empty() {
            return Self::Invalid(CommandError::UnexpectedArguments {
                name: spec.name,
                args: args.join(" "),
            });
        }
        Self::Command(spec.command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_a_message() {
        assert_eq!(CommandInput::parse("hello"), CommandInput::Message);
        assert_eq!(
            CommandInput::parse(" /help"),
            CommandInput::Command(SlashCommand::Help)
        );
        assert_eq!(CommandInput::parse("hello /help"), CommandInput::Message);
        assert_eq!(CommandInput::parse(""), CommandInput::Message);
    }

    #[test]
    fn recognizes_commands_and_aliases() {
        for (input, expected) in [
            ("/help", SlashCommand::Help),
            ("/h", SlashCommand::Help),
            ("/?", SlashCommand::Help),
            ("/clear", SlashCommand::Clear),
            ("/reset", SlashCommand::Clear),
            ("/quit", SlashCommand::Quit),
            ("/exit", SlashCommand::Quit),
        ] {
            assert_eq!(CommandInput::parse(input), CommandInput::Command(expected));
        }
    }

    #[test]
    fn is_case_insensitive_and_allows_surrounding_whitespace() {
        assert_eq!(
            CommandInput::parse("  /HeLp  "),
            CommandInput::Command(SlashCommand::Help)
        );
    }

    #[test]
    fn bare_slash_is_an_error() {
        assert_eq!(
            CommandInput::parse("/"),
            CommandInput::Invalid(CommandError::MissingName)
        );
        assert_eq!(
            CommandInput::parse("  /  "),
            CommandInput::Invalid(CommandError::MissingName)
        );
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert_eq!(
            CommandInput::parse("/nope"),
            CommandInput::Invalid(CommandError::Unknown {
                name: "nope".to_string()
            })
        );
    }

    #[test]
    fn arguments_are_rejected() {
        assert_eq!(
            CommandInput::parse("/help me"),
            CommandInput::Invalid(CommandError::UnexpectedArguments {
                name: "help",
                args: "me".to_string(),
            })
        );
    }

    #[test]
    fn help_text_lists_every_command() {
        let help = SlashCommand::help_text();
        for spec in &COMMANDS {
            assert!(help.contains(spec.usage), "missing {}", spec.usage);
            assert!(help.contains(spec.summary), "missing {}", spec.summary);
        }
        assert!(help.contains("Anything else is sent to the model."));
    }

    #[test]
    fn empty_query_suggests_every_command_in_order() {
        let names = suggestions("")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["help", "clear", "quit"]);
    }

    #[test]
    fn suggestions_filter_by_name_prefix() {
        let names = suggestions("cl")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["clear"]);
        assert!(suggestions("zzz").is_empty());
    }

    #[test]
    fn suggestions_match_aliases() {
        for (query, expected) in [("h", "help"), ("re", "clear"), ("ex", "quit")] {
            let names = suggestions(query)
                .into_iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>();
            assert_eq!(names, vec![expected], "query {query}");
        }
    }

    #[test]
    fn suggestions_are_case_insensitive() {
        let names = suggestions("HE")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["help"]);
    }
}
