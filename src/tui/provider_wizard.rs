//! The `/provider` wizard: a small, pure step machine.
//!
//! Everything here is data and pure transitions — no terminal, no network, no
//! file I/O. [`super::app`] owns an instance, feeds it keystrokes and completed
//! background results, renders its [`WizardView`], and performs the side
//! effects the transitions ask for ([`WizardAction`]). Keeping the decisions
//! separate is what makes the flow testable without a terminal, the same split
//! the rest of the plugin layer uses.
//!
//! The flow has two entry points, matching the two reasons someone opens it:
//!
//! * **add** — a name, a base URL, a key, and a model list for a new
//!   OpenAI-compatible endpoint;
//! * **sign in** — a provider a plugin contributed a sign-in entry for
//!   (Qoder, say), where the endpoint and protocol come from the plugin.
//!
//! ```text
//! Action ─┬─▶ AddName ─▶ AddBaseUrl ─▶ AddApiKey ─▶ AddModels ─▶ AddProbe ─▶ AddConfirm
//!         ├─▶ SignInPick ─▶ SignInMethod ─▶ SignInSecret ─▶ SignInRunning
//!         └─▶ Configured
//! ```

use cst_plugin_api::auth::AuthMethod;
use cst_plugin_api::auth::AuthMethodKind;
use cst_plugin_api::auth::AuthProvider;
use cst_plugin_api::auth::AuthStatus;

use crate::core::config::ModelInfo;
use crate::core::config::ProviderInstance;

/// Which top-level action the menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// An OpenAI-compatible endpoint: base URL + key.
    AddOpenAi,
    /// A provider a plugin contributes a sign-in entry for.
    SignIn,
    /// Review what is already configured.
    Configured,
    /// Leave the wizard.
    Close,
}

/// One sign-in target: either a configured instance already using a plugin's
/// protocol, or the plugin's default route (created on success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInTarget {
    /// The instance name to sign in (`"qoder"`). For a target created from a
    /// plugin entry, this is the entry's `id`.
    pub instance: String,
    /// Whether `instance` is already in `providers.toml`.
    pub configured: bool,
    /// The plugin's entry, providing the label, protocol, and methods.
    pub provider: AuthProvider,
    pub status: AuthStatus,
}

impl SignInTarget {
    /// The badge shown next to the label.
    pub fn status_label(&self) -> &'static str {
        match &self.status {
            AuthStatus::Unsupported => "",
            AuthStatus::SignedOut => "not signed in",
            AuthStatus::SignedIn { .. } => "signed in",
            AuthStatus::Expired => "sign-in expired",
        }
    }

    /// Whether the row offers "sign out" instead of "sign in" as its primary
    /// action.
    pub fn is_signed_in(&self) -> bool {
        matches!(self.status, AuthStatus::SignedIn { .. })
    }
}

/// A field the wizard is collecting text for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    BaseUrl,
    ApiKey,
    Models,
    Secret,
}

impl Field {
    /// The prompt line for this field.
    pub fn prompt(self, target: Option<&AuthProvider>) -> String {
        match self {
            Field::Name => "Route name".to_string(),
            Field::BaseUrl => "Base URL".to_string(),
            Field::ApiKey => "API key ($VAR or literal, empty for keyless)".to_string(),
            Field::Models => "Model ids (comma-separated, empty = discover them)".to_string(),
            Field::Secret => match target {
                Some(provider) => format!("{} token", provider.label),
                None => "Secret".to_string(),
            },
        }
    }

    /// Whether the value is sensitive and must not be echoed into the
    /// transcript.
    pub fn is_secret(self) -> bool {
        matches!(self, Field::ApiKey | Field::Secret)
    }
}

/// Where the wizard is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The top-level action menu.
    Action,
    /// The sign-in target picker.
    SignInPick,
    /// The method picker, when a target offers more than one.
    SignInMethod,
    /// Collecting an instance name for a new endpoint.
    AddName,
    /// Collecting a base URL.
    AddBaseUrl,
    /// Collecting an API key.
    AddApiKey,
    /// Collecting model ids.
    AddModels,
    /// A probe is running; no keys except cancel.
    AddProbe,
    /// Reviewing the draft; Enter writes it.
    AddConfirm,
    /// Collecting the secret a `SecretPrompt` method needs.
    SignInSecret,
    /// The sign-in future is running; Esc cancels.
    SignInRunning,
    /// The read-only list of configured providers.
    Configured,
}

