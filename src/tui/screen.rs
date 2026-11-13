//! The frame every full-frame list screen shares.
//!
//! `/settings` and `/context` both take the whole terminal: a header with a
//! counter, an optional strip of section names, a scrolling document, an
//! optional fixed panel under it (the `/context` inspector), a rule, a notice
//! line, and a help bar. This module owns that layout, the
//! scroll-follows-focus rule, and the wheel handling, so a screen supplies
//! only its document lines and its two footer lines.
//!
//! Nothing here reads or writes application state: [`Screen`] is just the
//! scroll offset plus the caller's per-frame settings.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme;
use crate::tui::wrap::display_width;

/// Which rows the screen is composed of.
struct Chunks {
    header: Rect,
    strip: Option<Rect>,
    body: Rect,
    panel: Option<Rect>,
    rule: Option<Rect>,
    notice: Option<Rect>,
    hint: Rect,
    /// How many document lines the body can show.
    body_height: usize,
}

impl Chunks {
    fn split(
        area: Rect,
        strip: bool,
        panel_height: u16,
        notice: bool,
        rule: bool,
        hint: u16,
    ) -> Self {
        let mut constraints = vec![Constraint::Length(1)]; // header
        if strip {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(1)); // the document
        if panel_height > 0 {
            constraints.push(Constraint::Length(panel_height));
        }
        if rule {
            constraints.push(Constraint::Length(1));
        }
        if notice {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(hint));

        let parts = Layout::vertical(constraints).split(area);
        // Walk the layout in the same order it was declared, so the two can
        // never drift apart.
        let mut slots = parts.iter();
        let header = *slots.next().expect("header slot");
        let strip = strip.then(|| *slots.next().expect("strip slot"));
        let body = *slots.next().expect("document slot");
        let panel = (panel_height > 0).then(|| *slots.next().expect("panel slot"));
        let rule = rule.then(|| *slots.next().expect("rule slot"));
        let notice = notice.then(|| *slots.next().expect("notice slot"));
        let hint = *slots.next().expect("hint slot");
        Self {
            header,
            strip,
            body,
            panel,
            rule,
            notice,
            hint,
            body_height: (body.height as usize).max(1),
        }
    }
}

/// The document as the screen needs it: lines plus the focus each one
/// belongs to. Scroll-follow only knows about focus, so a line with no
/// focus (a card border, a blank) never pulls the viewport.
pub struct Document {
    pub lines: Vec<Line<'static>>,
    /// `focus[i]` is the focus index of `lines[i]`, if any.
    pub focus: Vec<Option<usize>>,
}

impl Document {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            focus: Vec::new(),
        }
    }

    pub fn push(&mut self, line: Line<'static>, focus: Option<usize>) {
        self.lines.push(line);
        self.focus.push(focus);
    }

    /// Push one line per focus index, top to bottom.
    pub fn push_focused(lines: impl IntoIterator<Item = Line<'static>>) -> Self {
        let mut document = Self::new();
        for (index, line) in lines.into_iter().enumerate() {
            document.push(line, Some(index));
        }
        document
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The document line the given focus index sits on.
    fn line_of(&self, focus: usize) -> Option<usize> {
        self.focus.iter().position(|line| *line == Some(focus))
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

/// What a [`Screen`] draws around its document. `Default` is the plain shape:
/// no strip, no panel, no notice or description.
pub struct Chrome<'a> {
    /// Title in the header, e.g. `Settings` or `Context`.
    pub title: &'a str,
    /// Breadcrumb after the title (`› tps`).
    pub crumb: Option<&'a str>,
    /// `(index, total)` for the right-aligned `n/m` counter.
    pub counter: Option<(usize, usize)>,
    /// Section names for the strip; `None` hides the strip entirely.
    pub strip: Option<&'a [String]>,
    /// The highlighted strip entry.
    pub active_section: Option<usize>,
    /// Fixed-height panel between the document and the rule.
    pub panel_height: u16,
    /// Draw the full-width rule above the notice. `/settings` wants it;
    /// `/context` puts its inspector panel there instead.
    pub rule: bool,
    /// `(text, is_error)` for the notice line.
    pub notice: Option<(&'a str, bool)>,
    /// The focused row's description for the help bar's left side.
    pub description: Option<&'a str>,
    /// The key list pinned to the help bar's right edge.
    pub keys: &'a str,
}

impl<'a> Chrome<'a> {
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            crumb: None,
            counter: None,
            strip: None,
            active_section: None,
            panel_height: 0,
            rule: true,
            notice: None,
            description: None,
            keys: "",
        }
    }

    /// The help bar's height: two rows (description above, keys below) when
    /// there is a description to show and room for it, else one.
    fn hint_height(&self, area_height: u16) -> u16 {
        if self.description.is_some() && area_height >= 6 {
            2
        } else {
            1
        }
    }
}

