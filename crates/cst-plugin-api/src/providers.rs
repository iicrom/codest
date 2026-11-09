//! The provider protocol contract.
//!
//! A **protocol kind** is a [`ProtocolFactory`]: a `'static` value that knows
//! one wire protocol and can build a [`crate::model::ModelClient`] from an
//! instance's configuration. The host ships two (`openai-compat`, `test`);
//! plugins contribute more through [`crate::plugin::Plugin::protocols`].
//!
//! A **provider instance** ([`crate::config::ProviderInstance`]) is pure
//! config data naming a protocol kind by id. "A custom endpoint" is just an
//! instance of `openai-compat` with a different `base_url`; "a new vendor" is
//! a plugin contributing a factory plus instances of it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::ModelInfo;
use crate::config::ProviderInstance;
use crate::model::ModelClient;
use crate::model::ModelError;

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
    ///
    /// This is a *static* gate, answered from configuration alone. A protocol
    /// whose credential lives somewhere else (a sign-in grant, a keychain)
    /// keeps the default and reports the problem from `create` /
    /// `stream_chat` instead, where the message can name the real remedy.
    fn missing_credential(&self, _instance: &ProviderInstance) -> Option<&'static str> {
        None
    }
}
