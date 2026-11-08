//! Plugins shipped with codest.
//!
//! Each plugin lives in its own module and exposes a `static` value. The
//! registry below is the single place that decides what is compiled in and in
//! which order hooks run.

mod clear;
mod help;
mod prompt_debug;
mod quit;
mod status;

use crate::plugins::Plugin;

pub use clear::CLEAR;
pub use help::HELP;
pub use prompt_debug::PROMPT_DEBUG;
pub use quit::QUIT;
pub use status::STATUS;

/// Every plugin compiled into the binary, in hook and menu order.
pub static BUILTIN_PLUGINS: &[&dyn Plugin] = &[
    &help::HELP,
    &clear::CLEAR,
    &quit::QUIT,
    &status::STATUS,
    &prompt_debug::PROMPT_DEBUG,
];
