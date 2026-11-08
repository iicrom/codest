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
//! * [`session`] is the worker task that owns every recorded conversation and
//!   turns submissions into events. The TUI only ever sees
//!   [`session::AgentSession`]; [`sessions`] holds the built-in `/new` and
//!   `/sessions` command specs, whose behavior is implemented by front-ends.
//! * [`rollout`] persists sessions as JSONL under
//!   `~/.codest/sessions/<workspace-hash>/` (codex's rollout format,
//!   distilled): a metadata line plus one line per conversation change.

pub mod commands;
pub mod conversation;
pub mod model;
pub mod openai_compat;
pub mod protocol;
pub mod rollout;
pub mod session;
pub mod sessions;

#[cfg(test)]
pub(crate) mod test_model;

pub use commands::{
    ArgsPolicy, CommandCandidate, CommandError, CommandInput, CommandSpec, CommandTable,
    CommandTableError, Invocation,
};
pub use conversation::{Conversation, DEFAULT_SYSTEM_PROMPT};
pub use model::{ModelClient, ModelError};
pub use protocol::{Event, EventMsg, Message, Op, Role, Submission};
pub use rollout::{LoadedSession, RolloutItem, RolloutWriter, SessionMeta};
pub use session::{AgentSession, SessionBootstrap};
pub use sessions::SESSION_COMMANDS;
