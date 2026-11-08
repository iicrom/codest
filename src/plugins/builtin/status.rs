//! `/status`: show the backend, UI mode, transcript size, context usage, and
//! loaded plugins.
//!
//! This is the reference plugin for the static system: it contributes one
//! no-argument command and only reads from [`PluginContext`].

use crate::core::tokens::{format_context_usage, format_tokens};
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

/// The context line: usage against the window when both are known, bare
/// usage otherwise, and a placeholder before the first measured turn.
fn context_line(tokens: Option<u64>, window: Option<u64>) -> String {
    match tokens {
        Some(tokens) => {
            let mut line = format_context_usage(tokens, window);
            if window.is_none() {
                line.push_str(" · window unknown");
            }
            line
        }
        None => "no measured turn yet".to_string(),
    }
}

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
            "codest status\n  backend: {}\n  mode: {}\n  transcript: {} cells\n  context: {}\n  tokens: session in {} / out {}\n  commands: {}\n  plugins: {}",
            ctx.model(),
            ctx.mode(),
            ctx.transcript_len(),
            context_line(
                ctx.last_usage().map(|(input, output)| input.saturating_add(output)),
                ctx.context_window(),
            ),
            format_tokens(ctx.total_input()),
            format_tokens(ctx.total_output()),
            ctx.command_table().len(),
            ctx.plugin_ids().join(", ")
        );
        ctx.notice(text);
        Ok(())
    }
}
