//! The settings vocabulary: how a plugin describes the options `/settings`
//! may edit.
//!
//! A plugin declares a [`SettingsSection`] — a title, a one-line description,
//! and one [`SettingField`] per option. The host renders the section, reads
//! the stored values out of `config.toml` (a plugin's own pocket is its
//! `[plugins."<id>"]` table), writes an edit back atomically, and calls
//! [`crate::plugin::Plugin::on_settings_changed`] so the plugin can apply the
//! new value without a restart.
//!
//! Values are carried as *text* across the boundary (`"true"`, `"high"`,
//! `"64"`): the field's [`SettingKind`] says how to store it (a TOML boolean,
//! integer, or string), so a plugin never has to agree with the host on a
//! richer type. Declaring nothing keeps the plugin out of `/settings`
//! entirely — the default implementation of
//! [`crate::plugin::Plugin::settings`] returns `None`.

/// The namespace the host's own settings live in. A plugin id may not be
/// `codest` (ids are validated against the shipped set, which does not use
/// it), so the two can never collide.
pub const HOST_NAMESPACE: &str = "codest";

/// How a field's text value is stored and edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// A TOML boolean. Space/Enter/←→ flip it.
    Boolean,
    /// One of [`SettingField::options`]; ←→ cycle, Enter advances.
    Select,
    /// A TOML integer (`"64"`); Enter opens a one-line editor.
    Number,
    /// A free-form TOML string; Enter opens a one-line editor. Clearing the
    /// editor (empty text) removes the key, restoring the plugin's default.
    Text,
}

/// One choice of a [`SettingKind::Select`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingOption {
    /// The value written to `config.toml`.
    pub value: &'static str,
    /// Short label shown in the row.
    pub label: &'static str,
}

impl SettingOption {
    pub const fn new(value: &'static str, label: &'static str) -> Self {
        Self { value, label }
    }
}

/// One editable option.
///
/// Keys are relative to the owner: a plugin's key is a key of its
/// `[plugins."<id>"]` pocket, the host's is a path in `config.toml`
/// (`"status_bar.plugin_segments"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingField {
    pub key: &'static str,
    /// Row label, kept short — the row is one line.
    pub label: &'static str,
    /// One-line explanation shown in the help bar while the row is focused.
    pub description: &'static str,
    pub kind: SettingKind,
    /// Choices for [`SettingKind::Select`]; empty for other kinds.
    pub options: &'static [SettingOption],
    /// The value shown while `config.toml` has no entry for the key.
    pub default: Option<&'static str>,
    /// The value is read at startup and cannot be applied mid-run; the row
    /// carries a `restart` badge and the edit still persists.
    pub restart: bool,
}

impl SettingField {
    /// A boolean switch. `default_on` is what the row shows (and what the
    /// plugin should assume) while the key is absent.
    pub const fn toggle(
        key: &'static str,
        label: &'static str,
        description: &'static str,
        default_on: bool,
    ) -> Self {
        Self {
            key,
            label,
            description,
            kind: SettingKind::Boolean,
            options: &[],
            default: Some(if default_on { "true" } else { "false" }),
            restart: false,
        }
    }

