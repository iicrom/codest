//! The `/settings` screen.
//!
//! One card per section — the host's own settings first, then one card per
//! plugin that declared a [`SettingsSection`] — with one row per option and a
//! help bar at the bottom. The layout follows the reference TUI's settings
//! page: a pointer gutter, a label, a right-aligned value chip, the focused
//! row's description in the help bar (never a second line on the row, so
//! moving the focus never reflows the list), and a full-width rule above the
//! notice line.
//!
//! Every edit is written immediately through [`SettingsFile`] (validated and
//! atomic) and reported to the app as a [`SettingsEvent`], which applies the
//! new value to the running session and, for a plugin, announces it to the
//! plugin through `Plugin::on_settings_changed`. The screen never owns runtime
//! state — `config.toml` does.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use cst_plugin_api::settings::{SettingField, SettingKind};

use crate::core::config::GlobalConfig;
use crate::core::settings::{SettingOwner, SettingValue, SettingsFile};
use crate::tui::composer::Composer;
use crate::tui::theme;
use crate::tui::wrap::display_width;

/// The pointer gutter of a focused row (`❯ `, padded to two cells).
const POINTER: &str = "❯ ";
const POINTER_EMPTY: &str = "  ";

/// One card: a title, a one-line subtitle, and the fields it edits.
pub struct Section {
    pub title: String,
    pub description: &'static str,
    pub owner: SettingOwner,
    pub fields: &'static [SettingField],
}

/// What the screen asks the app to do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Esc: give the frame back to the chat.
    Close,
    /// A write landed; `section` names whose value changed so the app can
    /// apply it (the file is already saved).
    Saved { section: usize },
}

/// A one-line editor for a number/text field.
struct Editor {
    section: usize,
    field: usize,
    composer: Composer,
}

/// A rendered document line and the focus index it belongs to, if any.
struct DocLine {
    line: Line<'static>,
    focus: Option<usize>,
}

pub struct SettingsView {
    file: SettingsFile,
    /// The document as of the last successful read or write.
    global: GlobalConfig,
    sections: Vec<Section>,
    /// `values[section][field]`: the stored text, `None` while the key is
    /// absent (the row then shows the field's default).
    values: Vec<Vec<Option<String>>>,
    /// Flat focus order: `(section, field)` per focusable row.
    focusable: Vec<(usize, usize)>,
    focus: usize,
    editing: Option<Editor>,
    /// The last save outcome: `(text, is_error)`.
    notice: Option<(String, bool)>,
    /// Top document line currently shown; re-clamped on every render.
    scroll: usize,
}

impl SettingsView {
    pub fn new(file: SettingsFile, sections: Vec<Section>) -> Self {
        let global = crate::core::config::load_global(file.path()).unwrap_or_default();
        let mut view = Self {
            file,
            global,
            sections,
            values: Vec::new(),
            focusable: Vec::new(),
            focus: 0,
            editing: None,
            notice: None,
            scroll: 0,
        };
        view.reindex();
        view
    }

    /// Re-read the stored values and the focus order after a write (or on
    /// construction). A section with no fields still renders its card.
    fn reindex(&mut self) {
        self.values = self
            .sections
            .iter()
            .map(|section| {
                section
                    .fields
                    .iter()
                    .map(|field| self.file.text(section.owner, field.key))
                    .collect()
            })
            .collect();
        self.focusable = self
            .sections
            .iter()
            .enumerate()
            .flat_map(|(section, model)| (0..model.fields.len()).map(move |field| (section, field)))
            .collect();
        self.focus = self.focus.min(self.focusable.len().saturating_sub(1));
    }

    /// The document as it now stands on disk — the app applies values from
    /// here rather than re-reading the file.
    pub fn global(&self) -> &GlobalConfig {
        &self.global
    }

    /// The owner of card `section` — which table its edits land in.
    pub fn section_owner(&self, section: usize) -> Option<SettingOwner> {
        self.sections.get(section).map(|model| model.owner)
    }

