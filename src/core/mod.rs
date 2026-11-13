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
//!   [`session::AgentSession`]; [`sessions`] and [`context`] hold the
//!   built-in `/new`, `/sessions`, and `/context` command specs, whose
//!   behavior is implemented by front-ends.
//! * [`rollout`] persists sessions as JSONL under
//!   `~/.codest/sessions/<workspace-hash>/` (codex's rollout format,
//!   distilled): a metadata line plus one line per conversation change.
//! * [`context_store`] is the (unwired) storage contract for archiving
//!   context entries to local files — the future "index" externalization.

pub mod commands;
pub mod context;
pub mod context_store;
pub mod config;
pub mod conversation;
pub mod echo_model;
pub mod model;
pub mod openai_compat;
pub mod protocol;
pub mod providers;
pub mod reasoning;
pub mod rollout;
pub mod session;
pub mod session_id;
pub mod sessions;
pub mod settings;
pub mod sse;
pub mod tokens;
pub mod tools;
pub mod tools_switch;

pub use commands::{
    ArgsPolicy, CommandCandidate, CommandError, CommandInput, CommandSpec, CommandTable,
    CommandTableError, Invocation,
};
pub use config::{
    GlobalConfig, ModelInfo, ProviderInstance, expand_env_template, providers_path,
};
pub use conversation::{Conversation, DEFAULT_SYSTEM_PROMPT};
pub use echo_model::EchoModelClient;
pub use model::{ModelClient, ModelError, StreamItem, TurnOptions};
pub use protocol::{Event, EventMsg, Message, Op, Role, Submission};
pub use providers::{
    NO_MODEL_GUIDANCE, NO_MODEL_SELECTED, ModelPool, ProtocolFactory, ProviderRegistry,
    RegistryError, Selection, missing_credential_guidance,
};
pub use reasoning::{DEFAULT_EFFORT_LEVELS, EFFORT_COMMAND, REASONING_COMMANDS, THINKING_COMMAND};
pub use rollout::{LoadedSession, RolloutItem, RolloutWriter, SessionMeta};
pub use session::{AgentSession, SessionBootstrap};
pub use session_id::{is_session_id, new_session_id};
pub use sessions::SESSION_COMMANDS;
pub use context::CONTEXT_COMMANDS;
pub use settings::SETTINGS_COMMANDS;
pub use tokens::{context_percent, format_context_usage, format_tokens};
pub use tools_switch::TOOLS_COMMANDS;
pub use tools::{DEFAULT_MAX_TOOL_ROUNDS, ToolCall, ToolExecutor, ToolSpec};
