//! Submission Queue / Event Queue protocol.
//!
//! Mirrors the shape of `codex_protocol::protocol`: callers submit an [`Op`]
//! and receive a stream of [`Event`]s. Keeping the bus explicit means the TUI
//! never calls into model code directly and can be driven by a test core later.
//!
//! The message vocabulary ([`Role`], [`Message`]) lives in the contract crate
//! and is re-exported here: a model client needs it, and a client must not
//! have to depend on the bus that drives it.

pub use cst_plugin_api::message::Message;
pub use cst_plugin_api::message::Role;

/// Commands accepted by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Start one turn with a new user message.
    UserTurn { text: String },
    /// Continue the active conversation without appending a user message:
    /// the model is driven from the transcript as it stands. The recovery
    /// path for an interrupted turn — a crash left a dangling user message,
    /// a tool round without its results, or an abort cut the last answer
    /// short. Answered like `UserTurn` (`TurnStarted` … `TurnComplete`), or
    /// an `Error` plus `TurnComplete` when there is nothing to continue.
    ContinueTurn,
    /// Drop the transcript, keeping the system prompt.
    ClearConversation,
    /// Cancel the in-flight turn, keeping any text streamed so far.
    Interrupt,
    /// Stop the agent worker.
    Shutdown,
    /// Archive the active conversation and start a fresh session. Sessions are
    /// indexed by creation order and are never removed.
    NewSession,
    /// Make the session at `session` (creation-order index) active again.
    SwitchSession { session: usize },
    /// Name the session at `session` (see `/rename`); recorded in its rollout.
    RenameSession { session: usize, name: String },
    /// Make `selection` (`"provider/model"`, see `/model`) the backend for the
    /// following turns. Applied between turns; the current one is untouched.
    SwitchModel { selection: String },
    /// Set the reasoning effort sent with the following turns (`/effort`);
    /// `None` clears the override so no effort field is sent at all. Applied
    /// between turns, like `SwitchModel`.
    SetReasoningEffort { effort: Option<String> },
    /// Apply the named profile's presets (`/profile <name>`): its declared
    /// selection and effort, in that order. The profile's assembly-time
    /// values (tools, system prompt) are set at startup and are not touched.
    /// Applied between turns, like `SwitchModel`; unknown names error.
    SwitchProfile { name: String },
    /// Replace the active session's enabled-tool set (`/tools`): the whole
    /// set, not a delta, so one op carries a menu's multi-toggle result.
    /// Enabled tools are declared to the model on the following turns and
    /// are the only ones the executor will run; disabled ones are withheld
    /// from the declaration and rejected at execution time. Unknown names
    /// are ignored (a replayed set may name tools this run never declared).
    /// Applied between turns, like `SwitchModel`.
    SetToolsEnabled { enabled: Vec<String> },
    /// Return the active conversation as [`EventMsg::ConversationSnapshot`]
    /// (the `/context` viewer's read path — the worker is the only owner of
    /// the transcript, so the front-end asks rather than mirrors it).
    InspectConversation,
    /// Remove the transcript messages at `indices` (transcript positions —
    /// the system prompt is not addressable). The front-end expands tool-call
    /// rounds up front, so one op carries the whole cascade; the worker still
    /// validates the result satisfies the tool_calls↔tool wire constraints
    /// and rejects the op otherwise. Answered with a
    /// `ConversationSnapshot`, or an `Error`.
    DeleteMessages { indices: Vec<usize> },
    /// Replace the content of the transcript message at `index`. Roles and
    /// tool fields are untouched, so edits can never break the wire shape.
    /// Answered with a `ConversationSnapshot`, or an `Error`.
    EditMessage { index: usize, content: String },
    /// Undo the most recent delete/edit (`/context` checkpoints). Each
    /// mutation records its inverse; this pops and applies it. Answered with
    /// a `ConversationSnapshot`, or an `Error` when nothing is reversible.
    UndoContextEdit,
}

