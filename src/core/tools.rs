//! Tool-calling vocabulary shared by the core and the plugin layer.
//!
//! The types moved to [`cst_plugin_api::tools`] — a plugin declares tools with
//! them and a streaming protocol reassembles calls with the same
//! [`ToolCallAccumulator`](cst_plugin_api::tools::ToolCallAccumulator). The
//! host re-exports them under the historical `codest::core::tools` path; the
//! turn loop that drives model → tool → model rounds stays in the core.

pub use cst_plugin_api::tools::*;
