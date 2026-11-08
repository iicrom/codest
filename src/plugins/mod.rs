//! Compile-time ("static") plugin system.
//!
//! Plugins are ordinary Rust values linked into the binary. There is no
//! dynamic loading, no scripting layer, and no runtime discovery: the shipped
//! set is a `static` slice, so registration order and contents are fixed at
//! compile time. Adding a plugin means adding a module and one entry to
//! [`builtin::BUILTIN_PLUGINS`].
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

pub mod builtin;

use std::fmt;

use crate::core::commands::CommandTableError;
use crate::core::commands::{CommandInput, CommandTable};
use crate::core::providers::ProtocolFactory;

pub use crate::core::commands::{ArgsPolicy, CommandCandidate, CommandSpec, Invocation};
pub use builtin::BUILTIN_PLUGINS;

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

    /// Extra system-prompt text appended when the session starts.
    fn system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// Protocol kinds contributed to the provider registry. A custom
    /// provider = a `ProtocolFactory` here (the wire protocol, compile-time)
    /// plus instances in `providers.toml` (the endpoints, runtime config).
    /// See `core::providers`.
    fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
        &[]
    }

    /// Called once after the session is created and before the first frame.
    fn on_startup(&self, _ctx: &mut PluginContext<'_>) {}

    /// Called when a user turn is submitted, before it reaches the model.
    fn on_user_message(&self, _text: &str, _ctx: &mut PluginContext<'_>) {}

    /// Called when a turn finishes successfully, with the final assistant text.
    fn on_turn_complete(&self, _message: &str, _ctx: &mut PluginContext<'_>) {}

    /// Called after the core confirms that the conversation was cleared.
    fn on_conversation_cleared(&self, _ctx: &mut PluginContext<'_>) {}

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
    /// Drop the transcript and the core conversation.
    ClearConversation,
    /// Exit the application.
    Quit,
    /// Replace the composer contents.
    SetComposer(String),
    /// Submit a new user turn, if the agent is idle.
    SubmitUserTurn(String),
}

/// Read-only information passed to every hook and command handler.
#[derive(Debug, Clone, Copy)]
pub struct PluginInfo<'a> {
    pub model: &'a str,
    pub mode: &'a str,
    pub transcript_len: usize,
    pub plugin_ids: &'a [&'static str],
}

/// Mutable context handed to a plugin while it runs.
///
/// The context exposes read-only session facts and collects effects. It never
/// gives the plugin direct access to the terminal or the agent worker.
pub struct PluginContext<'a> {
    table: &'a CommandTable,
    info: PluginInfo<'a>,
    effects: Vec<PluginEffect>,
}

impl<'a> PluginContext<'a> {
    fn new(table: &'a CommandTable, info: PluginInfo<'a>) -> Self {
        Self {
            table,
            info,
            effects: Vec::new(),
        }
    }

