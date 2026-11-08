//! The provider registry: protocol kinds (compile time) × instances (config).
//!
//! This is the kind/instance split that lets a static plugin system carry
//! runtime-configured endpoints:
//!
//! * A **protocol kind** is a [`ProtocolFactory`] — a `'static` value that
//!   knows one wire protocol and can build a [`ModelClient`] from an
//!   instance's configuration. The built-ins are `openai-compat` and `test`;
//!   plugins contribute more through `Plugin::protocols`.
//! * A **provider instance** is a [`ProviderInstance`] — pure config data
//!   from `providers.toml`, naming a protocol kind by id. "A custom endpoint"
//!   is just an instance of `openai-compat` with a different `base_url`.
//!
//! A **selection** names one model of one instance as `"provider/model"`
//! (parsed at the *first* slash, so model ids may themselves contain `/`,
//! e.g. `gateway/deepseek/deepseek-r1`).
//!
//! The [`ModelPool`] wraps a registry with a lazily-built, cached client per
//! selection, so switching models never rebuilds clients and a config with
//! many models never pre-builds clients it never uses. It also owns the
//! fetched-model overlay: models discovered through a factory's
//! [`ProtocolFactory::list_models`] merge over the declared ones and persist
//! to `model-cache.json`, so the menu is instant on the next start and a
//! failed fetch never empties it.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::core::commands::ArgsPolicy;
use crate::core::commands::CommandSpec;
use crate::core::config::ModelInfo;
use crate::core::config::ProviderInstance;
use crate::core::echo_model::EchoModelClient;
use crate::core::model::ModelClient;
use crate::core::model::ModelError;
use crate::core::openai_compat::OpenAiCompatClient;

/// The `/model` command: opens the model picker. Like the session commands,
/// the spec only gives every front-end a shared, parseable shape (command
/// menu, `/help`); the picker behavior itself is a front-end concern.
pub static MODEL_COMMAND: CommandSpec = CommandSpec::new(
    "model",
    &[],
    "/model",
    "open the model list to switch provider/model",
    ArgsPolicy::None,
);

/// Merged into the shipped command table next to the session commands; no
/// plugin owns it.
pub static MODEL_COMMANDS: &[CommandSpec] = &[MODEL_COMMAND];

/// Shown when a turn is requested with no model selected at all (no
/// providers configured). Shared by the worker's own guard and the TUI's
/// submit guard so every front-end gives the same guidance.
pub const NO_MODEL_GUIDANCE: &str = "no model is configured; add an instance to \
     ~/.codest/providers.toml (or set CST_API_KEY) and restart";

/// Shown when providers exist but none has a selectable model yet — e.g. a
/// synthesized endpoint awaiting its models discovery.
pub const NO_MODEL_SELECTED: &str =
    "no model is selected; open /model to discover and pick one";

/// The completion prompt for an instance whose endpoint is only half
/// specified (a lone `--base-url` or `--api-key`).
pub fn missing_credential_guidance(provider: &str, missing: &str) -> String {
    format!(
        "provider `{provider}` is missing `{missing}`; set it (flag/env or \
         ~/.codest/providers.toml) and restart"
    )
}

/// One resolved model selection: `"provider/model"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub provider: String,
    pub model: String,
}

