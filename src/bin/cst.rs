//! `cst` binary: installed alias for the `codest` command.
//!
//! `cargo install --path .` installs both binaries, and both share
//! [`codest::cli`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codest::cli::run("cst").await
}
