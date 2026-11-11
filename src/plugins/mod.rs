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

use cst_plugin_api::auth::AuthProvider;
use cst_plugin_api::auth::AuthStatus;
use cst_plugin_api::commands::CommandTableError;
use cst_plugin_api::commands::{CommandInput, CommandTable};
use cst_plugin_api::providers::ProtocolFactory;
use cst_plugin_api::tools::ToolCall;
use cst_plugin_api::tools::ToolExecutor;
use cst_plugin_api::tools::ToolSpec;

pub use cst_plugin_api::commands::{ArgsPolicy, CommandCandidate, CommandSpec, Invocation};
pub use cst_plugin_api::plugin::{
    Plugin, PluginContext, PluginEffect, PluginError, PluginInfo, ReportBlock, ReportEntry,
    ReportGroup, RequestFacts,
};
pub use builtin::BUILTIN_PLUGINS;

/// Why the plugin registry could not be built.
#[derive(Debug, thiserror::Error)]
pub enum PluginHostError {
    #[error("plugin id must not be empty")]
    EmptyPluginId,
    #[error("plugin id `{id}` is registered more than once")]
    DuplicatePluginId { id: &'static str },
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
        let plugins: Vec<&'static dyn Plugin> =
            groups.iter().flat_map(|group| group.iter().copied()).collect();

        let mut ids = Vec::with_capacity(plugins.len());
        for plugin in &plugins {
            let id = plugin.id();
            if id.is_empty() {
                return Err(PluginHostError::EmptyPluginId);
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

        Ok(Self {
            plugins,
            table,
            ids,
            tools,
            tool_filter,
        })
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
        for plugin in &self.plugins {
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
        PluginInfo {
            model: "test-model",
            mode: "fullscreen",
            transcript_len: 2,
            context_window: Some(200_000),
            last_usage: Some((8_100, 1_200)),
            total_input: 16_200,
            total_output: 2_400,
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
        let host = PluginHost::with_extra_commands(
            &[FILTERED],
            &[],
            Some(vec!["echo".to_string()]),
        )
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
        let host = PluginHost::with_extra_commands(
            &[FILTERED],
            &[],
            Some(vec!["tooler".to_string()]),
        )
        .expect("valid host");
        assert_eq!(host.tools(), vec![PING_TOOL]);
    }

    #[test]
    fn an_unknown_tool_filter_id_is_a_startup_error() {
        let error = PluginHost::builtin_with_tools(Some(&["nope".to_string()])).unwrap_err();
        assert!(matches!(error, PluginHostError::UnknownFilteredPlugin { .. }));
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
        assert!(host.plugin_ids().contains(&"failing"), "{:?}", host.plugin_ids());
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
        let host = PluginHost::with_local(TOOLERS, Some(&["tooler".to_string()]))
            .expect("valid host");
        assert_eq!(host.tools(), vec![PING_TOOL]);

        let off = PluginHost::with_local(TOOLERS, Some(&["help".to_string()]))
            .expect("valid host");
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
                "help", "clear", "quit", "status", "continue", "new", "rename", "sessions",
                "model", "provider", "effort", "thinking", "profile", "tools", "context"
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
        // `shipped()` rather than `builtin()`: a user's `plugins/local.rs`
        // must not break the shipped-table assertion.
        let host = PluginHost::shipped().expect("shipped plugins");
        let effects = host.dispatch_command(&invocation(&host, "/help"), info(&host));
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
            host.dispatch_command(&invocation(&host, "/clear"), info(&host)),
            vec![PluginEffect::ClearConversation]
        );
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/quit"), info(&host)),
            vec![PluginEffect::Quit]
        );
        assert_eq!(
            host.dispatch_command(&invocation(&host, "/continue"), info(&host)),
            vec![PluginEffect::ContinueTurn]
        );
    }

    #[test]
    fn status_reports_host_context() {
        // `shipped()` rather than `builtin()`: a user's `plugins/local.rs`
        // must not break the shipped-table assertion.
        let host = PluginHost::shipped().expect("shipped plugins");
        let effects = host.dispatch_command(&invocation(&host, "/status"), info(&host));
        let [PluginEffect::Notice(text)] = effects.as_slice() else {
            panic!("expected one status notice, got {effects:?}");
        };
        assert!(text.contains("backend: test-model"), "{text}");
        assert!(text.contains("mode: fullscreen"), "{text}");
        assert!(text.contains("context: 9.3k/200.0k (4%)"), "{text}");
        assert!(text.contains("tokens: session in 16.2k / out 2.4k"), "{text}");
        assert!(
            text.contains("plugins: help, clear, quit, status"),
            "{text}"
        );
    }
}
