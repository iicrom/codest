# Development

English | [简体中文](zh/development.md)

Everything here runs offline. `cargo test` uses an in-process fake model and never touches the network; the smoke scripts that need an endpoint start their own local OpenAI-compatible server on a loopback port. Build with `cargo build` (Rust 1.88+, edition 2024); a debug binary is all the smoke scripts need.

## Build and run

```bash
cargo build                 # target/debug/codest and target/debug/cst (the alias binary)
cargo run -- --help         # default-run is `codest`
cargo install --path .      # installs both commands into ~/.cargo/bin
```

There is no feature matrix to pick from and no build script: the crate compiles from the dependencies declared in `Cargo.toml`. The only platform-conditional dependency is `libc` on Unix, used for the `bash` tool's process-group kill. Everything the smoke scripts need is the debug binary plus `python3` (standard library only).

## Testing

```bash
cargo test                                      # unit tests (offline, in-process fake model)

python3 scripts/openai_test_server.py --port 8080   # local OpenAI-compatible server
python3 scripts/pty_smoke.py                    # end-to-end smoke in a pty (both TUI modes)
python3 scripts/tool_smoke.py                   # real bash/read execution + /tools toggles
python3 scripts/markdown_smoke.py               # visual markdown check (both TUI modes)
python3 scripts/session_header_smoke.py         # asserts the session-id header hits the wire
python3 scripts/plugin_smoke.py                 # external plugin install/build round-trip
```

| Command | What it asserts |
| --- | --- |
| `cargo test` | Unit tests across core and TUI modules. The model is `EchoModelClient`, an in-process deterministic echo that streams word-sized deltas, so nothing leaves the process. Session tests drive a real worker over the bus and assert the event sequence, cancellation, tool rounds, and rollout contents. |
| `pty_smoke.py` | Spawns the binary in a pseudo-terminal, types a message, and renders the captured ANSI output to a plain text grid. Both TUI modes run by default: `fullscreen` must emit `\x1b[?1049h` (alternate screen) and `\x1b[?1000h` (mouse capture), `inline` must emit neither; the wheel must scroll, the message must appear in the transcript, the mode notice and idle footer must appear, the process must exit cleanly. A config variant additionally checks model discovery, the picker switch, and the footer's context window (declared and discovered); both variants rerun with `--continue` to check the restored session and its recorded usage. |
| `tool_smoke.py` | Two cases, `bash` and `read`, against the local server: a real tool round must show the command's output (`hello-from-tool`, `exit code: 0`) or the file's content (`alpha`, `gamma`, `3 lines total`); then `/tools <name> off` followed by `/context` must report the narrowed set as `Tools (3/4 enabled)` and still list the tool as `disabled`. |
| `markdown_smoke.py` | A visual check in both modes, from a scripted server reply: heading text, bold/italic/inline code, bullet and nested-bullet markers, an ordered marker, the `▎` quote bar, a rule, two code fences with `│ rust` / `│ python` labels and body, a table header and divider, a task checkbox — plus the absence of raw `**bold**` markers. Inline mode is checked for the heading, a code label, and the absence of raw markers. |
| `session_header_smoke.py` | Three cases against a server that dumps request headers: the default instance sends a `session-id` header whose value matches a UUIDv7; `wire.session_header = "none"` sends none; a model-level opt-out sends none. It also fails if the client sent no request at all. |
| `plugin_smoke.py` | External plugin round-trip, defaulting to a sibling checkout (`--plugin ../cst-provider-qoder`): `codest plugin install <path>` exits cleanly, writes `plugins.toml`, generates the wrapper crate, and builds a wrapper binary; the manifest depends on the plugin by path and the generated code links its `PLUGIN` static; the built binary reports the plugin id in `/status` and its command in `/help`; `codest plugin uninstall` regenerates a plugin-free wrapper and rebuilds. A `git status --porcelain` check on the codest checkout proves the install left the repository untouched (`--skip-git-check` opts out). |

The TUI, tool, markdown, and session-header scripts each start and stop their own test server on an ephemeral loopback port (`plugin_smoke.py` instead builds a wrapper crate), so the standalone `openai_test_server.py` line is only for poking at the server by hand. Every script accepts a binary path argument and defaults to `target/debug/codest` (`session_header_smoke.py` uses `--binary`, defaulting to `target/debug/cst`).

### Where the unit tests live

Tests are colocated with the code they cover, in a `#[cfg(test)] mod tests` at the bottom of each module — 29 modules carry one today. Sync tests are plain `#[test]`; worker and TUI-loop tests are `#[tokio::test]` and drive the real `AgentSession` over the `Op`/`Event` bus with `EchoModelClient`, so they exercise the same path a real front-end does. Nothing is mocked below the model client except the clock-sensitive bits; a session test asserts the actual event sequence (`TurnStarted` … `TurnUsage` … `TurnComplete`), not internal state.

