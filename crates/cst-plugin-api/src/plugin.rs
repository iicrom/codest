//! The plugin contract: commands, hooks, tools, protocols, and sign-in.
//!
//! Plugins are ordinary Rust values linked into the binary. This module holds
//! the *contract* — the trait and the data it exchanges with the host. The
//! host owns discovery, validation, and dispatch; a plugin only ever sees
//! [`PluginInfo`] (read-only facts) and returns [`PluginEffect`]s (data, never
//! terminal calls). Keeping effects data-only is what lets every front-end
//! apply them itself and keeps plugins independent of ratatui.
//!
//! ```text
//! TUI ──parse/dispatch──▶ PluginHost ──calls──▶ Plugin
//!   ▲                                            │
//!   └──────────── PluginEffect (data) ───────────┘
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::auth::SignInFuture;
use crate::auth::SignInNotifier;
use crate::commands::CommandSpec;
use crate::commands::Invocation;
use crate::config::PluginSettings;
use crate::event::EventSubscription;
use crate::event::PluginEvent;
use crate::providers::ProtocolFactory;
use crate::status::StatusSegment;
use crate::tools::ToolCall;
use crate::tools::ToolSpec;

/// A background task a plugin handed to the host via
/// [`PluginContext::spawn`]. The host drives it on its own runtime and
/// cancels it on exit; a long-lived task should watch
/// [`PluginBridge::cancelled`] and end when it fires.
pub type PluginTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// One compile-time extension.
///
/// Plugins are shared immutably (`&'static dyn Plugin`), so hooks and command
/// handlers take `&self`. A plugin that needs mutable state should use interior
/// mutability (`Mutex`, atomics, …), exactly like the rest of the static
/// registry.
pub trait Plugin: Sync {
    /// Stable identifier used in diagnostics and `/status`.
    fn id(&self) -> &'static str;

