//! Reasoning controls: the `/effort` and `/thinking` command specs plus the
//! effort-level validation they share.
//!
//! Like the session and model commands, the specs only give every front-end a
//! shared, parseable shape (command menu, `/help`); the behavior is a
//! front-end concern. `/effort` carries a per-session reasoning effort to the
//! model request (`Op::SetReasoningEffort`, sent as `reasoning_effort`);
//! `/thinking` is purely display-side (folding of `AgentThinkingDelta`
//! output).

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;

pub static EFFORT_COMMAND: CommandSpec = CommandSpec::new(
    "effort",
    &[],
    "/effort [level]",
    "show or set the model reasoning effort (no level opens a picker; off unsets)",
    ArgsPolicy::Optional,
);

pub static THINKING_COMMAND: CommandSpec = CommandSpec::new(
    "thinking",
    &[],
    "/thinking",
    "toggle folding of the model's thinking output",
    ArgsPolicy::None,
);

/// Reasoning commands merged into the shipped command table; no plugin owns
/// them, so front-ends dispatch them before asking the plugin host.
pub static REASONING_COMMANDS: &[CommandSpec] = &[EFFORT_COMMAND, THINKING_COMMAND];

/// The effort levels accepted for models that do not declare their own
/// [`crate::core::config::ModelInfo::reasoning`] list, lowest first.
pub const DEFAULT_EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high"];

/// The argument that clears the effort override (no `reasoning_effort` field
/// is sent at all). Always valid, and not part of the level sets above.
pub const EFFORT_OFF: &str = "off";

/// Validate an `/effort` argument against the active model's declared
/// `reasoning` levels, falling back to [`DEFAULT_EFFORT_LEVELS`] for models
/// that declare none. `off` ([`EFFORT_OFF`]) is always accepted.
pub fn validate_effort(value: &str, model_levels: Option<&[String]>) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("effort level is empty; try /effort to open the picker".to_string());
    }
    if value.eq_ignore_ascii_case(EFFORT_OFF) {
        return Ok(());
    }
    let available: Vec<&str> = match model_levels {
        Some(levels) if !levels.is_empty() => levels.iter().map(String::as_str).collect(),
        _ => DEFAULT_EFFORT_LEVELS.to_vec(),
    };
    if available.iter().any(|level| level.eq_ignore_ascii_case(value)) {
        return Ok(());
    }
    Err(format!(
        "unknown effort level `{value}`; available: {}, or {EFFORT_OFF} to unset",
        available.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_default_levels_case_insensitively() {
        for level in DEFAULT_EFFORT_LEVELS {
            assert!(validate_effort(level, None).is_ok(), "{level}");
            assert!(validate_effort(&level.to_ascii_uppercase(), None).is_ok());
        }
        assert!(validate_effort(" high ", None).is_ok());
        assert!(validate_effort(EFFORT_OFF, None).is_ok());
        assert!(validate_effort("OFF", None).is_ok());
    }

    #[test]
    fn rejects_unknown_levels_with_the_available_set() {
        let error = validate_effort("turbo", None).unwrap_err();
        assert!(error.contains("turbo"), "{error}");
        assert!(error.contains("minimal"), "{error}");
        assert!(error.contains("off"), "{error}");
    }

    #[test]
    fn declared_levels_win_over_the_default_set() {
        let levels = vec!["draft".to_string(), "deep".to_string()];
        assert!(validate_effort("deep", Some(&levels)).is_ok());
        assert!(validate_effort("off", Some(&levels)).is_ok());
        // `high` is a default level but not declared by this model.
        assert!(validate_effort("high", Some(&levels)).is_err());
    }

    #[test]
    fn empty_level_is_rejected_with_guidance() {
        let error = validate_effort("   ", None).unwrap_err();
        assert!(error.contains("empty"), "{error}");
    }
}
