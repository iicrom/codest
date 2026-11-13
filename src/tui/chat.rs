//! Transcript cells.
//!
//! Codex renders a large family of history cells (exec output, diffs, MCP
//! calls, approvals, …). A basic chat agent needs user, assistant, thinking,
//! error, notice, and an interruption marker — plus the tool-activity and
//! plugin-report cells the tool loop and `/context` introduced.
//!
//! Thinking and tool calls render as foldable cards, styled the way
//! cordis-harness-tui styles its transcript so they cannot be mistaken for
//! ordinary output. Each card is a header in its own color plus a body that
//! hangs from a spine:
//!
//! ```text
//! ⚓ thinking · 4.2s
//!   │ dim italic reasoning…
//!
//! ● bash(cargo build) · 4.2s
//!  ⎿  Compiling codest
//!     … +3 lines (ctrl+o to expand)
//! ```
//!
//! The tool header is `● name(args) · duration` with the name in the tool's
//! category color and the `⎿` spine tinted to match; the thinking header is
//! `⚓ thinking · duration` in the thinking color over a `│` rail. Folded, each
//! card is that one header line plus a dim hint naming its own expand key —
//! `ctrl+t` for thinking, `ctrl+o` for tool output, since the two fold
//! independently. Which cards show their body is decided per frame by
//! [`CardDisplay`], resolved from the global toggles — cells themselves stay
//! stateless.

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::plugins::ReportBlock;
use crate::plugins::ReportEntry;
use crate::tui::theme;
use crate::tui::wrap::display_width;
use crate::tui::wrap::wrap_str;

/// Braille spinner frames shared by the streaming thinking header and the
/// running tool card. `App::tick` advances them every 90 ms while a turn is
/// in flight.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many result lines a folded tool card previews before the `… +N lines`
/// hint — cordis caps a folded text body at three rows.
const TOOL_BODY_MAX_LINES: usize = 3;

/// Body spine of a tool card: the `⎿` bracket on the first row, blank indent
/// under it afterwards (both four columns wide, so rows align).
const BRACKET_FIRST: &str = " ⎿  ";
const BRACKET_REST: &str = "    ";
/// Body spine of a thinking card: a continuous vertical rail on every row,
/// so an expanded block reads as one hanging unit.
const RAIL: &str = "  │ ";

/// Per-cell display flags, resolved by the front-end from its global toggles
/// (Ctrl+T / `/thinking` and Ctrl+O) before rendering. The two card kinds
/// fold independently, so one flag each. Cells stay stateless: folding is
/// decided per frame, exactly like codex's `HistoryRenderMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardDisplay {
    /// Thinking cards show their body instead of the folded one-liner
    /// (Ctrl+T, or `/thinking`).
    pub thinking: bool,
    /// Tool cards show their full output instead of the head preview
    /// (Ctrl+O).
    pub tool_detail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCell {
    User(String),
    Assistant(String),
    /// The model's reasoning output for one turn. Display-only: a card that
    /// folds to a one-line marker unless `CardDisplay::thinking` is set.
    /// `duration_ms` records how long the stream ran, for the header chip.
    Thinking {
        text: String,
        duration_ms: Option<u64>,
    },
    Error(String),
    Notice(String),
    Interrupted,
    /// One executed tool call: the invocation plus its output (or failure).
    /// Display-only bookkeeping — the conversation itself keeps the wire
    /// messages. `duration_ms` is the measured wall-clock time of the call
    /// (`None` when replayed from a rollout, which does not record it).
    ToolCall {
        name: String,
        arguments: String,
        result: String,
        error: Option<String>,
        duration_ms: Option<u64>,
    },
    /// A plugin's structured report, e.g. `/context`: styled as a bold title
    /// over bold group headings and dim labeled entries.
    Report(ReportBlock),
}

