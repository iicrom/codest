# Plugins

English | [简体中文](zh/plugins.md)

A codest plugin is an ordinary Rust value linked into the binary that extends
the agent without touching the core or the terminal: `/help`, `/clear`,
`/quit`, `/status`, `/continue`, and the `bash`/`fs` tool sets are all plugins
— there is no privileged command layer. This guide covers the contract; see
[local-plugins.md](local-plugins.md) for installing third-party plugins.

## 1. Overview

One plugin can contribute any subset of the following, all through the
[`Plugin`](../crates/cst-plugin-api/src/plugin.rs) trait:

| Extension point | Trait method | Typical use |
| --- | --- | --- |
| Slash commands | `commands()` + `run_command()` | `/help`, `/clear`, `/status` |
| System-prompt fragment | `system_prompt()` | Domain instructions, tool conventions |
| Lifecycle hooks | `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` | Notices, reports, requested actions |
| Tools | `tools()` + `run_tool()` | Model-invoked capabilities (`bash`, `read`, …) |
| Protocol factories | `protocols()` | A new wire protocol for `providers.toml` instances |
| Sign-in entries | `auth_providers()` / `auth_status()` / `begin_sign_in()` | Browser/PAT sign-in behind `/provider` |

Plugins are **compile-time static**: a plugin is a `struct` with a `static`
value, registered in one `static` slice. Nothing is discovered or loaded at
runtime — the registry is an array you can read. The payoff is deterministic
registration order (= hook order = menu order), construction-time validation,
and plain Rust types end to end; the price is that adding a plugin recompiles
the binary (section 6; [local-plugins.md](local-plugins.md) covers third-party plugins).

## 2. The plugin contract

The trait and all data it exchanges live in the contract crate
[`cst-plugin-api`](../crates/cst-plugin-api/README.md) (`PLUGIN_API_VERSION`
is `1`), so external plugins implement it without depending on the host
binary. The host side lives in [`src/plugins/mod.rs`](../src/plugins/mod.rs).

### Trait methods

| Method | Receives | Returns | Default |
| --- | --- | --- | --- |
| `id()` | — | `&'static str` | — (required) |
| `commands()` | — | `&'static [CommandSpec]` | empty |
| `system_prompt()` | — | `Option<&'static str>` | `None` |
| `tools()` | — | `&'static [ToolSpec]` | empty |
| `run_tool(&ToolCall)` | the raw JSON arguments | `Result<String, PluginError>` | error |
| `protocols()` | — | `&'static [&'static dyn ProtocolFactory]` | empty |
| `auth_providers()` | — | `&'static [AuthProvider]` | empty |
| `auth_status(&str)` | instance name | `AuthStatus` | `Unsupported` |
| `begin_sign_in(...)` | instance, method, secret, notifier, cancel token | `Option<SignInFuture>` | `None` |
| `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` | `&mut PluginContext` | — | no-op |
| `run_command(&Invocation, &mut PluginContext)` | parsed command + args | `Result<(), PluginError>` | error |

Plugins are shared as `&'static dyn Plugin`, so everything takes `&self`;
stateful plugins use interior mutability (`Mutex`, atomics). Command specs,
prompt fragments, and tool specs are `'static` strings, borrowed by menus and
help text without cloning.

### Effects, not terminal calls

A plugin never touches the terminal, the session, or ratatui — the contract
crate carries no terminal code at all. Instead, a plugin *describes* what
should happen by requesting effects on its `PluginContext`; the front-end
collects them and applies each one. This is the only way a plugin can act:

| Effect | Requested via | Front-end behavior |
| --- | --- | --- |
| `Notice(String)` | `ctx.notice(text)` | Append a notice cell to the transcript |
| `Error(String)` | `ctx.error(text)` | Append an error cell |
| `Report(ReportBlock)` | `ctx.report(block)` | Append a titled report block (`/context` shape) |
| `ClearConversation` | `ctx.clear_conversation()` | Clear transcript + core conversation |
| `Quit` | `ctx.quit()` | Exit on the next frame |
| `SetComposer(String)` | `ctx.set_composer(text)` | Replace the composer text |
| `SubmitUserTurn(String)` | `ctx.submit_user_turn(text)` | Submit a user turn — only while idle |
| `ContinueTurn` | `ctx.continue_turn()` | Drive the current transcript without a new message (`/continue`) — only while idle |

