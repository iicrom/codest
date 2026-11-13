# Plugins

English | [简体中文](zh/plugins.md)

A codest plugin is an ordinary Rust value linked into the binary that extends
the agent without touching the core or the terminal: `/help`, `/clear`,
`/quit`, `/status`, `/continue`, and the shell (`bash`/`pwsh`) and `fs` tool sets are all plugins
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
| Tools | `tools()` + `run_tool()` | Model-invoked capabilities (`bash`/`pwsh`, `read`, …) |
| Protocol factories | `protocols()` | A new wire protocol for `providers.toml` instances |
| Sign-in entries | `auth_providers()` / `auth_status()` / `begin_sign_in()` | Browser/PAT sign-in behind `/provider` |
| Event stream | `subscriptions()` + `on_event()` | Meters, monitors, reactions to the agent's run |
| Status-bar segments | `ctx.set_status()` / `ctx.clear_status()` | Footer readouts (`tps:rate`, a clock, a countdown) |
| Background tasks | `ctx.spawn(future)` + `ctx.bridge()` | Long-lived work: a monitor server, a refresh loop |

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
| `subscriptions()` | — | `EventSubscription` | `NONE` |
| `on_event(&PluginEvent, &mut PluginContext)` | one core event + context | — | no-op |
| `run_command(&Invocation, &mut PluginContext)` | parsed command + args | `Result<(), PluginError>` | error |
| `settings()` | — | `Option<SettingsSection>` | `None` |
| `on_settings_changed(Option<&PluginSettings>, &mut PluginContext)` | the pocket as now written | — | no-op |

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
| `SetStatus(StatusSegment)` | `ctx.set_status(segment)` | Upsert one footer segment (`plugin:key`) |
| `ClearStatus(key)` | `ctx.clear_status(key)` | Remove one footer segment |

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
    &r#continue::CONTINUE, &tps::TPS, &tools::SHELL, &tools::FS,
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
| `tps` | `/tps`, a `/settings` toggle | Live tokens-per-second meter for the footer (off by default) |
| `shell` (`bash` / `pwsh`) | the same-named tool + prompt section | Runs one shell command |
| `fs` | `read`, `write`, `edit` tools | File operations |

Two tool plugins rather than one, so profile tool filters and the `/tools`
switch act per plugin; the per-`ToolSpec` `read_only` flag does the rest.

**`shell`** ([tools/shell.rs](../src/plugins/builtin/tools/shell.rs)) is the
engine; the platform module supplies only the command line and the wording.
[`tools/bash.rs`](../src/plugins/builtin/tools/bash.rs) (unix) and
[`tools/pwsh.rs`](../src/plugins/builtin/tools/pwsh.rs) (Windows) are thin
shims, and [`tools/mod.rs`](../src/plugins/builtin/tools/mod.rs) picks one
through a `#[cfg]` alias — so the tool name, the plugin id, the schema, the
prompt section, and the runner all follow the host. On unix it spawns
`$SHELL -c <command>` (falling back to `/bin/sh`); on Windows it spawns
`pwsh -NoLogo -NoProfile -NonInteractive -Command`. Either way it is a
one-shot command, not a persistent shell, run in the working directory.

