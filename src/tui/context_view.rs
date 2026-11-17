//! The `/context` viewer: the conversation as the model sees it, editable.
//!
//! Two modes share this module. Before a session has transcript messages the
//! command prints the classic prompt-debug report ([`build_context_report`],
//! carried over from the old `prompt_debug` plugin); once messages exist the
//! same command opens [`ContextView`], a full-frame take-over in the spirit
//! of reference TUIs' trajectory scenes: one row per context entry with a
//! role badge and a preview, a fixed inspector showing the selected entry in
//! full, and delete / edit / undo over the transcript. An assistant
//! tool-calls round drills in with Enter: its calls become the list, and `d`
//! splices one call — with the result answering it — out of the context.
//! Insertion is reserved (the core already has the primitive) but not wired.
//!
//! The view never touches the transcript itself — the worker owns it. Every
//! mutation is an op ([`crate::core::protocol::Op::DeleteMessages`],
//! [`crate::core::protocol::Op::EditMessage`],
//! [`crate::core::protocol::Op::UndoContextEdit`]) and the refreshed state
//! comes back as [`crate::core::protocol::EventMsg::ConversationSnapshot`].

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::core::protocol::{Message, Role};
use crate::core::tokens::{format_context_usage, format_tokens};
use crate::plugins::{PluginInfo, ReportBlock, ReportEntry, ReportGroup};
use crate::tui::composer::Composer;
use crate::tui::screen::{Document, Panel, Screen};
use crate::tui::theme;
use crate::tui::wrap::{display_width, wrap_str};

// ---------------------------------------------------------------------------
// The pre-session report (formerly the `prompt_debug` plugin)
// ---------------------------------------------------------------------------

/// Longest section text shown per entry before clipping, mirroring the
/// loaded-context panels of reference TUIs.
const SECTION_MAX_CHARS: usize = 800;

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

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The classic `/context` report: tools, the assembled system prompt with
/// per-plugin attribution, the request's destination and static fields, and
/// token usage. Built from [`PluginInfo`], which the front-end already
/// assembles for the plugin hooks.
pub fn build_context_report(info: &PluginInfo<'_>) -> ReportBlock {
    let mut groups: Vec<ReportGroup> = Vec::new();

    // Tools first: `/tools` is the one thing the report is routinely
    // consulted for, and the system prompt below can be long enough to
    // push everything else off screen.
    let tools = info.tools;
    let enabled_count = tools
        .iter()
        .filter(|tool| info.enabled_tools.iter().any(|name| name == tool.name))
        .count();
    groups.push(ReportGroup {
        heading: format!("Tools ({enabled_count}/{} enabled)", tools.len()),
        entries: match tools.is_empty() {
            true => vec![ReportEntry::new("", "none registered")],
            false => tools
                .iter()
                .map(|tool| {
                    let mut line = if info.enabled_tools.iter().any(|name| name == tool.name) {
                        "enabled".to_string()
                    } else {
                        "disabled".to_string()
                    };
                    if tool.read_only {
                        line.push_str(" · read-only");
                    }
                    ReportEntry::new(tool.name, line)
                })
                .collect(),
        },
    });

    // System prompt: the base prompt plus each plugin's fragment,
    // attributed so it is clear who contributes what.
    let sections = info.system_sections;
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
    let request = info.request;
    let mut request_entries = vec![ReportEntry::new("model", info.model)];
    if request.endpoint.is_empty() {
        request_entries.push(ReportEntry::new("endpoint", "(no route configured)"));
    } else {
        request_entries.push(ReportEntry::new("protocol", request.protocol));
        request_entries.push(ReportEntry::new("endpoint", request.endpoint));
    }
    request_entries.push(ReportEntry::new(
        "effort",
        match info.effort {
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
        format!("{} (system + transcript)", info.message_count),
    ));
    groups.push(ReportGroup {
        heading: "Request".to_string(),
        entries: request_entries,
    });

    // Usage: the measured context size, as in the footer.
    let context = match info
        .last_usage
        .map(|(input, output)| input.saturating_add(output))
    {
        Some(tokens) => {
            let mut line = format_context_usage(tokens, info.context_window);
            if info.context_window.is_none() {
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
                    format_tokens(info.total_input),
                    format_tokens(info.total_output)
                ),
            ),
        ],
    });

    ReportBlock {
        title: "context".to_string(),
        groups,
    }
}

// ---------------------------------------------------------------------------
// The in-session viewer
// ---------------------------------------------------------------------------

/// What one key press in the viewer asks the app to do. The view never
/// submits ops itself; the app owns the session handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextAction {
    Close,
    /// Delete the named transcript indices (already cascade-expanded).
    Delete {
        indices: Vec<usize>,
    },
    /// Splice one tool call (with its result) out of the transcript message
    /// at `index` — the drill-down's per-call delete.
    DeleteToolCall {
        index: usize,
        call_id: String,
    },
    /// Replace the content of the transcript message at `index`.
    Edit {
        index: usize,
        content: String,
    },
    /// Undo the most recent mutation.
    Undo,
}

/// State of the open `/context` viewer. `messages` carries the worker's own
/// snapshot (system prompt first); `None` until the first
/// `ConversationSnapshot` arrives.
#[derive(Default)]
pub struct ContextView {
    messages: Option<Vec<Message>>,
    selected: usize,
    /// A `d` press awaiting its `y/n`.
    confirm_delete: bool,
    /// A tool round drilled into: the assistant entry's display index, and
    /// which call the cursor sits on. `None` browses the message list.
    drilled: Option<(usize, usize)>,
    /// The edit box for the selected entry, when editing.
    editing: Option<Composer>,
    error: Option<String>,
    /// The list's viewport. Focus-follow gives way to the wheel: a wheel
    /// notch scrolls without moving the selection, and the next ↑↓ re-anchors
    /// the window on it.
    screen: Screen,
}

