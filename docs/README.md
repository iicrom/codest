# codest documentation

Start with the main [README](../README.md) ([简体中文](../README.zh-CN.md)) for the overview.

All user-facing guides are maintained bilingually: English in `docs/`, Chinese mirrors in [`zh/`](zh/). Design notes are Chinese-only.

## Getting started

| Document | Contents | 中文 |
| --- | --- | --- |
| [getting-started.md](getting-started.md) | Requirements, installation, quick start, CLI reference, `codest plugin`. | [zh](zh/getting-started.md) |
| [configuration.md](configuration.md) | `~/.codest` files, `config.toml`, provider/profile examples, session header, environment variables. | [zh](zh/configuration.md) |
| [commands.md](commands.md) | Every slash command, the candidate popup, all keybindings. | [zh](zh/commands.md) |

## Using codest

| Document | Contents | 中文 |
| --- | --- | --- |
| [sessions.md](sessions.md) | Multi-session workflow, rollout persistence, resume flags, `/continue`, the `session-id` header. | [zh](zh/sessions.md) |
| [tools.md](tools.md) | The tool-call loop, built-in `bash`/`fs` tools, `/tools` toggles, tool cards, offline testing. | [zh](zh/tools.md) |
| [tui.md](tui.md) | Fullscreen vs inline, Markdown rendering subset, streaming commit rule, code highlighting, thinking cards. | [zh](zh/tui.md) |

## Extending

| Document | Contents | 中文 |
| --- | --- | --- |
| [plugins.md](plugins.md) | Plugin contract: commands, hooks, tools, protocols, effects; built-in plugins; writing your first plugin. | [zh](zh/plugins.md) |
| [local-plugins.md](local-plugins.md) | Installing external plugins (`codest plugin install`), the out-of-tree wrapper crate, contract versioning. | [zh](zh/local-plugins.md) |
| [multi-provider.md](multi-provider.md) | Provider instances and models, wire quirks, model discovery, `/model`, the `/provider` wizard, plugin-contributed protocols. | [zh](zh/multi-provider.md) |
| [profiles.md](profiles.md) | Profile presets, assembly-time vs session-time values, selection precedence. | [zh](zh/profiles.md) |

## Internals

| Document | Contents | 中文 |
| --- | --- | --- |
| [architecture.md](architecture.md) | The `Op`/`Event` message flow, what was distilled from codex, project layout, extension points. | [zh](zh/architecture.md) |
| [development.md](development.md) | Build and run, offline test suite, smoke scripts, known limitations, roadmap, contributing. | [zh](zh/development.md) |

## Design notes (Chinese only)

Deeper rationale documents kept as internal design notes in Chinese. They are still accurate; translations are welcome.

| Document | Contents |
| --- | --- |
| [provider.md](provider.md) | `/provider` wizard design: flows, probing, atomic write-back. |
| [session-id.md](session-id.md) | Session id (UUIDv7), eager-id / lazy-file split, the session header. |
| [session-continue.md](session-continue.md) | `/continue` recovery semantics: transcript-tail state, `interrupted` sealing. |