impl Selection {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// Parse `"provider/model"` at the first slash; the model part may
    /// contain further slashes.
    pub fn parse(selection: &str) -> Result<Self, SelectionError> {
        let Some((provider, model)) = selection.split_once('/') else {
            return Err(SelectionError::MissingModel {
                selection: selection.to_string(),
            });
        };
        if provider.is_empty() || model.is_empty() {
            return Err(SelectionError::MissingModel {
                selection: selection.to_string(),
            });
        }
        Ok(Self {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }

    pub fn as_str(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

impl std::fmt::Display for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("invalid selection `{selection}`: expected \"provider/model\"")]
    MissingModel { selection: String },
    #[error("unknown provider `{provider}` (configured: {configured})")]
    UnknownProvider {
        provider: String,
        configured: String,
    },
}

/// A future yielding the models one provider exposes through its models API.
pub type ListModelsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, ModelError>> + Send + 'a>>;

/// One compile-time protocol kind. Factories are `&'static` values shared by
/// the registry; all per-instance state (URL, key, headers) arrives through
/// `instance`, so one factory serves every instance of its protocol.
pub trait ProtocolFactory: Sync {
    /// Stable id referenced by `providers.toml` (`protocol = "..."`).
    fn id(&self) -> &'static str;

    /// Human-readable protocol name for diagnostics.
    fn display_name(&self) -> &'static str;

    /// Build the client for one model of one instance. Failures here (an
    /// unusable base URL, say) surface at selection time as a turn error —
    /// never as a failed startup, so one broken instance cannot take the
    /// rest down.
    fn create(
        &self,
        instance: &ProviderInstance,
        model: &str,
    ) -> Result<Arc<dyn ModelClient>, ModelError>;

    /// Models discovery for instances of this protocol, if it has one
    /// (`GET {base_url}/models` for `openai-compat`). `None` means the
    /// protocol cannot list models.
    fn list_models<'a>(
        &'a self,
        _instance: &'a ProviderInstance,
        _cancel: CancellationToken,
    ) -> Option<ListModelsFuture<'a>> {
        None
    }

    /// The missing endpoint piece this protocol requires from `instance`,
    /// if a request cannot even be attempted. Protocols that need nothing
    /// (the in-process echo) keep the default; `openai-compat` requires a
    /// `base_url` always and an `api_key` wherever one was expected
    /// ([`ProviderInstance::requires_api_key`], the partial CLI synthesis
    /// case) — config-declared keyless endpoints stay keyless by intent.
    fn missing_credential(&self, _instance: &ProviderInstance) -> Option<&'static str> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("provider name must not be empty")]
    EmptyProviderName,
    #[error("provider name `{name}` must not contain `/`")]
    ProviderNameContainsSlash { name: String },
    #[error("provider `{name}` is declared more than once")]
    DuplicateProvider { name: String },
    #[error("provider `{name}` uses unknown protocol `{protocol}` (known: {known})")]
    UnknownProtocol {
        name: String,
        protocol: String,
        known: String,
    },
    #[error("model id must not be empty (provider `{name}`)")]
    EmptyModelId { name: String },
    #[error("model `{model}` is declared more than once on provider `{name}`")]
    DuplicateModel { name: String, model: String },
    #[error("protocol `{id}` is registered more than once")]
    DuplicateProtocol { id: String },
}

/// The built-in factories plus every plugin-contributed one, and the
/// configured instances validated against them. Construction is the
/// validation point: an instance naming an unknown protocol, or a duplicate
/// name, is a startup error — never a runtime surprise.
pub struct ProviderRegistry {
    factories: Vec<&'static dyn ProtocolFactory>,
    instances: Vec<ProviderInstance>,
}

impl ProviderRegistry {
    /// Validate `instances` against the built-in factories plus `extra`
    /// (typically collected from the plugin host).
    pub fn build(
        extra: &[&'static dyn ProtocolFactory],
        instances: Vec<ProviderInstance>,
    ) -> Result<Self, RegistryError> {
        let mut factories: Vec<&'static dyn ProtocolFactory> =
            vec![&OPENAI_COMPAT_FACTORY, &ECHO_FACTORY];
        for factory in extra {
            if factories.iter().any(|known| known.id() == factory.id()) {
                return Err(RegistryError::DuplicateProtocol {
                    id: factory.id().to_string(),
                });
            }
            factories.push(*factory);
        }

        let mut names: Vec<&str> = Vec::new();
        for instance in &instances {
            if instance.name.is_empty() {
                return Err(RegistryError::EmptyProviderName);
            }
            if instance.name.contains('/') {
                return Err(RegistryError::ProviderNameContainsSlash {
                    name: instance.name.clone(),
                });
            }
            if names.contains(&instance.name.as_str()) {
                return Err(RegistryError::DuplicateProvider {
                    name: instance.name.clone(),
                });
            }
            names.push(&instance.name);

            if is_unknown_protocol(&factories, instance.protocol_id()) {
                return Err(RegistryError::UnknownProtocol {
                    name: instance.name.clone(),
                    protocol: instance.protocol_id().to_string(),
                    known: known_protocol_ids(&factories).join(", "),
                });
            }

            let mut model_ids: Vec<&str> = Vec::new();
            for model in &instance.models {
                if model.id.is_empty() {
                    return Err(RegistryError::EmptyModelId {
                        name: instance.name.clone(),
                    });
                }
                if model_ids.contains(&model.id.as_str()) {
                    return Err(RegistryError::DuplicateModel {
                        name: instance.name.clone(),
                        model: model.id.clone(),
                    });
                }
                model_ids.push(&model.id);
            }
        }

        Ok(Self {
            factories,
            instances,
        })
    }

    pub fn factory(&self, id: &str) -> Option<&'static dyn ProtocolFactory> {
        self.factories.iter().copied().find(|f| f.id() == id)
    }