impl ContextView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a `ConversationSnapshot`. The selection follows its entry by
    /// position, clamped into the new range.
    pub fn apply_snapshot(&mut self, messages: Vec<Message>) {
        self.selected = self.selected.min(messages.len().saturating_sub(1));
        if self.editing.is_some() {
            // An edit box open across a refresh would now point at stale
            // text; close it — the snapshot is the newer truth.
            self.editing = None;
        }
        // The drill survives a refresh only while its entry is still a
        // tool-calls round: deleting a round's last call (or the round
        // itself) closes it honestly instead of pointing at nothing.
        if let Some((entry, cursor)) = self.drilled {
            let count = messages
                .get(entry)
                .filter(|message| message.role == Role::Assistant)
                .and_then(|message| message.tool_calls.as_ref())
                .map(|calls| calls.len())
                .unwrap_or(0);
            self.drilled = (count > 0).then(|| (entry, cursor.min(count - 1)));
        }
        self.confirm_delete = false;
        self.error = None;
        self.messages = Some(messages);
    }

    /// A failed op (shape rejection, out-of-range index, exhausted undo).
    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    fn entries(&self) -> Option<&[Message]> {
        self.messages.as_deref()
    }

    /// Entry count once a snapshot arrived (`None` while loading).
    pub fn entries_len(&self) -> Option<usize> {
        self.messages.as_ref().map(|messages| messages.len())
    }

    /// Whether a rejected op is being shown.
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// Feed a bracketed paste. Only an open edit box takes it — a paste into
    /// the list would otherwise land in the chat composer underneath this
    /// modal screen. Returns whether it was consumed.
    pub fn paste(&mut self, text: &str) -> bool {
        let Some(composer) = self.editing.as_mut() else {
            return false;
        };
        composer.insert_str(text);
        true
    }

    fn selected_entry(&self) -> Option<&Message> {
        self.entries()?.get(self.selected)
    }

    /// The transcript index (`selected - 1`; entry 0 is the system prompt).
    fn transcript_index(&self) -> Option<usize> {
        self.selected.checked_sub(1)
    }

    /// How many calls the drilled round currently carries.
    fn round_call_count(&self, entry: usize) -> usize {
        self.entries()
            .and_then(|entries| entries.get(entry))
            .and_then(|message| message.tool_calls.as_ref())
            .map(|calls| calls.len())
            .unwrap_or(0)
    }

    /// The per-call delete the confirmation would submit: the call under the
    /// drill cursor, addressed by transcript index and call id.
    fn drilled_delete_action(&self, entry: usize, cursor: usize) -> Option<ContextAction> {
        let message = self.entries()?.get(entry)?;
        let call = message.tool_calls.as_ref()?.get(cursor)?;
        let index = entry.checked_sub(1)?;
        Some(ContextAction::DeleteToolCall {
            index,
            call_id: call.id.clone(),
        })
    }

    /// Handle a key while the viewer is open. Fully modal: every key is
    /// consumed, and anything that reaches the worker comes back as an
    /// action for the app to submit.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<ContextAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // The edit box owns the keyboard first, like the `/provider`
        // wizard's text steps: typing edits, Enter saves, Esc cancels.
        if self.editing.is_some() {
            match key.code {
                KeyCode::Enter if !ctrl && !alt => {
                    let composer = self.editing.take()?;
                    let index = self.transcript_index()?;
                    return Some(ContextAction::Edit {
                        index,
                        content: composer.text(),
                    });
                }
                KeyCode::Enter if ctrl || alt => {
                    if let Some(composer) = self.editing.as_mut() {
                        composer.insert_char('\n');
                    }
                    return None;
                }
                KeyCode::Char('c') if ctrl => {
                    // Ctrl+C cancels the edit rather than quitting.
                    self.editing = None;
                    return None;
                }
                KeyCode::Esc => {
                    self.editing = None;
                    return None;
                }
                // Everything else edits the box: the same keys the main
                // composer and the wizard's text steps take.
                _ => {
                    if let Some(composer) = self.editing.as_mut() {
                        apply_edit_key(composer, key);
                    }
                    return None;
                }
            }
        }

        // A `d` press waits for its confirmation; nothing else acts. Drilled
        // in, the confirmed delete is the cursor's call; on the message list,
        // the selected entry with its cascade.
        if self.confirm_delete {
            self.confirm_delete = false;
            return match key.code {
                KeyCode::Char('y') => match self.drilled {
                    Some((entry, cursor)) => self.drilled_delete_action(entry, cursor),
                    None => {
                        let indices = self
                            .entries()
                            .map(|entries| expand_delete_indices(entries, self.selected));
                        indices
                            .filter(|indices| !indices.is_empty())
                            .map(|indices| ContextAction::Delete { indices })
                    }
                },
                _ => None,
            };
        }

        // Drilled into a tool round: the calls are the list. `d` arms a
        // per-call delete, Esc/Left/Backspace climb back out, `q` still
        // closes the whole viewer.
        if let Some((entry, cursor)) = self.drilled {
            let count = self.round_call_count(entry);
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => self.drilled = None,
                KeyCode::Char('q') => return Some(ContextAction::Close),
                KeyCode::Up => self.drilled = Some((entry, step_cursor(cursor, count, -1))),
                KeyCode::Down => self.drilled = Some((entry, step_cursor(cursor, count, 1))),
                KeyCode::PageUp => self.drilled = Some((entry, step_cursor(cursor, count, -11))),
                KeyCode::PageDown => self.drilled = Some((entry, step_cursor(cursor, count, 11))),
                KeyCode::Char('g') => self.drilled = Some((entry, 0)),
                KeyCode::Char('G') => self.drilled = Some((entry, count.saturating_sub(1))),
                KeyCode::Char('d') => {
                    self.drilled = Some((entry, cursor));
                    if count > 0 {
                        self.confirm_delete = true;
                    }
                }
                // The undo works right where the delete happened: the
                // refreshed snapshot keeps the drill on the restored round.
                KeyCode::Char('u') => return Some(ContextAction::Undo),
                // Anything else keeps the drill exactly where it is.
                _ => self.drilled = Some((entry, cursor)),
            }
            return None;
        }

        match key.code {
            KeyCode::Esc => Some(ContextAction::Close),
            KeyCode::Char('q') => Some(ContextAction::Close),
            KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::PageUp => {
                self.move_selection(-11);
                None
            }
            KeyCode::PageDown => {
                self.move_selection(11);
                None
            }
            KeyCode::Char('g') => {
                self.selected = 0;
                None
            }
            KeyCode::Char('G') => {
                if let Some(entries) = self.entries() {
                    self.selected = entries.len().saturating_sub(1);
                }
                None
            }
            KeyCode::Char('d') => {
                // Only deletable entries arm the confirmation; anything else
                // leaves the state untouched.
                if self.selected_entry().is_some_and(deletable) {
                    self.confirm_delete = true;
                }
                None
            }
            KeyCode::Enter => {
                // A tool-calls round opens its per-call drill-down; plain
                // entries have nothing inside to open.
                if let Some(message) = self.selected_entry()
                    && message.role == Role::Assistant
                    && message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
                {
                    self.drilled = Some((self.selected, 0));
                }
                None
            }
            KeyCode::Char('e') => {
                if let Some(message) = self.selected_entry().filter(|message| editable(message)) {
                    let mut composer = Composer::new();
                    composer.set_text(message.content.clone());
                    self.editing = Some(composer);
                }
                None
            }
            KeyCode::Char('u') => Some(ContextAction::Undo),
            _ => None,
        }
    }

    /// Move the selection one step, wrapping at both ends: `↑` on the first
    /// entry lands on the last and `↓` on the last lands on the first.
    fn move_selection(&mut self, delta: isize) {
        let Some(len) = self.entries().map(|entries| entries.len()) else {
            return;
        };
        if let Some(next) = crate::tui::screen::step_focus(self.selected, len, delta) {
            self.selected = next;
        }
    }

    /// Scroll the entry list with the wheel. The selection stays put, so a
    /// wheel notch browses without changing what `d`/`e` would act on.
    pub fn wheel(&mut self, lines: isize) {
        self.screen.scroll_by(lines);
    }

    /// Render over the whole frame area, through the shared full-frame chrome:
    /// a header with the entry count, the scrolling entry list, the inspector
    /// (or the edit box) as a fixed panel, and the help bar.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        let document = self.doc(width);
        let count = self.entries().map(|entries| entries.len());

        let crumb = match self.drilled {
            Some((entry, _)) => {
                let calls = self.round_call_count(entry);
                format!(
                    "transcript #{} · {} call{}",
                    entry.saturating_sub(1),
                    calls,
                    plural(calls)
                )
            }
            None => match count {
                Some(0) => "no entries".to_string(),
                Some(count) => format!(
                    "{count} entr{} (system + transcript)",
                    if count == 1 { "y" } else { "ies" }
                ),
                None => "loading…".to_string(),
            },
        };
        let mut chrome = crate::tui::screen::Chrome::new("Context");
        chrome.crumb = Some(&crumb);
        chrome.counter = match self.drilled {
            Some((entry, cursor)) => {
                let calls = self.round_call_count(entry);
                (calls > 0).then_some((cursor, calls))
            }
            None => count
                .filter(|count| *count > 0)
                .map(|count| (self.selected, count)),
        };
        // The inspector keeps roughly two fifths of the screen, bounded so a
        // tall terminal does not turn into one long panel and a short one
        // still shows the list.
        chrome.panel_height = match area.height {
            0..=3 => 0,
            height => ((height - 2) * 2 / 5).clamp(3, 12).min(height - 3),
        };
        chrome.notice = self.error.as_ref().map(|error| (error.as_str(), true));
        chrome.description = None;
        chrome.keys = self.hint_keys();

        // The inspector is rendered to lines here so the shared frame can
        // place it: it is read-only, and the panel shape keeps the layout
        // arithmetic in one place.
        let panel =
            (chrome.panel_height > 0).then(|| self.inspector_panel(width, chrome.panel_height));
        // Focus is the entry index on the message list, the call cursor when
        // drilled in.
        let focus = match self.drilled {
            Some((entry, cursor)) => (self.round_call_count(entry) > 0).then_some(cursor),
            None => count.map(|_| self.selected),
        };
        self.screen
            .render(frame, area, &document, focus, &chrome, panel);
        // The list may have shrunk under the wheel (a delete landed): keep
        // the viewport inside the new document.
        self.screen.clamp(document.len());
    }

    /// The entry list as the shared document: one focusable line per entry —
    /// or, drilled into a round, one focusable line per call.
    fn doc(&self, width: usize) -> Document {
        let Some(entries) = self.entries() else {
            return Document::new();
        };
        if let Some((entry, cursor)) = self.drilled {
            let calls = self.round_call_count(entry);
            return Document::push_focused(
                (0..calls).map(|position| call_line(entries, entry, position, position == cursor, width)),
            );
        }
        Document::push_focused(
            entries
                .iter()
                .enumerate()
                .map(|(index, message)| entry_line(message, index == self.selected, width)),
        )
    }

    /// The key list for the help bar's right edge.
    fn hint_keys(&self) -> &'static str {
        if self.editing.is_some() {
            return "Enter save · Ctrl+J newline · Esc cancel";
        }
        if self.confirm_delete {
            return if self.drilled.is_some() {
                "delete call + result? y/n"
            } else {
                "delete? y/n"
            };
        }
        if self.drilled.is_some() {
            return "↑↓ call · d delete call · u undo · Esc back · q close";
        }
        "↑↓ move · Enter tool calls · d delete · e edit · u undo · Esc close"
    }

    /// The inspector as lines: the edit box, or the selected entry — or the
    /// selected call, drilled in — in full with an honest overflow marker.
    fn inspector_panel(&self, width: usize, height: u16) -> crate::tui::screen::Panel<'static> {
        if let Some(composer) = self.editing.as_ref() {
            return editor_panel(composer, width, height);
        }
        crate::tui::screen::Panel::Lines(self.inspector_lines(width, height))
    }

    /// The selected entry in full, or nothing while there is none.
    fn inspector_lines(&self, width: usize, height: u16) -> Vec<Line<'static>> {
        let Some(messages) = self.entries() else {
            return Vec::new();
        };
        if let Some((entry, cursor)) = self.drilled {
            return call_inspector_lines(messages, entry, cursor, width, height);
        }
        let Some(message) = self.selected_entry() else {
            return Vec::new();
        };
        let index = self.selected;

        // Title row, then the full content; the overflow marker trades the
        // last row for an honest "+N more".
        let title = format!(
            "─ {} · {} chars · transcript #{}",
            role_label(message.role),
            message.content.chars().count(),
            index.saturating_sub(1),
        );
        let mut lines = vec![Line::from(Span::styled(
            title,
            Style::default().fg(theme::SUBTLE),
        ))];
        lines.extend(message_lines(message, width));
        clamp_with_overflow(lines, height)
    }
}

