//! Transcript cells.
//!
//! Codex renders a large family of history cells (exec output, diffs, MCP
//! calls, approvals, …). A basic chat agent needs user, assistant, thinking,
//! error, notice, and an interruption marker — plus the tool-activity and
//! plugin-report cells the tool loop and `/context` introduced.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::plugins::ReportBlock;
use crate::plugins::ReportEntry;
use crate::tui::theme;
use crate::tui::wrap::display_width;
use crate::tui::wrap::wrap_str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCell {
    User(String),
    Assistant(String),
    /// The model's reasoning output for one turn. Display-only: folded to a
    /// one-line marker unless `thinking_expanded` is passed to [`lines`].
    Thinking(String),
    Error(String),
    Notice(String),
    Interrupted,
    /// One executed tool call: the invocation plus its output (or failure).
    /// Display-only bookkeeping — the conversation itself keeps the wire
    /// messages.
    ToolCall {
        name: String,
        arguments: String,
        result: String,
        error: Option<String>,
    },
    /// A plugin's structured report, e.g. `/context`: styled as a bold title
    /// over bold group headings and dim labeled entries.
    Report(ReportBlock),
}

impl HistoryCell {
    /// Render this cell into owned lines for a transcript of `width` columns.
    /// `thinking_expanded` controls whether [`HistoryCell::Thinking`] cells
    /// show their body or a one-line folded marker (`/thinking`).
    pub fn lines(&self, width: usize, thinking_expanded: bool) -> Vec<Line<'static>> {
        match self {
            HistoryCell::User(text) => {
                prefixed("› ", theme::USER, true, text, Style::default(), width)
            }
            HistoryCell::Assistant(text) => assistant_lines(text, width, false),
            HistoryCell::Thinking(text) => {
                if thinking_expanded {
                    thinking_lines(text, width, false)
                } else {
                    vec![thinking_hidden_line()]
                }
            }
            HistoryCell::Error(text) => prefixed(
                "! ",
                theme::ERROR,
                true,
                text,
                Style::default().fg(theme::ERROR),
                width,
            ),
            HistoryCell::Notice(text) => prefixed(
                "  ",
                theme::DIM,
                false,
                text,
                Style::default().fg(theme::DIM),
                width,
            ),
            HistoryCell::Interrupted => vec![Line::from(Span::styled(
                "  ■ interrupted",
                Style::default()
                    .fg(theme::WARNING)
                    .add_modifier(Modifier::DIM),
            ))],
            HistoryCell::ToolCall {
                name,
                arguments,
                result,
                error,
            } => tool_call_lines(name, arguments, result, error.as_deref(), width),
            HistoryCell::Report(block) => report_lines(block, width),
        }
    }
}

/// Render one executed tool call: the bold accent `⚙` invocation with dim
/// arguments, over its dim output (or the failure in the error color).
fn tool_call_lines(
    name: &str,
    arguments: &str,
    result: &str,
    error: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let label_style = Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(theme::DIM);
    let label = format!("⚙ {name}");

    // A parameterless call reads better without the `{}` noise.
    let mut lines: Vec<Line<'static>> = match arguments.trim() {
        "" | "{}" => vec![Line::from(Span::styled(label, label_style))],
        trimmed => {
            // Arguments are secondary: dim, wrapped after the name, with the
            // continuation aligned under the first argument row.
            let available = width.saturating_sub(display_width(&label) + 1).max(1);
            let rows = wrap_str(trimmed, available);
            let mut lines = vec![Line::from(vec![
                Span::styled(label.clone(), label_style),
                Span::raw(" "),
                Span::styled(rows[0].to_string(), text_style),
            ])];
            let continuation = " ".repeat(display_width(&label) + 1);
            for row in rows.into_iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::raw(continuation.clone()),
                    Span::styled(row, text_style),
                ]));
            }
            lines
        }
    };

    if let Some(message) = error {
        lines.extend(prefixed(
            "! ",
            theme::ERROR,
            false,
            message,
            Style::default().fg(theme::ERROR),
            width,
        ));
    } else if !result.is_empty() {
        lines.extend(prefixed(
            "  ",
            theme::DIM,
            false,
            result,
            Style::default().fg(theme::DIM),
            width,
        ));
    }
    lines
}