impl Step {
    /// Whether this step collects text through the composer.
    pub fn field(&self) -> Option<Field> {
        match self {
            Step::AddName => Some(Field::Name),
            Step::AddBaseUrl => Some(Field::BaseUrl),
            Step::AddApiKey => Some(Field::ApiKey),
            Step::AddModels => Some(Field::Models),
            Step::SignInSecret => Some(Field::Secret),
            _ => None,
        }
    }

    /// Whether this step is a picker (↑↓ navigates a list).
    pub fn is_picker(&self) -> bool {
        matches!(
            self,
            Step::Action | Step::SignInPick | Step::SignInMethod | Step::AddConfirm | Step::Configured
        )
    }

    /// Whether the wizard is waiting on work it cannot cancel from a keypress.
    pub fn is_busy(&self) -> bool {
        matches!(self, Step::AddProbe | Step::SignInRunning)
    }
}

/// What the wizard needs the app to do after a transition.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardAction {
    /// Redraw only.
    None,
    /// Leave the wizard (`Esc` or the close row).
    Close,
    /// Probe the draft endpoint's models in the background.
    ProbeModels(ProviderInstance),
    /// Run the selected sign-in method in the background.
    BeginSignIn {
        instance: String,
        method: &'static str,
        secret: Option<String>,
    },
    /// Write this instance to `providers.toml` and install it in the pool.
    Persist(ProviderInstance),
    /// Write this plugin-provided instance (no key: its credential lives in
    /// the plugin's own store) and install it in the pool. Used after a
    /// successful sign-in on a route that was not configured yet.
    InstallDefaults(ProviderInstance),
}

/// The in-progress "add an OpenAI-compatible provider" draft.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddDraft {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
    /// Models the endpoint advertised on probe, if the probe ran.
    pub probed: Option<Vec<ModelInfo>>,
    /// Why the probe failed, shown on the confirm screen as a note.
    pub probe_error: Option<String>,
}

impl AddDraft {
    /// The instance this draft describes. An empty key stays `None` so a
    /// keyless endpoint reads as keyless in `providers.toml`.
    pub fn to_instance(&self) -> ProviderInstance {
        let api_key = self.api_key.trim();
        ProviderInstance {
            name: self.name.trim().to_string(),
            protocol: None,
            base_url: Some(self.base_url.trim().to_string()),
            api_key: if api_key.is_empty() {
                None
            } else {
                Some(api_key.to_string())
            },
            models: self
                .models
                .iter()
                .map(|id| ModelInfo::from_id(id.clone()))
                .collect(),
            // Leave the default alone: empty models means "discover them".
            fetch_models: None,
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        }
    }

    /// Model count the confirm screen advertises.
    pub fn model_count(&self) -> usize {
        if self.models.is_empty() {
            self.probed.as_ref().map(Vec::len).unwrap_or(0)
        } else {
            self.models.len()
        }
    }
}

/// The whole wizard state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wizard {
    pub step: Step,
    /// Highlighted row within the current step's list.
    pub selected: usize,
    pub targets: Vec<SignInTarget>,
    /// Discarded once sign-in replaces the menu, so it may be empty.
    pub add: AddDraft,
    /// The chosen sign-in target, kept across method/prompt steps.
    pub chosen: Option<SignInTarget>,
    /// The chosen method, kept across the secret prompt.
    pub method: Option<AuthMethod>,
    /// The secret collected for a `SecretPrompt` method.
    pub secret: Option<String>,
    /// A line the running step shows (`Signing in…`, the device URL, an error).
    pub status: Option<String>,
    /// Configured providers, captured when the list step opens.
    pub configured: Vec<ConfiguredRow>,
}

/// One row of the read-only configured list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredRow {
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub models: usize,
    /// The sign-in badge for a plugin-provided protocol, if it has one.
    pub status: Option<AuthStatus>,
}

/// Why a text value was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldError {
    EmptyName,
    NameHasSlash,
    NameTaken { name: String },
    EmptyBaseUrl,
    InvalidModelId { id: String },
}

