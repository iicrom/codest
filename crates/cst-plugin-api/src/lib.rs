//! The contract between codest and the plugins it links in.
//!
//! This crate carries **types only** — no HTTP clients, no config loading, no
//! terminal code. It exists so a provider plugin can be built and maintained
//! as a separate repository: the host depends on the plugin (to link it into
//! the binary), the plugin depends on this crate (to implement the contracts),
//! and neither needs to depend on the other.
//!
//! ```text
//! codest ──depends on──▶ cst-provider-qoder ──depends on──▶ cst-plugin-api
//!    └────────────────────── depends on ─────────────────────────┘
//! ```
//!
//! The host re-exports every module here under its historical paths
//! (`codest::core::model`, `codest::core::commands`, …), so moving a type
//! across this boundary does not rename anything visible to existing code.
//!
//! Bump [`PLUGIN_API_VERSION`] whenever a change here would break an existing
//! plugin build. Adding a trait method with a default implementation does not
//! require a bump; changing an existing signature does.

pub mod auth;
pub mod commands;
pub mod message;
pub mod model;
pub mod plugin;
pub mod providers;
pub mod sse;
pub mod tools;

pub mod config;

/// The contract version an external plugin is compiled against.
///
/// The host and a plugin agree by Cargo version, not by a runtime handshake:
/// a plugin built against an incompatible contract simply fails to compile.
/// This constant exists for diagnostics (`/status`, logs) and for the
/// `cst-plugin-api` dependency range in a plugin's `Cargo.toml`.
pub const PLUGIN_API_VERSION: u32 = 1;

pub use auth::AuthMethod;
pub use auth::AuthMethodKind;
pub use auth::AuthProvider;
pub use auth::AuthStatus;
pub use auth::SignInFuture;
pub use auth::SignInNotice;
pub use auth::SignInNotifier;
pub use auth::SignInOutcome;
pub use commands::ArgsPolicy;
pub use commands::CommandCandidate;
pub use commands::CommandError;
pub use commands::CommandInput;
pub use commands::CommandSpec;
pub use commands::CommandTable;
pub use commands::CommandTableError;
pub use commands::Invocation;
pub use config::EFFORT_FIELD_NONE;
pub use config::ModelInfo;
pub use config::ProviderInstance;
pub use config::WireQuirks;
pub use message::Message;
pub use message::Role;
pub use model::ModelClient;
pub use model::ModelError;
pub use model::ModelStream;
pub use model::StreamItem;
pub use model::TurnOptions;
pub use plugin::Plugin;
pub use plugin::PluginContext;
pub use plugin::PluginEffect;
pub use plugin::PluginError;
pub use plugin::PluginInfo;
pub use plugin::ReportBlock;
pub use plugin::ReportEntry;
pub use plugin::ReportGroup;
pub use plugin::RequestFacts;
pub use providers::ListModelsFuture;
pub use providers::ProtocolFactory;
pub use sse::SseDecoder;
pub use sse::SseFrame;
pub use tools::MAX_TOOL_ROUNDS;
pub use tools::ToolCall;
pub use tools::ToolCallAccumulator;
pub use tools::ToolExecutor;
pub use tools::ToolSpec;