    pub fn instances(&self) -> &[ProviderInstance] {
        &self.instances
    }

    pub fn instance(&self, name: &str) -> Option<&ProviderInstance> {
        self.instances.iter().find(|i| i.name == name)
    }

    pub fn protocol_display_name(&self, id: &str) -> &'static str {
        self.factory(id)
            .map(|factory| factory.display_name())
            .unwrap_or("unknown")
    }

    /// Resolve a selection against the configured instances. The provider
    /// must exist; the model is passed through (an unknown model id is the
    /// provider's error to answer, and some registries fill in later).
    pub fn parse_selection(&self, selection: &str) -> Result<Selection, SelectionError> {
        let parsed = Selection::parse(selection)?;
        if self.instance(&parsed.provider).is_none() {
            let configured = self
                .instances
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(SelectionError::UnknownProvider {
                provider: parsed.provider,
                configured: if configured.is_empty() {
                    "none".to_string()
                } else {
                    configured
                },
            });
        }
        Ok(parsed)
    }

    /// The selection to start with: `requested` (from config.toml/CLI) when
    /// it parses, else the first model of the first instance.
    pub fn default_selection(&self, requested: Option<&str>) -> Result<Selection, String> {
        if let Some(requested) = requested {
            return self.parse_selection(requested).map_err(|e| e.to_string());
        }
        let Some(instance) = self.instances.first() else {
            return Err("no providers configured".to_string());
        };
        let Some(model) = instance.models.first() else {
            return Err(format!(
                "provider `{}` has no models; declare `[[providers.models]]` \
                 or enable `fetch_models`",
                instance.name
            ));
        };
        Ok(Selection::new(&instance.name, &model.id))
    }
}

fn is_unknown_protocol(factories: &[&'static dyn ProtocolFactory], id: &str) -> bool {
    !factories.iter().any(|factory| factory.id() == id)
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("protocols", &known_protocol_ids(&self.factories))
            .field("instances", &self.instances)
            .finish()
    }
}