impl HistoryCell {
    /// Render this cell into owned lines for a transcript of `width` columns.
    /// `display` carries the per-frame fold flags for the card cells.
    pub fn lines(&self, width: usize, display: &CardDisplay) -> Vec<Line<'static>> {
        match self {
            HistoryCell::User(text) => {
                prefixed("› ", theme::USER, true, text, Style::default(), width)
            }
            HistoryCell::Assistant(text) => assistant_lines(text, width, false),
            HistoryCell::Thinking { text, duration_ms } => {
                thinking_card_lines(text, width, display.thinking, *duration_ms)
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
                duration_ms,
            } => tool_card_lines(
                name,
                arguments,
                result,
                error.as_deref(),
                *duration_ms,
                display.tool_detail,
                width,
            ),
            HistoryCell::Report(block) => report_lines(block, width),
        }
    }
}

/// Render one executed tool call as a card: the status glyph, the bold
/// category-colored tool name with its argument summary in parentheses, and
/// the duration chip on the header row; the output then hangs under a `⎿`
/// spine tinted like the header. Folded (`expanded == false`) the output shows
/// a head preview and a dim `… +N lines` hint; a failure rides in the body, in
/// the error color.
fn tool_card_lines(
    name: &str,
    arguments: &str,
    result: &str,
    error: Option<&str>,
    duration_ms: Option<u64>,
    expanded: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let category = tool_category_color(name);
    let gutter = Gutter::bracket(category);
    let glyph_style = Style::default().fg(if error.is_some() {
        theme::ERROR
    } else {
        category
    });
    let name_style = Style::default().fg(category).add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(theme::DIM);
    let glyph = if error.is_some() { "✗" } else { "●" };

    // The duration chip rides at the end of the header; the argument summary
    // bows out early enough to keep it visible. The parentheses wrapping the
    // summary cost two more columns.
    let chip = duration_ms.map(format_duration).map(|d| format!(" · {d}"));
    let chip_width = chip.as_ref().map(|c| display_width(c)).unwrap_or(0);
    let spent = display_width(glyph) + 1 + display_width(name) + 2 + chip_width;
    let args = {
        let budget = width.saturating_sub(spent).max(1);
        tool_summary(arguments, budget)
    };
    let has_summary = !args.summary.is_empty();
    let mut header = vec![
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" ".to_string()),
        Span::styled(name.to_string(), name_style),
    ];
    if has_summary {
        header.push(Span::styled(format!("({})", args.summary), text_style));
    }
    if let Some(chip) = chip {
        // The chip carries its own ` · ` separator.
        header.push(Span::styled(chip, text_style));
    }
    let mut lines = vec![Line::from(header)];

    // An expanded card keeps the full arguments above the output when the
    // header had to fold them (a multi-line bash command, typically).
    if expanded && args.folded && has_summary {
        lines.extend(gutter_lines(&args.full, width, text_style, &gutter));
    }

    if let Some(message) = error {
        lines.extend(gutter_lines(
            message,
            width,
            Style::default().fg(theme::ERROR),
            &gutter,
        ));
    } else if !result.trim().is_empty() {
        let (mut body, hidden) = capped_gutter_lines(
            result,
            width,
            if expanded {
                usize::MAX
            } else {
                TOOL_BODY_MAX_LINES
            },
            text_style,
            &gutter,
        );
        if hidden > 0 {
            let noun = if hidden == 1 { "line" } else { "lines" };
            body.push(Line::from(Span::styled(
                format!(" … +{hidden} {noun} (ctrl+o to expand)"),
                text_style,
            )));
        }
        lines.extend(body);
    } else {
        lines.push(Line::from(vec![
            Span::styled(gutter.first.to_string(), gutter.style),
            Span::styled("(no output)".to_string(), text_style),
        ]));
    }
    lines
}

/// The live stand-in shown while a tool executes: the braille spinner in the
/// status-glyph slot, the invocation in the header, and the elapsed row
/// hanging under the body spine. [`tool_card_lines`] replaces it once the call
/// completes.
pub fn running_tool_lines(
    name: &str,
    arguments: &str,
    elapsed: &str,
    spinner_frame: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let category = tool_category_color(name);
    let name_style = Style::default().fg(category).add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(theme::DIM);

    let spent = display_width(spinner_frame) + 1 + display_width(name) + 2;
    let budget = width.saturating_sub(spent).max(1);
    let summary = tool_summary(arguments, budget).summary;

    let mut header = vec![
        Span::styled(spinner_frame.to_string(), Style::default().fg(category)),
        Span::raw(" ".to_string()),
        Span::styled(name.to_string(), name_style),
    ];
    if !summary.is_empty() {
        header.push(Span::styled(format!("({summary})"), text_style));
    }
    let mut lines = vec![Line::from(header)];
    lines.push(Line::from(vec![
        Span::styled(BRACKET_FIRST.to_string(), Style::default().fg(category)),
        Span::styled(format!("Running… ({elapsed})"), text_style),
    ]));
    lines
}