    /// Commands contributed by this plugin, in menu order.
    fn commands(&self) -> &'static [CommandSpec] {
        &[]
    }

    /// Extra system-prompt text appended when the session starts. Skills,
    /// workspace instructions, and similar context are expected to arrive
    /// through this hook as ordinary prompt fragments — the core treats all
    /// of them alike, and `/context` shows each fragment attributed to its
    /// plugin.
    fn system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// Tools offered to the model on every turn. Declaring a tool here also
    /// makes this plugin the executor of its calls via [`Plugin::run_tool`].
    fn tools(&self) -> &'static [ToolSpec] {
        &[]
    }

    /// Execute one call of a tool this plugin declared via [`Plugin::tools`].
    /// `Ok` output becomes the tool message content; `Err` reaches both the
    /// model (as `error: …` content) and the transcript (as a failure line).
    fn run_tool(&self, _call: &ToolCall) -> Result<String, PluginError> {
        Err(PluginError::new(
            "this plugin declares no tool executor",
        ))
    }

    /// Hand this plugin the front-end's prompt service, once at startup.
    ///
    /// A plugin cannot draw, so a tool that needs a decision from the human
    /// asks the front-end through [`crate::prompt::PromptHost`] and blocks
    /// until the answer arrives. The host installs the service before any tool
    /// can run; a plugin that never asks keeps the default no-op and behaves
    /// exactly as before. All shipped plugins share one service, so the value
    /// is cheap to clone and store behind the plugin's own `Mutex`.
    fn set_prompt_host(&self, _host: Arc<dyn crate::prompt::PromptHost>) {}

    /// Protocol kinds contributed to the provider registry. A custom
    /// provider = a `ProtocolFactory` here (the wire protocol, compile-time)
    /// plus instances in `providers.toml` (the endpoints, runtime config).
    fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
        &[]
    }

    /// Sign-in entries this plugin offers to `/provider`. Each one names the
    /// protocol it authenticates; the host pairs configured instances with it.
    fn auth_providers(&self) -> &'static [crate::auth::AuthProvider] {
        &[]
    }

    /// The stored-credential state for `instance`, from local storage only.
    ///
    /// This runs on the TUI thread while the menu is built, so it must not
    /// perform network I/O or block. A plugin that cannot answer cheaply
    /// returns `Unsupported`.
    fn auth_status(&self, _instance: &str) -> crate::auth::AuthStatus {
        crate::auth::AuthStatus::Unsupported
    }

    /// Start one sign-in and return the future that drives it.
    ///
    /// `instance` is the configured provider name (`"qoder"`), `method` an
    /// [`crate::auth::AuthMethod::id`], and `secret` the value the host
    /// collected for a [`crate::auth::AuthMethodKind::SecretPrompt`] method.
    /// The future must honour `cancel` and report progress through `notify`;
    /// it persists the resulting credential itself and returns an outcome the
    /// host turns into a confirmation line.
    ///
    /// Returning `None` means this plugin does not handle that
    /// instance/method pair.
    fn begin_sign_in(
        &self,
        _instance: &str,
        _method: &str,
        _secret: Option<String>,
        _notify: SignInNotifier,
        _cancel: CancellationToken,
    ) -> Option<SignInFuture> {
        None
    }

    /// Called once after the session is created and before the first frame.
    fn on_startup(&self, _ctx: &mut PluginContext<'_>) {}

    /// Called when a user turn is submitted, before it reaches the model.
    fn on_user_message(&self, _text: &str, _ctx: &mut PluginContext<'_>) {}

    /// Called when a turn finishes successfully, with the final assistant text.
    fn on_turn_complete(&self, _message: &str, _ctx: &mut PluginContext<'_>) {}

    /// Called after the core confirms that the conversation was cleared.
    fn on_conversation_cleared(&self, _ctx: &mut PluginContext<'_>) {}

    /// The settings section `/settings` shows for this plugin.
    ///
    /// Every field names a key of this plugin's `[plugins."<id>"]` pocket in
    /// `config.toml`; the host renders the rows, persists an edit atomically,
    /// and then calls [`Plugin::on_settings_changed`] with the whole pocket as
    /// it now stands. Returning `None` (the default) keeps the plugin out of
    /// `/settings` — a plugin with nothing to configure declares nothing.
    fn settings(&self) -> Option<crate::settings::SettingsSection> {
        None
    }

    /// Apply a settings edit. `settings` is the plugin's whole pocket as it
    /// was just written (`None` when the edit removed the last key), so a
    /// plugin reads the keys it cares about and ignores the rest —
    /// [`crate::config::PluginSettings`] offers the typed readers.
    ///
    /// Called after the file write succeeded, on the front-end's thread, so it
    /// must be quick. The edit persists either way; a value that only takes
    /// effect at startup is declared as such
    /// ([`crate::settings::SettingField::restart`]) and needs no work here.
    fn on_settings_changed(
        &self,
        _settings: Option<&crate::config::PluginSettings>,
        _ctx: &mut PluginContext<'_>,
    ) {
    }

    /// Which event categories this plugin wants on [`Plugin::on_event`].
    ///
    /// The default subscribes to nothing: the host filters before dispatching,
    /// so an uninterested plugin never sees a copy of a stream delta. Declare
    /// the narrowest set that serves the plugin — `DELTAS` events arrive once
    /// per streamed chunk.
    fn subscriptions(&self) -> EventSubscription {
        EventSubscription::NONE
    }

    /// Called for every core event in a subscribed category, in emit order.
    ///
    /// `event.at` is the host's monotonic emit stamp; payload borrows end
    /// when the dispatch returns, so a plugin that must keep a delta copies
    /// it. Effects collected here (transcript cells, status segments, spawned
    /// tasks) apply like any other hook's — see [`PluginContext`].
    fn on_event(&self, _event: &PluginEvent<'_>, _ctx: &mut PluginContext<'_>) {}

    /// Handle one invocation. Returning `Err` becomes a visible error effect;
    /// returning `Ok` with no effects is a silent no-op.
    fn run_command(
        &self,
        invocation: &Invocation,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        Err(PluginError::new(format!(
            "plugin `{}` does not handle `/{}`",
            self.id(),
            invocation.spec.name
        )))
    }
}

/// A side effect requested by a plugin.
///
/// The host collects effects in plugin order; the front-end applies them in the
/// same order. This is the only way a plugin can touch the UI or the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginEffect {
    /// Append a dim notice to the transcript.
    Notice(String),
    /// Append an error cell to the transcript.
    Error(String),
    /// Append a titled report block to the transcript (`/context`).
    Report(ReportBlock),
    /// Drop the transcript and the core conversation.
    ClearConversation,
    /// Exit the application.
    Quit,
    /// Replace the composer contents.
    SetComposer(String),
    /// Submit a new user turn, if the agent is idle.
    SubmitUserTurn(String),
    /// Continue the active conversation without a new user message
    /// (`/continue`): the model is driven from the transcript as it stands —
    /// the recovery path for an interrupted turn. Honored only while the
    /// agent is idle and a model is usable.
    ContinueTurn,
    /// Set or replace one status-bar segment. `segment.key` is already the
    /// full `plugin:key` id — the [`PluginContext`] and [`PluginBridge`]
    /// helpers resolve the prefix, so effects always carry the scoped form.
    /// The host owns layout and truncation; a segment that no longer fits is
    /// dropped, not drawn over its neighbors.
    SetStatus(StatusSegment),
    /// Remove one status-bar segment by its full `plugin:key` id. Removing
    /// an absent key is a no-op.
    ClearStatus(String),
}

/// A plugin-authored report (e.g. `/context`): a bold title over titled
/// groups, whose entries carry an optional highlighted label and dim text —
/// the shape mirrors the loaded-context panels of reference TUIs. Pure data:
/// the front-end decides the actual styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportBlock {
    pub title: String,
    pub groups: Vec<ReportGroup>,
}

/// One titled group of a [`ReportBlock`], rendered as a bold section marker
/// over its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportGroup {
    pub heading: String,
    pub entries: Vec<ReportEntry>,
}