/// Move the drill cursor one step within a round's calls, wrapping at both
/// ends like the entry list; an empty round keeps the cursor at 0.
fn step_cursor(current: usize, count: usize, delta: isize) -> usize {
    match count {
        0 => 0,
        _ => crate::tui::screen::step_focus(current, count, delta).unwrap_or(0),
    }
}

/// The result message answering a call within the round's contiguous
/// `Role::Tool` block, when it is there.
fn result_of<'a>(
    entries: &'a [Message],
    entry: usize,
    call_id: &str,
) -> Option<&'a Message> {
    entries
        .get(entry + 1..)?
        .iter()
        .take_while(|message| message.role == Role::Tool)
        .find(|message| message.tool_call_id.as_deref() == Some(call_id))
}

/// Trim inspector lines into the panel height, trading the last row for an
/// honest "+N more" when they overflow.
fn clamp_with_overflow(mut lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let visible = (height as usize).max(1);
    let overflow = lines.len().saturating_sub(visible);
    lines.truncate(visible.saturating_sub(usize::from(overflow > 0)));
    if overflow > 0 {
        lines.push(Line::from(Span::styled(
            format!("… +{overflow} more lines"),
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}

/// One drilled-in call row: cursor marker, the call, and what its result
/// weighs — the number a per-call delete frees.
fn call_line(
    entries: &[Message],
    entry: usize,
    position: usize,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let Some(call) = entries
        .get(entry)
        .and_then(|message| message.tool_calls.as_ref())
        .and_then(|calls| calls.get(position))
    else {
        return Line::from(Span::raw(String::new()));
    };
    let cursor = if selected { "▸ " } else { "  " };
    let arguments = call.arguments.trim();
    let arguments = match arguments {
        "" | "{}" => String::new(),
        _ => format!(" {arguments}"),
    };
    let result_note = match result_of(entries, entry, &call.id) {
        Some(result) => match result.content.chars().count() {
            0 => " · result (empty)".to_string(),
            chars => format!(" · result {chars} chars"),
        },
        None => " · result (missing)".to_string(),
    };
    let available = width.saturating_sub(cursor.len()).max(1);
    let text = wrap_str(
        &format!("⚙ {}{}{result_note}", call.name, arguments),
        available,
    )
    .into_iter()
    .next()
    .unwrap_or_default();
    Line::from(vec![
        Span::styled(
            cursor.to_string(),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(
            text,
            Style::default().fg(if selected { theme::ACCENT } else { theme::DIM }),
        ),
    ])
}

/// The drilled call in full: identity, the arguments, then the result it
/// got — the content a delete would take out of the context.
fn call_inspector_lines(
    entries: &[Message],
    entry: usize,
    cursor: usize,
    width: usize,
    height: u16,
) -> Vec<Line<'static>> {
    let Some(call) = entries
        .get(entry)
        .and_then(|message| message.tool_calls.as_ref())
        .and_then(|calls| calls.get(cursor))
    else {
        return Vec::new();
    };
    let title = format!(
        "─ ⚙ {} · {} · transcript #{}",
        call.id,
        call.name,
        entry.saturating_sub(1),
    );
    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default().fg(theme::SUBTLE),
    ))];
    let arguments = call.arguments.trim();
    let body = match arguments {
        "" | "{}" => "(no arguments)".to_string(),
        _ => arguments.to_string(),
    };
    for row in wrap_str(&body, width.max(1)) {
        lines.push(Line::from(Span::styled(
            row,
            Style::default().fg(theme::ACCENT),
        )));
    }
    match result_of(entries, entry, &call.id) {
        Some(result) => {
            lines.push(Line::from(Span::styled(
                "─ result",
                Style::default().fg(theme::SUBTLE),
            )));
            if result.content.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(empty)",
                    Style::default().fg(theme::DIM),
                )));
            } else {
                for paragraph in result.content.split('\n') {
                    if paragraph.is_empty() {
                        lines.push(Line::raw(String::new()));
                        continue;
                    }
                    for row in wrap_str(paragraph, width.max(1)) {
                        lines.push(Line::raw(row));
                    }
                }
            }
        }
        None => lines.push(Line::from(Span::styled(
            "no result message",
            Style::default().fg(theme::DIM),
        ))),
    }
    clamp_with_overflow(lines, height)
}

