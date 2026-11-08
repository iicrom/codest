//! Configuration files under `codest_home()` (see [`codest_home`]), kept as
//! two files with separate concerns:
//!
//! * `config.toml` — global settings. Today that is one key, the default
//!   model selection; the file is reserved for future global options.
//! * `providers.toml` — the provider instances (endpoints, keys, models).
//!   Keeping instances in their own file lets the `/model` UI manage them
//!   later without touching global settings.
//!
//! Both files are optional: a missing file contributes nothing and the
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

use crate::core::rollout::codest_home;

/// The protocol kind assumed for instances that do not name one.
pub const DEFAULT_PROTOCOL: &str = "openai-compat";

/// `config.toml` — global settings.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    /// Initial model selection, `"provider/model"`.
    pub default: Option<String>,
}

/// One model of a provider: an id plus optional display metadata. All fields
/// beyond `id` are advisory; nothing consumes them yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    /// Reasoning effort levels the model accepts, lowest first.
    pub reasoning: Option<Vec<String>>,
}

impl ModelInfo {
    /// The id reduced to a bare `ModelInfo`, as produced by models discovery.
    pub fn from_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            context_window: None,
            reasoning: None,
        }
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
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
