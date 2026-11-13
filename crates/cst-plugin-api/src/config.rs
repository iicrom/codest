//! Provider configuration vocabulary.
//!
//! These are the types a `ProtocolFactory` receives and the ones the host
//! parses from `providers.toml`. They live in the contract crate because a
//! plugin must be able to name them without depending on the host binary.
//!
//! The host keeps the file-location helpers (`config_path`, `providers_path`,
//! …) and the loading/saving functions to itself: a plugin never touches the
//! user's configuration files.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

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
/// The request header that carries the session id by default.
///
/// This is codex's spelling, and the one OpenCode's console accepts from
/// codex-style clients: one stable value per conversation, reused across every
/// request so the upstream can route consistently and reuse its prompt cache.
pub const DEFAULT_SESSION_HEADER: &str = "session-id";
/// The `wire.session_header` / `models.session_header` value that never sends
/// the session header, for endpoints that reject unknown headers.
pub const SESSION_HEADER_NONE: &str = "none";

/// One model of a provider: an id plus optional display metadata. The fields
/// beyond `id` are advisory — `reasoning` and `default_level` feed `/effort`,
/// the rest await consumers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Reasoning effort levels the model accepts, lowest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Vec<String>>,
    /// The reasoning effort this model prefers when none is set explicitly.
    /// Adopted when the model is selected (startup or `/model`) until
    /// `/effort` overrides it. Accepted from providers.toml and the models
    /// API alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_level: Option<String>,
    /// Overrides the instance's `wire.session_header` for this model: a
    /// header name, or [`SESSION_HEADER_NONE`] to send no session header at
    /// all. `None` inherits the instance setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_header: Option<String>,
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
            session_header: None,
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
/// logic. `WireQuirks` covers only the points where the wire shape has no
/// safe fallback — which delta field carries reasoning text, where the
/// per-turn effort goes, and which header carries the session id. Everything
/// else (usage token spellings, context window spellings, request body fixes)
/// is already tolerated in code or expressible through
/// [`ProviderInstance::body_overrides`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WireQuirks {
    /// Delta fields tried in order as reasoning text (first hit wins). An
    /// empty list disables thinking parsing for the instance.
    pub thinking_fields: Vec<String>,
    /// The top-level request field that carries the per-turn effort selected
    /// with `/effort`. [`EFFORT_FIELD_NONE`] never sends the field.
    pub effort_field: String,
    /// The request header that carries this session's id on every request.
    /// [`SESSION_HEADER_NONE`] never sends it. A model's own
    /// [`ModelInfo::session_header`] overrides this per model.
    pub session_header: String,
}

impl Default for WireQuirks {
    fn default() -> Self {
        Self {
            thinking_fields: vec![DEFAULT_THINKING_FIELD.to_string()],
            effort_field: DEFAULT_EFFORT_FIELD.to_string(),
            session_header: DEFAULT_SESSION_HEADER.to_string(),
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
        if self.session_header.trim().is_empty() {
            return Err("wire.session_header must not be empty".to_string());
        }
        for field in &self.thinking_fields {
            if field.trim().is_empty() {
                return Err("wire.thinking_fields entries must not be empty".to_string());
            }
        }
        Ok(())
    }

    /// The header name session ids are sent under, or `None` when this
    /// instance opted out. Applied over [`Self::session_header`] by a model's
    /// own `session_header` when it declares one.
    pub fn session_header_name(&self) -> Option<&str> {
        let name = self.session_header.trim();
        (!name.is_empty() && name != SESSION_HEADER_NONE).then_some(name)
    }
}

/// Resolve the session header name for one model of an instance: the model's
/// own `session_header` wins over the instance's `wire.session_header`, and
/// [`SESSION_HEADER_NONE`] at either level suppresses it.
///
/// Lives here rather than in the host so a protocol plugin resolves it the
/// same way the built-in `openai-compat` factory does.
pub fn resolve_session_header<'a>(
    quirks: &'a WireQuirks,
    model_header: Option<&'a str>,
) -> Option<&'a str> {
    match model_header.map(str::trim) {
        Some(SESSION_HEADER_NONE) => None,
        Some(name) if !name.is_empty() => Some(name),
        Some(_) => quirks.session_header_name(),
        None => quirks.session_header_name(),
    }
}

/// Per-plugin settings from `config.toml`'s `[plugins."<id>"]` table.
///
/// The host interprets no plugin key — the whole table is the plugin's
/// pocket, exactly like [`ProviderInstance::options`] is a protocol's. A
/// plugin reads its pocket through
/// [`crate::plugin::PluginContext::plugin_settings`] using the typed readers
/// below; anything richer stays available as raw JSON.
///
/// ```toml
/// [plugins.tps]
/// enabled = true
/// window_ms = 500
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginSettings {
    /// The table as parsed TOML converted to JSON values (datetimes become
    /// their RFC 3339 text, like [`ProviderInstance::options`]).
    pub options: serde_json::Map<String, Value>,
}

impl<'de> Deserialize<'de> for PluginSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self {
            options: toml_table_to_json_map(deserializer)?,
        })
    }
}

impl PluginSettings {
    /// One entry as a string, or `None` when absent/not a string.
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.options.get(key).and_then(Value::as_str)
    }

    /// One entry as an unsigned integer.
    pub fn option_u64(&self, key: &str) -> Option<u64> {
        self.options.get(key).and_then(Value::as_u64)
    }

    /// One entry as a boolean.
    pub fn option_bool(&self, key: &str) -> Option<bool> {
        self.options.get(key).and_then(Value::as_bool)
    }
}

