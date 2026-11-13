//! The `/settings` screen.
//!
//! One card per section — the host's own settings first, then one card per
//! plugin that declared a [`SettingsSection`] — with one row per option.
//!
//! The chrome is not this module's: the header, section strip, page rule,
//! notice line, help bar, and scroll-follows-focus rule come from
//! [`crate::tui::screen`]; the card borders, pointer gutter, and value column
//! from [`crate::tui::cards`]; the one-line editor from
//! [`crate::tui::inline_edit`]. What is left here — and all this module should
//! ever be — is the mapping from a settings field to a row and from a key to
//! a write.
//!
//! Every edit is written immediately through [`SettingsFile`] (validated and
//! atomic) and reported to the app as a [`SettingsEvent`], which applies the
//! new value to the running session and, for a plugin, announces it to the
//! plugin through `Plugin::on_settings_changed`. The screen never owns runtime
//! state — `config.toml` does.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use cst_plugin_api::settings::{SettingField, SettingKind};

use crate::core::config::GlobalConfig;
use crate::core::settings::{SettingOwner, SettingValue, SettingsFile};
use crate::tui::cards::Row;
use crate::tui::inline_edit::{LineEditor, Outcome};
use crate::tui::screen::{Document, Screen};
use crate::tui::theme;

/// The marker a startup-only row carries, next to its label.
const RESTART_MARK: &str = "⟲";

/// One card: a title, a one-line subtitle, and the fields it edits.
pub struct Section {
    /// The card heading, shown on the border and in the section strip.
    pub title: String,
    /// The plugin that owns the card, when one does. `/settings <name>`
    /// accepts the id as well as the title, so a user who knows the id does
    /// not have to know what the plugin called its card.
    pub id: Option<&'static str>,
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
    /// apply it (the file is already saved). `restart` is the edited field's
    /// startup-only flag: the file changed, the running session must not.
    Saved { section: usize, restart: bool },
}

/// The row currently being edited.
struct Editor {
    section: usize,
    field: usize,
    line: LineEditor,
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
    screen: Screen,
}

