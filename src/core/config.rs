//! Configuration files under `codest_home()` (see [`codest_home`]), kept as
//! separate files with separate concerns:
//!
//! * `config.toml` — global settings. Today that is one key, the default
//!   model selection; the file is reserved for future global options.
//! * `providers.toml` — the provider instances (endpoints, keys, models).
//!   Keeping instances in their own file lets the `/model` UI manage them
//!   later without touching global settings.
//! * `profiles.toml` — named session presets (a default selection, an
//!   initial effort, the plugin tools to keep, a system-prompt addition).
//!   A profile composes capabilities for one run; it declares no endpoints.
//!
//! All files are optional: a missing file contributes nothing and the
//! registry falls back to its built-in instance, which preserves the
//! pre-config CLI/env behavior. A file that exists but does not parse is a
//! startup error — a typo must not silently disable half the configuration.
//! Unknown keys are rejected for the same reason.
//!
//! Secret values accept a `$VAR` template ([`expand_env_template`]) so keys
//! can stay out of the files.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;
use crate::core::rollout::codest_home;

/// The provider vocabulary lives in the contract crate so an external plugin
/// can name it. The host keeps this path as the historical import site.
pub use cst_plugin_api::config::{
    DEFAULT_EFFORT_FIELD, DEFAULT_PROTOCOL, DEFAULT_THINKING_FIELD, EFFORT_FIELD_NONE, ModelInfo,
    ProviderInstance, WireQuirks, expand_env_template, toml_table_to_json_map, toml_to_json,
};

/// `config.toml` — global settings.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    /// Initial model selection, `"provider/model"`.
    pub default: Option<String>,
}

/// The `/profile` command: with a name, switch this run's active profile;
/// without one, list the available profiles. Like the session and model
/// commands, the spec only gives every front-end a shared, parseable shape;
/// the behavior is a front-end concern.
pub static PROFILE_COMMAND: CommandSpec = CommandSpec::new(
    "profile",
    &[],
    "/profile [name]",
    "show the available profiles, or switch to one (assembly-time values \
     — tools and prompt — are set at startup)",
    ArgsPolicy::Optional,
);

/// Merged into the shipped command table next to the session commands; no
/// plugin owns it.
pub static PROFILE_COMMANDS: &[CommandSpec] = &[PROFILE_COMMAND];

/// `profiles.toml` — the named session presets, keyed by profile name. A
/// `BTreeMap` so listing and validation see one deterministic order.
pub type ProfileSet = BTreeMap<String, Profile>;

/// One profile: a named preset applied when a run starts — a default model
/// selection, an initial reasoning effort, the plugin tools to keep, and a
/// system-prompt addition. Values are *presets*, not a second configuration
/// layer: CLI flags still win over any of them, and a restored session
/// restores its own state instead.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Initial model selection, `"provider/model"`.
    pub default: Option<String>,
    /// Initial reasoning effort, as `/effort <level>` would set it. Applied
    /// verbatim — the vocabulary is the active model's to declare.
    pub effort: Option<String>,
    /// Plugin ids whose tools are declared to the model. `None` keeps every
    /// plugin's tools; a listed plugin's tools stay on and every other
    /// plugin's tools are not sent (an empty list strips all tools — for
    /// models without function calling). Slash commands, hooks, and prompt
    /// fragments are unaffected either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// System-prompt addition for this profile.
    #[serde(default)]
    pub prompt: ProfilePrompt,
}

/// The system-prompt part of a [`Profile`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilePrompt {
    /// Appended after the base prompt and the plugin fragments — the most
    /// specific text wins the last word.
    pub append: Option<String>,
}

impl Profile {
    /// The selection this profile starts with, if any.
    pub fn default_selection(&self) -> Option<&str> {
        self.default.as_deref()
    }
}

