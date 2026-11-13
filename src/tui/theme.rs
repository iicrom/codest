//! Shared colors. Kept in one place so the TUI can be restyled without
//! touching rendering logic.

use ratatui::style::Color;

pub const ACCENT: Color = Color::Cyan;
pub const USER: Color = Color::Cyan;
pub const ASSISTANT: Color = Color::Green;
pub const ERROR: Color = Color::LightRed;
pub const WARNING: Color = Color::Yellow;
/// Mid-gray for secondary structure (report group headings) — brighter than
/// [`DIM`], quieter than the default foreground, so bold headings read as
/// section markers without shouting.
pub const SUBTLE: Color = Color::Gray;
pub const DIM: Color = Color::DarkGray;
/// Thinking cards' identity color. Purple is far from the cyan accent, the
/// green assistant bullet, and the dark-gray notice text, so a thinking card —
/// folded to a single line by default — never reads as ordinary output.
pub const THINKING: Color = Color::LightMagenta;

// Tool-card category colors (the cordis-harness-tui convention: the card's
// status dot and tool name carry the tool's category, not its outcome — the
// outcome is the glyph itself, `●` success vs `✗` error).
/// Command runners: `bash`, `pwsh`.
pub const TOOL_EXEC: Color = Color::Cyan;
/// Read-only file tools (`read`).
pub const TOOL_READ: Color = Color::Green;
/// Mutating file tools (`write`, `edit`).
pub const TOOL_MUTATE: Color = Color::Yellow;

// Diff body tones. A `write`/`edit` card paints the file's new content as `+`
// rows and the text it replaced as `-` rows; the pair is deliberately not
// [`ERROR`] and [`ASSISTANT`], so a diff never reads as a failure or as a
// stray assistant bullet.
pub const DIFF_ADD: Color = Color::Green;
pub const DIFF_DEL: Color = Color::Red;

// Code-block syntax highlighting. Semantic names rather than "the color of a
// function": each kind is one palette entry, so restyling code means editing
// this block and `highlight::style` keeps its shape.
pub const CODE_KEYWORD: Color = Color::Magenta;
pub const CODE_TYPE: Color = Color::Cyan;
pub const CODE_FUNCTION: Color = Color::Blue;
pub const CODE_STRING: Color = Color::Green;
pub const CODE_NUMBER: Color = Color::Yellow;
pub const CODE_CONSTANT: Color = Color::LightYellow;
pub const CODE_ATTRIBUTE: Color = Color::LightMagenta;
/// Dimmer than [`DIM`]'s peers on purpose: comments should recede behind the
/// code they annotate without disappearing into the background.
pub const CODE_COMMENT: Color = Color::DarkGray;
