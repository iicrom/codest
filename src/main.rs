//! `codest` binary: primary command entry point.
//!
//! The `cst` alias lives in `src/bin/cst.rs`; both share [`codest::cli`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codest::cli::run("codest").await
}
