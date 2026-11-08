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

use crate::core::protocol::Message;

/// One item of a model's streamed response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamItem {
    /// Incremental assistant text (the visible reply).
    Delta(String),
    /// Incremental reasoning content (`delta.reasoning_content` on
    /// reasoning-tuned OpenAI-compatible providers).
    Thinking(String),
    /// Token usage reported for the turn.
    Usage { input: u64, output: u64 },
}

/// Boxed delta stream. `Err` terminates the turn with an error event.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<StreamItem, ModelError>> + Send>>;

/// A streaming chat backend.
///
/// Implementations must not panic and should return a stream even for invalid
/// input, reporting failures through the stream items instead.
pub trait ModelClient: Send + Sync {
    /// Human-readable backend label.
    fn label(&self) -> String;

    /// Stream assistant text for `messages`. `cancel` should be honoured both
    /// while waiting on the network and between deltas.
    fn stream_chat(&self, messages: Vec<Message>, cancel: CancellationToken) -> ModelStream;
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
