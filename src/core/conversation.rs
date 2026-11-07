//! Conversation history.
//!
//! The minimal core keeps one linear transcript. Codex has compaction,
//! rollouts, token accounting, and tool items; none of that is needed for a
//! first-pass chat agent, so the history is intentionally just a `Vec<Message>`.

use crate::core::protocol::Message;

pub const DEFAULT_SYSTEM_PROMPT: &str =
    "You are codest, a concise and helpful terminal assistant. Answer in the user's language.";

#[derive(Debug, Clone)]
pub struct Conversation {
    system: Message,
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system: Message::system(system_prompt),
            messages: Vec::new(),
        }
    }

    /// A snapshot suitable for a model request: system prompt first, then the
    /// full transcript.
    pub fn snapshot(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        out.push(self.system.clone());
        out.extend(self.messages.iter().cloned());
        out
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.messages.push(Message::assistant(text));
    }

    /// Drop every message, keeping the system prompt.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_puts_system_first() {
        let mut conv = Conversation::new("be brief");
        conv.push_user("hi");
        conv.push_assistant("hello");

        let snapshot = conv.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].content, "be brief");
        assert_eq!(snapshot[1].content, "hi");
        assert_eq!(snapshot[2].content, "hello");
    }

    #[test]
    fn clear_drops_messages_but_keeps_system_prompt() {
        let mut conv = Conversation::new("be brief");
        conv.push_user("hi");
        conv.push_assistant("hello");

        conv.clear();

        assert!(conv.is_empty());
        assert_eq!(conv.len(), 0);
        let snapshot = conv.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].content, "be brief");
    }
}
