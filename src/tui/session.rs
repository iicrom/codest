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
use crate::tui::chat::{CardDisplay, HistoryCell};
use crate::tui::wrap::char_width;
use ratatui::text::Line;
use std::time::Instant;

/// A tool call that is executing right now, rendered as a live card (spinner
/// header and an elapsed row) in the transcript tail until the matching
/// completion event turns it into a settled [`HistoryCell::ToolCall`].
/// Tools run sequentially, so at most one is ever live.
#[derive(Debug, Clone)]
pub struct RunningTool {
    pub name: String,
    pub arguments: String,
    /// When the call started, for the live elapsed readout.
    pub started_at: Instant,
}

/// What a cached cell render depends on. A settled cell is immutable — it
/// never changes once pushed — so the only inputs that can change its lines
/// are the viewport width and the two fold flags. codex keys its
/// active-cell layout cache on (identity, revision, width, render mode,
/// theme); the identity and revision halves are unnecessary here because
/// cached cells are only replaced on a key mismatch, never merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellRenderKey {
    width: usize,
    display: CardDisplay,
}

/// A cached render of one history cell.
struct RenderedCell {
    key: CellRenderKey,
    lines: Vec<Line<'static>>,
}

/// Rendered state of one recorded conversation.
pub struct SessionView {
    pub history: Vec<HistoryCell>,
    /// Cached renders parallel to `history`, filled lazily by
    /// [`Self::cell_lines`]. The transcript used to re-render every cell on
    /// every frame — for a settled transcript that is thousands of wasted
    /// markdown, JSON and wrap rebuilds per keystroke. Settled lines are now
    /// reused until the width or the fold flags change. The slot count is
    /// re-synced with `history` on every access: pushed cells have no cache
    /// yet, and a cleared or spliced history truncates stale entries, so no
    /// explicit invalidation is required for either.
    rendered: Vec<Option<RenderedCell>>,
    /// Assistant text currently streaming (not yet committed to history).
    pub live: Option<String>,
    /// Thinking text currently streaming for this turn. Display-only: it is
    /// committed as a [`HistoryCell::Thinking`] when visible content arrives
    /// or the turn ends, and never enters the conversation or the rollout.
    pub live_thinking: Option<String>,
    /// The tool call currently executing (its live card renders in the
    /// transcript tail). `None` between calls; settled cards live in
    /// `history`.
    pub running_tool: Option<RunningTool>,
    /// Explicit session name set by `/rename`; when unset the session is
    /// named after its first user message.
    pub name: Option<String>,
    /// When true the transcript sticks to the bottom as new lines arrive.
    pub follow: bool,
    pub scroll: usize,
    pub max_scroll: usize,
    /// Set when a report was just appended: the next render parks the
    /// viewport on the report's first line, so a long `/context` dump is read
    /// from its top instead of its tail. The row count is only known once the
    /// cell renders at the current width, hence the deferral.
    pub park_on_last_report: bool,
    /// Inline mode: how many history cells have been flushed to scrollback.
    pub printed_cells: usize,
    /// Inline mode: how many wrapped lines of `live` already went to scrollback.
    pub live_committed: usize,
    /// Inline mode: final assistant text whose remaining lines still need to be
    /// flushed after a turn ends.
    pub inline_final_text: Option<String>,
    /// The most recent usage the backend reported, as `(input, output)`
    /// tokens. The input of a chat-completions request carries the whole
    /// transcript, so this pair doubles as the current context size. It moves
    /// with every round that reports usage (`ContextUsage`, so the footer
    /// follows a running turn) and is settled by the completed turn's
    /// `TurnUsage`. `None` until this run measures anything.
    pub last_usage: Option<(u64, u64)>,
    /// Cumulative measured tokens across the session's completed turns.
    pub total_input: u64,
    pub total_output: u64,
}

impl SessionView {
    /// Keep the cache slots aligned with `history`: pushes leave holes, a
    /// clear or splice truncates whatever no longer has a cell.
    fn sync_rendered(&mut self) {
        self.rendered.resize_with(self.history.len(), || None);
    }

    /// The lines of cell `index` at `width` under `display`, rebuilt only
    /// when the cached width or fold flags no longer match. Settled cells
    /// are immutable, so a hit is always current content.
    pub fn cell_lines(
        &mut self,
        index: usize,
        width: usize,
        display: &CardDisplay,
    ) -> &[Line<'static>] {
        self.sync_rendered();
        let key = CellRenderKey {
            width,
            display: *display,
        };
        if !matches!(&self.rendered[index], Some(rendered) if rendered.key == key) {
            let lines = self.history[index].lines(width, display);
            self.rendered[index] = Some(RenderedCell { key, lines });
        }
        self.rendered[index]
            .as_ref()
            .expect("render just stored or cached")
            .lines
            .as_slice()
    }