/// Render a plugin report the way reference TUIs style their loaded-context
/// panels: the bold accent title, then each group's bold subtle heading over
/// dim entries whose labels are picked out in the accent color.
fn report_lines(block: &ReportBlock, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("▪ {}", block.title),
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    ))];
    for group in &block.groups {
        out.push(Line::from(Span::styled(
            format!("  {}", group.heading),
            Style::default()
                .fg(theme::SUBTLE)
                .add_modifier(Modifier::BOLD),
        )));
        for entry in &group.entries {
            out.extend(report_entry_lines(entry, width));
        }
    }
    out
}

/// One report entry: the accent label and dim text share the first row,
/// wrapped and multi-line continuations keep the entry indent (aligned under
/// the text when a label is present).
fn report_entry_lines(entry: &ReportEntry, width: usize) -> Vec<Line<'static>> {
    const ENTRY_INDENT: &str = "    ";
    let label_style = Style::default().fg(theme::ACCENT);
    let text_style = Style::default().fg(theme::DIM);

    let label_prefix = if entry.label.is_empty() {
        String::new()
    } else {
        format!("{}  ", entry.label)
    };
    let available = width
        .saturating_sub(display_width(ENTRY_INDENT) + display_width(&label_prefix))
        .max(1);
    let text = normalize(&entry.text);

    let mut out = Vec::new();
    // Only the very first row carries the label; its wrapped rows align under
    // the text start (blank columns), later logical lines return to the indent.
    let continuation_indent = if label_prefix.is_empty() {
        ENTRY_INDENT.to_string()
    } else {
        format!("{ENTRY_INDENT}{}", " ".repeat(display_width(&label_prefix)))
    };
    for (logical_index, logical) in text.split('\n').enumerate() {
        let first_logical = logical_index == 0;
        for (row_index, row) in wrap_str(logical, available).into_iter().enumerate() {
            let first_row = first_logical && row_index == 0;
            let mut spans = Vec::new();
            if first_row && !label_prefix.is_empty() {
                spans.push(Span::styled(ENTRY_INDENT.to_string(), Style::default()));
                spans.push(Span::styled(entry.label.to_string(), label_style));
                spans.push(Span::raw("  ".to_string()));
            } else {
                let indent = if first_logical {
                    continuation_indent.clone()
                } else {
                    ENTRY_INDENT.to_string()
                };
                spans.push(Span::raw(indent));
            }
            spans.push(Span::styled(row, text_style));
            out.push(Line::from(spans));
        }
    }

    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// Render the streaming assistant message. While `streaming`, a block cursor
/// is appended to the last line so the transcript visibly keeps up.
pub fn assistant_lines(text: &str, width: usize, streaming: bool) -> Vec<Line<'static>> {
    let mut lines = prefixed("• ", theme::ASSISTANT, true, text, Style::default(), width);
    if streaming && let Some(last) = lines.last_mut() {
        last.spans
            .push(Span::styled("▌", Style::default().fg(theme::ACCENT)));
    }
    lines
}

/// Render the thinking block: a dim `✻ thinking` header over the dim body.
/// While `streaming`, a block cursor is appended to the last body line.
pub fn thinking_lines(text: &str, width: usize, streaming: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "✻ thinking",
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    ))];
    let mut body = prefixed(
        "  ",
        theme::DIM,
        false,
        text,
        Style::default().fg(theme::DIM),
        width,
    );
    if streaming && let Some(last) = body.last_mut() {
        last.spans
            .push(Span::styled("▌", Style::default().fg(theme::ACCENT)));
    }
    lines.append(&mut body);
    lines
}

/// The one-line stand-in shown for a folded thinking cell.
fn thinking_hidden_line() -> Line<'static> {
    Line::from(Span::styled(
        "✻ thinking (hidden · /thinking to expand)",
        Style::default().fg(theme::DIM),
    ))
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "")
        .replace('\t', "    ")
}

