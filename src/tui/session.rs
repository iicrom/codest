//! Per-session view state.
//!
//! Sessions are recorded conversations: `/new` archives the current view and
//! starts a fresh one, and the `/sessions` menu switches back to any recorded
//! session. Only one session is ever active — the model context lives in the
//! core worker (`core::session`), the rendered transcript lives here. Views
//! are append-only and mirror the core's session indexes one to one, so
//! `views[i]` is always the view of conversation `i`.

use crate::core::protocol::Role;
use crate::core::rollout::LoadedSession;
use crate::tui::TuiMode;
use crate::tui::chat::HistoryCell;
use crate::tui::wrap::char_width;

/// Rendered state of one recorded conversation.
pub struct SessionView {
    pub history: Vec<HistoryCell>,
    /// Assistant text currently streaming (not yet committed to history).
    pub live: Option<String>,
    /// Thinking text currently streaming for this turn. Display-only: it is
    /// committed as a [`HistoryCell::Thinking`] when visible content arrives
    /// or the turn ends, and never enters the conversation or the rollout.
    pub live_thinking: Option<String>,
    /// Explicit session name set by `/rename`; when unset the session is
    /// named after its first user message.
    pub name: Option<String>,
    /// When true the transcript sticks to the bottom as new lines arrive.
    pub follow: bool,
    pub scroll: usize,
    pub max_scroll: usize,
    /// Inline mode: how many history cells have been flushed to scrollback.
    pub printed_cells: usize,
    /// Inline mode: how many wrapped lines of `live` already went to scrollback.
    pub live_committed: usize,
    /// Inline mode: final assistant text whose remaining lines still need to be
    /// flushed after a turn ends.
    pub inline_final_text: Option<String>,
    /// Usage of the last completed turn as reported by the backend:
    /// `(input, output)` tokens. The input of a chat-completions request
    /// carries the whole transcript, so this pair doubles as the current
    /// context size. `None` until the first measured turn in this run.
    pub last_usage: Option<(u64, u64)>,
    /// Cumulative measured tokens across the session's completed turns.
    pub total_input: u64,
    pub total_output: u64,
}

impl SessionView {
    /// The first session, which carries the startup banner.
    pub fn initial(model_label: &str, mode: TuiMode) -> Self {
        let mut view = Self::empty();
        view.history.push(HistoryCell::Notice(
            "codest — a minimal TUI agent distilled from codex".to_string(),
        ));
        view.history.push(HistoryCell::Notice(format!(
            "backend: {model_label} · mode: {}",
            mode.label()
        )));
        let hint = match mode {
            TuiMode::Fullscreen => {
                "Enter send · type / for commands · Ctrl+J newline · wheel/PgUp/PgDn scroll · Ctrl+C twice to quit"
            }
            TuiMode::Inline => {
                "Enter send · type / for commands · Ctrl+J newline · PgUp/PgDn terminal scrollback · Ctrl+C twice to quit"
            }
        };
        view.history.push(HistoryCell::Notice(hint.to_string()));
        view
    }

    /// A session created by `/new`.
    pub fn started() -> Self {
        let mut view = Self::empty();
        view.history
            .push(HistoryCell::Notice("New session started".to_string()));
        view
    }

