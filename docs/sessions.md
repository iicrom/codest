# Sessions

English | [简体中文](zh/sessions.md)

A session is one conversation: its message history, thinking cards, tool calls, and the per-session switches (model, effort, tools, profile) that go with them. Sessions are named, listed, switched in-process, and persisted to disk as append-only JSONL rollouts so they survive restarts.

## Session commands

| Command | Aliases | Behavior |
| --- | --- | --- |
| `/new` | — | Archive the current conversation and start a fresh session. |
| `/rename <name>` | — | Rename the current session (default name: its first user message). |
| `/sessions` | `/resume` | Open the session list; switch back and continue one. |
| `/continue` | — | Continue the current session without appending a new message — the recovery entry point after a crash or interrupt. |

## Behavior

- **Only one session generates at a time.** The active session is the only one that can be busy; there is no parallel generation across sessions.
- **Input is ignored while generating.** Typing still edits the composer, but a submission (message or command) is dropped until the turn ends.
- **Session commands need a turn boundary.** `/new`, `/sessions`, and `/continue` are not accepted while a turn runs — press `Esc` to interrupt first, then run the command.
- **Switching restores context.** Selecting a session in `/sessions` restores its conversation; the core also re-applies the model selection, the reasoning effort, and the enabled-tool set that session last used (best-effort — a selection that no longer resolves keeps the current model), and the footer usage counters are seeded from the recorded turns. Ops are FIFO, so the next user turn is answered with the restored context. Selecting the already-active session just closes the menu.
- **`/clear` is per session.** It clears only the active session's context and transcript; other sessions, and the composer's input history, are untouched.
- **The session list shows name, cell count, and a `current` marker.** Rows are numbered, the active session carries the marker, and the name is truncated to fit.
- **Default naming.** A session is named after its first user message until `/rename` overrides it; the name is recorded in the rollout, so it survives restarts.

## Session lifecycle

Session commands are front-end behavior, not model turns — they are dispatched locally and never appended to the conversation as a user message.

| Action | What it does |
| --- | --- |
| `/new` | The view switches to a fresh session immediately and the core is told; because ops are FIFO, the next turn lands on the new conversation. The old session stays in the list and on disk. |
| `/rename <name>` | Sets the display name. Newlines are flattened to spaces (the list renders one row per session), a notice is shown, and the rename is appended to the rollout so it survives a restart. |
| `/sessions` | Opens a modal picker over the recorded sessions, starting from the active one. `↑`/`↓` move, `Enter` confirms, `Esc` closes; typing and `Ctrl+C` still reach their normal handlers. |
| `Esc` (in the picker) | Closes the menu without switching. |

- The picker is modal for navigation only: `↑`/`↓` move the selection, plain `Enter` confirms. A confirmed switch appends a `Session resumed` notice to that session's transcript.
- `/clear` resets the active session's context and transcript, but keeps its name, its file, and the composer's input history. It also resets the usage counters, because the dropped turns no longer describe the context; the replayed rollout resets the same counters.
- The rollout writer lives in the core, like codex's: the worker appends a line whenever the conversation actually changes (user message, finished assistant message, tool calls and results, clear, rename, switches).

The picker renders one row per session — marker, 1-based number, the name (the `/rename` name, else its first user message, truncated to fit), the cell count, and a `current` marker on the active one:

```text
▸ 1 · refactor the rollout loader · 24 cells · current
  2 · why does the SSE parser drop frames · 9 cells
  3 · session 3 · 2 cells
```

## Persistence (rollout)

- Sessions are stored per workspace at `~/.codest/sessions/<workspace-hash>/rollout-<uuid>.jsonl`.
- The workspace hash is **FNV-1a of the startup directory**, so every project keeps its own session set.
- The uuid is a **UUIDv7**, so file-name order is creation order; the same id is the value sent in the `session-id` request header.
- The file is **append-only JSONL**. Every line is a timestamp plus one record, and the first line is the session metadata. The record kinds are `snake_case`-tagged:

