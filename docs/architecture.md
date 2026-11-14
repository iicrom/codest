# Architecture

English | [简体中文](zh/architecture.md)

codest is split so that the UI never talks to the model client directly. The front-end submits operations (`Op`) and receives a stream of events (`Event`); a worker owns the conversation, the history, and the network stream. This is the same producer/consumer decoupling as codex's SQ/EQ, kept small enough to read in one sitting.

```
┌──────────┐   Op (submit)    ┌────────────────┐  StreamItem   ┌────────────────────┐
│   TUI    │ ───────────────▶ │ session worker │ ────────────▶ │ ModelClient        │
│ tui/app  │                  │ core/session   │               │ core/openai_compat │
│          │ ◀─────────────── │                │ ◀──────────── │ (HTTP + SSE)       │
└──────────┘  Event (render)  └────────────────┘    chunks     └────────────────────┘
```

The bus itself ([`src/core/protocol.rs`](../src/core/protocol.rs)) defines only data: `Op` is the vocabulary of commands a front-end may issue, `Event` wraps an `EventMsg` with the id of the submission that caused it, and `Submission` is the queued pair. `Op::UserTurn`, `Op::ContinueTurn`, `Op::ClearConversation`, `Op::NewSession`, `Op::SwitchSession`, `Op::RenameSession`, `Op::SwitchModel`, `Op::SetReasoningEffort`, `Op::SwitchProfile`, `Op::SetToolsEnabled`, `Op::InspectConversation`, the `Op::DeleteMessages` / `Op::EditMessage` / `Op::UndoContextEdit` trio, `Op::Interrupt`, and `Op::Shutdown` all travel over the same channel. The reply side is likewise one enum: turn progress (`TurnStarted`, `AgentMessageDelta`, `AgentThinkingDelta`, `AgentMessage`, `AgentToolCallStarted`, `AgentToolCall`, `TurnUsage`, `TurnAborted`, `Error`, `TurnComplete`), session bookkeeping (`SessionStarted`, `SessionSwitched`, `SessionRenamed`, `ConversationCleared`), and configuration echoes (`ModelSwitched`, `ReasoningEffortSwitched`, `ProfileSwitched`, `ToolsSwitched`, `ConversationSnapshot`, `ShutdownComplete`).

## What was distilled from codex

| codex module | codest module | Notes |
| --- | --- | --- |
| `codex-rs/protocol` (SQ/EQ) | `src/core/protocol.rs` | `Op` / `Event` message bus; UI never calls the model directly. |
| `codex-rs/core` session loop | `src/core/session.rs` | Worker owns all conversation history and the cancel token; emits events. |
| Multi-session management | `src/core/sessions.rs` + `src/tui/session.rs` | Command specs + per-session view state (`/new`, `/sessions`). |
| Rollout persistence | `src/core/rollout.rs` | JSONL sessions under `~/.codest/sessions/`. |
| `codex-rs/core/src/client.rs` | `src/core/model.rs` | Streaming model client abstraction. |
| Responses client | `src/core/openai_compat.rs` | Minimal OpenAI Chat Completions + SSE parsing. |
| fake/OSS provider | `scripts/openai_test_server.py` | Local OpenAI-compatible test server (no in-crate mock backend). |
| `codex-rs/tui/src/app.rs` | `src/tui/app.rs` | Terminal event loop, rendering, input. |
| `bottom_pane/textarea.rs` | `src/tui/composer.rs` | Multi-line input, cursor, newline mapping. |
| `history_cell*` | `src/tui/chat.rs` | User/assistant/error/notice/interrupt cells; thinking and tool cards. |
| `markdown_render.rs` | `src/tui/markdown.rs` | Markdown → ratatui lines (highlighting behind `highlight.rs`). |
| wrapping | `src/tui/wrap.rs` | Unicode-width-aware wrapping and cursor mapping. |

codex's TUI is 100k+ lines and its core carries a large sandbox/tool/approval infrastructure tightly coupled to the app-server; it cannot be trimmed as-is. codest extracts an **independently compilable architectural skeleton**, not a file-by-file copy.

## Core message flow

One turn, from keystroke to rendered line, moves through three modules.

**1. Submission.** The TUI calls `AgentSession::submit`, which stamps the op with a per-process id (`sub-1`, `sub-2`, …) and pushes a `Submission` onto an unbounded channel. `AgentSession` hands the front-end only two primitives: `submit(op) -> String` and `next_event().await -> Option<Event>`. Every event produced for a submission carries its id, so concurrent ops can be told apart.

