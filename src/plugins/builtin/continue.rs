//! `/continue`: keep going in the current session without a new message.
//!
//! The recovery path for an interrupted turn: a crash left a dangling user
//! message or an unanswered tool round, or an abort cut the last answer
//! short. The transcript is driven as it stands — nothing is appended, so
//! the model picks up exactly where things stopped. See
//! `docs/session-continue.md` for the full semantics.

use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static CONTINUE: Continue = Continue;

pub struct Continue;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "continue",
    &[],
    "/continue",
    "continue the current session without a new message (resume an interrupted turn)",
    ArgsPolicy::None,
)];

impl Plugin for Continue {
    fn id(&self) -> &'static str {
        "continue"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        ctx.continue_turn();
        Ok(())
    }
}
