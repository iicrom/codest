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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
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

/// `/provider`: opens the provider wizard — add an OpenAI-compatible endpoint
/// by base URL and key, sign in to a provider a plugin contributed, or review
/// what is configured. Like `/model`, the spec only gives every front-end a
/// shared, parseable shape; the wizard itself is a front-end concern.
pub static PROVIDER_COMMAND: CommandSpec = CommandSpec::new(
    "provider",
    &[],
    "/provider",
    "add or sign in to a provider",
    ArgsPolicy::None,
);

/// Merged into the shipped command table next to the session commands; no
/// plugin owns it.
pub static PROVIDER_COMMANDS: &[CommandSpec] = &[PROVIDER_COMMAND];

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

/// The protocol contract lives in the contract crate so an external plugin can
/// implement it without depending on this binary. This path stays the
/// historical import site for the host and its built-in protocols.
pub use cst_plugin_api::providers::{ListModelsFuture, ProtocolFactory};

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
    #[error("provider `{name}`: {message}")]
    InvalidWireQuirks { name: String, message: String },
}

/// The built-in factories plus every plugin-contributed one, and the
/// configured instances validated against them. Construction is the
/// validation point: an instance naming an unknown protocol, or a duplicate
/// name, is a startup error — never a runtime surprise.
pub struct ProviderRegistry {
    /// Plugin-contributed factories only. A rebuild after
    /// [`ModelPool::install_instance`] must pass these back to `build`, which
    /// adds the built-ins itself — carrying the merged list would trip the
    /// duplicate-protocol check.
    extra: Vec<&'static dyn ProtocolFactory>,
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

