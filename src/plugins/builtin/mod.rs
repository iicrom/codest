//! Plugins shipped with codest.
//!
//! Each plugin lives in its own module and exposes a `static` value. The
//! registry below is the single place that decides what is compiled in and in
//! which order hooks run.

mod clear;
mod help;
mod quit;
mod status;
mod tools;

use crate::plugins::Plugin;

pub use clear::CLEAR;
pub use help::HELP;
pub use quit::QUIT;
pub use status::STATUS;
/// The host's shell tool plugin: `bash` on unix, `pwsh` on Windows. The id
/// and the tool name follow the platform, so `/tools` and `profile.tools`
/// speak the same word on either host.
pub use tools::SHELL;
pub use tools::FS;

/// Every plugin compiled into the binary, in hook and menu order. The tool
/// plugins come last so their system-prompt section lands after the command
/// plugins' contributions. (`/context` used to live here as the
/// `prompt_debug` plugin; it is a front-end command now — see
/// `crate::core::context` and `crate::tui::context_view`.)
pub static BUILTIN_PLUGINS: &[&dyn Plugin] = &[
    &help::HELP,
    &clear::CLEAR,
    &quit::QUIT,
    &status::STATUS,
    &tools::SHELL,
    &tools::FS,
];
