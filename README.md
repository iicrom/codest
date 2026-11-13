# codest

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](https://github.com/iicrom/codest/pulls)

English | [简体中文](README.zh-CN.md)

**codest** (alias **`cst`**) is a minimal TUI chat agent distilled from the `tui` + `core` architecture of [OpenAI codex](https://github.com/openai/codex). It connects to any OpenAI Chat Completions-compatible endpoint and covers the essentials: streaming replies, tool calling, session persistence, and a compile-time static plugin system — as a single small Rust crate that is deliberately easy to read end to end.

codest is *not* a codex clone. It intentionally leaves out codex's operational machinery: no sandbox, no approval flow, no MCP, no image input. What it keeps is the architecture — an `Op`/`Event` message bus, a session worker, a streaming SSE client, rollout persistence, and a ratatui TUI — distilled into independently compilable modules.

```bash
cargo install --git https://github.com/iicrom/codest
codest    # or: cst
```

Highlights: multi-provider switching with `/model`, named profiles, `bash`/`pwsh` and `fs` tools with per-session toggles, resumable sessions with UUIDv7 ids, thinking and tool cards, Markdown replies with syntax highlighting, and two TUI modes (fullscreen / inline).

## Documentation

### Getting started

| Document | Contents | 中文 |
| --- | --- | --- |
| [Getting started](docs/getting-started.md) | Requirements, installation, quick start, CLI reference. | [中文](docs/zh/getting-started.md) |
| [Configuration](docs/configuration.md) | `~/.codest` files, `config.toml`, environment variables. | [中文](docs/zh/configuration.md) |
| [Commands and keybindings](docs/commands.md) | All slash commands and keyboard shortcuts. | [中文](docs/zh/commands.md) |

### Using codest

| Document | Contents | 中文 |
| --- | --- | --- |
| [Sessions](docs/sessions.md) | Multi-session workflow, rollout persistence, `/continue` recovery, session headers. | [中文](docs/zh/sessions.md) |
| [Tool calling](docs/tools.md) | The tool loop, built-in `bash`/`pwsh` and `fs` tools, `/tools` toggles, tool cards. | [中文](docs/zh/tools.md) |
| [Terminal UI](docs/tui.md) | Fullscreen vs inline, Markdown rendering, code highlighting, thinking cards. | [中文](docs/zh/tui.md) |

### Extending

| Document | Contents | 中文 |
| --- | --- | --- |
| [Plugins](docs/plugins.md) | Plugin contract, built-in plugins, writing your first plugin. | [中文](docs/zh/plugins.md) |
| [External plugins](docs/local-plugins.md) | `codest plugin install`, the out-of-tree wrapper crate, contract versioning. | [中文](docs/zh/local-plugins.md) |
| [Multi-provider](docs/multi-provider.md) | Provider instances, wire quirks, model discovery, `/provider`, custom protocols. | [中文](docs/zh/multi-provider.md) |
| [Profiles](docs/profiles.md) | Profile presets, selection precedence, assembly-time vs session-time values. | [中文](docs/zh/profiles.md) |

### Internals

| Document | Contents | 中文 |
| --- | --- | --- |
| [Architecture](docs/architecture.md) | Message flow, what was distilled from codex, project layout, extension points. | [中文](docs/zh/architecture.md) |
| [Development](docs/development.md) | Build, offline test suite, smoke scripts, known limitations, roadmap, contributing. | [中文](docs/zh/development.md) |
| [Design notes](docs/README.md#design-notes-chinese-only) | Deeper rationale, Chinese only: `/provider` wizard, session ids, `/continue`. | — |

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
