//! Transcript cells.
//!
//! Codex renders a large family of history cells (exec output, diffs, MCP
//! calls, approvals, …). A basic chat agent needs exactly six: user,
//! assistant, thinking, error, notice, and an interruption marker.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

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
        }
    }
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
}