            if let Some(wire) = &instance.wire
                && let Err(message) = wire.validate()
            {
                return Err(RegistryError::InvalidWireQuirks {
                    name: instance.name.clone(),
                    message,
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
            extra: extra.to_vec(),
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

/// The plugin-contributed factories of a live registry, for a rebuild that
/// keeps every plugin protocol while replacing the instance list.
fn factories_of(registry: &ProviderRegistry) -> Vec<&'static dyn ProtocolFactory> {
    registry.extra.clone()
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

/// The provider registry, plus the fetched-models overlay and its on-disk
/// cache.
///
/// Clients are deliberately **not** cached here. A client carries the session
/// header name but is handed the session id per turn, and the id follows the
/// active session — two sessions on the same `provider/model` must not share a
/// client, or the second one would keep announcing the first one's id. Clients
/// are cheap to build (the underlying HTTP connection pool is process-wide, in
/// [`crate::core::openai_compat`]), so the worker builds one per turn.
pub struct ModelPool {
    registry: RwLock<Arc<ProviderRegistry>>,
    /// Models discovered through the providers' models APIs, merged over the
    /// declared ones. Keyed by provider name.
    fetched: Mutex<BTreeMap<String, Vec<ModelInfo>>>,
    /// The endpoint each `fetched` entry was discovered from. A name alone
    /// cannot identify a model list — the same name may point at a different
    /// endpoint between runs (`--base-url`, an edited `providers.toml`), and
    /// reusing the old list under the new URL is exactly the pollution this
    /// guards against.
    fetched_from: Mutex<BTreeMap<String, String>>,
    cache_path: Option<PathBuf>,
}

impl ModelPool {
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry: RwLock::new(Arc::new(registry)),
            fetched: Mutex::new(BTreeMap::new()),
            fetched_from: Mutex::new(BTreeMap::new()),
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

    /// The current registry, as an owned snapshot.
    ///
    /// Callers hold this for the duration of one lookup rather than borrowing
    /// the pool, because [`ModelPool::install_instance`] replaces the snapshot
    /// while the TUI may be iterating (the `/provider` wizard runs between
    /// frames, but discovery tasks read the registry from another task).
    pub fn registry(&self) -> Arc<ProviderRegistry> {
        self.registry.read().expect("pool lock").clone()
    }

    /// Add or replace one instance at runtime — the write half of `/provider`.
    ///
    /// `instance` is validated the same way startup validates configuration: a
    /// duplicate name in the *current* set is a replacement (that is what
    /// "sign in to an existing provider" does), but an unknown protocol, an
    /// empty model id, or invalid wire quirks are errors and leave the pool
    /// untouched.
    ///
    /// No client bookkeeping is needed: clients are built per turn, so the
    /// next one already reads the new `base_url`/`api_key`.
    pub fn install_instance(&self, instance: ProviderInstance) -> Result<(), RegistryError> {
        let name = instance.name.clone();
        {
            let mut guard = self.registry.write().expect("pool lock");
            let mut instances: Vec<ProviderInstance> = guard
                .instances()
                .iter()
                .filter(|existing| existing.name != name)
                .cloned()
                .collect();
            instances.push(instance);
            let rebuilt = ProviderRegistry::build(&factories_of(&guard), instances)?;
            *guard = Arc::new(rebuilt);
        }
        // Outside the write guard: `forget` re-reads the registry to prune the
        // cache file.
        self.forget(&name);
        Ok(())
    }

    /// Drop one instance, plus its fetched models and cached clients. Used by
    /// the wizard's delete action.
    pub fn remove_instance(&self, name: &str) -> Result<bool, RegistryError> {
        {
            let mut guard = self.registry.write().expect("pool lock");
            if guard.instance(name).is_none() {
                return Ok(false);
            }
            let instances: Vec<ProviderInstance> = guard
                .instances()
                .iter()
                .filter(|existing| existing.name != name)
                .cloned()
                .collect();
            let rebuilt = ProviderRegistry::build(&factories_of(&guard), instances)?;
            *guard = Arc::new(rebuilt);
        }
        self.forget(name);
        Ok(true)
    }

    /// Drop a provider's discovered models in memory *and* on disk: the cache
    /// file would otherwise resurrect them on the next start.
    fn forget(&self, provider: &str) {
        self.fetched.lock().expect("pool lock").remove(provider);
        self.fetched_from
            .lock()
            .expect("pool lock")
            .remove(provider);
        self.save_cache();
    }

    /// Test hook: install a discovered list as if `refresh` had just fetched
    /// it from the provider's current endpoint.
    #[cfg(test)]
    fn stage_fetched(&self, provider: &str, models: Vec<ModelInfo>) {
        let endpoint = self
            .registry()
            .instance(provider)
            .map(endpoint_of)
            .unwrap_or_default();
        self.fetched
            .lock()
            .expect("pool lock")
            .insert(provider.to_string(), models);
        self.fetched_from
            .lock()
            .expect("pool lock")
            .insert(provider.to_string(), endpoint);
    }

    /// The raw missing piece name (`"base_url"` / `"api_key"`), if the
    /// provider's protocol says the endpoint is half specified.
    pub fn missing_credential_raw(&self, provider: &str) -> Option<&'static str> {
        let registry = self.registry();
        let instance = registry.instance(provider)?;
        let factory = registry.factory(instance.protocol_id())?;
        factory.missing_credential(instance)
    }

    /// The completion guidance for `provider`, if its protocol says the
    /// endpoint is only half specified. `None` means requests may proceed.
    pub fn missing_credential(&self, provider: &str) -> Option<String> {
        self.missing_credential_raw(provider)
            .map(|missing| missing_credential_guidance(provider, missing))
    }

    /// The client for one selection, built fresh. Callers that need a session
    /// id on the wire pass it per turn through `TurnOptions`, not here.
    pub fn client(&self, selection: &Selection) -> Result<Arc<dyn ModelClient>, PoolError> {
        let registry = self.registry();
        let instance = registry
            .instance(&selection.provider)
            .ok_or_else(|| PoolError::Selection(SelectionError::UnknownProvider {
                provider: selection.provider.clone(),
                configured: registry
                    .instances()
                    .iter()
                    .map(|instance| instance.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            }))?;
        let factory = registry
            .factory(instance.protocol_id())
            .expect("protocol existence is validated at registry build");
        let client = factory
            .create(instance, &selection.model)
            .map_err(|source| PoolError::Create {
                provider: selection.provider.clone(),
                source,
            })?;
        Ok(client)
    }

    /// One row per selectable model: configured models (declared order)
    /// first, then fetched models the config does not already declare. A
    /// declared model keeps precedence over its discovered twin, but fields
    /// the declaration leaves unset are filled in from discovery — a sparse
    /// `[[providers.models]]` entry still gets the context window (and the
    /// rest of the advisory metadata) the models API reports.
    pub fn models_of(&self, provider: &str) -> Vec<ModelInfo> {
        let mut models = self
            .registry()
            .instance(provider)
            .map(|instance| instance.models.clone())
            .unwrap_or_default();
        if let Some(fetched) = self.fetched.lock().expect("pool lock").get(provider) {
            for model in fetched {
                match models.iter_mut().find(|declared| declared.id == model.id) {
                    Some(declared) => {
                        // Declared precedence: fill only what the config left unset.
                        if declared.name.is_none() {
                            declared.name = model.name.clone();
                        }
                        if declared.context_window.is_none() {
                            declared.context_window = model.context_window;
                        }
                        if declared.reasoning.is_none() {
                            declared.reasoning = model.reasoning.clone();
                        }
                        if declared.default_level.is_none() {
                            declared.default_level = model.default_level.clone();
                        }
                    }
                    None => models.push(model.clone()),
                }
            }
        }
        models
    }

    /// The metadata for one selection: the declared model entry when the
    /// config declares it, else the discovered one.
    pub fn model_info(&self, selection: &Selection) -> Option<ModelInfo> {
        self.models_of(&selection.provider)
            .into_iter()
            .find(|model| model.id == selection.model)
    }

    /// Selection rows for the `/model` menu: every configured model of every
    /// instance, in declaration order.
    pub fn selections(&self) -> Vec<Selection> {
        let mut out = Vec::new();
        let registry = self.registry();
        for instance in registry.instances() {
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
        let registry = self.registry();
        let instance = registry
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
        let factory = registry
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
            let endpoint = endpoint_of(&instance);
            self.fetched_from
                .lock()
                .expect("pool lock")
                .insert(provider.to_string(), endpoint);
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
        let registry = self.registry();
        registry
            .instance(provider)
            .is_some_and(|instance| {
                registry
                    .factory(instance.protocol_id())
                    .is_some_and(|factory| {
                        factory
                            .list_models(instance, CancellationToken::new())
                            .is_some()
                    })
            })
    }

    /// Preload the on-disk cache, keeping only entries that still describe a
    /// configured endpoint.
    ///
    /// The cache is keyed by provider name, but a name does not identify a
    /// model list: `--base-url` or an edited `providers.toml` can point the
    /// same name at a different endpoint between runs. An entry is therefore
    /// used only when its recorded endpoint matches the instance's current
    /// one; untouched entries (the older, untagged format) cannot be checked
    /// and are dropped — they are rediscovered on this run. A stale entry
    /// would otherwise merge another endpoint's models into `/model`, which
    /// is the pollution this rules out.
    fn load_cache(&mut self, path: &PathBuf) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(cache) = serde_json::from_str::<ModelCache>(&content) else {
            return;
        };
        let registry = self.registry();
        let mut fetched = BTreeMap::new();
        let mut fetched_from = BTreeMap::new();
        for (provider, cached) in cache.providers {
            let Some(instance) = registry.instance(&provider) else {
                continue;
            };
            let endpoint = endpoint_of(instance);
            let CachedModels::Tagged(entry) = cached else {
                continue;
            };
            if entry.base_url.as_deref() != Some(endpoint.as_str()) {
                continue;
            }
            fetched.insert(provider.clone(), entry.models);
            fetched_from.insert(provider, endpoint);
        }
        *self.fetched.lock().expect("pool lock") = fetched;
        *self.fetched_from.lock().expect("pool lock") = fetched_from;
    }

    /// Write the overlay back, pruning entries that no longer belong to a
    /// configured endpoint. Without the prune every provider ever configured
    /// would be re-persisted forever.
    fn save_cache(&self) {
        let Some(path) = &self.cache_path else {
            return;
        };
        let registry = self.registry();
        let fetched = self.fetched.lock().expect("pool lock");
        let fetched_from = self.fetched_from.lock().expect("pool lock");
        let mut providers = BTreeMap::new();
        for (provider, models) in fetched.iter() {
            // A candidate whose endpoint moved on is not this run's list;
            // `refresh` will refill it for the current endpoint.
            let Some(instance) = registry.instance(provider) else {
                continue;
            };
            let endpoint = endpoint_of(instance);
            if fetched_from.get(provider) != Some(&endpoint) {
                continue;
            }
            providers.insert(
                provider.clone(),
                CachedModels::Tagged(CachedEntry {
                    base_url: Some(endpoint),
                    models: models.clone(),
                }),
            );
        }
        drop(fetched_from);
        drop(fetched);
        let cache = ModelCache { providers };
        if let Ok(json) = serde_json::to_string_pretty(&cache)
            && let Err(error) = std::fs::write(path, json + "\n")
        {
            eprintln!("codest: cannot write model cache: {error}");
        }
    }
}

/// The endpoint a provider's models come from, as recorded in the cache:
/// the instance's `base_url`, trimmed so cosmetic slash edits do not count as
/// a different endpoint. An instance without one has no cacheable endpoint.
fn endpoint_of(instance: &ProviderInstance) -> String {
    instance
        .base_url
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ModelCache {
    #[serde(default)]
    providers: BTreeMap<String, CachedModels>,
}

/// One cached model list. The tagged form records the endpoint it came from
/// so a name pointing elsewhere cannot reuse it; the untagged form is the
/// pre-endpoint-tagging file layout, read only to be discarded.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum CachedModels {
    Tagged(CachedEntry),
    Legacy(Vec<ModelInfo>),
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedEntry {
    base_url: Option<String>,
    #[serde(default)]
    models: Vec<ModelInfo>,
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

impl OpenAiCompatFactory {
    /// The client one instance of `openai-compat` builds into: URL, key,
    /// static headers, `body_overrides`, wire quirks, and the session header
    /// name, all baked in at construction. Split out of
    /// [`ProtocolFactory::create`] so tests can inspect the built client's
    /// state.
    ///
    /// The session header *name* is configuration, so it is resolved here; the
    /// session *id* is per-turn state and rides [`TurnOptions`] instead.
    fn build_client(
        instance: &ProviderInstance,
        model: &str,
    ) -> Result<OpenAiCompatClient, ModelError> {
        let api_key = instance.resolved_api_key().unwrap_or_default();
        let base_url = instance.base_url.clone().unwrap_or_default();
        let headers = instance
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let quirks = instance.wire.clone().unwrap_or_default();
        // A model's own `session_header` overrides the instance's.
        let model_header = instance
            .models
            .iter()
            .find(|declared| declared.id == model)
            .and_then(|declared| declared.session_header.as_deref());
        let session_header =
            crate::core::config::resolve_session_header(&quirks, model_header).map(str::to_string);
        Ok(OpenAiCompatClient::new(api_key, base_url, model)?
            .with_headers(headers)
            .with_body_overrides(instance.body_overrides.clone())
            .with_quirks(quirks)
            .with_session_header(session_header))
    }
}

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
        Ok(Arc::new(Self::build_client(instance, model)?))
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
            let url = models_url(instance, base_url);
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

/// The models-API URL for one instance.
///
/// `options.models_url` names it explicitly, for endpoints whose models API
/// does not live at `{base_url}/models`. Validation is codecs-level, not
/// vendor-level: no host is known here. Without the option the URL is
/// `{base_url}/models`, exactly as before the option existed.
fn models_url(instance: &ProviderInstance, base_url: &str) -> String {
    match instance
        .option_str(MODELS_URL_OPTION)
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        Some(url) => url.to_string(),
        None => format!("{}/models", base_url.trim_end_matches('/')),
    }
}

/// The `options` key naming an explicit models-API URL.
const MODELS_URL_OPTION: &str = "models_url";

/// Parse a models directory into advisory [`ModelInfo`] records.
///
/// Only shapes seen on the wire are handled, never vendor names. The outer
/// envelope is OpenAI's `{data: [{...}]}`; within an entry the metadata
/// spellings that vary are read in order of specificity:
///
/// * context window — `limit.context` (`{context, output}`), then
///   `context_length` (OpenRouter), then `context_window`;
/// * reasoning levels — `capabilities.reasoning.effort` (an ordered list,
///   lowest first), falling back to a top-level `reasoning` array, else the
///   `default_level` field;
/// * default level — `capabilities.reasoning.default`, then `default_level`.
///
/// Entries without an id are skipped, duplicates are collapsed, order is
/// preserved. Unknown fields are ignored: the directory carries far more than
/// this client consumes.
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
        let default_level = entry
            .pointer("/capabilities/reasoning/default")
            .and_then(serde_json::Value::as_str)
            .or_else(|| entry.get("default_level").and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|level| !level.is_empty())
            .map(str::to_string);
        let reasoning = reasoning_levels(entry);
        // The window: a `{context, output}` limits object first, then the
        // two flat spellings older endpoints use.
        let context_window = entry
            .pointer("/limit/context")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                entry
                    .get("context_length")
                    .and_then(serde_json::Value::as_u64)
            })
            .or_else(|| {
                entry
                    .get("context_window")
                    .and_then(serde_json::Value::as_u64)
            });
        models.push(ModelInfo {
            id: id.to_string(),
            name,
            context_window,
            reasoning,
            default_level,
            // Models discovery reports no per-model overrides.
            session_header: None,
        });
    }
    models
}

/// The effort levels one directory entry accepts, lowest first. An absent or
/// empty list means "unknown", which `/effort` reads as the shared defaults.
fn reasoning_levels(entry: &serde_json::Value) -> Option<Vec<String>> {
    let list = entry
        .pointer("/capabilities/reasoning/effort")
        .or_else(|| entry.get("reasoning"))
        .and_then(serde_json::Value::as_array)?;
    let levels: Vec<String> = list
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(str::to_string)
        .collect();
    (!levels.is_empty()).then_some(levels)
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
            wire: None,
            options: Default::default(),
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
    fn registry_validates_wire_quirks() {
        use crate::core::config::WireQuirks;

        let mut inst = instance("p", Some(DEFAULT_PROTOCOL), &["m"]);
        inst.wire = Some(WireQuirks {
            thinking_fields: vec!["  ".to_string()],
            ..Default::default()
        });
        let error = ProviderRegistry::build(&[], vec![inst]).unwrap_err();
        assert!(matches!(error, RegistryError::InvalidWireQuirks { .. }));
        assert!(
            error.to_string().contains("provider `p`"),
            "the error names the instance: {error}"
        );
    }

    #[test]
    fn openai_factory_bakes_instance_wire_quirks() {
        use crate::core::config::WireQuirks;

        let quirks = WireQuirks {
            thinking_fields: vec!["reasoning".to_string(), "thinking".to_string()],
            effort_field: "reasoning".to_string(),
            ..Default::default()
        };
        let mut inst = instance("p", Some(DEFAULT_PROTOCOL), &["m"]);
        inst.wire = Some(quirks.clone());

        let client = OpenAiCompatFactory::build_client(&inst, "m").expect("client builds");
        assert_eq!(client.quirks(), &quirks);

        // No wire table: the built-in spellings, unchanged.
        let inst = instance("plain", Some(DEFAULT_PROTOCOL), &["m"]);
        let client = OpenAiCompatFactory::build_client(&inst, "m").expect("client builds");
        assert_eq!(client.quirks(), &WireQuirks::default());
    }

    /// The session header name is configuration: the instance decides, a model
    /// may override it, and `none` at either level turns it off. The value
    /// (the session id) arrives per turn and is not part of the client.
    #[test]
    fn session_header_resolves_instance_then_model() {
        use crate::core::TurnOptions;
        use crate::core::config::{SESSION_HEADER_NONE, WireQuirks};

        // An instance with no wire table still sends `session-id`.
        let inst = instance("p", Some(DEFAULT_PROTOCOL), &["m"]);
        let client = OpenAiCompatFactory::build_client(&inst, "m").expect("client builds");
        assert_eq!(
            client.session_headers(&TurnOptions {
                effort: None,
                tools: Vec::new(),
                session_id: Some("sid".to_string()),
            }),
            vec![("session-id", "sid")]
        );

        // The instance renames it for everyone…
        let mut inst = instance("p", Some(DEFAULT_PROTOCOL), &["m"]);
        inst.wire = Some(WireQuirks {
            session_header: "x-session".to_string(),
            ..Default::default()
        });
        let client = OpenAiCompatFactory::build_client(&inst, "m").expect("client builds");
        assert_eq!(
            client.session_headers(&TurnOptions {
                effort: None,
                tools: Vec::new(),
                session_id: Some("sid".to_string()),
            }),
            vec![("x-session", "sid")]
        );

        // …and a single model can opt out while the instance keeps sending.
        let mut inst = instance("p", Some(DEFAULT_PROTOCOL), &["quiet", "loud"]);
        inst.models[0].session_header = Some(SESSION_HEADER_NONE.to_string());
        let quiet = OpenAiCompatFactory::build_client(&inst, "quiet").expect("client builds");
        assert!(
            quiet
                .session_headers(&TurnOptions {
                    effort: None,
                    tools: Vec::new(),
                    session_id: Some("sid".to_string()),
                })
                .is_empty(),
            "a model that opts out sends no session header"
        );
        let loud = OpenAiCompatFactory::build_client(&inst, "loud").expect("client builds");
        assert_eq!(
            loud.session_headers(&TurnOptions {
                effort: None,
                tools: Vec::new(),
                session_id: Some("sid".to_string()),
            }),
            vec![("session-id", "sid")],
            "the neighbouring model keeps the default"
        );

        // An instance-wide opt-out applies to models that do not override it.
        let mut inst = instance("p", Some(DEFAULT_PROTOCOL), &["m"]);
        inst.wire = Some(WireQuirks {
            session_header: SESSION_HEADER_NONE.to_string(),
            ..Default::default()
        });
        let client = OpenAiCompatFactory::build_client(&inst, "m").expect("client builds");
        assert!(
            client
                .session_headers(&TurnOptions {
                    effort: None,
                    tools: Vec::new(),
                    session_id: Some("sid".to_string()),
                })
                .is_empty()
        );
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

    /// Clients are never shared: a client carries the session header name but
    /// is handed the session id per turn, so two sessions on one selection must
    /// each get their own. Reuse would announce the wrong session upstream.
    #[test]
    fn pool_builds_a_fresh_client_per_call() {
        let registry =
            ProviderRegistry::build(&[], vec![instance("p", None, &["m1", "m2"])]).unwrap();
        let pool = ModelPool::new(registry);

        let first = pool.client(&Selection::new("p", "m1")).unwrap();
        let again = pool.client(&Selection::new("p", "m1")).unwrap();
        assert!(
            !Arc::ptr_eq(&first, &again),
            "the same selection must not share a client across sessions"
        );
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

        pool.stage_fetched(
            "p",
            vec![
                ModelInfo::from_id("m1"),
                ModelInfo::from_id("m2"),
                ModelInfo::from_id("m3"),
            ],
        );

        let models = pool.models_of("p");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2", "m3"], "declared first, fetched merged");
        assert_eq!(pool.selections().len(), 3);
    }

    #[test]
    fn models_of_fills_sparse_declared_entries_from_discovery() {
        let mut sparse = ModelInfo::from_id("m1");
        sparse.context_window = Some(8_000);
        let instance = ProviderInstance {
            name: "p".to_string(),
            protocol: Some(DEFAULT_PROTOCOL.to_string()),
            base_url: None,
            api_key: None,
            models: vec![sparse, ModelInfo::from_id("m2")],
            fetch_models: Some(true),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry =
            ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        let pool = ModelPool::new(registry);
        let mut fetched_m1 = ModelInfo::from_id("m1");
        fetched_m1.name = Some("Discovered One".to_string());
        fetched_m1.context_window = Some(128_000);
        fetched_m1.default_level = Some("high".to_string());
        let mut fetched_m2 = ModelInfo::from_id("m2");
        fetched_m2.context_window = Some(200_000);
        pool.stage_fetched("p", vec![fetched_m1, fetched_m2]);

        let models = pool.models_of("p");
        // The declared window wins; the unset fields come from discovery.
        assert_eq!(models[0].context_window, Some(8_000));
        assert_eq!(models[0].name.as_deref(), Some("Discovered One"));
        assert_eq!(models[0].default_level.as_deref(), Some("high"));
        // A declaration without any metadata is filled wholesale.
        assert_eq!(models[1].context_window, Some(200_000));
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
        pool.stage_fetched("p", vec![ModelInfo::from_id("fetched")]);
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

    /// A cached list belongs to one endpoint. Pointing the same provider name
    /// at a different URL — `--base-url`, an edited `providers.toml` — must
    /// not resurrect the previous endpoint's models.
    #[test]
    fn cache_is_dropped_when_the_provider_endpoint_moves() {
        let dir = temp_cache_dir("codest-pool-cache-moved");
        let cache_path = dir.join("model-cache.json");

        let before = instance("p", Some(DEFAULT_PROTOCOL), &["declared"]);
        let pool = ModelPool::new(ProviderRegistry::build(&[], vec![before]).unwrap())
            .with_cache(cache_path.clone());
        pool.stage_fetched("p", vec![ModelInfo::from_id("old-endpoint-model")]);
        pool.save_cache();

        // Same name, different base_url: the cached list no longer applies.
        let mut moved = instance("p", Some(DEFAULT_PROTOCOL), &["declared"]);
        moved.base_url = Some("http://127.0.0.1:2/v1".to_string());
        let pool = ModelPool::new(ProviderRegistry::build(&[], vec![moved]).unwrap())
            .with_cache(cache_path.clone());
        let ids: Vec<String> = pool.models_of("p").iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["declared"], "the old endpoint's models are gone");

        // …and the prune is durable: the entry is not written back either.
        pool.save_cache();
        let content = std::fs::read_to_string(&cache_path).expect("read cache");
        assert!(!content.contains("old-endpoint-model"), "{content}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Providers that are no longer configured must not linger in the file.
    #[test]
    fn cache_prunes_providers_that_are_no_longer_configured() {
        let dir = temp_cache_dir("codest-pool-cache-prune");
        let cache_path = dir.join("model-cache.json");

        let pool = ModelPool::new(
            ProviderRegistry::build(
                &[],
                vec![
                    instance("kept", Some(DEFAULT_PROTOCOL), &["m"]),
                    instance("dropped", Some(DEFAULT_PROTOCOL), &["m"]),
                ],
            )
            .unwrap(),
        )
        .with_cache(cache_path.clone());
        pool.stage_fetched("kept", vec![ModelInfo::from_id("fetched")]);
        pool.stage_fetched("dropped", vec![ModelInfo::from_id("orphan")]);
        pool.save_cache();

        // A run that only knows `kept` rewrites the file without the orphan:
        // the name `dropped` would otherwise be re-persisted forever.
        let pool = ModelPool::new(
            ProviderRegistry::build(&[], vec![instance("kept", Some(DEFAULT_PROTOCOL), &["m"])])
                .unwrap(),
        )
        .with_cache(cache_path.clone());
        pool.save_cache();
        let content = std::fs::read_to_string(&cache_path).expect("read cache");
        assert!(content.contains("kept"), "{content}");
        assert!(!content.contains("orphan"), "{content}");
        assert!(!content.contains("dropped"), "{content}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removing a provider drops its cached list from disk too, so a delete
    /// in the wizard is not undone by the next start.
    #[test]
    fn removing_an_instance_forgets_its_cache() {
        let dir = temp_cache_dir("codest-pool-cache-remove");
        let cache_path = dir.join("model-cache.json");

        let pool = ModelPool::new(
            ProviderRegistry::build(&[], vec![instance("p", Some(DEFAULT_PROTOCOL), &["m"])])
                .unwrap(),
        )
        .with_cache(cache_path.clone());
        pool.stage_fetched("p", vec![ModelInfo::from_id("fetched")]);
        pool.save_cache();
        assert!(
            std::fs::read_to_string(&cache_path)
                .expect("read cache")
                .contains("fetched")
        );

        assert!(pool.remove_instance("p").expect("remove"));
        let content = std::fs::read_to_string(&cache_path).expect("read cache");
        assert!(!content.contains("fetched"), "{content}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pre-tagging file layout carries no endpoint, so it cannot be
    /// validated: it is dropped rather than trusted, and the run refetches.
    #[test]
    fn untagged_cache_entries_are_discarded() {
        let dir = temp_cache_dir("codest-pool-cache-legacy");
        let cache_path = dir.join("model-cache.json");
        std::fs::write(
            &cache_path,
            r#"{"providers":{"p":[{"id":"legacy-model"}]}}"#,
        )
        .expect("write legacy cache");

        let pool = ModelPool::new(
            ProviderRegistry::build(&[], vec![instance("p", Some(DEFAULT_PROTOCOL), &["m"])])
                .unwrap(),
        )
        .with_cache(cache_path.clone());
        let ids: Vec<String> = pool.models_of("p").iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["m"], "the unverifiable list is not used");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_cache_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn openai_models_list_parses_and_dedupes() {
        let payload = serde_json::json!({
            "data": [
                {"id": "m1", "name": "Model One", "default_level": "high",
                 "context_length": 128000},
                {"id": "m1"},
                {"id": ""},
                {"no-id": true},
                {"id": "m2", "default_level": "  ", "context_window": 200000},
                {"id": "m3", "default_level": "thinking"},
                {"id": "m4", "context_length": "big"}
            ]
        });
        let models = models_from_openai_list(&payload);
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].display_name(), "Model One");
        assert_eq!(models[0].default_level.as_deref(), Some("high"));
        assert_eq!(models[0].context_window, Some(128_000));
        assert_eq!(models[1].id, "m2");
        // A blank default is not a level.
        assert_eq!(models[1].default_level, None);
        assert_eq!(models[1].context_window, Some(200_000));
        assert_eq!(models[2].id, "m3");
        assert_eq!(models[2].default_level.as_deref(), Some("thinking"));
        assert_eq!(models[2].context_window, None);
        // A non-numeric window is ignored, not an error.
        assert_eq!(models[3].id, "m4");
        assert_eq!(models[3].context_window, None);

        assert!(models_from_openai_list(&serde_json::json!({})).is_empty());
    }

    /// The `capabilities` / `limit` shape: window under `limit.context`,
    /// levels under `capabilities.reasoning.effort`, default under
    /// `capabilities.reasoning.default`.
    #[test]
    fn models_list_parses_limits_and_capabilities() {
        let payload = serde_json::json!({
            "object": "list",
            "data": [{
                "id": "some-model",
                "object": "model",
                "name": "Some Model",
                "canonical_id": "vendor/some-model-1",
                "limit": {"context": 1_000_000, "output": 384_000},
                "capabilities": {
                    "tools": {"supported": true},
                    "reasoning": {
                        "supported": true,
                        "effort": ["low", "high", "max"],
                        "default": "high"
                    }
                },
                "cost": {"text": {"input": {"price": 0.15}, "output": {"price": 0.6}}}
            }]
        });
        let models = models_from_openai_list(&payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "some-model");
        assert_eq!(models[0].name.as_deref(), Some("Some Model"));
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(
            models[0].reasoning.as_deref(),
            Some(&["low".to_string(), "high".to_string(), "max".to_string()][..])
        );
        assert_eq!(models[0].default_level.as_deref(), Some("high"));
    }

    /// A model whose capabilities carry no `effort` list, and one carrying a
    /// bare `toggle`, must not invent levels — `/effort` then uses its shared
    /// defaults.
    #[test]
    fn models_list_tolerates_missing_effort_lists() {
        let payload = serde_json::json!({
            "data": [
                {"id": "toggle-only", "limit": {"context": 256_000},
                 "capabilities": {"reasoning": {"supported": true, "toggle": true}}},
                {"id": "bare", "capabilities": {"reasoning": {"supported": true}}},
                {"id": "unsupported", "capabilities": {"reasoning": false}},
                {"id": "flat-window", "context_window": 128_000},
                {"id": "no-metadata"}
            ]
        });
        let models = models_from_openai_list(&payload);
        assert_eq!(models.len(), 5);
        assert_eq!(models[0].context_window, Some(256_000));
        assert_eq!(models[0].reasoning, None);
        assert_eq!(models[0].default_level, None);
        assert_eq!(models[1].reasoning, None);
        assert_eq!(models[2].reasoning, None);
        // The flat spelling still works, so older endpoints do not regress.
        assert_eq!(models[3].context_window, Some(128_000));
        assert_eq!(models[4].context_window, None);
        assert_eq!(models[4].reasoning, None);
    }

    /// `limit.context` is the most specific spelling, so it wins over the
    /// older flat ones when an endpoint reports both.
    #[test]
    fn models_list_prefers_limit_context() {
        let payload = serde_json::json!({
            "data": [{"id": "m", "limit": {"context": 500_000},
                      "context_length": 128_000, "context_window": 200_000}]
        });
        assert_eq!(
            models_from_openai_list(&payload)[0].context_window,
            Some(500_000)
        );
    }

    /// `options.models_url` names the models API explicitly; without it the
    /// URL stays `{base_url}/models`, exactly as before the option existed.
    #[test]
    fn models_url_honours_the_option_and_defaults_to_the_base_path() {
        let mut instance = instance("p", Some(DEFAULT_PROTOCOL), &["m"]);
        assert_eq!(
            models_url(&instance, "https://host/v1/"),
            "https://host/v1/models"
        );

        instance.options.insert(
            MODELS_URL_OPTION.to_string(),
            serde_json::json!("https://host/inference/go/v1/models"),
        );
        assert_eq!(
            models_url(&instance, "https://host/v1"),
            "https://host/inference/go/v1/models"
        );

        // A blank option is the same as no option.
        instance
            .options
            .insert(MODELS_URL_OPTION.to_string(), serde_json::json!("   "));
        assert_eq!(
            models_url(&instance, "https://host/v1"),
            "https://host/v1/models"
        );
    }

    #[test]
    fn model_info_finds_declared_and_fetched_entries() {
        let mut declared = ModelInfo::from_id("m1");
        declared.default_level = Some("high".to_string());
        let mut fetched = ModelInfo::from_id("fetched");
        fetched.default_level = Some("low".to_string());
        let instance = ProviderInstance {
            name: "p".to_string(),
            protocol: Some(DEFAULT_PROTOCOL.to_string()),
            base_url: None,
            api_key: None,
            models: vec![declared],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry =
            ProviderRegistry::build(&[], vec![instance]).expect("valid registry");
        let pool = ModelPool::new(registry);
        pool.stage_fetched("p", vec![fetched]);

        assert_eq!(
            pool.model_info(&Selection::new("p", "m1"))
                .and_then(|model| model.default_level),
            Some("high".to_string()),
            "the declared entry wins"
        );
        assert_eq!(
            pool.model_info(&Selection::new("p", "fetched"))
                .and_then(|model| model.default_level),
            Some("low".to_string())
        );
        assert!(pool.model_info(&Selection::new("p", "nope")).is_none());
    }

    /// `/provider` installs an instance after the pool exists: the registry
    /// must see it and a replacement must win.
    ///
    /// Nothing needs evicting, because clients are built per turn — the next
    /// one reads the replaced instance. The check below pins that property by
    /// asserting the newly built client points at the new endpoint.
    #[test]
    fn install_instance_registers_and_replaces() {
        let registry = ProviderRegistry::build(&[], vec![instance("p", None, &["m1"])]).unwrap();
        let pool = ModelPool::new(registry);

        let mut replacement = instance("p", None, &["m1"]);
        replacement.base_url = Some("http://127.0.0.1:2/v1".to_string());
        pool.install_instance(replacement).expect("install");

        // Clients are built per turn, so the next one is built from the
        // *replaced* instance — no cache to invalidate.
        let installed = pool.registry().instance("p").expect("instance").clone();
        let client = OpenAiCompatFactory::build_client(&installed, "m1").expect("client");
        assert_eq!(client.endpoint(), "http://127.0.0.1:2/v1/chat/completions");

        // A brand-new provider becomes selectable.
        pool.install_instance(instance("q", None, &["qm"]))
            .expect("install new");
        assert!(pool.registry().instance("q").is_some());
        assert!(pool.client(&Selection::new("q", "qm")).is_ok());

        // Replacing keeps one instance per name rather than appending.
        let names: Vec<String> = pool
            .registry()
            .instances()
            .iter()
            .map(|instance| instance.name.clone())
            .collect();
        assert_eq!(names, vec!["p".to_string(), "q".to_string()]);
    }

    /// The install path validates exactly like startup: an unknown protocol is
    /// rejected and the previous registry stays intact.
    #[test]
    fn install_instance_rejects_unknown_protocol() {
        let registry = ProviderRegistry::build(&[], vec![instance("p", None, &["m1"])]).unwrap();
        let pool = ModelPool::new(registry);
        let broken = instance("p", Some("no-such-protocol"), &["m1"]);

        let error = pool.install_instance(broken).expect_err("must reject");
        assert!(matches!(error, RegistryError::UnknownProtocol { .. }));
        assert_eq!(
            pool.registry()
                .instance("p")
                .map(|instance| instance.protocol_id().to_string()),
            Some(DEFAULT_PROTOCOL.to_string()),
            "the failed install left the old instance in place"
        );
    }

    #[test]
    fn remove_instance_reports_whether_it_existed() {
        let registry = ProviderRegistry::build(&[], vec![instance("p", None, &["m1"])]).unwrap();
        let pool = ModelPool::new(registry);
        assert!(pool.remove_instance("p").expect("remove"));
        assert!(pool.registry().instance("p").is_none());
        assert!(!pool.remove_instance("p").expect("remove again"));
    }
}
