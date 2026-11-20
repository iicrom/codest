# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) with one addition of our own: **every branch merged into `main` rolls the minor version one step and appends a section here** (see [docs/development.md](docs/development.md#branch-merge-and-release-bookkeeping)). The `version` in `Cargo.toml` and the newest section header must always agree.

## [Unreleased]

## [0.4.2] — 2026-11-20

Hotfix on `main` (`4c48a9c`), no merge: the image-input gate shipped in 0.4.1
could not see the capability on the endpoints that spell it as MIME types.

### Fixed

- Models discovery reads image input from `modalities.input` too, and a modality entry counts when it is the bare `image` **or** any `image/…` type (`image/png`, `image/jpeg`, case-insensitive). A directory that enumerates the accepted file types — what an OpenRouter/opencode-shaped gateway returns — matched no spelling 0.4.1 knew, so every vision model there was read as text-only and lost `read_image` to the gate. The boolean spellings keep precedence, so an explicit `capabilities.vision = false` still wins over a list, and a list with no image type is still a "no".
- `scripts/image_gate_smoke.py` covers both discovery spellings as wire-level cases (`capabilities.vision`, and a MIME-only `modalities.input`), and the test server serves both shapes.

## [0.4.1] — 2026-11-19

`read_image` now reaches only the models that can read it. A model's image
input is a declared capability, and the host enforces it rather than trusting
the endpoint to reject what it cannot use.

### Added

- Image-input gating: `read_image` is offered only to models that accept image input. A model declares it with `vision = true` in `providers.toml`, or the host reads it from models discovery (`capabilities.vision`, `supports_vision`, or an accepted-input list — `architecture.input_modalities`, `input_modalities`, `modalities.input` — naming `image` or any `image/…` MIME type, the spelling a directory that enumerates file types returns). An undeclared model counts as text-only — silence is not a claim, so the gate fails closed.
- `ToolSpec::requires_image_input` and `PluginInfo::image_input`, so a plugin can mark a tool that needs vision and explain its availability.
- `scripts/image_gate_smoke.py`: a wire-level check that the declared `tools` array carries `read_image` for a vision model and withholds it otherwise. The test server's `--dump-headers` now records the request body alongside the headers.

### Changed

- The worker enforces the gate in three places: it leaves such a tool out of the request's `tools`, rejects a call for it before the executor runs (naming `/model` as the fix), and strips the transcript's attached images from every request to a text-only model. The conversation and the rollout keep the images, so switching back to a vision model restores them.
- `/tools` shows an image tool as `[-] · needs a vision model` on a text-only model; Space does not toggle it and `/tools <name> on` explains why. `/context` reports it as `unavailable` and excludes it from the enabled count.

### Internal

- `ToolSpec` gained a required field, so `PLUGIN_API_VERSION` is now `3` and `cst-plugin-api` is `0.3`; a plugin built against the previous contract fails at compile time rather than starting without the field.

## [0.4.0] — 2026-11-18

Merge of `feat/read-image-tool` (`d9db34b`) and `feat/pending-queue-control` (`566dc52`): tool results can carry images, and the TUI owns the queue of messages typed mid-turn.

### Added

- `read_image` tool (`fs` plugin): loads a local PNG/JPEG/GIF/WebP, downscales it to the model's image patch budget (codex's `view_image` policy — 2048 px / 2500 patches by default, `detail: "original"` up to 6000 px / 10 000 patches), and attaches the prepared bytes to the tool result. The tool is `read_only`, appears in `/tools`, and is recorded in rollouts so a resumed session keeps its images.
- Plugin contract: `ToolOutput { text, images }`, `ToolImage`, and the defaulted `Plugin::run_tool_output` / `ToolExecutor::execute_output` pair, so a tool can return media without breaking text-only plugins.
- Queued-message management: `Ctrl+R` focuses the queue (`↑↓` select, `Enter` takes the message back into the composer, `Delete` withdraws it, `Alt+Enter` steers the selection, `Esc` closes), and `Alt+S` releases the whole held queue — steered into a running turn, or, when idle, the head starting a turn with the rest following one per turn. Both are host shortcuts, rebindable in `/settings`.
- `scripts/openai_test_server.py` gained an `@read-image <path> [high|original]` keyword, and looks past the image-carrying follow-up message when it decides which turn a request belongs to.

### Fixed

- `Esc`/`Ctrl+C` now really stops the conversation: queued messages are held (visible and editable above the composer) instead of being submitted straight into the worker's FIFO, which ran them as the next turn the moment the abort settled. A steer the turn never claimed is likewise held rather than resubmitted.
- A tool result's `detail` is the tool's budget label (`high`/`original`), not the wire's vocabulary: the client sends `original` as `high`, the only larger-than-low value chat-completions accepts, instead of an enum value endpoints reject.
- Attached images no longer accumulate across a whole session's requests: the newest 4 MB (decoded) ride a request and older ones stop being re-sent, while their text results and the rollout keep them.
- The image decoder refuses a canvas past 16384 px per side before allocating it, so a small file declaring a huge one cannot be used as a decompression bomb.
- `/context` reported an attachment's size as its base64 encoding, 33% above the file's own bytes; it now shows the decoded size, matching the tool's result line.
- Idle `Alt+S` starts one turn and holds the rest, but flashed "sent N queued messages"; it now reports what it started. A steer claim also retires the row its text names, so identical steers cannot be retired out of order.

### Internal

- The chat-completions client sends a tool result's images as a follow-up `user` message with `image_url` parts (`role: "tool"` carries text only), the shape gateway adapters accept. The carrier follows the whole run of tool results in a round, so a parallel round's `tool_call_id`s stay answered back to back.
- `image` and `base64` dependencies (image with only the GIF/JPEG/PNG/WebP codecs compiled in).
- Corrected the documented `PLUGIN_API_VERSION`, which had said `1` since the contract moved to `2`.

## [0.3.1] — 2026-11-17

Hotfix on `main` (`a9cd72e`), no merge — the patch release the release bookkeeping reserves for one.

### Fixed

- An error cell no longer copies with its `! ` gutter. `!` was missing from the chrome set, so a copied error message carried `! ` down the left of every row — the one cell kind whose gutter survived the copy cleanup.

## [0.3.0] — 2026-11-16

Merge of `fix/copy-strips-decoration` (`f681b98`): the mouse-copy cleanup and the `pwsh` tool's lost-output fix.

### Fixed

- A multi-row mouse copy no longer carries the transcript's decoration into the clipboard: the block's common left chrome (card `┌ │ └` spine, the wrapped-row indent, markdown's `• ` / `▎ ` markers) is stripped, while relative indentation and the body's own `+`/`-` and number columns stay.
- The `pwsh` tool no longer swallows table-shaped output. The epilogue ended with `exit 0`, which tears the runspace down before PowerShell's implicit formatting pass writes the pending objects, so `Get-ChildItem | Select-Object Mode,Name` — the usual first call of a session — reached the model as a bare `[exit code: 0]`. The epilogue now sets the code with `$host.SetShouldExit(...)`, which records the process exit code and still lets the formatter flush; string and `Out-String` pipelines were never affected, which is why only tables looked empty.

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

[Unreleased]: https://github.com/iicrom/codest/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/iicrom/codest/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/iicrom/codest/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/iicrom/codest/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/iicrom/codest/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/iicrom/codest/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/iicrom/codest/compare/3f23ae5...v0.2.0
[0.1.0]: https://github.com/iicrom/codest/compare/54d1c4a...3f23ae5