fn known_protocol_ids(factories: &[&'static dyn ProtocolFactory]) -> Vec<&'static str> {
    factories.iter().map(|factory| factory.id()).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error(transparent)]
    Selection(#[from] SelectionError),
    #[error("provider `{provider}`: {source}")]
    Create {
        provider: String,
        #[source]
        source: ModelError,
    },
    #[error("provider `{provider}`: protocol `{protocol}` cannot list models")]
    NoDiscovery { provider: String, protocol: String },
    #[error("provider `{provider}` is not configured for model discovery: {reason}")]
    DiscoveryNotConfigured { provider: String, reason: String },
}

/// Clients per selection, built on first use; plus the fetched-models overlay
/// and its on-disk cache.
pub struct ModelPool {
    registry: ProviderRegistry,
    clients: Mutex<HashMap<String, Arc<dyn ModelClient>>>,
    /// Models discovered through the providers' models APIs, merged over the
    /// declared ones. Keyed by provider name.
    fetched: Mutex<BTreeMap<String, Vec<ModelInfo>>>,
    cache_path: Option<PathBuf>,
}

impl ModelPool {
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry,
            clients: Mutex::new(HashMap::new()),
            fetched: Mutex::new(BTreeMap::new()),
            cache_path: None,
        }
    }

    /// Persist fetched model lists here and preload them at startup, so the
    /// menu is instant and a failed refresh never empties it.
    pub fn with_cache(mut self, path: PathBuf) -> Self {
        self.load_cache(&path);
        self.cache_path = Some(path);
        self
    }

    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    /// The raw missing piece name (`"base_url"` / `"api_key"`), if the
    /// provider's protocol says the endpoint is half specified.
    pub fn missing_credential_raw(&self, provider: &str) -> Option<&'static str> {
        let instance = self.registry.instance(provider)?;
        let factory = self.registry.factory(instance.protocol_id())?;
        factory.missing_credential(instance)
    }

    /// The completion guidance for `provider`, if its protocol says the
    /// endpoint is only half specified. `None` means requests may proceed.
    pub fn missing_credential(&self, provider: &str) -> Option<String> {
        self.missing_credential_raw(provider)
            .map(|missing| missing_credential_guidance(provider, missing))
    }

    /// The client for one selection, built on first use and cached.
    pub fn client(&self, selection: &Selection) -> Result<Arc<dyn ModelClient>, PoolError> {
        let key = selection.as_str();
        if let Some(client) = self.clients.lock().expect("pool lock").get(key.as_str()) {
            return Ok(client.clone());
        }

        let instance = self
            .registry
            .instance(&selection.provider)
            .expect("provider existence is validated by parse_selection");
        let factory = self
            .registry
            .factory(instance.protocol_id())
            .expect("protocol existence is validated at registry build");
        let client = factory
            .create(instance, &selection.model)
            .map_err(|source| PoolError::Create {
                provider: selection.provider.clone(),
                source,
            })?;
        self.clients
            .lock()
            .expect("pool lock")
            .insert(key.to_string(), client.clone());
        Ok(client)
    }

    /// One row per selectable model: configured models (declared order)
    /// first, then fetched models the config does not already declare.
    pub fn models_of(&self, provider: &str) -> Vec<ModelInfo> {
        let mut models = self
            .registry
            .instance(provider)
            .map(|instance| instance.models.clone())
            .unwrap_or_default();
        let declared: std::collections::HashSet<String> =
            models.iter().map(|model| model.id.clone()).collect();
        if let Some(fetched) = self.fetched.lock().expect("pool lock").get(provider) {
            for model in fetched {
                if !declared.contains(&model.id) {
                    models.push(model.clone());
                }
            }
        }
        models
    }

    /// Selection rows for the `/model` menu: every configured model of every
    /// instance, in declaration order.
    pub fn selections(&self) -> Vec<Selection> {
        let mut out = Vec::new();
        for instance in self.registry.instances() {
            for model in self.models_of(&instance.name) {
                out.push(Selection::new(&instance.name, &model.id));
            }
        }
        out
    }

    /// Fetch one provider's models through its factory, merge them over the
    /// declared list, and refresh the cache. Bounded by a timeout so a menu
    /// refresh can never hang the UI. Returns the merged model count.
    pub async fn refresh(&self, provider: &str) -> Result<usize, PoolError> {
        let instance = self
            .registry
            .instance(provider)
            .ok_or_else(|| PoolError::DiscoveryNotConfigured {
                provider: provider.to_string(),
                reason: "no such provider".to_string(),
            })?
            .clone();
        // A half-specified endpoint (lone base_url or api_key) cannot be
        // queried meaningfully; the caller surfaces the completion prompt.
        if let Some(missing) = self.missing_credential_raw(provider) {
            return Err(PoolError::DiscoveryNotConfigured {
                provider: provider.to_string(),
                reason: format!("`{missing}` is not set"),
            });
        }
        let factory = self
            .registry
            .factory(instance.protocol_id())
            .expect("protocol existence is validated at registry build");
        let list = factory.list_models(&instance, CancellationToken::new()).ok_or_else(|| {
            PoolError::NoDiscovery {
                provider: provider.to_string(),
                protocol: instance.protocol_id().to_string(),
            }
        })?;

        let models = tokio::time::timeout(REFRESH_TIMEOUT, list)
            .await
            .map_err(|_| PoolError::DiscoveryNotConfigured {
                provider: provider.to_string(),
                reason: "models request timed out".to_string(),
            })?
            .map_err(|source| PoolError::Create {
                provider: provider.to_string(),
                source,
            })?;

        let count = {
            let mut fetched = self.fetched.lock().expect("pool lock");
            fetched.insert(provider.to_string(), models);
            drop(fetched);
            // `models_of` re-locks `fetched`, so the guard above must be
            // gone — a lock held across this call would deadlock the menu
            // reading on the other end of the pool.
            self.models_of(provider).len()
        };
        self.save_cache();
        Ok(count)
    }

    /// Whether the provider's factory can list models at all (menu hint).
    pub fn can_discover(&self, provider: &str) -> bool {
        self.registry
            .instance(provider)
            .is_some_and(|instance| {
                self.registry
                    .factory(instance.protocol_id())
                    .is_some_and(|factory| {
                        factory
                            .list_models(instance, CancellationToken::new())
                            .is_some()
                    })
            })
    }

    fn load_cache(&mut self, path: &PathBuf) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(cache) = serde_json::from_str::<ModelCache>(&content) else {
            return;
        };
        *self.fetched.lock().expect("pool lock") = cache.providers;
    }

    fn save_cache(&self) {
        let Some(path) = &self.cache_path else {
            return;
        };
        let cache = ModelCache {
            providers: self.fetched.lock().expect("pool lock").clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&cache)
            && let Err(error) = std::fs::write(path, json + "\n")
        {
            eprintln!("codest: cannot write model cache: {error}");
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ModelCache {
    #[serde(default)]
    providers: BTreeMap<String, Vec<ModelInfo>>,
}

/// Timeout for one models-API request.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Built-in factories
// ---------------------------------------------------------------------------

/// `openai-compat`: the shipped HTTP protocol. Instances customize it with
/// `base_url`, `api_key`, `headers`, and `body_overrides`.
struct OpenAiCompatFactory;

/// The `openai-compat` factory singleton, built into every registry.
static OPENAI_COMPAT_FACTORY: OpenAiCompatFactory = OpenAiCompatFactory;

impl ProtocolFactory for OpenAiCompatFactory {
    fn id(&self) -> &'static str {
        crate::core::config::DEFAULT_PROTOCOL
    }

    fn display_name(&self) -> &'static str {
        "OpenAI-compatible"
    }

    fn missing_credential(&self, instance: &ProviderInstance) -> Option<&'static str> {
        if !instance.has_base_url() {
            return Some("base_url");
        }
        if instance.requires_api_key && instance.resolved_api_key().is_none() {
            return Some("api_key");
        }
        None
    }

    fn create(
        &self,
        instance: &ProviderInstance,
        model: &str,
    ) -> Result<Arc<dyn ModelClient>, ModelError> {
        let api_key = instance.resolved_api_key().unwrap_or_default();
        let base_url = instance.base_url.clone().unwrap_or_default();
        let headers = instance
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let client = OpenAiCompatClient::new(api_key, base_url, model)?
            .with_headers(headers)
            .with_body_overrides(instance.body_overrides.clone());
        Ok(Arc::new(client))
    }

    fn list_models<'a>(
        &'a self,
        instance: &'a ProviderInstance,
        cancel: CancellationToken,
    ) -> Option<ListModelsFuture<'a>> {
        Some(Box::pin(async move {
            let base_url = instance.base_url.as_deref().map(str::trim).filter(|url| !url.is_empty()).ok_or_else(|| {
                ModelError::NotConfigured("base_url is not set; the models API needs one".to_string())
            })?;
            let url = format!("{}/models", base_url.trim_end_matches('/'));
            let api_key = instance.resolved_api_key().unwrap_or_default();

            let http = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(ModelError::network)?;
            let mut request = http.get(&url);
            if !api_key.is_empty() {
                request = request.bearer_auth(&api_key);
            }
            for (key, value) in &instance.headers {
                request = request.header(key, value);
            }

            let response = tokio::select! {
                _ = cancel.cancelled() => Err(ModelError::Network("cancelled".to_string()))?,
                result = request.send() => result.map_err(ModelError::network)?,
            };
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(ModelError::Http {
                    status: status.as_u16(),
                    body: body.chars().take(600).collect(),
                });
            }

            let payload: Value = response.json().await.map_err(ModelError::network)?;
            Ok(models_from_openai_list(&payload))
        }))
    }
}