    /// Focus the first field of the card named `name` (a `/settings <name>`
    /// argument). Returns false when no card carries that title.
    pub fn focus_section(&mut self, name: &str) -> bool {
        let Some(section) = self
            .sections
            .iter()
            .position(|model| model.title.eq_ignore_ascii_case(name))
        else {
            return false;
        };
        if let Some(index) = self
            .focusable
            .iter()
            .position(|(candidate, _)| *candidate == section)
        {
            self.focus = index;
            return true;
        }
        // A card with nothing to edit is still worth navigating to: keep the
        // focus where it is and let the window follow the card instead.
        self.scroll = 0;
        true
    }

    /// The card titles, for the "no such section" message.
    pub fn section_titles(&self) -> Vec<&str> {
        self.sections
            .iter()
            .map(|model| model.title.as_str())
            .collect()
    }

    pub fn set_notice_error(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), true));
    }

    fn focused(&self) -> Option<(usize, usize)> {
        self.focusable.get(self.focus).copied()
    }

    /// The stored text for one row, or the field's default while unset.
    fn text_of(&self, section: usize, field: usize) -> Option<String> {
        self.values
            .get(section)
            .and_then(|values| values.get(field))
            .cloned()
            .flatten()
            .or_else(|| {
                self.sections
                    .get(section)
                    .and_then(|model| model.fields.get(field))
                    .and_then(|field| field.default)
                    .map(str::to_string)
            })
    }

    /// Whether the row carries an explicit value in `config.toml`.
    fn is_set(&self, section: usize, field: usize) -> bool {
        self.values
            .get(section)
            .and_then(|values| values.get(field))
            .is_some_and(Option::is_some)
    }

    /// Handle one key. `None` means the screen consumed it with no effect on
    /// the app; `Some(event)` asks the app to act.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SettingsEvent> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // The editor owns the keyboard while it is open.
        if self.editing.is_some() {
            match key.code {
                KeyCode::Enter if !ctrl && !alt => return self.commit_editor(),
                KeyCode::Esc => {
                    self.editing = None;
                    return None;
                }
                KeyCode::Char('c') if ctrl => {
                    // Ctrl+C leaves the editor, not the app.
                    self.editing = None;
                    return None;
                }
                _ => {
                    if let Some(editor) = self.editing.as_mut() {
                        apply_edit_key(&mut editor.composer, key);
                    }
                    return None;
                }
            }
        }

        match key.code {
            KeyCode::Esc => return Some(SettingsEvent::Close),
            KeyCode::Char('c') if ctrl => return Some(SettingsEvent::Close),
            KeyCode::Up => self.move_focus(-1),
            KeyCode::Down => self.move_focus(1),
            KeyCode::PageUp => self.move_focus(-5),
            KeyCode::PageDown => self.move_focus(5),
            KeyCode::Home => self.focus = 0,
            KeyCode::End => self.focus = self.focusable.len().saturating_sub(1),
            KeyCode::Char(' ') if !ctrl && !alt => return self.activate(false),
            KeyCode::Enter if !ctrl && !alt => return self.activate(true),
            KeyCode::Right if !ctrl && !alt => return self.activate(false),
            KeyCode::Left if !ctrl && !alt => return self.activate_previous(),
            _ => {}
        }
        None
    }

    fn move_focus(&mut self, delta: isize) {
        let len = self.focusable.len();
        if len == 0 {
            return;
        }
        self.focus = (self.focus as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// Enter `delta` focus steps, skipping rows of a card the caller filtered
    /// out (nothing today: every card is shown).
    fn activate(&mut self, enter: bool) -> Option<SettingsEvent> {
        let (section, field) = self.focused()?;
        let spec = self.sections.get(section)?.fields.get(field)?;
        let current = self.text_of(section, field);
        let mut event = None;
        match spec.kind {
            SettingKind::Boolean => {
                let next = current.as_deref() != Some("true");
                event = self.save(section, field, Some(bool_value(next)));
            }
            SettingKind::Select => {
                if let Some(next) = spec.cycle(current.as_deref(), true) {
                    event = self.save(section, field, Some(SettingValue::Text(next.to_string())));
                }
            }
            SettingKind::Number | SettingKind::Text => {
                if enter {
                    let mut composer = Composer::new();
                    // Seed with what is stored, not with the default: typing
                    // over a friendly default would otherwise pin it.
                    if let Some(text) = self.values[section][field].clone() {
                        composer.set_text(text);
                    }
                    self.editing = Some(Editor {
                        section,
                        field,
                        composer,
                    });
                }
            }
        }
        event
    }

    /// ← steps a select (or a boolean) backwards; on a text row it does
    /// nothing, so a stray arrow cannot wipe a value.
    fn activate_previous(&mut self) -> Option<SettingsEvent> {
        let (section, field) = self.focused()?;
        let spec = self.sections.get(section)?.fields.get(field)?;
        let current = self.text_of(section, field);
        match spec.kind {
            SettingKind::Boolean => {
                let next = current.as_deref() != Some("true");
                self.save(section, field, Some(bool_value(next)))
            }
            SettingKind::Select => spec.cycle(current.as_deref(), false).and_then(|next| {
                self.save(section, field, Some(SettingValue::Text(next.to_string())))
            }),
            _ => None,
        }
    }

    /// Save the editor's text: empty removes the key (the plugin's default),
    /// anything else is parsed by the field's kind first.
    fn commit_editor(&mut self) -> Option<SettingsEvent> {
        let editor = self.editing.take()?;
        let section = editor.section;
        let field = editor.field;
        let text = editor.composer.text();
        let spec = *self.sections.get(section)?.fields.get(field)?;
        let value = if text.trim().is_empty() {
            None
        } else {
            match SettingValue::parse(&spec, &text) {
                Ok(value) => Some(value),
                Err(message) => {
                    // Keep the editor open with the offending text: the row is
                    // not lost, and the notice says what went wrong.
                    self.notice = Some((message, true));
                    self.editing = Some(editor);
                    return None;
                }
            }
        };
        self.save(section, field, value)
    }

    /// Write one value and report it. A rejected write leaves the file as it
    /// was and shows the reason in the notice line.
    fn save(
        &mut self,
        section: usize,
        field: usize,
        value: Option<SettingValue>,
    ) -> Option<SettingsEvent> {
        let model = self.sections.get(section)?;
        let spec = model.fields.get(field)?;
        let owner = model.owner;
        let title = model.title.clone();
        let key = spec.key;
        match self.file.write(owner, key, value) {
            Ok(global) => {
                self.global = global;
                self.reindex();
                self.notice = Some((format!("saved {title}.{key}"), false));
                Some(SettingsEvent::Saved { section })
            }
            Err(error) => {
                self.notice = Some((format!("save failed: {error}"), true));
                None
            }
        }
    }

    /// Render over the whole frame area.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        // Header, section strip, body, rule, notice, help bar. The help bar
        // takes two rows when there is room (description above, keys below)
        // so a long hint is not truncated to a stub next to the key list.
        let hint_height = if area.height >= 6 { 2 } else { 1 };
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),    // the card list
            Constraint::Length(1), // the page rule
            Constraint::Length(1), // the last save's outcome
            Constraint::Length(hint_height),
        ])
        .split(area);
        let body_height = (chunks[2].height as usize).max(1);

        let doc = self.doc(width);
        let focused_line = doc
            .iter()
            .position(|line| line.focus == Some(self.focus))
            .unwrap_or(0);
        let max_scroll = doc.len().saturating_sub(body_height);
        if focused_line < self.scroll {
            self.scroll = focused_line;
        } else if focused_line >= self.scroll + body_height {
            self.scroll = focused_line + 1 - body_height;
        }
        self.scroll = self.scroll.min(max_scroll);

        self.render_header(frame, chunks[0], width);
        self.render_strip(frame, chunks[1], width);
        let visible: Vec<Line<'static>> = doc
            .into_iter()
            .skip(self.scroll)
            .take(body_height)
            .map(|line| line.line)
            .collect();
        frame.render_widget(
            Paragraph::new(ratatui::text::Text::from(visible)),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(width),
                Style::default().fg(theme::DIM),
            ))),
            chunks[3],
        );
        self.render_notice(frame, chunks[4]);
        self.render_hint(frame, chunks[5], width);
    }

    fn render_header(&mut self, frame: &mut Frame, area: Rect, _width: usize) {
        let mut spans = vec![Span::styled(
            " Settings",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some((section, _)) = self.focused()
            && let Some(model) = self.sections.get(section)
        {
            spans.push(Span::styled(
                format!(" › {}", model.title),
                Style::default().fg(theme::SUBTLE),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        let counter = if self.focusable.is_empty() {
            String::new()
        } else {
            format!("{}/{} ", self.focus + 1, self.focusable.len())
        };
        // The counter rides the right edge.
        if !counter.is_empty() {
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
    }

    /// The section strip: every card title, the focused one highlighted, so a
    /// long list still says which card the focus is in.
    fn render_strip(&self, frame: &mut Frame, area: Rect, width: usize) {
        let active = self.focused().map(|(section, _)| section);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for (index, model) in self.sections.iter().enumerate() {
            let label = format!(" {} ", model.title);
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
            if index + 1 < self.sections.len() && used + 3 <= width {
                used += 3;
                spans.push(Span::styled(gap, Style::default().fg(theme::DIM)));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_notice(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.notice {
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

    /// The help bar: the focused row's description on the left (truncated
    /// first), the keys pinned to the right — a truncated hint still reads, a
    /// truncated key list hides the keys nobody can guess. When rows are
    /// editable the bar takes two lines (description above, keys below) so a
    /// long explanation is not cut to a stub.
    fn render_hint(&self, frame: &mut Frame, area: Rect, width: usize) {
        let keys = if self.editing.is_some() {
            "Enter save · Esc cancel"
        } else if self.focused().is_some() {
            match self.focused_kind() {
                Some(SettingKind::Boolean) => "↑↓ select · Enter toggle · Esc close",
                Some(SettingKind::Select) => "↑↓ select · Enter/←→ change · Esc close",
                Some(SettingKind::Number | SettingKind::Text) => {
                    "↑↓ select · Enter edit · Esc close"
                }
                None => "Esc close",
            }
        } else {
            "Esc close"
        };
        if area.height >= 2 {
            let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
            self.hint_line(frame, rows[0], width, self.focused_description(), "");
            self.hint_line(frame, rows[1], width, None, keys);
            return;
        }
        self.hint_line(frame, area, width, self.focused_description(), keys);
    }

    /// One help-bar row: the left text truncated to fit, the right text pinned
    /// to the edge.
    fn hint_line(
        &self,
        frame: &mut Frame,
        area: Rect,
        width: usize,
        left: Option<String>,
        right: &str,
    ) {
        let right_width = display_width(right);
        let budget = width.saturating_sub(right_width + 1);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut left_width = 0;
        if let Some(text) = left
            && budget >= 8
        {
            let text = crate::tui::session::truncated(&text, budget);
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

    /// The kind of the focused row, for the help bar and its height.
    fn focused_kind(&self) -> Option<SettingKind> {
        let (section, field) = self.focused()?;
        Some(self.sections.get(section)?.fields.get(field)?.kind)
    }

    fn focused_description(&self) -> Option<String> {
        let (section, field) = self.focused()?;
        let spec = self.sections.get(section)?.fields.get(field)?;
        let mut text = spec.description.to_string();
        if spec.restart {
            text.push_str(" · restart to apply");
        }
        if !self.is_set(section, field) {
            text.push_str(" · default");
        }
        Some(text)
    }

    /// The whole document as lines: one card per section, one line per field.
    fn doc(&self, width: usize) -> Vec<DocLine> {
        let mut lines: Vec<DocLine> = Vec::new();
        for (index, model) in self.sections.iter().enumerate() {
            lines.push(DocLine {
                line: card_top(&model.title, model.description, width),
                focus: None,
            });
            if model.fields.is_empty() {
                lines.push(DocLine {
                    line: card_body(
                        Line::from(Span::styled(
                            "  no configurable options",
                            Style::default().fg(theme::DIM),
                        )),
                        width,
                    ),
                    focus: None,
                });
            }
            for (field_index, spec) in model.fields.iter().enumerate() {
                let focus = self
                    .focusable
                    .iter()
                    .position(|pair| *pair == (index, field_index));
                let is_focused =
                    self.editing.as_ref().is_some_and(|editor| {
                        editor.section == index && editor.field == field_index
                    }) || (self.editing.is_none() && focus == Some(self.focus));
                let row = self.field_line(index, field_index, spec, is_focused, width);
                lines.push(DocLine {
                    line: card_body(row, width),
                    focus,
                });
            }
            lines.push(DocLine {
                line: card_bottom(width),
                focus: None,
            });
        }
        lines
    }

    fn field_line(
        &self,
        section: usize,
        field: usize,
        spec: &SettingField,
        focused: bool,
        width: usize,
    ) -> Line<'static> {
        let editing = self
            .editing
            .as_ref()
            .filter(|editor| editor.section == section && editor.field == field);
        let pointer = if focused { POINTER } else { POINTER_EMPTY };
        let pointer_style = if focused {
            Style::default().fg(theme::ACCENT)
        } else {
            Style::default().fg(theme::DIM)
        };
        let label_style = if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let value = match editing {
            Some(editor) => format!("{}▌", editor.composer.text()),
            None => self.value_text(section, field, spec),
        };
        let value_style = match (spec.kind, focused) {
            (SettingKind::Boolean, _) => {
                if self.text_of(section, field).as_deref() == Some("true") {
                    Style::default().fg(theme::ASSISTANT)
                } else {
                    Style::default().fg(theme::DIM)
                }
            }
            (_, true) => Style::default().fg(theme::ACCENT),
            (_, false) => Style::default().fg(theme::SUBTLE),
        };
        // Card borders (`│ ` + ` │`, 4 cells) and the pointer gutter (2), so
        // the value column keeps a stable right edge whatever the terminal
        // width. A long value (a text setting) is truncated rather than
        // pushing the right wall off screen.
        let available = width.saturating_sub(4 + 2);
        let value_budget = (available / 2).max(6);
        let value = if display_width(&value) > value_budget {
            crate::tui::session::truncated(&value, value_budget)
        } else {
            value
        };
        let value_width = display_width(&value);
        let label_budget = available.saturating_sub(value_width + 2);
        let label = if display_width(spec.label) > label_budget {
            crate::tui::session::truncated(spec.label, label_budget.max(4))
        } else {
            spec.label.to_string()
        };
        let pad = label_budget.saturating_sub(display_width(&label)).max(1);
        let spans = vec![
            Span::styled(pointer, pointer_style),
            Span::styled(label, label_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(value, value_style),
        ];
        Line::from(spans)
    }

    /// The value chip: `[x]`/`[ ]`, `‹ label ›`, the stored text, or the
    /// unset/default marker.
    fn value_text(&self, section: usize, field: usize, spec: &SettingField) -> String {
        let stored = self.text_of(section, field);
        match spec.kind {
            SettingKind::Boolean => {
                if stored.as_deref() == Some("true") {
                    "[x]".to_string()
                } else {
                    "[ ]".to_string()
                }
            }
            SettingKind::Select => {
                let label = stored
                    .as_deref()
                    .map(|value| spec.option_label(value))
                    .unwrap_or_else(|| "(unset)".to_string());
                format!("‹ {label} ›")
            }
            SettingKind::Number | SettingKind::Text => {
                match (stored, self.is_set(section, field)) {
                    (Some(text), true) => text,
                    (Some(text), false) => format!("{text} (default)"),
                    (None, _) => "(unset)".to_string(),
                }
            }
        }
    }
}

fn bool_value(flag: bool) -> SettingValue {
    SettingValue::Bool(flag)
}

/// `╭─ title ─ subtitle ─────╮`
fn card_top(title: &str, subtitle: &str, width: usize) -> Line<'static> {
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
fn card_body(row: Line<'static>, width: usize) -> Line<'static> {
    let inner = width.saturating_sub(4);
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
fn card_bottom(width: usize) -> Line<'static> {
    let style = Style::default().fg(theme::DIM);
    Line::from(vec![
        Span::styled("╰", style),
        Span::styled("─".repeat(width.saturating_sub(2)), style),
        Span::styled("╯", style),
    ])
}

/// The editor's keymap: one line, printable text, the composer's kills.
fn apply_edit_key(composer: &mut Composer, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if !ctrl && !alt => composer.insert_char(c),
        KeyCode::Backspace => composer.backspace(),
        KeyCode::Delete => composer.delete(),
        KeyCode::Left => composer.move_left(),
        KeyCode::Right => composer.move_right(),
        KeyCode::Home => composer.home(),
        KeyCode::End => composer.end(),
        KeyCode::Char('a') if ctrl => composer.home(),
        KeyCode::Char('e') if ctrl => composer.end(),
        KeyCode::Char('u') if ctrl => composer.kill_to_start(),
        KeyCode::Char('k') if ctrl => composer.kill_to_end(),
        KeyCode::Char('w') if ctrl => composer.delete_word_backward(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cst_plugin_api::settings::SettingOption;

    static OPTIONS: [SettingOption; 2] = [
        SettingOption::new("auto", "Auto"),
        SettingOption::new("never", "Never"),
    ];

    static TPS_FIELDS: [SettingField; 3] = [
        SettingField::toggle("enabled", "Meter", "live t/s readout", false),
        SettingField::select("mode", "Mode", "when to fold", &OPTIONS, "auto"),
        SettingField::number("window_ms", "Window", "sample window", None).restart(),
    ];

    static HOST_FIELDS: [SettingField; 1] = [SettingField::toggle(
        "status_bar.plugin_segments",
        "Plugin segments",
        "footer segments",
        true,
    )];

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "codest-settings-view-{}-{:x}-{name}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn view(name: &str, seed: &str) -> SettingsView {
        let path = temp_path(name);
        std::fs::write(&path, seed).expect("seed");
        let file = SettingsFile::load(&path).expect("load");
        SettingsView::new(
            file,
            vec![
                Section {
                    title: "codest".to_string(),
                    description: "host settings",
                    owner: SettingOwner::Host,
                    fields: &HOST_FIELDS,
                },
                Section {
                    title: "tps".to_string(),
                    description: "throughput meter",
                    owner: SettingOwner::Plugin("tps"),
                    fields: &TPS_FIELDS,
                },
            ],
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn focus_walks_every_row_and_escape_closes() {
        let mut view = view("focus", "");
        assert_eq!(view.focused(), Some((0, 0)));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((1, 0)));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((1, 1)));
        // The end of the list is the last row, not a wrap back to the top.
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((1, 2)));
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.focused(), Some((1, 1)));
        assert_eq!(
            view.handle_key(key(KeyCode::Esc)),
            Some(SettingsEvent::Close)
        );
    }

    #[test]
    fn a_boolean_toggle_saves_and_reports_the_section() {
        let mut view = view("toggle", "");
        view.handle_key(key(KeyCode::Down));
        let event = view.handle_key(key(KeyCode::Enter));
        assert_eq!(event, Some(SettingsEvent::Saved { section: 1 }));
        assert_eq!(
            view.global().plugins["tps"].option_bool("enabled"),
            Some(true)
        );
        // Flipping again writes `false` rather than dropping the key.
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            view.global().plugins["tps"].option_bool("enabled"),
            Some(false)
        );
    }

    #[test]
    fn a_select_cycles_forward_with_enter_and_back_with_left() {
        let mut view = view("select", "");
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.text_of(1, 1).as_deref(), Some("auto"));
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(view.text_of(1, 1).as_deref(), Some("never"));
        view.handle_key(key(KeyCode::Left));
        assert_eq!(view.text_of(1, 1).as_deref(), Some("auto"));
    }

    #[test]
    fn a_text_row_edits_rejects_garbage_and_clearing_removes_the_key() {
        let mut view = view("text", "");
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        // Enter opens the editor; typing "abc" is not a number.
        view.handle_key(key(KeyCode::Enter));
        for c in "abc".chars() {
            view.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(view.handle_key(key(KeyCode::Enter)), None);
        assert!(
            view.editing.is_some(),
            "the editor stays open on a bad value"
        );
        assert_eq!(
            view.notice.as_ref().map(|(_, error)| *error),
            Some(true),
            "the notice explains the rejection"
        );

        // Clearing the box and confirming removes the key again.
        for _ in 0..3 {
            view.handle_key(key(KeyCode::Backspace));
        }
        for c in "250".chars() {
            view.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            Some(SettingsEvent::Saved { section: 1 })
        );
        assert_eq!(
            view.global().plugins["tps"].option_u64("window_ms"),
            Some(250)
        );

        // Opening again and emptying the box drops the key (and the empty
        // pocket with it).
        view.handle_key(key(KeyCode::Enter));
        for _ in 0..3 {
            view.handle_key(key(KeyCode::Backspace));
        }
        view.handle_key(key(KeyCode::Enter));
        assert!(!view.global().plugins.contains_key("tps"));
    }

    #[test]
    fn the_host_section_writes_a_dotted_key() {
        let mut view = view("host", "");
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            Some(SettingsEvent::Saved { section: 0 })
        );
        assert!(!view.global().status_bar.plugin_segments);
        // The value is on disk in the host's dotted form, and the typed view
        // the app reads matches the write.
        let text = std::fs::read_to_string(view.file.path()).expect("read");
        assert!(text.contains("plugin_segments = false"), "{text}");
        let reread = crate::core::config::load_global(view.file.path()).expect("parse");
        assert!(!reread.status_bar.plugin_segments);
    }

    #[test]
    fn the_card_layout_brackets_every_row_and_keeps_the_value_column() {
        let mut view = view("render", "");
        let frame = render_view(&mut view, 60, 16);
        let text = frame.join("\n");
        // A card per section: title + subtitle on the top border, rows inside.
        assert!(text.contains("╭─ codest ─ host settings"), "{text}");
        assert!(text.contains("╭─ tps ─ throughput meter"), "{text}");
        assert!(text.contains("╰"), "{text}");
        // The first row is focused, so it carries the pointer and its value.
        assert!(text.contains("❯ Plugin segments"), "{text}");
        assert!(text.contains("[x]"), "the host switch defaults on: {text}");
        // Every body line is exactly the frame width: the value column and
        // the right wall never drift.
        for line in &frame {
            if line.starts_with('│') || line.ends_with('│') {
                assert_eq!(display_width(line), 60, "{line:?}\n{text}");
            }
        }
        // The help bar pins its keys to the right edge.
        assert_eq!(
            frame.last().map(|line| display_width(line)),
            Some(60),
            "{text}"
        );
        // The description rides the row above the keys, so a long hint is not
        // truncated to a stub next to them.
        assert!(
            frame
                .iter()
                .any(|line| line.contains("footer segments · default")),
            "{text}"
        );
    }

    /// Draw the screen into an off-screen buffer and return its rows as text.
    fn render_view(view: &mut SettingsView, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_named_section_is_found_case_insensitively() {
        let mut view = view("named", "");
        assert!(view.focus_section("TPS"));
        assert_eq!(view.focused(), Some((1, 0)));
        assert!(!view.focus_section("nope"));
        assert_eq!(view.section_titles(), vec!["codest", "tps"]);
    }
}
