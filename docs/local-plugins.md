# External plugins

English | [简体中文](zh/local-plugins.md)

An external plugin is a plugin maintained outside this repository and linked
into the binary at build time. `codest plugin install` is the only entry
point: one command records the plugin, generates a wrapper crate *outside*
the checkout, and builds a codest binary with the plugin linked in — the
repository itself stays untouched. The plugin contract itself is covered in
[plugins.md](plugins.md).

## 1. How linking works without a dependency cycle

Something in the source must reference `&SOME_PLUGIN` for it to end up in
the binary — plugins are compile-time linked, and that does not change for
external ones. What changes is *where* that something lives: a generated
wrapper crate under `~/.codest/`, not your checkout.

The contract types (`Plugin`, `ProtocolFactory`, `ModelClient`, `ToolSpec`,
`AuthProvider`, …) live in the standalone crate
[`cst-plugin-api`](../crates/cst-plugin-api/README.md), which is what makes a
separate plugin repository possible:

```text
codest ──depends on──▶ your-plugin ──depends on──▶ cst-plugin-api
   └──────────────────── depends on ──────────────────┘
```

The host depends on the plugin (to link it), the plugin depends on the
contract crate (to implement the trait), and codest is never a dependency of
a plugin — so Cargo sees no cycle. The host re-exports every contract module
under its historical paths (`codest::core::model`, `codest::plugins::Plugin`,
…), so the split renamed nothing.

**Versioning.** `PLUGIN_API_VERSION` is `1`. There is no runtime handshake:
the host and a plugin agree through **Cargo versions**, and an incompatible
contract fails to **compile**, not to start — which is the better failure.
Adding a trait method with a default implementation does not bump the
version; changing an existing signature or its meaning does. Depend on a
range (`cst-plugin-api = "0.1"`), not an exact pin.

## 2. Installing

```bash
codest plugin install ../my-plugin                                    # local checkout
codest plugin install my-plugin --git https://host/my-plugin.git --tag v1
codest plugin install my-plugin --version '^0.1'                      # crates.io
```

| Flag | Meaning |
| --- | --- |
| target (positional) | Checkout path, crates.io package name, or a git URL |
| `--git <url>` | Install the named package from this repository |
| `--package <name>` | Package name when it cannot be inferred from the target |
| `--rev` / `--branch` / `--tag` | Pin the git source (mutually exclusive) |
| `--version <req>` | Version requirement for a crates.io install (default `*`) |
| `--symbol <name>` | Exported `static` to link; repeatable. Default `PLUGIN`, or the plugin's `[package.metadata.codest] plugins` |
| `--crate-name <name>` | Rust identifier override (default: package name with `-` → `_`; keywords rejected) |
| `--codest-src <path>` | The codest checkout to link against (see below) |
| `--no-build` | Rewrite the list and wrapper only; skip cargo |

A bare git URL passed as the target also works; `--package` supplies the
name when the URL does not end in a package name. Installing the same
package name again replaces the entry.

### What happens under the hood

1. The plugin entry is recorded in `~/.codest/plugins.toml` (replaced
   atomically; on any later failure the previous state is restored).
2. A wrapper crate is regenerated at `~/.codest/plugins/crate/` — outside
   the repository, so `git status` stays clean and tricks like
   `git update-index --skip-worktree` are never needed.
3. The wrapper is built there with `cargo build`, and the binary paths are
   printed.

The wrapper is a plain Cargo package (`codest-local`) with two binaries and
one dependency line per plugin:

```rust
// ~/.codest/plugins/crate/src/main.rs (generated — do not edit)
use codest::plugins::Plugin;

static LOCAL_PLUGINS: &[&dyn Plugin] = &[
    &my_plugin::PLUGIN,
];

fn main() {
    codest::cli::run_blocking("codest", LOCAL_PLUGINS);
}
```

`src/bin/cst.rs` is the same list under the `cst` alias. The source of truth
is `~/.codest/plugins.toml`; the generated files are rewritten wholesale on
every `install`/`uninstall`/`build`, so edit the state file and rebuild
instead of hand-editing the wrapper. Concurrent `codest plugin` invocations
are rejected outright (a lock file, not a queue).

### `--codest-src`: which host the wrapper links

The wrapper `path`-depends on a codest checkout. Candidates are tried in
order, each verified (`Cargo.toml` declares package `codest` and
`crates/cst-plugin-api` exists); a failing candidate degrades to the next:

1. explicit `--codest-src` (a bad explicit path is a hard error, not a
   degradation);
2. `$CODEST_SRC`;
3. the source tree this binary was compiled from
   (`CARGO_MANIFEST_DIR`, baked in at compile time);
4. the checkout recorded in `~/.codest/plugins.toml`.

So a binary installed with `cargo install` can still install plugins — it
remembers the source path from its build. If that tree has moved, pass
`--codest-src` explicitly.

### Contract identity

