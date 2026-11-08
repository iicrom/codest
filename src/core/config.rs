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
use serde_json::Value;

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;
use crate::core::rollout::codest_home;

/// The protocol kind assumed for instances that do not name one.
pub const DEFAULT_PROTOCOL: &str = "openai-compat";

/// The delta field read as reasoning text by default (the DeepSeek spelling).
pub const DEFAULT_THINKING_FIELD: &str = "reasoning_content";
/// The request field that carries the per-turn effort by default (the
/// OpenAI-standard top-level spelling).
pub const DEFAULT_EFFORT_FIELD: &str = "reasoning_effort";
/// The `wire.effort_field` value that never sends the per-turn effort, for
/// endpoints that reject unknown request fields outright.
pub const EFFORT_FIELD_NONE: &str = "none";

/// `config.toml` — global settings.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    /// Initial model selection, `"provider/model"`.
    pub default: Option<String>,
}

/// One model of a provider: an id plus optional display metadata. The fields
/// beyond `id` are advisory — `reasoning` and `default_level` feed `/effort`,
/// the rest await consumers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    /// Reasoning effort levels the model accepts, lowest first.
    pub reasoning: Option<Vec<String>>,
    /// The reasoning effort this model prefers when none is set explicitly.
    /// Adopted when the model is selected (startup or `/model`) until
    /// `/effort` overrides it. Accepted from providers.toml and the models
    /// API alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_level: Option<String>,
}

impl ModelInfo {
    /// The id reduced to a bare `ModelInfo`, as produced by models discovery.
    pub fn from_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            context_window: None,
            reasoning: None,
            default_level: None,
        }
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

/// Per-instance wire quirks for the `openai-compat` protocol: the
/// field-spelling variations providers actually disagree on, lifted out of
/// code into data. Zero configuration reproduces the built-in behavior, so
/// only the endpoint that needs it pays.
///
/// The vocabulary is deliberately small and free-form: providers invent new
/// spellings faster than an enum can follow, and a field name is data, not
/// logic. `WireQuirks` covers only the two points where the wire shape has no
/// safe fallback — which delta field carries reasoning text, and where the
/// per-turn effort goes. Everything else (usage token spellings, context
/// window spellings, request body fixes) is already tolerated in code or
/// expressible through [`ProviderInstance::body_overrides`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WireQuirks {
    /// Delta fields tried in order as reasoning text (first hit wins). An
    /// empty list disables thinking parsing for the instance.
    pub thinking_fields: Vec<String>,
    /// The top-level request field that carries the per-turn effort selected
    /// with `/effort`. [`EFFORT_FIELD_NONE`] never sends the field.
    pub effort_field: String,
}

impl Default for WireQuirks {
    fn default() -> Self {
        Self {
            thinking_fields: vec![DEFAULT_THINKING_FIELD.to_string()],
            effort_field: DEFAULT_EFFORT_FIELD.to_string(),
        }
    }
}

impl WireQuirks {
    /// Startup-time sanity checks: field names must be non-empty. Values
    /// outside the documented vocabulary are allowed on purpose — a
    /// provider's novel spelling is exactly what this layer is for.
    pub fn validate(&self) -> Result<(), String> {
        if self.effort_field.trim().is_empty() {
            return Err("wire.effort_field must not be empty".to_string());
        }
        for field in &self.thinking_fields {
            if field.trim().is_empty() {
                return Err("wire.thinking_fields entries must not be empty".to_string());
            }
        }
        Ok(())
    }
}

/// One provider instance: an endpoint configuration bound to a protocol kind
/// ([`ProviderInstance::protocol_id`]). This is pure config data — turning it
/// into a client is the registry's job.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInstance {
    /// Unique instance name; also the `provider` half of a selection. Must
    /// not contain `/` (selections parse at the first slash).
    pub name: String,
    /// Protocol kind id; defaults to [`DEFAULT_PROTOCOL`].
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    /// API key or `$VAR` template. An empty/unresolved key fails the turn
    /// with `NotConfigured` instead of failing startup, so a partial
    /// providers.toml still boots.
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    /// Fetch the model list from the provider's models API. Defaults to true
    /// for instances that declare no models and false otherwise.
    pub fetch_models: Option<bool>,
    /// Static headers sent with every request (provider quirks, disguises).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// JSON fields merged over the request body; overrides win.
    #[serde(default, deserialize_with = "toml_table_to_json_map")]
    pub body_overrides: serde_json::Map<String, Value>,
    /// Wire quirks for the `openai-compat` protocol, baked into the client at
    /// construction. `None` uses the built-in spellings
    /// (`thinking_fields = [reasoning_content]`, `effort_field =
    /// "reasoning_effort"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<WireQuirks>,
    /// Set only for instances synthesized from partial CLI/env input, where
    /// `base_url` and `api_key` are expected as a pair: the openai-compat
    /// factory gates such an instance at use time with a completion prompt
    /// instead of sending a doomed request. Config-declared instances may
    /// legitimately omit `api_key` (keyless endpoints), so they are never
    /// gated for it.
    #[serde(skip)]
    pub requires_api_key: bool,
}

impl ProviderInstance {
    pub fn protocol_id(&self) -> &str {
        self.protocol.as_deref().unwrap_or(DEFAULT_PROTOCOL)
    }

    /// Whether models discovery should run for this instance.
    pub fn should_fetch_models(&self) -> bool {
        self.fetch_models.unwrap_or(self.models.is_empty())
    }

    /// The key resolved from its `$VAR` template, if any. `None` means the
    /// instance carries no key (or its template is unset); `Some("")` is
    /// impossible by construction.
    pub fn resolved_api_key(&self) -> Option<String> {
        self.api_key.as_deref().and_then(expand_env_template)
    }

    /// Whether the base URL is usable for a request.
    pub fn has_base_url(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty())
    }
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvidersFile {
    #[serde(default)]
    providers: Vec<ProviderInstance>,
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

/// Expand a `$VAR` template. A literal (no leading `$`) passes through;
/// `$VAR` reads the environment, and an unset or empty variable yields `None`
/// so callers can report the missing template instead of sending garbage.
pub fn expand_env_template(value: &str) -> Option<String> {
    match value.strip_prefix('$') {
        None => Some(value.to_string()),
        Some("") => Some(value.to_string()),
        Some(var) => std::env::var(var)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
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

/// Deserialize a TOML table into a `serde_json` map. TOML is JSON-compatible
/// apart from datetimes, which render as their RFC 3339 string.
fn toml_table_to_json_map<'de, D>(deserializer: D) -> Result<serde_json::Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let table = toml::Table::deserialize(deserializer)?;
    Ok(toml_to_json(&toml::Value::Table(table))
        .as_object()
        .cloned()
        .unwrap_or_default())
}

fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => Value::from(*number),
        toml::Value::Boolean(flag) => Value::from(*flag),
        toml::Value::Datetime(stamp) => Value::String(stamp.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => {
            Value::Object(table.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect())
        }
    }
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
}