/// The tool's arguments as a card header shows them.
struct ToolArgs {
    /// One-line summary: the command for the shell tool (`bash` / `pwsh`), the
    /// path for the file tools, the raw JSON otherwise.
    summary: String,
    /// The full value [`Self::summary`] folded out of (the whole command or
    /// path string) — an expanded card shows it in the body when the header
    /// had to fold it.
    full: String,
    /// True when the value was multi-line or clipped to the header budget.
    folded: bool,
}

/// Build the header summary. Multi-line values fold to their first row and
/// over-long ones clip at `max` display columns; `folded` is true when
/// either happened.
fn tool_summary(arguments: &str, max: usize) -> ToolArgs {
    let trimmed = arguments.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return ToolArgs {
            summary: String::new(),
            full: String::new(),
            folded: false,
        };
    }
    let parsed: Option<serde_json::Value> = serde_json::from_str(trimmed).ok();
    let field = match &parsed {
        Some(serde_json::Value::Object(map)) => {
            let key = if map.contains_key("command") {
                "command"
            } else {
                "path"
            };
            map.get(key).and_then(serde_json::Value::as_str)
        }
        _ => None,
    };
    let full = field.map_or_else(|| trimmed.to_string(), str::to_string);
    let first_line = full.lines().next().unwrap_or("").trim_end();
    let clipped = display_width(first_line) > max;
    let summary = if clipped {
        let mut row = wrap_str(first_line, max.saturating_sub(1).max(1))
            .into_iter()
            .next()
            .unwrap_or_default();
        row.push('…');
        row
    } else {
        first_line.to_string()
    };
    let folded = full.contains('\n') || clipped;
    ToolArgs {
        summary,
        full,
        folded,
    }
}

/// A card's category color: the status glyph and tool name carry what the
/// tool *is*, not how it ended (the glyph itself reports success/failure).
fn tool_category_color(name: &str) -> Color {
    match name {
        // The host's shell tool: `bash` on unix, `pwsh` on Windows. Both
        // names are listed so a card replayed from a session recorded on the
        // other platform still colors as a command runner.
        "bash" | "pwsh" => theme::TOOL_EXEC,
        "read" => theme::TOOL_READ,
        "write" | "edit" => theme::TOOL_MUTATE,
        _ => theme::ACCENT,
    }
}

/// `123ms`, `4.2s`, `1m 05s` — the duration chip format shared by the tool
/// and thinking cards.
pub fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1_000)
    }
}

/// A card's body spine: the chrome prefixing its first row, the indent that
/// keeps every later row aligned, and the style the spine carries — tinted
/// like the card's header, so a card body reads as part of the card rather
/// than as loose transcript text.
struct Gutter {
    first: &'static str,
    rest: &'static str,
    style: Style,
}

impl Gutter {
    /// The tool card's ` ⎿  ` bracket, in the tool's category color.
    fn bracket(color: Color) -> Self {
        Self {
            first: BRACKET_FIRST,
            rest: BRACKET_REST,
            style: Style::default().fg(color),
        }
    }

    /// The thinking card's `  │ ` rail, in the thinking color.
    fn rail() -> Self {
        Self {
            first: RAIL,
            rest: RAIL,
            style: Style::default().fg(theme::THINKING),
        }
    }
}

/// Wrap `text` under `gutter`, styling the body with `style`.
fn gutter_lines(text: &str, width: usize, style: Style, gutter: &Gutter) -> Vec<Line<'static>> {
    let available = width.saturating_sub(display_width(gutter.first)).max(1);
    let mut lines = Vec::new();
    for (logical_index, logical) in normalize(text).split('\n').enumerate() {
        for (row_index, row) in wrap_str(logical, available).into_iter().enumerate() {
            let spine = if logical_index == 0 && row_index == 0 {
                gutter.first
            } else {
                gutter.rest
            };
            lines.push(Line::from(vec![
                Span::styled(spine.to_string(), gutter.style),
                Span::styled(row, style),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            gutter.first.to_string(),
            gutter.style,
        )));
    }
    lines
}