/// Whether `d` may target this entry: user and assistant messages, including
/// the tool-calls round (its results come along by cascade). The system
/// prompt and lone tool results are locked.
fn deletable(message: &Message) -> bool {
    matches!(message.role, Role::User | Role::Assistant)
}

/// Feed one key to the edit box's composer — the wizard text steps' keymap:
/// printable chars, movement, kills, Ctrl+J for a newline.
fn apply_edit_key(composer: &mut Composer, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(c) if !ctrl && !alt => composer.insert_char(c),
        KeyCode::Char('j') if ctrl => composer.insert_char('\n'),
        KeyCode::Enter if ctrl || alt || shift => composer.insert_char('\n'),
        KeyCode::Backspace => composer.backspace(),
        KeyCode::Delete => composer.delete(),
        KeyCode::Left => composer.move_left(),
        KeyCode::Right => composer.move_right(),
        KeyCode::Up => {
            composer.move_up();
        }
        KeyCode::Down => {
            composer.move_down();
        }
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

/// Whether `e` may open this entry: any non-system message's content.
fn editable(message: &Message) -> bool {
    message.role != Role::System
}

/// The transcript indices one delete removes: the entry itself, plus — for
/// an assistant tool-calls message — the tool results that answer its calls.
/// Indices are transcript positions (the display index minus the system
/// entry). A locked entry expands to nothing.
pub fn expand_delete_indices(messages: &[Message], selected: usize) -> Vec<usize> {
    let Some(message) = messages.get(selected) else {
        return Vec::new();
    };
    if !deletable(message) {
        return Vec::new();
    }
    let mut indices = vec![selected - 1];
    if let Some(calls) = &message.tool_calls {
        let ids: BTreeSet<&str> = calls.iter().map(|call| call.id.as_str()).collect();
        let mut cursor = selected + 1;
        while cursor < messages.len() {
            let next = &messages[cursor];
            if next.role != Role::Tool {
                break;
            }
            match next.tool_call_id.as_deref() {
                Some(id) if ids.contains(id) => indices.push(cursor - 1),
                _ => break,
            }
            cursor += 1;
        }
    }
    indices
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn role_color(role: Role) -> ratatui::style::Color {
    match role {
        Role::System => theme::SUBTLE,
        Role::User => theme::USER,
        Role::Assistant => theme::ASSISTANT,
        Role::Tool => theme::WARNING,
    }
}

/// One list row: cursor marker, role badge, and a whitespace-collapsed
/// preview truncated to the width.
fn entry_line(message: &Message, selected: bool, width: usize) -> Line<'static> {
    let cursor = if selected { "▸ " } else { "  " };
    let label = role_label(message.role);
    let badge = format!("[{label}]");
    let badge_width = display_width(&badge);
    let gutter = cursor.len() + badge_width + 1;
    let available = width.saturating_sub(gutter).max(1);
    let preview = wrap_str(&preview_text(message), available)
        .into_iter()
        .next()
        .unwrap_or_default();

    let mut spans = vec![
        Span::styled(
            cursor.to_string(),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            badge,
            Style::default()
                .fg(role_color(message.role))
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::raw(" "),
        Span::styled(preview, Style::default().fg(theme::DIM)),
    ];
    // The system prompt is the one entry no operation may touch.
    if message.role == Role::System {
        spans.push(Span::styled(" ·locked", Style::default().fg(theme::DIM)));
    }
    Line::from(spans)
}

/// The one-line preview: collapsed content, or the tool calls an assistant
/// message asks for.
fn preview_text(message: &Message) -> String {
    let collapsed: String = message
        .content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let calls = message
        .tool_calls
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|call| {
            let arguments = call.arguments.trim();
            let arguments = match arguments {
                "" | "{}" => String::new(),
                trimmed => format!(" {trimmed}"),
            };
            format!("⚙ {}{}", call.name, arguments)
        })
        .collect::<Vec<_>>()
        .join(" · ");
    match (collapsed.is_empty(), calls.is_empty()) {
        (true, true) => "(empty)".to_string(),
        (true, false) => calls,
        (false, true) => collapsed,
        (false, false) => format!("{collapsed} {calls}"),
    }
}

/// The selected entry in full: tool calls first (they are the structure),
/// then the content, hard-wrapped to the width. An attached image is named
/// rather than dumped — the inline data URL would be megabytes of base64 —
/// so the viewer still says the tool result carries one.
fn message_lines(message: &Message, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(calls) = &message.tool_calls {
        for call in calls {
            let label = format!("⚙ {} {}", call.name, call.arguments.trim());
            for row in wrap_str(label.trim_end(), width) {
                lines.push(Line::from(Span::styled(
                    row,
                    Style::default().fg(theme::ACCENT),
                )));
            }
        }
    }
    for image in &message.images {
        lines.push(Line::from(Span::styled(
            format!(
                "🖼  image attachment · {} · {} · detail {}",
                image.mime,
                crate::plugins::builtin::tools::image::human_bytes(image.payload_bytes()),
                image.detail,
            ),
            Style::default().fg(theme::DIM),
        )));
    }
    if message.content.is_empty() {
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "(empty)",
                Style::default().fg(theme::DIM),
            )));
        }
        return lines;
    }
    for paragraph in message.content.split('\n') {
        if paragraph.is_empty() {
            lines.push(Line::from(Span::raw(String::new())));
            continue;
        }
        for row in wrap_str(paragraph, width.max(1)) {
            lines.push(Line::from(Span::raw(row)));
        }
    }
    lines
}