/// One entry of a [`ReportGroup`]: an optional highlighted label followed by
/// the dim entry text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    /// Highlighted before the text (`model`, `base`); empty for label-less
    /// entries.
    pub label: String,
    /// Entry body; may span multiple lines.
    pub text: String,
}

impl ReportEntry {
    pub fn new(label: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            text: text.into(),
        }
    }
}

/// Per-request wire facts for `/context`-style displays: where the request
/// goes and which static fields ride along.
#[derive(Debug, Clone, Copy)]
pub struct RequestFacts<'a> {
    pub protocol: &'a str,
    pub base_url: &'a str,
    /// The full chat-completions endpoint (`base_url` + `/chat/completions`).
    pub endpoint: &'a str,
    /// The instance's `body_overrides` as compact `key=value` strings.
    pub overrides: &'a [(String, String)],
}

/// Read-only information passed to every hook and command handler.
#[derive(Debug, Clone, Copy)]
pub struct PluginInfo<'a> {
    pub model: &'a str,
    pub mode: &'a str,
    pub transcript_len: usize,
    /// The active model's context window, when declared or discovered.
    pub context_window: Option<u64>,
    /// Usage of the last completed turn: `(input, output)` tokens. `None`
    /// before the first measured turn of this run.
    pub last_usage: Option<(u64, u64)>,
    /// Cumulative measured tokens across the session's completed turns.
    pub total_input: u64,
    pub total_output: u64,
    pub plugin_ids: &'a [&'static str],
    /// The assembled system prompt as labeled sections: `base` first, then
    /// one `plugin:<id>` entry per non-empty fragment.
    pub system_sections: &'a [(String, String)],
    /// The reasoning effort sent with each turn (`/effort`); `None` sends no
    /// effort field at all.
    pub effort: Option<&'a str>,
    /// Where requests go and which static fields ride along.
    pub request: RequestFacts<'a>,
    /// Tools the plugins declared, in registration order.
    pub tools: &'a [ToolSpec],
    /// The subset of [`Self::tools`] enabled in the active session (the
    /// `/tools` switch). Equal to every declared name until a session
    /// narrows it; empty is a legitimate chat-only session.
    pub enabled_tools: &'a [String],
    /// Approximate message count of the active session's conversation: the
    /// system prompt plus one entry per user/assistant/tool transcript cell.
    pub message_count: usize,
    /// This run's per-plugin settings, parsed from `config.toml`'s
    /// `[plugins."<id>"]` tables, in table order. The host interprets no
    /// plugin key; a plugin reads its own pocket through
    /// [`PluginContext::plugin_settings`].
    pub plugin_configs: &'a [(String, PluginSettings)],
}

/// Mutable context handed to a plugin while it runs.
///
/// The context exposes read-only session facts and collects effects. It never
/// gives the plugin direct access to the terminal or the agent worker. One
/// context serves one plugin: `plugin_id` scopes the status-bar helpers, and
/// `plugin_settings` is that plugin's own `[plugins."<id>"]` pocket.
pub struct PluginContext<'a> {
    table: &'a crate::commands::CommandTable,
    info: PluginInfo<'a>,
    plugin_id: &'static str,
    settings: Option<&'a PluginSettings>,
    bridge: PluginBridge,
    effects: Vec<PluginEffect>,
    tasks: Vec<PluginTask>,
}

