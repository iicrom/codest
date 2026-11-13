//! Card chrome and row layout for the full-frame list screens.
//!
//! A "card" is a bordered block with a title and a one-line subtitle on its
//! top border, one row per item inside, and a bottom border. `/settings`
//! draws a card per section today; any screen that groups listed options can
//! use the same pieces, so the border glyphs, the pointer gutter, and the
//! value column stay identical across screens.
//!
//! Everything here is presentation only: the caller decides what a row's
//! label and value are, and which row has the focus.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::theme;
use crate::tui::wrap::display_width;

/// The pointer gutter of a focused row (`❯ `, padded to two cells).
pub const POINTER: &str = "❯ ";
/// The same gutter, empty, so unfocused rows line up under focused ones.
pub const POINTER_EMPTY: &str = "  ";

/// Width of the pointer gutter.
pub const GUTTER: usize = 2;
/// Width of the card borders (`│ ` on the left, ` │` on the right).
pub const FRAME: usize = 4;

/// The columns a row has to work with inside a card of `width`.
pub fn row_width(width: usize) -> usize {
    width.saturating_sub(FRAME + GUTTER)
}

/// One row inside a card: a pointer, a label, and a right-aligned value.
pub struct Row {
    pub label: String,
    pub value: String,
    pub focused: bool,
    pub value_style: Style,
}

impl Row {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            focused: false,
            value_style: Style::default(),
        }
    }

    /// Mark the row as the focused one (pointer gutter + bold label).
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// The style of the right-aligned value chip.
    pub fn value_style(mut self, style: Style) -> Self {
        self.value_style = style;
        self
    }

    /// Lay the row out in `width` columns: the value keeps a stable right
    /// edge, so a long label truncates before a long value is squeezed out.
    /// Both sides are clipped rather than allowed to push the right wall off
    /// screen.
    pub fn line(&self, width: usize) -> Line<'static> {
        let available = row_width(width);
        let value_budget = (available / 2).max(6);
        let value = if display_width(&self.value) > value_budget {
            crate::tui::session::truncated(&self.value, value_budget)
        } else {
            self.value.clone()
        };
        let value_width = display_width(&value);
        let label_budget = available.saturating_sub(value_width + 2);
        let label = if display_width(&self.label) > label_budget {
            crate::tui::session::truncated(&self.label, label_budget.max(4))
        } else {
            self.label.clone()
        };
        let pad = label_budget.saturating_sub(display_width(&label)).max(1);
        let pointer_style = if self.focused {
            Style::default().fg(theme::ACCENT)
        } else {
            Style::default().fg(theme::DIM)
        };
        let label_style = if self.focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled(
                if self.focused { POINTER } else { POINTER_EMPTY },
                pointer_style,
            ),
            Span::styled(label, label_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(value, self.value_style),
        ])
    }
}

/// `╭─ title ─ subtitle ─────╮`
pub fn top(title: &str, subtitle: &str, width: usize) -> Line<'static> {
    let inner = width.saturating_sub(2);
    let head = format!("─ {title} ─ {subtitle} ");
    let head_width = display_width(&head);
    let fill = inner.saturating_sub(head_width);
    Line::from(vec![
        Span::styled("╭", Style::default().fg(theme::DIM)),
        Span::styled(
            crate::tui::session::truncated(&head, inner),
            Style::default()
                .fg(theme::SUBTLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(fill), Style::default().fg(theme::DIM)),
        Span::styled("╮", Style::default().fg(theme::DIM)),
    ])
}

/// `│ <row>  │`, padded to the full width.
pub fn body(row: Line<'static>, width: usize) -> Line<'static> {
    let inner = width.saturating_sub(FRAME);
    let row_width: usize = row
        .spans
        .iter()
        .map(|span| display_width(&span.content))
        .sum();
    let mut spans = vec![Span::styled("│ ", Style::default().fg(theme::DIM))];
    spans.extend(row.spans);
    spans.push(Span::raw(" ".repeat(inner.saturating_sub(row_width))));
    spans.push(Span::styled(" │", Style::default().fg(theme::DIM)));
    Line::from(spans)
}

/// `╰──────────╯`
pub fn bottom(width: usize) -> Line<'static> {
    let style = Style::default().fg(theme::DIM);
    Line::from(vec![
        Span::styled("╰", style),
        Span::styled("─".repeat(width.saturating_sub(2)), style),
        Span::styled("╯", style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_body_line_fills_exactly_the_frame_width() {
        let row = Row::new("Enabled", "[x]").focused(true);
        for width in [24usize, 40, 60, 100] {
            let line = body(row.line(width), width);
            assert_eq!(display_width(&plain(&line)), width, "{width}: {line:?}");
        }
    }

    #[test]
    fn a_long_value_truncates_before_a_long_label_loses_its_padding() {
        let row = Row::new(
            "A label that is far too long to fit in a narrow card",
            "a very long stored value that would not fit either",
        );
        let line = plain(&body(row.line(40), 40));
        assert!(line.contains('…'), "{line}");
        assert_eq!(display_width(&line), 40, "{line}");
    }

    #[test]
    fn only_the_focused_row_carries_the_pointer() {
        let row = |focused| plain(&Row::new("Row", "on").focused(focused).line(40));
        assert!(row(true).starts_with(POINTER), "{}", row(true));
        assert!(row(false).starts_with(POINTER_EMPTY), "{}", row(false));
    }

    #[test]
    fn the_borders_bracket_the_width() {
        let top_line = plain(&top("tps", "throughput meter", 40));
        assert!(
            top_line.starts_with("╭─ tps ─ throughput meter "),
            "{top_line}"
        );
        assert!(top_line.ends_with('╮'), "{top_line}");
        assert_eq!(display_width(&top_line), 40);

        let bottom_line = plain(&bottom(40));
        assert!(bottom_line.starts_with('╰') && bottom_line.ends_with('╯'));
        assert_eq!(display_width(&bottom_line), 40);
    }
}
