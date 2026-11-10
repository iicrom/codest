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
    },
    /// Token usage for a completed turn, right before its `TurnComplete`.
    TurnUsage { input: u64, output: u64 },
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
}

impl Event {
    pub fn new(id: impl Into<String>, msg: EventMsg) -> Self {
        Self { id: id.into(), msg }
    }
}