impl FieldError {
    pub fn message(&self) -> String {
        match self {
            FieldError::EmptyName => "a route name is required".to_string(),
            FieldError::NameHasSlash => "a route name must not contain `/`".to_string(),
            FieldError::NameTaken { name } => {
                format!("`{name}` is already configured; pick another name")
            }
            FieldError::EmptyBaseUrl => "a base URL is required".to_string(),
            FieldError::InvalidModelId { id } => format!("`{id}` is not a usable model id"),
        }
    }
}

impl Wizard {
    /// Open on the action menu.
    pub fn new(targets: Vec<SignInTarget>) -> Self {
        Self {
            step: Step::Action,
            selected: 0,
            targets,
            add: AddDraft::default(),
            chosen: None,
            method: None,
            secret: None,
            status: None,
            configured: Vec::new(),
        }
    }

    /// The action rows, in menu order. `Sign in` only appears when at least one
    /// plugin contributed an entry — the wizard must not offer a dead end.
    pub fn actions(&self) -> Vec<Action> {
        let mut actions = vec![Action::AddOpenAi];
        if !self.targets.is_empty() {
            actions.push(Action::SignIn);
        }
        actions.push(Action::Configured);
        actions.push(Action::Close);
        actions
    }

    /// Move the highlight, wrapping.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.row_count();
        if len == 0 {
            return;
        }
        let current = self.selected.min(len - 1);
        self.selected = if delta < 0 {
            (current + len - 1) % len
        } else {
            (current + 1) % len
        };
    }

    /// How many rows the current step shows.
    pub fn row_count(&self) -> usize {
        match self.step {
            Step::Action => self.actions().len(),
            Step::SignInPick => self.targets.len(),
            Step::SignInMethod => self
                .chosen
                .as_ref()
                .map(|target| target.provider.methods.len())
                .unwrap_or(0),
            Step::AddConfirm => 1,
            Step::Configured => self.configured.len(),
            _ => 0,
        }
    }

    /// The field the current step collects, if any.
    pub fn field(&self) -> Option<Field> {
        self.step.field()
    }

    /// The chosen method's kind, once a method is picked.
    pub fn method_kind(&self) -> Option<AuthMethodKind> {
        self.method.map(|method| method.kind)
    }

    /// Accept the highlighted row of a picker step. Name validation happens in
    /// [`Wizard::submit_field`], because rows carry no text.
    pub fn confirm(&mut self) -> WizardAction {
        match self.step {
            Step::Action => self.confirm_action(),
            Step::SignInPick => self.confirm_target(),
            Step::SignInMethod => self.confirm_method(),
            Step::AddConfirm => {
                let draft = self.add.to_instance();
                self.status = None;
                WizardAction::Persist(draft)
            }
            _ => WizardAction::None,
        }
    }

    fn confirm_action(&mut self) -> WizardAction {
        let actions = self.actions();
        match actions.get(self.selected.min(actions.len().saturating_sub(1))) {
            Some(Action::AddOpenAi) => {
                self.add = AddDraft::default();
                self.step = Step::AddName;
                self.selected = 0;
                WizardAction::None
            }
            Some(Action::SignIn) => {
                self.step = Step::SignInPick;
                self.selected = 0;
                WizardAction::None
            }
            Some(Action::Configured) => {
                self.step = Step::Configured;
                self.selected = 0;
                WizardAction::None
            }
            _ => WizardAction::Close,
        }
    }

    fn confirm_target(&mut self) -> WizardAction {
        let Some(target) = self.targets.get(self.selected.min(self.targets.len().saturating_sub(1))).cloned()
        else {
            return WizardAction::Close;
        };
        self.chosen = Some(target.clone());
        self.secret = None;
        self.status = None;

        // A signed-in target first offers to redo or leave it alone; that
        // choice is folded into the method list as its first row so the
        // machine stays a single picker chain.
        if target.provider.methods.len() > 1 {
            self.step = Step::SignInMethod;
            self.selected = 0;
            return WizardAction::None;
        }
        let Some(method) = target.provider.methods.first().copied() else {
            self.status = Some(format!("{} offers no sign-in method", target.provider.label));
            return WizardAction::None;
        };
        self.method = Some(method);
        self.after_method_chosen(target, method)
    }

    fn confirm_method(&mut self) -> WizardAction {
        let Some(target) = self.chosen.clone() else {
            return WizardAction::Close;
        };
        let Some(method) = target.provider.methods.get(self.selected).copied() else {
            return WizardAction::None;
        };
        self.method = Some(method);
        self.after_method_chosen(target, method)
    }

    fn after_method_chosen(&mut self, target: SignInTarget, method: AuthMethod) -> WizardAction {
        match method.kind {
            AuthMethodKind::Browser => {
                self.step = Step::SignInRunning;
                self.status = Some(format!("starting {} sign-in…", target.provider.label));
                WizardAction::BeginSignIn {
                    instance: target.instance,
                    method: method.id,
                    secret: None,
                }
            }
            AuthMethodKind::SecretPrompt => {
                self.step = Step::SignInSecret;
                self.selected = 0;
                WizardAction::None
            }
        }
    }

    /// Advance from a text field with its trimmed value. Returns the action to
    /// run, or a refusal that leaves the wizard on the same step.
    pub fn submit_field(
        &mut self,
        value: &str,
        existing_names: &[String],
    ) -> Result<WizardAction, FieldError> {
        let value = value.trim();
        match self.step {
            Step::AddName => {
                if value.is_empty() {
                    return Err(FieldError::EmptyName);
                }
                if value.contains('/') {
                    return Err(FieldError::NameHasSlash);
                }
                if existing_names.iter().any(|name| name == value) {
                    return Err(FieldError::NameTaken {
                        name: value.to_string(),
                    });
                }
                self.add.name = value.to_string();
                self.step = Step::AddBaseUrl;
                Ok(WizardAction::None)
            }
            Step::AddBaseUrl => {
                if value.is_empty() {
                    return Err(FieldError::EmptyBaseUrl);
                }
                self.add.base_url = value.to_string();
                self.step = Step::AddApiKey;
                Ok(WizardAction::None)
            }
            Step::AddApiKey => {
                // An empty key is valid: local servers need none.
                self.add.api_key = value.to_string();
                self.step = Step::AddModels;
                Ok(WizardAction::None)
            }
            Step::AddModels => {
                let models = parse_model_ids(value)?;
                self.add.models = models;
                self.add.probe_error = None;
                self.step = Step::AddProbe;
                Ok(WizardAction::ProbeModels(self.add.to_instance()))
            }
            Step::SignInSecret => {
                if value.is_empty() {
                    return Err(FieldError::EmptyName);
                }
                let Some(target) = self.chosen.clone() else {
                    return Ok(WizardAction::Close);
                };
                let Some(method) = self.method else {
                    return Ok(WizardAction::Close);
                };
                self.secret = Some(value.to_string());
                self.step = Step::SignInRunning;
                self.status = Some(format!("signing in to {}…", target.provider.label));
                Ok(WizardAction::BeginSignIn {
                    instance: target.instance,
                    method: method.id,
                    secret: Some(value.to_string()),
                })
            }
            _ => Ok(WizardAction::None),
        }
    }

    /// The probe finished: record the outcome and move to the confirm screen.
    /// A failed probe is not fatal — the endpoint may not expose `/models`, and
    /// manually entered ids still work.
    pub fn probe_finished(&mut self, result: Result<Vec<ModelInfo>, String>) {
        if self.step != Step::AddProbe {
            return;
        }
        match result {
            Ok(models) => {
                self.add.probed = Some(models);
                self.add.probe_error = None;
            }
            Err(error) => {
                self.add.probed = None;
                self.add.probe_error = Some(error);
            }
        }
        self.step = Step::AddConfirm;
        self.selected = 0;
    }

    /// Report progress from a running sign-in. The URL, when present, is
    /// appended so the human can open it.
    pub fn sign_in_notice(&mut self, message: String, url: Option<String>) {
        self.status = Some(match url {
            Some(url) => format!("{message} — {url}"),
            None => message,
        });
    }

    /// The sign-in finished. On success this returns an action to install a
    /// default instance when the plugin's route was not configured yet — the
    /// credential exists, but the endpoint still has to reach `providers.toml`.
    pub fn sign_in_finished(&mut self, result: Result<Option<String>, String>) -> WizardAction {
        let chosen = self.chosen.clone();
        let succeeded = result.is_ok();
        match result {
            Ok(account) => {
                let label = chosen
                    .as_ref()
                    .map(|target| target.provider.label)
                    .unwrap_or("provider");
                self.status = Some(match account {
                    Some(account) => format!("signed in to {label} ({account})"),
                    None => format!("signed in to {label}"),
                });
            }
            Err(error) => self.status = Some(format!("sign-in failed: {error}")),
        }
        let action = match chosen {
            Some(target) if succeeded && !target.configured => {
                WizardAction::InstallDefaults(default_instance(&target))
            }
            _ => WizardAction::None,
        };
        self.step = Step::Action;
        self.selected = 0;
        self.chosen = None;
        self.method = None;
        self.secret = None;
        action
    }

    /// Go back one step, or close from the action menu.
    pub fn back(&mut self) -> WizardAction {
        match self.step {
            Step::Action => WizardAction::Close,
            Step::AddName => WizardAction::Close,
            Step::AddBaseUrl => {
                self.step = Step::AddName;
                WizardAction::None
            }
            Step::AddApiKey => {
                self.step = Step::AddBaseUrl;
                WizardAction::None
            }
            Step::AddModels => {
                self.step = Step::AddApiKey;
                WizardAction::None
            }
            Step::AddProbe | Step::AddConfirm => {
                self.step = Step::AddModels;
                WizardAction::None
            }
            Step::SignInMethod => {
                self.step = Step::SignInPick;
                self.chosen = None;
                WizardAction::None
            }
            Step::SignInSecret => {
                self.step = Step::SignInMethod;
                self.method = None;
                WizardAction::None
            }
            Step::SignInRunning => WizardAction::Close,
            Step::SignInPick | Step::Configured => {
                self.step = Step::Action;
                self.selected = 0;
                WizardAction::None
            }
        }
    }

    /// The footer hint for the current step.
    pub fn hint(&self) -> &'static str {
        match self.step {
            Step::Action | Step::SignInPick | Step::SignInMethod => {
                "↑↓ select · Enter confirm · Esc close"
            }
            Step::AddConfirm => "Enter write · Esc back",
            Step::AddProbe | Step::SignInRunning => "working… · Esc cancel",
            Step::Configured => "Esc back",
            _ => "Enter next · Esc back",
        }
    }
}