/// The edit box: a bordered block over the inspector area with the
/// composer's wrapped rows and the terminal cursor placed inside it.
fn editor_panel<'a>(composer: &Composer, width: usize, height: u16) -> Panel<'a> {
    let area = Rect {
        x: 0,
        y: 0,
        width: width as u16,
        height,
    };
    let composer_width = width.saturating_sub(4).max(1);
    let rows = composer.visual_rows(composer_width);
    let (cursor_row, cursor_column) = composer.cursor_visual(composer_width);
    let inner_height = (height.saturating_sub(2) as usize).max(1);
    let offset = cursor_row.saturating_sub(inner_height.saturating_sub(1));
    let visible = rows.iter().skip(offset).take(inner_height);
    let lines: Vec<Line<'static>> = visible
        .map(|row| Line::from(Span::raw(row.clone())))
        .collect();
    let paragraph = Paragraph::new(ratatui::text::Text::from(lines))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::ACCENT)),
        )
        .wrap(ratatui::widgets::Wrap { trim: false });
    // The caret is one cell inside the border, on the row the composer says.
    let column = u16::try_from(cursor_column).unwrap_or(u16::MAX);
    let x = 2u16.saturating_add(column);
    let y = 1u16.saturating_add((cursor_row - offset) as u16);
    Panel::Cursor {
        paragraph,
        position: (
            x.min(area.width.saturating_sub(2)),
            y.min(area.height.saturating_sub(1)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tools::ToolCall;
    use crate::plugins::RequestFacts;
    use crossterm::event::KeyModifiers;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn view_with(messages: Vec<Message>) -> ContextView {
        let mut view = ContextView::new();
        view.apply_snapshot(messages);
        view
    }

    /// A transcript long enough to scroll: the wheel tests need more entries
    /// than a short terminal can show.
    fn long_transcript() -> Vec<Message> {
        let mut messages = vec![Message::system("be brief")];
        for index in 0..30 {
            messages.push(Message::user(format!("message {index}")));
        }
        messages
    }

    #[test]
    fn the_wheel_scrolls_without_moving_the_selection() {
        let mut view = view_with(long_transcript());
        assert_eq!(view.selected, 0);
        // Render once so the screen learns the page height.
        render_rows(&mut view, 60, 16);
        view.wheel(4);
        assert!(view.screen.scroll > 0, "the viewport moved");
        assert_eq!(view.selected, 0, "the selection did not move");
        view.wheel(-99);
        assert_eq!(view.screen.scroll, 0, "never negative");
    }

    #[test]
    fn a_keyboard_step_re_anchors_the_window_on_the_selection() {
        let mut view = view_with(long_transcript());
        render_rows(&mut view, 60, 16);
        view.wheel(6);
        assert!(view.screen.scroll > 0);
        // A step down moves the selection, and the next frame re-anchors the
        // window on it — the scroll offset the wheel set does not survive.
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.selected, 1);
        render_rows(&mut view, 60, 16);
        assert_eq!(
            view.screen.scroll, 1,
            "the window snapped back to the selection instead of staying scrolled"
        );
    }

    fn transcript() -> Vec<Message> {
        vec![
            Message::system("be brief"),
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("run ping"),
            Message::assistant_tool_calls(vec![call("call_1", "ping")]),
            Message::tool_result("call_1", "pong"),
            Message::assistant("done"),
        ]
    }

    // -- delete expansion -------------------------------------------------

    #[test]
    fn expansion_covers_a_whole_tool_round() {
        let messages = transcript();
        // The tool-calls assistant at display index 4 takes its result along.
        assert_eq!(expand_delete_indices(&messages, 4), vec![3, 4]);
        // A plain user entry expands to itself; transcript index = display - 1.
        assert_eq!(expand_delete_indices(&messages, 1), vec![0]);
        // Locked entries expand to nothing at all.
        assert_eq!(expand_delete_indices(&messages, 0), Vec::<usize>::new());
        assert_eq!(expand_delete_indices(&messages, 5), Vec::<usize>::new());
        assert_eq!(expand_delete_indices(&messages, 99), Vec::<usize>::new());
    }

    /// Results for other calls of the same assistant stay in the cascade;
    /// a foreign result (or a non-tool message) ends it.
    #[test]
    fn expansion_stops_at_unrelated_entries() {
        let messages = vec![
            Message::system("be brief"),
            Message::assistant_tool_calls(vec![call("a", "ping"), call("b", "ping")]),
            Message::tool_result("a", "pong"),
            Message::tool_result("b", "pong"),
            Message::user("thanks"),
        ];
        assert_eq!(expand_delete_indices(&messages, 1), vec![0, 1, 2]);

        let interleaved = vec![
            Message::system("be brief"),
            Message::assistant_tool_calls(vec![call("a", "ping")]),
            Message::user("interrupting"),
            Message::tool_result("a", "pong"),
        ];
        assert_eq!(expand_delete_indices(&interleaved, 1), vec![0]);
    }

    // -- key handling ------------------------------------------------------

    #[test]
    fn a_snapshot_arriving_late_populates_the_view() {
        let mut view = ContextView::new();
        assert_eq!(view.entries(), None);
        view.apply_snapshot(transcript());
        assert_eq!(view.entries().map(|entries| entries.len()), Some(7));
    }

    #[test]
    fn navigation_wraps_at_both_ends() {
        let mut view = view_with(transcript());
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.selected, 1);
        // ↑ from the first entry lands on the last, ↓ from the last on the
        // first: a long transcript never traps the reader at an end.
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.selected, 0);
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.selected, 6, "wrapped to the last entry");
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.selected, 0, "wrapped back to the first");
        // A page step cycles by its delta, so on a list shorter than the page
        // it lands somewhere in the middle rather than clamping to the end.
        view.handle_key(key(KeyCode::PageDown));
        assert_eq!(view.selected, 4, "11 steps from 0 wrap through 7 entries");
        view.handle_key(key(KeyCode::Char('G')));
        assert_eq!(view.selected, 6);
        view.handle_key(key(KeyCode::Char('g')));
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn delete_requires_confirmation_and_expands_the_cascade() {
        let mut view = view_with(transcript());
        for _ in 0..4 {
            view.handle_key(key(KeyCode::Down));
        }
        assert_eq!(view.selected, 4, "the tool-calls assistant");

        // n backs out without an action.
        view.handle_key(key(KeyCode::Char('d')));
        assert!(view.confirm_delete);
        assert_eq!(view.handle_key(key(KeyCode::Char('n'))), None);
        assert!(!view.confirm_delete);

        // y submits the whole round (assistant + its result).
        view.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            view.handle_key(key(KeyCode::Char('y'))),
            Some(ContextAction::Delete {
                indices: vec![3, 4]
            })
        );
    }

    #[test]
    fn locked_entries_never_arm_the_delete_confirmation() {
        let mut view = view_with(transcript());
        // System entry.
        view.handle_key(key(KeyCode::Char('d')));
        assert!(!view.confirm_delete);
        assert_eq!(view.handle_key(key(KeyCode::Char('y'))), None);
        // Tool result (5 Downs from 0).
        for _ in 0..5 {
            view.handle_key(key(KeyCode::Down));
        }
        view.handle_key(key(KeyCode::Char('d')));
        assert!(!view.confirm_delete);
        assert_eq!(view.handle_key(key(KeyCode::Char('y'))), None);
    }

    // -- the tool-round drill-down ------------------------------------------

    fn multi_call_transcript() -> Vec<Message> {
        vec![
            Message::system("be brief"),
            Message::assistant_tool_calls(vec![call("a", "ping"), call("b", "grep")]),
            Message::tool_result("a", "pong"),
            Message::tool_result("b", "TODO: fix it"),
        ]
    }

    #[test]
    fn enter_drills_into_a_tool_round_and_esc_climbs_back() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(view.drilled, Some((1, 0)));

        // Plain entries have nothing to drill into.
        let mut plain = view_with(transcript());
        plain.handle_key(key(KeyCode::Down));
        plain.handle_key(key(KeyCode::Enter));
        assert_eq!(plain.drilled, None);

        // Esc (and Left/Backspace) leave the round; the selection is kept.
        assert_eq!(view.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(view.drilled, None);
        assert_eq!(view.selected, 1);
    }

    #[test]
    fn the_drill_moves_the_call_cursor_and_wraps() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.drilled, Some((1, 1)));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.drilled, Some((1, 0)), "wrapped to the first call");
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.drilled, Some((1, 1)), "wrapped to the last call");
        view.handle_key(key(KeyCode::Char('G')));
        assert_eq!(view.drilled, Some((1, 1)));
        view.handle_key(key(KeyCode::Char('g')));
        assert_eq!(view.drilled, Some((1, 0)));
    }

    #[test]
    fn the_drill_delete_submits_one_call_with_its_result() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Down)); // the second call

        // n backs out, like on the message list.
        view.handle_key(key(KeyCode::Char('d')));
        assert!(view.confirm_delete);
        assert_eq!(view.handle_key(key(KeyCode::Char('n'))), None);

        // y splices just this call out: transcript index 0, call `b`.
        view.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            view.handle_key(key(KeyCode::Char('y'))),
            Some(ContextAction::DeleteToolCall {
                index: 0,
                call_id: "b".to_string()
            })
        );
    }

    /// The undo works from inside the drill too — the refreshed snapshot
    /// keeps the drill on the restored round.
    #[test]
    fn the_drill_undo_submits_an_undo_action() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            view.handle_key(key(KeyCode::Char('u'))),
            Some(ContextAction::Undo)
        );
    }

    #[test]
    fn a_snapshot_keeps_the_drill_while_the_round_survives() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Down)); // cursor on the second call

        // A refresh after one call went: the round is still there, the
        // cursor clamps into it.
        let after = vec![
            Message::system("be brief"),
            Message::assistant_tool_calls(vec![call("b", "grep")]),
            Message::tool_result("b", "TODO: fix it"),
        ];
        view.apply_snapshot(after);
        assert_eq!(view.drilled, Some((1, 0)));

        // A round emptied entirely (or replaced by plain text) closes it.
        let emptied = vec![
            Message::system("be brief"),
            Message::assistant("just text now"),
        ];
        view.apply_snapshot(emptied);
        assert_eq!(view.drilled, None);
    }

    #[test]
    fn drilled_rows_show_calls_results_and_hints() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        let rows = render_rows(&mut view, 70, 14);
        let rendered = rows.join("\n");
        assert!(rendered.contains("transcript #0 · 2 calls"), "{rendered}");
        assert!(rendered.contains("⚙ ping"), "{rendered}");
        assert!(rendered.contains("result 4 chars"), "{rendered}");
        assert!(rendered.contains("d delete call"), "{rendered}");

        // The confirm hint names what a delete takes out.
        view.handle_key(key(KeyCode::Char('d')));
        let rows = render_rows(&mut view, 70, 14);
        assert!(
            rows.join("\n").contains("delete call + result? y/n"),
            "{}",
            rows.join("\n")
        );
    }

    #[test]
    fn the_drill_inspector_shows_arguments_and_the_result() {
        let mut view = view_with(multi_call_transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Down)); // the grep call
        let rows = render_rows(&mut view, 60, 14);
        let rendered = rows.join("\n");
        assert!(rendered.contains("⚙ b · grep"), "{rendered}");
        assert!(rendered.contains("─ result"), "{rendered}");
        assert!(rendered.contains("TODO: fix it"), "{rendered}");
    }

    #[test]
    fn editing_types_into_a_box_and_saves_on_enter() {
        let mut view = view_with(transcript());
        view.handle_key(key(KeyCode::Down)); // user "hello"
        view.handle_key(key(KeyCode::Char('e')));
        assert!(view.editing.is_some());

        // Typing reaches the edit box, then Enter saves: transcript index 0.
        view.handle_key(key(KeyCode::Char('!')));
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            Some(ContextAction::Edit {
                index: 0,
                content: "hello!".to_string()
            })
        );
        assert!(view.editing.is_none());
    }

    #[test]
    fn editing_cancels_on_esc_and_ctrl_c() {
        let mut view = view_with(transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char('e')));
        view.handle_key(key(KeyCode::Char('x')));
        assert_eq!(view.handle_key(key(KeyCode::Esc)), None);
        assert!(view.editing.is_none());
        // Nothing was saved: a fresh edit box shows the original text.
        view.handle_key(key(KeyCode::Char('e')));
        let composer = view.editing.as_ref().unwrap();
        assert_eq!(composer.text(), "hello");
        view.handle_key(ctrl(KeyCode::Char('c')));
        assert!(view.editing.is_none());
    }

    #[test]
    fn undo_and_close_come_through_as_actions() {
        let mut view = view_with(transcript());
        assert_eq!(
            view.handle_key(key(KeyCode::Char('u'))),
            Some(ContextAction::Undo)
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Esc)),
            Some(ContextAction::Close)
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Char('q'))),
            Some(ContextAction::Close)
        );
    }

    #[test]
    fn the_view_is_modal_while_open() {
        // Random keys never escape the viewer; they are consumed.
        let mut view = view_with(transcript());
        assert_eq!(view.handle_key(key(KeyCode::Char('x'))), None);
        assert_eq!(view.handle_key(key(KeyCode::Tab)), None);
        // A late snapshot closes any open edit box: the snapshot is newer.
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char('e')));
        view.apply_snapshot(transcript());
        assert!(view.editing.is_none());
        assert!(view.error.is_none());
    }

    #[test]
    fn errors_survive_until_the_next_snapshot() {
        let mut view = view_with(transcript());
        view.set_error("a tool result has no tool-call message before it".to_string());
        assert!(view.error.is_some());
        view.apply_snapshot(transcript());
        assert!(view.error.is_none(), "a fresh snapshot clears the error");
    }

    // -- rendering ---------------------------------------------------------

    /// Render into a `TestBackend` and return the drawn rows.
    fn render_rows(view: &mut ContextView, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|cell| cell.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn rows_show_badges_previews_and_the_lock() {
        let mut view = view_with(transcript());
        // Tall enough for all seven entries plus the inspector panel: the
        // shared frame gives the list what the panel and footer leave.
        let rows = render_rows(&mut view, 60, 18);
        let rendered = rows.join("\n");
        assert!(rendered.contains("Context"), "{rendered}");
        assert!(rendered.contains("7 entries"), "{rendered}");
        assert!(rendered.contains("[system]"), "{rendered}");
        assert!(rendered.contains("·locked"), "{rendered}");
        assert!(rendered.contains("[user]"), "{rendered}");
        assert!(rendered.contains("hello"), "{rendered}");
        assert!(rendered.contains("[assistant]"), "{rendered}");
        assert!(rendered.contains("⚙ ping"), "{rendered}");
        assert!(rendered.contains("[tool]"), "{rendered}");
        assert!(rendered.contains("▸ "), "{rendered}");
        assert!(rendered.contains("u undo"), "{rendered}");
    }

    #[test]
    fn the_inspector_shows_the_full_content_with_an_overflow_marker() {
        let mut view = view_with(transcript());
        view.selected = 1;
        let rows = render_rows(&mut view, 40, 8);
        let rendered = rows.join("\n");
        assert!(rendered.contains("transcript #0"), "{rendered}");
        assert!(rendered.contains("hello"), "{rendered}");

        let long = vec![
            Message::system("be brief"),
            Message::user("word ".repeat(200).trim()),
        ];
        let mut view = view_with(long);
        view.selected = 1;
        let rows = render_rows(&mut view, 30, 8);
        let rendered = rows.join("\n");
        assert!(rendered.contains("+"), "{rendered}");
        assert!(rendered.contains("more lines"), "{rendered}");
    }

    #[test]
    fn editing_renders_a_box_over_the_inspector() {
        let mut view = view_with(transcript());
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char('e')));
        let rows = render_rows(&mut view, 40, 12);
        let rendered = rows.join("\n");
        assert!(rendered.contains("Enter save"), "{rendered}");
        assert!(rendered.contains("hello"), "{rendered}");
    }

    #[test]
    fn an_error_line_shows_in_the_hints_row() {
        let mut view = view_with(transcript());
        view.set_error("rejected: broken tool shape".to_string());
        let rows = render_rows(&mut view, 80, 12);
        let rendered = rows.join("\n");
        assert!(
            rendered.contains("rejected: broken tool shape"),
            "{rendered}"
        );
    }

    // -- the pre-session report -------------------------------------------

    static OVERRIDES: std::sync::LazyLock<Vec<(String, String)>> =
        std::sync::LazyLock::new(|| vec![("temperature".to_string(), "0.2".to_string())]);
    static TEST_TOOLS: std::sync::LazyLock<Vec<crate::core::ToolSpec>> =
        std::sync::LazyLock::new(|| {
            vec![crate::core::ToolSpec {
                name: "ping",
                description: "reply with pong",
                parameters_json: "",
                read_only: true,
            }]
        });
    static ENABLED_PING: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec!["ping".to_string()]);

    fn info<'a>(
        host: &'a crate::plugins::PluginHost,
        sections: &'a [(String, String)],
    ) -> PluginInfo<'a> {
        PluginInfo {
            model: "openai/gpt-test",
            mode: "fullscreen",
            transcript_len: 2,
            context_window: Some(200_000),
            last_usage: Some((8_100, 1_200)),
            last_cached: Some(6_400),
            total_input: 16_200,
            total_output: 2_400,
            total_cached: 12_800,
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
            enabled_tools: &ENABLED_PING,
            message_count: 3,
            plugin_configs: &[],
        }
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
        let host = crate::plugins::PluginHost::new(&[]).expect("valid plugin");
        let block = build_context_report(&info(&host, &sections));

        assert_eq!(block.title, "context");
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
        assert!(
            !request
                .entries
                .iter()
                .any(|entry| entry.label == "body_overrides")
        );

        let tools = group(&block, "Tools");
        assert_eq!(tools.heading, "Tools (1/1 enabled)");
        // Compact rows: the state (and the read-only marker) replaces the
        // long description so the whole set fits on screen.
        assert_eq!(entry(tools, "ping"), "enabled · read-only");

        let usage = group(&block, "Usage");
        assert!(entry(usage, "context").starts_with("9.3k/200.0k"));
        assert_eq!(entry(usage, "tokens"), "session in 16.2k / out 2.4k");
    }

    /// `/tools` narrowing the set is visible in the report: the tool stays
    /// listed, marked disabled, so `/context` explains what the model sees.
    #[test]
    fn report_marks_tools_disabled_by_the_session_switch() {
        static NOTHING_ENABLED: [String; 0] = [];
        let host = crate::plugins::PluginHost::new(&[]).expect("valid plugin");
        let mut narrowed = info(&host, &[]);
        narrowed.enabled_tools = &NOTHING_ENABLED;
        let block = build_context_report(&narrowed);

        let tools = group(&block, "Tools");
        assert_eq!(tools.heading, "Tools (0/1 enabled)");
        assert_eq!(entry(tools, "ping"), "disabled · read-only");
    }

    #[test]
    fn report_handles_empty_route_and_no_tools() {
        let host = crate::plugins::PluginHost::new(&[]).expect("valid plugin");
        let mut bare = info(&host, &[]);
        bare.request = RequestFacts {
            protocol: "",
            base_url: "",
            endpoint: "",
            overrides: &[],
        };
        bare.effort = None;
        bare.tools = &[];
        bare.enabled_tools = &[];
        let block = build_context_report(&bare);

        let prompt = group(&block, "System prompt");
        assert_eq!(prompt.heading, "System prompt");
        assert_eq!(prompt.entries[0].text, "(empty)");

        let request = group(&block, "Request");
        assert_eq!(entry(request, "endpoint"), "(no route configured)");
        assert_eq!(entry(request, "effort"), "not sent");
        assert_eq!(entry(request, "body_overrides"), "none");

        let tools = group(&block, "Tools");
        assert_eq!(tools.heading, "Tools (0/0 enabled)");
        assert_eq!(tools.entries[0].text, "none registered");
    }

    #[test]
    fn long_sections_are_clipped_with_a_marker() {
        let long = "x".repeat(2_000);
        let sections = vec![("base".to_string(), long.clone())];
        let host = crate::plugins::PluginHost::new(&[]).expect("valid plugin");
        let block = build_context_report(&info(&host, &sections));
        let prompt = group(&block, "System prompt");
        let text = entry(prompt, "base");
        assert!(text.contains("…(+1200 chars)"));
        assert!(text.len() < long.len());
    }
}