/// Events emitted by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventMsg {
    /// A turn was accepted and the model request is starting.
    TurnStarted,
    /// Incremental assistant text.
    AgentMessageDelta { delta: String },
    /// Incremental reasoning content, rendered dim (and foldable via
    /// `/thinking`) by the front-end.
    AgentThinkingDelta { delta: String },
    /// The complete assistant text for the turn. Sent once, after streaming —
    /// and once per tool round that streamed text before its calls.
    AgentMessage { message: String },
    /// A tool call the model asked for is about to execute. The front-end
    /// renders a live "running" card that [`EventMsg::AgentToolCall`]
    /// settles.
    AgentToolCallStarted { name: String, arguments: String },
    /// One tool call the model asked for has been executed (or failed).
    /// Display-only: tool activity never surfaces as agent text.
    AgentToolCall {
        name: String,
        arguments: String,
        /// The tool's output on success; empty on failure.
        result: String,
        /// Why the call failed, when it did. The model receives the failure
        /// as the tool message content and may react to it.
        error: Option<String>,
        /// Wall-clock time the call took, in milliseconds. A disabled or
        /// unknown tool completes instantly, so small values are honest.
        duration_ms: u64,
    },
    /// Token usage as the backend reported it for the round that just
    /// finished streaming, sent as soon as it arrives rather than at turn
    /// end. Display-only: the front-end updates its context readout from it,
    /// while [`EventMsg::TurnUsage`] stays the single record of a completed
    /// turn (accumulated in `/status`, persisted in the rollout). `cached`
    /// mirrors the round's `StreamItem::Usage`: the prompt-prefix subset of
    /// `input`, `None` when the backend reports no cache accounting.
    ContextUsage {
        input: u64,
        output: u64,
        cached: Option<u64>,
    },
    /// Token usage for a completed turn, right before its `TurnComplete`.
    /// `cached` and `model` carry what the rollout's `TurnUsage` record
    /// persists, so the front-end's per-turn attribution matches the file.
    TurnUsage {
        input: u64,
        output: u64,
        cached: Option<u64>,
        model: Option<String>,
    },
    /// The turn was cancelled by the user.
    TurnAborted { reason: String },
    /// The turn hit an error.
    Error { message: String },
    /// The turn is over; the front-end may accept the next submission.
    TurnComplete,
    /// The conversation history was dropped (system prompt retained).
    ConversationCleared,
    /// A fresh session was created by `Op::NewSession` and is now active.
    SessionStarted { session: usize },
    /// The active session was switched to the given creation-order index.
    SessionSwitched { session: usize },
    /// The session at `session` was named via `Op::RenameSession`.
    SessionRenamed { session: usize, name: String },
    /// The active model is now `model` (`"provider/model"`, see
    /// `Op::SwitchModel`).
    ModelSwitched { model: String },
    /// The reasoning effort for the following turns is now `effort`
    /// (`None` = no effort field is sent; see `Op::SetReasoningEffort`).
    ReasoningEffortSwitched { effort: Option<String> },
    /// The named profile is now active (`Op::SwitchProfile`). Its selection
    /// and effort changes ride along as their own events.
    ProfileSwitched { name: String },
    /// The enabled-tool set for the active session is now `enabled`
    /// (`Op::SetToolsEnabled`, `/tools`). Also emitted after a session
    /// switch or creation, because the set is per session and the
    /// front-end's mirror needs to resync.
    ToolsSwitched { enabled: Vec<String> },
    /// The active conversation, as asked by `Op::InspectConversation` and
    /// re-sent as the confirmation of every context mutation
    /// (`DeleteMessages` / `EditMessage` / `UndoContextEdit`) so the
    /// `/context` viewer refreshes atomically from the worker's own state.
    /// System prompt first, then the transcript.
    ConversationSnapshot { messages: Vec<Message> },
    /// The worker has stopped.
    ShutdownComplete,
}

/// A submission queued for the agent worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub id: String,
    pub op: Op,
}

/// An event emitted by the agent worker. `id` identifies the submission that
/// caused it (turn events share the id of their `UserTurn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub msg: EventMsg,
    /// The worker's monotonic emit time, stamped once here in `Event::new`
    /// so every emit site gets a consistent clock without touching them.
    /// Forwarded to plugins as `PluginEvent::at`; a throughput meter or a
    /// monitor's batching derives its timing from it.
    pub at: std::time::Instant,
}

impl Event {
    pub fn new(id: impl Into<String>, msg: EventMsg) -> Self {
        Self {
            id: id.into(),
            msg,
            at: std::time::Instant::now(),
        }
    }
}
