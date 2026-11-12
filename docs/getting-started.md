# Getting started

English | [简体中文](zh/getting-started.md)

Install codest, point it at an endpoint, and run a first session. The rest of this page is the reference for that first run: requirements, both installation paths, the quick-start recipes, and the full CLI surface.

## Requirements

- Rust **1.88+** (edition 2024) — only needed to build; there are no runtime dependencies outside the binary.
- Any terminal. No platform-specific code beyond Unix process-group handling for the `bash` tool.

## Installation

```bash
# from git
cargo install --git https://github.com/iicrom/codest

# or from a local checkout
git clone https://github.com/iicrom/codest
cd codest
cargo install --path .
```

This installs two equivalent commands:

```bash
codest   # primary command
cst      # short alias, identical CLI
```

## Quick start

### Against a real endpoint

```bash
export CST_API_KEY=sk-...
export CST_BASE_URL=https://api.openai.com/v1
export CST_MODEL=gpt-4o-mini            # optional; /model discovers the list when omitted
codest
```

or with explicit flags:

```bash
codest --api-key sk-... --base-url https://api.openai.com/v1 --model gpt-4o-mini
```

Notes:

- `--base-url` / `--api-key` are a **pair** (the single-endpoint path). Provide only one and codest still starts — it prompts for the other when you first send a message or open `/model`.
- `--base-url` always runs a one-off `untitled` instance, even once `~/.codest/providers.toml` exists: configured providers keep their endpoints and cached model lists, and stay selectable through `/model`. Only a lone `--api-key` still completes the selected instance in place (see [multi-provider.md](multi-provider.md)).
- Model discovery runs in the background at startup; its metadata (context window, effort levels) feeds the footer and `/model`.

### Fully offline

```bash
# terminal 1 — local OpenAI-compatible test server (Python stdlib only, no key needed)
python3 scripts/openai_test_server.py --port 8080

# terminal 2
codest --api-key test --base-url http://127.0.0.1:8080/v1 --model test-model
```

The test server streams SSE, serves `GET /models`, never validates keys, and turns message keywords (`@bash`, `@read`, `@write`, `@edit`, `@tool`) into real tool calls — enough to exercise the whole agent loop without a network.

You can also skip the server entirely: declare `protocol = "test"` on a provider instance to use the built-in in-process echo backend.

### Resuming

```bash
codest --continue    # activate the most recent saved session of this workspace (-c)
codest --resume      # same, but open the session picker first
```

## CLI reference

| Flag | Env | Description |
| --- | --- | --- |
| `--model <id>` | `CST_MODEL` | Model id to start with (the `model` half of a selection). |
| `--provider <name>` | `CST_PROVIDER` | Provider instance to start with. |
| `--base-url <url>` | `CST_BASE_URL` | OpenAI-compatible base URL (or a full `/chat/completions` URL). Runs its own `untitled` instance, leaving configured providers untouched. |
| `--api-key <key>` | `CST_API_KEY` | API key for the endpoint. Hidden from `--help` output. |
| `--system <text>` | — | Override the system prompt. |
| `--continue`, `-c` | — | Continue the most recent saved session of this workspace. |
| `--resume` | — | Load every saved session and open the session picker at startup. |
| `--profile <name>` | `CST_PROFILE` | Start with a profile from `~/.codest/profiles.toml`. |
| `--tui-mode <mode>` | `CST_TUI_MODE` | `fullscreen` (default) or `inline`; `regular` is an alias of `inline`. |

Additional env fallbacks: `OPENAI_API_KEY` / `OPENAI_BASE_URL` feed the synthesized single endpoint when the `CST_*` vars are unset.

### `codest plugin` subcommand

```bash
codest plugin install <path | --git <url> | <crates.io-name>>   # [--rev|--branch|--tag] [--symbol NAME]...
codest plugin uninstall <name>
codest plugin list
codest plugin build [--release | --install]
```

See [local-plugins.md](local-plugins.md) for what install actually does (it builds a wrapper crate outside this repository).

## Next steps

- [configuration.md](configuration.md) — `config.toml`, `providers.toml`, `profiles.toml`, session header, environment variables.
- [commands.md](commands.md) — every slash command and its behavior.
- [sessions.md](sessions.md) — rollout persistence, `--continue` / `--resume`, recovery.
- [tools.md](tools.md) — the `bash` and `fs` tools, per-session toggles, tool rounds.
- [tui.md](tui.md) — keybindings, fullscreen vs inline, markdown and thinking cards.
- [architecture.md](architecture.md) — the `Op`/`Event` bus, the session worker, the module map.

[← Back to the README](../README.md)
