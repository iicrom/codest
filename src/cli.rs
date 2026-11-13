//! Command-line parsing and startup assembly shared by the `codest` binary
//! and its `cst` alias.
//!
//! The agent talks to any provider declared in `~/.codest/providers.toml`
//! (protocol registry × instances); without that file it synthesizes the
//! historical single OpenAI-compatible endpoint from flags and env, so
//! `--api-key/--base-url/--model` keep working unchanged.
//!
//! For local end-to-end testing, run `scripts/openai_test_server.py` and
//! point an instance's `base_url` at it.

use std::path::PathBuf;
use std::sync::Arc;

use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::Subcommand;

use crate::core::config;
use crate::core::providers::ModelPool;
use crate::core::providers::ProviderRegistry;
use crate::core::providers::RegistryError;
use crate::core::providers::Selection;
use crate::plugins::PluginHost;
use crate::plugins::install;
use crate::tui::RestoreMode;
use crate::tui::TuiMode;

#[derive(Parser, Debug)]
#[command(version, about = "A minimal TUI chat agent distilled from codex")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Model id sent to the backend (the `model` half of a selection).
    #[arg(long, env = "CST_MODEL")]
    model: Option<String>,

    /// Provider instance to start with (the `provider` half of a selection).
    #[arg(long, env = "CST_PROVIDER")]
    provider: Option<String>,

    /// OpenAI-compatible base URL, or a full /chat/completions URL.
    /// Overrides the selected instance's `base_url`.
    #[arg(long, env = "CST_BASE_URL")]
    base_url: Option<String>,

    /// API key for the endpoint. Overrides the selected instance's `api_key`.
    #[arg(long, env = "CST_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Override the system prompt.
    #[arg(long)]
    system: Option<String>,

    /// Continue the most recent saved session of this workspace.
    #[arg(long, short = 'c', conflicts_with = "resume")]
    continue_: bool,

    /// Load every saved session of this workspace and open the session picker
    /// at startup.
    #[arg(long)]
    resume: bool,

    /// Start with a profile from `~/.codest/profiles.toml`: its default
    /// selection, initial effort, plugin tool set, and system-prompt
    /// addition. An unknown name falls back to no profile with a note.
    #[arg(long, env = "CST_PROFILE")]
    profile: Option<String>,

    /// Terminal UI mode: fullscreen (alternate screen) or inline (scrollback).
    #[arg(long, value_enum, env = "CST_TUI_MODE", default_value = "fullscreen")]
    tui_mode: TuiMode,
}

/// Subcommands. Without one, codest starts the TUI as usual.
#[derive(Subcommand, Debug)]
enum Command {
    /// Install, remove, list, and rebuild linked plugins.
    ///
    /// Linked plugins live in a generated crate under `~/.codest/`, so the
    /// codest checkout is never modified.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(Subcommand, Debug)]
// clap's derive builds each variant's fields in place, so boxing the payload
// is not available here. The enum lives on the stack once at startup.
#[allow(clippy::large_enum_variant)]
enum PluginAction {
    /// Add (or reinstall) a plugin and rebuild the plugin-linked binary.
    Install {
        /// A checkout path, a crates.io package name, or a git URL.
        target: Option<String>,

        /// Install the named package from this git repository.
        #[arg(long, value_name = "URL")]
        git: Option<String>,

        /// Package name, when it cannot be inferred from the target.
        #[arg(long, value_name = "NAME")]
        package: Option<String>,

        /// Pin a git revision.
        #[arg(long, conflicts_with_all = ["branch", "tag"])]
        rev: Option<String>,

        /// Follow a git branch.
        #[arg(long)]
        branch: Option<String>,

        /// Pin a git tag.
        #[arg(long)]
        tag: Option<String>,

        /// Version requirement for a crates.io install (default `*`).
        #[arg(long)]
        version: Option<String>,

        /// Exported `static` to link; repeat for several (default `PLUGIN`,
        /// or the plugin's `[package.metadata.codest] plugins`).
        #[arg(long = "symbol", value_name = "NAME")]
        symbols: Vec<String>,

        /// Rust identifier for the plugin crate (default: package name with
        /// `-` replaced by `_`).
        #[arg(long = "crate-name", value_name = "NAME")]
        crate_name: Option<String>,

        /// The codest checkout to link against (default: this source tree,
        /// recorded state, or `$CODEST_SRC`).
        #[arg(long = "codest-src", value_name = "PATH")]
        codest_src: Option<PathBuf>,

        /// Only rewrite the plugin list and generated crate; do not build.
        #[arg(long)]
        no_build: bool,
    },