/// [`gutter_lines`] limited to `max` logical lines (trailing blanks are not
/// counted); returns the lines and how many logical lines were hidden.
fn capped_gutter_lines(
    text: &str,
    width: usize,
    max: usize,
    style: Style,
    gutter: &Gutter,
) -> (Vec<Line<'static>>, usize) {
    let mut logical: Vec<String> = normalize(text).split('\n').map(str::to_owned).collect();
    while logical.last().is_some_and(|line| line.trim().is_empty()) {
        logical.pop();
    }
    if max >= logical.len() {
        return (gutter_lines(&logical.join("\n"), width, style, gutter), 0);
    }
    let hidden = logical.len() - max;
    (
        gutter_lines(&logical[..max].join("\n"), width, style, gutter),
        hidden,
    )
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

/// Render an assistant message as markdown. While `streaming`, a block cursor
/// is appended to the last line so the transcript visibly keeps up.
///
/// The first row carries the `•` gutter and every later row is indented under
/// the message text, so headings, lists, quote bars, tables, and code fences
/// keep their own markers inside the transcript's indent.
pub fn assistant_lines(text: &str, width: usize, streaming: bool) -> Vec<Line<'static>> {
    assistant_lines_tracked(text, width, streaming).0
}

/// [`assistant_lines`] plus the number of leading lines that will not change
/// when more of the message streams in. Inline mode prints exactly that prefix
/// to the terminal's scrollback, which cannot be rewritten afterwards.
pub fn assistant_lines_tracked(
    text: &str,
    width: usize,
    streaming: bool,
) -> (Vec<Line<'static>>, usize) {
    let width = width.max(4);
    const GUTTER: usize = 2;
    let (mut lines, settled) =
        crate::tui::markdown::render_tracked(text, width.saturating_sub(GUTTER).max(1));
    for (index, line) in lines.iter_mut().enumerate() {
        let pad = if index == 0 {
            Span::styled(
                "• ",
                Style::default()
                    .fg(theme::ASSISTANT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        line.spans.insert(0, pad);
    }
    if streaming && let Some(last) = lines.last_mut() {
        last.spans
            .push(Span::styled("▌", Style::default().fg(theme::ACCENT)));
    }
    (lines, settled)
}

/// The settled thinking card: a `⚓ thinking` header in the thinking color
/// (with the duration chip when the stream was timed) over the dim italic
/// body hanging under the thinking rail, or — folded — a one-line marker
/// naming the expand key.
pub fn thinking_card_lines(
    text: &str,
    width: usize,
    expanded: bool,
    duration_ms: Option<u64>,
) -> Vec<Line<'static>> {
    let mut header = thinking_header(duration_ms);
    if !expanded {
        header.push(Span::styled(
            " (ctrl+t to expand)".to_string(),
            Style::default().fg(theme::DIM),
        ));
        return vec![Line::from(header)];
    }
    let mut lines = vec![Line::from(header)];
    lines.extend(gutter_lines(
        text,
        width,
        Style::default()
            .fg(theme::DIM)
            .add_modifier(Modifier::ITALIC),
        &Gutter::rail(),
    ));
    lines
}

/// The `⚓ thinking` header spans, with the duration chip when the stream was
/// timed. The glyph is bold so the card keeps an anchor even when the rest of
/// the row recedes.
fn thinking_header(duration_ms: Option<u64>) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(
            "⚓".to_string(),
            Style::default()
                .fg(theme::THINKING)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".to_string()),
        Span::styled("thinking".to_string(), Style::default().fg(theme::THINKING)),
    ];
    if let Some(ms) = duration_ms {
        spans.push(Span::styled(
            format!(" · {}", format_duration(ms)),
            Style::default().fg(theme::DIM),
        ));
    }
    spans
}

/// The thinking card while its stream is live (the transcript tail): a
/// braille spinner header with the dim italic body on the rail underneath and
/// the block cursor riding the last row. The body shows only when `expanded`;
/// the spinner alone still signals that reasoning is happening.
pub fn thinking_live_lines(
    text: &str,
    width: usize,
    expanded: bool,
    spinner_frame: &str,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            spinner_frame.to_string(),
            Style::default().fg(theme::THINKING),
        ),
        Span::raw(" ".to_string()),
        Span::styled(
            "thinking…".to_string(),
            Style::default()
                .fg(theme::THINKING)
                .add_modifier(Modifier::ITALIC),
        ),
    ])];
    if expanded {
        let mut body = gutter_lines(
            text,
            width,
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
            &Gutter::rail(),
        );
        if let Some(last) = body.last_mut() {
            last.spans
                .push(Span::styled("▌", Style::default().fg(theme::ACCENT)));
        }
        lines.append(&mut body);
    }
    lines
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "")
        .replace('\t', "    ")
}

