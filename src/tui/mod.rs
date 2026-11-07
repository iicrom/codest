//! Minimal terminal UI for the chat agent.
//!
//! The layout is a transcript, a composer, and a one-line footer. The event
//! loop selects over terminal events, agent events, and a tick used for the
//! spinner, which is the same shape as codex's `App::run` loop without the
//! app-server, approvals, and overlay machinery.

pub mod app;
pub mod chat;
pub mod composer;
pub mod theme;
pub mod wrap;

use clap::ValueEnum;

pub use app::run;

/// How the TUI owns the terminal.
///
/// Matches `pi`'s `tuiMode` (`fullscreen` / `regular`) and cht's `fullscreen`
/// boolean: fullscreen uses the alternate screen and app-owned scrolling,
/// inline leaves the transcript in the terminal's native scrollback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum TuiMode {
    /// Alternate screen, app-side scrolling, and mouse-wheel support.
    #[default]
    Fullscreen,
    /// Inline UI; the terminal owns the scrollback.
    #[value(alias = "regular")]
    Inline,
}

impl TuiMode {
    pub fn label(self) -> &'static str {
        match self {
            TuiMode::Fullscreen => "fullscreen",
            TuiMode::Inline => "inline",
        }
    }
}
