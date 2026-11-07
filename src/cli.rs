//! Command-line parsing and endpoint configuration shared by the `codest`
//! binary and its `cst` alias.
//!
//! The shipped agent only speaks to OpenAI-compatible endpoints. For local
//! end-to-end testing, run `scripts/openai_test_server.py` and point
//! `--base-url` at it.

use std::sync::Arc;

use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;

use crate::core::ModelClient;
use crate::core::openai_compat::OpenAiCompatClient;
use crate::tui::TuiMode;

#[derive(Parser, Debug)]
#[command(version, about = "A minimal TUI chat agent distilled from codex")]
struct Args {
    /// Model name sent to the backend.
    #[arg(long, env = "CST_MODEL")]
    model: Option<String>,

    /// OpenAI-compatible base URL, or a full /chat/completions URL.
    #[arg(long, env = "CST_BASE_URL")]
    base_url: Option<String>,

    /// API key for the OpenAI-compatible endpoint.
    #[arg(long, env = "CST_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Override the system prompt.
    #[arg(long)]
    system: Option<String>,

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
    let client = build_client(&args)?;
    crate::tui::run(client, args.system, args.tui_mode).await
}

fn build_client(args: &Args) -> anyhow::Result<Arc<dyn ModelClient>> {
    let api_key = args
        .api_key
        .clone()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no API key: set CST_API_KEY / OPENAI_API_KEY or pass --api-key.\n\
                 For local testing, start scripts/openai_test_server.py and use \
                 `--base-url http://127.0.0.1:8080/v1 --api-key test`."
            )
        })?;

    let base_url = args
        .base_url
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let model = args
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let client = OpenAiCompatClient::new(api_key, base_url, model)?;
    Ok(Arc::new(client))
}
