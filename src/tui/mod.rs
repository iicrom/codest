//! Minimal terminal UI for the chat agent.
//!
//! The layout is a transcript, a composer, and a one-line footer. The event
//! loop selects over terminal events, agent events, and a tick used for the
//! spinner, which is the same shape as codex's `App::run` loop without the
//! app-server, approvals, and overlay machinery.

pub mod app;
pub mod cards;
pub mod chat;
pub mod composer;
pub mod context_view;
pub mod highlight;
pub mod inline_edit;
pub mod markdown;
pub mod prompt_dialog;
pub mod provider_wizard;
pub mod report_popup;
pub mod screen;
pub mod session;
pub mod settings_view;
pub mod theme;
pub mod tool_body;
pub mod usage_view;
pub mod wrap;

use clap::ValueEnum;

pub use app::ProfileStartup;
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

/// How the TUI picks its starting sessions from the workspace's saved
/// rollouts under `~/.codest/sessions/<workspace-hash>/`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestoreMode {
    /// Start a fresh session; saved ones stay on disk until requested.
    #[default]
    None,
    /// Continue the most recent saved session (`--continue`).
    Continue,
    /// Load every saved session and open the picker at startup (`--resume`).
    Picker,
}