fn prefixed(
    prefix: &'static str,
    prefix_color: Color,
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

    fn expanded_all() -> CardDisplay {
        CardDisplay {
            thinking: true,
            tool_detail: true,
        }
    }

    fn folded_all() -> CardDisplay {
        CardDisplay {
            thinking: false,
            tool_detail: false,
        }
    }

    #[test]
    fn user_cell_has_prompt_and_indent() {
        let lines = HistoryCell::User("hello world".to_string()).lines(10, &expanded_all());
        assert_eq!(plain(&lines), vec!["› hello", "  world"]);
    }

    #[test]
    fn assistant_cell_wraps_and_marks_streaming() {
        let lines = assistant_lines("hello world", 8, true);
        assert_eq!(plain(&lines), vec!["• hello", "  world▌"]);
    }

    #[test]
    fn assistant_cell_renders_markdown_under_the_gutter() {
        let lines = assistant_lines("# Title\n\n- one\n- two", 40, false);
        assert_eq!(plain(&lines), vec!["• Title", "  ", "  • one", "  • two"]);
        // The `•` gutter is accent-bold; list markers keep their own style.
        let first = &lines[0].spans[0];
        assert_eq!(first.style.fg, Some(theme::ASSISTANT));
        assert!(first.style.add_modifier.contains(Modifier::BOLD));

        // A fenced block keeps its lines, indented under the message.
        let lines = assistant_lines("```\na\n```", 40, false);
        assert_eq!(plain(&lines), vec!["• │ a"]);
    }

    #[test]
    fn assistant_cell_reports_its_settled_prefix() {
        // The last block is still growing, so only the heading is settled.
        let (lines, settled) = assistant_lines_tracked("# done\n\nstill typing", 40, true);
        assert_eq!(plain(&lines), vec!["• done", "  ", "  still typing▌"]);
        assert_eq!(settled, 1);

        // A trailing blank line closes the message: everything is settled.
        let (lines, settled) = assistant_lines_tracked("done\n\n", 40, false);
        assert_eq!(settled, lines.len());
    }

    #[test]
    fn notice_is_dim_and_indented() {
        let lines = HistoryCell::Notice("note".to_string()).lines(20, &expanded_all());
        assert_eq!(plain(&lines), vec!["  note"]);
    }

    #[test]
    fn thinking_cell_expands_to_header_and_italic_body() {
        let cell = HistoryCell::Thinking {
            text: "reasoning hard".to_string(),
            duration_ms: None,
        };
        assert_eq!(
            plain(&cell.lines(20, &expanded_all())),
            vec!["⚓ thinking", "  │ reasoning hard"]
        );

        // The rail is the thinking card's own color, so the body reads as part
        // of the card; the text itself stays dim italic.
        let rendered = cell.lines(20, &expanded_all());
        assert_eq!(rendered[0].spans[0].style.fg, Some(theme::THINKING));
        assert_eq!(
            rendered[1].spans[0].style.fg,
            Some(theme::THINKING),
            "the `│` spine carries the card color"
        );
        assert_eq!(
            rendered[1].spans[1].style.fg,
            Some(theme::DIM),
            "and the reasoning text stays dim italic"
        );
        assert!(
            rendered[1]
                .spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn thinking_cell_folds_to_a_marker_with_the_expand_key() {
        let cell = HistoryCell::Thinking {
            text: "reasoning hard".to_string(),
            duration_ms: Some(4_200),
        };
        assert_eq!(
            plain(&cell.lines(60, &folded_all())),
            vec!["⚓ thinking · 4.2s (ctrl+t to expand)"]
        );

        // Even folded the header is the thinking color, not the notice gray.
        let rendered = cell.lines(60, &folded_all());
        assert_eq!(rendered[0].spans[2].style.fg, Some(theme::THINKING));
        assert_eq!(
            rendered[0].spans[4].style.fg,
            Some(theme::DIM),
            "the expand hint recedes"
        );
    }

    #[test]
    fn thinking_live_card_spins_and_cursor_marks_the_tail() {
        let lines = thinking_live_lines("deep thought", 20, true, "⠹");
        assert_eq!(plain(&lines), vec!["⠹ thinking…", "  │ deep thought▌"]);

        // Folded, the spinner header alone keeps signaling that reasoning is
        // happening.
        let lines = thinking_live_lines("deep thought", 20, false, "⠹");
        assert_eq!(plain(&lines), vec!["⠹ thinking…"]);
    }

    #[test]
    fn tool_card_previews_its_output_and_folds_the_rest() {
        let cell = HistoryCell::ToolCall {
            name: "bash".to_string(),
            arguments: r#"{"command": "ls -la"}"#.to_string(),
            result: "a\nb\nc\nd\ne".to_string(),
            error: None,
            duration_ms: Some(4_200),
        };
        // Folded: header, the three-line head preview, then the hint.
        assert_eq!(
            plain(&cell.lines(60, &folded_all())),
            vec![
                "● bash(ls -la) · 4.2s",
                " ⎿  a",
                "    b",
                "    c",
                " … +2 lines (ctrl+o to expand)",
            ]
        );
        // Expanded: the whole output, no hint.
        assert_eq!(
            plain(&cell.lines(60, &expanded_all())),
            vec![
                "● bash(ls -la) · 4.2s",
                " ⎿  a",
                "    b",
                "    c",
                "    d",
                "    e",
            ]
        );
    }

    #[test]
    fn tool_card_shows_errors_and_empty_output() {
        let cell = HistoryCell::ToolCall {
            name: "echo".to_string(),
            arguments: r#"{ "x": 1 }"#.to_string(),
            result: String::new(),
            error: Some("unknown tool".to_string()),
            duration_ms: None,
        };
        // The failure rides in the card body, outside any cap, in the error
        // style; no preview follows a failed call.
        assert_eq!(
            plain(&cell.lines(40, &expanded_all())),
            vec![r#"✗ echo({ "x": 1 })"#, " ⎿  unknown tool"]
        );
        assert_eq!(
            cell.lines(40, &expanded_all())[1].spans[1].style.fg,
            Some(theme::ERROR)
        );

        let cell = HistoryCell::ToolCall {
            name: "ping".to_string(),
            arguments: "{}".to_string(),
            result: "pong".to_string(),
            error: None,
            duration_ms: None,
        };
        assert_eq!(
            plain(&cell.lines(40, &expanded_all())),
            vec!["● ping", " ⎿  pong"]
        );
    }

    #[test]
    fn tool_card_folds_multiline_commands_and_expansion_restores_them() {
        let cell = HistoryCell::ToolCall {
            name: "bash".to_string(),
            arguments: r#"{"command": "echo one\necho two"}"#.to_string(),
            result: "one\ntwo".to_string(),
            error: None,
            duration_ms: Some(120),
        };
        // The header carries the command's first row only.
        let folded = cell.lines(60, &folded_all());
        assert_eq!(plain(&folded)[0], "● bash(echo one) · 120ms");
        assert_eq!(folded.len(), 3, "header + two output rows, no args block");

        // Expanded, the full command rides above the output under the spine.
        assert_eq!(
            plain(&cell.lines(60, &expanded_all())),
            vec![
                "● bash(echo one) · 120ms",
                " ⎿  echo one",
                "    echo two",
                " ⎿  one",
                "    two",
            ]
        );
    }

    #[test]
    fn tool_card_glyph_and_name_follow_the_category() {
        let cell = HistoryCell::ToolCall {
            name: "edit".to_string(),
            arguments: r#"{"path": "a.rs"}"#.to_string(),
            result: "ok".to_string(),
            error: None,
            duration_ms: None,
        };
        let lines = cell.lines(40, &expanded_all());
        let glyph = &lines[0].spans[0];
        assert_eq!(glyph.content.as_ref(), "●");
        assert_eq!(glyph.style.fg, Some(theme::TOOL_MUTATE));
        assert_eq!(lines[0].spans[2].style.fg, Some(theme::TOOL_MUTATE));

        let cell = HistoryCell::ToolCall {
            name: "edit".to_string(),
            arguments: r#"{"path": "a.rs"}"#.to_string(),
            result: String::new(),
            error: Some("boom".to_string()),
            duration_ms: None,
        };
        let lines = cell.lines(40, &expanded_all());
        assert_eq!(lines[0].spans[0].content.as_ref(), "✗");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::ERROR));
    }

    #[test]
    fn tool_card_spine_is_tinted_like_its_header() {
        let cell = HistoryCell::ToolCall {
            name: "read".to_string(),
            arguments: r#"{"path": "src/main.rs"}"#.to_string(),
            result: "fn main() {}".to_string(),
            error: None,
            duration_ms: None,
        };
        let lines = cell.lines(40, &expanded_all());
        // The `⎿` spine takes the read category color, the body stays dim.
        assert_eq!(lines[1].spans[0].content.as_ref(), BRACKET_FIRST);
        assert_eq!(lines[1].spans[0].style.fg, Some(theme::TOOL_READ));
        assert_eq!(lines[1].spans[1].style.fg, Some(theme::DIM));
    }

    #[test]
    fn running_tool_card_spins_with_an_elapsed_row() {
        let lines = running_tool_lines("bash", r#"{"command": "cargo build"}"#, "12s", "⠸", 60);
        assert_eq!(
            plain(&lines),
            vec!["⠸ bash(cargo build)", " ⎿  Running… (12s)"]
        );
    }

    #[test]
    fn tool_summary_prefers_command_then_path_then_raw_json() {
        let args = tool_summary(r#"{"command": "ls -la"}"#, 40);
        assert_eq!(args.summary, "ls -la");
        assert_eq!(args.full, "ls -la");
        assert!(!args.folded);

        let args = tool_summary(r#"{"path": "src/main.rs", "limit": 10}"#, 40);
        assert_eq!(args.summary, "src/main.rs");
        assert!(!args.folded);

        // Unparseable or non-summary arguments fall back to the raw JSON.
        let args = tool_summary("{ not json", 40);
        assert_eq!(args.summary, "{ not json");
        assert!(!args.folded);

        // A multi-line command folds to its first row and reports the fold,
        // keeping the full command for the expanded body.
        let args = tool_summary(r#"{"command": "one\ntwo"}"#, 40);
        assert_eq!(args.summary, "one");
        assert_eq!(args.full, "one\ntwo");
        assert!(args.folded);
    }

    #[test]
    fn format_duration_scales_units() {
        assert_eq!(format_duration(120), "120ms");
        assert_eq!(format_duration(4_200), "4.2s");
        assert_eq!(format_duration(65_000), "1m 05s");
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
        let rendered = cell.lines(60, &expanded_all());
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
        assert_eq!(
            labeled[1],
            ("base".to_string(), Style::default().fg(theme::ACCENT))
        );
        assert_eq!(
            labeled[3],
            (
                "You are codest.".to_string(),
                Style::default().fg(theme::DIM)
            )
        );
    }

    #[test]
    fn report_entry_wraps_and_aligns_continuations() {
        let cell = HistoryCell::Report(ReportBlock {
            title: "context".to_string(),
            groups: vec![ReportGroup {
                heading: "Request".to_string(),
                entries: vec![ReportEntry::new(
                    "endpoint",
                    "https://example.test/v1/chat/completions long tail",
                )],
            }],
        });
        let rendered = cell.lines(40, &expanded_all());
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
