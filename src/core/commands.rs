//! Slash-command model and parser.
//!
//! The definitions moved to [`cst_plugin_api::commands`] so an external plugin
//! can declare commands without depending on this binary. The host re-exports
//! them under the historical `codest::core::commands` path, which keeps every
//! existing reference — and every built-in plugin — unchanged.
//!
//! Only input whose first non-whitespace character is `/` is considered;
//! recognized commands are dispatched locally and never sent to the model,
//! unknown commands produce a [`CommandError`], and everything else is a
//! normal user message.

pub use cst_plugin_api::commands::*;