    /// The full command table, useful for `/help`-style commands.
    pub fn command_table(&self) -> &CommandTable {
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

    pub fn plugin_ids(&self) -> &[&'static str] {
        self.info.plugin_ids
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

    fn into_effects(self) -> Vec<PluginEffect> {
        self.effects
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

/// Why the plugin registry could not be built.
#[derive(Debug, thiserror::Error)]
pub enum PluginHostError {
    #[error("plugin id must not be empty")]
    EmptyPluginId,
    #[error("plugin id `{id}` is registered more than once")]
    DuplicatePluginId { id: &'static str },
    #[error(transparent)]
    CommandTable(#[from] CommandTableError),
}

/// Owns the static plugin list and the command table they contribute.
pub struct PluginHost {
    plugins: &'static [&'static dyn Plugin],
    table: CommandTable,
    ids: Vec<&'static str>,
}

impl PluginHost {
    /// Validate plugin ids and build the command table they contribute.
    pub fn new(plugins: &'static [&'static dyn Plugin]) -> Result<Self, PluginHostError> {
        Self::with_extra_commands(plugins, &[])
    }

    /// Build a host whose command table also carries front-end built-in
    /// commands that no plugin owns (the core session commands and `/model`).
    /// Name collisions between plugins and extras are rejected like
    /// duplicates.
    fn with_extra_commands(
        plugins: &'static [&'static dyn Plugin],
        extra: &[&'static [CommandSpec]],
    ) -> Result<Self, PluginHostError> {
        let mut ids = Vec::with_capacity(plugins.len());
        for plugin in plugins {
            let id = plugin.id();
            if id.is_empty() {
                return Err(PluginHostError::EmptyPluginId);
            }
            if ids.contains(&id) {
                return Err(PluginHostError::DuplicatePluginId { id });
            }
            ids.push(id);
        }

        let specs = plugins
            .iter()
            .flat_map(|plugin| plugin.commands())
            .chain(extra.iter().flat_map(|slice| slice.iter()));
        let table = CommandTable::new(specs)?;
        Ok(Self {
            plugins,
            table,
            ids,
        })
    }

    /// The plugins compiled into the binary, plus the built-in front-end
    /// commands (`/new`, `/rename`, `/sessions`, `/model`, `/effort`,
    /// `/thinking`) that the front-end handles itself.
    pub fn builtin() -> Result<Self, PluginHostError> {
        Self::with_extra_commands(
            BUILTIN_PLUGINS,
            &[
                crate::core::sessions::SESSION_COMMANDS,
                crate::core::providers::MODEL_COMMANDS,
                crate::core::reasoning::REASONING_COMMANDS,
            ],
        )
    }

    pub fn plugins(&self) -> &'static [&'static dyn Plugin] {
        self.plugins
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

    pub fn parse(&self, input: &str) -> CommandInput {
        self.table.parse(input)
    }

    pub fn suggestions(&self, query: &str) -> Vec<CommandCandidate> {
        self.table.suggestions(query)
    }

    pub fn help_text(&self) -> String {
        self.table.help_text()
    }

    /// Run the owning plugin for an invocation and collect its effects.
    pub fn dispatch_command(
        &self,
        invocation: &Invocation,
        info: PluginInfo<'_>,
    ) -> Vec<PluginEffect> {
        let mut ctx = PluginContext::new(&self.table, info);
        match self.owner(invocation.spec.name) {
            Some(plugin) => {
                if let Err(error) = plugin.run_command(invocation, &mut ctx) {
                    ctx.error(error.to_string());
                }
            }
            None => ctx.error(format!(
                "no plugin owns `/{}`; the command table is inconsistent",
                invocation.spec.name
            )),
        }
        ctx.into_effects()
    }

    pub fn startup(&self, info: PluginInfo<'_>) -> Vec<PluginEffect> {
        self.run_hooks(info, |plugin, ctx| plugin.on_startup(ctx))
    }

    pub fn user_message(&self, text: &str, info: PluginInfo<'_>) -> Vec<PluginEffect> {
        self.run_hooks(info, |plugin, ctx| plugin.on_user_message(text, ctx))
    }

    pub fn turn_complete(&self, message: &str, info: PluginInfo<'_>) -> Vec<PluginEffect> {
        self.run_hooks(info, |plugin, ctx| plugin.on_turn_complete(message, ctx))
    }

    pub fn conversation_cleared(&self, info: PluginInfo<'_>) -> Vec<PluginEffect> {
        self.run_hooks(info, |plugin, ctx| plugin.on_conversation_cleared(ctx))
    }

    fn run_hooks(
        &self,
        info: PluginInfo<'_>,
        mut hook: impl FnMut(&dyn Plugin, &mut PluginContext<'_>),
    ) -> Vec<PluginEffect> {
        let mut ctx = PluginContext::new(&self.table, info);
        for plugin in self.plugins {
            hook(*plugin, &mut ctx);
        }
        ctx.into_effects()
    }

    fn owner(&self, name: &str) -> Option<&'static dyn Plugin> {
        self.plugins
            .iter()
            .copied()
            .find(|plugin| plugin.commands().iter().any(|spec| spec.name == name))
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
        PluginInfo {
            model: "test-model",
            mode: "fullscreen",
            transcript_len: 2,
            plugin_ids: host.plugin_ids(),
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
        let effects = host.dispatch_command(&invocation(&host, "/echo hi"), info(&host));
        assert_eq!(effects, vec![PluginEffect::Notice("echo: hi".to_string())]);
    }

    #[test]
    fn plugin_errors_become_error_effects() {
        let host = PluginHost::new(FAILING_PLUGINS).expect("valid plugins");
        let effects = host.dispatch_command(&invocation(&host, "/fail"), info(&host));
        assert_eq!(effects, vec![PluginEffect::Error("boom".to_string())]);
    }

    #[test]
    fn hooks_run_in_registration_order() {
        let host = host();
        assert_eq!(
            host.startup(info(&host)),
            vec![PluginEffect::Notice("audit:startup".to_string())]
        );
        assert_eq!(
            host.user_message("hello", info(&host)),
            vec![PluginEffect::Notice("audit:user:hello".to_string())]
        );
        assert_eq!(
            host.turn_complete("done", info(&host)),
            vec![PluginEffect::Notice("audit:complete:done".to_string())]
        );
        assert_eq!(
            host.conversation_cleared(info(&host)),
            vec![PluginEffect::Notice("audit:cleared".to_string())]
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
    fn builtin_host_exposes_the_core_commands() {
        let host = PluginHost::builtin().expect("builtin plugins");
        let names = host
            .table()
            .specs()
            .iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "help", "clear", "quit", "status", "new", "rename", "sessions", "model",
                "effort", "thinking"
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
            requires_api_key: false,
        };
        let registry = crate::core::providers::ProviderRegistry::build(&factories, vec![instance])
            .expect("the plugin protocol is known");
        assert_eq!(registry.protocol_display_name("vendor-protocol"), "Vendor (example)");

        // Registering the same protocol id twice in one build is an error.
        let error = crate::core::providers::ProviderRegistry::build(
            &[factories[0], factories[0]],
            vec![],
        )
        .unwrap_err();
        assert!(error.to_string().contains("more than once"), "{error}");
    }


    #[test]
    fn help_lists_plugin_commands() {
        let host = PluginHost::builtin().expect("builtin plugins");
        let effects = host.dispatch_command(&invocation(&host, "/help"), info(&host));
        let [PluginEffect::Notice(text)] = effects.as_slice() else {
            panic!("expected one help notice, got {effects:?}");
        };
        assert!(text.contains("/status"), "{text}");
        assert!(text.contains("/clear"), "{text}");
    }

    #[test]
    fn clear_and_quit_are_effects() {
        let host = PluginHost::builtin().expect("builtin plugins");
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/clear"), info(&host)),
            vec![PluginEffect::ClearConversation]
        );
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/quit"), info(&host)),
            vec![PluginEffect::Quit]
        );
    }

    #[test]
    fn status_reports_host_context() {
        let host = PluginHost::builtin().expect("builtin plugins");
        let effects = host.dispatch_command(&invocation(&host, "/status"), info(&host));
        let [PluginEffect::Notice(text)] = effects.as_slice() else {
            panic!("expected one status notice, got {effects:?}");
        };
        assert!(text.contains("backend: test-model"), "{text}");
        assert!(text.contains("mode: fullscreen"), "{text}");
        assert!(
            text.contains("plugins: help, clear, quit, status"),
            "{text}"
        );
    }
}
