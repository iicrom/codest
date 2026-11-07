//! `/clear`: drop the visible transcript and the model-visible history.

use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static CLEAR: Clear = Clear;

pub struct Clear;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "clear",
    &["reset"],
    "/clear",
    "clear the conversation context and transcript",
    ArgsPolicy::None,
)];

impl Plugin for Clear {
    fn id(&self) -> &'static str {
        "clear"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        ctx.clear_conversation();
        Ok(())
    }
}
