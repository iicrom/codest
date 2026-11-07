//! `/help`: render the command table contributed by every plugin.

use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static HELP: Help = Help;

pub struct Help;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "help",
    &["h", "?"],
    "/help",
    "show this help",
    ArgsPolicy::None,
)];

impl Plugin for Help {
    fn id(&self) -> &'static str {
        "help"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        let text = ctx.command_table().help_text();
        ctx.notice(text);
        Ok(())
    }
}
