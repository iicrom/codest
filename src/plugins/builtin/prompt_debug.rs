//! `/context`: show what actually goes into the model's prompt — the
//! assembled system prompt with per-plugin attribution, the request's
//! destination and static fields, the tools offered, and token usage.
//!
//! The prompt-debug report: like [`crate::plugins::builtin::status`], it only
//! reads from [`PluginContext`] and renders one report block. Anything a
//! plugin injects via `system_prompt` or `tools` shows up here automatically,
//! which is how future skills/workspace-instruction plugins surface.

use crate::core::tokens::{format_context_usage, format_tokens};
use crate::plugins::{
    ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError, ReportBlock,
    ReportEntry, ReportGroup,
};

pub static PROMPT_DEBUG: PromptDebug = PromptDebug;

pub struct PromptDebug;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "context",
    &[],
    "/context",
    "show the assembled prompt, request config, and tools",
    ArgsPolicy::None,
)];

/// Longest section text shown per entry before clipping, mirroring the
/// loaded-context panels of reference TUIs.
const SECTION_MAX_CHARS: usize = 800;

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Clip long text for display, keeping the head and marking the cut.
fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(" …(+");
    out.push_str(&(text.chars().count() - max_chars).to_string());
    out.push_str(" chars)");
    out
}

impl Plugin for PromptDebug {
    fn id(&self) -> &'static str {
        "prompt_debug"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        _invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        let mut groups: Vec<ReportGroup> = Vec::new();

        // System prompt: the base prompt plus each plugin's fragment,
        // attributed so it is clear who contributes what.
        let sections = ctx.system_sections();
        groups.push(ReportGroup {
            heading: match sections.is_empty() {
                true => "System prompt".to_string(),
                false => format!(
                    "System prompt ({} section{})",
                    sections.len(),
                    plural(sections.len())
                ),
            },
            entries: match sections.is_empty() {
                true => vec![ReportEntry::new("", "(empty)")],
                false => sections
                    .iter()
                    .map(|(label, text)| ReportEntry::new(label, clip(text, SECTION_MAX_CHARS)))
                    .collect(),
            },
        });

        // Request: where turns go and which static fields ride along. Each
        // override becomes its own labeled entry, like the other fields.
        let request = ctx.request();
        let mut request_entries = vec![ReportEntry::new("model", ctx.model())];
        if request.endpoint.is_empty() {
            request_entries.push(ReportEntry::new("endpoint", "(no route configured)"));
        } else {
            request_entries.push(ReportEntry::new("protocol", request.protocol));
            request_entries.push(ReportEntry::new("endpoint", request.endpoint));
        }
        request_entries.push(ReportEntry::new(
            "effort",
            match ctx.effort() {
                Some(effort) => format!("{effort} → reasoning_effort"),
                None => "not sent".to_string(),
            },
        ));
        if request.overrides.is_empty() {
            request_entries.push(ReportEntry::new("body_overrides", "none"));
        } else {
            for (key, value) in request.overrides {
                request_entries.push(ReportEntry::new(key, value));
            }
        }
        request_entries.push(ReportEntry::new(
            "messages",
            format!("{} (system + transcript)", ctx.message_count()),
        ));
        groups.push(ReportGroup {
            heading: "Request".to_string(),
            entries: request_entries,
        });

        // Tools: every tool the plugins offered to the model this session.
        let tools = ctx.tools();
        groups.push(ReportGroup {
            heading: format!("Tools ({})", tools.len()),
            entries: match tools.is_empty() {
                true => vec![ReportEntry::new("", "none registered")],
                false => tools
                    .iter()
                    .map(|tool| ReportEntry::new(tool.name, tool.description))
                    .collect(),
            },
        });

        // Usage: the measured context size, as in the footer.
        let context = match ctx.last_usage().map(|(input, output)| input.saturating_add(output)) {
            Some(tokens) => {
                let mut line = format_context_usage(tokens, ctx.context_window());
                if ctx.context_window().is_none() {
                    line.push_str(" · window unknown");
                }
                line
            }
            None => "no measured turn yet".to_string(),
        };
        groups.push(ReportGroup {
            heading: "Usage".to_string(),
            entries: vec![
                ReportEntry::new("context", context),
                ReportEntry::new(
                    "tokens",
                    format!(
                        "session in {} / out {}",
                        format_tokens(ctx.total_input()),
                        format_tokens(ctx.total_output())
                    ),
                ),
            ],
        });

        ctx.report(ReportBlock {
            title: "context".to_string(),
            groups,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;
    use crate::core::ToolSpec;
    use crate::plugins::{PluginHost, PluginInfo, RequestFacts};

    static OVERRIDES: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
        vec![("temperature".to_string(), "0.2".to_string())]
    });
    static TEST_TOOLS: LazyLock<Vec<ToolSpec>> = LazyLock::new(|| {
        vec![ToolSpec {
            name: "ping",
            description: "reply with pong",
            parameters_json: "",
        }]
    });
    static CONTEXT_PLUGINS: &[&dyn Plugin] = &[&PROMPT_DEBUG];