impl<'a> PluginContext<'a> {
    /// Build a context. The host calls this; plugins only ever receive one.
    /// `settings` is resolved from `info.plugin_configs` by `plugin_id`.
    pub fn new(
        table: &'a crate::commands::CommandTable,
        info: PluginInfo<'a>,
        plugin_id: &'static str,
        bridge: PluginBridge,
    ) -> Self {
        let settings = info
            .plugin_configs
            .iter()
            .find(|(id, _)| id == plugin_id)
            .map(|(_, settings)| settings);
        Self {
            table,
            info,
            plugin_id,
            settings,
            bridge,
            effects: Vec::new(),
            tasks: Vec::new(),
        }
    }

    /// The full command table, useful for `/help`-style commands.
    pub fn command_table(&self) -> &crate::commands::CommandTable {
        self.table
    }

    pub fn model(&self) -> &str {
        self.info.model
    }

    pub fn mode(&self) -> &str {
        self.info.mode
    }

    pub fn transcript_len(&self) -> usize {
        self.info.transcript_len
    }

    /// The active model's context window, when known.
    pub fn context_window(&self) -> Option<u64> {
        self.info.context_window
    }

    /// The last completed turn's `(input, output)` tokens, if measured.
    pub fn last_usage(&self) -> Option<(u64, u64)> {
        self.info.last_usage
    }

    /// Cumulative measured input tokens across the session.
    pub fn total_input(&self) -> u64 {
        self.info.total_input
    }

    /// Cumulative measured output tokens across the session.
    pub fn total_output(&self) -> u64 {
        self.info.total_output
    }

    pub fn plugin_ids(&self) -> &[&'static str] {
        self.info.plugin_ids
    }

    /// The assembled system prompt as labeled sections.
    pub fn system_sections(&self) -> &[(String, String)] {
        self.info.system_sections
    }

    /// The reasoning effort sent with each turn; `None` sends no field.
    pub fn effort(&self) -> Option<&str> {
        self.info.effort
    }

    /// Where requests go and which static fields ride along.
    pub fn request(&self) -> RequestFacts<'_> {
        self.info.request
    }

    /// Tools the plugins declared, in registration order.
    pub fn tools(&self) -> &[ToolSpec] {
        self.info.tools
    }

    /// The subset of [`Self::tools`] enabled in the active session
    /// (`/tools`); every declared tool until a session narrows the set.
    pub fn enabled_tools(&self) -> &[String] {
        self.info.enabled_tools
    }

    /// Approximate message count of the active session's conversation.
    pub fn message_count(&self) -> usize {
        self.info.message_count
    }

    pub fn effects(&self) -> &[PluginEffect] {
        &self.effects
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.effects.push(PluginEffect::Notice(text.into()));
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.effects.push(PluginEffect::Error(text.into()));
    }

    pub fn report(&mut self, block: ReportBlock) {
        self.effects.push(PluginEffect::Report(block));
    }

    pub fn clear_conversation(&mut self) {
        self.effects.push(PluginEffect::ClearConversation);
    }

    pub fn quit(&mut self) {
        self.effects.push(PluginEffect::Quit);
    }

    pub fn set_composer(&mut self, text: impl Into<String>) {
        self.effects.push(PluginEffect::SetComposer(text.into()));
    }

    pub fn submit_user_turn(&mut self, text: impl Into<String>) {
        self.effects.push(PluginEffect::SubmitUserTurn(text.into()));
    }

    pub fn continue_turn(&mut self) {
        self.effects.push(PluginEffect::ContinueTurn);
    }

    /// This plugin's stable id — useful for attributing diagnostics.
    pub fn plugin_id(&self) -> &'static str {
        self.plugin_id
    }

    /// This plugin's own settings from `config.toml`'s `[plugins."<id>"]`
    /// table, when the user declared one. The host interprets none of the
    /// keys; see [`PluginSettings`].
    pub fn plugin_settings(&self) -> Option<&'a PluginSettings> {
        self.settings
    }

    /// A handle background tasks can use after this dispatch returns. Clone
    /// it into a [`PluginContext::spawn`] future to keep sending effects from
    /// there.
    pub fn bridge(&self) -> PluginBridge {
        self.bridge.clone()
    }

    /// Set or replace one status-bar segment. `segment.key` is the
    /// plugin-local key (`"rate"`); the stored full key becomes
    /// `plugin_id:key`.
    pub fn set_status(&mut self, segment: StatusSegment) {
        self.effects
            .push(PluginEffect::SetStatus(self.scope_segment(segment)));
    }

    /// Remove one of this plugin's status-bar segments, by its plugin-local
    /// key. Removing an absent segment is a no-op.
    pub fn clear_status(&mut self, key: &str) {
        self.effects
            .push(PluginEffect::ClearStatus(self.scope_key(key)));
    }

    /// Hand a background task to the host. The task outlives this dispatch:
    /// the host spawns it on its own runtime, cancels it on exit, and applies
    /// whatever it sends through [`PluginContext::bridge`]. A long-lived task
    /// (a monitor server, a refresh loop) should select on
    /// [`PluginBridge::cancelled`] so a quit ends it promptly.
    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        self.tasks.push(Box::pin(task));
    }

    /// Prefix a segment's local key with this plugin's id.
    fn scope_segment(&self, mut segment: StatusSegment) -> StatusSegment {
        segment.key = self.scope_key(&segment.key);
        segment
    }

    /// The full `plugin:key` id for a plugin-local key.
    fn scope_key(&self, key: &str) -> String {
        format!("{}:{key}", self.plugin_id)
    }

    /// Take the collected effects and spawned tasks. The host calls this
    /// after dispatch.
    pub fn into_parts(self) -> (Vec<PluginEffect>, Vec<PluginTask>) {
        (self.effects, self.tasks)
    }

    /// Take the collected effects, dropping any spawned tasks. Prefer
    /// [`PluginContext::into_parts`] — the host must forward tasks for them
    /// to run.
    pub fn into_effects(self) -> Vec<PluginEffect> {
        self.effects
    }
}