    /// Remove a plugin and rebuild the plugin-linked binary.
    Uninstall {
        /// Package name as listed by `codest plugin list`.
        name: String,

        /// Only rewrite the plugin list and generated crate; do not build.
        #[arg(long)]
        no_build: bool,
    },

    /// List installed plugins.
    List,

    /// Regenerate the wrapper from the saved list and build it.
    Build {
        /// Build with optimizations.
        #[arg(long)]
        release: bool,

        /// `cargo install` the result instead of only building it.
        #[arg(long)]
        install: bool,

        /// The codest checkout to link against.
        #[arg(long = "codest-src", value_name = "PATH")]
        codest_src: Option<PathBuf>,
    },
}

/// Parse arguments and run the TUI.
///
/// `program` is the invoked binary name (`codest` or its `cst` alias); it is
/// used in clap's help and version output.
pub async fn run(program: &'static str) -> anyhow::Result<()> {
    run_with(program, &[]).await
}

/// [`run`] with externally-linked plugins injected into the registry.
///
/// A binary built by `codest plugin install` passes the `static` slice from
/// its generated crate here; the shipped binary passes an empty one.
pub async fn run_with(
    program: &'static str,
    local: &'static [&'static dyn crate::plugins::Plugin],
) -> anyhow::Result<()> {
    let command = Args::command().name(program);
    let matches = command.get_matches();
    let args = Args::from_arg_matches(&matches)?;

    // A subcommand is a one-shot operation: it must not touch the config
    // files or the TUI, which would fail in a fresh terminal-less context.
    if let Some(command) = args.command {
        return run_command(command);
    }

    let profiles = std::sync::Arc::new(
        config::load_profiles(&config::profiles_path())
            .map_err(|error| anyhow::anyhow!("{error}"))?,
    );
    // The active profile borrows through the `Arc`; the TUI gets a clone.
    let active: Option<(&str, &config::Profile)> = args.profile.as_deref().and_then(|name| {
        let profile = profiles.get(name);
        if profile.is_none() {
            note_missing_profile(name, &profiles);
        }
        profile.map(|profile| (name, profile))
    });
    let plugins = std::sync::Arc::new(PluginHost::with_local(
        local,
        active.and_then(|(_, profile)| profile.tools.as_deref()),
    )?);
    let global = config::load_global(&config::config_path())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (pool, pinned) = build_pool(&plugins, &global, &args)?;
    let registry = pool.registry();
    let selection = initial_selection(
        &registry,
        &global,
        &args,
        active.map(|(_, p)| p),
        pinned.as_deref(),
    )?;

    // Fail fast on an unusable selection rather than at the first turn.
    if let Some(selection) = &selection {
        pool.client(selection)?;
    }

    let restore = if args.continue_ {
        RestoreMode::Continue
    } else if args.resume {
        RestoreMode::Picker
    } else {
        RestoreMode::None
    };
    crate::tui::run(
        plugins,
        pool,
        selection,
        args.system,
        args.tui_mode,
        restore,
        crate::tui::ProfileStartup {
            set: profiles.clone(),
            active: active.map(|(name, _)| name.to_string()),
        },
        global.max_tool_rounds,
        global.status_bar,
        global
            .plugins
            .iter()
            .map(|(id, settings)| (id.clone(), settings.clone()))
            .collect(),
    )
    .await
}

/// Blocking entry point for generated wrapper binaries.
///
/// The generated crate has no `tokio`/`anyhow` dependency of its own, so this
/// builds a runtime, runs [`run_with`], and reports a failure on stderr with a
/// non-zero exit instead of unwinding.
pub fn run_blocking(
    program: &'static str,
    local: &'static [&'static dyn crate::plugins::Plugin],
) {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{program}: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(run_with(program, local)) {
        eprintln!("{program}: {error:#}");
        std::process::exit(1);
    }
}

/// Run one of the `codest plugin …` subcommands.
fn run_command(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Plugin { action } => run_plugin(action),
    }
}

