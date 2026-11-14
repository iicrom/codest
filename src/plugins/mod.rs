//! Compile-time ("static") plugin system.
//!
//! Plugins are ordinary Rust values linked into the binary. There is no
//! dynamic loading, no scripting layer, and no runtime discovery: the shipped
//! set is a `static` slice, so registration order and contents are fixed at
//! compile time. Adding a built-in plugin means adding a module and one entry
//! to [`builtin::BUILTIN_PLUGINS`]. A *local* plugin is linked by whatever
//! binary wraps this crate and injected through [`PluginHost::with_local`];
//! `codest plugin install` generates that wrapper, so the checkout itself is
//! never modified (see `docs/local-plugins.md`).
//!
//! The contract has two halves:
//!
//! * **commands** — static [`CommandSpec`] metadata plus [`Plugin::run_command`];
//! * **hooks** — lifecycle callbacks that return [`PluginEffect`]s.
//!
//! Effects are data, not terminal calls. The TUI (or any other front-end)
//! applies them, which keeps plugins independent of ratatui and makes the whole
//! system testable without a terminal.
//!
//! ```text
//! TUI ──parse/dispatch──▶ PluginHost ──calls──▶ Plugin
//!   ▲                                            │
//!   └──────────── PluginEffect (data) ───────────┘
//! ```
//!
//! The `Plugin` trait and its data types live in [`cst_plugin_api::plugin`]:
//! an external plugin crate implements the trait against that crate rather
//! than against this binary, which is what breaks the dependency cycle a
//! "link the plugin into the host" install would otherwise create. Everything
//! in this module is host-side — discovery, validation, dispatch.

pub mod builtin;
pub mod install;

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use cst_plugin_api::auth::AuthProvider;
use cst_plugin_api::auth::AuthStatus;
use cst_plugin_api::commands::CommandTableError;
use cst_plugin_api::commands::{CommandInput, CommandTable};
use cst_plugin_api::providers::ProtocolFactory;
use cst_plugin_api::tools::ToolCall;
use cst_plugin_api::tools::ToolExecutor;
use cst_plugin_api::tools::ToolSpec;
use tokio_util::sync::CancellationToken;

pub use builtin::BUILTIN_PLUGINS;
pub use cst_plugin_api::commands::{ArgsPolicy, CommandCandidate, CommandSpec, Invocation};
pub use cst_plugin_api::config::PluginSettings;
pub use cst_plugin_api::event::{DeltaKind, EventSubscription, PluginEvent, PluginEventKind};
pub use cst_plugin_api::plugin::{
    Plugin, PluginBridge, PluginContext, PluginEffect, PluginError, PluginInfo, PluginTask,
    ReportBlock, ReportEntry, ReportGroup, RequestFacts,
};
pub use cst_plugin_api::prompt::{
    PromptAnswer, PromptAnswerKind, PromptForm, PromptHost, PromptOption, PromptQuestion,
    PromptResponse,
};
pub use cst_plugin_api::status::{StatusSegment, StatusTone};

/// The effect sink every dispatch's bridges share: sends land on the host's
/// effect channel for the front-end to apply.
type EffectSink = Arc<dyn Fn(PluginEffect) + Send + Sync>;

/// The paired receiver, held until the front-end claims it exactly once.
type EffectsReceiver = Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<PluginEffect>>>;

/// What one dispatch pass produced: transcript/UI effects in plugin order,
/// plus the background tasks plugins asked to spawn
/// ([`PluginContext::spawn`]). The front-end applies the effects and drives
/// the tasks on its own runtime; the host is synchronous and owns no runtime.
pub struct PluginOutput {
    pub effects: Vec<PluginEffect>,
    pub tasks: Vec<PluginTask>,
}

impl PluginOutput {
    fn empty() -> Self {
        Self {
            effects: Vec::new(),
            tasks: Vec::new(),
        }
    }

    fn from_ctx(ctx: PluginContext<'_>) -> Self {
        let (effects, tasks) = ctx.into_parts();
        Self { effects, tasks }
    }
}

