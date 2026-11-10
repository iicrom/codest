//! Minimal markdown rendering for the assistant transcript.
//!
//! Codex has a full renderer (`markdown_render.rs`: syntax-highlighted code,
//! tables with width allocation, mermaid, math, OSC 8 links, copy sources) and
//! dsh has a `marked` + ANSI one. This is the small core both share: headings,
//! paragraphs, lists, quotes, fenced code, rules, tables, and the usual inline
//! styles, rendered straight to styled `ratatui` lines.
//!
//! Every logical line is wrapped with the transcript's own Unicode-width rules,
//! and wrapped continuations stay under the line's gutter (`▎ `, `• `, `1. `),
//! which is what keeps list bodies and quote text aligned in the reference
//! TUIs.
//!
//! Rendering is stateless per call, so a streaming message re-renders from the
//! accumulated source every frame. [`render_tracked`] additionally reports how
//! many leading lines are already final, which is what inline mode needs: the
//! terminal's scrollback can never be rewritten, so only those lines may be
//! committed. A line counts as final once it was emitted with no inline tag,
//! code fence, or table open and holds no leftover delimiter — anything else
//! can still change as more text arrives (a `**` that later closes turns the
//! literal asterisks into bold text, an unfinished table flips from a
//! paragraph to aligned columns).

use std::sync::Arc;

use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::tui::highlight;
use crate::tui::theme;
use crate::tui::wrap::char_width;
use crate::tui::wrap::display_width;
use crate::tui::wrap::wrap_chars;
use crate::tui::wrap::wrap_chars_hard;

/// Render `source` into lines at most `width` columns wide.
///
/// The caller supplies the content width: it already accounts for whatever
/// gutter the transcript adds around these lines.
pub fn render(source: &str, width: usize) -> Vec<Line<'static>> {
    render_tracked(source, width).0
}

/// Render `source` and report how many leading lines are final.
///
/// "Final" means the line cannot change when more text is appended to
/// `source`. Only the last top-level block can still be edited: an unfinished
/// `**` turns into bold, a half-typed table flips from prose to columns, and
/// a fence keeps swallowing lines. Everything before that block is settled.
///
/// The parser closes all constructs at EOF, so the event stream alone cannot
/// tell an open block from a finished one; the byte offsets can, because the
/// last top-level `Start` marks where the mutable block begins.
///
/// Inline mode prints only `lines[..settled]` to the terminal's scrollback and
/// keeps the rest on screen.
pub fn render_tracked(source: &str, width: usize) -> (Vec<Line<'static>>, usize) {
    let width = width.max(8);
    let trimmed = source.trim_end();
    if trimmed.trim().is_empty() {
        return (vec![Line::default()], 0);
    }

    let parser = Parser::new_ext(trimmed, options()).into_offset_iter();
    let mut depth = 0usize;
    let mut last_block_start = 0usize;
    let mut writer = Writer::new(width);
    for (event, range) in parser {
        match &event {
            Event::Start(_) => {
                if depth == 0 {
                    last_block_start = range.start;
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            // Rules and block HTML are single events, not tag pairs.
            Event::Rule | Event::Html(_) if depth == 0 => last_block_start = range.start,
            _ => {}
        }
        writer.event(event);
    }
    let lines = writer.finish();

    // A trailing blank line closed the final block, so nothing is in flight.
    if ends_with_blank_line(source) {
        let settled = lines.len();
        return (lines, settled);
    }
    if last_block_start == 0 {
        // One block spans the whole message: nothing is safe to commit yet.
        return (lines, 0);
    }
    let settled = render_plain(trimmed[..last_block_start].trim_end(), width)
        .len()
        .min(lines.len());
    (lines, settled)
}

/// Whether `source` ends with a blank line, i.e. the last block is closed.
fn ends_with_blank_line(source: &str) -> bool {
    let source = source.trim_end_matches([' ', '\t', '\r']);
    source.ends_with("\n\n")
}

fn options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

fn render_plain(source: &str, width: usize) -> Vec<Line<'static>> {
    let mut writer = Writer::new(width);
    for event in Parser::new_ext(source, options()) {
        writer.event(event);
    }
    writer.finish()
}

struct Link {
    dest: String,
    label: Vec<Span<'static>>,
    is_image: bool,
}

/// A fenced block being collected: the body arrives one `Event::Text` at a
/// time, and the language is only known from the opening fence.
struct CodeBlock {
    lang: Option<String>,
    text: String,
}

/// The fence's info string reduced to a language token.
///
/// pulldown-cmark hands back everything after the backticks (`rust`, but also
/// `rust,ignore` or `rust title="x"`), so take the first word and drop the
/// punctuation people append to it.
fn normalize_lang(info: &str) -> Option<String> {
    let word = info.split_whitespace().next()?;
    let token = word
        .split([',', '{', '.'])
        .next()
        .unwrap_or(word)
        .trim_matches(['`', '"', '\'']);
    if token.is_empty() {
        return None;
    }
    Some(token.to_ascii_lowercase())
}

struct Table {
    alignments: Vec<Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    cell: Vec<Span<'static>>,
    in_cell: bool,
}

impl Table {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            row: Vec::new(),
            cell: Vec::new(),
            in_cell: false,
        }
    }

    fn columns(&self) -> usize {
        self.alignments
            .len()
            .max(self.header.len())
            .max(self.rows.iter().map(Vec::len).max().unwrap_or(0))
    }
}