**2. The worker accepts the turn.** `run_worker` (in [`src/core/session.rs`](../src/core/session.rs)) owns all sessions, the rollout writers, and the tool set. On `Op::UserTurn` it appends the text to the active `Conversation`, records it in the rollout, builds a client for the current selection, calls `ModelClient::stream_chat(messages, options, cancel)`, and moves the resulting stream into an `ActiveTurn`. `TurnOptions` carries the per-turn effort, the declared tool specs, and the active session id — all resolved at that moment, so a `/model` or `/tools` change between turns takes effect on the next request without touching a client. Before any of that, two guards answer with guidance instead of a doomed request: no backend selected at all, and a half-specified endpoint (a lone `base_url` or `api_key`).

**3. Streaming.** `ModelClient` ([`../crates/cst-plugin-api/src/model.rs`](../crates/cst-plugin-api/src/model.rs)) returns a `ModelStream`, a boxed stream of `Result<StreamItem, ModelError>` rather than a whole response. `run_turn` `select!`s between the stream and the turn's cancellation token, and forwards each item as an event: `StreamItem::Delta` becomes `AgentMessageDelta`, `StreamItem::Thinking` becomes `AgentThinkingDelta`, `StreamItem::ToolCalls` is held for the tool round, `StreamItem::Usage` is remembered for the footer. Tool calls then run one by one — `AgentToolCallStarted` before each, `AgentToolCall` after — and the results are appended to the conversation before the next round re-streams. The round loop ends when the model stops calling tools, or errors out with a "tool round limit reached" message once `max_tool_rounds` is set and hit.

Text that streamed before a round's tool calls is committed first, together with the round's reasoning, so the transcript keeps its order (text → calls → results) and a replay reconstructs the same shape. `StreamItem` is `#[non_exhaustive]`, so a plugin protocol adding a variant is ignored rather than breaking this build.

**4. Settling.** `finalize_complete` records the round's reasoning and assistant text in the rollout, pushes the text into the conversation, emits `AgentMessage`, then `TurnUsage` and `TurnComplete`. The front-end renders on the event stream; it never reads conversation state directly except through `Op::InspectConversation`, whose answer is a `ConversationSnapshot`.

**Cancellation.** A `CancellationToken` is created per turn and shared with the client. The front-end's `Esc` path calls `InterruptHandle::interrupt`, which cancels the token of the in-flight turn; the worker's `select!` is biased toward cancellation, so it finalizes immediately: any tool call the model asked for but whose result never arrived is sealed with an explicit `interrupted` tool result (keeping the next request wire-valid), partial assistant text is committed, and the turn ends with `TurnAborted` plus `TurnComplete`. A blocking tool already running on the blocking pool cannot be stopped — its result is dropped. `Op::Interrupt` exists on the bus too, but the TUI uses the handle directly so the interrupt never queues behind the turn it is cancelling. The same `InterruptHandle` is installed at the start of each turn and cleared on every finalize path, so an idle `Esc` is a no-op rather than a stray cancel.

**Steering.** A mid-turn user message takes a second direct path, parallel to cancellation: `AgentSession::steer` pushes into a `SteerHandle`-owned channel, and `run_turn` drains it at every *step boundary* — after a tool round's results are recorded, before the next request is assembled — appending each message to the conversation (and rollout) and emitting `SteerClaimed`. Whatever never reaches a boundary — a tool-less turn that completed, an error, an abort — is drained before the finalize and handed back as `SteerUnclaimed`, so the front-end can requeue it as a regular turn instead of losing the input; the worker's idle wait drains the same channel, since with no turn running there is no boundary to claim anything. Like the interrupt, this bypasses the submission FIFO deliberately: a message sent through the queue would wait out the whole turn.

**Queued turns.** Enter on a busy front-end submits a plain `Op::UserTurn`, which the FIFO holds until the running turn finalizes — the worker never reads the channel mid-turn. Because a config change made *after* a message was queued must still govern that message's turn, the worker pre-drains the config-op family (`SwitchModel`, `SetReasoningEffort`, `SwitchProfile`, `SetToolsEnabled`) out of the channel before starting any turn and applies them first; everything else keeps its FIFO place. That is what makes `/model` "applies from the next turn" hold regardless of queue order.

## State ownership

Only the worker mutates conversation state. That is the rule the rest of the design follows from, and it explains why several things look indirect.

