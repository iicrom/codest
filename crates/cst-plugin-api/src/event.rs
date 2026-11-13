//! The event-stream contract: what a plugin can observe while the agent runs.
//!
//! The four lifecycle hooks (`on_startup`, `on_user_message`,
//! `on_turn_complete`, `on_conversation_cleared`) predate this module and stay
//! as they are. [`PluginEvent`] is the superset: one hook, [`crate::plugin::Plugin::on_event`],
//! receives everything the agent worker emits, gated by the categories a
//! plugin declares through [`crate::plugin::Plugin::subscriptions`]. A plugin
//! that subscribes to nothing pays nothing — the host filters before
//! dispatching, so uninterested plugins never see a copy of a stream delta.
//!
//! The vocabulary mirrors the host-internal bus 1:1 on purpose: the bus is
//! free to grow, but the contract here is what external plugins compile
//! against, so kinds are added deliberately and everything is `#[non_exhaustive]`
//! where adding a variant would otherwise be a breaking change.
//!
//! Timing: every event carries the host's monotonic emit stamp
//! ([`PluginEvent::at`]). A throughput meter, a latency histogram, or a
//! monitor's batching all derive their clocks from it — plugins never guess
//! when a delta arrived from arrival-order alone.

use std::time::Instant;

use crate::message::Message;

/// Which half of a [`PluginEventKind::MessageDelta`] streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// Assistant answer text.
    Text,
    /// Reasoning text, as streamed by thinking models.
    Thinking,
}

/// One event from the agent, borrowing its payload from the host's bus.
///
/// Borrowing (rather than cloning) means a delta costs nothing to forward:
/// the host hands out a `&str` into the event it just built. The borrow ends
/// when the dispatch returns; a plugin that must keep payload copies it.
#[derive(Debug, Clone, Copy)]
pub struct PluginEvent<'a> {
    /// The host's monotonic emit time — when the worker produced this event,
    /// not when the dispatch ran.
    pub at: Instant,
    pub kind: PluginEventKind<'a>,
}