fn run_plugin(action: PluginAction) -> anyhow::Result<()> {
    match action {
        PluginAction::Install {
            target,
            git,
            package,
            rev,
            branch,
            tag,
            version,
            symbols,
            crate_name,
            codest_src,
            no_build,
        } => {
            let request = install::InstallRequest {
                target,
                git,
                package,
                rev,
                branch,
                tag,
                version,
                symbols,
                crate_name,
                codest_src,
            };
            // Remembered before the request moves into the installer: a
            // non-path source cannot be inspected up front, so a failure
            // there gets one extra line of guidance.
            let remote = request.git.is_some()
                || request
                    .target
                    .as_deref()
                    .is_some_and(|target| !std::path::Path::new(target).exists());
            let outcome = install::install(&request, !no_build).map_err(|error| {
                if remote {
                    anyhow::anyhow!(
                        "{error}\n\n\
                         note: a git or crates.io plugin must depend on `cst-plugin-api` \
                         by version, not by a relative `path` — a checkout that only \
                         works next to this repo cannot be built from a clone. \
                         Install such a plugin with `codest plugin install <local path>` \
                         instead."
                    )
                } else {
                    anyhow::anyhow!("{error}")
                }
            })?;
            let verb = if outcome.replaced { "replaced" } else { "installed" };
            println!(
                "{verb} {} ({})",
                outcome.plugin.name,
                source_label(&outcome.plugin)
            );
            println!("  list   {}", outcome.state_path.display());
            println!("  crate  {}", outcome.crate_dir.display());
            match &outcome.binary {
                Some(binary) => {
                    println!("  binary {}", binary.display());
                    println!();
                    println!("Run it, or `codest plugin build --install` to replace the");
                    println!("installed command.");
                }
                None => println!("  (not built; run `codest plugin build`)"),
            }
            Ok(())
        }
        PluginAction::Uninstall { name, no_build } => {
            let outcome =
                install::uninstall(&name, !no_build).map_err(|error| anyhow::anyhow!("{error}"))?;
            println!("removed {} ({} plugin(s) left)", outcome.name, outcome.remaining);
            match &outcome.binary {
                Some(binary) => println!("  rebuilt {}", binary.display()),
                None => println!("  (not rebuilt; run `codest plugin build`)"),
            }
            Ok(())
        }
        PluginAction::List => {
            let state = install::list().map_err(|error| anyhow::anyhow!("{error}"))?;
            if state.plugins.is_empty() {
                println!("no plugins installed");
                println!(
                    "note: linked plugins are compile-time — absent ones are simply not in \
                     this binary"
                );
                return Ok(());
            }
            for plugin in &state.plugins {
                println!(
                    "{}  {}  symbols: {}",
                    plugin.name,
                    source_label(plugin),
                    plugin.symbols.join(", ")
                );
            }
            if let Some(src) = &state.codest_src {
                println!();
                println!("linked against {src}");
            }
            Ok(())
        }
        PluginAction::Build {
            release,
            install,
            codest_src,
        } => {
            if release && install {
                // `cargo install` picks release on its own; asking for both
                // would silently ignore the flag.
                return Err(anyhow::anyhow!(
                    "--release and --install are alternatives: `cargo install` \
                     always builds optimized"
                ));
            }
            let outcome = install::build(codest_src.as_deref(), release, install)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            println!("built {} ({})", outcome.crate_dir.display(), outcome.profile);
            for binary in &outcome.binaries {
                println!("  {}", binary.display());
            }
            Ok(())
        }
    }
}

fn source_label(plugin: &install::InstalledPlugin) -> String {
    match plugin.kind() {
        install::SourceKind::Path => format!(
            "path {}",
            plugin.path.as_deref().unwrap_or_default()
        ),
        install::SourceKind::Git => {
            let mut label = format!(
                "git {}",
                plugin.git.as_deref().unwrap_or_default()
            );
            for (key, value) in [
                ("rev", &plugin.rev),
                ("branch", &plugin.branch),
                ("tag", &plugin.tag),
            ] {
                if let Some(value) = value {
                    label.push_str(&format!(" ({key} {value})"));
                }
            }
            label
        }
        install::SourceKind::Registry => {
            format!("crates.io {}", plugin.version.as_deref().unwrap_or("*"))
        }
    }
}