    /// Drop the cached render of one cell. Used by inline mode once a cell's
    /// lines went to scrollback: they can never render again, so holding
    /// them would only grow memory with the session.
    pub fn drop_cell_render(&mut self, index: usize) {
        if index < self.rendered.len() {
            self.rendered[index] = None;
        }
    }

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
        // Reasoning blocks carry the index of the message they preceded; they
        // render as thinking cards just above it, the way they streamed.
        let mut thinking = loaded.thinking.iter().peekable();
        for (position, message) in loaded.messages.iter().enumerate() {
            while let Some(record) = thinking.next_if(|record| record.before <= position) {
                view.history.push(HistoryCell::Thinking {
                    text: record.text.clone(),
                    duration_ms: record.duration_ms,
                });
            }
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
                            // Rollouts record messages, not timings; a
                            // restored card simply carries no duration chip.
                            duration_ms: None,
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
        // A reasoning block can outlive its message (a transcript edited down
        // to nothing after it): keep it as a trailing card rather than losing
        // it silently.
        for record in thinking {
            view.history.push(HistoryCell::Thinking {
                text: record.text.clone(),
                duration_ms: record.duration_ms,
            });
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
            rendered: Vec::new(),
            live: None,
            live_thinking: None,
            running_tool: None,
            name: None,
            follow: true,
            scroll: 0,
            max_scroll: 0,
            park_on_last_report: false,
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
    fn restored_view_puts_thinking_cards_back_above_their_answers() {
        let loaded = LoadedSession {
            meta: crate::core::SessionMeta {
                version: 1,
                id: "s".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                cwd: "/tmp/demo".to_string(),
                system_prompt: "be brief".to_string(),
                name: None,
                provider: None,
                model: None,
                profile: None,
            },
            path: std::path::PathBuf::from("/tmp/s.jsonl"),
            effort: None,
            messages: vec![
                crate::core::protocol::Message::user("q"),
                crate::core::protocol::Message::assistant("a"),
                crate::core::protocol::Message::user("q2"),
                crate::core::protocol::Message::assistant("a2"),
            ],
            // One block before the first answer, one before the second.
            thinking: vec![
                crate::core::rollout::ThinkingRecord {
                    before: 1,
                    text: "pondering q".to_string(),
                    duration_ms: Some(4_200),
                },
                crate::core::rollout::ThinkingRecord {
                    before: 3,
                    text: "pondering q2".to_string(),
                    duration_ms: None,
                },
            ],
            undo: Vec::new(),
            name: None,
            model: None,
            last_usage: None,
            total_input: 0,
            total_output: 0,
            profile: None,
            tools: None,
        };

        let view = SessionView::restored(&loaded);
        let cells: Vec<&str> = view
            .history
            .iter()
            .map(|cell| match cell {
                HistoryCell::User(_) => "user",
                HistoryCell::Assistant(_) => "assistant",
                HistoryCell::Thinking { .. } => "thinking",
                _ => "other",
            })
            .collect();
        assert_eq!(
            cells,
            vec![
                "user",
                "thinking",
                "assistant",
                "user",
                "thinking",
                "assistant",
            ]
        );
        assert!(matches!(
            &view.history[1],
            HistoryCell::Thinking { text, duration_ms: Some(4_200) } if text == "pondering q"
        ));
        // The restored cards count as already printed, like the rest.
        assert_eq!(view.printed_cells, view.history.len());
    }

    #[test]
    fn restored_thinking_without_a_message_follows_the_transcript() {
        let mut loaded = LoadedSession {
            meta: crate::core::SessionMeta {
                version: 1,
                id: "s".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                cwd: "/tmp/demo".to_string(),
                system_prompt: "be brief".to_string(),
                name: None,
                provider: None,
                model: None,
                profile: None,
            },
            path: std::path::PathBuf::from("/tmp/s.jsonl"),
            effort: None,
            messages: vec![crate::core::protocol::Message::user("q")],
            thinking: vec![crate::core::rollout::ThinkingRecord {
                before: 5,
                text: "late".to_string(),
                duration_ms: None,
            }],
            undo: Vec::new(),
            name: None,
            model: None,
            last_usage: None,
            total_input: 0,
            total_output: 0,
            profile: None,
            tools: None,
        };
        loaded
            .messages
            .push(crate::core::protocol::Message::user("q2"));

        // The block anchors past the last message it could precede: it is
        // rendered as a trailing card rather than dropped.
        let view = SessionView::restored(&loaded);
        assert!(matches!(
            view.history.last(),
            Some(HistoryCell::Thinking { text, .. }) if text == "late"
        ));
        assert_eq!(view.history.len(), 3);
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
