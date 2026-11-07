//! `/quit`: leave the application.

use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static QUIT: Quit = Quit;

pub struct Quit;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "quit",
    &["exit"],
    "/quit",
    "quit codest",
    ArgsPolicy::None,
)];

impl Plugin for Quit {
    fn id(&self) -> &'static str {
        "quit"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        ctx.quit();
        Ok(())
    }
}