/// Stateful pulldown-cmark consumer. The layout state is the whole point: the
/// `prefix` stack carries the current block's gutter (quote bars, list
/// markers, the code fence bar) and wrapped continuations are padded to its
/// display width.
struct Writer {
    width: usize,
    out: Vec<Line<'static>>,
    /// Gutter spans for the block being rendered: quote bars, list markers,
    /// the code-block bar.
    prefix: Vec<Span<'static>>,
    /// Prefix lengths to restore when the block that pushed them ends.
    marks: Vec<usize>,
    /// Inline style stack (strong, emphasis, strikethrough, heading, link).
    styles: Vec<Style>,
    /// Spans of the logical line being assembled.
    current: Vec<Span<'static>>,
    /// Whether `current` holds a line that still needs to be emitted.
    open: bool,
    /// A block just ended; the next block is preceded by one blank line.
    blank_pending: bool,
    /// Fenced code being collected, one `Event::Text` per source line.
    code: Option<CodeBlock>,
    /// Open lists: `Some(next_number)` for ordered, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Per open list item, the prefix length just before its marker was
    /// pushed. A nested item replaces its parent's marker with spaces so the
    /// ladder steps in by one marker width per level.
    items: Vec<usize>,
    link: Option<Link>,
    table: Option<Table>,
}