/// Why the plugin registry could not be built.
#[derive(Debug, thiserror::Error)]
pub enum PluginHostError {
    #[error("plugin id must not be empty")]
    EmptyPluginId,
    #[error("plugin id `{id}` is registered more than once")]
    DuplicatePluginId { id: &'static str },
    #[error("plugin id `{id}` is reserved for the host's own `/settings` card")]
    ReservedPluginId { id: &'static str },
    #[error("tool `{name}` is declared more than once")]
    DuplicateTool { name: &'static str },
    #[error("tool filter names unknown plugin `{id}` (known: {known})")]
    UnknownFilteredPlugin { id: String, known: String },
    #[error(transparent)]
    CommandTable(#[from] CommandTableError),
}

/// Commands the front-end ships with and handles itself. They live in the
/// shared table so parsing, the command menu, and `/help` agree with what the
/// TUI can actually do; no plugin owns them.
static SHIPPED_COMMAND_GROUPS: &[&[CommandSpec]] = &[
    crate::core::sessions::SESSION_COMMANDS,
    crate::core::providers::MODEL_COMMANDS,
    crate::core::providers::PROVIDER_COMMANDS,
    crate::core::reasoning::REASONING_COMMANDS,
    crate::core::config::PROFILE_COMMANDS,
    crate::core::tools_switch::TOOLS_COMMANDS,
    crate::core::context::CONTEXT_COMMANDS,
    crate::core::settings::SETTINGS_COMMANDS,
    crate::core::usage::USAGE_COMMANDS,
];

/// Owns the static plugin list and the command table they contribute.
pub struct PluginHost {
    plugins: Vec<&'static dyn Plugin>,
    table: CommandTable,
    ids: Vec<&'static str>,
    /// Declared tool names paired with the plugin that owns each one.
    tools: Vec<(&'static str, &'static dyn Plugin)>,
    /// Which plugins' tools are declared and executable: `None` keeps every
    /// plugin's tools; `Some(ids)` keeps only the named plugins'. Commands,
    /// hooks, prompts, and protocols are never filtered.
    tool_filter: Option<Vec<String>>,
    /// The sink background tasks send their effects through
    /// ([`PluginBridge`]). Shared by every dispatch; the paired receiver is
    /// claimed once by the front-end via [`PluginHost::take_effects_receiver`].
    effects_sink: EffectSink,
    effects_rx: EffectsReceiver,
    /// Cancellation for every spawned plugin task. The front-end cancels it
    /// on exit; tasks are expected to watch it and end promptly.
    cancel: CancellationToken,
    /// The front-end's prompt service, once installed
    /// ([`PluginHost::install_prompt_host`]). Tool plugins that ask the user a
    /// question keep this handle.
    prompt_host: Mutex<Option<Arc<dyn PromptHost>>>,
}

impl PluginHost {
    /// Validate plugin ids and build the command table they contribute.
    pub fn new(plugins: &'static [&'static dyn Plugin]) -> Result<Self, PluginHostError> {
        Self::with_extra_commands(&[plugins], &[], None)
    }

    /// Build a host whose command table also carries front-end built-in
    /// commands that no plugin owns (the core session commands, `/model`,
    /// `/provider`). Name collisions between plugins and extras are rejected
    /// like duplicates.
    ///
    /// `groups` lets the binary keep separately-owned registries — the shipped
    /// plugins and the user's local ones — without flattening them into one
    /// `static`. Flattening order is group order, then slice order; duplicate
    /// ids across groups fail here, so a local plugin cannot shadow a shipped
    /// one.
    fn with_extra_commands(
        groups: &[&'static [&'static dyn Plugin]],
        extra: &[&'static [CommandSpec]],
        tool_filter: Option<Vec<String>>,
    ) -> Result<Self, PluginHostError> {
        let plugins: Vec<&'static dyn Plugin> = groups
            .iter()
            .flat_map(|group| group.iter().copied())
            .collect();

        let mut ids = Vec::with_capacity(plugins.len());
        for plugin in &plugins {
            let id = plugin.id();
            if id.is_empty() {
                return Err(PluginHostError::EmptyPluginId);
            }
            // `/settings` shows the host's own card under this name and
            // `[plugins."<id>"]` pockets share one file, so a plugin may not
            // claim it.
            if id == cst_plugin_api::settings::HOST_NAMESPACE {
                return Err(PluginHostError::ReservedPluginId { id });
            }
            if ids.contains(&id) {
                return Err(PluginHostError::DuplicatePluginId { id });
            }
            ids.push(id);
        }

        // A tool filter naming a plugin that does not exist would silently
        // strip the wrong tools, so it is a startup error — same discipline
        // as the configuration files.
        if let Some(enabled) = &tool_filter {
            for id in enabled {
                if !ids.contains(&id.as_str()) {
                    return Err(PluginHostError::UnknownFilteredPlugin {
                        id: id.clone(),
                        known: ids.join(", "),
                    });
                }
            }
        }

        let specs = plugins
            .iter()
            .flat_map(|plugin| plugin.commands())
            .chain(extra.iter().flat_map(|slice| slice.iter()));
        let table = CommandTable::new(specs)?;

        // Tool declarations get the same duplicate treatment as ids and
        // command names: two plugins answering the same tool name would make
        // execution ambiguous.
        let mut tools = Vec::new();
        for plugin in &plugins {
            for spec in plugin.tools() {
                if tools.iter().any(|(name, _)| *name == spec.name) {
                    return Err(PluginHostError::DuplicateTool { name: spec.name });
                }
                tools.push((spec.name, *plugin));
            }
        }

        let (effects_sink, effects_rx) = Self::build_effects_channel();
        Ok(Self {
            plugins,
            table,
            ids,
            tools,
            tool_filter,
            effects_sink,
            effects_rx,
            cancel: CancellationToken::new(),
            prompt_host: Mutex::new(None),
        })
    }

    /// The effect channel every background task sends through, created per
    /// host. The sink is shared by all dispatches; the receiver is handed to
    /// the front-end exactly once.
    fn build_effects_channel() -> (EffectSink, EffectsReceiver) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PluginEffect>();
        let sink = Arc::new(move |effect: PluginEffect| {
            // A closed receiver means the front-end is gone; dropping the
            // effect is the right response, not a panic in a task.
            let _ = tx.send(effect);
        });
        (sink, Mutex::new(Some(rx)))
    }

    /// The plugins compiled into the binary, plus the built-in front-end
    /// commands (`/new`, `/rename`, `/sessions`, `/model`, `/provider`,
    /// `/effort`, `/thinking`, `/profile`) that the front-end handles itself.
    pub fn builtin() -> Result<Self, PluginHostError> {
        Self::builtin_with_tools(None)
    }

    /// The builtin host with a profile's tool filter applied: `Some(ids)`
    /// declares and executes only the named plugins' tools (unknown ids are
    /// a startup error), `None` keeps every plugin's tools. Everything else
    /// a plugin contributes is untouched — a profile composes the session,
    /// it does not uninstall plugins.
    ///
    /// `local` is empty in the shipped binary; a wrapper built by
    /// `codest plugin install` passes the plugins it linked here.
    pub fn builtin_with_tools(enabled: Option<&[String]>) -> Result<Self, PluginHostError> {
        Self::with_local(&[], enabled)
    }

    /// Build a host from the built-in plugins plus a caller-supplied local
    /// slice, with the profile's tool filter applied.
    ///
    /// This is the seam an externally-built binary injects through: the
    /// shipped set stays a compile-time constant while the *linked* set is
    /// whatever the wrapper crate declares. Local ids, command names, and
    /// tool names are validated against the built-in ones, so a plugin can
    /// neither shadow nor be silently shadowed by a shipped one.
    pub fn with_local(
        local: &'static [&'static dyn Plugin],
        enabled: Option<&[String]>,
    ) -> Result<Self, PluginHostError> {
        Self::with_extra_commands(
            &[BUILTIN_PLUGINS, local],
            SHIPPED_COMMAND_GROUPS,
            enabled.map(<[String]>::to_vec),
        )
    }

    /// The shipped plugins only — a stable table for tests and diagnostics,
    /// unaffected by whatever a user added to `plugins/local.rs`.
    pub fn shipped() -> Result<Self, PluginHostError> {
        Self::with_extra_commands(&[BUILTIN_PLUGINS], SHIPPED_COMMAND_GROUPS, None)
    }

    pub fn plugins(&self) -> &[&'static dyn Plugin] {
        &self.plugins
    }

    pub fn plugin_ids(&self) -> &[&'static str] {
        &self.ids
    }

    pub fn table(&self) -> &CommandTable {
        &self.table
    }

    /// Every protocol factory contributed by the compiled-in plugins, in
    /// registration order. The provider registry merges these with its
    /// built-ins; duplicate ids fail the registry build.
    pub fn protocol_factories(&self) -> Vec<&'static dyn ProtocolFactory> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.protocols().iter().copied())
            .collect()
    }

    /// Every sign-in entry the plugins contribute, paired with the plugin that
    /// owns it — the `/provider` menu's "sign in to a plugin provider" list.
    pub fn auth_providers(&self) -> Vec<(&'static dyn Plugin, AuthProvider)> {
        let mut entries = Vec::new();
        for plugin in &self.plugins {
            for auth in plugin.auth_providers() {
                entries.push((*plugin, *auth));
            }
        }
        entries
    }

