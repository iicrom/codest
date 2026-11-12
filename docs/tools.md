# Tool calling

English | [简体中文](zh/tools.md)

codest implements the complete OpenAI tool-call loop. The concrete tools are not built into the core: each one is declared by a plugin, and the built-in `bash` and `fs` sets are ordinary plugins (§3 in [plugins.md](plugins.md)).

## The loop

1. The plugin host collects every enabled tool into a declared `ToolSpec`: a name, a description, a JSON Schema for the arguments, and a `read_only` flag (advisory — it marks inspection-only tools for the UI and for read-only profile filtering, and enforces nothing).
2. The specs ride the request in its `tools` field. When the set is empty, no `tools` field is sent at all.
3. The model answers with tool calls. Streamed `delta.tool_calls` fragments are reassembled per call index into complete calls (id, name, raw JSON arguments). Each call is dispatched to the plugin that declared that tool, through its `run_tool(&ToolCall)` method.
4. The tool returns either result text or an error text; either way it goes back to the model as a `role: "tool"` message carrying the matching `tool_call_id`.
5. The next round starts with the results in the conversation snapshot. Tool execution runs on the blocking pool, so `run_tool` may block; the interrupt token is watched while it runs, and an interrupted call's outcome is discarded.

Tool rounds are **unbounded by default** (codex behavior): the turn ends when the model stops calling tools, the request errors, or you press `Esc`. Set `max_tool_rounds = 64` in `config.toml` for a guardrail; the turn then ends with a `tool round limit reached` error once that many rounds have completed.

On the wire, a declared tool is a standard chat-completions `tools` entry, and its result answers the call by id:

```json
{"tools": [{"type": "function", "function": {"name": "read", "description": "Read a text file with line numbers.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}]}
```

```json
{"role": "tool", "tool_call_id": "call_1", "content": "[exit code: 0]\n..."}
```

## Built-in tools

### `bash`

- One shell command via `$SHELL -c` (falling back to `/bin/sh`), run in the workspace directory.
- `timeout` parameter in seconds, default **300**, capped at **600** — an over-limit value is clamped, not rejected.
- On expiry the whole **process group** is killed (`process_group(0)`), so background children cannot outlive the timeout or hold the pipes open.
- Output merges stdout and stderr (stdout first, then stderr) and ends with an exit-code footer (`[exit code: N]`, `[terminated by signal]`, or the timeout notice).
- Tail-truncated to the last **2000 lines / 50 KB** with an actionable notice; single lines are clipped at 2000 characters, and each pipe is capped at 1 MiB while collecting so a runaway command cannot exhaust memory.

### `fs`

| Tool | Behavior |
| --- | --- |
| `read` | Line-numbered window over a text file, with `offset` (1-based) and `limit` continuation; refuses binary files and points at `bash` instead. Marked `read_only`. |
| `write` | Create or overwrite a file with complete content, creating parent directories as needed. |
| `edit` | Exact-string replacement via `old_string`/`new_string`; requires a unique match or `replace_all`. BOM and CRLF are preserved, so untouched lines keep their bytes. |

All paths resolve against the working directory when relative.

## Session tool toggles

`/tools` controls which declared tools this session offers:

| Command | Effect |
| --- | --- |
| `/tools` | Open a checkbox picker over the declared tools (`↑`/`↓` move, Space toggles, `Enter` applies the whole set as one op). |
| `/tools <name>` | Flip one tool on or off. |
| `/tools <name> on` / `off` | Pin one tool on or off. |

`read_only` tools are marked in the picker; unknown tool names and directions (`on`/`off`, plus `enable`/`disable`) are rejected with an error listing the available tools.

- **Disabled tools are removed from the request's `tools` field entirely** — saving tokens and preventing hallucinated calls.
- A call to a disabled tool that the model makes anyway is **rejected at execution time**: it never runs, and the model gets a `tool … is disabled in this session (re-enable it with /tools)` error. A name nobody declares comes back as `unknown tool …`. Both travel back like any other tool error, prefixed `error: ` on the wire.
- The enabled set is **per session** and is recorded in the rollout, so a resumed session continues with the tools it last ran with.
- **Profile vs `/tools` are two different seams.** A profile's `tools` filter works by **plugin id at assembly time** (startup only), deciding which plugins' tools are declared at all; `/tools` then fine-tunes individual tools **per tool at session time**. See [profiles.md](profiles.md).

The two layers compose: profile `tools = ["fs"]` removes every `bash` tool from the session's world, and `/tools write off` then hides `write` from the model as well. Nothing restores a tool the profile filtered out — that would need a restart without the profile.

## Tool cards

Every call renders as a tool card in the transcript:

```text
● bash(cargo build) · 4.2s
 ⎿  Compiling codest v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s)
    [exit code: 0]
 … +12 lines (ctrl+o to expand)
```

- The header is `● <tool>(<first argument>) · <elapsed>`, with the tool name in its category color. A restored card carries no duration chip — rollouts record messages, not timings.
- The `⎿` body shows the first **3 lines** of the result on a hanging spine.
- The rest collapses into `… +N lines (ctrl+o to expand)`; `Ctrl+O` expands or folds all tool output (see [commands.md](commands.md)). Folded and expanded are per-frame view decisions, so the cells themselves stay stateless.
- While a call runs, a live card with a spinner header and an elapsed row sits in the transcript tail; tools run sequentially, so at most one is ever live.

## Testing the loop offline

`scripts/openai_test_server.py` is a stdlib-only OpenAI-compatible server that turns message keywords into real tool calls, so the whole loop can be exercised without a network:

| Keyword | Call |
| --- | --- |
| `@bash <command>` | `bash` with `{"command": "<command>"}` |
| `@read <path>` | `read` with `{"path": "<path>"}` |
| `@write <path> <content>` | `write` |
| `@edit <path> \| <old> \| <new>` | `edit` |
| `@tool [args…]` | the tool named by `--tool` (default `ping`, which nothing implements) with `--tool-arguments` |

Any message without a keyword gets a **help reply** listing the keywords, so a plain send confirms the server is up. The server also takes `--delay` (seconds between streamed tokens), `--model`, and `--dump-headers <path>` (one JSON line per request with its headers, for asserting what reached the wire).

```bash
# terminal 1
python3 scripts/openai_test_server.py --port 8080

# terminal 2
codest --api-key test --base-url http://127.0.0.1:8080/v1 --model test-model
```

`scripts/tool_smoke.py` drives the same loop end to end: real `bash`/`read` execution plus `/tools` toggles.

## Design note

- Tools are the plugin system's model-facing half: a plugin declares them with `tools()` and executes them with `run_tool()`, and nothing in the core knows a tool by name.
- Tool errors are conversation content, not turn failures: a bad argument, a missing file, or a non-zero exit reaches the model as text it can act on, and the loop continues.
- Plugins also supply **skills and workspace instructions** through `system_prompt()`; `/context` labels each fragment with its source, so the assembled prompt can be audited per tool and per fragment.

## Related

- [plugins.md](plugins.md) — the plugin contract, effects, and the built-in tool plugins in depth.
- [commands.md](commands.md) — `/tools`, `Ctrl+O`, and the rest of the command surface.
- [sessions.md](sessions.md) — how tool state and tool calls are recorded and restored.

[← Back to the README](../README.md)