impl Writer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            prefix: Vec::new(),
            marks: Vec::new(),
            styles: Vec::new(),
            current: Vec::new(),
            open: false,
            blank_pending: false,
            code: None,
            lists: Vec::new(),
            items: Vec::new(),
            link: None,
            table: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        if self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.out
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.code_span(&code),
            Event::InlineMath(text) | Event::DisplayMath(text) => self.code_span(&text),
            Event::SoftBreak => self.break_line(false),
            Event::HardBreak => self.break_line(true),
            Event::Rule => self.rule(),
            Event::Html(html) => self.html(&html, false),
            Event::InlineHtml(html) => self.html(&html, true),
            Event::FootnoteReference(_) => {}
            Event::TaskListMarker(checked) => self.task_marker(checked),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.start_block();
                self.open = true;
            }
            Tag::Heading { level, .. } => {
                self.start_block();
                self.styles.push(heading_style(level));
                self.open = true;
            }
            Tag::BlockQuote(_) => {
                self.start_block();
                self.push_prefix(Span::styled("▎ ", Style::default().fg(theme::SUBTLE)));
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                // Indented blocks carry no language: they render the same
                // minus the label.
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => normalize_lang(&info),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some(CodeBlock {
                    lang,
                    text: String::new(),
                });
                self.push_prefix(Span::styled("│ ", Style::default().fg(theme::SUBTLE)));
            }
            Tag::List(start) => {
                self.start_block();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                self.blank_pending = false;
                // A nested item steps in one marker width: the enclosing item's
                // marker turns into spaces of its own width, and this item's
                // marker lands to its right.
                if let Some(&marker_index) = self.items.last()
                    && let Some(span) = self.prefix.get_mut(marker_index)
                {
                    let width = display_width(span.content.as_ref());
                    *span = Span::raw(" ".repeat(width));
                }
                let marker = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                let marker_index = self.prefix.len();
                self.push_prefix(Span::styled(marker, Style::default().fg(theme::ACCENT)));
                self.items.push(marker_index);
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(link_style());
                self.link = Some(Link {
                    dest: dest_url.to_string(),
                    label: Vec::new(),
                    is_image: false,
                });
            }
            Tag::Image { dest_url, .. } => {
                self.styles.push(link_style());
                self.link = Some(Link {
                    dest: dest_url.to_string(),
                    label: Vec::new(),
                    is_image: true,
                });
            }
            Tag::Table(alignments) => {
                self.start_block();
                self.table = Some(Table::new(alignments));
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.clear();
                    table.in_cell = true;
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.blank_pending = true;
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
                self.blank_pending = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.pop_prefix();
                self.blank_pending = true;
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::Item => {
                self.flush();
                self.pop_prefix();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.blank_pending = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => self.end_link(),
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.header = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    if !row.is_empty() {
                        table.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    let cell = std::mem::take(&mut table.cell);
                    table.in_cell = false;
                    table.row.push(cell);
                }
            }
            _ => {}
        }
    }

    /// Begin a top-level-ish block: finish the previous line, and pay the
    /// blank separator owed by whatever block ended before it.
    fn start_block(&mut self) {
        self.flush();
        if !self.out.is_empty() && self.blank_pending {
            self.out.push(Line::default());
        }
        self.blank_pending = false;
    }

    fn push_prefix(&mut self, span: Span<'static>) {
        self.marks.push(self.prefix.len());
        self.prefix.push(span);
    }

    fn pop_prefix(&mut self) {
        if let Some(len) = self.marks.pop() {
            self.prefix.truncate(len);
        }
    }

    fn inline_style(&self) -> Style {
        let mut style = Style::default();
        for layer in &self.styles {
            style = style.patch(*layer);
        }
        style
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = self.code.as_mut() {
            code.text.push_str(text);
            return;
        }
        let style = self.inline_style();
        if self.table.as_ref().is_some_and(|table| table.in_cell) || self.link.is_some() {
            // Cell and link labels are single-line: fold soft breaks to spaces.
            self.push_inline(vec![Span::styled(text.replace('\n', " "), style)]);
            return;
        }
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.push_inline(vec![Span::styled(part.to_string(), style)]);
            }
            if parts.peek().is_some() {
                self.flush();
            }
        }
    }

    fn code_span(&mut self, code: &str) {
        let style = self.inline_style().patch(code_style());
        let span = Span::styled(code.replace('\n', " "), style);
        self.push_inline(vec![span]);
    }

    fn task_marker(&mut self, checked: bool) {
        let (label, color) = if checked {
            ("[x] ", theme::ASSISTANT)
        } else {
            ("[ ] ", theme::DIM)
        };
        self.push_inline(vec![Span::styled(label, Style::default().fg(color))]);
    }

    fn break_line(&mut self, _hard: bool) {
        if self.table.as_ref().is_some_and(|table| table.in_cell) {
            self.push_inline(vec![Span::raw(" ")]);
            return;
        }
        if self.link.is_some() {
            self.push_inline(vec![Span::raw(" ")]);
            return;
        }
        self.flush();
    }

    fn rule(&mut self) {
        self.start_block();
        // Three box-drawing dashes, like the reference renderers: a rule is a
        // separator, not a full-width banner.
        let prefix = self.prefix.clone();
        self.push_wrapped(
            &prefix,
            vec![Span::styled("───", Style::default().fg(theme::SUBTLE))],
        );
        self.blank_pending = true;
    }

    fn html(&mut self, html: &str, inline: bool) {
        let style = Style::default().fg(theme::DIM);
        if inline {
            self.push_inline(vec![Span::styled(html.to_string(), style)]);
            return;
        }
        let prefix = self.prefix.clone();
        for line in html.lines() {
            self.push_wrapped(&prefix, vec![Span::styled(line.to_string(), style)]);
        }
        self.blank_pending = true;
    }

    /// Append spans to the current line, a table cell, or a link label.
    fn push_inline(&mut self, spans: Vec<Span<'static>>) {
        if spans.is_empty() {
            return;
        }
        if let Some(table) = self.table.as_mut()
            && table.in_cell
        {
            table.cell.extend(spans);
            return;
        }
        if let Some(link) = self.link.as_mut() {
            link.label.extend(spans);
            return;
        }
        self.current.extend(spans);
        self.open = true;
    }

    /// Emit the assembled logical line, wrapped under the current gutter.
    fn flush(&mut self) {
        if !self.open {
            self.current.clear();
            return;
        }
        self.open = false;
        let spans = std::mem::take(&mut self.current);
        let prefix = self.prefix.clone();
        self.push_wrapped(&prefix, spans);
    }

    fn push_wrapped(&mut self, prefix: &[Span<'static>], spans: Vec<Span<'static>>) {
        let pad = spans_width(prefix);
        let available = self.width.saturating_sub(pad).max(1);
        for (index, row) in wrap_styled(&spans, available).into_iter().enumerate() {
            let mut line = Vec::with_capacity(row.len() + 1);
            if index == 0 {
                line.extend(prefix.iter().cloned());
            } else if pad > 0 {
                line.push(Span::raw(" ".repeat(pad)));
            }
            line.extend(row);
            self.out.push(Line::from(line));
        }
    }

    fn end_code_block(&mut self) {
        let Some(block) = self.code.take() else {
            self.blank_pending = true;
            return;
        };
        let prefix = self.prefix.clone();

        if let Some(lang) = &block.lang {
            self.push_wrapped(
                &prefix,
                vec![Span::styled(lang.clone(), Style::default().fg(theme::DIM))],
            );
        }

        // `lines()` drops the fence's trailing newline but keeps interior
        // blank lines; leading blanks are kept as typed.
        let lines: Vec<&str> = block.text.lines().collect();
        // One entry per line; an empty entry means the line stays unstyled,
        // which is also the whole-block fallback for unknown languages.
        let segments = match &block.lang {
            Some(lang) => highlight::block(lang, &lines),
            None => Arc::new(vec![Vec::new(); lines.len()]),
        };

        for (index, line) in lines.iter().enumerate() {
            let spans = match segments.get(index) {
                Some(segments) if !segments.is_empty() => segments
                    .iter()
                    .filter_map(|segment| {
                        let text = line.get(segment.start..segment.end)?;
                        Some(Span::styled(
                            text.to_string(),
                            highlight::style(segment.kind),
                        ))
                    })
                    .collect::<Vec<_>>(),
                _ => vec![Span::raw(line.to_string())],
            };
            // Code is not reflowed: `push_wrapped` would break at spaces and
            // drop them, changing what the snippet says.
            self.push_code_line(&prefix, spans);
        }
        self.pop_prefix();
        self.blank_pending = true;
    }

    /// Emit one logical code line, hard-wrapped under the current gutter.
    fn push_code_line(&mut self, prefix: &[Span<'static>], spans: Vec<Span<'static>>) {
        let pad = spans_width(prefix);
        let available = self.width.saturating_sub(pad).max(1);
        for (index, row) in wrap_styled_hard(&spans, available).into_iter().enumerate() {
            let mut line = Vec::with_capacity(row.len() + 1);
            if index == 0 {
                line.extend(prefix.iter().cloned());
            } else if pad > 0 {
                line.push(Span::raw(" ".repeat(pad)));
            }
            line.extend(row);
            self.out.push(Line::from(line));
        }
    }

    fn end_link(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        self.styles.pop();
        let mut spans = Vec::new();
        if link.is_image {
            spans.push(Span::styled("🖼 ", Style::default().fg(theme::DIM)));
        }
        let label_text: String = link
            .label
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        spans.extend(link.label);
        if span_text(&spans).trim().is_empty() && link.is_image {
            spans.push(Span::styled("image", Style::default().fg(theme::DIM)));
        }
        // A bare autolink's label is already the destination; repeating it in
        // parentheses would only add noise.
        if !link.dest.is_empty() && link.dest != label_text {
            spans.push(Span::styled(
                format!(" ({})", link.dest),
                Style::default().fg(theme::DIM),
            ));
        }
        self.push_inline(spans);
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let cols = table.columns();
        if cols == 0 {
            self.blank_pending = true;
            return;
        }

        let mut widths = vec![3usize; cols];
        for row in std::iter::once(&table.header).chain(table.rows.iter()) {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(spans_width(cell));
            }
        }
        // Shrink the widest column until the row fits, never below 3 columns
        // so a value still reads as a value rather than a stray glyph.
        loop {
            let total: usize = widths.iter().sum::<usize>() + 2 * (cols - 1);
            if total <= self.width {
                break;
            }
            let Some((index, _)) = widths.iter().enumerate().max_by_key(|(_, width)| **width)
            else {
                break;
            };
            if widths[index] <= 3 {
                break;
            }
            widths[index] -= 1;
        }

        let prefix = self.prefix.clone();
        self.push_table_row(&prefix, &table.header, &widths, &table.alignments, true);
        let mut divider = Vec::with_capacity(cols * 2);
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                divider.push(Span::raw("  "));
            }
            divider.push(Span::styled(
                "─".repeat(*width),
                Style::default().fg(theme::SUBTLE),
            ));
        }
        self.push_wrapped(&prefix, divider);
        for row in &table.rows {
            self.push_table_row(&prefix, row, &widths, &table.alignments, false);
        }
        self.blank_pending = true;
    }

    fn push_table_row(
        &mut self,
        prefix: &[Span<'static>],
        row: &[Vec<Span<'static>>],
        widths: &[usize],
        alignments: &[Alignment],
        header: bool,
    ) {
        let mut spans = Vec::new();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            let cell = row.get(index).map(Vec::as_slice).unwrap_or(&[]);
            let (cell, used) = clip_styled(cell, *width);
            let padding = width.saturating_sub(used);
            let (left, right) = match alignments.get(index).copied().unwrap_or(Alignment::None) {
                Alignment::Right => (padding, 0),
                Alignment::Center => (padding / 2, padding - padding / 2),
                _ => (0, padding),
            };
            if left > 0 {
                spans.push(Span::raw(" ".repeat(left)));
            }
            if header {
                spans.extend(cell.into_iter().map(|mut span| {
                    span.style = span.style.add_modifier(Modifier::BOLD);
                    span
                }));
            } else {
                spans.extend(cell);
            }
            if right > 0 {
                spans.push(Span::raw(" ".repeat(right)));
            }
        }
        self.push_wrapped(prefix, spans);
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    let accent = Style::default().fg(theme::ACCENT);
    match level {
        HeadingLevel::H1 => accent.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => accent.add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        HeadingLevel::H5 => Style::default()
            .fg(theme::SUBTLE)
            .add_modifier(Modifier::ITALIC),
        HeadingLevel::H6 => Style::default().fg(theme::SUBTLE),
    }
}

fn code_style() -> Style {
    Style::default().fg(theme::ACCENT)
}

fn link_style() -> Style {
    Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::UNDERLINED)
}

