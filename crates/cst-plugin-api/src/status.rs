//! The status-bar contract: how a plugin contributes a footer segment.
//!
//! The footer stays the host's — a plugin never draws, and the contract crate
//! keeps no terminal types. A plugin submits *data* ([`StatusSegment`], pure
//! text plus a tone) and the host owns layout, ordering, truncation, and the
//! palette. That is the same split as [`crate::plugin::PluginEffect`]: data
//! in, pixels out of scope.
//!
//! A segment is identified by `plugin_id:key` — the full key the host stores
//! and the user names in `[status_bar] order`. Plugins build segments with
//! their *local* key and hand them to
//! [`crate::plugin::PluginContext::set_status`] or
//! [`crate::plugin::PluginBridge::set_status`], which resolve the prefix; the
//! [`crate::plugin::PluginEffect`] variants carry the resolved form.

/// How strongly a segment reads. The host maps tones to its theme: `Accent`
/// is the same highlight the state word gets while streaming, `Warn` the
/// warning color, `Dim` the quiet default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    /// Secondary text — the default for informational segments.
    Dim,
    /// Highlighted text — for values the user should notice (a fast
    /// transfer, a live counter).
    Accent,
    /// Warning text — for values that need attention (a slow stream, a
    /// threshold crossed).
    Warn,
}

/// One status-bar segment: a short text with a tone, keyed for updates.
///
/// Keep the text short — the footer is one line and the host drops segments
/// that no longer fit, in `[status_bar] order` priority. Update by key:
/// setting a segment with the same key replaces its text and tone, in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSegment {
    /// The plugin-local key (`"rate"`). The context/bridge helpers prefix the
    /// plugin id, so the stored full key reads `tps:rate`.
    pub key: String,
    /// The rendered text, verbatim. No separators needed — the host inserts
    /// `·` between segments.
    pub text: String,
    pub tone: StatusTone,
}

impl StatusSegment {
    /// A dim segment.
    pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
            tone: StatusTone::Dim,
        }
    }

    /// A segment with an explicit tone.
    pub fn with_tone(key: impl Into<String>, text: impl Into<String>, tone: StatusTone) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
            tone,
        }
    }
}
