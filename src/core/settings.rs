//! `/settings`: the settings screen's storage layer.
//!
//! The screen edits `config.toml` in place. Two kinds of section share it:
//!
//! * the host's own settings, addressed by a dotted key path from the root
//!   (`status_bar.plugin_segments`);
//! * one section per plugin, whose keys are the keys of that plugin's
//!   `[plugins."<id>"]` pocket — the same pocket
//!   [`crate::plugins::PluginContext::plugin_settings`] hands to the plugin,
//!   so a value written here is what the next startup reads.
//!
//! A write is validated before it lands: the rendered file is parsed back into
//! a [`GlobalConfig`] (strict, `deny_unknown_fields`), and only a document that
//! parses is written — atomically, through [`crate::core::config`]'s
//! temporary-sibling rename. Comments and hand formatting are lost the first
//! time a setting is saved, exactly like the `/provider` wizard's writes; the
//! file is machine-managed from then on.
//!
//! Plugins never touch files (see [`cst_plugin_api::config`]); the screen asks
//! the plugin for a [`SettingsSection`] and this module owns every byte on
//! disk.

use std::path::Path;
use std::path::PathBuf;

use cst_plugin_api::settings::SettingField;
use cst_plugin_api::settings::SettingKind;
use cst_plugin_api::settings::SettingsSection;

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;
use crate::core::config;
use crate::core::config::ConfigError;
use crate::core::config::GlobalConfig;

/// `/settings`: the screen edits `config.toml`. The spec only gives every
/// front-end a shared, parseable shape; the screen itself is a front-end
/// concern (this crate's TUI renders it).
pub static SETTINGS_COMMAND: CommandSpec = CommandSpec::new(
    "settings",
    &[],
    "/settings [section]",
    "open the settings screen (host and plugin options, saved to config.toml)",
    ArgsPolicy::Optional,
);

/// Settings commands merged into the shipped command table; no plugin owns
/// them, so front-ends dispatch them before asking the plugin host.
pub static SETTINGS_COMMANDS: &[CommandSpec] = &[SETTINGS_COMMAND];

/// The host's own settings section — what `/settings` shows besides the
/// plugins. Keys are dotted paths in `config.toml`.
static HOST_FIELDS: [SettingField; 1] = [SettingField::toggle(
    "status_bar.plugin_segments",
    "Plugin status segments",
    "Draw plugin-contributed footer segments (tps rate and sparkline, …). \
     The host's own readouts never hide.",
    true,
)];

pub static HOST_SETTINGS: SettingsSection =
    SettingsSection::new("codest", "host settings", &HOST_FIELDS);

/// Which table of `config.toml` one section edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingOwner {
    /// The host's own settings, addressed by a dotted key path from the root.
    Host,
    /// One plugin's `[plugins."<id>"]` pocket.
    Plugin(&'static str),
}

impl SettingOwner {
    /// The table the section's keys live in.
    fn base(&self) -> Vec<&str> {
        match self {
            SettingOwner::Host => Vec::new(),
            SettingOwner::Plugin(id) => vec!["plugins", id],
        }
    }
}

/// One value as the screen writes it: the field's [`SettingKind`] decides the
/// TOML type it lands as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    Bool(bool),
    Integer(i64),
    Text(String),
}

impl SettingValue {
    /// Parse the editor's text into a value of `kind`, refusing input that
    /// would not read back as the same thing.
    pub fn parse(field: &SettingField, text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        match field.kind {
            SettingKind::Boolean => match trimmed {
                "true" | "on" | "yes" => Ok(Self::Bool(true)),
                "false" | "off" | "no" => Ok(Self::Bool(false)),
                other => Err(format!("`{other}` is not a boolean (true / false)")),
            },
            // Non-negative only: the readers a plugin reaches for
            // (`PluginSettings::option_u64`) return `None` for a negative
            // TOML integer, so storing one would be a silent no-op at the
            // next read rather than the value the user typed.
            SettingKind::Number => match trimmed.parse::<u64>() {
                Ok(number) => Ok(Self::Integer(
                    i64::try_from(number).map_err(|_| format!("`{trimmed}` is too large"))?,
                )),
                Err(_) => Err(format!("`{trimmed}` is not a non-negative integer")),
            },
            SettingKind::Select => {
                if field.options.iter().any(|option| option.value == trimmed) {
                    Ok(Self::Text(trimmed.to_string()))
                } else {
                    let known = field
                        .options
                        .iter()
                        .map(|option| option.value)
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(format!("`{trimmed}` is not one of: {known}"))
                }
            }
            SettingKind::Text => Ok(Self::Text(text.to_string())),
        }
    }

    fn to_toml(&self) -> toml::Value {
        match self {
            SettingValue::Bool(flag) => toml::Value::Boolean(*flag),
            SettingValue::Integer(number) => toml::Value::Integer(*number),
            SettingValue::Text(text) => toml::Value::String(text.clone()),
        }
    }
}