    fn info<'a>(host: &'a PluginHost, sections: &'a [(String, String)]) -> PluginInfo<'a> {
        PluginInfo {
            model: "openai/gpt-test",
            mode: "fullscreen",
            transcript_len: 2,
            context_window: Some(200_000),
            last_usage: Some((8_100, 1_200)),
            total_input: 16_200,
            total_output: 2_400,
            plugin_ids: host.plugin_ids(),
            system_sections: sections,
            effort: Some("high"),
            request: RequestFacts {
                protocol: "openai-compat",
                base_url: "https://api.example.test/v1",
                endpoint: "https://api.example.test/v1/chat/completions",
                overrides: &OVERRIDES,
            },
            tools: &TEST_TOOLS,
            message_count: 3,
        }
    }

    fn dispatch(info: PluginInfo<'_>) -> ReportBlock {
        let host = PluginHost::new(CONTEXT_PLUGINS).expect("valid plugin");
        let invocation = match host.parse("/context") {
            crate::plugins::CommandInput::Command(invocation) => invocation,
            other => panic!("expected a command, got {other:?}"),
        };
        let effects = host.dispatch_command(&invocation, info);
        let mut effects = effects.into_iter();
        match effects.next() {
            Some(crate::plugins::PluginEffect::Report(block)) => {
                assert_eq!(block.title, "context");
                assert!(effects.next().is_none(), "one report, nothing else");
                block
            }
            other => panic!("expected a report effect, got {other:?}"),
        }
    }

    fn empty_host() -> PluginHost {
        PluginHost::new(&[]).expect("empty host")
    }

    fn group<'a>(block: &'a ReportBlock, heading_prefix: &str) -> &'a ReportGroup {
        block
            .groups
            .iter()
            .find(|group| group.heading.starts_with(heading_prefix))
            .unwrap_or_else(|| panic!("no `{heading_prefix}` group in {block:?}"))
    }

    fn entry<'a>(group: &'a ReportGroup, label: &str) -> &'a str {
        group
            .entries
            .iter()
            .find(|entry| entry.label == label)
            .map(|entry| entry.text.as_str())
            .unwrap_or_else(|| panic!("no `{label}` entry in {:?}", group.heading))
    }

    #[test]
    fn report_lists_prompt_request_tools_and_usage() {
        let sections = vec![
            ("base".to_string(), "You are codest.".to_string()),
            ("plugin:persona".to_string(), "Be terse.".to_string()),
        ];
        let host = empty_host();
        let block = dispatch(info(&host, &sections));

        let prompt = group(&block, "System prompt");
        assert_eq!(prompt.heading, "System prompt (2 sections)");
        assert_eq!(entry(prompt, "base"), "You are codest.");
        assert_eq!(entry(prompt, "plugin:persona"), "Be terse.");

        let request = group(&block, "Request");
        assert_eq!(entry(request, "model"), "openai/gpt-test");
        assert_eq!(
            entry(request, "endpoint"),
            "https://api.example.test/v1/chat/completions"
        );
        assert_eq!(entry(request, "effort"), "high → reasoning_effort");
        assert_eq!(entry(request, "temperature"), "0.2");
        assert_eq!(entry(request, "messages"), "3 (system + transcript)");
        assert!(!request.entries.iter().any(|entry| entry.label == "body_overrides"));

        let tools = group(&block, "Tools");
        assert_eq!(tools.heading, "Tools (1)");
        assert_eq!(entry(tools, "ping"), "reply with pong");

        let usage = group(&block, "Usage");
        assert!(entry(usage, "context").starts_with("9.3k/200.0k"));
        assert_eq!(entry(usage, "tokens"), "session in 16.2k / out 2.4k");
    }

    #[test]
    fn report_handles_empty_route_and_no_tools() {
        let host = empty_host();
        let mut bare = info(&host, &[]);
        bare.request = RequestFacts {
            protocol: "",
            base_url: "",
            endpoint: "",
            overrides: &[],
        };
        bare.effort = None;
        bare.tools = &[];
        let block = dispatch(bare);

        let prompt = group(&block, "System prompt");
        assert_eq!(prompt.heading, "System prompt");
        assert_eq!(prompt.entries[0].text, "(empty)");

        let request = group(&block, "Request");
        assert_eq!(entry(request, "endpoint"), "(no route configured)");
        assert_eq!(entry(request, "effort"), "not sent");
        assert_eq!(entry(request, "body_overrides"), "none");

        let tools = group(&block, "Tools");
        assert_eq!(tools.heading, "Tools (0)");
        assert_eq!(tools.entries[0].text, "none registered");
    }

    #[test]
    fn long_sections_are_clipped_with_a_marker() {
        let long = "x".repeat(2_000);
        let sections = vec![("base".to_string(), long.clone())];
        let host = empty_host();
        let block = dispatch(info(&host, &sections));
        let prompt = group(&block, "System prompt");
        let text = entry(prompt, "base");
        assert!(text.contains("…(+1200 chars)"));
        assert!(text.len() < long.len());
    }
}
