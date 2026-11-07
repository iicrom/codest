//! The minimal agent core.
//!
//! This is a deliberately small descendant of codex's `protocol` + `core`
//! split:
//!
//! * [`protocol`] defines the SQ/EQ message bus (`Op` submissions in, `Event`
//!   events out) exactly like `codex_protocol::protocol`.
//! * [`conversation`] is the message history.
//! * [`commands`] defines the slash-command table and parser shared by every
//!   front-end. The command set itself is contributed by the plugin layer.
//! * [`model`] is the abstraction over a streaming model backend;
//!   [`openai_compat`] is the only shipped implementation (HTTP + SSE).
//! * [`session`] is the worker task that owns the conversation and turns
//!   submissions into events. The TUI only ever sees [`session::AgentSession`].

pub mod commands;
pub mod conversation;
pub mod model;
pub mod openai_compat;
pub mod protocol;
pub mod session;

#[cfg(test)]
pub(crate) mod test_model;

pub use commands::{
    ArgsPolicy, CommandError, CommandInput, CommandSpec, CommandTable, CommandTableError,
    Invocation,
};
pub use conversation::{Conversation, DEFAULT_SYSTEM_PROMPT};
pub use model::{ModelClient, ModelError};
pub use protocol::{Event, EventMsg, Message, Op, Role, Submission};
pub use session::AgentSession;
