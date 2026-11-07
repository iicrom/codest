//! `cst` — a minimal TUI chat agent distilled from codex.
//!
//! The crate is split the same way codex is:
//!
//! * [`core`] owns the submission/event protocol, conversation state, and the
//!   model client. It has no terminal or rendering dependencies.
//! * [`tui`] owns the terminal event loop, chat transcript, and composer. It
//!   talks to the core exclusively through [`core::AgentSession`].
//!
//! Keeping the boundary sharp is the whole point of this extraction: the core
//! can be driven by any front-end, and the TUI can be tested without a model.

pub mod core;
pub mod tui;