/// `~/.codest/profiles.toml`; missing or empty file → no profiles.
pub fn load_profiles(path: &Path) -> Result<ProfileSet, ConfigError> {
    let content = read_optional(path)?;
    match content {
        None => Ok(ProfileSet::new()),
        Some(content) => {
            let file: ProfilesFile =
                toml::from_str(&content).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok(file.profiles)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesFile {
    #[serde(default)]
    profiles: ProfileSet,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot parse {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: std::io::Error },
    #[error("cannot serialize {path}: {message}")]
    Serialize { path: PathBuf, message: String },
}

/// `~/.codest/config.toml`; missing file → empty settings.
pub fn load_global(path: &Path) -> Result<GlobalConfig, ConfigError> {
    let content = read_optional(path)?;
    match content {
        None => Ok(GlobalConfig::default()),
        Some(content) => toml::from_str(&content).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// `~/.codest/providers.toml`; missing or empty file → no instances.
pub fn load_providers(path: &Path) -> Result<Vec<ProviderInstance>, ConfigError> {
    let content = read_optional(path)?;
    match content {
        None => Ok(Vec::new()),
        Some(content) => {
            let file: ProvidersFile =
                toml::from_str(&content).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok(file.providers)
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProvidersFile {
    #[serde(default)]
    providers: Vec<ProviderInstance>,
}

/// Write `instances` back to `path` as `[[providers]]` entries.
///
/// This is the write half the `/provider` wizard needs. Two consequences are
/// deliberate:
///
/// * **Comments and layout are lost.** The file becomes machine-managed after
///   the first write (no `toml_edit` dependency for in-place edits). Users who
///   want hand-formatted files should keep editing them by hand — the wizard
///   only writes when it is explicitly asked to add a provider.
/// * **The write is validated and atomic.** [`write_checked`] rejects a set the
///   registry rules would reject, and the file is replaced via a temporary
///   sibling + rename so a failure never leaves a half-written config.
pub fn save_providers(path: &Path, instances: &[ProviderInstance]) -> Result<(), ConfigError> {
    // `body_overrides` is a raw JSON map, and JSON can carry `null` where TOML
    // cannot. Drop nulls rather than failing the write over a value that would
    // never round-trip through TOML in the first place.
    let sanitized: Vec<ProviderInstance> = instances
        .iter()
        .map(|instance| {
            let mut instance = instance.clone();
            instance.body_overrides.retain(|_, value| !value.is_null());
            instance.options.retain(|_, value| !value.is_null());
            instance
        })
        .collect();
    let file = ProvidersFile {
        providers: sanitized,
    };
    let text = toml::to_string_pretty(&file).map_err(|source| ConfigError::Serialize {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    write_checked(path, &text)
}

/// Replace `path` with `text` through a temporary sibling.
///
/// Shared with the plugin installer, which writes its own state file and the
/// generated wrapper manifests under `~/.codest/`.
pub(crate) fn write_checked(path: &Path, text: &str) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text).map_err(|source| ConfigError::Write {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn read_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// `~/.codest/config.toml`.
pub fn config_path() -> PathBuf {
    codest_home().join("config.toml")
}

/// `~/.codest/providers.toml`.
pub fn providers_path() -> PathBuf {
    codest_home().join("providers.toml")
}

/// `~/.codest/profiles.toml`.
pub fn profiles_path() -> PathBuf {
    codest_home().join("profiles.toml")
}

/// `~/.codest/model-cache.json` — the fetched model lists.
pub fn model_cache_path() -> PathBuf {
    codest_home().join("model-cache.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "codest-config-{}-{:x}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, content).expect("write temp config");
        path
    }

    #[test]
    fn missing_files_are_empty_defaults() {
        let missing = std::env::temp_dir().join("codest-config-does-not-exist.toml");
        assert_eq!(load_global(&missing).unwrap(), GlobalConfig::default());
        assert!(load_providers(&missing).unwrap().is_empty());
    }

    #[test]
    fn parses_global_and_provider_files() {
        let global = write_temp("global.toml", "default = \"glm/glm-5.3\"\n");
        assert_eq!(
            load_global(&global).unwrap(),
            GlobalConfig {
                default: Some("glm/glm-5.3".to_string())
            }
        );

        let providers = write_temp(
            "providers.toml",
            r#"
[[providers]]
name = "glm"
base_url = "https://api.example.com/v1"
api_key = "$GLM_KEY"
fetch_models = true

[[providers.models]]
id = "glm-5.3"
name = "GLM-5.3"
context_window = 200000
reasoning = ["low", "high"]
default_level = "high"

[[providers]]
name = "local"
protocol = "test"
api_key = "none"
headers = { "X-A" = "1" }

[providers.body_overrides]
temperature = 0.5
nested = { deep = [1, "two", true] }
"#,
        );
        let instances = load_providers(&providers).unwrap();
        assert_eq!(instances.len(), 2);

        assert_eq!(instances[0].name, "glm");
        assert_eq!(instances[0].protocol_id(), DEFAULT_PROTOCOL);
        assert_eq!(instances[0].models.len(), 1);
        assert_eq!(instances[0].models[0].display_name(), "GLM-5.3");
        assert_eq!(instances[0].models[0].context_window, Some(200000));
        assert_eq!(instances[0].models[0].default_level.as_deref(), Some("high"));
        assert!(instances[0].should_fetch_models());

        assert_eq!(instances[1].protocol_id(), "test");
        assert_eq!(instances[1].headers["X-A"], "1");
        assert_eq!(instances[1].body_overrides["temperature"], serde_json::json!(0.5));
        assert_eq!(
            instances[1].body_overrides["nested"],
            serde_json::json!({"deep": [1, "two", true]})
        );
        // The `local` instance declares no models, so discovery defaults on.
        assert!(instances[1].should_fetch_models());
    }

    #[test]
    fn fetch_models_defaults_to_true_only_without_declared_models() {
        let path = write_temp(
            "fetch-defaults.toml",
            r#"
[[providers]]
name = "fetching"

[[providers]]
name = "static"
models = [{ id = "m1" }]

[[providers]]
name = "forced-off"
fetch_models = false
"#,
        );
        let instances = load_providers(&path).unwrap();
        assert!(instances[0].should_fetch_models());
        assert!(!instances[1].should_fetch_models());
        assert!(!instances[2].should_fetch_models());
    }

    #[test]
    fn wire_quirks_parse_or_default() {
        // No wire table: the client falls back to the built-in spellings.
        let path = write_temp(
            "no-wire.toml",
            r#"
[[providers]]
name = "plain"
base_url = "https://api.example.com/v1"
"#,
        );
        let instances = load_providers(&path).unwrap();
        assert!(instances[0].wire.is_none());

        // Explicit quirks parse as declared.
        let path = write_temp(
            "wire.toml",
            r#"
[[providers]]
name = "quirky"
base_url = "https://api.example.com/v1"

[providers.wire]
thinking_fields = ["reasoning", "thinking"]
effort_field = "reasoning"
"#,
        );
        let instances = load_providers(&path).unwrap();
        let wire = instances[0].wire.as_ref().expect("wire parsed");
        assert_eq!(
            wire.thinking_fields,
            vec!["reasoning".to_string(), "thinking".to_string()]
        );
        assert_eq!(wire.effort_field, "reasoning");

        // Omitted keys fall back to the defaults, not to empty.
        let path = write_temp(
            "wire-partial.toml",
            r#"
[[providers]]
name = "stiff"
base_url = "https://api.example.com/v1"

[providers.wire]
effort_field = "none"
"#,
        );
        let instances = load_providers(&path).unwrap();
        let wire = instances[0].wire.as_ref().expect("wire parsed");
        assert_eq!(wire.effort_field, EFFORT_FIELD_NONE);
        assert_eq!(wire.thinking_fields, vec![DEFAULT_THINKING_FIELD.to_string()]);

        // Unknown keys inside [wire] are rejected, like everywhere else.
        let path = write_temp(
            "wire-unknown.toml",
            r#"
[[providers]]
name = "typo"
base_url = "https://api.example.com/v1"

[providers.wire]
effort_filed = "reasoning"
"#,
        );
        assert!(load_providers(&path).is_err(), "unknown wire keys are rejected");
    }

    #[test]
    fn wire_quirks_validate_rejects_empty_field_names() {
        let quirks = WireQuirks {
            thinking_fields: vec!["  ".to_string()],
            ..Default::default()
        };
        assert!(quirks.validate().is_err());

        let quirks = WireQuirks {
            effort_field: " ".to_string(),
            ..Default::default()
        };
        assert!(quirks.validate().is_err());

        // An empty thinking list is a deliberate opt-out, not an error.
        let quirks = WireQuirks {
            thinking_fields: Vec::new(),
            effort_field: "thinking".to_string(),
        };
        assert!(quirks.validate().is_ok(), "free-form field names are the point");
        assert!(WireQuirks::default().validate().is_ok());
    }

    #[test]
    fn profiles_parse_or_default() {
        // Missing file: no profiles at all.
        let missing = std::env::temp_dir().join("codest-profiles-does-not-exist.toml");
        assert!(load_profiles(&missing).unwrap().is_empty());

        let path = write_temp(
            "profiles.toml",
            r#"
[profiles.code]
default = "glm/glm-5.3"
effort = "high"
tools = ["prompt_debug"]

[profiles.code.prompt]
append = "你是资深 Rust 工程师…"

[profiles.bare]
"#,
        );
        let profiles = load_profiles(&path).unwrap();
        assert_eq!(profiles.len(), 2);

        let code = &profiles["code"];
        assert_eq!(code.default_selection(), Some("glm/glm-5.3"));
        assert_eq!(code.effort.as_deref(), Some("high"));
        assert_eq!(code.tools.as_deref(), Some(&["prompt_debug".to_string()][..]));
        assert_eq!(code.prompt.append.as_deref(), Some("你是资深 Rust 工程师…"));

        // A bare profile is all defaults; `tools` absent means keep them all.
        let bare = &profiles["bare"];
        assert_eq!(bare.default_selection(), None);
        assert_eq!(bare.effort, None);
        assert_eq!(bare.tools, None);
        assert_eq!(bare.prompt.append, None);

        // Unknown keys are rejected, both at the profile and prompt level.
        let path = write_temp("profiles-unknown.toml", "[profiles.x]\nnope = 1\n");
        assert!(load_profiles(&path).is_err());
        let path = write_temp("profiles-unknown-prompt.toml", "[profiles.x.prompt]\nnope = 1\n");
        assert!(load_profiles(&path).is_err());
    }

    #[test]
    fn parse_errors_name_the_file() {
        let path = write_temp("broken.toml", "default = \n");
        let error = load_global(&path).unwrap_err();
        assert!(error.to_string().contains("broken.toml"), "{error}");

        let path = write_temp("unknown.toml", "default = \"a\"\nnope = 1\n");
        assert!(load_global(&path).is_err(), "unknown keys are rejected");

        // Global settings do not belong in providers.toml — the `default`
        // selection lives in config.toml only.
        let path = write_temp("misplaced.toml", "default = \"a/b\"\n");
        assert!(load_providers(&path).is_err());
    }

    #[test]
    fn env_templates_expand_literals_pass_through() {
        // Literal values are untouched.
        assert_eq!(expand_env_template("sk-test"), Some("sk-test".to_string()));
        assert_eq!(expand_env_template("$"), Some("$".to_string()));

        // Unset variables resolve to nothing.
        assert_eq!(expand_env_template("$CODEST_NO_SUCH_VAR_XYZ"), None);

        // Set variables resolve, trimmed; an empty value counts as unset.
        unsafe { std::env::set_var("CODEST_TEST_VAR_XYZ", "  secret  ") };
        assert_eq!(
            expand_env_template("$CODEST_TEST_VAR_XYZ"),
            Some("secret".to_string())
        );
        unsafe { std::env::set_var("CODEST_TEST_EMPTY_XYZ", "   ") };
        assert_eq!(expand_env_template("$CODEST_TEST_EMPTY_XYZ"), None);
    }

    /// The wizard's write path: what it saves must load back identically,
    /// including the plugin-private `[providers.options]` pocket.
    #[test]
    fn save_providers_round_trips_through_the_loader() {
        let path = std::env::temp_dir().join(format!(
            "codest-config-{}-save/providers.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
        let instances = vec![
            ProviderInstance {
                name: "qoder".to_string(),
                protocol: Some("qoder".to_string()),
                base_url: Some("https://api3.qoder.sh/".to_string()),
                api_key: None,
                models: vec![ModelInfo::from_id("auto")],
                fetch_models: Some(true),
                headers: Default::default(),
                body_overrides: Default::default(),
                wire: None,
                options: [(
                    "open_api_url".to_string(),
                    serde_json::json!("https://openapi.qoder.sh"),
                )]
                .into_iter()
                .collect(),
                requires_api_key: false,
            },
            ProviderInstance {
                name: "local".to_string(),
                protocol: None,
                base_url: Some("http://127.0.0.1:8080/v1".to_string()),
                api_key: Some("$LOCAL_KEY".to_string()),
                models: Vec::new(),
                fetch_models: None,
                headers: [("X-Test".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
                body_overrides: [("temperature".to_string(), serde_json::json!(0.2))]
                    .into_iter()
                    .collect(),
                wire: None,
                options: Default::default(),
                requires_api_key: false,
            },
        ];

        save_providers(&path, &instances).expect("write");
        // The parent directory is created, not required to exist.
        assert!(path.exists());
        assert_eq!(load_providers(&path).expect("read back"), instances);

        // A second write replaces the file rather than appending to it.
        save_providers(&path, &instances[1..]).expect("rewrite");
        let loaded = load_providers(&path).expect("read back");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "local");

        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    /// JSON can carry `null`; TOML cannot. A null in `body_overrides` is
    /// dropped instead of failing the whole write.
    #[test]
    fn save_providers_drops_null_overrides() {
        let path = std::env::temp_dir().join(format!(
            "codest-config-{}-null/providers.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
        let instance = ProviderInstance {
            name: "x".to_string(),
            protocol: None,
            base_url: None,
            api_key: None,
            models: Vec::new(),
            fetch_models: None,
            headers: Default::default(),
            body_overrides: [
                ("keep".to_string(), serde_json::json!(1)),
                ("drop".to_string(), serde_json::Value::Null),
            ]
            .into_iter()
            .collect(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        save_providers(&path, &[instance]).expect("write");
        let loaded = load_providers(&path).expect("read back");
        assert_eq!(
            loaded[0].body_overrides.get("keep"),
            Some(&serde_json::json!(1))
        );
        assert_eq!(loaded[0].body_overrides.get("drop"), None);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }
}
