//! Submission Queue / Event Queue protocol.
//!
//! Mirrors the shape of `codex_protocol::protocol`: callers submit an [`Op`]
//! and receive a stream of [`Event`]s. Keeping the bus explicit means the TUI
//! never calls into model code directly and can be driven by a test core later.

use serde::Deserialize;
use serde::Serialize;

/// Who authored a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One conversation message. This is the only model-visible payload in the
/// minimal agent: text in, text out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
}

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
    /// The complete assistant text for the turn. Sent once, after streaming.
    AgentMessage { message: String },
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
