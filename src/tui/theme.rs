//! Shared colors. Kept in one place so the TUI can be restyled without
//! touching rendering logic.

use ratatui::style::Color;

pub const ACCENT: Color = Color::Cyan;
pub const USER: Color = Color::Cyan;
pub const ASSISTANT: Color = Color::Green;
pub const ERROR: Color = Color::LightRed;
pub const WARNING: Color = Color::Yellow;
pub const DIM: Color = Color::DarkGray;