The one hard constraint: the host and every plugin must resolve
`cst-plugin-api` to the **same** crate, or `&MyPlugin` is not a
`&dyn Plugin` (a compile error, never a runtime surprise).

- Plugin depends on the contract by `path` → the installer checks that the
  path is exactly the host's copy and rejects a mismatch.
- Plugin depends by version/git (required for git and crates.io sources) →
  the generated manifest carries
  `[patch.crates-io] cst-plugin-api = { path = "<codest-src>/crates/cst-plugin-api" }`
  so both sides unify on the host's copy. It is emitted only when needed.

A checkout that only builds next to this repository (relative `path` dep on
the contract) must therefore be installed from its local path; cloned
elsewhere it fails with `no matching package named cst-plugin-api found`,
and the installer appends that explanation to the error without touching
`plugins.toml`.

## 3. Managing

| Command | Effect |
| --- | --- |
| `codest plugin list` | Installed plugins, sources, symbols, and the linked codest source |
| `codest plugin uninstall <name>` | Remove one entry and rebuild (other plugins survive); `--no-build` skips the build |
| `codest plugin build` | Regenerate the wrapper from the saved list and build it |
| `codest plugin build --release` | Same, with optimizations |
| `codest plugin build --install` | `cargo install --path … --force` the result instead of only building it |
| `codest plugin build --codest-src <path>` | Relink against another checkout |

`--release` and `--install` are mutually exclusive: `cargo install` always
builds optimized, so asking for both would silently ignore one flag — codest
rejects the combination instead.

The build never removes plugins from the binary implicitly: a plugin that is
absent is simply not in the binary, because the list drives the wrapper.

## 4. Writing an installable plugin

A plugin is an ordinary library crate that depends on the contract and
exports `static` values implementing `Plugin`:

```toml
# Cargo.toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2024"

[dependencies]
cst-plugin-api = "0.1"          # a range, so the host's copy can unify
```

```rust
use cst_plugin_api::plugin::{Plugin, PluginContext, PluginError};

pub static PLUGIN: MyPlugin = MyPlugin;

pub struct MyPlugin;

impl Plugin for MyPlugin {
    fn id(&self) -> &'static str {
        "my-plugin"
    }

    fn system_prompt(&self) -> Option<&'static str> {
        Some("Always answer in bullet points.")
    }
}
```

If the plugin exports more than one `static`, declare them once in the
manifest so installers do not need `--symbol`:

```toml
[package.metadata.codest]
plugins = ["PLUGIN", "TOOL_PLUGIN"]
```

Everything a plugin can contribute — commands, hooks, tools, protocol
factories, sign-in entries — is the same contract the built-ins implement;
see [plugins.md](plugins.md) for the trait reference and
[multi-provider.md](multi-provider.md) for the protocol/auth path. Test the
crate standalone with `cargo test`; it needs nothing from the host.

## 5. Limitations and design notes

- **Rebuild to change.** Plugin edits require `codest plugin build` — that
  is the compile-time linking trade-off, not a bug.
- **No sandbox, no isolation.** Plugins run with host privileges and a panic
  takes down the process. Static plugins are trusted code; isolation belongs
  in a separate process, not here.
- **No signature verification yet.** Nothing authenticates where a plugin
  came from; git/crates.io sources inherit the supply-chain posture of the
  registry they point at.
- **Generated files are machine-managed.** Rewrites drop manual comments;
  `plugins.toml` and the wrapper are overwritten wholesale.
- **One writer at a time.** Concurrent `codest plugin` commands fail with a
  lock error rather than queueing.
- **crates.io sources need the contract published.** Until `cst-plugin-api`
  is on crates.io, a registry install resolves the contract through the
  generated `[patch.crates-io]` to the local checkout — it builds, but the
  effective contract is the local copy's.

## 6. Reference implementation

`cst-provider-qoder` is the reference external plugin: a Qoder wire-protocol
provider contributing a `ProtocolFactory` plus browser and
personal-access-token sign-in through the auth contract. It lives in a
separate repository, published alongside codest. The smoke script
[../scripts/plugin_smoke.py](../scripts/plugin_smoke.py) exercises the real
install path against it: install, run `/status` and `/help` in the built
binary under a pseudo-terminal, verify the plugin id and commands appear,
uninstall, verify the rebuilt binary no longer contains them, and check that
`git status` is unchanged throughout.

## 7. Related docs

- [plugins.md](plugins.md) — the plugin contract and built-in plugins
- [multi-provider.md](multi-provider.md) — provider instances, wire quirks, custom protocols
- [profiles.md](profiles.md) — profile presets and the `tools` filter
- [../README.md](../README.md) — getting started
- Design notes (Chinese): [provider.md](provider.md), [session-id.md](session-id.md),
  [session-continue.md](session-continue.md)
- Contract crate reference: [../crates/cst-plugin-api/README.md](../crates/cst-plugin-api/README.md)
