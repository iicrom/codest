//! Model client abstraction and its streaming contract.
//!
//! A client returns a stream of [`StreamItem`]s rather than a whole response.
//! That is the one property the TUI needs in order to render output as it
//! arrives, and it keeps the interface independent of any particular vendor
//! API. The item vocabulary is deliberately open-ended (`#[non_exhaustive]`):
//! reasoning content and usage exist today; tool calls and friends can be
//! added later without changing the trait.

use std::fmt;
use std::pin::Pin;

use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::message::Message;

/// One item of a model's streamed response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamItem {
    /// Incremental assistant text (the visible reply).
    Delta(String),
    /// Incremental reasoning content (`delta.reasoning_content` on
    /// reasoning-tuned OpenAI-compatible providers).
    Thinking(String),
    /// Complete tool calls the model asked for, flushed once their streamed
    /// argument fragments have been reassembled (typically at
    /// `finish_reason: "tool_calls"` or `[DONE]`).
    ToolCalls(Vec<crate::tools::ToolCall>),
    /// Token usage reported for one streaming round. `cached` is the
    /// prompt-prefix subset of `input` the provider served from its prompt
    /// cache (`prompt_tokens_details.cached_tokens` on OpenAI-compatible
    /// APIs), when it reports one — `None` means the backend does not
    /// expose cache accounting, not zero.
    Usage {
        input: u64,
        output: u64,
        cached: Option<u64>,
    },
}

/// Boxed delta stream. `Err` terminates the turn with an error event.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<StreamItem, ModelError>> + Send>>;

/// Per-turn request options, threaded from the worker's state (set via ops)
/// down to the client. Options a backend does not understand are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnOptions {
    /// Reasoning effort for this turn (e.g. `"low"`/`"medium"`/`"high"`,
    /// `/effort`); `None` sends no effort field at all.
    pub effort: Option<String>,
    /// Tools declared by plugins and offered to the model this turn. Empty
    /// sends no `tools` field at all.
    pub tools: Vec<crate::tools::ToolSpec>,
    /// The active session's id, sent under the client's configured session
    /// header (`session-id` by default) so an upstream can route and cache
    /// per conversation. Stable for a session across turns and restarts;
    /// `None` sends no session header.
    ///
    /// This rides the per-turn options rather than the client because the
    /// value follows the *active session*, while the header name is baked into
    /// the client from provider configuration.
    pub session_id: Option<String>,
}

/// A streaming chat backend.
///
/// Implementations must not panic and should return a stream even for invalid
/// input, reporting failures through the stream items instead.
pub trait ModelClient: Send + Sync {
    /// Human-readable backend label.
    fn label(&self) -> String;

    /// Stream assistant text for `messages`. `cancel` should be honoured both
    /// while waiting on the network and between deltas.
    fn stream_chat(
        &self,
        messages: Vec<Message>,
        options: TurnOptions,
        cancel: CancellationToken,
    ) -> ModelStream;
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("network error: {0}")]
    Network(String),

    #[error("model API returned HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("invalid response from model API: {0}")]
    Decode(String),

    #[error("model backend is not configured: {0}")]
    NotConfigured(String),
}

impl ModelError {
    pub fn network(error: impl fmt::Display) -> Self {
        Self::Network(error.to_string())
    }

    pub fn decode(error: impl fmt::Display) -> Self {
        Self::Decode(error.to_string())
    }
}