The modules with the heaviest coverage are the ones where a mistake is silent: `core/session.rs` (turns, cancellation, tool rounds, session switching), `core/rollout.rs` (replay, clears, renames, context checkpoints), `core/config.rs` (strict parsing, `$VAR` templates, wire quirks), `tui/markdown.rs` (each syntax construct, wrapping, table shrink), `tui/chat.rs` (card rendering and folding), and `tui/app.rs` (mode setup, footer, menus, `/provider` wiring).

`cargo test` needs no network once dependencies are fetched; the run itself is entirely in-process.

The session tests are worth skimming before changing the worker; their names are the specification. A representative slice: `streams_a_turn_end_to_end`, `tool_calls_execute_and_continue_the_turn`, `set_tools_enabled_withholds_declarations_and_rejects_calls`, `interrupt_lands_during_a_slow_tool_call`, `interrupt_keeps_partial_output`, `an_abort_mid_tool_call_closes_the_round_with_an_interrupted_result`, `a_runaway_tool_loop_hits_the_configured_round_limit`, `thinking_is_recorded_for_resume_but_stays_out_of_the_conversation`, `new_session_starts_fresh_context_and_switch_restores_it`, `a_fresh_session_has_an_id_before_it_has_a_file`, `switching_sessions_switches_the_id_sent_upstream`, `switch_model_swaps_the_backend_between_turns`, `an_empty_route_answers_turns_with_guidance`, and `switching_from_an_empty_route_arms_the_backend`. Between them they pin the two invariants the architecture rests on: the event sequence of a turn, and the fact that only `Op`s change worker state.

### Running one script by hand

```bash
cargo build                                     # produces target/debug/codest and target/debug/cst
python3 scripts/pty_smoke.py --mode inline      # one mode instead of both
python3 scripts/tool_smoke.py target/release/codest
python3 scripts/session_header_smoke.py --binary target/debug/cst
python3 scripts/plugin_smoke.py --skip-git-check
```

Each prints one `PASS`/`FAIL` line per assertion and returns a non-zero exit code if any failed, so a CI step only needs `cargo test && python3 scripts/...`.

### The test server

`scripts/openai_test_server.py` is stdlib-only (Python 3, no third-party packages) and takes `--host`, `--port` (0 for ephemeral), `--delay` (seconds between streamed tokens, default 0.02), `--model`, `--tool`, `--tool-arguments`, `--dump-headers` (append one JSON line per request), and `--quiet`. It serves:

- `POST /chat/completions` — an OpenAI-shaped request in, an SSE stream out (`data:` frames, then `data: [DONE]`), including a final chunk carrying usage. Reasoning deltas precede the visible reply, mirroring reasoning-tuned backends whose `reasoning_content` arrives first;
- `GET /models` — an OpenAI-shaped model catalog, so `fetch_models` discovery has something to find;
- keyword triggers — `@bash`, `@read`, `@write`, `@edit` map to that built-in tool with the text after the keyword (`@bash ls -la` → `bash` with `{"command": "ls -la"}`); `@write <path> <content>` and `@edit <path> | <old> | <new>` follow a fixed shape; `@tool` calls whatever `--tool` / `--tool-arguments` name;
- plain messages — any message without a keyword gets a help reply that lists these keywords and the endpoints.

The server holds no state between requests: it decides what to answer by inspecting the messages it was sent. A turn that already carries a tool round gets the round-summary reply instead of another call — without that guard the keyword would stay in the user message and the loop would never terminate. That single rule is what lets a smoke test drive the complete agent loop — request, tool call, tool result, follow-up round — with no network and no real model.

### Adding a test

- **Core behavior** — add a `#[tokio::test]` to `src/core/session.rs`'s test module, build an `AgentSession` with `EchoModelClient` (or a small recording client for request-shape assertions), submit the op, and collect events until `TurnComplete`. `collect_turn` / `collect_until` helpers already exist there.
- **Rendering** — add a plain `#[test]` to the module that renders the thing: `tui/markdown.rs` for Markdown, `tui/chat.rs` for cells, `tui/wrap.rs` for wrapping. Assert on `plain(&lines)` (the flattened text) so the test states what the user sees, not the span structure.
- **End-to-end behavior** — add an assertion to the matching smoke script. It runs against the real binary and the real event loop, so it catches wiring mistakes a unit test cannot; that is also why a regression test for a TUI bug belongs in `pty_smoke.py`.

## Repository conventions