| State | Owner | How the front-end sees it |
| --- | --- | --- |
| Conversation history (system prompt + messages) | Worker, one `Conversation` per session | `ConversationSnapshot` on request or after a context mutation |
| Session list | Worker (append-only, creation-order indexes) | `SessionStarted` / `SessionSwitched` / `SessionRenamed` |
| Active model selection | Worker's `ModelBackend` (`Fixed` / `Pooled` / `Empty`) | `ModelSwitched`, plus the startup route |
| Reasoning effort | Worker, per run | `ReasoningEffortSwitched` |
| Enabled tool set | Worker, per session | `ToolsSwitched`, emitted on change and resynced on session switch |
| Rollout files | Worker's `Rollouts` (one writer slot per session) | not visible; replay happens at startup |
| Rendered transcript, scroll, fold flags | Front-end, one view per session | not visible to the worker |
| Pending model stream | Worker's single `ActiveTurn` | `AgentMessageDelta` / `AgentThinkingDelta` |

Two consequences are worth calling out. First, switching sessions mid-run restores that session's model and tool set, because both are recorded per session and reapplied when it becomes active; the events arrive before the confirming `SessionSwitched` so the front-end's mirror is consistent by the time it reacts. Second, the front-end keeps a mirror of configuration (footer labels, menu checkboxes) and treats the worker's event as authoritative — an optimistic notice may appear first, but the event settles it.

A worker streams at most one turn at a time. Submissions are FIFO, so a `/model`, `/effort`, `/profile`, or `/tools` change submitted during a turn lands *between* turns: the running turn keeps its `TurnOptions` and the next one picks up the new values. That is why the config ops are described as "applied between turns" rather than "immediate" — and the pre-drain above guarantees that "between" means *before the next turn starts*, not merely "after the current one ends".

## The front-end loop

`tui/app.rs` runs one `tokio::select!` over five inputs: a crossterm `EventStream` (keys, mouse, paste, resize), the worker's `next_event()`, a channel of background `/provider` results, a channel of scheduled notices (model discovery reports through it), and a 90 ms ticker whose missed ticks are skipped. The select loop redraws at the top of every iteration, so any of them can produce a frame. The ticker exists so spinners animate and time-boxed footer messages expire even when nothing else happens; it is not a render-on-demand loop.

The loop is where plugin effects are applied. A plugin never touches the terminal: it returns one of the `PluginEffect` values — notice, error, report block, floating report window, clear conversation, quit, set composer, submit a turn, continue the turn — and the front-end translates each into its own action. Because the effects are data, the same plugin code runs under any front-end, and the plugin layer needs no ratatui dependency.

Rendering is per event and per tick: the app keeps one `SessionView` per worker session, holding the rendered cells plus the scroll and fold flags, and draws whichever view is active. `/provider` work and model discovery run on their own tasks and report back through channels, so a slow probe never blocks the render thread.

## Startup assembly

`src/cli.rs` is the assembly point, and it runs before any worker exists. It loads `profiles.toml` (keeping the `--profile` preset), builds the `PluginHost` with that profile's tool filter applied, loads `config.toml` for the global defaults, and builds the `ModelPool` from `providers.toml` (or synthesizes a single endpoint from flags and env when there is no file). It then resolves the startup selection — `--provider`/`--model`, else the active profile's `default`, else `config.toml`'s `default`, else the first model of the first instance — and proves it builds a client, so a broken endpoint fails at startup rather than on the first turn. Finally it maps `--continue` / `--resume` to a `RestoreMode` and hands everything to `tui::run`.

Two details matter for anyone adding a setting. The profile's *assembly-time* values (tool filter, system-prompt append) are applied here, once; its *session-time* values (selection, effort) are seeded into the worker and can still change via ops. And `ModelBackend` has three shapes — `Fixed` for tests and simple hosts, `Pooled` for the multi-provider runtime, `Empty` for the "nothing configured" start, where a turn answers with guidance instead of a request until a `/model` switch migrates it.

## Project layout

