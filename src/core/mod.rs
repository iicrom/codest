//! The minimal agent core.
//!
//! This is a deliberately small descendant of codex's `protocol` + `core`
//! split:
//!
//! * [`protocol`] defines the SQ/EQ message bus (`Op` submissions in, `Event`
//!   events out) exactly like `codex_protocol::protocol`.
//! * [`conversation`] is the message history.
//! * [`model`] is the abstraction over a streaming model backend, with two
//!   implementations: [`mock`] (offline) and [`openai_compat`] (HTTP + SSE).
//! * [`session`] is the worker task that owns the conversation and turns
//!   submissions into events. The TUI only ever sees [`session::AgentSession`].

pub mod conversation;
pub mod mock;
pub mod model;
pub mod openai_compat;
pub mod protocol;
pub mod session;

pub use conversation::Conversation;
pub use model::{ModelClient, ModelError};
pub use protocol::{Event, EventMsg, Message, Op, Role, Submission};
pub use session::AgentSession;