    /// A pick-one-of list.
    pub const fn select(
        key: &'static str,
        label: &'static str,
        description: &'static str,
        options: &'static [SettingOption],
        default: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            description,
            kind: SettingKind::Select,
            options,
            default: Some(default),
            restart: false,
        }
    }

    /// A TOML integer.
    pub const fn number(
        key: &'static str,
        label: &'static str,
        description: &'static str,
        default: Option<&'static str>,
    ) -> Self {
        Self {
            key,
            label,
            description,
            kind: SettingKind::Number,
            options: &[],
            default,
            restart: false,
        }
    }

    /// A free-form string.
    pub const fn text(
        key: &'static str,
        label: &'static str,
        description: &'static str,
        default: Option<&'static str>,
    ) -> Self {
        Self {
            key,
            label,
            description,
            kind: SettingKind::Text,
            options: &[],
            default,
            restart: false,
        }
    }

    /// Mark the field as startup-only: the edit persists, but the running
    /// session keeps the value it booted with.
    pub const fn restart(mut self) -> Self {
        self.restart = true;
        self
    }

    /// The label of `value` for a select field, or the value itself when the
    /// stored text names no option (a stale value from an older build).
    pub fn option_label(&self, value: &str) -> String {
        self.options
            .iter()
            .find(|option| option.value == value)
            .map(|option| option.label.to_string())
            .unwrap_or_else(|| value.to_string())
    }

    /// The value a fresh row shows: the default, else the first option.
    pub fn initial_value(&self) -> Option<&'static str> {
        self.default
            .or_else(|| self.options.first().map(|option| option.value))
    }

    /// The next/previous select value (cycling), starting from `current`.
    /// `current` that names no option starts the cycle at its nearest end.
    pub fn cycle(&self, current: Option<&str>, forward: bool) -> Option<&'static str> {
        if self.options.is_empty() {
            return None;
        }
        let len = self.options.len();
        let index =
            current.and_then(|value| self.options.iter().position(|option| option.value == value));
        let next = match (index, forward) {
            (Some(index), true) => (index + 1) % len,
            (Some(index), false) => (index + len - 1) % len,
            (None, true) => 0,
            (None, false) => len - 1,
        };
        Some(self.options[next].value)
    }
}

/// One screen's worth of fields: what a plugin (or the host) offers to
/// `/settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsSection {
    /// Section heading — the owner's display name (`tps`, `codest`).
    pub title: &'static str,
    /// One-line explanation shown next to the heading.
    pub description: &'static str,
    pub fields: &'static [SettingField],
}

impl SettingsSection {
    pub const fn new(
        title: &'static str,
        description: &'static str,
        fields: &'static [SettingField],
    ) -> Self {
        Self {
            title,
            description,
            fields,
        }
    }

    /// The declared field named `key`, if any.
    pub fn field(&self, key: &str) -> Option<&SettingField> {
        self.fields.iter().find(|field| field.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static OPTIONS: [SettingOption; 2] = [
        SettingOption::new("auto", "Auto"),
        SettingOption::new("never", "Never"),
    ];

    static FIELDS: [SettingField; 3] = [
        SettingField::toggle("enabled", "Meter", "tokens per second", false),
        SettingField::select("mode", "Mode", "when to fold", &OPTIONS, "auto"),
        SettingField::number("window", "Window", "sample window", None).restart(),
    ];

    static SECTION: SettingsSection = SettingsSection::new("tps", "throughput meter", &FIELDS);

    #[test]
    fn fields_carry_kind_defaults_and_the_restart_flag() {
        assert_eq!(
            SECTION.field("enabled").unwrap().initial_value(),
            Some("false")
        );
        assert_eq!(SECTION.field("mode").unwrap().initial_value(), Some("auto"));
        assert_eq!(SECTION.field("window").unwrap().initial_value(), None);
        assert!(SECTION.field("window").unwrap().restart);
        assert!(!SECTION.field("mode").unwrap().restart);
        assert!(SECTION.field("absent").is_none());
    }

    #[test]
    fn selects_cycle_and_unknown_values_restart_the_cycle() {
        let field = SECTION.field("mode").unwrap();
        assert_eq!(field.cycle(Some("auto"), true), Some("never"));
        assert_eq!(field.cycle(Some("never"), true), Some("auto"));
        assert_eq!(field.cycle(Some("auto"), false), Some("never"));
        // A value no option names starts at the near end, not at a panic.
        assert_eq!(field.cycle(Some("stale"), true), Some("auto"));
        assert_eq!(field.cycle(Some("stale"), false), Some("never"));
        assert_eq!(field.cycle(None, true), Some("auto"));
        assert_eq!(field.option_label("never"), "Never");
        assert_eq!(field.option_label("stale"), "stale");
    }

    #[test]
    fn non_select_fields_have_no_cycle() {
        let field = SECTION.field("enabled").unwrap();
        assert_eq!(field.cycle(Some("true"), true), None);
    }
}