fn span_text(spans: &[Span<'static>]) -> String {
    spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

/// Wrap styled spans at `width`, splitting only at character boundaries so
/// each row keeps its own per-span styles.
fn wrap_styled(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    wrap_styled_rows(spans, |chars| wrap_chars(chars, width))
}

/// Like [`wrap_styled`] but never reflows across a space: used for code, where
/// a dropped break space would change the source.
fn wrap_styled_hard(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    wrap_styled_rows(spans, |chars| wrap_chars_hard(chars, width))
}

/// Flatten styled spans to styled characters, let `split` choose the rows, and
/// rebuild spans per row (re-merging neighbours that share a style).
fn wrap_styled_rows(
    spans: &[Span<'static>],
    split: impl Fn(&[char]) -> Vec<(usize, usize)>,
) -> Vec<Vec<Span<'static>>> {
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|span| span.content.chars().map(|c| (c, span.style)))
        .collect();
    let plain: Vec<char> = chars.iter().map(|(c, _)| *c).collect();
    split(&plain)
        .into_iter()
        .map(|(start, end)| {
            let mut row: Vec<Span<'static>> = Vec::new();
            for &(c, style) in &chars[start..end] {
                match row.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => row.push(Span::styled(c.to_string(), style)),
                }
            }
            row
        })
        .collect()
}