Effects are collected in plugin order and applied in that order. The
idle-gated effects make recursion impossible: `on_user_message` runs after
`busy` is set and `on_turn_complete` before it is cleared, so a hook asking
for `SubmitUserTurn` is a no-op, not a loop. Commands run while the agent is
idle, so a "submit a prompt" command works.

### What a hook can see

`PluginContext` exposes read-only session facts plus the effect requests
above: `command_table()`, `model()`, `mode()`, `transcript_len()`,
`message_count()`, `context_window()`, `last_usage()`, `total_input()`,
`total_output()`, `plugin_ids()`, `system_sections()`, `effort()`,
`request()` (protocol, base URL, endpoint, `body_overrides`), `tools()`, and
`enabled_tools()` — never the agent session or the event loop. Errors are
soft: `run_command` / `run_tool` returning `Err` becomes an `Error` effect
(command) or `error: …` fed to the model plus a transcript failure line
(tool); registry problems are startup errors (section 4); a plugin panic is
not caught — static plugins are trusted code, on par with the rest of the
binary.

## 3. Built-in plugins

The registry is [`src/plugins/builtin/mod.rs`](../src/plugins/builtin/mod.rs):

```rust
pub static BUILTIN_PLUGINS: &[&dyn Plugin] = &[
    &help::HELP, &clear::CLEAR, &quit::QUIT, &status::STATUS,
    &r#continue::CONTINUE, &tools::BASH, &tools::FS,
];
```

Order is contract: it is the hook execution order, the command-menu order,
and the system-prompt assembly order. The tool plugins come last so their
prompt section lands after the command plugins' contributions.

| Plugin (id) | Contributes | What it does |
| --- | --- | --- |
| `help` | `/help` (`/h`, `/?`) | Renders the shared command table |
| `clear` | `/clear` | Requests `ClearConversation` |
| `quit` | `/quit` (`/exit`) | Requests `Quit` |
| `status` | `/status` | Backend, mode, transcript size, context usage, token totals, plugin list |
| `continue` | `/continue` | Requests `ContinueTurn` — resume an interrupted turn |
| `bash` | `bash` tool + prompt section | Runs one shell command |
| `fs` | `read`, `write`, `edit` tools | File operations |

Two tool plugins rather than one, so profile tool filters and the `/tools`
switch act per plugin; the per-`ToolSpec` `read_only` flag does the rest.

**`bash`** ([tools/bash.rs](../src/plugins/builtin/tools/bash.rs)) spawns
`$SHELL -c <command>` (falling back to `/bin/sh`) in the working directory —
a one-shot command, not a persistent shell. The `timeout` argument defaults
to 300s and is capped at 600s; expiry kills the whole process group (the
child was given its own group at spawn), so backgrounded children do not
outlive the call. Both pipes are drained on threads to prevent deadlock,
capped at 1 MiB per stream, tail-truncated before reaching the model.

**`fs`** ([tools/fs.rs](../src/plugins/builtin/tools/fs.rs)) implements
`read` (line-numbered window, `offset`/`limit` continuation, binary files
refused, BOM hidden), `write` (create or overwrite, parent directories
included), and `edit` (exact `old_string`/`new_string` replacement — the
match must be unique unless `replace_all` is set). `read` is marked
`read_only`, the mutating tools are not; that flag drives `/tools` markers
and read-only profiles.

Output caps are shared in [tools/truncate.rs](../src/plugins/builtin/tools/truncate.rs):
**2000 lines / 50 KB** per result, single lines clipped at 2000 chars. Shell
output keeps the *tail* (recent output is what matters after a long build);
file reads keep the *head* and report the next `offset`.

Tool dispatch end to end: the request carries the declared tools, the model
streams `tool_calls`, the worker dispatches each call by name to the plugin
that declared it (`PluginHost` implements the core `ToolExecutor` trait), and
the result goes back as a `role: "tool"` message. Rounds are unbounded by
default — the loop ends when the model stops calling tools, the user
interrupts, or the request fails; `max_tool_rounds` in `config.toml`
re-enables a guard. Tool execution runs on the blocking pool, so `run_tool`
may block; hooks cannot.

## 4. Startup validation

`PluginHost::new` / `with_local` validate the whole registry before anything
runs, and a failure is a **startup error**, not a runtime warning:

