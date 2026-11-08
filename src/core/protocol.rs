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
    /// A tool result fed back to the model (`role: "tool"` on the wire).
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// One conversation message. Text in / text out, plus the optional
/// tool-calling fields the chat-completions wire format needs: an assistant
/// message may carry `tool_calls`, and a `Role::Tool` message names the call
/// it answers via `tool_call_id`. Both default to `None` so persisted
/// rollouts written before tools existed still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<crate::core::tools::ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
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

    /// An assistant message that asks for tool calls instead of (or before)
    /// visible text.
    pub fn assistant_tool_calls(tool_calls: Vec<crate::core::tools::ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    /// The result of one tool call, keyed to the model's `tool_call.id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
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
    /// Apply the named profile's presets (`/profile <name>`): its declared
    /// selection and effort, in that order. The profile's assembly-time
    /// values (tools, system prompt) are set at startup and are not touched.
    /// Applied between turns, like `SwitchModel`; unknown names error.
    SwitchProfile { name: String },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_messages_serialize_without_tool_fields() {
        let text = serde_json::to_string(&Message::user("hi")).expect("serialize");
        assert_eq!(text, r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn tool_fields_survive_a_roundtrip() {
        let call = crate::core::tools::ToolCall {
            id: "call_1".to_string(),
            name: "ping".to_string(),
            arguments: "{}".to_string(),
        };
        let assistant = Message::assistant_tool_calls(vec![call.clone()]);
        let text = serde_json::to_string(&assistant).expect("serialize");
        assert!(text.contains(r#""tool_calls""#));
        assert_eq!(
            serde_json::from_str::<Message>(&text).expect("deserialize"),
            assistant
        );

        let result = Message::tool_result("call_1", "pong");
        assert_eq!(
            serde_json::from_str::<Message>(&serde_json::to_string(&result).expect("serialize"))
                .expect("deserialize"),
            result
        );
    }

    /// Rollouts written before tools existed carry neither optional field.
    #[test]
    fn messages_without_tool_fields_still_load() {
        let message: Message =
            serde_json::from_str(r#"{"role":"assistant","content":"hello"}"#).expect("deserialize");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.tool_calls, None);
        assert_eq!(message.tool_call_id, None);
    }
}