/// The content of the optional panel under the document.
pub enum Panel<'a> {
    /// Plain lines, drawn as a `Paragraph`.
    Lines(Vec<Line<'a>>),
    /// A pre-built paragraph, for a panel that carries its own block/borders.
    Paragraph(Paragraph<'a>),
    /// A paragraph plus the terminal cursor position, relative to the panel's
    /// top-left corner — an edit box, which has to place the caret.
    Cursor {
        paragraph: Paragraph<'a>,
        position: (u16, u16),
    },
}

/// The state a full-frame list screen keeps between frames.
#[derive(Debug, Default)]
pub struct Screen {
    /// Top document line currently shown; re-clamped on every render.
    pub scroll: usize,
    /// How many document lines the last render could show. The wheel needs
    /// this, and it is only known at render time.
    last_body_height: usize,
    /// The focus the viewport was last anchored to. The viewport follows the
    /// focus only when it *moves*: a wheel notch sets `scroll` on its own and
    /// must survive the next frame, so re-anchoring on every render would
    /// undo it immediately.
    last_focus: Option<usize>,
}

impl Screen {
    /// Draw one frame. `focus` is the focused row's index and `document` must
    /// report which lines carry which focus index.
    ///
    /// The viewport follows the focus: a focused row scrolled above the
    /// window or below it moves the window by the minimum needed, never
    /// re-centering.
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        document: &Document,
        focus: Option<usize>,
        chrome: &Chrome<'_>,
        panel: Option<Panel<'_>>,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // The screen takes the whole terminal over a layout it did not draw,
        // so start from a blank canvas: a row this screen leaves partly empty
        // must not keep a tail of the chat that was underneath.
        frame.render_widget(ratatui::widgets::Clear, area);
        let width = area.width as usize;
        let chunks = Chunks::split(
            area,
            chrome.strip.is_some(),
            chrome.panel_height,
            chrome.notice.is_some(),
            chrome.rule,
            chrome.hint_height(area.height),
        );
        self.last_body_height = chunks.body_height;

        let focused_line = focus.and_then(|focus| document.line_of(focus)).unwrap_or(0);
        let max_scroll = document.len().saturating_sub(chunks.body_height);
        // Re-anchor only when the focus moved: a wheel notch parks the
        // viewport somewhere else on purpose, and the frames that follow it
        // (a spinner tick, a notice) must not yank it back to the focus.
        let focus_moved = focus != self.last_focus;
        self.last_focus = focus;
        if focus_moved {
            if focused_line < self.scroll {
                self.scroll = focused_line;
            } else if focused_line >= self.scroll + chunks.body_height {
                self.scroll = focused_line + 1 - chunks.body_height;
            }
        }
        self.scroll = self.scroll.min(max_scroll);

        render_header(frame, chunks.header, chrome);
        if let (Some(area), Some(titles)) = (chunks.strip, chrome.strip) {
            render_strip(frame, area, width, titles, chrome.active_section);
        }
        let visible: Vec<Line<'static>> = document
            .lines
            .iter()
            .skip(self.scroll)
            .take(chunks.body_height)
            .cloned()
            .collect();
        frame.render_widget(
            Paragraph::new(ratatui::text::Text::from(visible)),
            chunks.body,
        );
        if let (Some(area), Some(panel)) = (chunks.panel, panel) {
            match panel {
                Panel::Lines(lines) => {
                    frame.render_widget(Paragraph::new(ratatui::text::Text::from(lines)), area)
                }
                Panel::Paragraph(paragraph) => frame.render_widget(paragraph, area),
                Panel::Cursor {
                    paragraph,
                    position,
                } => {
                    frame.render_widget(paragraph, area);
                    let x = area
                        .x
                        .saturating_add(position.0)
                        .min(area.x + area.width.saturating_sub(1));
                    let y = area
                        .y
                        .saturating_add(position.1)
                        .min(area.y + area.height.saturating_sub(1));
                    frame.set_cursor_position(ratatui::layout::Position::new(x, y));
                }
            }
        }
        if let Some(area) = chunks.rule {
            render_rule(frame, area, width);
        }
        if let Some(area) = chunks.notice {
            render_notice(frame, area, chrome.notice);
        }
        render_hint(frame, chunks.hint, width, chrome.description, chrome.keys);
    }

    /// Scroll by `delta` lines (negative scrolls up), clamped to the document
    /// and keeping the current page size in mind. Returns whether the wheel
    /// had effect, so a screen knows whether it needs a redraw.
    pub fn scroll_by(&mut self, delta: isize) -> bool {
        let before = self.scroll;
        let next = self.scroll as isize + delta;
        self.scroll = next.max(0) as usize;
        self.scroll != before
    }

    /// Clamp `scroll` to the document after the document changed shape (a
    /// snapshot arrived, entries were deleted).
    pub fn clamp(&mut self, document_len: usize) {
        let max = document_len.saturating_sub(self.last_body_height.max(1));
        self.scroll = self.scroll.min(max);
    }
}