    /// The plugin that contributes `protocol`, if any. `/provider` uses this
    /// to decide whether an instance's protocol has a sign-in entry behind it.
    pub fn auth_plugin_for_protocol(&self, protocol: &str) -> Option<&'static dyn Plugin> {
        self.plugins.iter().copied().find(|plugin| {
            plugin
                .auth_providers()
                .iter()
                .any(|auth| auth.protocol == protocol)
        })
    }

    /// The sign-in state of `instance`, as reported by the plugin that serves
    /// its protocol. `Unsupported` when nothing serves it.
    pub fn auth_status(&self, instance: &str, protocol: &str) -> AuthStatus {
        match self.auth_plugin_for_protocol(protocol) {
            Some(plugin) => plugin.auth_status(instance),
            None => AuthStatus::Unsupported,
        }
    }

    /// Build the effective system prompt: the base prompt followed by every
    /// non-empty plugin fragment, separated by a blank line.
    pub fn system_prompt(&self, base: &str) -> String {
        let mut prompt = base.trim_end().to_string();
        for fragment in self
            .plugins
            .iter()
            .filter_map(|plugin| plugin.system_prompt())
            .filter(|fragment| !fragment.trim().is_empty())
        {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(fragment.trim());
        }
        prompt
    }

    /// The system prompt as labeled sections — the base prompt first, then
    /// every non-empty plugin fragment attributed to its plugin. The same
    /// assembly rules as [`PluginHost::system_prompt`], exposed for
    /// `/context`-style displays.
    pub fn system_prompt_sections(&self, base: &str) -> Vec<(String, String)> {
        let mut sections = Vec::new();
        if !base.trim_end().is_empty() {
            sections.push((String::from("base"), base.trim_end().to_string()));
        }
        for plugin in &self.plugins {
            if let Some(fragment) = plugin
                .system_prompt()
                .filter(|fragment| !fragment.trim().is_empty())
            {
                sections.push((
                    format!("plugin:{}", plugin.id()),
                    fragment.trim().to_string(),
                ));
            }
        }
        sections
    }

    /// Every tool the enabled plugins declared, in registration order — the
    /// tool filter ([`PluginHost::builtin_with_tools`]) applies here, so the
    /// model only ever sees the tools the profile kept.
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.plugins
            .iter()
            .filter(|plugin| self.tool_plugin_enabled(plugin.id()))
            .flat_map(|plugin| plugin.tools().iter().copied())
            .collect()
    }

    /// Whether `id`'s tools pass the host's tool filter.
    fn tool_plugin_enabled(&self, id: &str) -> bool {
        match &self.tool_filter {
            None => true,
            Some(enabled) => enabled.iter().any(|name| name == id),
        }
    }

    fn tool_owner(&self, name: &str) -> Option<&'static dyn Plugin> {
        self.tools
            .iter()
            .find(|(tool, _)| *tool == name)
            .map(|(_, plugin)| *plugin)
            .filter(|plugin| self.tool_plugin_enabled(plugin.id()))
    }

    pub fn parse(&self, input: &str) -> CommandInput {
        self.table.parse(input)
    }

    pub fn suggestions(&self, query: &str) -> Vec<CommandCandidate> {
        self.table.suggestions(query)
    }

    pub fn help_text(&self) -> String {
        self.table.help_text()
    }

    /// Run the owning plugin for an invocation and collect its output.
    pub fn dispatch_command(&self, invocation: &Invocation, info: PluginInfo<'_>) -> PluginOutput {
        match self.owner(invocation.spec.name) {
            Some(plugin) => {
                let mut ctx = self.context(plugin.id(), info);
                if let Err(error) = plugin.run_command(invocation, &mut ctx) {
                    ctx.error(error.to_string());
                }
                PluginOutput::from_ctx(ctx)
            }
            None => {
                let mut ctx = self.context("__host__", info);
                ctx.error(format!(
                    "no plugin owns `/{}`; the command table is inconsistent",
                    invocation.spec.name
                ));
                PluginOutput::from_ctx(ctx)
            }
        }
    }

    pub fn startup(&self, info: PluginInfo<'_>) -> PluginOutput {
        self.run_hooks(info, |plugin, ctx| plugin.on_startup(ctx))
    }

    pub fn user_message(&self, text: &str, info: PluginInfo<'_>) -> PluginOutput {
        self.run_hooks(info, |plugin, ctx| plugin.on_user_message(text, ctx))
    }

    pub fn turn_complete(&self, message: &str, info: PluginInfo<'_>) -> PluginOutput {
        self.run_hooks(info, |plugin, ctx| plugin.on_turn_complete(message, ctx))
    }

    pub fn conversation_cleared(&self, info: PluginInfo<'_>) -> PluginOutput {
        self.run_hooks(info, |plugin, ctx| plugin.on_conversation_cleared(ctx))
    }

    /// The settings sections the loaded plugins declare, in registration
    /// order. `/settings` renders one section per entry; a plugin that
    /// returned `None` (the default) contributes nothing.
    pub fn settings_sections(
        &self,
    ) -> Vec<(&'static str, cst_plugin_api::settings::SettingsSection)> {
        self.plugins
            .iter()
            .filter_map(|plugin| {
                let section = plugin.settings()?;
                (!section.fields.is_empty()).then_some((plugin.id(), section))
            })
            .collect()
    }

    /// Tell one plugin that its settings pocket changed. The front-end calls
    /// this after a successful write; `settings` is the pocket as the file now
    /// reads (`None` once the last key was removed).
    pub fn settings_changed(
        &self,
        plugin_id: &str,
        settings: Option<&PluginSettings>,
        info: PluginInfo<'_>,
    ) -> PluginOutput {
        match self.owner_of_id(plugin_id) {
            Some(plugin) => {
                let mut ctx = self.context(plugin.id(), info);
                plugin.on_settings_changed(settings, &mut ctx);
                PluginOutput::from_ctx(ctx)
            }
            None => PluginOutput::empty(),
        }
    }

    fn owner_of_id(&self, plugin_id: &str) -> Option<&'static dyn Plugin> {
        self.plugins
            .iter()
            .copied()
            .find(|plugin| plugin.id() == plugin_id)
    }

    /// Fan one core event out to the plugins that subscribed to its category,
    /// in registration order.
    ///
    /// The payload borrows from `event`, so this costs nothing per event; the
    /// host is expected to filter through [`Plugin::subscriptions`] first,
    /// which happens here. A plugin that returned no subscription never sees
    /// the dispatch at all.
    pub fn dispatch_event(&self, event: &crate::core::Event, info: PluginInfo<'_>) -> PluginOutput {
        let plugin_event = PluginEvent {
            at: event.at,
            kind: plugin_event_kind(&event.msg),
        };
        let mut output = PluginOutput::empty();
        for plugin in &self.plugins {
            if !plugin
                .subscriptions()
                .contains(plugin_event.kind.subscription())
            {
                continue;
            }
            let mut ctx = self.context(plugin.id(), info);
            plugin.on_event(&plugin_event, &mut ctx);
            let (mut effects, tasks) = ctx.into_parts();
            output.effects.append(&mut effects);
            output.tasks.extend(tasks);
        }
        output
    }

    /// Build one plugin's context: its id scopes the status helpers, its
    /// settings resolve from `info.plugin_configs`, and its bridge clones the
    /// host's effect sink and cancellation token.
    fn context<'a>(&'a self, plugin_id: &'static str, info: PluginInfo<'a>) -> PluginContext<'a> {
        let bridge = PluginBridge::new(
            plugin_id,
            self.cancel.clone(),
            Arc::clone(&self.effects_sink),
        );
        PluginContext::new(&self.table, info, plugin_id, bridge)
    }

    /// Claim the receiver background tasks send effects through. The
    /// front-end calls this once, at startup; later calls return `None`.
    pub fn take_effects_receiver(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<PluginEffect>> {
        self.effects_rx
            .lock()
            .expect("plugin effects receiver lock")
            .take()
    }

    /// Install the front-end's prompt service and hand it to every plugin.
    ///
    /// Called once at startup, before the first turn: a plugin's tool can then
    /// put a structured question to the user and block on the answer. A
    /// front-end without one (a headless runner, a test) simply never calls
    /// this, and a plugin that asks gets `None` back — the guidance path, not
    /// a failure.
    pub fn install_prompt_host(&self, host: Arc<dyn PromptHost>) {
        *self.prompt_host.lock().expect("plugin prompt host lock") = Some(host.clone());
        for plugin in &self.plugins {
            plugin.set_prompt_host(host.clone());
        }
    }

    /// The token that cancels every spawned plugin task. The front-end
    /// cancels it on exit, after which an `abort` sweep cleans up whatever
    /// did not watch the token.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn run_hooks(
        &self,
        info: PluginInfo<'_>,
        hook: impl Fn(&dyn Plugin, &mut PluginContext<'_>),
    ) -> PluginOutput {
        let mut output = PluginOutput::empty();
        for plugin in &self.plugins {
            let mut ctx = self.context(plugin.id(), info);
            hook(*plugin, &mut ctx);
            let (mut effects, tasks) = ctx.into_parts();
            output.effects.append(&mut effects);
            output.tasks.extend(tasks);
        }
        output
    }

    fn owner(&self, name: &str) -> Option<&'static dyn Plugin> {
        self.plugins
            .iter()
            .copied()
            .find(|plugin| plugin.commands().iter().any(|spec| spec.name == name))
    }
}