impl SettingsView {
    /// Build the screen over `sections` from `file`'s current document.
    ///
    /// `SettingsFile::load` already parsed the file as TOML, so the typed
    /// re-read here is expected to succeed; a failure would mean the raw
    /// table and `GlobalConfig` disagree, which the validated write path
    /// prevents. Panicking names the file rather than silently showing
    /// defaults that are not what is stored.
    pub fn new(file: SettingsFile, sections: Vec<Section>) -> Self {
        let global = crate::core::config::load_global(file.path()).unwrap_or_else(|error| {
            panic!(
                "{} parsed as TOML but not as a codest config: {error}",
                file.path().display()
            )
        });
        let mut view = Self {
            file,
            global,
            sections,
            values: Vec::new(),
            focusable: Vec::new(),
            focus: 0,
            editing: None,
            notice: None,
            screen: Screen::default(),
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

    /// Focus the card named `name` (a `/settings <name>` argument), matching
    /// the card title or the owning plugin's id, case-insensitively. Returns
    /// false when no card answers to that name.
    pub fn focus_section(&mut self, name: &str) -> bool {
        let Some(section) = self.sections.iter().position(|model| {
            model.title.eq_ignore_ascii_case(name)
                || model.id.is_some_and(|id| id.eq_ignore_ascii_case(name))
        }) else {
            return false;
        };
        if let Some(index) = self
            .focusable
            .iter()
            .position(|(candidate, _)| *candidate == section)
        {
            self.focus = index;
        }
        // A card with nothing to edit still gets navigated to: the focus
        // stays where it is and the window moves to the card instead.
        self.scroll_to_section(section);
        true
    }

    /// Put the top of card `section` at the top of the viewport; the next
    /// render clamps it to the last page.
    fn scroll_to_section(&mut self, section: usize) {
        let mut line = 0usize;
        for model in self.sections.iter().take(section) {
            // Top border, the body (at least the "no options" line), bottom.
            line += 2 + model.fields.len().max(1);
        }
        self.screen.scroll = line;
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
        if let Some(mut editor) = self.editing.take() {
            match editor.line.handle_key(key) {
                None => {
                    self.editing = Some(editor);
                    return None;
                }
                Some(Outcome::Cancelled) => return None,
                Some(Outcome::Confirmed(text)) => {
                    return self.commit_editor(editor.section, editor.field, text);
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

    /// Feed a bracketed paste. The editor takes it while it is open; with no
    /// editor the paste is refused, so text never lands in the composer
    /// underneath a modal screen. Returns whether it was consumed.
    pub fn paste(&mut self, text: &str) -> bool {
        let Some(editor) = self.editing.as_mut() else {
            return false;
        };
        editor.line.insert_paste(text);
        true
    }

    /// Scroll the list with the wheel. The focus does not move: the next
    /// keyboard step re-anchors the viewport on it, and the entry under the
    /// pointer is not the one a key would act on.
    pub fn wheel(&mut self, lines: isize) {
        self.screen.scroll_by(lines);
    }

    /// Move the focus one row, wrapping at both ends: `↑` on the first row
    /// lands on the last and `↓` on the last lands on the first.
    fn move_focus(&mut self, delta: isize) {
        if let Some(next) = crate::tui::screen::step_focus(self.focus, self.focusable.len(), delta)
        {
            self.focus = next;
        }
    }

    /// Act on the focused row. `enter` distinguishes `Enter` (which opens a
    /// text or number editor) from `→`/`Space` (which only step values).
    fn activate(&mut self, enter: bool) -> Option<SettingsEvent> {
        let (section, field) = self.focused()?;
        let spec = *self.sections.get(section)?.fields.get(field)?;
        let current = self.text_of(section, field);
        match spec.kind {
            SettingKind::Boolean => {
                let next = current.as_deref() != Some("true");
                self.save(section, field, Some(SettingValue::Bool(next)))
            }
            SettingKind::Select => spec.cycle(current.as_deref(), true).and_then(|next| {
                self.save(section, field, Some(SettingValue::Text(next.to_string())))
            }),
            SettingKind::Number | SettingKind::Text => {
                if enter {
                    self.open_editor(section, field);
                }
                None
            }
        }
    }

    /// ← steps a select (or a boolean) backwards; on a text row it does
    /// nothing, so a stray arrow cannot wipe a value.
    fn activate_previous(&mut self) -> Option<SettingsEvent> {
        let (section, field) = self.focused()?;
        let spec = *self.sections.get(section)?.fields.get(field)?;
        let current = self.text_of(section, field);
        match spec.kind {
            SettingKind::Boolean => {
                let next = current.as_deref() != Some("true");
                self.save(section, field, Some(SettingValue::Bool(next)))
            }
            SettingKind::Select => spec.cycle(current.as_deref(), false).and_then(|next| {
                self.save(section, field, Some(SettingValue::Text(next.to_string())))
            }),
            _ => None,
        }
    }

    /// Open the one-line editor on a text or number row, seeded with the
    /// stored value — not the default, since typing over a friendly default
    /// would pin it as an explicit value.
    fn open_editor(&mut self, section: usize, field: usize) {
        let line = match self.stored(section, field) {
            Some(text) => LineEditor::with_text(text),
            None => LineEditor::empty(),
        };
        self.editing = Some(Editor {
            section,
            field,
            line,
        });
    }

    /// Re-open the editor holding `text` (a rejected value the user should
    /// be able to fix rather than retype).
    fn open_editor_with(&mut self, section: usize, field: usize, text: String) {
        self.editing = Some(Editor {
            section,
            field,
            line: LineEditor::with_text(text),
        });
    }

    /// The value as stored in the file, ignoring the field's default.
    fn stored(&self, section: usize, field: usize) -> Option<String> {
        self.values
            .get(section)
            .and_then(|values| values.get(field))
            .cloned()
            .flatten()
    }

    /// Save the editor's text: empty removes the key (restoring the plugin's
    /// default), anything else is parsed by the field's kind first.
    fn commit_editor(
        &mut self,
        section: usize,
        field: usize,
        text: String,
    ) -> Option<SettingsEvent> {
        let spec = *self.sections.get(section)?.fields.get(field)?;
        let value = if text.trim().is_empty() {
            None
        } else {
            match SettingValue::parse(&spec, &text) {
                Ok(value) => Some(value),
                Err(message) => {
                    // Keep the editor open holding the offending text: the
                    // row is not lost and the notice says what went wrong.
                    self.notice = Some((message, true));
                    self.open_editor_with(section, field, text);
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
        let restart = spec.restart;
        match self.file.write(owner, key, value) {
            Ok(global) => {
                self.global = global;
                self.reindex();
                self.notice = Some((format!("saved {title}.{key}"), false));
                Some(SettingsEvent::Saved { section, restart })
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
        let document = self.doc(area.width as usize);
        let crumb = self
            .focused()
            .and_then(|(section, _)| self.sections.get(section))
            .map(|model| model.title.clone());
        let titles: Vec<String> = self
            .sections
            .iter()
            .map(|model| model.title.clone())
            .collect();
        let active = self.focused().map(|(section, _)| section);
        let counter = (!self.focusable.is_empty()).then_some((self.focus, self.focusable.len()));
        let description = self.focused_description();
        let notice = self
            .notice
            .as_ref()
            .map(|(text, error)| (text.as_str(), *error));
        let focus = self.focused().map(|_| self.focus);
        let mut chrome = crate::tui::screen::Chrome::new("Settings");
        chrome.crumb = crumb.as_deref();
        chrome.counter = counter;
        chrome.strip = Some(&titles);
        chrome.active_section = active;
        chrome.notice = notice;
        chrome.description = description.as_deref();
        chrome.keys = self.keys();
        self.screen
            .render(frame, area, &document, focus, &chrome, None);
    }

    /// The whole document as lines: one card per section, one line per field.
    fn doc(&self, width: usize) -> Document {
        let mut document = Document::new();
        for (index, model) in self.sections.iter().enumerate() {
            document.push(
                crate::tui::cards::top(&model.title, model.description, width),
                None,
            );
            if model.fields.is_empty() {
                document.push(
                    crate::tui::cards::body(
                        Line::from(Span::styled(
                            "  no configurable options",
                            Style::default().fg(theme::DIM),
                        )),
                        width,
                    ),
                    None,
                );
            }
            for (field_index, spec) in model.fields.iter().enumerate() {
                let focus = self
                    .focusable
                    .iter()
                    .position(|pair| *pair == (index, field_index));
                // While an editor is open it owns the highlight, so the
                // pointer does not also mark whatever row the focus is on.
                let is_focused = match &self.editing {
                    Some(editor) => editor.section == index && editor.field == field_index,
                    None => focus == Some(self.focus),
                };
                let row = self.field_row(index, field_index, spec, is_focused);
                document.push(crate::tui::cards::body(row.line(width), width), focus);
            }
            document.push(crate::tui::cards::bottom(width), None);
        }
        document
    }

    /// One field as a row: its label (plus the restart marker) and the value
    /// chip, styled by kind and focus.
    fn field_row(&self, section: usize, field: usize, spec: &SettingField, focused: bool) -> Row {
        let editing = self
            .editing
            .as_ref()
            .filter(|editor| editor.section == section && editor.field == field);
        let value = match editing {
            Some(editor) => editor.line.display(),
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
        // A startup-only row says so where the user looks; the help bar
        // spells it out when the row is focused.
        let label = if spec.restart {
            format!("{} {RESTART_MARK}", spec.label)
        } else {
            spec.label.to_string()
        };
        Row::new(label, value)
            .focused(focused)
            .value_style(value_style)
    }

    /// The value chip: `[x]`/`[ ]`, `‹ label ›`, the stored text, or the
    /// default/unset marker.
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

    /// The focused row's description, plus the qualifiers the row cannot say
    /// in one line.
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

    /// The help bar's key list for what the keyboard currently does.
    fn keys(&self) -> &'static str {
        if self.editing.is_some() {
            return "Enter save · Esc cancel";
        }
        match self.focused_kind() {
            Some(SettingKind::Boolean) => "↑↓ select · Enter toggle · Esc close",
            Some(SettingKind::Select) => "↑↓ select · Enter/←→ change · Esc close",
            Some(SettingKind::Number | SettingKind::Text) => "↑↓ select · Enter edit · Esc close",
            None => "Esc close",
        }
    }

    fn focused_kind(&self) -> Option<SettingKind> {
        let (section, field) = self.focused()?;
        Some(self.sections.get(section)?.fields.get(field)?.kind)
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
                    id: None,
                    description: "host settings",
                    owner: SettingOwner::Host,
                    fields: &HOST_FIELDS,
                },
                Section {
                    // A plugin whose card title differs from its id: both
                    // must be accepted by `/settings <name>`.
                    title: "Throughput".to_string(),
                    id: Some("tps"),
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
    fn focus_walks_every_row_and_wraps_at_the_ends() {
        let mut view = view("focus", "");
        assert_eq!(view.focused(), Some((0, 0)));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((1, 0)));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((1, 1)));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((1, 2)));
        // ↓ on the last row wraps to the first, ↑ on the first to the last.
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.focused(), Some((0, 0)), "wrapped to the top");
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.focused(), Some((1, 2)), "wrapped to the bottom");
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.focused(), Some((1, 1)));
        assert_eq!(
            view.handle_key(key(KeyCode::Esc)),
            Some(SettingsEvent::Close)
        );
    }

    #[test]
    fn the_wheel_scrolls_without_moving_the_focus() {
        let mut view = view("wheel", "");
        assert_eq!(view.focused(), Some((0, 0)));
        view.wheel(3);
        assert_eq!(view.screen.scroll, 3, "the viewport moved");
        assert_eq!(view.focused(), Some((0, 0)), "the focus did not");
        view.wheel(-99);
        assert_eq!(view.screen.scroll, 0, "never negative");
    }

    #[test]
    fn a_boolean_toggle_saves_and_reports_the_section() {
        let mut view = view("toggle", "");
        view.handle_key(key(KeyCode::Down));
        let event = view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            event,
            Some(SettingsEvent::Saved {
                section: 1,
                restart: false
            })
        );
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
    fn a_startup_only_field_reports_the_restart_flag_so_the_app_skips_it() {
        let mut view = view("restart", "");
        // The third row of the tps card is the number field marked restart.
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::End));
        assert_eq!(view.focused(), Some((1, 2)));
        view.handle_key(key(KeyCode::Enter));
        for c in "250".chars() {
            view.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            Some(SettingsEvent::Saved {
                section: 1,
                restart: true
            }),
            "the app needs the flag to know this one must not apply live"
        );
        assert_eq!(
            view.global().plugins["tps"].option_u64("window_ms"),
            Some(250)
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
        view.handle_key(key(KeyCode::End));
        // Enter opens the editor; typing "abc" is not a number.
        view.handle_key(key(KeyCode::Enter));
        assert!(view.editing.is_some(), "Enter opens the editor");
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

        // The rejected text is still in the box, so a fix does not retype it.
        assert_eq!(
            view.editing.as_ref().map(|e| e.line.text()),
            Some("abc".to_string())
        );
        for _ in 0..3 {
            view.handle_key(key(KeyCode::Backspace));
        }
        for c in "250".chars() {
            view.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            Some(SettingsEvent::Saved {
                section: 1,
                restart: true
            })
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
            Some(SettingsEvent::Saved {
                section: 0,
                restart: false
            })
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
        assert!(text.contains("╭─ Throughput ─ throughput meter"), "{text}");
        assert!(text.contains("╰"), "{text}");
        // The first row is focused, so it carries the pointer and its value.
        assert!(text.contains("❯ Plugin segments"), "{text}");
        assert!(text.contains("[x]"), "the host switch defaults on: {text}");
        // Every body line is exactly the frame width: the value column and
        // the right wall never drift.
        for line in &frame {
            if line.starts_with('│') || line.ends_with('│') {
                assert_eq!(
                    crate::tui::wrap::display_width(line),
                    60,
                    "{line:?}\n{text}"
                );
            }
        }
        // The description rides the row above the keys, so a long hint is not
        // truncated to a stub next to them.
        assert!(
            frame
                .iter()
                .any(|line| line.contains("footer segments · default")),
            "{text}"
        );
    }

    #[test]
    fn a_startup_only_row_is_marked_on_the_row_itself() {
        let mut view = view("badge", "");
        let frame = render_view(&mut view, 60, 16);
        let text = frame.join("\n");
        assert!(
            text.contains(&format!("Window {RESTART_MARK}")),
            "the row carries the restart marker: {text}"
        );
        // The default row — a plain toggle — carries no marker.
        assert!(text.contains("Meter"), "{text}");
        assert!(!text.contains(&format!("Meter {RESTART_MARK}")), "{text}");
    }

    #[test]
    fn a_named_section_matches_the_title_or_the_plugin_id() {
        let mut view = view("named", "");
        assert!(view.focus_section("THROUGHPUT"), "the title matches");
        assert_eq!(view.focused(), Some((1, 0)));
        assert!(view.focus_section("tps"), "the plugin id matches too");
        assert_eq!(view.focused(), Some((1, 0)));
        assert!(view.focus_section("codest"));
        assert_eq!(view.focused(), Some((0, 0)));
        assert!(!view.focus_section("nope"));
        assert_eq!(view.section_titles(), vec!["codest", "Throughput"]);
    }

    #[test]
    fn a_paste_with_no_editor_open_is_refused_so_it_cannot_leak() {
        let mut view = view("paste", "");
        // Refused, not swallowed: the app drops the paste instead of feeding
        // the composer underneath this modal screen.
        assert!(!view.paste("leaked text"));
    }

    #[test]
    fn a_paste_into_the_editor_lands_there_with_newlines_flattened() {
        let mut view = view("paste-editor", "");
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::End));
        view.handle_key(key(KeyCode::Enter));
        assert!(view.paste("12\n34"), "the editor consumes it");
        assert_eq!(
            view.editing.as_ref().map(|editor| editor.line.text()),
            Some("12 34".to_string())
        );
    }
}