/// Move `focus` by `delta` in a list of `len` rows, *wrapping*: `↑` on the
/// first row lands on the last and `↓` on the last lands on the first, which
/// is what the full-frame screens use so a long list never traps the user at
/// an end. Returns `None` for an empty list.
pub fn step_focus(focus: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let len_i = len as isize;
    let next = (focus as isize + delta).rem_euclid(len_i);
    Some(next as usize)
}

/// ` Title › crumb` on the left, the `n/m` counter on the right.
fn render_header(frame: &mut Frame<'_>, area: Rect, chrome: &Chrome<'_>) {
    let mut spans = vec![Span::styled(
        format!(" {}", chrome.title),
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(crumb) = chrome.crumb {
        spans.push(Span::styled(
            format!(" › {crumb}"),
            Style::default().fg(theme::SUBTLE),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let Some((index, len)) = chrome.counter else {
        return;
    };
    if len == 0 {
        return;
    }
    let counter = format!("{}/{} ", index + 1, len);
    let counter_width = (display_width(&counter) as u16).min(area.width);
    let right = Rect::new(
        area.x + area.width - counter_width,
        area.y,
        counter_width,
        area.height,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            counter,
            Style::default().fg(theme::DIM),
        ))),
        right,
    );
}

/// The section strip: every title, the active one highlighted, so a long
/// list still says which card the focus is in.
fn render_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    width: usize,
    titles: &[String],
    active: Option<usize>,
) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (index, title) in titles.iter().enumerate() {
        let label = format!(" {title} ");
        let label_width = display_width(&label);
        if used + label_width > width {
            break;
        }
        used += label_width;
        let style = if Some(index) == active {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        };
        spans.push(Span::styled(label, style));
        let gap = " · ";
        if index + 1 < titles.len() && used + 3 <= width {
            used += 3;
            spans.push(Span::styled(gap, Style::default().fg(theme::DIM)));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_rule(frame: &mut Frame<'_>, area: Rect, width: usize) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(theme::DIM),
        ))),
        area,
    );
}