/// The note for a `--profile` naming nothing: the run continues without a
/// profile — a wrong name must neither fail the startup nor pass silently.
fn note_missing_profile(name: &str, profiles: &config::ProfileSet) {
    let available = if profiles.is_empty() {
        "none".to_string()
    } else {
        profiles
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!(
        "codest: note: no profile `{name}` in {} (available: {available}); \
         continuing without one",
        config::profiles_path().display()
    );
}

/// Load the configuration files and build the provider pool.
///
/// There is no built-in default provider: an endpoint is synthesized from
/// flags/env only when `providers.toml` is empty **and** something actually
/// names one (`--api-key`/`--base-url`, `CST_API_KEY`/`OPENAI_API_KEY`,
/// `OPENAI_BASE_URL` — local test servers need no key, so a lone base URL
/// counts). With nothing configured at all the pool starts empty and the
/// front-end guides the user to add a provider.
///
/// The second element of the pair is the provider name this run's flags
/// pinned (see [`resolve_instances`]); the startup selection honours it over
/// any configured default.
fn build_pool(
    plugins: &PluginHost,
    global: &config::GlobalConfig,
    args: &Args,
) -> anyhow::Result<(Arc<ModelPool>, Option<String>)> {
    let loaded = config::load_providers(&config::providers_path())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (instances, pinned) = resolve_instances(loaded, global, args)?;

    let factories = plugins.protocol_factories();
    let registry = ProviderRegistry::build(&factories, instances)
        .map_err(|error: RegistryError| anyhow::anyhow!("{error}"))?;
    Ok((
        Arc::new(ModelPool::new(registry).with_cache(config::model_cache_path())),
        pinned,
    ))
}

/// The provider name a `--base-url` endpoint always runs under.
///
/// A flag-named endpoint is a route of its own, not a rewrite of somebody
/// else's: the model cache is keyed by provider name, so borrowing a
/// configured instance's name would make two different endpoints share one
/// cached model list.
const UNTITLED_PROVIDER: &str = "untitled";

/// Apply the flag/env endpoint rules to the configured instances.
///
/// * `--base-url` (or `CST_BASE_URL`) names its own endpoint: a throwaway
///   `untitled` instance is appended to the configured ones, which stay
///   selectable through `/model`. `--provider` cannot apply at the same time
///   and is noted as ignored. This is also what keeps a one-off endpoint from
///   overwriting a configured provider's `base_url`, and therefore from
///   polluting that provider's cached model list. `OPENAI_BASE_URL` is
///   deliberately not consulted here: it is a fallback for the
///   *nothing-configured* path, so exporting it must not hijack a normal run.
/// * Otherwise, with nothing configured, the historical flag/env synthesis
///   applies unchanged.
/// * Otherwise only `--api-key` can still act: it completes the instance this
///   run selects without renaming or re-pointing it.
///
/// The returned name is the provider this run pinned, if any.
fn resolve_instances(
    mut instances: Vec<config::ProviderInstance>,
    global: &config::GlobalConfig,
    args: &Args,
) -> anyhow::Result<(Vec<config::ProviderInstance>, Option<String>)> {
    let configured = !instances.is_empty();
    if args
        .base_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
    {
        if args.provider.is_some() {
            eprintln!(
                "codest: note: --provider ignored: --base-url runs its own \
                 `{UNTITLED_PROVIDER}` instance"
            );
        }
        let synthesized = synthesized_openai_instance(args)
            .expect("a non-empty `--base-url` always synthesizes");
        // A stale same-named entry in providers.toml would collide with it.
        instances.retain(|instance| instance.name != synthesized.name);
        instances.push(synthesized);
        if !configured {
            note_synthesized_endpoint();
        }
        return Ok((instances, Some(UNTITLED_PROVIDER.to_string())));
    }

    if !configured {
        return match synthesized_openai_instance(args) {
            Some(instance) => {
                note_synthesized_endpoint();
                Ok((vec![instance], None))
            }
            None => Ok((Vec::new(), None)),
        };
    }

    // Flags override the instance this run selects (see
    // `resolve_provider_name` for which one that is).
    if args.api_key.is_some() {
        let target =
            resolve_provider_name(global, args).unwrap_or_else(|| instances[0].name.clone());
        let Some(instance) = instances.iter_mut().find(|i| i.name == target) else {
            return Err(anyhow::anyhow!(
                "no provider `{target}` in {} (configured: {})",
                config::providers_path().display(),
                instances
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        instance.api_key = args.api_key.clone();
    }
    Ok((instances, None))
}

/// The one-line note for the nothing-configured path: the run is using an
/// endpoint that only exists in memory.
fn note_synthesized_endpoint() {
    eprintln!(
        "codest: no providers.toml; using the endpoint from flags/env \
         (create {} to add more)",
        config::providers_path().display()
    );
}

/// The pre-config single endpoint — only when flags/env actually name one.
///
/// `base_url` and `api_key` are a pair: either alone synthesizes an
/// *incomplete* instance that is gated at use time with a prompt to supply
/// the other. Neither at all means no endpoint and an empty route. There are
/// no defaults here — no `api.openai.com`, no fallback model id: a synthesized
/// instance without a model discovers its list from `base_url` via `/model`.
fn synthesized_openai_instance(args: &Args) -> Option<config::ProviderInstance> {
    let api_key = args
        .api_key
        .clone()
        .or_else(|| std::env::var("CST_API_KEY").ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .filter(|key| !key.trim().is_empty());
    let base_url = args
        .base_url
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .filter(|url| !url.trim().is_empty());

    if api_key.is_none() && base_url.is_none() {
        return None;
    }

    let model = args
        .model
        .clone()
        .or_else(|| std::env::var("CST_MODEL").ok());
    Some(config::ProviderInstance {
        name: UNTITLED_PROVIDER.to_string(),
        protocol: None,
        base_url,
        api_key,
        // With an explicit model the initial selection is fully specified;
        // discovery still runs (best-effort): it fills the /model menu with
        // the endpoint's other models and enriches the metadata — the
        // context window among them.
        models: model.map(config::ModelInfo::from_id).into_iter().collect(),
        fetch_models: Some(true),
        headers: Default::default(),
        body_overrides: Default::default(),
        wire: None,
        options: Default::default(),
        requires_api_key: true,
    })
}

/// Which instance this run selects, before the registry exists: `--provider`,
/// else the provider half of config.toml's `default`.
fn resolve_provider_name(global: &config::GlobalConfig, args: &Args) -> Option<String> {
    if let Some(provider) = args.provider.as_deref() {
        return Some(provider.to_string());
    }
    global
        .default
        .as_deref()
        .and_then(|selection| selection.split_once('/'))
        .map(|(provider, _)| provider.to_string())
}

/// The selection this run starts with; `None` means no route at all (the
/// front-end guides the user to add a provider).
///
/// Precedence: a flag-pinned provider (`--base-url`'s `untitled` route) >
/// `--provider`/`--model` > the active profile's `default` > config.toml
/// `default` > the first model of the first instance. A model id nobody
/// declared yet (it may still arrive through models discovery) proceeds with
/// a warning; an unknown *provider* is a hard error.
fn initial_selection(
    registry: &ProviderRegistry,
    global: &config::GlobalConfig,
    args: &Args,
    profile: Option<&config::Profile>,
    pinned: Option<&str>,
) -> anyhow::Result<Option<Selection>> {
    // No providers at all: an empty route wins over a stray `default`, so
    // the TUI can still start and guide the user.
    if registry.instances().is_empty() {
        if args.provider.is_some() || args.model.is_some() {
            eprintln!("codest: --provider/--model ignored: no providers are configured");
        }
        return Ok(None);
    }

    // The preset this run starts from: the active profile's declared
    // selection when one is active, else the global default. Flags still
    // override either half of it.
    let preset = profile
        .and_then(config::Profile::default_selection)
        .or(global.default.as_deref());
    let selection = if let Some(pinned) = pinned {
        // The endpoint named by `--base-url` is the route this run asked
        // for; no configured default may redirect it. (`--model` still
        // supplies the model half, and an empty one waits for discovery.)
        Some(Selection::new(
            pinned,
            args.model.clone().unwrap_or_default(),
        ))
    } else if let Some(provider) = args.provider.as_deref() {
        let model = args.model.clone().unwrap_or_else(|| {
            registry
                .instance(provider)
                .and_then(|instance| instance.models.first())
                .map(|model| model.id.clone())
                .unwrap_or_default()
        });
        Some(Selection::new(provider, model))
    } else if let Some(default) = preset {
        let parsed = registry
            .parse_selection(default)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Some(match args.model.as_deref() {
            Some(model) if model != parsed.model => Selection::new(&parsed.provider, model),
            _ => parsed,
        })
    } else if let Some(model) = args.model.as_deref() {
        let provider = registry
            .instances()
            .first()
            .map(|instance| instance.name.clone())
            .ok_or_else(|| anyhow::anyhow!("no providers configured"))?;
        Some(Selection::new(provider, model))
    } else {
        registry.default_selection(None).ok()
    };

    // A provider whose model list is still empty starts with an empty
    // route — but only when no model was actually named: an explicitly
    // requested model id is kept and the endpoint validates it, while an
    // invented one (no `--model`, no default) waits for `/model` to discover
    // the list. Without discovery there is nothing that could ever fill the
    // list, so an invented id with discovery off stays an error.
    let selection = match selection {
        Some(selection) => {
            let needs_model_fill = registry
                .instance(&selection.provider)
                .is_some_and(|instance| {
                    instance.models.is_empty() && selection.model.is_empty()
                });
            if needs_model_fill {
                let instance = registry
                    .instance(&selection.provider)
                    .expect("checked above");
                if instance.should_fetch_models() {
                    eprintln!(
                        "codest: note: provider `{}` has no models yet; \
                         /model will discover them",
                        selection.provider
                    );
                    None
                } else {
                    return Err(anyhow::anyhow!(
                        "provider `{}` has no models; declare `[[providers.models]]` \
                         or set --model",
                        selection.provider
                    ));
                }
            } else {
                Some(selection)
            }
        }
        None => None,
    };

    if let Some(selection) = &selection
        && let Some(instance) = registry.instance(&selection.provider)
        && !instance.models.is_empty()
        && !instance.models.iter().any(|model| model.id == selection.model)
    {
        eprintln!(
            "codest: note: model `{}` is not declared for provider `{}`; \
             it may still be valid (declared models arrive via fetch_models)",
            selection.model, selection.provider
        );
    }
    Ok(selection)
}


#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["codest"];
        argv.extend_from_slice(extra);
        Args::parse_from(argv)
    }

    /// A registry with two providers, each declaring two static models.
    fn two_provider_registry() -> ProviderRegistry {
        let instance = |name: &str| config::ProviderInstance {
            name: name.to_string(),
            protocol: Some("test".to_string()),
            base_url: Some("http://127.0.0.1:1/v1".to_string()),
            api_key: Some("k".to_string()),
            models: vec![
                config::ModelInfo::from_id("m1"),
                config::ModelInfo::from_id("m2"),
            ],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        ProviderRegistry::build(&[], vec![instance("p"), instance("q")]).expect("registry")
    }

    #[test]
    fn profile_default_selection_wins_over_the_global_one() {
        let registry = two_provider_registry();
        let global = config::GlobalConfig {
            default: Some("q/m2".to_string()),
            max_tool_rounds: None,
            ..Default::default()
        };
        let profile = config::Profile {
            default: Some("p/m2".to_string()),
            ..Default::default()
        };

        // The profile's default beats the global default.
        let selection =
            initial_selection(&registry, &global, &args(&[]), Some(&profile), None).unwrap();
        assert_eq!(selection, Some(Selection::new("p", "m2")));

        // `--model` still overrides the preset's model half.
        let selection = initial_selection(
            &registry,
            &global,
            &args(&["--model", "m1"]),
            Some(&profile),
            None,
        )
        .unwrap();
        assert_eq!(selection, Some(Selection::new("p", "m1")));

        // `--provider` wins over the preset outright (first declared model).
        let selection = initial_selection(
            &registry,
            &global,
            &args(&["--provider", "q"]),
            Some(&profile),
            None,
        )
        .unwrap();
        assert_eq!(selection, Some(Selection::new("q", "m1")));

        // Without a profile the global default applies again.
        let selection = initial_selection(&registry, &global, &args(&[]), None, None).unwrap();
        assert_eq!(selection, Some(Selection::new("q", "m2")));
    }

    /// `--base-url` names its own route: the configured instances keep their
    /// names, URLs, and cached model lists, and the run is pinned to the
    /// throwaway `untitled` instance.
    #[test]
    fn base_url_synthesizes_untitled_alongside_configured_instances() {
        let configured = |name: &str| config::ProviderInstance {
            name: name.to_string(),
            protocol: None,
            base_url: Some("https://configured.test/v1".to_string()),
            api_key: Some("k".to_string()),
            models: vec![config::ModelInfo::from_id("m1")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let global = config::GlobalConfig::default();
        let loaded = vec![configured("opencode-go")];

        let (instances, pinned) = resolve_instances(
            loaded.clone(),
            &global,
            &args(&[
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--api-key",
                "one-off",
            ]),
        )
        .expect("resolves");

        assert_eq!(pinned.as_deref(), Some("untitled"));
        assert_eq!(
            instances.len(),
            2,
            "the configured instance survives and the endpoint joins it"
        );
        // The configured endpoint is untouched: no `base_url` rewrite, and so
        // no cache pollution under its name.
        let kept = instances
            .iter()
            .find(|instance| instance.name == "opencode-go")
            .expect("configured instance kept");
        assert_eq!(kept.base_url.as_deref(), Some("https://configured.test/v1"));
        assert_eq!(kept.api_key.as_deref(), Some("k"));
        // The endpoint is its own instance.
        let synthesized = instances
            .iter()
            .find(|instance| instance.name == "untitled")
            .expect("synthesized instance");
        assert_eq!(synthesized.base_url.as_deref(), Some("http://127.0.0.1:9/v1"));
        assert_eq!(synthesized.api_key.as_deref(), Some("one-off"));

        // A stale same-named entry would collide, so it is replaced.
        let with_untitled = {
            let mut stale = configured("untitled");
            stale.base_url = Some("https://stale.test/v1".to_string());
            vec![configured("opencode-go"), stale]
        };
        let (instances, _) = resolve_instances(
            with_untitled,
            &global,
            &args(&["--base-url", "http://127.0.0.1:9/v1"]),
        )
        .expect("resolves");
        let untitled: Vec<_> = instances
            .iter()
            .filter(|instance| instance.name == "untitled")
            .collect();
        assert_eq!(untitled.len(), 1, "exactly one `untitled` instance");
        assert_eq!(untitled[0].base_url.as_deref(), Some("http://127.0.0.1:9/v1"));
    }

    /// Only `--api-key` still completes the selected instance in place; it
    /// must not rename or re-point anything.
    #[test]
    fn api_key_alone_overrides_the_selected_instance() {
        let instance = config::ProviderInstance {
            name: "p".to_string(),
            protocol: Some("test".to_string()),
            base_url: Some("http://127.0.0.1:1/v1".to_string()),
            api_key: None,
            models: vec![config::ModelInfo::from_id("m1")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        let global = config::GlobalConfig {
            default: Some("p/m1".to_string()),
            max_tool_rounds: None,
            ..Default::default()
        };

        let (instances, pinned) =
            resolve_instances(vec![instance.clone()], &global, &args(&["--api-key", "secret"]))
                .expect("resolves");
        assert_eq!(pinned, None, "no endpoint flag, no pinned route");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, "p");
        assert_eq!(instances[0].base_url, instance.base_url);
        assert_eq!(instances[0].api_key.as_deref(), Some("secret"));
    }

    /// A pinned route outranks every configured default — including a stale
    /// `config.toml` default naming a provider that no longer exists.
    #[test]
    fn pinned_route_ignores_configured_defaults() {
        let registry = two_provider_registry();
        let global = config::GlobalConfig {
            default: Some("gone/m2".to_string()),
            max_tool_rounds: None,
            ..Default::default()
        };

        let selection = initial_selection(
            &registry,
            &global,
            &args(&["--base-url", "http://127.0.0.1:9/v1"]),
            None,
            Some("p"),
        )
        .expect("a stray default must not fail the run");
        // No `--model`: the model half waits for discovery.
        assert_eq!(selection, Some(Selection::new("p", "")));

        // `--model` still supplies the model half.
        let selection = initial_selection(
            &registry,
            &global,
            &args(&["--base-url", "http://127.0.0.1:9/v1", "--model", "m2"]),
            None,
            Some("p"),
        )
        .expect("resolves");
        assert_eq!(selection, Some(Selection::new("p", "m2")));
    }

    /// A pool over one synthesized-style instance, for gate assertions.
    fn pool_of(instance: config::ProviderInstance) -> Arc<ModelPool> {
        let registry = ProviderRegistry::build(&[], vec![instance]).expect("registry");
        Arc::new(ModelPool::new(registry))
    }

    #[test]
    fn synthesis_requires_an_endpoint_half() {
        // Nothing named: no instance, no defaults.
        assert!(synthesized_openai_instance(&args(&[])).is_none());
        assert!(synthesized_openai_instance(&args(&["--model", "m"])).is_none());

        // A lone half synthesizes an incomplete instance — no default URL,
        // no default key.
        let only_url = synthesized_openai_instance(&args(&["--base-url", "http://x/v1"]))
            .expect("url alone synthesizes");
        assert_eq!(only_url.base_url.as_deref(), Some("http://x/v1"));
        assert_eq!(only_url.api_key, None);
        assert!(only_url.requires_api_key);
        // The synthesized instance is registered under its literal name.
        let name = only_url.name.clone();
        assert_eq!(
            pool_of(only_url).missing_credential_raw(&name),
            Some("api_key")
        );

        let only_key = synthesized_openai_instance(&args(&["--api-key", "k"]))
            .expect("key alone synthesizes");
        assert_eq!(only_key.base_url, None);
        let name = only_key.name.clone();
        assert_eq!(
            pool_of(only_key).missing_credential_raw(&name),
            Some("base_url")
        );

        // Both halves: complete, and no invented model id.
        let both = synthesized_openai_instance(&args(&[
            "--base-url",
            "http://x/v1",
            "--api-key",
            "k",
        ]))
        .expect("pair synthesizes");
        let name = both.name.clone();
        assert_eq!(pool_of(both).missing_credential_raw(&name), None);
        let both = synthesized_openai_instance(&args(&[
            "--base-url",
            "http://x/v1",
            "--api-key",
            "k",
        ]))
        .expect("pair synthesizes");
        assert!(both.models.is_empty(), "no default model id");
        assert!(both.should_fetch_models(), "model list comes from discovery");

        // An explicit model pins the initial selection but not the list:
        // discovery still runs to enrich the menu and the model metadata.
        let with_model = synthesized_openai_instance(&args(&[
            "--base-url",
            "http://x/v1",
            "--api-key",
            "k",
            "--model",
            "m1",
        ]))
        .expect("complete");
        assert_eq!(with_model.models.len(), 1);
        assert!(with_model.should_fetch_models());

        // Config-declared instances are never gated for a key (keyless
        // endpoints like ollama are legitimate).
        let keyless = config::ProviderInstance {
            name: "local".to_string(),
            protocol: None,
            base_url: Some("http://127.0.0.1:1/v1".to_string()),
            api_key: None,
            models: vec![config::ModelInfo::from_id("m")],
            fetch_models: Some(false),
            headers: Default::default(),
            body_overrides: Default::default(),
            wire: None,
            options: Default::default(),
            requires_api_key: false,
        };
        assert_eq!(pool_of(keyless).missing_credential_raw("local"), None);
    }

    #[test]
    fn no_subcommand_still_parses_the_tui_flags() {
        let parsed = args(&["--model", "m1", "--provider", "p"]);
        assert!(parsed.command.is_none());
        assert_eq!(parsed.model.as_deref(), Some("m1"));
        assert_eq!(parsed.provider.as_deref(), Some("p"));
    }

    #[test]
    fn plugin_install_parses_a_path_target() {
        let parsed = args(&["plugin", "install", "../cst-provider-qoder"]);
        let Some(Command::Plugin {
            action:
                PluginAction::Install {
                    target,
                    git,
                    no_build,
                    symbols,
                    ..
                },
        }) = parsed.command
        else {
            panic!("expected `plugin install`");
        };
        assert_eq!(target.as_deref(), Some("../cst-provider-qoder"));
        assert!(git.is_none());
        assert!(!no_build);
        assert!(symbols.is_empty());
    }

    #[test]
    fn plugin_install_parses_a_git_source_with_revision_and_symbols() {
        let parsed = args(&[
            "plugin",
            "install",
            "cst-provider-qoder",
            "--git",
            "https://example.invalid/p.git",
            "--tag",
            "v1",
            "--symbol",
            "PLUGIN",
            "--symbol",
            "EXTRA",
            "--crate-name",
            "p",
            "--codest-src",
            "/src/cst",
        ]);
        let Some(Command::Plugin {
            action:
                PluginAction::Install {
                    target,
                    git,
                    tag,
                    symbols,
                    crate_name,
                    codest_src,
                    ..
                },
        }) = parsed.command
        else {
            panic!("expected `plugin install`");
        };
        assert_eq!(target.as_deref(), Some("cst-provider-qoder"));
        assert_eq!(git.as_deref(), Some("https://example.invalid/p.git"));
        assert_eq!(tag.as_deref(), Some("v1"));
        assert_eq!(symbols, vec!["PLUGIN".to_string(), "EXTRA".to_string()]);
        assert_eq!(crate_name.as_deref(), Some("p"));
        assert_eq!(codest_src.as_deref(), Some(std::path::Path::new("/src/cst")));
    }

    #[test]
    fn plugin_revision_flags_are_mutually_exclusive() {
        let error = Args::try_parse_from([
            "codest", "plugin", "install", "p", "--git", "u", "--rev", "a", "--tag", "b",
        ])
        .expect_err("rev and tag conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn plugin_list_and_uninstall_parse() {
        let Some(Command::Plugin {
            action: PluginAction::List,
        }) = args(&["plugin", "list"]).command
        else {
            panic!("expected `plugin list`");
        };
        let Some(Command::Plugin {
            action:
                PluginAction::Uninstall {
                    name,
                    no_build: false,
                },
        }) = args(&["plugin", "uninstall", "cst-provider-qoder"]).command
        else {
            panic!("expected `plugin uninstall`");
        };
        assert_eq!(name, "cst-provider-qoder");
    }

    #[test]
    fn plugin_build_parses_release_and_install() {
        let Some(Command::Plugin {
            action:
                PluginAction::Build {
                    release,
                    install,
                    ..
                },
        }) = args(&["plugin", "build", "--release"]).command
        else {
            panic!("expected `plugin build`");
        };
        assert!(release);
        assert!(!install);
    }
}
