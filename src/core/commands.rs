//! Slash-command model and parser.
//!
//! The core owns the *shape* of a command (name, aliases, arguments, help) and
//! the parser, but not the command set: a [`CommandTable`] is built from the
//! [`CommandSpec`]s contributed by plugins. That keeps every front-end in
//! agreement about what counts as a command while letting the plugin layer
//! decide which commands exist.
//!
//! Only input whose first non-whitespace character is `/` is considered;
//! recognized commands are dispatched locally and never sent to the model,
//! unknown commands produce a [`CommandError`], and everything else is a
//! normal user message.

use std::fmt;
use std::fmt::Write as _;

/// Whether a command accepts trailing arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArgsPolicy {
    /// No arguments; any trailing text is an error.
    #[default]
    None,
    /// Arguments may be omitted.
    Optional,
    /// At least one non-whitespace argument is required.
    Required,
}

/// Static metadata for one command.
///
/// Specs are `'static` because the shipped command set is compiled into the
/// binary. Keeping them borrowed (rather than owned strings) lets the command
/// menu and parser hand out references without cloning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Primary name, without the leading `/`.
    pub name: &'static str,
    /// Alternative names, without the leading `/`.
    pub aliases: &'static [&'static str],
    /// Usage string shown by `/help`, including the leading `/`.
    pub usage: &'static str,
    /// One-line description shown by `/help` and the command menu.
    pub summary: &'static str,
    /// Argument policy enforced by the parser.
    pub args: ArgsPolicy,
}

impl CommandSpec {
    pub const fn new(
        name: &'static str,
        aliases: &'static [&'static str],
        usage: &'static str,
        summary: &'static str,
        args: ArgsPolicy,
    ) -> Self {
        Self {
            name,
            aliases,
            usage,
            summary,
            args,
        }
    }

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

/// A parsed command ready to be dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub spec: &'static CommandSpec,
    /// Raw trailing text, with leading/trailing whitespace removed. Internal
    /// whitespace (including newlines) is preserved for plugins to parse.
    pub args: String,
}

