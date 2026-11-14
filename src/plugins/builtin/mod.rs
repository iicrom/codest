//! Plugins shipped with codest.
//!
//! Each plugin lives in its own module and exposes a `static` value. The
//! registry below is the single place that decides what is compiled in and in
//! which order hooks run.

mod ask_user;
mod clear;
// `continue` is a keyword; the raw identifier keeps the file named after the
// command like every other plugin here.
mod r#continue;
mod help;
mod quit;
mod status;
mod tools;
mod tps;

use crate::plugins::Plugin;

pub use ask_user::{ASK_USER, CUSTOM_ROW, NEXT_ROW};
pub use clear::CLEAR;
pub use help::HELP;
pub use r#continue::CONTINUE;
pub use quit::QUIT;
pub use status::STATUS;
/// The host's shell tool plugin: `bash` on unix, `pwsh` on Windows. The id
/// and the tool name follow the platform, so `/tools` and `profile.tools`
/// speak the same word on either host.
pub use tools::SHELL;
pub use tools::FS;
pub use tps::TPS;

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
    &r#continue::CONTINUE,
    &tps::TPS,
    &ask_user::ASK_USER,
    &tools::SHELL,
    &tools::FS,
];