/// The host is what the agent worker talks to when a model asks for a tool:
/// dispatch to the plugin that declared the tool, errors included.
impl ToolExecutor for PluginHost {
    fn execute(&self, call: &ToolCall) -> Result<String, String> {
        let plugin = self
            .tool_owner(&call.name)
            .ok_or_else(|| format!("unknown tool `{}`", call.name))?;
        plugin.run_tool(call).map_err(|error| error.to_string())
    }
}

/// Mirror one host-internal bus message into the contract's event vocabulary,
/// borrowing the payload (`String` fields become `&str`). The mirror is
/// total: every [`EventMsg`] variant has exactly one kind, so a monitor
/// never loses a step of the stream.
fn plugin_event_kind(msg: &crate::core::EventMsg) -> PluginEventKind<'_> {
    use crate::core::EventMsg;
    match msg {
        EventMsg::TurnStarted => PluginEventKind::TurnStarted,
        EventMsg::AgentMessageDelta { delta } => PluginEventKind::MessageDelta {
            kind: DeltaKind::Text,
            text: delta,
        },
        EventMsg::AgentThinkingDelta { delta } => PluginEventKind::MessageDelta {
            kind: DeltaKind::Thinking,
            text: delta,
        },
        EventMsg::AgentMessage { message } => PluginEventKind::AgentMessage { text: message },
        EventMsg::AgentToolCallStarted { name, arguments } => {
            PluginEventKind::ToolCallStarted { name, arguments }
        }
        EventMsg::AgentToolCall {
            name,
            result,
            error,
            duration_ms,
            ..
        } => PluginEventKind::ToolCallFinished {
            name,
            result,
            error: error.as_deref(),
            duration_ms: *duration_ms,
        },
        EventMsg::ContextUsage { input, output, .. } => PluginEventKind::RoundUsage {
            input: *input,
            output: *output,
        },
        // The plugin-facing kinds stay (input, output): cache attribution and
        // model records are front-end concerns (`/usage`, the rollout), not
        // something the shipped plugins consume.
        EventMsg::TurnUsage { input, output, .. } => PluginEventKind::TurnUsage {
            input: *input,
            output: *output,
        },
        EventMsg::TurnAborted { reason } => PluginEventKind::TurnAborted { reason },
        EventMsg::Error { message } => PluginEventKind::Error { message },
        EventMsg::SteerClaimed { text } => PluginEventKind::SteerClaimed { text },
        EventMsg::SteerUnclaimed { texts } => PluginEventKind::SteerUnclaimed { texts },
        EventMsg::TurnComplete => PluginEventKind::TurnComplete,
        EventMsg::ConversationCleared => PluginEventKind::ConversationCleared,
        EventMsg::ConversationSnapshot { messages } => {
            PluginEventKind::ConversationSnapshot { messages }
        }
        EventMsg::SessionStarted { session } => {
            PluginEventKind::SessionStarted { session: *session }
        }
        EventMsg::SessionSwitched { session } => {
            PluginEventKind::SessionSwitched { session: *session }
        }
        EventMsg::SessionRenamed { session, name } => PluginEventKind::SessionRenamed {
            session: *session,
            name,
        },
        EventMsg::ModelSwitched { model } => PluginEventKind::ModelSwitched { model },
        EventMsg::ReasoningEffortSwitched { effort } => PluginEventKind::EffortSwitched {
            effort: effort.as_deref(),
        },
        EventMsg::ProfileSwitched { name } => PluginEventKind::ProfileSwitched { name },
        EventMsg::ToolsSwitched { enabled } => PluginEventKind::ToolsSwitched { enabled },
        EventMsg::ShutdownComplete => PluginEventKind::Shutdown,
    }
}