The engine owns the shared behavior: the `timeout` argument defaults to 300s
and is capped at 600s; expiry kills the whole process tree (unix signals the
child's own process group; Windows runs `taskkill /F /T`), so backgrounded
children do not outlive the call. Both pipes are drained on threads to
prevent deadlock, capped at 1 MiB per stream, tail-truncated before reaching
the model.

The argument shape is identical on both hosts, so a call written for one
works on the other; only the name changes. A replayed `tools_switched` that
names the other platform's tool is dropped by the existing validation.

Windows specifics: the command is wrapped in a script that pins
`[Console]::OutputEncoding` to UTF-8 and normalizes the exit code
(`$LASTEXITCODE` for a native command, else `$?` for a failing cmdlet, else
`1` for a caught exception — a bare `pwsh -Command` reports a cmdlet failure
as `0`). A missing `pwsh` fails with the install hint rather than falling
back to `powershell.exe`.

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

## 6. Observing the agent: events, status segments, background tasks

The four lifecycle hooks cover turn boundaries. Three primitives cover
everything else a reactive plugin needs, and each is general on purpose:
they exist so a *family* of plugins is possible without new host API per
idea.

### The event stream

Declare the categories you want and implement one hook:

```rust
use crate::plugins::{
    DeltaKind, EventSubscription, Plugin, PluginContext, PluginEvent, PluginEventKind,
};

impl Plugin for MyMeter {
    fn id(&self) -> &'static str {
        "my-meter"
    }

    fn subscriptions(&self) -> EventSubscription {
        EventSubscription::DELTAS
            .union(EventSubscription::USAGE)
            .union(EventSubscription::LIFECYCLE)
    }

    fn on_event(&self, event: &PluginEvent<'_>, ctx: &mut PluginContext<'_>) {
        match event.kind {
            // `event.at` is the host's monotonic emit stamp.
            PluginEventKind::MessageDelta { kind: DeltaKind::Text, text, .. } => {
                let _ = (text, ctx);
            }
            // Real token counts as the backend reported them.
            PluginEventKind::TurnUsage { output, .. } => {
                let _ = output;
            }
            _ => {}
        }
    }
}
```

Categories: `LIFECYCLE` (turn started/settled/aborted/complete, errors,
cleared, shutdown), `DELTAS` (text and thinking deltas), `USAGE` (per-round
and per-turn token counts), `TOOLS` (tool-call lifecycle with durations),
`SESSION` (session/model/effort/profile/tool switches), `SNAPSHOT` (full
conversation snapshots). Unsubscribed categories cost nothing: the host
filters before dispatching, so a plugin that wants only turn boundaries
never sees a copy of a stream delta.

Two facts make the stream meterable:

* **`event.at` is the host's monotonic emit stamp**, taken once in
  `Event::new` — a throughput meter derives its clock from it instead of
  guessing from arrival order.
* **`SNAPSHOT` events carry the whole conversation** — the host asks the
  worker for one at plugin startup and after every session switch, so a
  monitor joins mid-stream without replaying history. "When in doubt,
  re-snapshot" is the intended recovery strategy, same as the reference
  monitors.

The payload borrows from the event; a plugin that must keep a delta copies
it. The four lifecycle hooks keep working unchanged — `on_event` is the
superset, and the built-in `tps` plugin ([tps.rs](../src/plugins/builtin/tps.rs))
is the reference implementation: a live estimate from deltas, corrected by
the usage event, with one sample per turn feeding an idle sparkline.

Its reporting contract is worth copying, because it is a general rule for
footer readouts rather than a tps detail:

* **A live value retires with its turn.** `tps:rate` is cleared on
  `TurnComplete`; a frozen number would claim the agent is still streaming.
* **A sparkline is a shape, not a number.** It appears from the second
  sample on (one sample is not a trend) and carries no digits; the numeric
  readout is the live rate's job.
* **Session boundaries clear the meter.** `SessionStarted`,
  `SessionSwitched` and `ConversationCleared` all drop the history — a
  throughput figure must describe the conversation on screen. The enabled
  switch survives, so the meter keeps running across the switch.
* **A turn's tokens are summed over its rounds.** `TurnUsage` carries only
  the last round while the decode span covers all of them; the meter banks
  each round's `RoundUsage` and falls back to `TurnUsage` when a backend
  reports no per-round counts.

### Status-bar segments

A plugin submits *data*, the host owns the pixels:
`ctx.set_status(StatusSegment::new("rate", "42 t/s"))` upserts the segment
stored as `tps:rate` (the `plugin:key` id is resolved from the plugin id),
and `ctx.clear_status("rate")` removes it. A tone — `Dim`, `Accent`,
`Warn` — maps onto the host palette. There is no arbitrary drawing, on
purpose (section 7). Segments render between the context readout and the
state word, in the priority the user sets:

```toml
[status_bar]
plugin_segments = false             # master switch (default on)
order = ["tps:rate", "tps:spark"]   # display priority, left to right
```

Segments not listed keep registration order; keys nothing currently
displays are ignored, so a stale entry costs nothing. On a narrow terminal
the right-hand hint — already the lowest-priority line on screen — degrades
first, then plugin segments drop from their own tail, and the host's own
readouts and the state word never move. Because the footer
repaints on the 90 ms tick, a segment animates the moment its plugin sets
new text — countdowns and gauges need no host change.

### Background tasks and per-plugin settings

`ctx.spawn(future)` hands a task to the host's runtime: it outlives the
dispatch, its effects reach the UI through the plugin's bridge, and the
host cancels it on exit. The future keeps nothing but a `PluginBridge` —
a clonable effect sink plus the cancellation token:

```rust
fn on_startup(&self, ctx: &mut PluginContext<'_>) {
    let bridge = ctx.bridge();
    ctx.spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                    bridge.set_status(StatusSegment::new("clock", local_time()));
                }
                _ = bridge.cancelled() => break, // cooperative shutdown
            }
        }
    });
}
```

A long-lived task must select on `bridge.cancelled()` — cancellation is
cooperative, and the host's final `abort` sweep after it is not graceful.
A task that panics surfaces as a transcript notice; it cannot take the UI
loop down. This is the sanctioned home for the slow work a synchronous
hook cannot do: a monitor's WebSocket server, a refresh loop, a watchdog.

Settings come from `config.toml`'s per-plugin pocket — the host
interprets no key, the same freedom `providers.toml`'s `options` gives
protocols:

```toml
[plugins.my-plugin]
enabled = true
listen = "127.0.0.1:8737"
```

`ctx.plugin_settings()` returns the pocket as a `PluginSettings` with
`option_str` / `option_u64` / `option_bool` readers plus the raw JSON map.
A table naming an unknown plugin id is a **startup error** — a silent
no-op hides typos — so remove the table when you uninstall a plugin.

### Settings in `/settings`

A plugin with options declares them instead of documenting them: returning a
`SettingsSection` from `Plugin::settings()` puts a card on the `/settings`
screen, one row per field, and turns every edit into a write to this plugin's
pocket. The host owns rendering, validation, and persistence; the plugin owns
the meaning of each key.

```rust
static FIELDS: [SettingField; 2] = [
    SettingField::toggle("enabled", "Meter", "live tokens-per-second readout", false),
    SettingField::select("mode", "Mode", "when to fold the readout", &MODES, "auto"),
];

fn settings(&self) -> Option<SettingsSection> {
    // The title is the card heading; `/settings my-plugin` still finds the
    // card by its plugin id, so the title is free to be a readable name.
    Some(SettingsSection::new("Meter", "live throughput", &FIELDS))
}

fn on_settings_changed(
    &self,
    settings: Option<&PluginSettings>,
    ctx: &mut PluginContext<'_>,
) {
    // Called after a successful write. It is skipped for a field declared
    // `restart()`, which the host already knows not to apply mid-run.
    let enabled = settings.and_then(|s| s.option_bool("enabled")).unwrap_or(false);
    self.set_enabled(enabled);
}
```

Four rules make a card predictable:

* **Keys are pocket keys.** `SettingField::key` addresses one entry of
  `[plugins."<id>"]` — the same entry `ctx.plugin_settings()` reads, so a value
  written by the screen is what the next startup parses. No second format.
* **Values are text plus a kind.** `Boolean`, `Select` (with `SettingOption`s),
  `Number`, and `Text` decide how the row edits and how the text is stored as
  TOML (`true`, `64`, `"high"`). An unparsable number is refused before it
  touches the file.
* **`default` is what an absent key means.** The row shows it marked
  `(default)`, and clearing a text row removes the key — restoring this value
  rather than pinning a copy of it.
* **`restart()` marks a startup-only field.** The edit still persists; the row
  carries a `⟲` marker and the help bar says `restart to apply`, but the host
  **skips `on_settings_changed`** for it — the running session keeps the value
  it booted with. Declare it whenever the value feeds something built once at
  startup (a server address, a protocol choice).

The built-in `tps` plugin ([tps.rs](../src/plugins/builtin/tps.rs)) is the
reference: one toggle, saved to `[plugins.tps] enabled`, applied in place so
the meter turns on without a restart. Its `/tps` command stays a run-only
shortcut for the same switch.

**A monitoring plugin, end to end:** subscribe to `EventSubscription::ALL`,
treat `SNAPSHOT` events as the authoritative transcript, forward batched
deltas best-effort from a spawned WebSocket server, and use
`ctx.submit_user_turn()` for remote prompts. What the reference monitors
(pi-monitor) do over their wire protocol composes from exactly these
primitives — nothing new in the host is required.

## 7. Design notes

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

**Boundaries, on purpose:** hooks and `on_event` run synchronously on the
front-end thread and must return quickly — slow work belongs in `run_tool`
or a `ctx.spawn` task, never in a hook. There is no sandboxing or crash
isolation — plugins run with host privileges (a spawned task's panic is
contained to that task, that is scheduling, not security). State is
in-process and dies with the process. Plugins cannot draw custom widgets,
register key bindings, or touch the terminal: the status bar is text plus a
tone, the transcript is cells, and that is deliberate. External plugins keep
the same contract and get compiled in by a generated wrapper crate
([local-plugins.md](local-plugins.md)).

**Why declarative status segments instead of draw callbacks?** ratatui
types in the contract crate would turn every footer upgrade into a recompile
for every plugin and hand layout arbitration to whichever plugin draws last.
Text plus a tone covers every readout a meter or monitor needs, and a
draw-level API can be added later as another default trait method without
breaking anything — the same compatibility rule that let the event stream
slip in beside the lifecycle hooks.

**Growth rule:** every framework feature must be a primitive that enables a
family of plugins — the event stream, status segments, `spawn`, the settings
pocket — not a one-off hook for one idea. The built-in plugins are the
validation: if a proposed API cannot be exercised by one of them, it is
probably not a primitive yet.

## 8. Related docs

- [local-plugins.md](local-plugins.md) — installing and managing external plugins
- [multi-provider.md](multi-provider.md) — provider instances, wire quirks, custom protocols
- [profiles.md](profiles.md) — profile presets and the `tools` filter
- [../README.md](../README.md) — getting started
- Design notes (Chinese): [provider.md](provider.md), [session-id.md](session-id.md), [session-continue.md](session-continue.md)
- Contract crate reference: [../crates/cst-plugin-api/README.md](../crates/cst-plugin-api/README.md)
