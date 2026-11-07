//! `/status`: show the backend, UI mode, transcript size, and loaded plugins.
//!
//! This is the reference plugin for the static system: it contributes one
//! no-argument command and only reads from [`PluginContext`].

use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static STATUS: Status = Status;

pub struct Status;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "status",
    &[],
    "/status",
    "show backend, mode, and plugin status",
    ArgsPolicy::None,
)];

impl Plugin for Status {
    fn id(&self) -> &'static str {
        "status"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        let text = format!(
            "codest status\n  backend: {}\n  mode: {}\n  transcript: {} cells\n  commands: {}\n  plugins: {}",
            ctx.model(),
            ctx.mode(),
            ctx.transcript_len(),
            ctx.command_table().len(),
            ctx.plugin_ids().join(", ")
        );
        ctx.notice(text);
        Ok(())
    }
}
