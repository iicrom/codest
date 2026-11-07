//! Submission Queue / Event Queue protocol.
//!
//! Mirrors the shape of `codex_protocol::protocol`: callers submit an [`Op`]
//! and receive a stream of [`Event`]s. Keeping the bus explicit means the TUI
//! never calls into model code directly and can be driven by a mock core later.

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
}

/// Events emitted by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventMsg {
    /// A turn was accepted and the model request is starting.
    TurnStarted,
    /// Incremental assistant text.
    AgentMessageDelta { delta: String },
    /// The complete assistant text for the turn. Sent once, after streaming.
    AgentMessage { message: String },
    /// The turn was cancelled by the user.
    TurnAborted { reason: String },
    /// The turn hit an error.
    Error { message: String },
    /// The turn is over; the front-end may accept the next submission.
    TurnComplete,
    /// The conversation history was dropped (system prompt retained).
    ConversationCleared,
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