| Line kind | What it records |
| --- | --- |
| `session_meta` | Session id, creation time, workspace `cwd`, the system prompt, and the initial name — always the first line. |
| `user_message` | The submitted prompt. |
| `assistant_message` | A completed assistant reply. |
| `assistant_reasoning` | One thinking card (text plus the stream's duration); display-only. |
| `assistant_tool_calls` | The whole round's tool calls, replayed as one assistant message with `tool_calls`. |
| `tool_result` | One tool's output, replayed as a `role: "tool"` message answering its `tool_call_id`. |
| `cleared` | A `/clear` within the session (system prompt retained). |
| `renamed` | A `/rename`; the last one wins on replay. |
| `model_switched` | A model switch; the last one wins on replay, so a resumed session starts on the model it last used. |
| `reasoning_effort_switched` | The session's reasoning effort (`/effort`, `/profile`); the last one wins on replay, and an explicit `off` is recorded as a choice rather than as "unset". |
| `tools_switched` | The session's enabled-tool set; the last one wins, and absence means every declared tool was on. |
| `profile_switched` | The active profile's name. |
| `turn_usage` | Prompt/completion token counts of one completed turn, with the model that served it. |
| `context_deleted` / `context_edited` / `context_restored` | Edits made in the `/context` viewer: indices plus the removed content (the delete's inverse), and the undo marker. |

- **Replay is faithful.** Rebuilding the conversation from the file reproduces it exactly — including collapsed-vs-expanded card state. Thinking records are display metadata: they are replayed as cards but never re-enter the model context.
- **Eager id, lazy file.** The id is assigned when the session starts (so the very first request already carries the session header), but the file is created lazily on the first user message. Empty sessions never create a file.
- **Writes are best-effort.** A failed write never breaks the conversation; it is reported to stderr and the turn continues.
- **A file whose name is not a uuid is not a session.** Listing skips it, including the pre-uuid hex-nanosecond names of older builds.
- **Replay tolerates the future.** Unknown record types are skipped when reading, so a newer codest can extend the format without breaking older readers; a reasoning line that lost the message it preceded (a file truncated between the two) is dropped rather than shown above an unrelated turn.
- **The root is relocatable.** `CODEST_HOME` replaces `~/.codest`, taking the whole `sessions/` tree with it.

```text
~/.codest/sessions/
└── 3f9c1a77b2d04e18/                  # FNV-1a of the startup directory
    ├── rollout-0192f3c1-...-a1b2.jsonl   # created first
    └── rollout-0192f3c1-...-c3d4.jsonl   # created later (UUIDv7 order = name order)
```

```jsonl
{"timestamp":"2026-01-04T12:00:00Z","type":"session_meta","version":1,"id":"0192f3c1-...","created_at":"2026-01-04T12:00:00Z","cwd":"/work/project","system_prompt":"..."}
{"timestamp":"2026-01-04T12:00:09Z","type":"user_message","message":"why does the parser drop frames?"}
{"timestamp":"2026-01-04T12:00:14Z","type":"assistant_reasoning","text":"...","duration_ms":4200}
{"timestamp":"2026-01-04T12:00:15Z","type":"assistant_message","message":"Because ..."}
```

- **No concurrent-write protection.** Two codest instances on the same workspace can interleave appends into different files, and nothing arbitrates. There is also no archive/delete/search and no cross-workspace migration.

## Startup and resume flags

```bash
codest --continue    # activate the most recent saved session of this workspace (-c)
codest --resume      # same, but open the session picker first
```

| Flag | Behavior |
| --- | --- |
| `--continue`, `-c` | Load the most recent saved session of this workspace and continue it directly. |
| `--resume` | Load every saved session of this workspace and open the picker at startup. |

The two flags are mutually exclusive; without either, codest starts a fresh session (with a fresh id, and no file until the first message).

What a resumed session brings back:

| Restored | Notes |
| --- | --- |
| Conversation and thinking cards | Replayed from the rollout before anything renders. |
| Model selection | The selection that session last used, adopted before the first render. |
| Enabled-tool set | The last `/tools` state, resynced to the front-end. |
| Footer usage | Seeded from the recorded turns, so the context size shows before the first new turn. |
| Profile *label* | The profile the session last used is shown as active; its presets are not re-applied — a restored session restores its own state instead. |

With nothing saved, both flags fall back to a fresh session; `--resume` still opens the picker, which then holds just that one new session.

## `/continue`: in-process recovery

- `/continue` resumes the current session **without re-sending a message** — it drives the existing transcript as it stands, so the model picks up exactly where things stopped. It does not fire the user-message hooks.
- A tool round interrupted by a crash or `Ctrl+C` is sealed with an explicit `interrupted` tool result (`interrupted: the session ended before this tool call finished`), so the next request stays wire-valid: every `tool_call` has a matching `role: "tool"` reply, and the model can react to the interruption instead of the request being rejected.
- Sealing happens in two places, and either one restores a wire-valid transcript: on **abort** the closing result is appended to the conversation *and* the rollout, keeping file and memory in sync; on **replay** the same seal runs in memory, covering files truncated by a crash that never reached the abort path.
- The file itself is **never rewritten** — it is append-only, so sealing on replay is idempotent; a dangling line stays on disk and any later result line completes the round naturally.
- An empty transcript is refused with an error rather than starting a turn.
- It only runs while idle: `/continue` during generation is ignored. With no usable route it prints the same add-a-provider guidance as a normal send.

See [session-continue.md](session-continue.md) (Chinese design note) for the full recovery semantics, including the transcript-tail cases and the abort/replay dual path.

## Session id and the `session-id` header

- Every session has a UUIDv7 id. By default each request carries it as a `session-id` header (codex's spelling), so endpoints that route or cache per conversation recognize it without configuration.
- The header is configurable per instance via `wire.session_header`, and per model via `models.session_header`, which overrides the instance's.

Disable or rename it per instance:

```toml
[[providers]]
name = "strict-endpoint"
base_url = "https://example.test/v1"
wire = { session_header = "none" }          # this endpoint rejects unknown headers

[[providers]]
name = "gateway"
base_url = "https://gateway.test/v1"
wire = { session_header = "x-session-id" }  # use the gateway's spelling
```

- `"none"` opts the instance (or model) out entirely; an empty or blank name is a configuration error, not another way to opt out.
- A model-level `session_header` can also opt back in on an instance that opted out.
- Static `headers` cannot pin the header: the per-turn session id is applied after them, so a hand-written same-name entry is overridden, matching how `/effort` overrides `body_overrides`.

See [session-id.md](session-id.md) (Chinese design note) for the eager-id/lazy-file split and the compatibility rules, and [configuration.md](configuration.md) for the surrounding file reference.

## Related

- [commands.md](commands.md) — the full command table and keybindings.
- [tools.md](tools.md) — per-session tool toggles and how they are restored.
- [configuration.md](configuration.md) — `config.toml`, `providers.toml`, session header, environment variables.

[← Back to the README](../README.md)
