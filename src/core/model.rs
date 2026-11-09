//! Model client abstraction and its streaming contract.
//!
//! The trait and its vocabulary moved to [`cst_plugin_api::model`] so an
//! external provider plugin can implement [`ModelClient`] without depending on
//! this binary. The host re-exports them under the historical
//! `codest::core::model` path.

pub use cst_plugin_api::model::*;