/// `~/.codest/config.toml` held as a raw TOML table.
///
/// The typed [`GlobalConfig`] is the app's view of the same file; this keeps
/// the document the screen edits, so a save rewrites only what changed and
/// re-parses the whole thing before it lands.
pub struct SettingsFile {
    path: PathBuf,
    table: toml::Table,
}

impl SettingsFile {
    /// Read `path`. A missing file is an empty document — the first save
    /// creates it.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let table = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(Self { path, table })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored text of one setting — what its row shows. `None` when the
    /// key is absent, so the row falls back to the field's default.
    pub fn text(&self, owner: SettingOwner, key: &str) -> Option<String> {
        let (table, leaf) = self.locate(owner, key)?;
        table.get(&leaf).map(display_value)
    }

    /// Write one setting, or remove the key when `value` is `None`, then
    /// return the re-read [`GlobalConfig`].
    ///
    /// The write is atomic and validated: the rendered document is parsed
    /// back first, so a value the strict loader would reject is an error
    /// instead of a broken `config.toml`. Removing a plugin pocket's last key
    /// drops the table itself, so no empty `[plugins."<id>"]` header lingers;
    /// a removal that would leave every other table intact leaves it intact.
    pub fn write(
        &mut self,
        owner: SettingOwner,
        key: &str,
        value: Option<SettingValue>,
    ) -> Result<GlobalConfig, ConfigError> {
        let mut segments = owner.base();
        let key_path: Vec<&str> = key.split('.').filter(|part| !part.is_empty()).collect();
        if key_path.is_empty() {
            return Err(ConfigError::Serialize {
                path: self.path.clone(),
                message: format!("`{key}` is not a valid setting key"),
            });
        }
        let (leaf, parents) = key_path.split_last().expect("checked non-empty");
        segments.extend_from_slice(parents);

        // Mutate a copy: a rejected document must leave the file and this
        // document exactly as they were.
        let mut table = self.table.clone();
        match value {
            Some(value) => {
                let target = ensure_table(&mut table, &segments, &self.path)?;
                target.insert((*leaf).to_string(), value.to_toml());
            }
            None => {
                match table_at_mut(&mut table, &segments) {
                    Some(target) => {
                        target.remove(*leaf);
                        prune(&mut table, &segments);
                    }
                    // Nothing stored: the document is already the answer.
                    None => return config::load_global(&self.path.clone()),
                }
            }
        }

        let text = render(&table, &self.path)?;
        let global: GlobalConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source,
        })?;
        config::write_checked(&self.path, &text)?;
        self.table = table;
        Ok(global)
    }

    /// The table and leaf key one setting lives at.
    fn locate(&self, owner: SettingOwner, key: &str) -> Option<(&toml::Table, String)> {
        let mut segments = owner.base();
        let key_path: Vec<&str> = key.split('.').filter(|part| !part.is_empty()).collect();
        let (leaf, parents) = key_path.split_last()?;
        segments.extend_from_slice(parents);
        Some((table_at(&self.table, &segments)?, (*leaf).to_string()))
    }
}

fn render(table: &toml::Table, path: &Path) -> Result<String, ConfigError> {
    toml::to_string_pretty(table).map_err(|source| ConfigError::Serialize {
        path: path.to_path_buf(),
        message: source.to_string(),
    })
}

/// How a stored value reads in a row: strings verbatim, everything else in
/// its TOML form (`true`, `64`, `["a", "b"]`).
fn display_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn table_at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Table> {
    match path.split_first() {
        None => Some(table),
        Some((segment, rest)) => table_at(table.get(*segment)?.as_table()?, rest),
    }
}

fn table_at_mut<'a>(table: &'a mut toml::Table, path: &[&str]) -> Option<&'a mut toml::Table> {
    match path.split_first() {
        None => Some(table),
        Some((segment, rest)) => table_at_mut(table.get_mut(*segment)?.as_table_mut()?, rest),
    }
}

/// The table at `path`, creating missing levels. An existing non-table value
/// on the way is an error rather than something to overwrite: it is the
/// user's data.
fn ensure_table<'a>(
    table: &'a mut toml::Table,
    path: &[&str],
    file: &Path,
) -> Result<&'a mut toml::Table, ConfigError> {
    match path.split_first() {
        None => Ok(table),
        Some((segment, rest)) => {
            let entry = table
                .entry((*segment).to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            let Some(child) = entry.as_table_mut() else {
                return Err(ConfigError::Serialize {
                    path: file.to_path_buf(),
                    message: format!("`{segment}` is not a table"),
                });
            };
            ensure_table(child, rest, file)
        }
    }
}

