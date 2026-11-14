//! The bottom floating report window for `/status` and `/usage`.
//!
//! A bordered pane anchored to the bottom of the frame that floats over the
//! transcript while a turn streams: it is drawn last each frame over a
//! [`Clear`], so the output streaming underneath keeps repainting without
//! ever disturbing it, and closing it leaves no transcript residue — the
//! readout is a glance, not a record. Modal while open: scroll keys move
//! the content, Esc / Enter / `q` dismiss it, everything else is swallowed.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;

use crate::plugins::ReportBlock;
use crate::tui::chat::report_lines;
use crate::tui::theme;

/// A report held open over the transcript until dismissed. Pure data: the
/// wrapped lines are recomputed at the current width each frame, so a
/// resize reflows instead of clipping.
pub struct ReportPopup {
    block: ReportBlock,
    /// Body rows scrolled off the top. Clamped against the visible height
    /// at render time, so `End` can just ask for everything.
    scroll: usize,
}

impl ReportPopup {
    pub fn new(block: ReportBlock) -> Self {
        Self { block, scroll: 0 }
    }

    /// The report the popup shows, for tests and future re-renders.
    pub fn block(&self) -> &ReportBlock {
        &self.block
    }

    /// The wrapped body lines at `width` (the popup's inner width), minus
    /// the block's own `▪ title` row — the border title carries it.
    fn body_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = report_lines(&self.block, width);
        if !lines.is_empty() {
            lines.remove(0);
        }
        lines
    }

    /// Rows the popup wants at `width`, capped at `max` (borders included).
    pub fn preferred_height(&self, width: u16, max: u16) -> u16 {
        let inner_width = width.saturating_sub(2) as usize;
        let body = self.body_lines(inner_width).len() as u16;
        (body + 2).min(max)
    }

    /// Draw bottom-anchored within `area`, floating over whatever is below.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 4 || area.height < 3 {
            return;
        }
        let height = self.preferred_height(area.width, area.height);
        let rect = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(height),
            width: area.width,
            height,
        };

        let inner_width = rect.width.saturating_sub(2) as usize;
        let body = self.body_lines(inner_width);
        let visible = rect.height.saturating_sub(2) as usize;
        let scroll = self.scroll.min(body.len().saturating_sub(visible));
        let window: Vec<Line<'static>> = body.into_iter().skip(scroll).take(visible).collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ACCENT))
            .title(Line::from(Span::styled(
                format!(" {} ", self.block.title),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )))
            .title_bottom(Line::from(Span::styled(
                " Esc/Enter close · ↑↓ scroll ".to_string(),
                Style::default().fg(theme::SUBTLE),
            )));
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(Text::from(window))
                .block(block)
                .style(Style::default().fg(theme::DIM)),
            rect,
        );
    }

    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows);
    }

    pub fn scroll_home(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_end(&mut self) {
        self.scroll = usize::MAX;
    }
}