/// Parse the OpenAI-shaped `{data: [{id, ...}]}` directory. Entries without
/// an id are skipped, duplicates are collapsed, order is preserved.
pub fn models_from_openai_list(payload: &serde_json::Value) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    let Some(entries) = payload.get("data").and_then(serde_json::Value::as_array) else {
        return models;
    };
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        models.push(ModelInfo {
            id: id.to_string(),
            name,
            context_window: None,
            reasoning: None,
        });
    }
    models
}

/// `test`: the in-process echo model. Zero-config offline demo backend.
struct EchoFactory;

/// The `test` factory singleton, built into every registry.
static ECHO_FACTORY: EchoFactory = EchoFactory;

impl ProtocolFactory for EchoFactory {
    fn id(&self) -> &'static str {
        "test"
    }

    fn display_name(&self) -> &'static str {
        "In-process echo (testing)"
    }

    fn create(
        &self,
        _instance: &ProviderInstance,
        _model: &str,
    ) -> Result<Arc<dyn ModelClient>, ModelError> {
        Ok(Arc::new(EchoModelClient::new(Duration::ZERO)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::DEFAULT_PROTOCOL;

    fn instance(name: &str, protocol: Option<&str>, models: &[&str]) -> ProviderInstance {
        ProviderInstance {
            name: name.to_string(),
            protocol: protocol.map(str::to_string),
            base_url: Some("http://127.0.0.1:1/v1".to_string()),
            api_key: Some("k".to_string()),
            models: models
                .iter()
                .map(|id| ModelInfo::from_id(*id))
                .collect(),
            fetch_models: Some(false),
            headers: BTreeMap::new(),
            body_overrides: serde_json::Map::new(),
            requires_api_key: false,
        }
    }

    #[test]
    fn selection_parses_at_the_first_slash() {
        let selection = Selection::parse("openai/gpt-4o").unwrap();
        assert_eq!(selection.provider, "openai");
        assert_eq!(selection.model, "gpt-4o");
        assert_eq!(selection.as_str(), "openai/gpt-4o");

        // Model ids may contain slashes themselves.
        let selection = Selection::parse("gateway/deepseek/deepseek-r1").unwrap();
        assert_eq!(selection.provider, "gateway");
        assert_eq!(selection.model, "deepseek/deepseek-r1");

        assert!(Selection::parse("nomodel").is_err());
        assert!(Selection::parse("openai/").is_err());
        assert!(Selection::parse("/gpt").is_err());
    }

    #[test]
    fn registry_validates_instances() {
        // Unknown protocol names the known ones.
        let error = ProviderRegistry::build(&[], vec![instance("x", Some("nope"), &["m"])])
            .unwrap_err();
        assert!(error.to_string().contains("openai-compat"), "{error}");

        // Duplicates and empty names are startup errors.
        let instances = vec![instance("dup", None, &["m"]), instance("dup", None, &["m"])];
        assert!(matches!(
            ProviderRegistry::build(&[], instances).unwrap_err(),
            RegistryError::DuplicateProvider { .. }
        ));
        assert!(ProviderRegistry::build(&[], vec![instance("", None, &["m"])]).is_err());
        assert!(ProviderRegistry::build(&[], vec![instance("p/slash", None, &["m"])]).is_err());
        assert!(ProviderRegistry::build(&[], vec![instance("p", None, &[""])]).is_err());
        assert!(ProviderRegistry::build(&[], vec![instance("p", None, &["m", "m"])]).is_err());
    }

    #[test]
    fn registry_resolves_selections_and_defaults() {
        let registry =
            ProviderRegistry::build(&[], vec![instance("p", None, &["m1", "m2"])]).unwrap();

        let selection = registry.parse_selection("p/m2").unwrap();
        assert_eq!(selection, Selection::new("p", "m2"));

        let error = registry.parse_selection("other/m").unwrap_err();
        assert!(error.to_string().contains("configured: p"), "{error}");

        assert_eq!(
            registry.default_selection(None).unwrap(),
            Selection::new("p", "m1")
        );
        assert_eq!(
            registry.default_selection(Some("p/m2")).unwrap(),
            Selection::new("p", "m2")
        );
        assert!(registry.default_selection(Some("other/m")).is_err());
    }

    #[test]
    fn empty_registry_defaults_to_an_actionable_error() {
        let registry = ProviderRegistry::build(&[], Vec::new()).unwrap();
        assert!(registry.default_selection(None).is_err());

        let registry = ProviderRegistry::build(&[], vec![instance("p", None, &[])]).unwrap();
        let error = registry.default_selection(None).unwrap_err();
        assert!(error.contains("no models"), "{error}");
    }

    #[test]
    fn pool_builds_clients_lazily_and_caches_them() {
        let registry =
            ProviderRegistry::build(&[], vec![instance("p", None, &["m1", "m2"])]).unwrap();
        let pool = ModelPool::new(registry);

        let first = pool.client(&Selection::new("p", "m1")).unwrap();
        let again = pool.client(&Selection::new("p", "m1")).unwrap();
        assert!(Arc::ptr_eq(&first, &again), "cached per selection");
        let other = pool.client(&Selection::new("p", "m2")).unwrap();
        assert!(!Arc::ptr_eq(&first, &other));

        // OpenAI-compat accepts any model id; the endpoint answers for itself.
        let unknown = pool.client(&Selection::new("p", "never-seen")).unwrap();
        assert_eq!(unknown.label(), "never-seen");
    }

    #[test]
    fn models_of_merges_fetched_over_declared() {
        let registry = ProviderRegistry::build(
            &[],
            vec![instance("p", Some(DEFAULT_PROTOCOL), &["m1"])],
        )
        .unwrap();
        let pool = ModelPool::new(registry);
        assert_eq!(pool.models_of("p").len(), 1);

        pool.fetched
            .lock()
            .expect("lock")
            .insert("p".to_string(), vec![
                ModelInfo::from_id("m1"),
                ModelInfo::from_id("m2"),
                ModelInfo::from_id("m3"),
            ]);

        let models = pool.models_of("p");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2", "m3"], "declared first, fetched merged");
        assert_eq!(pool.selections().len(), 3);
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "codest-pool-cache-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cache_path = dir.join("model-cache.json");

        let registry = ProviderRegistry::build(
            &[],
            vec![instance("p", Some(DEFAULT_PROTOCOL), &["m1"])],
        )
        .unwrap();
        let pool = ModelPool::new(registry).with_cache(cache_path.clone());
        pool.fetched
            .lock()
            .expect("lock")
            .insert("p".to_string(), vec![ModelInfo::from_id("fetched")]);
        pool.save_cache();

        // A fresh pool preloads the overlay: the menu is instant on restart
        // and a failed refresh never empties it.
        let registry = ProviderRegistry::build(
            &[],
            vec![instance("p", Some(DEFAULT_PROTOCOL), &["m1"])],
        )
        .unwrap();
        let pool = ModelPool::new(registry).with_cache(cache_path);
        let ids: Vec<String> = pool.models_of("p").iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["m1", "fetched"]);
    }

    #[test]
    fn openai_models_list_parses_and_dedupes() {
        let payload = serde_json::json!({
            "data": [
                {"id": "m1", "name": "Model One"},
                {"id": "m1"},
                {"id": ""},
                {"no-id": true},
                {"id": "m2"}
            ]
        });
        let models = models_from_openai_list(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].display_name(), "Model One");
        assert_eq!(models[1].id, "m2");

        assert!(models_from_openai_list(&serde_json::json!({})).is_empty());
    }
}