/// The instance a successful sign-in creates when its route was not configured.
/// The plugin named the protocol and (optionally) the default endpoint; the
/// credential itself is the plugin's business, so no `api_key` is written.
pub fn default_instance(target: &SignInTarget) -> ProviderInstance {
    ProviderInstance {
        name: target.instance.clone(),
        protocol: Some(target.provider.protocol.to_string()),
        base_url: target.provider.default_base_url.map(str::to_string),
        api_key: None,
        models: Vec::new(),
        fetch_models: Some(true),
        headers: Default::default(),
        body_overrides: Default::default(),
        wire: None,
        options: Default::default(),
        requires_api_key: false,
    }
}

/// Split a comma/whitespace separated list, dropping empties and duplicates.
fn parse_model_ids(value: &str) -> Result<Vec<String>, FieldError> {
    let mut models: Vec<String> = Vec::new();
    for piece in value.split([',', ' ', '\t', '\n']) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        if !models.iter().any(|existing| existing == piece) {
            models.push(piece.to_string());
        }
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BROWSER: AuthMethod = AuthMethod {
        id: "browser",
        label: "Sign in with a browser",
        kind: AuthMethodKind::Browser,
    };
    const PAT: AuthMethod = AuthMethod {
        id: "pat",
        label: "Paste a Personal Access Token",
        kind: AuthMethodKind::SecretPrompt,
    };

    static METHODS: [AuthMethod; 2] = [BROWSER, PAT];

    static QODER: AuthProvider = AuthProvider {
        id: "qoder",
        protocol: "qoder",
        label: "Qoder",
        default_base_url: Some("https://api3.qoder.sh/"),
        methods: &METHODS,
    };

    fn target() -> SignInTarget {
        SignInTarget {
            instance: "qoder".to_string(),
            configured: true,
            provider: QODER,
            status: AuthStatus::SignedOut,
        }
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn action_rows_hide_sign_in_without_a_plugin_entry() {
        let wizard = Wizard::new(Vec::new());
        assert_eq!(
            wizard.actions(),
            vec![Action::AddOpenAi, Action::Configured, Action::Close]
        );

        let wizard = Wizard::new(vec![target()]);
        assert_eq!(
            wizard.actions(),
            vec![
                Action::AddOpenAi,
                Action::SignIn,
                Action::Configured,
                Action::Close
            ]
        );
    }

    #[test]
    fn adding_a_provider_walks_name_url_key_models_then_probes() {
        let mut wizard = Wizard::new(Vec::new());
        // Row 0 is "Add an OpenAI-compatible provider".
        assert_eq!(wizard.confirm(), WizardAction::None);
        assert_eq!(wizard.step, Step::AddName);

        assert_eq!(wizard.submit_field("  gateway  ", &[]), Ok(WizardAction::None));
        assert_eq!(wizard.step, Step::AddBaseUrl);
        assert_eq!(wizard.add.name, "gateway");

        assert_eq!(
            wizard.submit_field("http://127.0.0.1:8080/v1", &[]),
            Ok(WizardAction::None)
        );
        assert_eq!(wizard.step, Step::AddApiKey);

        // An empty key is accepted: keyless local endpoints exist.
        assert_eq!(wizard.submit_field("", &[]), Ok(WizardAction::None));
        assert_eq!(wizard.step, Step::AddModels);

        let action = wizard
            .submit_field("alpha, beta alpha,alpha", &[])
            .expect("models");
        assert_eq!(wizard.step, Step::AddProbe);
        assert_eq!(wizard.add.models, vec!["alpha", "beta"], "duplicates collapse");
        match action {
            WizardAction::ProbeModels(instance) => {
                assert_eq!(instance.name, "gateway");
                assert_eq!(instance.api_key, None);
                assert_eq!(instance.protocol, None);
                assert_eq!(instance.models.len(), 2);
            }
            other => panic!("expected a probe, got {other:?}"),
        }
    }

    #[test]
    fn name_validation_rejects_empty_slashes_and_duplicates() {
        let mut wizard = Wizard::new(Vec::new());
        wizard.confirm();
        assert_eq!(wizard.submit_field("", &[]), Err(FieldError::EmptyName));
        assert_eq!(wizard.submit_field("a/b", &[]), Err(FieldError::NameHasSlash));
        assert_eq!(
            wizard.submit_field("taken", &names(&["taken"])),
            Err(FieldError::NameTaken {
                name: "taken".to_string()
            })
        );
        assert_eq!(wizard.step, Step::AddName, "a refusal keeps the step");
    }

    #[test]
    fn a_failed_probe_still_reaches_the_confirm_screen() {
        let mut wizard = Wizard::new(Vec::new());
        wizard.confirm();
        wizard.submit_field("gw", &[]).unwrap();
        wizard.submit_field("http://x/v1", &[]).unwrap();
        wizard.submit_field("$KEY", &[]).unwrap();
        wizard.submit_field("", &[]).unwrap();
        assert_eq!(wizard.step, Step::AddProbe);

        wizard.probe_finished(Err("models request timed out".to_string()));
        assert_eq!(wizard.step, Step::AddConfirm);
        assert_eq!(wizard.add.probe_error.as_deref(), Some("models request timed out"));
        assert_eq!(wizard.add.model_count(), 0);

        match wizard.confirm() {
            WizardAction::Persist(instance) => {
                assert_eq!(instance.name, "gw");
                assert_eq!(instance.api_key.as_deref(), Some("$KEY"));
                assert!(instance.models.is_empty(), "empty = discover them");
            }
            other => panic!("expected a persist, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_probe_reports_the_discovered_count() {
        let mut wizard = Wizard::new(Vec::new());
        wizard.step = Step::AddProbe;
        wizard.add = AddDraft {
            name: "gw".to_string(),
            base_url: "http://x/v1".to_string(),
            ..Default::default()
        };
        wizard.probe_finished(Ok(vec![ModelInfo::from_id("m1"), ModelInfo::from_id("m2")]));
        assert_eq!(wizard.step, Step::AddConfirm);
        assert_eq!(wizard.add.model_count(), 2);
    }

    #[test]
    fn two_methods_open_a_picker_and_one_runs_directly() {
        let mut wizard = Wizard::new(vec![target()]);
        // Action: row 1 is "Sign in".
        wizard.move_selection(1);
        wizard.confirm();
        assert_eq!(wizard.step, Step::SignInPick);

        // Two methods: pick the second (PAT).
        wizard.confirm();
        assert_eq!(wizard.step, Step::SignInMethod);
        assert_eq!(wizard.row_count(), 2);
        wizard.move_selection(1);
        assert_eq!(wizard.confirm(), WizardAction::None);
        assert_eq!(wizard.step, Step::SignInSecret, "PAT needs a secret");

        let action = wizard
            .submit_field("pt-123", &[])
            .expect("secret accepted");
        assert_eq!(
            action,
            WizardAction::BeginSignIn {
                instance: "qoder".to_string(),
                method: "pat",
                secret: Some("pt-123".to_string()),
            }
        );
        assert_eq!(wizard.step, Step::SignInRunning);
    }

    #[test]
    fn a_single_browser_method_starts_immediately() {
        static ONE: [AuthMethod; 1] = [BROWSER];
        static CN: AuthProvider = AuthProvider {
            id: "qoder-cn",
            protocol: "qoder",
            label: "Qoder CN",
            default_base_url: Some("https://gateway.qoder.com.cn/"),
            methods: &ONE,
        };
        let mut wizard = Wizard::new(vec![SignInTarget {
            instance: "qoder-cn".to_string(),
            configured: false,
            provider: CN,
            status: AuthStatus::SignedOut,
        }]);
        wizard.move_selection(1);
        wizard.confirm();
        wizard.confirm();
        assert_eq!(wizard.step, Step::SignInRunning);
        match wizard.sign_in_finished(Ok(None)) {
            WizardAction::InstallDefaults(instance) => {
                assert_eq!(instance.name, "qoder-cn");
                assert_eq!(instance.protocol.as_deref(), Some("qoder"));
                assert_eq!(
                    instance.base_url.as_deref(),
                    Some("https://gateway.qoder.com.cn/")
                );
                assert_eq!(instance.api_key, None);
                assert_eq!(instance.fetch_models, Some(true));
            }
            other => panic!("expected a default install, got {other:?}"),
        }
    }

    #[test]
    fn notices_and_completion_land_in_status_and_reset_the_flow() {
        let mut wizard = Wizard::new(vec![target()]);
        wizard.chosen = Some(target());
        wizard.step = Step::SignInRunning;

        wizard.sign_in_notice(
            "Approve in your browser".to_string(),
            Some("https://qoder.com/device".to_string()),
        );
        assert_eq!(
            wizard.status.as_deref(),
            Some("Approve in your browser — https://qoder.com/device")
        );

        // The target was already configured, so no default install is needed.
        let action = wizard.sign_in_finished(Ok(Some("me@example.com".to_string())));
        assert_eq!(action, WizardAction::None);
        assert_eq!(wizard.step, Step::Action);
        assert_eq!(
            wizard.status.as_deref(),
            Some("signed in to Qoder (me@example.com)")
        );
        assert!(wizard.chosen.is_none());
        assert!(wizard.method.is_none());
    }

    #[test]
    fn back_walks_the_add_flow_and_closes_from_the_top() {
        let mut wizard = Wizard::new(Vec::new());
        wizard.step = Step::AddModels;
        assert_eq!(wizard.back(), WizardAction::None);
        assert_eq!(wizard.step, Step::AddApiKey);
        wizard.back();
        assert_eq!(wizard.step, Step::AddBaseUrl);
        wizard.back();
        assert_eq!(wizard.step, Step::AddName);
        assert_eq!(wizard.back(), WizardAction::Close);
    }

    #[test]
    fn signed_in_targets_are_labelled() {
        let mut row = target();
        assert_eq!(row.status_label(), "not signed in");
        assert!(!row.is_signed_in());
        row.status = AuthStatus::SignedIn {
            account: Some("me".to_string()),
        };
        assert_eq!(row.status_label(), "signed in");
        assert!(row.is_signed_in());
        row.status = AuthStatus::Expired;
        assert_eq!(row.status_label(), "sign-in expired");
    }
}