/// Truncate styled spans to `width` display columns, returning the spans and
/// the width actually used.
fn clip_styled(spans: &[Span<'static>], width: usize) -> (Vec<Span<'static>>, usize) {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        for c in span.content.chars() {
            let w = char_width(c);
            if used + w > width {
                return (out, used);
            }
            used += w;
            match out.last_mut() {
                Some(last) if last.style == span.style => last.content.to_mut().push(c),
                _ => out.push(Span::styled(c.to_string(), span.style)),
            }
        }
    }
    (out, used)
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
    fn plain_paragraph_wraps_to_the_given_width() {
        assert_eq!(
            plain(&render("the quick brown fox", 9)),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn blank_lines_separate_blocks_but_not_soft_breaks() {
        assert_eq!(
            plain(&render("one\nsoft\ntwo\n\nthree", 40)),
            vec!["one", "soft", "two", "", "three"]
        );
    }

    #[test]
    fn headings_use_the_bold_ladder_and_are_separated() {
        let lines = render("# Title\n\nbody", 40);
        assert_eq!(plain(&lines), vec!["Title", "", "body"]);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );

        let lines = render("### Third", 40);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn bullets_and_ordered_items_hang_under_their_marker() {
        assert_eq!(
            plain(&render("- alpha\n- beta", 40)),
            vec!["• alpha", "• beta"]
        );
        assert_eq!(
            plain(&render("1. one\n2. two", 40)),
            vec!["1. one", "2. two"]
        );
        // A wrapped item body aligns under the text, not the marker.
        assert_eq!(
            plain(&render("- one two three", 12)),
            vec!["• one two", "  three"]
        );
    }

    #[test]
    fn nested_lists_indent_by_marker_width() {
        assert_eq!(
            plain(&render("- top\n  - nested", 40)),
            vec!["• top", "  • nested"]
        );
    }

    #[test]
    fn blockquotes_get_a_bar_gutter() {
        let lines = render("> quoted\nbar", 40);
        assert_eq!(plain(&lines), vec!["▎ quoted", "▎ bar"]);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::SUBTLE));

        // Bars nest.
        assert_eq!(plain(&render("> > deep", 40)), vec!["▎ ▎ deep"]);
    }

    #[test]
    fn rules_are_drawn_and_separated() {
        assert_eq!(
            plain(&render("a\n\n---\n\nb", 40)),
            vec!["a", "", "───", "", "b"]
        );
    }

    #[test]
    fn fenced_code_keeps_lines_and_gets_a_bar() {
        let lines = render("```\nlet x = 1;\n\nx\n```", 40);
        assert_eq!(plain(&lines), vec!["│ let x = 1;", "│ ", "│ x"]);
    }

    #[test]
    fn a_fence_label_names_the_language() {
        // The label is the fence's info string, on its own dim line.
        let lines = render("```rust\nfn main() {}\n```", 40);
        assert_eq!(plain(&lines), vec!["│ rust", "│ fn main() {}"]);
        assert_eq!(lines[0].spans.last().unwrap().style.fg, Some(theme::DIM));

        // Attributes and comma suffixes reduce to the bare language token.
        assert_eq!(
            plain(&render("```rust,ignore\nx\n```", 40))
                .first()
                .unwrap(),
            "│ rust"
        );
        // An indented block has no language, so no label.
        let indented = render("    plain\n", 40);
        assert_eq!(plain(&indented), vec!["│ plain"]);
    }

    #[test]
    fn fenced_code_is_highlighted_by_language() {
        let lines = render("```rust\nlet x = 42; // note\n```", 40);
        let body = lines.last().expect("code line");
        let colored: Vec<_> = body
            .spans
            .iter()
            .map(|span| span.style.fg)
            .filter(|fg| *fg != Some(theme::SUBTLE))
            .collect();
        assert!(
            colored.contains(&Some(theme::CODE_NUMBER)),
            "expected the number to be colored: {body:?}"
        );
        assert!(
            colored.contains(&Some(theme::CODE_COMMENT)),
            "expected the comment to be colored: {body:?}"
        );
    }

    #[test]
    fn an_unknown_fence_language_still_renders_plain() {
        let lines = render("```nosuchlang\nlet x = 1;\n```", 40);
        assert_eq!(plain(&lines), vec!["│ nosuchlang", "│ let x = 1;"]);
        // Label plus body, both uncolored apart from the gutter and label.
        let body = lines.last().unwrap();
        let body_spans: Vec<_> = body.spans.iter().skip(1).collect();
        assert!(
            body_spans.iter().all(|span| span.style.fg.is_none()),
            "unknown language must not color the body: {body:?}"
        );
    }

    #[test]
    fn highlighted_code_keeps_non_ascii_text() {
        // Token spans come back as byte ranges; slicing one at a non-boundary
        // would silently drop the run, which is invisible in ASCII samples.
        let lines = render("```rust\nlet s = \"日本語 text\"; // 注释 🙂\n```", 60);
        assert_eq!(plain(&lines)[0], "│ rust");
        let body = &plain(&lines)[1];
        assert!(body.contains("日本語"), "CJK dropped: {body}");
        assert!(body.contains("注释"), "CJK comment dropped: {body}");
        assert!(body.contains('🙂'), "emoji dropped: {body}");
    }

    #[test]
    fn code_wraps_without_eating_spaces_or_indent() {
        // Word wrapping would break at these spaces, drop one, and lose the
        // four-space indent on the continuation row.
        let source = "```rust\n    let value = compute(alpha, beta);\n```";
        let lines = render(source, 20);
        let body: Vec<String> = plain(&lines)[1..].to_vec();
        assert!(body.len() > 1, "expected a wrapped line: {body:?}");
        // The first row carries the bar; continuations are padded to the same
        // width so they stay under the code block rather than at column 0.
        assert!(
            body[0].starts_with("│ "),
            "first row lost its bar: {:?}",
            body[0]
        );
        for row in &body[1..] {
            assert!(
                row.starts_with("  "),
                "continuation lost its indent: {row:?}"
            );
        }
        // Re-joining the rows reproduces the original source exactly: the hard
        // wrap drops nothing, unlike the word wrap used for prose. The gutter
        // is `│ ` on the first row and two spaces on the rest.
        let joined: String = body
            .iter()
            .map(|row| {
                row.strip_prefix("│ ")
                    .or_else(|| row.strip_prefix("  "))
                    .unwrap_or(row)
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(joined, "    let value = compute(alpha, beta);");
    }

    #[test]
    fn inline_styles_apply_to_their_spans_only() {
        let lines = render("a **bold** b `code` c", 40);
        let spans = &lines[0].spans;
        assert_eq!(plain(&lines), vec!["a bold b code c"]);
        let bold = spans
            .iter()
            .find(|span| span.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let plain_span = spans
            .iter()
            .find(|span| span.content.as_ref() == "a ")
            .expect("plain span");
        assert!(!plain_span.style.add_modifier.contains(Modifier::BOLD));

        let lines = render("`x`", 40);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::ACCENT));
    }

    #[test]
    fn links_show_the_label_and_a_dim_destination() {
        assert_eq!(
            plain(&render("[docs](https://example.test)", 60)),
            vec!["docs (https://example.test)"]
        );
        // An autolink's label already is the destination.
        assert_eq!(
            plain(&render("<https://example.test>", 60)),
            vec!["https://example.test"]
        );
    }

    #[test]
    fn task_items_render_checkboxes() {
        assert_eq!(
            plain(&render("- [x] done\n- [ ] todo", 40)),
            vec!["• [x] done", "• [ ] todo"]
        );
    }

    #[test]
    fn tables_align_columns_and_draw_a_header_rule() {
        // Columns are content-sized with a 3-column minimum so the header rule
        // still reads as a divider.
        let lines = plain(&render(
            "| name | n |\n|---|---|\n| a | 1 |\n| bbb | 22 |",
            40,
        ));
        assert_eq!(
            lines,
            vec!["name  n  ", "────  ───", "a     1  ", "bbb   22 ",]
        );
    }

    #[test]
    fn tables_shrink_the_widest_column_to_fit() {
        let lines = plain(&render("| a | b |\n|---|---|\n| xxxxxxxx | y |", 14));
        for line in &lines {
            assert!(line.chars().count() <= 14, "{line:?} is too wide");
        }
    }

    #[test]
    fn empty_input_still_yields_one_line() {
        assert_eq!(plain(&render("", 20)), vec![""]);
        assert_eq!(plain(&render("   \n\n", 20)), vec![""]);
    }

    #[test]
    fn tracked_settles_everything_at_a_block_boundary() {
        // A trailing blank line closes the last block: appends start a new
        // block, so nothing stays in flight.
        let (lines, settled) = render_tracked("one\n\ntwo\n\n", 40);
        assert_eq!(settled, lines.len());
        // Without one, the trailing block may still grow. `one` is settled;
        // the blank separator and `two` belong to the mutable region.
        let (lines, settled) = render_tracked("one\n\ntwo", 40);
        assert_eq!(settled, 1);
        assert_eq!(lines.len(), 3);
        // A single unfinished block has no settled line at all.
        let (_, settled) = render_tracked("just typing", 40);
        assert_eq!(settled, 0);
    }

    #[test]
    fn settled_lines_match_the_prefix_render() {
        let source = "## Head\n\npara one\n\npara two that is still";
        let (lines, settled) = render_tracked(source, 20);
        let prefix = plain(&render("## Head\n\npara one", 20));
        assert_eq!(plain(&lines)[..settled], prefix[..]);
        // The in-flight tail is what the pane keeps redrawing.
        assert_eq!(settled, prefix.len());
        assert!(plain(&lines).len() > settled);
    }

    #[test]
    fn an_unclosed_construct_never_falls_in_the_settled_prefix() {
        // `**` is literal text until its partner arrives; the paragraph it
        // belongs to must not be committed before then.
        let (_, settled) = render_tracked("intro line\n\nstill **typing", 40);
        assert_eq!(settled, 1);
        // A fence swallows following blank lines, so nothing inside it is
        // stable either.
        let (_, settled) = render_tracked("```\nlet x = 1;\n\nstill code", 40);
        assert_eq!(settled, 0);
    }

    #[test]
    fn wide_characters_wrap_by_display_width() {
        assert_eq!(plain(&render("你好世界你好", 8)), vec!["你好世界", "你好"]);
    }
}