/// The payload of one [`PluginEvent`], matched without unwrapping the
/// envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginEventKind<'a> {
    /// A turn was accepted and the model request is starting.
    TurnStarted,
    /// Incremental assistant output. Both kinds stream repeatedly across a
    /// turn; a throughput meter counts them against [`PluginEvent::at`].
    MessageDelta { kind: DeltaKind, text: &'a str },
    /// The complete assistant text for one streaming round. Redundant with
    /// the deltas that produced it, kept for monitors that want the settled
    /// form.
    AgentMessage { text: &'a str },
    /// A tool call the model asked for is about to execute.
    ToolCallStarted { name: &'a str, arguments: &'a str },
    /// One tool call has been executed (or failed). `result` is the tool's
    /// output on success (empty on failure); `error` says why it failed.
    ToolCallFinished {
        name: &'a str,
        result: &'a str,
        error: Option<&'a str>,
        duration_ms: u64,
    },
    /// Token usage as the backend reported it for the round that just
    /// finished streaming. `output` is that round's output tokens.
    RoundUsage { input: u64, output: u64 },
    /// Token usage for the completed turn, right before [`PluginEventKind::TurnComplete`].
    TurnUsage { input: u64, output: u64 },
    /// The turn was cancelled by the user.
    TurnAborted { reason: &'a str },
    /// The turn (or an op) hit an error.
    Error { message: &'a str },
    /// The turn is over.
    TurnComplete,
    /// The conversation history was dropped (system prompt retained).
    ConversationCleared,
    /// The active conversation: system prompt first, then the transcript.
    /// Sent on [`crate::plugin::Plugin`] startup and after every session
    /// switch (the host asks the worker), plus as the confirmation of every
    /// transcript mutation. A monitor's fallback read: when in doubt,
    /// re-snapshot rather than replay deltas.
    ConversationSnapshot { messages: &'a [Message] },
    /// A fresh session was created and is now active.
    SessionStarted { session: usize },
    /// The active session switched to the given creation-order index.
    SessionSwitched { session: usize },
    /// The session at `session` was named.
    SessionRenamed { session: usize, name: &'a str },
    /// The active model is now `model` (`"provider/model"`).
    ModelSwitched { model: &'a str },
    /// The reasoning effort for the following turns is now `effort`
    /// (`None` = no effort field is sent).
    EffortSwitched { effort: Option<&'a str> },
    /// The named profile is now active.
    ProfileSwitched { name: &'a str },
    /// The enabled-tool set for the active session is now `enabled`.
    ToolsSwitched { enabled: &'a [String] },
    /// The worker has stopped. The last event a plugin sees.
    Shutdown,
}

impl PluginEventKind<'_> {
    /// The subscription category this kind belongs to. The host dispatches an
    /// event to a plugin when the plugin's
    /// [`crate::plugin::Plugin::subscriptions`] contains this set.
    pub fn subscription(&self) -> EventSubscription {
        match self {
            Self::MessageDelta { .. } => EventSubscription::DELTAS,
            Self::RoundUsage { .. } | Self::TurnUsage { .. } => EventSubscription::USAGE,
            Self::ToolCallStarted { .. } | Self::ToolCallFinished { .. } => EventSubscription::TOOLS,
            Self::ConversationSnapshot { .. } => EventSubscription::SNAPSHOT,
            Self::SessionStarted { .. }
            | Self::SessionSwitched { .. }
            | Self::SessionRenamed { .. }
            | Self::ModelSwitched { .. }
            | Self::EffortSwitched { .. }
            | Self::ProfileSwitched { .. }
            | Self::ToolsSwitched { .. } => EventSubscription::SESSION,
            Self::TurnStarted
            | Self::AgentMessage { .. }
            | Self::TurnAborted { .. }
            | Self::Error { .. }
            | Self::TurnComplete
            | Self::ConversationCleared
            | Self::Shutdown => EventSubscription::LIFECYCLE,
        }
    }
}

/// The set of event categories a plugin subscribes to.
///
/// A bit set rather than an enum so a plugin declares interests
/// independently: `DELTAS.union(USAGE)` reads as the intent it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventSubscription(u8);

impl EventSubscription {
    /// No categories — the default. A plugin with this (or no
    /// `subscriptions` override) never sees [`crate::plugin::Plugin::on_event`].
    pub const NONE: Self = Self(0);
    /// Turn and error lifecycle: started, settled text, usage-of-turn is
    /// separate ([`Self::USAGE`]), aborted, error, complete, cleared,
    /// shutdown.
    pub const LIFECYCLE: Self = Self(1 << 0);
    /// Streaming deltas (text and thinking).
    pub const DELTAS: Self = Self(1 << 1);
    /// Token usage, per round and per turn.
    pub const USAGE: Self = Self(1 << 2);
    /// Tool call lifecycle.
    pub const TOOLS: Self = Self(1 << 3);
    /// Session, model, effort, profile, and tool-set switches.
    pub const SESSION: Self = Self(1 << 4);
    /// Full conversation snapshots.
    pub const SNAPSHOT: Self = Self(1 << 5);
    /// Every category.
    pub const ALL: Self = Self(0b11_1111);

    /// Combine two category sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether `self` includes every category in `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether `self` selects no categories at all.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_flags_compose() {
        let both = EventSubscription::DELTAS.union(EventSubscription::USAGE);
        assert!(both.contains(EventSubscription::DELTAS));
        assert!(both.contains(EventSubscription::USAGE));
        assert!(!both.contains(EventSubscription::TOOLS));
        assert!(EventSubscription::ALL.contains(both));
        assert!(EventSubscription::NONE.is_empty());
        assert!(both.union(EventSubscription::ALL) == EventSubscription::ALL);
    }

    #[test]
    fn kinds_map_to_their_categories() {
        use std::time::Instant;
        let at = Instant::now();
        let text = String::from("hi");
        let delta = PluginEventKind::MessageDelta { kind: DeltaKind::Text, text: &text };
        assert_eq!(delta.subscription(), EventSubscription::DELTAS);

        let empty: [Message; 0] = [];
        let snapshot = PluginEventKind::ConversationSnapshot { messages: &empty };
        assert_eq!(snapshot.subscription(), EventSubscription::SNAPSHOT);

        let envelope = PluginEvent { at, kind: delta };
        assert_eq!(envelope.kind.subscription(), EventSubscription::DELTAS);
    }
}