/// A cheap, clonable handle for a plugin's background tasks.
///
/// A task spawned through [`PluginContext::spawn`] outlives the dispatch that
/// created it, so it cannot borrow the (single-use, `&mut`) context. The
/// bridge is what it keeps instead: an effect sink scoped to the plugin, and
/// the host's cancellation token. Sends land on the host's effect channel
/// and are applied like any other effect.
#[derive(Clone)]
pub struct PluginBridge {
    sink: Arc<dyn Fn(PluginEffect) + Send + Sync>,
    owner: &'static str,
    cancel: CancellationToken,
}

impl PluginBridge {
    /// Build a bridge from a sink and the host's cancellation token. The host
    /// calls this; plugins receive clones from
    /// [`PluginContext::bridge`].
    pub fn new(
        owner: &'static str,
        cancel: CancellationToken,
        sink: Arc<dyn Fn(PluginEffect) + Send + Sync>,
    ) -> Self {
        Self { sink, owner, cancel }
    }

    /// Send one effect from a background task. Transcript-cell effects append
    /// to the active session's view; status segments upsert by key.
    pub fn send(&self, effect: PluginEffect) {
        (self.sink)(effect);
    }

    /// Set or replace one status-bar segment from a background task.
    /// `segment.key` is the plugin-local key.
    pub fn set_status(&self, mut segment: StatusSegment) {
        segment.key = self.scope_key(&segment.key);
        self.send(PluginEffect::SetStatus(segment));
    }

    /// Remove one status-bar segment from a background task, by its
    /// plugin-local key.
    pub fn clear_status(&self, key: &str) {
        self.send(PluginEffect::ClearStatus(self.scope_key(key)));
    }

    /// Append a transcript notice from a background task.
    pub fn notice(&self, text: impl Into<String>) {
        self.send(PluginEffect::Notice(text.into()));
    }

    /// The token the host cancels on exit. A long-lived task should select on
    /// this so a quit ends it promptly — cancellation is cooperative, and the
    /// host's final `abort` is not graceful.
    pub fn cancelled(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The full `plugin:key` id for a plugin-local key.
    fn scope_key(&self, key: &str) -> String {
        format!("{}:{key}", self.owner)
    }
}

impl std::fmt::Debug for PluginBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBridge")
            .field("owner", &self.owner)
            .finish()
    }
}

/// An error reported by a plugin. It is turned into a visible
/// [`PluginEffect::Error`] by the host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PluginError {
    message: String,
}

impl PluginError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}
