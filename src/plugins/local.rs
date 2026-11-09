//! Locally-added plugins.
//!
//! This is the seam an external plugin repository installs into. The file is
//! committed empty so the host always builds; adding a plugin is two edits:
//!
//! 1. add the dependency to `Cargo.toml` (or `cargo add --path ../my-plugin`);
//! 2. uncomment its `static` in [`LOCAL_PLUGINS`].
//!
//! Then `cargo build`. See `docs/local-plugins.md` for the full walkthrough.
//!
//! ```toml
//! # Cargo.toml
//! cst-provider-qoder = { path = "../cst-provider-qoder" }
//! ```
//!
//! ```ignore
//! pub static LOCAL_PLUGINS: &[&dyn Plugin] = &[&cst_provider_qoder::PLUGIN];
//! ```
//!
//! Because this file is meant to stay modified, ask git to ignore local edits:
//!
//! ```sh
//! git update-index --skip-worktree src/plugins/local.rs
//! ```
//!
//! Built-in plugins keep their own registry ([`super::builtin::BUILTIN_PLUGINS`]);
//! the host concatenates the two, validating ids and command names across
//! both, so a local plugin cannot shadow a shipped one.

use super::Plugin;

/// Plugins added locally by the user; empty in a fresh checkout.
pub static LOCAL_PLUGINS: &[&dyn Plugin] = &[
    // &cst_provider_qoder::PLUGIN,
];
