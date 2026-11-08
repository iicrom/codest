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

use std::sync::Arc;

use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;

use crate::core::config;
use crate::core::providers::ModelPool;
use crate::core::providers::ProviderRegistry;
use crate::core::providers::RegistryError;
use crate::core::providers::Selection;
use crate::plugins::PluginHost;
use crate::tui::RestoreMode;
use crate::tui::TuiMode;

#[derive(Parser, Debug)]
#[command(version, about = "A minimal TUI chat agent distilled from codex")]
struct Args {
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

    /// Terminal UI mode: fullscreen (alternate screen) or inline (scrollback).
    #[arg(long, value_enum, env = "CST_TUI_MODE", default_value = "fullscreen")]
    tui_mode: TuiMode,
}

/// Parse arguments and run the TUI.
///
/// `program` is the invoked binary name (`codest` or its `cst` alias); it is
/// used in clap's help and version output.
pub async fn run(program: &'static str) -> anyhow::Result<()> {
    let command = Args::command().name(program);
    let matches = command.get_matches();
    let args = Args::from_arg_matches(&matches)?;

    let plugins = PluginHost::builtin()?;
    let global = config::load_global(&config::config_path())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let pool = build_pool(&plugins, &global, &args)?;
    let selection = initial_selection(pool.registry(), &global, &args)?;

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
    )
    .await
}

/// Load the configuration files and build the provider pool.
///
/// There is no built-in default provider: an endpoint is synthesized from
/// flags/env only when `providers.toml` is empty **and** something actually
/// names one (`--api-key`/`--base-url`, `CST_API_KEY`/`OPENAI_API_KEY`,
/// `OPENAI_BASE_URL` — local test servers need no key, so a lone base URL
/// counts). With nothing configured at all the pool starts empty and the
/// front-end guides the user to add a provider.
fn build_pool(
    plugins: &PluginHost,
    global: &config::GlobalConfig,
    args: &Args,
) -> anyhow::Result<Arc<ModelPool>> {
    let mut instances = config::load_providers(&config::providers_path())
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    let mut synthesized = false;
    if instances.is_empty() {
        match synthesized_openai_instance(args) {
            Some(instance) => {
                instances.push(instance);
                synthesized = true;
            }
            None => {
                return Ok(Arc::new(
                    ModelPool::new(ProviderRegistry::build(
                        &plugins.protocol_factories(),
                        Vec::new(),
                    )?)
                    .with_cache(config::model_cache_path()),
                ));
            }
        }
    } else {
        // Flags override the instance this run selects (see
        // `resolve_provider_name` for which one that is).
        let target = resolve_provider_name(global, args)
            .unwrap_or_else(|| instances[0].name.clone());
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
        if args.base_url.is_some() {
            instance.base_url = args.base_url.clone();
        }
        if args.api_key.is_some() {
            instance.api_key = args.api_key.clone();
        }
    }

    let registry = ProviderRegistry::build(&plugins.protocol_factories(), instances)
        .map_err(|error: RegistryError| anyhow::anyhow!("{error}"))?;
    if synthesized {
        eprintln!(
            "codest: no providers.toml; using the endpoint from flags/env \
             (create {} to add more)",
            config::providers_path().display()
        );
    }
    Ok(Arc::new(
        ModelPool::new(registry).with_cache(config::model_cache_path()),
    ))
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
        name: "untitled".to_string(),
        protocol: None,
        base_url,
        api_key,
        // With an explicit model the endpoint is fully specified; without
        // one, models discovery fills the list from `base_url`.
        models: model.map(config::ModelInfo::from_id).into_iter().collect(),
        fetch_models: None,
        headers: Default::default(),
        body_overrides: Default::default(),
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
/// Precedence: `--provider`/`--model` > config.toml `default` > the first
/// model of the first instance. A model id nobody declared yet (it may still
/// arrive through models discovery) proceeds with a warning; an unknown
/// *provider* is a hard error.
fn initial_selection(
    registry: &ProviderRegistry,
    global: &config::GlobalConfig,
    args: &Args,
) -> anyhow::Result<Option<Selection>> {
    // No providers at all: an empty route wins over a stray `default`, so
    // the TUI can still start and guide the user.
    if registry.instances().is_empty() {
        if args.provider.is_some() || args.model.is_some() {
            eprintln!("codest: --provider/--model ignored: no providers are configured");
        }
        return Ok(None);
    }

    let selection = if let Some(provider) = args.provider.as_deref() {
        let model = args.model.clone().unwrap_or_else(|| {
            registry
                .instance(provider)
                .and_then(|instance| instance.models.first())
                .map(|model| model.id.clone())
                .unwrap_or_default()
        });
        Some(Selection::new(provider, model))
    } else if let Some(default) = global.default.as_deref() {
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

        // An explicit model pins the list instead of discovery.
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
        assert!(!with_model.should_fetch_models());

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
            requires_api_key: false,
        };
        assert_eq!(pool_of(keyless).missing_credential_raw("local"), None);
    }
}
