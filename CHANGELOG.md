# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) with one addition of our own: **every branch merged into `main` rolls the minor version one step and appends a section here** (see [docs/development.md](docs/development.md#branch-merge-and-release-bookkeeping)). The `version` in `Cargo.toml` and the newest section header must always agree.

## [Unreleased]

### Fixed

- A multi-row mouse copy no longer carries the transcript's decoration into the clipboard: the block's common left chrome (card `┌ │ └` spine, the wrapped-row indent, markdown's `• ` / `▎ ` markers) is stripped, while relative indentation and the body's own `+`/`-` and number columns stay.

## [0.2.0] — 2026-11-15

Merge of `feat/plan-plugin` (`e539881`), plus the release bookkeeping this changelog introduces.

### Added

- `/plan` as an external plugin example: plugin-contributed tool sets, and a system-prompt appendix that plugin sections can extend.

### Internal

- The post-merge bookkeeping rule: changelog section, minor version bump, `v*` tag. Backfilled this file from the history below.
- The two `/status` plugin-listing assertions now expect the host's own shell tool name instead of a hardcoded `bash`, so the suite passes on Windows.

## [0.1.0] — 2026-11-15

The development line from `54d1c4a` (`chore: initial commit`) through `3f23ae5` (`merge: feat/mouse-selection-and-shortcut-registry into main`), collected retroactively: no tags were cut while this work landed, so it all sits under one heading. The release links below therefore point at commit ranges rather than at tags.

### Added

- Tool-calling agent loop: a turn keeps calling tools until the model stops; built-in `fs` tools plus a shell tool (`pwsh` on Windows hosts, `bash` elsewhere), each behind a per-session `/tools` switch.
- Slash command system with a live candidate popup and completion.
- Compile-time static plugin system: commands, hooks, tools, protocols, effects, `/settings` screens that plugins can extend, plugin event streams with status segments and background tasks.
- External provider plugin contracts and `codest plugin install` / `uninstall` for out-of-tree wrapper crates.
- Multi-session support with rollout persistence, `/continue` to resume an interrupted turn without a new message, and a UUIDv7 session id sent as a session header.
- Multi-provider support: provider instances and models, model discovery, `/model`, the `/provider` wizard, profile presets and provider wire quirks.
- Context window tracking with usage readout, a `/context` prompt viewer with per-message delete, edit, undo and per-call delete in the drill-down, and a `/usage` report with cache hit rate.
- Reasoning display with `/effort` and `/thinking`; card-style thinking and tool cells with folding, a thinking preview while folded, and restoration of thinking cards on resume.
- Markdown rendering of assistant replies, syntax highlighting for fenced code, and diffs for file tools.
- `ask_user_question` built-in plugin, rendered in the TUI with a custom-answer editor.
- Mid-turn queueing and steering, floating `/status` and `/usage` windows, and next-turn model switching.
- `Tab`-cycled effort and a `Ctrl+S` startup default in `/model`, double-press interrupt confirm.
- Mouse text selection with copy-on-release, a shortcut registry with key-capture rebinding, and `Ctrl+End` jump to the newest line.

### Changed

- Package renamed to `codest`; the `cst` binary remains as an alias.
- `/settings` and `/context` now share one full-frame screen implementation.
- TUI render pipeline reworked for speed: cached per-card renders, coalesced frames, tokenizing only the cards the viewport shows, and syntect switched to the oniguruma engine.
- Documentation rewritten bilingually for the open-source release.

### Fixed

- The footer reports the `Ctrl+O` toggle instead of the transcript, and drops whole hints rather than clipping mid-word.
- `--base-url` gets its own route, the model cache is isolated per instance, and the selected effort is restored.
- `↑`/`↓` and `Tab` walk out of the custom-answer editor.
- The tool-card body is drawn with a single box-drawing set.
- The test server's usage tracks the conversation and reports cached tokens.

### Internal

- Open-source release preparation and an offline test suite: unit tests colocated with the code, plus `pty` / markdown / tool smoke scripts driven against a local test server.

[Unreleased]: https://github.com/iicrom/codest/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/iicrom/codest/compare/3f23ae5...v0.2.0
[0.1.0]: https://github.com/iicrom/codest/compare/54d1c4a...3f23ae5
