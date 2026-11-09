//! Shared SSE (`text/event-stream`) framing.
//!
//! The decoder moved to [`cst_plugin_api::sse`] so plugin protocols reuse it
//! instead of reimplementing (and re-bugging) chunk-boundary handling. The
//! host re-exports it under the historical `codest::core::sse` path.

pub use cst_plugin_api::sse::*;