```
codest/
├── Cargo.toml
├── LICENSE
├── README.md                  # this file (English)
├── README.zh-CN.md            # Chinese version
├── crates/
│   └── cst-plugin-api/        # plugin contract crate (breaks the Cargo dependency cycle)
├── docs/                      # user guides (English) + design notes (Chinese); docs/zh/ = 中文版
├── scripts/                   # offline test server + pty smoke tests
└── src/
    ├── main.rs                # codest entry point
    ├── bin/cst.rs             # cst alias entry point
    ├── cli.rs                 # clap CLI + startup assembly
    ├── lib.rs
    ├── core/
    │   ├── protocol.rs        # Message / Op / Event bus
    │   ├── commands.rs        # command table / parsing
    │   ├── sessions.rs        # /new /rename /sessions command specs
    │   ├── reasoning.rs       # /effort /thinking specs + effort levels
    │   ├── providers.rs       # protocol factories / registry / model pool / /model
    │   ├── config.rs          # config.toml / providers.toml / profiles.toml + $ENV templates
    │   ├── context.rs         # /context command
    │   ├── context_store.rs   # context browse/edit state
    │   ├── tools.rs           # tool dispatch
    │   ├── tools_switch.rs    # /tools session toggles
    │   ├── settings.rs        # /settings command + config.toml writes
    │   ├── conversation.rs    # linear conversation history
    │   ├── model.rs           # ModelClient trait + StreamItem + errors
    │   ├── openai_compat.rs   # HTTP + SSE streaming client (openai-compat protocol)
    │   ├── sse.rs             # shared SSE frame parser
    │   ├── tokens.rs          # token accounting
    │   ├── rollout.rs         # session persistence (JSONL rollout)
    │   ├── session_id.rs      # UUIDv7 session ids
    │   ├── echo_model.rs      # in-process echo model (tests + `test` protocol backend)
    │   └── session.rs         # agent worker + AgentSession
    ├── plugins/
    │   ├── mod.rs             # Plugin trait host / effects / protocol collection
    │   ├── install.rs         # codest plugin install|uninstall|list|build
    │   └── builtin/           # static registry: help, clear, quit, status, continue, tps, tools/{shell,bash,pwsh,fs,truncate}
    └── tui/
        ├── app.rs             # event loop / fullscreen & inline rendering / menus
        ├── session.rs         # per-session view state
        ├── chat.rs            # history cells (incl. thinking & tool cards)
        ├── composer.rs        # input editor
        ├── provider_wizard.rs # /provider wizard
        ├── context_view.rs    # /context viewer
        ├── settings_view.rs   # /settings screen (field → row mapping)
        ├── screen.rs          # full-frame screen chrome: header/strip/rule/notice/hint + scroll
        ├── cards.rs           # card borders, pointer gutter, row/value column
        ├── inline_edit.rs     # one-line editor shared by the list screens
        ├── markdown.rs        # Markdown rendering (pulldown-cmark)
        ├── highlight.rs       # code highlighting (syntect → TokenKind)
        ├── theme.rs           # colors
        └── wrap.rs            # wrapping & cursor mapping
```

Three boundaries are worth naming. `crates/cst-plugin-api` is a separate crate so a plugin can depend on the contract without depending on the binary; `src/core/model.rs` only re-exports the trait from it. `src/core` knows nothing about ratatui, and `src/tui` knows nothing about HTTP. `src/plugins` sits beside both and reaches them only through the contract types.

## Extension points

| You want to | Go to |
| --- | --- |
| Add a slash command, hook, tool, protocol, or auth flow | [plugins.md](plugins.md) — the `Plugin` trait and its effects. |
| Support another endpoint's wire protocol or quirks | [multi-provider.md](multi-provider.md) — instances, wire quirks, protocol factories. |
| Package a model + effort + tools + prompt preset | [profiles.md](profiles.md) — presets and selection precedence. |
| Install a plugin from another repository | [local-plugins.md](local-plugins.md) — the out-of-tree wrapper crate. |

Three contract files carry the seams: [`../crates/cst-plugin-api/src/plugin.rs`](../crates/cst-plugin-api/src/plugin.rs) (the `Plugin` trait and effects), [`../crates/cst-plugin-api/src/model.rs`](../crates/cst-plugin-api/src/model.rs) (`ModelClient`, `StreamItem`, `TurnOptions`), and [`../crates/cst-plugin-api/src/sse.rs`](../crates/cst-plugin-api/src/sse.rs) (the shared SSE decoder plugin protocols reuse). `src/core/model.rs` and `src/core/sse.rs` are thin re-exports of the last two, kept so the historical module paths still work.

## Related

- [tui.md](tui.md) — how events become pixels: modes, Markdown, cards.
- [configuration.md](configuration.md) — where providers and profiles are declared.
- [../src/core/protocol.rs](../src/core/protocol.rs) — the full `Op` / `EventMsg` vocabulary.
- [../src/core/session.rs](../src/core/session.rs) — the worker loop and turn finalization.

[← Back to the README](../README.md)