/// Drop tables along `path` that a removal left empty, deepest first.
fn prune(table: &mut toml::Table, path: &[&str]) {
    let Some((segment, rest)) = path.split_first() else {
        return;
    };
    let empty = match table.get_mut(*segment).and_then(toml::Value::as_table_mut) {
        Some(child) => {
            prune(child, rest);
            child.is_empty()
        }
        None => false,
    };
    if empty {
        table.remove(*segment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "codest-settings-{}-{:x}-{name}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    static OPTIONS: [cst_plugin_api::settings::SettingOption; 2] = [
        cst_plugin_api::settings::SettingOption::new("light", "Light"),
        cst_plugin_api::settings::SettingOption::new("full", "Full"),
    ];

    static FIELDS: [SettingField; 3] = [
        SettingField::toggle("enabled", "Meter", "…", false),
        SettingField::number("window_ms", "Window", "…", None),
        SettingField::select("frame", "Frame", "…", &OPTIONS, "light"),
    ];

    #[test]
    fn values_parse_by_kind_and_reject_garbage() {
        assert_eq!(
            SettingValue::parse(&FIELDS[0], "on"),
            Ok(SettingValue::Bool(true))
        );
        assert!(SettingValue::parse(&FIELDS[0], "maybe").is_err());
        assert_eq!(
            SettingValue::parse(&FIELDS[1], " 250 "),
            Ok(SettingValue::Integer(250))
        );
        assert!(SettingValue::parse(&FIELDS[1], "2.5").is_err());
        // A negative value would read back as `None` through `option_u64`, so
        // it is refused before it can be stored.
        assert!(SettingValue::parse(&FIELDS[1], "-1").is_err());
        assert_eq!(
            SettingValue::parse(&FIELDS[2], "full"),
            Ok(SettingValue::Text("full".to_string()))
        );
        assert!(SettingValue::parse(&FIELDS[2], "rounded").is_err());
    }

    #[test]
    fn a_plugin_pocket_round_trips_and_prunes_when_empty() {
        let path = temp_path("plugin-pocket");
        let mut file = SettingsFile::load(&path).expect("empty document");
        assert_eq!(file.text(SettingOwner::Plugin("tps"), "enabled"), None);

        // The first write creates the file and the pocket.
        let global = file
            .write(
                SettingOwner::Plugin("tps"),
                "enabled",
                Some(SettingValue::Bool(true)),
            )
            .expect("write");
        assert_eq!(
            global.plugins["tps"].option_bool("enabled"),
            Some(true),
            "the typed view sees the write immediately"
        );
        assert_eq!(
            file.text(SettingOwner::Plugin("tps"), "enabled").as_deref(),
            Some("true")
        );

        // Other pockets and the host tables survive a second write.
        file.write(
            SettingOwner::Host,
            "status_bar.plugin_segments",
            Some(SettingValue::Bool(false)),
        )
        .expect("host write");
        file.write(
            SettingOwner::Plugin("tps"),
            "window_ms",
            Some(SettingValue::Integer(250)),
        )
        .expect("second plugin write");
        let global = config::load_global(&path).expect("parse back");
        assert!(!global.status_bar.plugin_segments);
        assert_eq!(global.plugins["tps"].option_u64("window_ms"), Some(250));
        assert_eq!(global.plugins["tps"].option_bool("enabled"), Some(true));

        // Removing the last key drops the pocket header entirely.
        file.write(SettingOwner::Plugin("tps"), "enabled", None)
            .expect("remove");
        file.write(SettingOwner::Plugin("tps"), "window_ms", None)
            .expect("remove");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("[plugins"), "{text}");
        let global = config::load_global(&path).expect("parse back");
        assert!(global.plugins.is_empty());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_write_that_would_not_parse_is_refused_and_the_file_stays() {
        let path = temp_path("reject");
        std::fs::write(&path, "default = \"a/b\"\n").expect("seed");
        let mut file = SettingsFile::load(&path).expect("load");

        // A host key outside the document's vocabulary must fail the parse
        // check rather than land as a startup error on the next run.
        assert!(
            file.write(
                SettingOwner::Host,
                "status_bar.nonsense",
                Some(SettingValue::Bool(true)),
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "default = \"a/b\"\n"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_file_reads_as_defaults_and_keeps_every_other_table() {
        let path = temp_path("preserve");
        std::fs::write(
            &path,
            "default = \"a/b\"\nmax_tool_rounds = 8\n\n[status_bar]\norder = [\"tps:rate\"]\n",
        )
        .expect("seed");
        let mut file = SettingsFile::load(&path).expect("load");
        file.write(
            SettingOwner::Plugin("tps"),
            "enabled",
            Some(SettingValue::Bool(true)),
        )
        .expect("write");
        let global = config::load_global(&path).expect("parse back");
        assert_eq!(global.default.as_deref(), Some("a/b"));
        assert_eq!(global.max_tool_rounds, Some(8));
        assert_eq!(global.status_bar.order, ["tps:rate"]);
        assert_eq!(global.plugins["tps"].option_bool("enabled"), Some(true));
        std::fs::remove_file(&path).ok();
    }
}