/// How a submitted line should be handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandInput {
    /// Not slash-prefixed: send it to the model as a normal user message.
    Message,
    /// A recognized command.
    Command(Invocation),
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
    /// A no-argument command was given arguments.
    UnexpectedArguments { name: &'static str, args: String },
    /// A command that requires arguments was given none.
    MissingArguments {
        name: &'static str,
        usage: &'static str,
    },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => write!(f, "missing command name after `/`; try /help"),
            Self::Unknown { name } => write!(f, "unknown command `/{name}`; try /help"),
            Self::UnexpectedArguments { name, args } => {
                write!(f, "`/{name}` does not take arguments (got `{args}`)")
            }
            Self::MissingArguments { name, usage } => {
                write!(f, "`/{name}` needs arguments; usage: {usage}")
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// Why a command table could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandTableError {
    #[error("invalid command name `{name}`: use lowercase ASCII letters, digits, `-`, `_`, or `?`")]
    InvalidName { name: &'static str },
    #[error("command name or alias `{name}` is already registered")]
    DuplicateName { name: &'static str },
}

/// The ordered set of commands contributed by the loaded plugins.
#[derive(Debug, Clone, Default)]
pub struct CommandTable {
    specs: Vec<&'static CommandSpec>,
}

impl CommandTable {
    /// Build a table, rejecting invalid names and collisions across both
    /// primary names and aliases. Registration order is preserved.
    pub fn new(
        specs: impl IntoIterator<Item = &'static CommandSpec>,
    ) -> Result<Self, CommandTableError> {
        let mut seen: Vec<&'static str> = Vec::new();
        let mut table = Vec::new();

        for spec in specs {
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                if !valid_name(name) {
                    return Err(CommandTableError::InvalidName { name });
                }
                if seen.contains(&name) {
                    return Err(CommandTableError::DuplicateName { name });
                }
                seen.push(name);
            }
            table.push(spec);
        }

        Ok(Self { specs: table })
    }

    /// Every command in registration order.
    pub fn specs(&self) -> &[&'static CommandSpec] {
        &self.specs
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Resolve a primary name or alias, case-insensitively.
    pub fn resolve(&self, name: &str) -> Option<&'static CommandSpec> {
        self.specs.iter().copied().find(|spec| {
            spec.name.eq_ignore_ascii_case(name)
                || spec
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
        })
    }

    /// Commands whose name or one of its aliases starts with `query`
    /// (case-insensitive). An empty query returns every command in table order.
    pub fn suggestions(&self, query: &str) -> Vec<&'static CommandSpec> {
        let query = query.trim().to_ascii_lowercase();
        self.specs
            .iter()
            .copied()
            .filter(|spec| {
                spec.name.starts_with(&query)
                    || spec.aliases.iter().any(|alias| alias.starts_with(&query))
            })
            .collect()
    }

    /// Classify a submission. Leading/trailing whitespace is ignored, and
    /// command names are matched case-insensitively.
    pub fn parse(&self, input: &str) -> CommandInput {
        let trimmed = input.trim();
        let Some(rest) = trimmed.strip_prefix('/') else {
            return CommandInput::Message;
        };

        let rest = rest.trim_start();
        let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let raw_name = &rest[..name_end];
        if raw_name.is_empty() {
            return CommandInput::Invalid(CommandError::MissingName);
        }

        let Some(spec) = self.resolve(raw_name) else {
            return CommandInput::Invalid(CommandError::Unknown {
                name: raw_name.to_ascii_lowercase(),
            });
        };

        let args = rest[name_end..].trim().to_string();
        match spec.args {
            ArgsPolicy::None if !args.is_empty() => {
                CommandInput::Invalid(CommandError::UnexpectedArguments {
                    name: spec.name,
                    args,
                })
            }
            ArgsPolicy::Required if args.is_empty() => {
                CommandInput::Invalid(CommandError::MissingArguments {
                    name: spec.name,
                    usage: spec.usage,
                })
            }
            _ => CommandInput::Command(Invocation { spec, args }),
        }
    }

    /// The `/help` body: usage, summary, and aliases for every command.
    pub fn help_text(&self) -> String {
        let width = self
            .specs
            .iter()
            .map(|spec| spec.usage.chars().count())
            .max()
            .unwrap_or(8)
            .max(8);

        let mut text = String::from("slash commands:");
        for spec in &self.specs {
            // Writing into a String cannot fail.
            let _ = write!(
                text,
                "\n  {:<width$} {}{}",
                spec.usage,
                spec.summary,
                spec.alias_hint(),
                width = width
            );
        }
        let _ = write!(text, "\nAnything else is sent to the model.");
        text
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'?')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    static HELP: CommandSpec =
        CommandSpec::new("help", &["h", "?"], "/help", "show help", ArgsPolicy::None);
    static CLEAR: CommandSpec = CommandSpec::new(
        "clear",
        &["reset"],
        "/clear",
        "clear the conversation",
        ArgsPolicy::None,
    );
    static ECHO: CommandSpec = CommandSpec::new(
        "echo",
        &[],
        "/echo <text>",
        "echo text back",
        ArgsPolicy::Required,
    );
    static NOTE: CommandSpec = CommandSpec::new(
        "note",
        &[],
        "/note [text]",
        "add a note",
        ArgsPolicy::Optional,
    );

    fn table() -> CommandTable {
        CommandTable::new([&HELP, &CLEAR, &ECHO, &NOTE]).expect("valid table")
    }

    #[test]
    fn plain_text_is_a_message() {
        let table = table();
        assert_eq!(table.parse("hello"), CommandInput::Message);
        assert_eq!(
            table.parse(" /help"),
            CommandInput::Command(Invocation {
                spec: &HELP,
                args: String::new()
            })
        );
        assert_eq!(table.parse("hello /help"), CommandInput::Message);
        assert_eq!(table.parse(""), CommandInput::Message);
    }

    #[test]
    fn recognizes_commands_and_aliases() {
        let table = table();
        for (input, expected) in [
            ("/help", &HELP),
            ("/h", &HELP),
            ("/?", &HELP),
            ("/clear", &CLEAR),
            ("/reset", &CLEAR),
        ] {
            assert_eq!(
                table.parse(input),
                CommandInput::Command(Invocation {
                    spec: expected,
                    args: String::new()
                }),
                "input {input}"
            );
        }
    }

    #[test]
    fn is_case_insensitive_and_allows_surrounding_whitespace() {
        let table = table();
        assert_eq!(
            table.parse("  /HeLp  "),
            CommandInput::Command(Invocation {
                spec: &HELP,
                args: String::new()
            })
        );
    }

    #[test]
    fn bare_slash_is_an_error() {
        let table = table();
        assert_eq!(
            table.parse("/"),
            CommandInput::Invalid(CommandError::MissingName)
        );
        assert_eq!(
            table.parse("  /  "),
            CommandInput::Invalid(CommandError::MissingName)
        );
    }

    #[test]
    fn unknown_command_is_an_error() {
        let table = table();
        assert_eq!(
            table.parse("/nope"),
            CommandInput::Invalid(CommandError::Unknown {
                name: "nope".to_string()
            })
        );
    }

    #[test]
    fn no_argument_commands_reject_arguments() {
        let table = table();
        assert_eq!(
            table.parse("/help me"),
            CommandInput::Invalid(CommandError::UnexpectedArguments {
                name: "help",
                args: "me".to_string(),
            })
        );
    }

    #[test]
    fn required_arguments_must_be_present() {
        let table = table();
        assert_eq!(
            table.parse("/echo"),
            CommandInput::Invalid(CommandError::MissingArguments {
                name: "echo",
                usage: "/echo <text>",
            })
        );
        assert_eq!(
            table.parse("/echo   "),
            CommandInput::Invalid(CommandError::MissingArguments {
                name: "echo",
                usage: "/echo <text>",
            })
        );
    }

    #[test]
    fn arguments_preserve_internal_whitespace() {
        let table = table();
        match table.parse("/echo  hello   world\nsecond line") {
            CommandInput::Command(invocation) => {
                assert_eq!(invocation.args, "hello   world\nsecond line");
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn optional_arguments_are_passed_through() {
        let table = table();
        assert_eq!(
            table.parse("/note"),
            CommandInput::Command(Invocation {
                spec: &NOTE,
                args: String::new()
            })
        );
        assert_eq!(
            table.parse("/note buy milk"),
            CommandInput::Command(Invocation {
                spec: &NOTE,
                args: "buy milk".to_string()
            })
        );
    }

    #[test]
    fn duplicate_names_and_aliases_are_rejected() {
        static DUPLICATE: CommandSpec =
            CommandSpec::new("h", &[], "/h", "duplicate alias", ArgsPolicy::None);
        assert_eq!(
            CommandTable::new([&HELP, &DUPLICATE]).unwrap_err(),
            CommandTableError::DuplicateName { name: "h" }
        );
    }

    #[test]
    fn invalid_names_are_rejected() {
        static INVALID: CommandSpec =
            CommandSpec::new("Not Valid", &[], "/bad", "bad", ArgsPolicy::None);
        assert_eq!(
            CommandTable::new([&INVALID]).unwrap_err(),
            CommandTableError::InvalidName { name: "Not Valid" }
        );
    }

    #[test]
    fn help_text_lists_every_command() {
        let help = table().help_text();
        for spec in table().specs() {
            assert!(help.contains(spec.usage), "missing {}", spec.usage);
            assert!(help.contains(spec.summary), "missing {}", spec.summary);
        }
        assert!(help.contains("Anything else is sent to the model."));
    }

    #[test]
    fn empty_query_suggests_every_command_in_order() {
        let names = table()
            .suggestions("")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["help", "clear", "echo", "note"]);
    }

    #[test]
    fn suggestions_filter_by_name_prefix() {
        let table = table();
        let names = table
            .suggestions("cl")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["clear"]);
        assert!(table.suggestions("zzz").is_empty());
    }

    #[test]
    fn suggestions_match_aliases() {
        let table = table();
        for (query, expected) in [("h", "help"), ("re", "clear")] {
            let names = table
                .suggestions(query)
                .into_iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>();
            assert_eq!(names, vec![expected], "query {query}");
        }
    }

    #[test]
    fn suggestions_are_case_insensitive() {
        let names = table()
            .suggestions("HE")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["help"]);
    }
}