| Check | Error |
| --- | --- |
| Plugin id non-empty and unique | `EmptyPluginId` / `DuplicatePluginId` |
| Command names/aliases valid and globally unique | `CommandTable(InvalidName)` / `(DuplicateName)` |
| Tool names unique across plugins | `DuplicateTool` |
| Profile tool filter names known plugins | `UnknownFilteredPlugin` |

Command names allow lowercase ASCII letters, digits, `-`, `_`, and `?` (the
last for `/help`'s `/?` alias). Duplicates across the built-in and local
groups are rejected together, so a local plugin can never shadow a shipped one.

The command table also carries the **front-end shipped commands** — `/new`,
`/rename`, `/sessions`, `/model`, `/provider`, `/effort`, `/thinking`,
`/profile`, `/tools`, `/context`. No plugin owns them; they live in the same
table so parsing, the menu, and `/help` agree, and the front-end handles them
before dispatch ever reaches a plugin. `PluginHost::shipped()` builds a host
from the built-in plugins only — a stable table for tests and diagnostics
that does not change with whatever a user installed.

## 5. Writing your first plugin

A minimal command plugin, modeled on
[help.rs](../src/plugins/builtin/help.rs) and
[clear.rs](../src/plugins/builtin/clear.rs):

```rust
// src/plugins/builtin/review.rs
use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static REVIEW: Review = Review;

pub struct Review;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "review",
    &[],
    "/review <focus>",
    "ask the model to review the current change",
    ArgsPolicy::Required,
)];

impl Plugin for Review {
    fn id(&self) -> &'static str {
        "review"
    }
    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }
    fn run_command(
        &self,
        invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        ctx.submit_user_turn(format!(
            "Review the current change. Focus: {}",
            invocation.args
        ));
        Ok(())
    }
}
```

Register it — the only other file you touch is the registry,
`src/plugins/builtin/mod.rs`:

```rust
mod review;
pub static BUILTIN_PLUGINS: &[&dyn Plugin] = &[
    // ... existing entries ...
    &review::REVIEW,
];
```

That is the whole change: no core, TUI, or CLI edits — parsing, alias
matching, argument policy (`Required` rejects a bare `/review`), completion,
the menu, and `/help` all pick the command up from the shared `CommandTable`.
Then:

```bash
cargo test          # offline; add a host-level test asserting the effects
cargo clippy --all-targets -- -D warnings
```

A hooks-only plugin skips `commands()` entirely and implements, say,
`on_turn_complete`; a tool plugin implements `tools()` + `run_tool` with
`ToolSpec { name, description, parameters_json, read_only }` — the schema is
raw JSON-Schema text so it can live in a `const`. State goes in a `Mutex`,
via interior mutability like the shipped plugins.

## 6. Design notes

**Why compile-time instead of dynamic `.so` loading?** Dynamic loading would
need a hand-rolled C ABI, version negotiation, and panic/FFI bridges — real
risk for little gain at this scale. A static registry gives determinism (no
discovery order, no load failures, no runtime version mismatch), readability,
and near-zero overhead. Distributed registration via `inventory`/`linkme` was
rejected too: linker-section behavior and platform-dependent ordering are
worse than an explicit array, and the plugin contract would not change if
that is ever revisited.

**Why effects?** Because they are data, any front-end — the TUI, a headless
runner, tests — applies them however it wants, which is why plugins need no
ratatui dependency and the system is testable without a terminal.

**Boundaries, on purpose:** hooks run synchronously on the front-end thread
and must return quickly (tool calls are the sanctioned way to do slow work);
there is no sandboxing or crash isolation — plugins run with host privileges;
state is in-process and dies with the process; plugins cannot draw custom
widgets or register key bindings. External plugins keep the same contract and
get compiled in by a generated wrapper crate ([local-plugins.md](local-plugins.md)).

## 7. Related docs

- [local-plugins.md](local-plugins.md) — installing and managing external plugins
- [multi-provider.md](multi-provider.md) — provider instances, wire quirks, custom protocols
- [profiles.md](profiles.md) — profile presets and the `tools` filter
- [../README.md](../README.md) — getting started
- Design notes (Chinese): [provider.md](provider.md), [session-id.md](session-id.md), [session-continue.md](session-continue.md)
- Contract crate reference: [../crates/cst-plugin-api/README.md](../crates/cst-plugin-api/README.md)
