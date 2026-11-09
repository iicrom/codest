//! The sign-in contract: how a plugin offers an authentication entry to the
//! host's `/provider` menu.
//!
//! The host owns the menu, the prompts, and the cancellation; the plugin owns
//! the protocol (browser device flow, token exchange) and the credential
//! storage. That split keeps the host free of any vendor's OAuth shape while
//! still letting a plugin add a provider end to end.
//!
//! ```text
//! /provider ──lists──▶ Plugin::auth_providers()      (static metadata)
//!           ──badge──▶ Plugin::auth_status(instance)  (sync, local file read)
//!           ──run───▶ Plugin::begin_sign_in(...)      (async, cancellable)
//!                          │
//!                          └── SignInNotifier ──▶ wizard status line + transcript
//! ```
//!
//! `auth_status` is deliberately synchronous: the menu opens instantly and a
//! network round trip never blocks the event loop. `begin_sign_in` returns a
//! future the TUI drives on its own task, so a browser authorization that
//! takes a minute cannot freeze rendering.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::plugin::PluginError;

/// A sign-in entry a plugin contributes. One per provider page it serves
/// (`qoder`, `qoder-cn`), independent of how many configured instances use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthProvider {
    /// Default instance name (`auth_providers.toml`-free sign-in creates an
    /// instance under this name when none uses the protocol yet).
    pub id: &'static str,
    /// The protocol id this entry signs in to; the host pairs configured
    /// instances with it via [`crate::config::ProviderInstance::protocol_id`].
    pub protocol: &'static str,
    /// Display name (`Qoder`).
    pub label: &'static str,
    /// Endpoint used when sign-in has to create the instance.
    pub default_base_url: Option<&'static str>,
    /// Methods in preference order; the first is the recommended one. At
    /// least one is required.
    pub methods: &'static [AuthMethod],
}

/// One way to authenticate, in the order the host should offer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthMethod {
    /// Stable id passed back to [`crate::plugin::Plugin::begin_sign_in`]
    /// (`"browser"`, `"pat"`).
    pub id: &'static str,
    /// Menu label (`Sign in with a browser`, `Paste a Personal Access Token`).
    pub label: &'static str,
    pub kind: AuthMethodKind,
}

/// Whether the host collects a secret before starting, and whether it can
/// show a URL while the flow runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethodKind {
    /// No input: the plugin drives everything, usually through
    /// [`SignInNotifier::notify`] with a URL.
    Browser,
    /// The host asks for a secret (an API key, a token) and passes it to
    /// [`crate::plugin::Plugin::begin_sign_in`] as `secret`.
    SecretPrompt,
}

/// What the plausibly-known credential state is, from local storage only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// This plugin offers no sign-in for the named instance.
    Unsupported,
    /// No stored credential.
    SignedOut,
    /// A stored credential exists and is considered current.
    SignedIn { account: Option<String> },
    /// A stored credential exists but is past its expiry and needs renewal
    /// or a fresh sign-in; the menu shows it as needing attention.
    Expired,
}

/// A progress line the wizard shows while a flow runs. `url` is surfaced
/// verbatim (the human opens it themselves — the host never starts a browser
/// on a plugin's behalf).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInNotice {
    pub message: String,
    pub url: Option<String>,
}

impl SignInNotice {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            url: None,
        }
    }

    pub fn with_url(message: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            url: Some(url.into()),
        }
    }
}

/// A cheap, clonable channel for reporting progress from a running flow.
#[derive(Clone)]
pub struct SignInNotifier(Arc<dyn Fn(SignInNotice) + Send + Sync>);

impl SignInNotifier {
    pub fn new(sink: impl Fn(SignInNotice) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }

    pub fn notify(&self, notice: SignInNotice) {
        (self.0)(notice);
    }
}

impl std::fmt::Debug for SignInNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignInNotifier")
    }
}

/// What a completed sign-in reports back. The plugin has already persisted
/// whatever it needs; the host only uses `account` for the confirmation line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignInOutcome {
    /// A human-readable account label (an email, a name), when the plugin
    /// knows one.
    pub account: Option<String>,
}

/// The future a sign-in returns. `'static` because the TUI drives it on a
/// spawned task; everything it needs must be cloned in by then.
pub type SignInFuture =
    Pin<Box<dyn Future<Output = Result<SignInOutcome, PluginError>> + Send + 'static>>;