/// `✓ saved …` / `✗ save failed: …`, or an empty row when nothing happened.
fn render_notice(frame: &mut Frame<'_>, area: Rect, notice: Option<(&str, bool)>) {
    let line = match notice {
        None => Line::from(""),
        Some((text, true)) => Line::from(Span::styled(
            format!(" ✗ {text}"),
            Style::default().fg(theme::ERROR),
        )),
        Some((text, false)) => Line::from(Span::styled(
            format!(" ✓ {text}"),
            Style::default().fg(theme::ASSISTANT),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The help bar: the focused row's description on the left (truncated first),
/// the keys pinned to the right — a truncated hint still reads, a truncated
/// key list hides the keys nobody can guess.
fn render_hint(
    frame: &mut Frame<'_>,
    area: Rect,
    width: usize,
    description: Option<&str>,
    keys: &str,
) {
    if area.height >= 2 {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
        hint_line(frame, rows[0], width, description, "");
        hint_line(frame, rows[1], width, None, keys);
        return;
    }
    hint_line(frame, area, width, description, keys);
}

fn hint_line(frame: &mut Frame<'_>, area: Rect, width: usize, left: Option<&str>, right: &str) {
    let right_width = display_width(right);
    let budget = width.saturating_sub(right_width + 1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut left_width = 0;
    if let Some(text) = left
        && budget >= 8
    {
        let text = crate::tui::session::truncated(text, budget);
        left_width = display_width(&text);
        spans.push(Span::styled(
            text,
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(right_width + left_width)),
    ));
    spans.push(Span::styled(
        right.to_string(),
        Style::default().fg(theme::DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(
        screen: &mut Screen,
        document: &Document,
        focus: Option<usize>,
        height: u16,
    ) -> Vec<String> {
        let mut chrome = Chrome::new("Settings");
        chrome.crumb = Some("tps");
        chrome.counter = Some((focus.unwrap_or(0), 3));
        let strip = vec!["codest".to_string(), "tps".to_string()];
        chrome.strip = Some(&strip);
        chrome.active_section = Some(1);
        chrome.notice = Some(("saved tps.enabled", false));
        chrome.description = Some("live t/s readout");
        chrome.keys = "↑↓ select · Esc close";
        let mut terminal = Terminal::new(TestBackend::new(60, height)).expect("terminal");
        terminal
            .draw(|frame| screen.render(frame, frame.area(), document, focus, &chrome, None))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..60)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn rows(n: usize) -> Document {
        Document::push_focused((0..n).map(|index| Line::from(format!("row {index}"))))
    }

    #[test]
    fn the_viewport_follows_the_focus_by_the_minimum_amount() {
        let document = rows(40);
        let mut screen = Screen::default();
        // A short terminal shows few rows; focus stays visible at the bottom.
        render(&mut screen, &document, Some(0), 10);
        assert_eq!(screen.scroll, 0);

        render(&mut screen, &document, Some(12), 10);
        assert!(screen.scroll > 0, "the focus scrolled into view");

        // Moving back up brings the window back to the top, never past it.
        render(&mut screen, &document, Some(0), 10);
        assert_eq!(screen.scroll, 0);

        // At the end, the window stops at the last page rather than running on.
        render(&mut screen, &document, Some(39), 10);
        assert!(
            screen.scroll + 4 <= 40,
            "scroll {} exceeds the document",
            screen.scroll
        );
    }

    #[test]
    fn the_header_strip_rule_notice_and_hint_all_render() {
        let mut screen = Screen::default();
        let frame = render(&mut screen, &rows(3), Some(1), 12);
        let text = frame.join("\n");
        assert!(text.contains("Settings › tps"), "{text}");
        assert!(text.contains("2/3"), "the counter is 1-based: {text}");
        assert!(text.contains("codest  ·  tps"), "{text}");
        assert!(text.contains("✓ saved tps.enabled"), "{text}");
        assert!(text.contains("live t/s readout"), "{text}");
        assert!(text.contains("↑↓ select · Esc close"), "{text}");
        // The hint row pins its keys to the right edge, and the rule spans
        // the frame: those two are the rows that must be exactly the width.
        let rule = frame
            .iter()
            .find(|line| line.starts_with('─'))
            .expect("a page rule");
        assert_eq!(display_width(rule), 60, "{text}");
        assert_eq!(
            frame.last().map(|line| display_width(line)),
            Some(60),
            "the key list ends at the right edge: {text}"
        );
    }

    #[test]
    fn an_error_notice_uses_the_error_color_and_a_quiet_frame_shows_nothing() {
        let mut screen = Screen::default();
        let mut chrome = Chrome::new("Context");
        chrome.notice = Some(("`maybe` is not a boolean", true));
        chrome.keys = "Esc close";
        let document = rows(1);
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).expect("terminal");
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &document, Some(0), &chrome, None))
            .expect("draw");
        let text = (0..8)
            .map(|y| {
                (0..60)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("✗ `maybe` is not a boolean"), "{text}");
    }

    #[test]
    fn a_zero_sized_area_is_a_no_op() {
        let mut screen = Screen::default();
        let chrome = Chrome::new("Settings");
        let document = rows(1);
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                screen.render(
                    frame,
                    Rect::new(0, 0, 0, 0),
                    &document,
                    Some(0),
                    &chrome,
                    None,
                );
            })
            .expect("zero-area draw must not panic");
    }

    /// The panel is one of the two shapes `/context` needs: a fixed-height
    /// block under the document, taken out of the document's own space.
    #[test]
    fn a_panel_takes_rows_from_the_document() {
        let mut screen = Screen::default();
        let mut chrome = Chrome::new("Context");
        chrome.panel_height = 4;
        let document = rows(20);
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("terminal");
        terminal
            .draw(|frame| {
                let panel = Panel::Lines(vec![Line::from("inspector body")]);
                screen.render(
                    frame,
                    frame.area(),
                    &document,
                    Some(0),
                    &chrome,
                    Some(panel),
                );
            })
            .expect("draw");
        let text = (0..16)
            .map(|y| {
                (0..60)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("inspector body"), "{text}");
        assert!(text.contains("row 0"), "{text}");
        // 16 rows: header 1 + document 9 + panel 4 + rule 1 + hint 1.
        assert_eq!(screen.last_body_height, 9, "{text}");
    }

    #[test]
    fn the_wheel_moves_the_viewport_and_stops_at_the_top() {
        let mut screen = Screen::default();
        let document = rows(40);
        render(&mut screen, &document, Some(0), 10);
        assert!(screen.scroll_by(5), "the wheel scrolled");
        assert_eq!(screen.scroll, 5);
        assert!(screen.scroll_by(-99), "the wheel went back up");
        assert_eq!(screen.scroll, 0, "never negative");
        assert!(!screen.scroll_by(-1), "already at the top says so");
    }

    /// The bug this pins: the wheel set an offset, and the very next frame
    /// pulled the viewport back to the focus, so scrolling looked like a
    /// no-op at the terminal. Only a focus *move* may re-anchor the window.
    #[test]
    fn a_wheel_scroll_survives_the_next_frame() {
        let mut screen = Screen::default();
        let document = rows(40);
        render(&mut screen, &document, Some(0), 10);
        assert_eq!(screen.scroll, 0);

        screen.scroll_by(4);
        assert_eq!(screen.scroll, 4);
        // Same focus, redrawn: the offset the wheel chose must still stand.
        let frame = render(&mut screen, &document, Some(0), 10);
        assert_eq!(screen.scroll, 4, "the next frame kept the wheel offset");
        assert!(
            frame.join("\n").contains("row 4"),
            "the window really shows the scrolled rows: {frame:#?}"
        );
        assert!(!frame.join("\n").contains("row 0"), "{frame:#?}");

        // Moving the focus re-anchors on it, which is what a keyboard step
        // is supposed to do.
        render(&mut screen, &document, Some(1), 10);
        assert_eq!(screen.scroll, 1, "the focus move re-anchored the view");
        // A step that stays inside the window does not re-center: the scroll
        // only moves the minimum needed to keep the focus visible.
        render(&mut screen, &document, Some(2), 10);
        assert_eq!(screen.scroll, 1, "no needless re-centering inside the page");
        // A step off the bottom pulls the window along, placing the focus on
        // the last visible row.
        render(&mut screen, &document, Some(20), 10);
        let body = screen.last_body_height;
        assert_eq!(
            screen.scroll,
            21 - body,
            "the window followed the focus down, leaving it on the last row"
        );
    }

    #[test]
    fn clamping_after_a_shorter_document_stays_in_range() {
        let mut screen = Screen::default();
        let document = rows(40);
        render(&mut screen, &document, Some(39), 10);
        let deep = screen.scroll;
        assert!(deep > 0);
        screen.clamp(5);
        assert!(screen.scroll < deep, "clamped to the shorter document");
    }

    #[test]
    fn focus_wraps_at_both_ends() {
        // ↓ on the last row wraps to the first, ↑ on the first to the last.
        assert_eq!(step_focus(4, 5, 1), Some(0));
        assert_eq!(step_focus(0, 5, -1), Some(4));
        // A page step wraps too rather than overshooting.
        assert_eq!(step_focus(1, 5, -5), Some(1));
        assert_eq!(step_focus(3, 5, 5), Some(3));
        assert_eq!(step_focus(0, 0, 1), None, "an empty list has no rows");
        assert_eq!(step_focus(0, 1, 1), Some(0), "a single row stays put");
    }
}
