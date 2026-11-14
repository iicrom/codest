//! `/status`: show the backend, UI mode, transcript size, context usage, and
//! loaded plugins.
//!
//! This is the reference plugin for the static system: it contributes one
//! no-argument command and only reads from [`PluginContext`].

use crate::core::tokens::{cache_percent, format_cache_rate, format_context_usage, format_tokens};
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
        // The cache line rates the same rounds the usage counts, so the two
        // lines can never disagree: last round's hit rate beside the
        // session's weighted one. Both parts drop when the backend reports
        // no cache accounting — a zero sum may be "nothing cached yet", and
        // a 0% read would claim a measurement.
        let cache = match ctx.last_usage() {
            Some((input, _)) => {
                let last = format_cache_rate(ctx.last_cached(), input)
                    .map(|rate| format!("{rate} (last)"));
                let session = (ctx.total_cached() > 0).then(|| {
                    format!(
                        "cache {}% (session)",
                        cache_percent(ctx.total_cached(), ctx.total_input()).unwrap_or(0)
                    )
                });
                match (last, session) {
                    (Some(last), Some(session)) => Some(format!("{last} · {session}")),
                    (Some(last), None) => Some(last),
                    (None, Some(session)) => Some(session),
                    (None, None) => None,
                }
            }
            None => None,
        };
        let cache_line = cache
            .map(|cache| format!("  cache: {cache}\n"))
            .unwrap_or_default();
        let text = format!(
            "codest status\n  backend: {}\n  mode: {}\n  transcript: {} cells\n  context: {}\n{}  tokens: session in {} / out {}\n  commands: {}\n  plugins: {}",
            ctx.model(),
            ctx.mode(),
            ctx.transcript_len(),
            context_line(
                ctx.last_usage()
                    .map(|(input, output)| input.saturating_add(output)),
                ctx.context_window(),
            ),
            cache_line,
            format_tokens(ctx.total_input()),
            format_tokens(ctx.total_output()),
            ctx.command_table().len(),
            ctx.plugin_ids().join(", ")
        );
        ctx.notice(text);
        Ok(())
    }
}
