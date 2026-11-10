//! Conversation history.
//!
//! The minimal core keeps one linear transcript. Codex has compaction,
//! rollouts, token accounting, and tool items; none of that is needed for a
//! first-pass chat agent, so the history is intentionally just a `Vec<Message>`.
//!
//! The `/context` viewer adds surgical primitives on top: deleting and
//! editing transcript messages, each of which records a checkpoint (the
//! inverse operation) so `undo_last_edit` can restore it. The checkpoints are
//! plain data — the rollout replays them back into a live stack after a
//! restart, so an undo never expires with the process.

use std::collections::BTreeSet;

use crate::core::protocol::Message;
use crate::core::protocol::Role;

pub const DEFAULT_SYSTEM_PROMPT: &str =
    "You are codest, a concise and helpful terminal assistant. Answer in the user's language.";

/// One reversible context modification: the inverse of a `delete_at` or
/// `edit_content`, recorded automatically so [`Conversation::undo_last_edit`]
/// can restore it. Entries are LIFO — each one was computed against the
/// transcript state at its time, and only undos rewind that state, so the
/// stored positions stay valid exactly while the entry is on the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoEntry {
    /// Messages removed by one delete, as `(transcript index, message)`
    /// pairs in ascending index order. Re-inserting the pairs in order
    /// restores the exact prior transcript.
    Deleted { removed: Vec<(usize, Message)> },
    /// The message at `index` carried `previous` before the edit.
    Edited { index: usize, previous: String },
}

#[derive(Debug, Clone)]
pub struct Conversation {
    system: Message,
    messages: Vec<Message>,
    /// Checkpoints of the `/context` edits, newest last.
    undo: Vec<UndoEntry>,
}