fn prefixed(
    prefix: &'static str,
    prefix_color: ratatui::style::Color,
    bold: bool,
    text: &str,
    content_style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let text = normalize(text);
    let width = width.max(4);
    let indent = " ".repeat(display_width(prefix));
    let available = width.saturating_sub(display_width(prefix)).max(1);
    let prefix_style = if bold {
        Style::default()
            .fg(prefix_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(prefix_color)
    };

    let mut lines = Vec::new();
    for (logical_index, logical) in text.split('\n').enumerate() {
        let rows = wrap_str(logical, available);
        for (row_index, row) in rows.into_iter().enumerate() {
            let mut spans = Vec::with_capacity(2);
            if logical_index == 0 && row_index == 0 {
                spans.push(Span::styled(prefix, prefix_style));
            } else {
                spans.push(Span::raw(indent.clone()));
            }
            spans.push(Span::styled(row, content_style));
            lines.push(Line::from(spans));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ReportGroup;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn user_cell_has_prompt_and_indent() {
        let lines = HistoryCell::User("hello world".to_string()).lines(10, true);
        assert_eq!(plain(&lines), vec!["› hello", "  world"]);
    }

    #[test]
    fn assistant_cell_wraps_and_marks_streaming() {
        let lines = assistant_lines("hello world", 8, true);
        assert_eq!(plain(&lines), vec!["• hello", "  world▌"]);
    }

    #[test]
    fn notice_is_dim_and_indented() {
        let lines = HistoryCell::Notice("note".to_string()).lines(20, true);
        assert_eq!(plain(&lines), vec!["  note"]);
    }

    #[test]
    fn thinking_cell_expands_to_header_and_dim_body() {
        let lines = HistoryCell::Thinking("reasoning hard".to_string()).lines(20, true);
        assert_eq!(plain(&lines), vec!["✻ thinking", "  reasoning hard"]);
    }

    #[test]
    fn thinking_cell_folds_to_a_marker() {
        let lines = HistoryCell::Thinking("reasoning hard".to_string()).lines(60, false);
        assert_eq!(plain(&lines), vec!["✻ thinking (hidden · /thinking to expand)"]);
    }

    #[test]
    fn thinking_streaming_appends_a_cursor() {
        let lines = thinking_lines("deep thought", 20, true);
        assert_eq!(plain(&lines), vec!["✻ thinking", "  deep thought▌"]);
    }

    #[test]
    fn tool_call_cell_renders_invocation_and_dim_result() {
        let cell = HistoryCell::ToolCall {
            name: "ping".to_string(),
            arguments: "{}".to_string(),
            result: "pong".to_string(),
            error: None,
        };
        assert_eq!(plain(&cell.lines(40, true)), vec!["⚙ ping", "  pong"]);

        let cell = HistoryCell::ToolCall {
            name: "echo".to_string(),
            arguments: r#"{ "x": 1 }"#.to_string(),
            result: String::new(),
            error: Some("unknown tool".to_string()),
        };
        assert_eq!(
            plain(&cell.lines(40, true)),
            vec![r#"⚙ echo { "x": 1 }"#, "! unknown tool"]
        );
    }

    #[test]
    fn report_cell_renders_styled_groups_and_entries() {
        fn styles(line: &Line<'_>) -> Vec<(String, Style)> {
            line.spans
                .iter()
                .map(|span| (span.content.to_string(), span.style))
                .collect()
        }

        let cell = HistoryCell::Report(ReportBlock {
            title: "context".to_string(),
            groups: vec![ReportGroup {
                heading: "System prompt (1 section)".to_string(),
                entries: vec![
                    ReportEntry::new("base", "You are codest."),
                    ReportEntry::new("", "plain entry"),
                ],
            }],
        });
        let rendered = cell.lines(60, true);
        let text: Vec<String> = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(text[0], "▪ context");
        assert_eq!(text[1], "  System prompt (1 section)");
        assert_eq!(text[2], "    base  You are codest.");
        assert_eq!(text[3], "    plain entry");

        // Title and group heading are bold; the heading uses the subtle gray.
        let title = styles(&rendered[0]);
        assert!(title[0].1.add_modifier.contains(Modifier::BOLD));
        let heading = styles(&rendered[1]);
        assert!(heading[0].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(heading[0].1.fg, Some(theme::SUBTLE));
        // Entry labels are accent, entry text dim, continuations aligned
        // under the text.
        let labeled = styles(&rendered[2]);
        assert_eq!(labeled[1], ("base".to_string(), Style::default().fg(theme::ACCENT)));
        assert_eq!(labeled[3], ("You are codest.".to_string(), Style::default().fg(theme::DIM)));
    }

    #[test]
    fn report_entry_wraps_and_aligns_continuations() {
        let cell = HistoryCell::Report(ReportBlock {
            title: "context".to_string(),
            groups: vec![ReportGroup {
                heading: "Request".to_string(),
                entries: vec![ReportEntry::new("endpoint", "https://example.test/v1/chat/completions long tail")],
            }],
        });
        let rendered = cell.lines(40, true);
        let text: Vec<String> = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Continuation rows align under the entry text, after the label
        // (4-space entry indent + `endpoint` + two separators = 14 columns).
        assert!(text[2].starts_with("    endpoint  https://"));
        assert!(text[3].starts_with(&" ".repeat(14)));
        assert_eq!(text[3].trim_start(), "at/completions long tail");
        assert!(!text[3].trim().is_empty());
    }
}