- **Layout.** The binary is `codest` (`src/main.rs`) with the `cst` alias (`src/bin/cst.rs`), plus the `crates/cst-plugin-api` workspace member. Edition 2024, `rust-version = "1.88"`, resolver 3.
- **Dev profile.** `[profile.dev]` sets `opt-level = 0` and `debug = "line-tables-only"`; the shorter debug info keeps incremental builds quick, and a debug binary is what the smoke scripts drive.
- **No separate lint config.** There is no committed `rustfmt.toml`, `clippy.toml`, or CI workflow; `cargo fmt` and `cargo clippy` run on their defaults, and the few deliberate exceptions (`large_enum_variant`, `too_many_arguments`) are local `#[allow]` attributes with a reason.
- **Offline by construction.** No test, unit or smoke, reaches the public internet. Anything that needs a server starts one on `127.0.0.1` with port 0 and reads back the chosen port.
- **State isolation.** Every script points `CODEST_HOME` at a fresh temp directory (`$CODEST_HOME`, else `$HOME/.codest` is the real root), which is how they avoid touching a developer's state and how `--continue` cases are reproducible.

## Adding a plugin or touching docs

A plugin is Rust code linked into the binary. If you are adding one, start with [plugins.md](plugins.md): the `Plugin` trait, the effect vocabulary, the built-in plugins as examples, the startup validation rules, and the notes on why the registry is a static array. If you are installing a plugin from another repository rather than writing one here, [local-plugins.md](local-plugins.md) covers the out-of-tree wrapper crate, the generated crate layout, and contract versioning.

Docs are maintained bilingually: `docs/*.md` is primary, `docs/zh/*.md` mirrors it section by section with identical tables and code blocks. A fact that lives in code should be checked against the source before it is written down; the module-level doc comments are usually the most accurate statement of intent.

## Debugging a failing smoke test

- Run the binary against the server by hand first: start `openai_test_server.py --port 8080`, then launch codest with `--api-key test --base-url http://127.0.0.1:8080/v1 --model test-model`. If the loop works interactively, the failure is in the test's expectations, not the code.
- `pty_smoke.py` and `markdown_smoke.py` print the rendered screen next to the failing assertion; `tool_smoke.py` prints the screen on stderr when a needle is missing. Start there — the captured grid usually shows exactly which line is absent.
- For header questions, run the server with `--dump-headers` and read the JSON lines: the presence and spelling of a header are directly inspectable.
- A TUI test that hangs is usually waiting on an idle footer that never appears; check that the turn actually reached `TurnComplete` before assuming the render is at fault.

## Known limitations

- Text-only chat: no image input, no approvals, no MCP.
- Markdown rendering is a curated subset: no OSC 8 hyperlinks, no key/value table degradation, no mermaid or math; images show alt text and URL only.
- Plugins are compile-time static: adding or changing one requires a rebuild; no runtime `.so` or script plugins. Plugins run with host privileges — no sandbox.
- Plugin hooks run synchronously on the TUI thread; no blocking I/O in hooks.
- Sessions have no concurrent-write protection (two codest instances on the same workspace can interleave), no archive/delete/search, and no cross-workspace migration.
- The HTTP client is one request + SSE parsing: no retry, no proxy, no auth refresh. API keys support `$VAR` templates but there is no OAuth/credential storage; token rotation is up to the protocol factory.
- No provider configured → codest starts with an empty route: sending a message yields add-a-provider guidance instead of a request.
- In `fullscreen` there is no in-app mouse selection/copy; use the terminal's native selection (usually hold `Shift` and drag).
- In `inline`, lines already written to the scrollback are not re-wrapped on resize.

## Roadmap

1. Token budget and compaction for `Conversation` (driven by `context_window` and turn usage).
2. Write `/effort` back to `config.toml` as a global default.
3. Provider deletion in `/provider` (`ModelPool::remove_instance()` is ready, not yet wired); comment-preserving write-back via `toml_edit`.
4. Markdown: OSC 8 hyperlinks and key/value table degradation.
5. In-app mouse selection and OSC 52 copy in `fullscreen`.
6. Distributed static registration via `inventory`/`linkme` (the `Plugin` contract would not change).
7. Optional plugin signing and version-constraint solving once `cst-plugin-api` is published.

## Contributing

Issues and pull requests are welcome. For a PR, please:

1. Keep `cargo test` green (it is fully offline);
2. Add or update the relevant smoke script if you touch a user-visible flow;
3. Follow [Conventional Commits](https://www.conventionalcommits.org) — the history is `feat:` / `fix:` / `chore:` / `test:`.

If you are reporting a rendering or TUI bug, a `scripts/markdown_smoke.py` or `scripts/pty_smoke.py` reproduction helps a lot. Both render the captured screen to plain text and print every assertion as `PASS` or `FAIL` with a name, so the failing check can usually be read straight off the output; a new assertion in that script is also the natural regression test for the fix.

## Related

- [architecture.md](architecture.md) — the module map, the `Op`/`Event` bus, and the turn flow the tests exercise.
- [plugins.md](plugins.md) — writing and registering a plugin.
- [local-plugins.md](local-plugins.md) — installing plugins from other repositories.
- [tui.md](tui.md) — what the pty and markdown smoke scripts are looking at.
- [../README.md](../README.md) — installation and quick start.
- [../scripts/openai_test_server.py](../scripts/openai_test_server.py) — the offline test server.

[← Back to the README](../README.md)