impl Conversation {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system: Message::system(system_prompt),
            messages: Vec::new(),
            undo: Vec::new(),
        }
    }

    /// A conversation rebuilt from a replayed rollout: the transcript, plus
    /// the checkpoint stack the `ContextDeleted`/`ContextEdited`/`
    /// `ContextRestored` markers reconstruct. Messages with the system role
    /// are ignored, like [`Conversation::push_message`].
    pub fn replayed(
        system_prompt: impl Into<String>,
        messages: Vec<Message>,
        undo: Vec<UndoEntry>,
    ) -> Self {
        let mut conversation = Self::new(system_prompt);
        for message in messages {
            conversation.push_message(message);
        }
        conversation.undo = undo;
        conversation
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

    /// Append a pre-built message, keeping the system prompt untouched.
    /// Used when replaying a restored session's rollout.
    pub fn push_message(&mut self, message: Message) {
        match message.role {
            Role::System => {}
            Role::User | Role::Assistant | Role::Tool => self.messages.push(message),
        }
    }

    /// Drop every message, keeping the system prompt. The checkpoints go with
    /// it: a `/clear` is itself not reversible, and restoring entries into a
    /// cleared transcript would surprise.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.undo.clear();
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

    /// How many checkpoints `undo_last_edit` can still pop.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// Remove the transcript messages at `indices` (any order; the system
    /// prompt is not addressable). Records one checkpoint covering the whole
    /// set. Fails without side effects when any index is out of range, so a
    /// stale viewer cannot corrupt the transcript.
    ///
    /// Returns the removed `(index, message)` pairs in ascending order —
    /// what the rollout marker and the checkpoint both need.
    pub fn delete_at(&mut self, indices: &[usize]) -> Result<Vec<(usize, Message)>, String> {
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        if let Some(last) = sorted.last().filter(|last| **last >= self.messages.len()) {
            return Err(format!(
                "cannot delete message {last}: the transcript has {} messages",
                self.messages.len()
            ));
        }
        // Remove back-to-front so the not-yet-removed indices stay valid,
        // then flip the pairs into ascending order for storage/replay.
        let mut removed: Vec<(usize, Message)> = sorted
            .iter()
            .rev()
            .map(|&index| (index, self.messages.remove(index)))
            .collect();
        removed.reverse();
        self.undo.push(UndoEntry::Deleted {
            removed: removed.clone(),
        });
        Ok(removed)
    }

    /// Replace the content of the transcript message at `index`, recording
    /// the old text as a checkpoint. Roles and tool fields are untouched.
    /// Returns the replaced content, for the rollout marker.
    pub fn edit_content(
        &mut self,
        index: usize,
        content: impl Into<String>,
    ) -> Result<String, String> {
        if index >= self.messages.len() {
            return Err(format!(
                "cannot edit message {index}: the transcript has {} messages",
                self.messages.len()
            ));
        }
        let message = &mut self.messages[index];
        let previous = std::mem::take(&mut message.content);
        message.content = content.into();
        self.undo
            .push(UndoEntry::Edited { index, previous: previous.clone() });
        Ok(previous)
    }

    /// Undo the most recent delete or edit. `false` when nothing recorded.
    pub fn undo_last_edit(&mut self) -> bool {
        match self.undo.pop() {
            Some(UndoEntry::Deleted { removed }) => {
                // Ascending insertion: each index counts original-order
                // predecessors, which are all back in place by the time it
                // is inserted.
                for (index, message) in removed {
                    self.messages.insert(index, message);
                }
                true
            }
            Some(UndoEntry::Edited { index, previous }) => match self.messages.get_mut(index) {
                Some(message) => {
                    message.content = previous;
                    true
                }
                // Unreachable for a healthy stack (LIFO keeps positions
                // valid); treat as spent rather than panic.
                None => false,
            },
            None => false,
        }
    }

    /// Whether the tool_calls ↔ tool-result wire shape holds for `messages`:
    /// every tool result must sit directly after the assistant message whose
    /// `tool_calls` include its `tool_call_id`, and every call must be
    /// answered exactly once. The worker runs this on a delete's result —
    /// the chat-completions endpoint rejects conversations that break it.
    pub fn validate_tool_structure(messages: &[Message]) -> Result<(), String> {
        let mut index = 0;
        while index < messages.len() {
            let message = &messages[index];
            match message.role {
                Role::Tool => {
                    return Err("a tool result has no tool-call message before it".to_string());
                }
                Role::Assistant if message.tool_calls.is_some() => {
                    let calls = message.tool_calls.as_deref().unwrap_or(&[]);
                    let mut pending: BTreeSet<&str> =
                        calls.iter().map(|call| call.id.as_str()).collect();
                    let mut cursor = index + 1;
                    while cursor < messages.len() && messages[cursor].role == Role::Tool {
                        let id = messages[cursor].tool_call_id.as_deref().unwrap_or("");
                        if !pending.remove(id) {
                            return Err(format!(
                                "a tool result answers unknown or completed call `{id}`"
                            ));
                        }
                        cursor += 1;
                    }
                    if let Some(id) = pending.iter().next() {
                        return Err(format!("tool call `{id}` has no result message"));
                    }
                    index = cursor;
                }
                _ => index += 1,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tools::ToolCall;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "ping".to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn transcript() -> Conversation {
        let mut conversation = Conversation::new("be brief");
        conversation.push_user("hi");
        conversation.push_assistant("hello");
        conversation
    }

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

    #[test]
    fn delete_removes_messages_and_records_a_checkpoint() {
        let mut conv = transcript();
        let removed = conv.delete_at(&[1]).expect("in range");

        assert_eq!(removed, vec![(1, Message::assistant("hello"))]);
        assert_eq!(conv.messages(), &[Message::user("hi")]);
        assert_eq!(conv.undo_depth(), 1);
    }

    #[test]
    fn delete_accepts_unordered_indices_and_reports_ascending() {
        let mut conv = Conversation::new("be brief");
        conv.push_user("1");
        conv.push_user("2");
        conv.push_user("3");
        conv.push_user("4");

        let removed = conv.delete_at(&[3, 0, 2]).expect("in range");
        assert_eq!(
            removed.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
        assert_eq!(conv.messages().len(), 1);
    }

    #[test]
    fn delete_out_of_range_fails_without_side_effects() {
        let mut conv = transcript();
        assert!(conv.delete_at(&[2]).is_err());
        assert_eq!(conv.len(), 2);
        assert_eq!(conv.undo_depth(), 0);
    }

    #[test]
    fn edit_replaces_content_and_records_the_previous_text() {
        let mut conv = transcript();
        let previous = conv.edit_content(0, "hello instead").expect("in range");

        assert_eq!(previous, "hi");
        assert_eq!(conv.messages()[0].content, "hello instead");
        assert_eq!(conv.messages()[0].role, Role::User);
        assert_eq!(conv.undo_depth(), 1);
    }

    #[test]
    fn undo_restores_deletes_and_edits_in_lifo_order() {
        let mut conv = transcript();
        conv.edit_content(1, "edited").expect("in range");
        conv.delete_at(&[0]).expect("in range");
        assert_eq!(conv.messages(), &[Message::assistant("edited")]);

        // Undo the delete first: the user message returns to index 0.
        assert!(conv.undo_last_edit());
        assert_eq!(
            conv.messages(),
            &[Message::user("hi"), Message::assistant("edited")]
        );

        // Then the edit: the assistant text reverts.
        assert!(conv.undo_last_edit());
        assert_eq!(
            conv.messages(),
            &[Message::user("hi"), Message::assistant("hello")]
        );
        assert!(!conv.undo_last_edit());
    }

    #[test]
    fn undo_of_a_multi_message_delete_reinserts_exactly() {
        let mut conv = Conversation::new("be brief");
        conv.push_user("1");
        conv.push_user("2");
        conv.push_user("3");
        conv.push_user("4");
        conv.delete_at(&[3, 0]).expect("in range");
        assert_eq!(conv.messages().len(), 2);

        assert!(conv.undo_last_edit());
        assert_eq!(
            conv.messages()
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "3", "4"]
        );
    }

    #[test]
    fn clear_also_drops_the_checkpoints() {
        let mut conv = transcript();
        conv.delete_at(&[0]).expect("in range");
        conv.clear();
        assert!(!conv.undo_last_edit(), "no checkpoints survive a clear");
    }

    #[test]
    fn replayed_rebuilds_transcript_and_checkpoints() {
        let mut conv = transcript();
        conv.edit_content(0, "rewritten").expect("in range");
        let undo = match conv.undo.pop() {
            Some(entry) => vec![entry],
            None => panic!("an edit records a checkpoint"),
        };

        let mut restored = Conversation::replayed("be brief", conv.messages().to_vec(), undo);
        assert_eq!(restored.messages()[0].content, "rewritten");
        assert!(restored.undo_last_edit());
        assert_eq!(restored.messages()[0].content, "hi");
    }

    #[test]
    fn tool_structure_accepts_plain_text_and_complete_rounds() {
        let mut conv = Conversation::new("be brief");
        conv.push_user("hi");
        conv.push_message(Message::assistant_tool_calls(vec![call("call_1")]));
        conv.push_message(Message::tool_result("call_1", "pong"));
        conv.push_assistant("done");
        assert_eq!(
            Conversation::validate_tool_structure(conv.messages()),
            Ok(())
        );
    }

    #[test]
    fn tool_structure_rejects_broken_shapes() {
        let orphan = vec![Message::tool_result("call_1", "pong")];
        assert!(Conversation::validate_tool_structure(&orphan).is_err());

        let unanswered = vec![
            Message::assistant_tool_calls(vec![call("call_1")]),
            Message::assistant("skipped the result"),
        ];
        assert!(Conversation::validate_tool_structure(&unanswered).is_err());

        let wrong_id = vec![
            Message::assistant_tool_calls(vec![call("call_1")]),
            Message::tool_result("call_9", "pong"),
        ];
        assert!(Conversation::validate_tool_structure(&wrong_id).is_err());

        let duplicate = vec![
            Message::assistant_tool_calls(vec![call("call_1")]),
            Message::tool_result("call_1", "pong"),
            Message::tool_result("call_1", "pong again"),
        ];
        assert!(Conversation::validate_tool_structure(&duplicate).is_err());

        // A result after an intervening message is an orphan, not an answer.
        let delayed = vec![
            Message::assistant_tool_calls(vec![call("call_1")]),
            Message::assistant("text first"),
            Message::tool_result("call_1", "pong"),
        ];
        assert!(Conversation::validate_tool_structure(&delayed).is_err());
    }

    /// Deleting an assistant tool-calls message together with its results
    /// keeps the shape valid; deleting either half alone would not.
    #[test]
    fn deleting_a_whole_tool_round_keeps_the_wire_shape() {
        let mut conv = Conversation::new("be brief");
        conv.push_user("hi");
        conv.push_message(Message::assistant_tool_calls(vec![call("call_1")]));
        conv.push_message(Message::tool_result("call_1", "pong"));
        conv.push_assistant("done");

        conv.delete_at(&[1, 2]).expect("in range");
        assert_eq!(
            Conversation::validate_tool_structure(conv.messages()),
            Ok(())
        );
        assert_eq!(conv.len(), 2);

        // The undo of the round also restores a valid shape.
        assert!(conv.undo_last_edit());
        assert_eq!(
            Conversation::validate_tool_structure(conv.messages()),
            Ok(())
        );
        assert_eq!(conv.len(), 4);
    }
}