    /// A session restored from a rollout file: transcript cells rebuilt from
    /// the replayed messages, named by the recorded renames. Old lines count
    /// as already printed so inline mode does not dump the whole restored
    /// transcript into scrollback when the view becomes active.
    pub fn restored(loaded: &LoadedSession) -> Self {
        let mut view = Self::empty();
        // Tool-call cells are keyed by the model's call ids so the follow-up
        // `Role::Tool` messages attach their output to the right cell.
        let mut call_cells = std::collections::HashMap::new();
        for message in &loaded.messages {
            match message.role {
                Role::User => view
                    .history
                    .push(HistoryCell::User(message.content.clone())),
                Role::Assistant => {
                    if !message.content.is_empty() {
                        view.history
                            .push(HistoryCell::Assistant(message.content.clone()));
                    }
                    for call in message.tool_calls.iter().flatten() {
                        view.history.push(HistoryCell::ToolCall {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            result: String::new(),
                            error: None,
                        });
                        call_cells.insert(call.id.clone(), view.history.len() - 1);
                    }
                }
                Role::Tool => {
                    if let Some(id) = &message.tool_call_id
                        && let Some(&index) = call_cells.get(id)
                        && let Some(HistoryCell::ToolCall { result, .. }) =
                            view.history.get_mut(index)
                    {
                        *result = message.content.clone();
                    }
                }
                Role::System => {}
            }
        }
        view.name = loaded.name.clone().or(loaded.meta.name.clone());
        // The rollout's recorded usage seeds the footer immediately: a
        // resumed session shows its restored context size before the first
        // new turn measures it again.
        view.last_usage = loaded.last_usage;
        view.total_input = loaded.total_input;
        view.total_output = loaded.total_output;
        view.printed_cells = view.history.len();
        view
    }

    fn empty() -> Self {
        Self {
            history: Vec::new(),
            live: None,
            live_thinking: None,
            name: None,
            follow: true,
            scroll: 0,
            max_scroll: 0,
            printed_cells: 0,
            live_committed: 0,
            inline_final_text: None,
            last_usage: None,
            total_input: 0,
            total_output: 0,
        }
    }

    /// The first line of this session's first user message — the default
    /// session name.
    pub fn preview(&self) -> Option<&str> {
        self.history.iter().find_map(|cell| match cell {
            HistoryCell::User(text) => Some(text.lines().next().unwrap_or(text)),
            _ => None,
        })
    }

    /// The name shown in the session menu: an explicit `/rename` name wins
    /// over the default (the first user message). `None` means the caller
    /// should fall back to the session's index.
    pub fn display_name(&self) -> Option<&str> {
        self.name.as_deref().or(self.preview())
    }
}

/// Short preview text cut off at `max` columns with an ellipsis.
pub fn truncated(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = char_width(ch);
        if width + ch_width > max.saturating_sub(1) {
            out.push('…');
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_view_carries_the_startup_banner() {
        let view = SessionView::initial("test-model", TuiMode::Fullscreen);
        assert_eq!(view.history.len(), 3);
        assert!(
            matches!(&view.history[0], HistoryCell::Notice(text) if text.contains("codest")),
            "{:?}",
            view.history[0]
        );
        assert!(
            matches!(&view.history[1], HistoryCell::Notice(text) if text.contains("test-model")),
            "{:?}",
            view.history[1]
        );
        assert!(view.follow);
        assert_eq!(view.preview(), None);
    }

    #[test]
    fn started_view_carries_a_notice() {
        let view = SessionView::started();
        assert!(
            matches!(&view.history[0], HistoryCell::Notice(text) if text == "New session started")
        );
    }

    #[test]
    fn preview_returns_the_first_user_line() {
        let mut view = SessionView::empty();
        view.history.push(HistoryCell::Notice("nope".to_string()));
        assert_eq!(view.preview(), None);
        view.history
            .push(HistoryCell::User("hello\nworld".to_string()));
        assert_eq!(view.preview(), Some("hello"));
    }

    #[test]
    fn display_name_prefers_rename_over_first_user_message() {
        let mut view = SessionView::empty();
        assert_eq!(view.display_name(), None);
        view.history
            .push(HistoryCell::User("hello there".to_string()));
        assert_eq!(view.display_name(), Some("hello there"));
        view.name = Some("custom".to_string());
        assert_eq!(view.display_name(), Some("custom"));
    }

    #[test]
    fn truncated_cuts_at_the_column_limit() {
        assert_eq!(truncated("hello", 10), "hello");
        assert_eq!(truncated("hello world", 8), "hello w…");
        // Wide characters count as two columns, so only two fit at width 6.
        assert_eq!(truncated("你好世界", 6), "你好…");
        assert_eq!(truncated("好", 6), "好");
    }
}