/// One provider instance: an endpoint configuration bound to a protocol kind
/// ([`ProviderInstance::protocol_id`]). This is pure config data — turning it
/// into a client is the registry's job.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInstance {
    /// Unique instance name; also the `provider` half of a selection. Must
    /// not contain `/` (selections parse at the first slash).
    pub name: String,
    /// Protocol kind id; defaults to [`DEFAULT_PROTOCOL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API key or `$VAR` template. An empty/unresolved key fails the turn
    /// with `NotConfigured` instead of failing startup, so a partial
    /// providers.toml still boots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelInfo>,
    /// Fetch the model list from the provider's models API. Defaults to true
    /// for instances that declare no models and false otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_models: Option<bool>,
    /// Static headers sent with every request (provider quirks, disguises).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// JSON fields merged over the request body; overrides win.
    #[serde(default, deserialize_with = "toml_table_to_json_map", skip_serializing_if = "serde_json::Map::is_empty")]
    pub body_overrides: serde_json::Map<String, Value>,
    /// Wire quirks for the `openai-compat` protocol, baked into the client at
    /// construction. `None` uses the built-in spellings
    /// (`thinking_fields = [reasoning_content]`, `effort_field =
    /// "reasoning_effort"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<WireQuirks>,
    /// Protocol-private configuration, handed to the instance's
    /// [`crate::providers::ProtocolFactory`] verbatim. A plugin's settings are
    /// plain TOML under `[providers.options]`; the host interprets no *plugin*
    /// key, so a new plugin needs no host change and no schema migration. The
    /// built-in `openai-compat` factory reads one key of its own here —
    /// `models_url`, an explicit models-API URL for endpoints whose directory
    /// does not live at `{base_url}/models`. Unknown *top-level* keys are
    /// still rejected.
    #[serde(default, deserialize_with = "toml_table_to_json_map", skip_serializing_if = "serde_json::Map::is_empty")]
    pub options: serde_json::Map<String, Value>,
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

    /// One `options` entry as a string, or `None` when absent/not a string.
    /// Plugins read their settings through this instead of parsing JSON by
    /// hand for the common case.
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.options.get(key).and_then(Value::as_str)
    }

    /// One `options` entry as an unsigned integer.
    pub fn option_u64(&self, key: &str) -> Option<u64> {
        self.options.get(key).and_then(Value::as_u64)
    }

    /// One `options` entry as a boolean.
    pub fn option_bool(&self, key: &str) -> Option<bool> {
        self.options.get(key).and_then(Value::as_bool)
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
/// Deserialize a TOML table into a `serde_json` map. TOML is JSON-compatible
/// apart from datetimes, which render as their RFC 3339 string.
pub fn toml_table_to_json_map<'de, D>(deserializer: D) -> Result<serde_json::Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let table = toml::Table::deserialize(deserializer)?;
    Ok(toml_to_json(&toml::Value::Table(table))
        .as_object()
        .cloned()
        .unwrap_or_default())
}

/// Convert TOML values to JSON. Datetimes become their RFC 3339 text because
/// JSON has no datetime type and the host only ever forwards these values.
pub fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => Value::from(*number),
        toml::Value::Boolean(flag) => Value::from(*flag),
        toml::Value::Datetime(stamp) => Value::String(stamp.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, value)| (key.clone(), toml_to_json(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_templates_expand_literals_pass_through() {
        assert_eq!(expand_env_template("plain"), Some("plain".to_string()));
        // A lone `$` is a literal, not an empty variable name.
        assert_eq!(expand_env_template("$"), Some("$".to_string()));
        // An unset variable yields `None` so callers can report the template.
        assert_eq!(expand_env_template("$CST_TEST_DEFINITELY_UNSET_VAR"), None);
    }

    #[test]
    fn options_are_plain_toml_and_read_typed() {
        let instance: ProviderInstance = toml::from_str(
            r#"
            name = "qoder"
            protocol = "qoder"

            [options]
            open_api_url = "https://openapi.qoder.sh"
            max_output_tokens = 131072
            debug = true
            "#,
        )
        .expect("parse");
        assert_eq!(instance.option_str("open_api_url"), Some("https://openapi.qoder.sh"));
        assert_eq!(instance.option_u64("max_output_tokens"), Some(131072));
        assert_eq!(instance.option_bool("debug"), Some(true));
        assert_eq!(instance.option_str("absent"), None);
        // A mistyped scalar kind is not coerced.
        assert_eq!(instance.option_str("max_output_tokens"), None);
    }

    #[test]
    fn unknown_top_level_keys_are_still_rejected() {
        let error = toml::from_str::<ProviderInstance>(
            r#"
            name = "x"
            nonsense = 1
            "#,
        )
        .expect_err("unknown keys must fail");
        assert!(error.to_string().contains("nonsense"));
    }

    #[test]
    fn plugin_settings_are_a_free_form_pocket() {
        // The whole `[plugins."<id>"]` table is the plugin's pocket: no
        // nesting under `options`, no host-known keys to trip over.
        let settings: PluginSettings = toml::from_str(
            r#"
            enabled = true
            window_ms = 500
            label = "wire"
            "#,
        )
        .expect("parse");
        assert_eq!(settings.option_bool("enabled"), Some(true));
        assert_eq!(settings.option_u64("window_ms"), Some(500));
        assert_eq!(settings.option_str("label"), Some("wire"));
        assert_eq!(settings.option_str("absent"), None);
        // A mistyped scalar kind is not coerced, like provider options.
        assert_eq!(settings.option_bool("window_ms"), None);
        assert_eq!(PluginSettings::default().option_str("anything"), None);
    }
}
