//! `cst` binary: CLI parsing and backend selection.

use std::sync::Arc;

use clap::Parser;
use clap::ValueEnum;
use cst::core::ModelClient;
use cst::core::mock::MockModelClient;
use cst::core::openai_compat::OpenAiCompatClient;
use cst::tui::TuiMode;

#[derive(Parser, Debug)]
#[command(
    name = "cst",
    version,
    about = "A minimal TUI chat agent distilled from codex"
)]
struct Args {
    /// Use the built-in offline mock model (no network).
    #[arg(long)]
    mock: bool,

    /// Model backend. Defaults to mock when no API key is available.
    #[arg(long, value_enum)]
    provider: Option<Provider>,

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Provider {
    Mock,
    Openai,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (client, notice) = build_client(&args)?;
    if let Some(notice) = notice {
        eprintln!("{notice}");
    }
    cst::tui::run(client, args.system, args.tui_mode).await
}

fn build_client(args: &Args) -> anyhow::Result<(Arc<dyn ModelClient>, Option<String>)> {
    let api_key = args
        .api_key
        .clone()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .filter(|key| !key.trim().is_empty());

    let provider = args.provider.unwrap_or_else(|| {
        if args.mock {
            Provider::Mock
        } else if api_key.is_some() {
            Provider::Openai
        } else {
            Provider::Mock
        }
    });

    match provider {
        Provider::Mock => {
            let notice = if args.mock || args.provider.is_some() {
                None
            } else {
                Some(
                    "cst: no API key found — using the offline mock model. \
                     Set CST_API_KEY (or pass --api-key) to use a real endpoint."
                        .to_string(),
                )
            };
            Ok((Arc::new(MockModelClient::default()), notice))
        }
        Provider::Openai => {
            let Some(api_key) = api_key else {
                anyhow::bail!("no API key: set CST_API_KEY / OPENAI_API_KEY or pass --api-key");
            };
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
            Ok((Arc::new(client), None))
        }
    }
}
