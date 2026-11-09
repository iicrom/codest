//! The conversation message vocabulary.
//!
//! Split out of the host's submission/event protocol because a model client
//! only ever needs this half: a role and its content, plus the optional
//! tool-calling fields the chat-completions wire format requires.

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
    pub tool_calls: Option<Vec<crate::tools::ToolCall>>,
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
    pub fn assistant_tool_calls(tool_calls: Vec<crate::tools::ToolCall>) -> Self {
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
        let call = crate::tools::ToolCall {
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