impl fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginHost")
            .field("plugins", &self.ids)
            .field("commands", &self.table.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Event;
    use crate::core::EventMsg;
    use crate::core::ModelClient;
    use crate::core::config::ProviderInstance;
    use crate::core::echo_model::EchoModelClient;
    use crate::core::model::ModelError;

    static ECHO_COMMAND: CommandSpec = CommandSpec::new(
        "echo",
        &[],
        "/echo <text>",
        "echo text",
        ArgsPolicy::Required,
    );

    static PING_TOOL: ToolSpec = ToolSpec {
        name: "ping",
        description: "reply with pong",
        parameters_json: r#"{ "type": "object" }"#,
        read_only: true,
    };

    struct Echo;

    impl Plugin for Echo {
        fn id(&self) -> &'static str {
            "echo"
        }

        fn commands(&self) -> &'static [CommandSpec] {
            std::slice::from_ref(&ECHO_COMMAND)
        }

        fn run_command(
            &self,
            invocation: &Invocation,
            ctx: &mut PluginContext<'_>,
        ) -> Result<(), PluginError> {
            ctx.notice(format!("echo: {}", invocation.args));
            Ok(())
        }
    }

    struct Audit;

    impl Plugin for Audit {
        fn id(&self) -> &'static str {
            "audit"
        }

        fn on_startup(&self, ctx: &mut PluginContext<'_>) {
            ctx.notice("audit:startup");
        }

        fn on_user_message(&self, text: &str, ctx: &mut PluginContext<'_>) {
            ctx.notice(format!("audit:user:{text}"));
        }

        fn on_turn_complete(&self, message: &str, ctx: &mut PluginContext<'_>) {
            ctx.notice(format!("audit:complete:{message}"));
        }

        fn on_conversation_cleared(&self, ctx: &mut PluginContext<'_>) {
            ctx.notice("audit:cleared");
        }
    }

    struct Persona;

    impl Plugin for Persona {
        fn id(&self) -> &'static str {
            "persona"
        }

        fn system_prompt(&self) -> Option<&'static str> {
            Some("Be terse.")
        }
    }

    struct Failing;

    static FAIL_COMMAND: CommandSpec =
        CommandSpec::new("fail", &[], "/fail", "always fails", ArgsPolicy::None);

    impl Plugin for Failing {
        fn id(&self) -> &'static str {
            "failing"
        }

        fn commands(&self) -> &'static [CommandSpec] {
            std::slice::from_ref(&FAIL_COMMAND)
        }

        fn run_command(
            &self,
            _invocation: &Invocation,
            _ctx: &mut PluginContext<'_>,
        ) -> Result<(), PluginError> {
            Err(PluginError::new("boom"))
        }
    }

    static ECHO: Echo = Echo;
    static AUDIT: Audit = Audit;
    static PERSONA: Persona = Persona;
    static FAILING: Failing = Failing;

    static TEST_PLUGINS: &[&dyn Plugin] = &[&ECHO, &AUDIT];
    static PERSONA_PLUGINS: &[&dyn Plugin] = &[&PERSONA];
    static FAILING_PLUGINS: &[&dyn Plugin] = &[&FAILING];

    fn host() -> PluginHost {
        PluginHost::new(TEST_PLUGINS).expect("valid plugins")
    }

    fn info(host: &PluginHost) -> PluginInfo<'_> {
        static NO_TOOLS: [ToolSpec; 0] = [];
        static NO_CONFIGS: [(String, PluginSettings); 0] = [];
        PluginInfo {
            model: "test-model",
            mode: "fullscreen",
            transcript_len: 2,
            context_window: Some(200_000),
            last_usage: Some((8_100, 1_200)),
            last_cached: Some(6_400),
            total_input: 16_200,
            total_output: 2_400,
            total_cached: 12_800,
            plugin_ids: host.plugin_ids(),
            system_sections: &[],
            effort: None,
            request: RequestFacts {
                protocol: "",
                base_url: "",
                endpoint: "",
                overrides: &[],
            },
            tools: &NO_TOOLS,
            enabled_tools: &[],
            message_count: 1,
            plugin_configs: &NO_CONFIGS,
        }
    }

    fn invocation(host: &PluginHost, input: &str) -> Invocation {
        match host.parse(input) {
            CommandInput::Command(invocation) => invocation,
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_runs_the_owning_plugin() {
        let host = host();
        let output = host.dispatch_command(&invocation(&host, "/echo hi"), info(&host));
        assert_eq!(
            output.effects,
            vec![PluginEffect::Notice("echo: hi".to_string())]
        );
    }

    #[test]
    fn plugin_errors_become_error_effects() {
        let host = PluginHost::new(FAILING_PLUGINS).expect("valid plugins");
        let output = host.dispatch_command(&invocation(&host, "/fail"), info(&host));
        assert_eq!(
            output.effects,
            vec![PluginEffect::Error("boom".to_string())]
        );
    }

    #[test]
    fn hooks_run_in_registration_order() {
        let host = host();
        assert_eq!(
            host.startup(info(&host)).effects,
            vec![PluginEffect::Notice("audit:startup".to_string())]
        );
        assert_eq!(
            host.user_message("hello", info(&host)).effects,
            vec![PluginEffect::Notice("audit:user:hello".to_string())]
        );
        assert_eq!(
            host.turn_complete("done", info(&host)).effects,
            vec![PluginEffect::Notice("audit:complete:done".to_string())]
        );
        assert_eq!(
            host.conversation_cleared(info(&host)).effects,
            vec![PluginEffect::Notice("audit:cleared".to_string())]
        );
    }

    /// Subscribes to deltas and session facts; publishes a scoped status
    /// segment and one background task on demand.
    struct Meter;

    impl Plugin for Meter {
        fn id(&self) -> &'static str {
            "meter"
        }

        fn subscriptions(&self) -> EventSubscription {
            EventSubscription::DELTAS.union(EventSubscription::SESSION)
        }

        fn on_event(&self, event: &PluginEvent<'_>, ctx: &mut PluginContext<'_>) {
            match event.kind {
                PluginEventKind::MessageDelta { text, .. } => {
                    ctx.notice(format!("meter:delta:{}", text.len()));
                    if text.contains("status") {
                        ctx.set_status(StatusSegment::new("rate", "42 t/s"));
                        // A background task keeps only its bridge: sends land
                        // on the host's receiver after the dispatch returns.
                        let bridge = ctx.bridge();
                        ctx.spawn(async move {
                            bridge.notice("meter:from-task");
                        });
                    }
                }
                PluginEventKind::ModelSwitched { model } => {
                    ctx.notice(format!("meter:model:{model}"));
                }
                _ => {}
            }
        }
    }

    static METER: Meter = Meter;
    static METER_PLUGINS: &[&dyn Plugin] = &[&METER];

    #[test]
    fn dispatch_event_delivers_subscribed_kinds_only() {
        let host = PluginHost::new(METER_PLUGINS).expect("valid plugins");
        // Usage is not in the meter's subscription: it never sees the event.
        let usage = Event::new(
            "t",
            EventMsg::TurnUsage {
                input: 10,
                output: 2,
                cached: None,
                model: None,
            },
        );
        assert!(host.dispatch_event(&usage, info(&host)).effects.is_empty());

        // Deltas are.
        let delta = Event::new("t", EventMsg::AgentMessageDelta { delta: "hi".into() });
        assert_eq!(
            host.dispatch_event(&delta, info(&host)).effects,
            vec![PluginEffect::Notice("meter:delta:2".to_string())]
        );
    }

    #[test]
    fn dispatch_event_scopes_status_keys_and_collects_tasks() {
        let host = PluginHost::new(METER_PLUGINS).expect("valid plugins");
        let delta = Event::new(
            "t",
            EventMsg::AgentMessageDelta {
                delta: "status".into(),
            },
        );
        let output = host.dispatch_event(&delta, info(&host));
        assert_eq!(
            output.effects,
            vec![
                PluginEffect::Notice("meter:delta:6".to_string()),
                PluginEffect::SetStatus(StatusSegment {
                    key: "meter:rate".to_string(),
                    text: "42 t/s".to_string(),
                    tone: StatusTone::Dim,
                }),
            ]
        );
        assert_eq!(output.tasks.len(), 1, "the spawned task rides the output");
        // A second dispatch of the same event spawns nothing extra by
        // itself — tasks come only from what the handler asked for.
        assert_eq!(
            host.dispatch_event(&delta, info(&host)).tasks.len(),
            1,
            "dispatch is stateless per call"
        );
    }

    #[tokio::test]
    async fn a_spawned_tasks_effects_reach_the_host_receiver() {
        let host = Arc::new(PluginHost::new(METER_PLUGINS).expect("valid plugins"));
        // Claim the receiver before dispatching, then run what the dispatch
        // spawned: its bridge send lands on this end of the channel.
        let mut rx = host.take_effects_receiver().expect("receiver");
        let delta = Event::new(
            "t",
            EventMsg::AgentMessageDelta {
                delta: "status".into(),
            },
        );
        let output = host.dispatch_event(&delta, info(&host));
        for task in output.tasks {
            task.await;
        }
        assert_eq!(
            rx.recv().await,
            Some(PluginEffect::Notice("meter:from-task".to_string()))
        );
    }

    #[test]
    fn events_are_stamped_at_construction() {
        let first = Event::new("t", EventMsg::TurnStarted);
        let second = Event::new("t", EventMsg::TurnComplete);
        assert!(first.at <= second.at, "emit stamps advance monotonically");
    }

    #[test]
    fn plugin_settings_resolve_per_plugin() {
        struct Reader;

        impl Plugin for Reader {
            fn id(&self) -> &'static str {
                "reader"
            }

            fn on_startup(&self, ctx: &mut PluginContext<'_>) {
                let enabled = ctx
                    .plugin_settings()
                    .and_then(|settings| settings.option_bool("enabled"))
                    .unwrap_or(false);
                ctx.notice(format!("reader:enabled:{enabled}"));
            }
        }

        static READER: Reader = Reader;
        static READER_PLUGINS: &[&dyn Plugin] = &[&READER];
        let host = PluginHost::new(READER_PLUGINS).expect("valid plugins");

        let configs = vec![("reader".to_string(), {
            let mut options = serde_json::Map::new();
            options.insert("enabled".to_string(), serde_json::json!(true));
            PluginSettings { options }
        })];
        let mut configured = info(&host);
        configured.plugin_configs = &configs;
        assert_eq!(
            host.startup(configured).effects,
            vec![PluginEffect::Notice("reader:enabled:true".to_string())]
        );

        // Without a pocket the same hook sees the default.
        assert_eq!(
            host.startup(info(&host)).effects,
            vec![PluginEffect::Notice("reader:enabled:false".to_string())]
        );
    }

    #[test]
    fn settings_sections_come_from_the_plugins_that_declare_one() {
        use cst_plugin_api::settings::SettingField;
        use cst_plugin_api::settings::SettingsSection;

        struct Tunable;

        static FIELDS: [SettingField; 1] = [SettingField::toggle("enabled", "Meter", "…", false)];
        static SECTION: SettingsSection = SettingsSection::new("tunable", "…", &FIELDS);

        impl Plugin for Tunable {
            fn id(&self) -> &'static str {
                "tunable"
            }

            fn settings(&self) -> Option<SettingsSection> {
                Some(SECTION)
            }

            fn on_settings_changed(
                &self,
                settings: Option<&PluginSettings>,
                ctx: &mut PluginContext<'_>,
            ) {
                let enabled = settings.and_then(|settings| settings.option_bool("enabled"));
                ctx.notice(format!("tunable:{enabled:?}"));
            }
        }

        static TUNABLE: Tunable = Tunable;
        static TUNABLE_PLUGINS: &[&dyn Plugin] = &[&TUNABLE];
        let host = PluginHost::new(TUNABLE_PLUGINS).expect("valid plugins");

        // Only the plugins that declared a section appear, in order.
        let sections = host.settings_sections();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "tunable");
        assert_eq!(sections[0].1.fields[0].key, "enabled");

        // The plugin is told the pocket as written, `None` once it is empty —
        // and an id nothing owns is a silent no-op, not a panic.
        let mut options = serde_json::Map::new();
        options.insert("enabled".to_string(), serde_json::json!(true));
        let pocket = PluginSettings { options };
        assert_eq!(
            host.settings_changed("tunable", Some(&pocket), info(&host))
                .effects,
            vec![PluginEffect::Notice("tunable:Some(true)".to_string())]
        );
        assert_eq!(
            host.settings_changed("tunable", None, info(&host)).effects,
            vec![PluginEffect::Notice("tunable:None".to_string())]
        );
        assert!(
            host.settings_changed("absent", None, info(&host))
                .effects
                .is_empty()
        );
    }

    #[test]
    fn system_prompt_appends_fragments() {
        let host = PluginHost::new(PERSONA_PLUGINS).expect("valid plugins");
        assert_eq!(host.system_prompt("base"), "base\n\nBe terse.");
        assert_eq!(host.system_prompt("base\n"), "base\n\nBe terse.");
        assert_eq!(host.system_prompt(""), "Be terse.");
    }

    #[test]
    fn duplicate_plugin_ids_are_rejected() {
        struct TwinA;
        struct TwinB;

        impl Plugin for TwinA {
            fn id(&self) -> &'static str {
                "twin"
            }
        }
        impl Plugin for TwinB {
            fn id(&self) -> &'static str {
                "twin"
            }
        }

        static TWIN_A: TwinA = TwinA;
        static TWIN_B: TwinB = TwinB;
        static TWINS: &[&dyn Plugin] = &[&TWIN_A, &TWIN_B];

        assert!(matches!(
            PluginHost::new(TWINS),
            Err(PluginHostError::DuplicatePluginId { id: "twin" })
        ));
    }

    /// `/settings` always shows the host's own card under `HOST_NAMESPACE`,
    /// so a plugin claiming that id would put two cards with one title on
    /// screen and make `/settings codest` ambiguous.
    #[test]
    fn the_host_namespace_is_not_a_plugin_id() {
        struct Impostor;

        impl Plugin for Impostor {
            fn id(&self) -> &'static str {
                cst_plugin_api::settings::HOST_NAMESPACE
            }
        }

        static IMPOSTOR: Impostor = Impostor;
        static IMPOSTORS: &[&dyn Plugin] = &[&IMPOSTOR];

        assert!(matches!(
            PluginHost::new(IMPOSTORS),
            Err(PluginHostError::ReservedPluginId { id: "codest" })
        ));
    }

    #[test]
    fn duplicate_commands_are_rejected() {
        struct EchoAgain;

        impl Plugin for EchoAgain {
            fn id(&self) -> &'static str {
                "echo-again"
            }

            fn commands(&self) -> &'static [CommandSpec] {
                std::slice::from_ref(&ECHO_COMMAND)
            }
        }

        static ECHO_AGAIN: EchoAgain = EchoAgain;
        static DUPLICATES: &[&dyn Plugin] = &[&ECHO, &ECHO_AGAIN];

        assert!(matches!(
            PluginHost::new(DUPLICATES),
            Err(PluginHostError::CommandTable(
                CommandTableError::DuplicateName { name: "echo" }
            ))
        ));
    }

    #[test]
    fn duplicate_tools_are_rejected() {
        struct ToolerA;
        struct ToolerB;

        impl Plugin for ToolerA {
            fn id(&self) -> &'static str {
                "tooler-a"
            }

            fn tools(&self) -> &'static [ToolSpec] {
                std::slice::from_ref(&PING_TOOL)
            }
        }
        impl Plugin for ToolerB {
            fn id(&self) -> &'static str {
                "tooler-b"
            }

            fn tools(&self) -> &'static [ToolSpec] {
                std::slice::from_ref(&PING_TOOL)
            }
        }

        static TOOLER_A: ToolerA = ToolerA;
        static TOOLER_B: ToolerB = ToolerB;
        static TOOLERS: &[&dyn Plugin] = &[&TOOLER_A, &TOOLER_B];

        assert!(matches!(
            PluginHost::new(TOOLERS),
            Err(PluginHostError::DuplicateTool { name: "ping" })
        ));
    }

    struct Tooler;

    impl Plugin for Tooler {
        fn id(&self) -> &'static str {
            "tooler"
        }

        fn tools(&self) -> &'static [ToolSpec] {
            std::slice::from_ref(&PING_TOOL)
        }

        fn run_tool(&self, call: &ToolCall) -> Result<String, PluginError> {
            Ok(format!("pong {}", call.arguments))
        }
    }

    static TOOLER_PLUGIN: Tooler = Tooler;
    static TOOLER_PLUGINS: &[&dyn Plugin] = &[&TOOLER_PLUGIN];

    #[test]
    fn executor_dispatches_to_the_declaring_plugin() {
        let host = PluginHost::new(TOOLER_PLUGINS).expect("valid plugins");
        assert_eq!(host.tools(), vec![PING_TOOL]);

        let call = ToolCall {
            id: "call_1".to_string(),
            name: "ping".to_string(),
            arguments: "{}".to_string(),
        };
        assert_eq!(host.execute(&call), Ok("pong {}".to_string()));

        let unknown = ToolCall {
            name: "nope".to_string(),
            ..call
        };
        assert_eq!(
            host.execute(&unknown),
            Err("unknown tool `nope`".to_string())
        );
    }

    #[test]
    fn tool_filter_keeps_only_the_named_plugins_tools() {
        // TOOLER declares the `ping` tool; ECHO declares the `/echo` command.
        // Enabling only echo's tools strips the tool but keeps its command.
        static FILTERED: &[&dyn Plugin] = &[&ECHO, &TOOLER_PLUGIN];
        let host =
            PluginHost::with_extra_commands(&[FILTERED], &[], Some(vec!["echo".to_string()]))
                .expect("valid host");
        assert!(host.tools().is_empty(), "tooler's tool is filtered out");
        assert!(
            matches!(host.parse("/echo hi"), CommandInput::Command(_)),
            "commands are never filtered"
        );

        // The filtered tool is not executable either — the model was never
        // told about it, so a call is a hallucination.
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "ping".to_string(),
            arguments: "{}".to_string(),
        };
        assert_eq!(host.execute(&call), Err("unknown tool `ping`".to_string()));

        // Enabling tooler keeps the tool, and `None` keeps everything.
        let host =
            PluginHost::with_extra_commands(&[FILTERED], &[], Some(vec!["tooler".to_string()]))
                .expect("valid host");
        assert_eq!(host.tools(), vec![PING_TOOL]);
    }

    #[test]
    fn an_unknown_tool_filter_id_is_a_startup_error() {
        let error = PluginHost::builtin_with_tools(Some(&["nope".to_string()])).unwrap_err();
        assert!(matches!(
            error,
            PluginHostError::UnknownFilteredPlugin { .. }
        ));
        assert!(error.to_string().contains("nope"), "{error}");
        assert!(
            error.to_string().contains("help"),
            "the known ids are listed: {error}"
        );
    }

    #[test]
    fn injected_plugins_join_the_host_beside_the_builtin_ones() {
        // What a wrapper generated by `codest plugin install` does: it hands
        // its own `static` slice to the host, so the plugin's command becomes
        // dispatchable without any shipped file changing.
        let host = PluginHost::with_local(FAILING_PLUGINS, None).expect("valid host");
        assert!(
            host.plugin_ids().contains(&"failing"),
            "{:?}",
            host.plugin_ids()
        );
        assert!(
            host.plugin_ids().contains(&"help"),
            "the built-ins are still there: {:?}",
            host.plugin_ids()
        );
        assert!(
            matches!(host.parse("/fail"), CommandInput::Command(_)),
            "the injected plugin's command is in the table"
        );
    }

    #[test]
    fn an_injected_plugin_cannot_shadow_a_builtin_id() {
        struct Impostor;

        impl Plugin for Impostor {
            fn id(&self) -> &'static str {
                "help"
            }
        }

        static IMPOSTOR: Impostor = Impostor;
        static IMPOSTOR_PLUGINS: &[&dyn Plugin] = &[&IMPOSTOR];
        let error = PluginHost::with_local(IMPOSTOR_PLUGINS, None).unwrap_err();
        assert!(matches!(
            error,
            PluginHostError::DuplicatePluginId { id: "help" }
        ));
    }

    #[test]
    fn the_tool_filter_also_applies_to_injected_plugins() {
        static TOOLERS: &[&dyn Plugin] = &[&TOOLER_PLUGIN];
        let host =
            PluginHost::with_local(TOOLERS, Some(&["tooler".to_string()])).expect("valid host");
        assert_eq!(host.tools(), vec![PING_TOOL]);

        let off = PluginHost::with_local(TOOLERS, Some(&["help".to_string()])).expect("valid host");
        assert!(off.tools().is_empty(), "the local tool is filtered out");
    }

    #[test]
    fn system_prompt_sections_label_base_and_plugins() {
        let host = PluginHost::new(PERSONA_PLUGINS).expect("valid plugins");
        assert_eq!(
            host.system_prompt_sections("base"),
            vec![
                ("base".to_string(), "base".to_string()),
                ("plugin:persona".to_string(), "Be terse.".to_string()),
            ]
        );
        // An empty base prompt contributes no section at all.
        assert_eq!(
            host.system_prompt_sections(""),
            vec![("plugin:persona".to_string(), "Be terse.".to_string())]
        );
    }

    #[test]
    fn builtin_host_exposes_the_core_commands() {
        // `shipped()` rather than `builtin()`: whatever a user linked through
        // `codest plugin install` must not break the shipped-table assertion.
        let host = PluginHost::shipped().expect("shipped plugins");
        let names = host
            .table()
            .specs()
            .iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "help", "clear", "quit", "status", "continue", "tps", "new", "rename", "sessions",
                "model", "provider", "effort", "thinking", "profile", "tools", "context",
                "settings", "usage"
            ]
        );
    }

    /// A plugin contributing a protocol kind to the provider registry —
    /// the "custom provider" extension path.
    struct Vendor;

    impl Plugin for Vendor {
        fn id(&self) -> &'static str {
            "vendor"
        }

        fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
            static FACTORIES: &[&'static dyn ProtocolFactory] = &[&VENDOR_FACTORY];
            FACTORIES
        }
    }

    /// A minimal custom protocol: one that builds a client from an instance
    /// without touching the network.
    struct VendorFactory;

    impl ProtocolFactory for VendorFactory {
        fn id(&self) -> &'static str {
            "vendor-protocol"
        }

        fn display_name(&self) -> &'static str {
            "Vendor (example)"
        }

        fn create(
            &self,
            instance: &ProviderInstance,
            _model: &str,
        ) -> Result<std::sync::Arc<dyn ModelClient>, ModelError> {
            // Resolving the key exercises the config seam even though the
            // echo model ignores it (an unset template yields None here).
            let _api_key = instance.resolved_api_key();
            Ok(std::sync::Arc::new(EchoModelClient::new(
                std::time::Duration::ZERO,
            )))
        }
    }

    static VENDOR_FACTORY: VendorFactory = VendorFactory;

    #[test]
    fn plugin_protocols_feed_the_provider_registry() {
        static VENDOR_PLUGINS: &[&dyn Plugin] = &[&Vendor];

        let host = PluginHost::new(VENDOR_PLUGINS).expect("valid plugins");
        let factories = host.protocol_factories();
        assert_eq!(factories.len(), 1);
        assert_eq!(factories[0].id(), "vendor-protocol");

        let instance = ProviderInstance {
            name: "vendored".to_string(),
            protocol: Some("vendor-protocol".to_string()),
            base_url: None,
            api_key: Some("$CODEST_NO_SUCH_VAR".to_string()),
            models: vec![],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let registry = crate::core::providers::ProviderRegistry::build(&factories, vec![instance])
            .expect("the plugin protocol is known");
        assert_eq!(
            registry.protocol_display_name("vendor-protocol"),
            "Vendor (example)"
        );

        // Registering the same protocol id twice in one build is an error.
        let error =
            crate::core::providers::ProviderRegistry::build(&[factories[0], factories[0]], vec![])
                .unwrap_err();
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn help_lists_plugin_commands() {
        // `shipped()` rather than `builtin()`: a user's `plugins/local.rs`
        // must not break the shipped-table assertion.
        let host = PluginHost::shipped().expect("shipped plugins");
        let effects = host
            .dispatch_command(&invocation(&host, "/help"), info(&host))
            .effects;
        let [PluginEffect::Notice(text)] = effects.as_slice() else {
            panic!("expected one help notice, got {effects:?}");
        };
        assert!(text.contains("/status"), "{text}");
        assert!(text.contains("/clear"), "{text}");
    }

    #[test]
    fn clear_quit_and_continue_are_effects() {
        // `shipped()` rather than `builtin()`: a user's `plugins/local.rs`
        // must not break the shipped-table assertion.
        let host = PluginHost::shipped().expect("shipped plugins");
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/clear"), info(&host))
                .effects,
            vec![PluginEffect::ClearConversation]
        );
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/quit"), info(&host))
                .effects,
            vec![PluginEffect::Quit]
        );
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/continue"), info(&host))
                .effects,
            vec![PluginEffect::ContinueTurn]
        );
    }

    #[test]
    fn status_reports_host_context() {
        // `shipped()` rather than `builtin()`: a user's `plugins/local.rs`
        // must not break the shipped-table assertion.
        let host = PluginHost::shipped().expect("shipped plugins");
        let effects = host
            .dispatch_command(&invocation(&host, "/status"), info(&host))
            .effects;
        let [PluginEffect::PresentReport(block)] = effects.as_slice() else {
            panic!("expected one status report popup, got {effects:?}");
        };
        assert_eq!(block.title, "codest status");
        let entry = |label: &str| {
            block.groups[0]
                .entries
                .iter()
                .find(|entry| entry.label == label)
                .unwrap_or_else(|| panic!("missing {label} entry: {:?}", block.groups[0]))
                .text
                .clone()
        };
        assert_eq!(entry("backend"), "test-model");
        assert_eq!(entry("mode"), "fullscreen");
        assert_eq!(entry("context"), "9.3k/200.0k (4%)");
        assert_eq!(entry("tokens"), "session in 16.2k / out 2.4k");
        assert_eq!(
            entry("plugins"),
            "help, clear, quit, status, continue, tps, ask_user_question, bash, fs"
        );
    }
}
