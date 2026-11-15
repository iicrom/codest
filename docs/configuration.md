# Configuration

English | [简体中文](zh/configuration.md)

Everything codest stores lives under `~/.codest` (override the root with `CODEST_HOME`). All files are optional; a file that exists but fails to parse — including unknown fields — is a startup error. Secret values accept a `$VAR` template so keys never have to be written to disk. This page is the file-and-key reference; the deep dives for provider instances and profiles are linked, not duplicated, here.

## Configuration files

| Path | Purpose |
| --- | --- |
| `config.toml` | Global settings: `default = "provider/model"`, `max_tool_rounds`, the `[status_bar]` footer table, per-plugin `[plugins."<id>"]` pockets. |
| `providers.toml` | Provider instances (endpoints, keys, models). |
| `profiles.toml` | Named session presets. |
| `plugins.toml` | Installed external plugins (machine-managed). |
| `model-cache.json` | Cache of discovered model lists. |
| `sessions/<workspace-hash>/rollout-<uuid>.jsonl` | Session rollouts (append-only JSONL). |

Parsing is strict everywhere (`deny_unknown_fields`): a typo is a startup error naming the file, rather than a silently disabled setting. `$VAR` templates are expanded at use time and trimmed; an unset or empty variable resolves to nothing, which fails the turn — never startup.

Three of the files are machine-managed and you normally do not edit them by hand:

| File | Written by | Consequence |
| --- | --- | --- |
| `plugins.toml` | `codest plugin install` / `uninstall` | Holds the external plugin list plus the codest checkout the wrapper crate links against. |
| `model-cache.json` | Startup discovery | Preloaded so `/model` is instant and a failed refresh never empties the menu. Each entry records the endpoint it came from and is used only while that endpoint is still the configured one; entries for providers that are no longer configured are pruned on write. A file from an older codest (untagged entries) cannot be validated and is refetched once. |
| `providers.toml` | `/provider` wizard (only on confirm) | A wizard write is atomic but machine-formatted: hand comments and layout are lost. |

Only `config.toml`, `providers.toml`, and `profiles.toml` are meant to be hand-edited, and each is optional — a missing file simply contributes nothing.

## `config.toml`

The global file holds top-level keys plus three tables:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | string | — | Initial model selection, `"provider/model"`. |
| `default_effort` | string | — | Initial reasoning effort for fresh sessions (`off` included). Set by `Ctrl+S` in the `/model` menu. |
| `double_press_interrupt` | boolean | `true` | While a turn runs, `Esc`/`Ctrl+C` must be pressed twice to interrupt; the first press only shows a hint. |
| `max_tool_rounds` | integer | absent | Optional cap on model → tool → model rounds per user turn. |
| `tool_output_token_limit` | integer | `4000` | Per-output tool result budget, in estimated tokens: an oversized result enters the conversation as head + a truncation notice + tail; the rollout file and the tool card keep the full output. `0` disables the cut. |
| `[status_bar]` | table | segments on | The footer: `plugin_segments` master switch and `order` display priority for plugin segments, plus the `context` / `cache_rate` toggles for the host's context readout. |
| `[shortcuts]` | table | — | Keyboard shortcut overrides; see the section below. |
| `[plugins."<id>"]` | table | — | A plugin's private settings pocket; the host interprets no key. |

```toml
# ~/.codest/config.toml
default = "openai/gpt-4o-mini"
default_effort = "high"               # Ctrl+S in /model writes both keys
# double_press_interrupt = true
# max_tool_rounds = 64
# tool_output_token_limit = 4000     # per-output tool result budget (0 = off)

[status_bar]
plugin_segments = true                # plugin segments in the footer (default on)
order = ["tps:rate", "tps:spark"]     # display priority, left to right

[shortcuts]                           # rebind registered shortcuts (all optional)
thinking = "ctrl+t, alt+t"            # alternatives are comma-separated
tool_detail = "ctrl+o"
page_up = ""                          # empty value = disable outright
"tps:meter" = "ctrl+g"                # a plugin's shortcut is scoped plugin:id

[plugins.tps]                         # the built-in tps meter's pocket
enabled = true                        # start with the meter on (default off)
```

`max_tool_rounds` is **unbounded by default**, following codex: the turn ends when the model stops calling tools, errors, or you press `Esc`. Set a value to re-enable the runaway guard — the turn then ends with a `tool round limit reached` error once that many rounds have completed. `64` is the commonly used guardrail.

`tool_output_token_limit` is the per-output budget of the live conversation: the estimate is the same script-weighted count `/usage` renders (ASCII ≈ 4 chars/token, CJK ≈ 1.4). Like codex, the rollout file always stores the full payload and the tool card shows it whole — only what the model sees is trimmed, at record time and again on replay, so a restored session carries the same shape.

`default` is the third step of the startup selection chain: `--provider`/`--model` > active profile's `default` > `config.toml` `default` > first model of first instance. It names one model of one instance as `"provider/model"`, parsed at the first slash, so model ids may themselves contain `/`.

`default_effort` sits at the matching step of the effort chain: a restored session restores its own recorded effort, else the active profile's `effort` wins, else `default_effort`, and only then does the model's `default_level` seed it. `Ctrl+S` in the `/model` menu writes it together with `default`.

**`[status_bar]`** controls the footer's optional pieces. `plugin_segments` is the plugin side's master switch, and `order` lists full segment keys (`plugin:key`) in display priority; unlisted segments keep their registration order, and keys nothing currently displays are ignored, so a stale entry costs nothing. On a narrow terminal the lowest-priority segments drop first. The host's own context readout has two toggles of its own: `context` (the `used/window (n%)` segment; off leaves the model · effort · state skeleton) and `cache_rate` (the `cache 80%` part inside it, shown only while the backend reports cached prompt tokens). Both default to on and are editable live in `/settings`.

**`[shortcuts]`** rebinds keyboard shortcuts. Each key names a registered shortcut — the host's own ids are bare (`thinking`, `tool_detail`, `page_up`, `page_down`, `jump_bottom`, `history_prev`, `history_next`, `quit`) and a plugin's are scoped `plugin:id` (`tps:meter`) — and the value is a comma-separated combo list (`"ctrl+t, alt+t"`); an **empty** value (`thinking = ""`) disables the shortcut outright — no key fires it — while an absent key runs the registered default. A character key must carry `ctrl` or `alt`, `Esc`, `Enter`, `Tab`, and the composer's editing keys are reserved and refused, and a combo another shortcut already binds is an error. A shortcut registered with a lock (`fixed`) cannot be rebound at all. Validation is strict, like the rest of this file: an unknown id, a malformed combo, or a collision fails startup with the offending key named. The same edits are available — and pre-validated — on the dedicated **Shortcuts** card in `/settings`, which carries one row per registered shortcut, host actions and plugin registrations alike (a plugin's row is labeled with its owner, `Toggle tps meter (tps)`); a plugin that registers a shortcut is rebinding-ready with no extra work. Rebinding is by capture, not typing: `Enter` on a row arms it, the next keypress becomes the combo, and further presses before `Enter` add alternatives; a rejected key (reserved, a bare character, already bound) never enters the capture, and `Esc` cancels. `Backspace` commits an **empty** binding, which **disables** the shortcut outright — no key fires it, and the row reads `(disabled)` — distinct from removing the `[shortcuts]` key by hand, which runs the registered default again. To return to the default binding, capture the default combo.

**`[plugins."<id>"]`** is the per-plugin settings pocket — the whole table is handed to the plugin verbatim ([plugins.md](plugins.md), section 6); a plugin reads it through `PluginContext::plugin_settings()`. A table naming an unknown plugin id is a **startup error** (a silent no-op would hide a typo), so remove the table when you uninstall a plugin.

### `/settings`

/settings is the screen that edits this file: one card for the host's own options (`[status_bar] plugin_segments`, `[status_bar] context`, `[status_bar] cache_rate`, `double_press_interrupt`, and every registered keyboard shortcut — host actions first) and one card per plugin that declared settings or registered shortcuts, each row showing the stored value or the default. `↑`/`↓` walk the rows, `Enter` flips a switch / advances a list / opens a one-line editor (a Shortcuts row captures the pressed combo instead), `←`/`→` step a list (and a switch) backwards/forwards, `Esc` closes; `/settings tps` opens with that card focused.

```text
 Settings › tps                                            1/3
 codest  ·  Shortcuts  ·  tps
╭─ codest ─ host settings ─────────────────────────────────╮
│ ❯ Plugin segments                                  [x]   │
╰──────────────────────────────────────────────────────────╯
╭─ Shortcuts ─ keyboard shortcuts ─────────────────────────╮
│   Thinking cards                              ctrl+t     │
│   … (one row per registered shortcut)                     │
│   Toggle tps meter (tps)                      ctrl+g     │
╰──────────────────────────────────────────────────────────╯
╭─ tps ─ throughput meter ─────────────────────────────────╮
│   Tokens-per-second meter                          [x]   │
╰──────────────────────────────────────────────────────────╯
```

Every edit is written immediately — there is no unsaved-draft state — and a value lands in exactly the table it came from: a host row in its dotted `config.toml` path, a plugin row in that plugin's `[plugins."<id>"]` pocket. Three consequences are worth knowing:

* **The write is validated and atomic.** The rewritten document is parsed back into the strict `GlobalConfig` first; anything it would reject is refused with the reason in the notice line, and the file is replaced through a temporary sibling, so a failure never leaves a half-written `config.toml`.
* **Comments and hand formatting are lost on the first save.** The file becomes machine-managed from then on, exactly like a `/provider` wizard write.
* **Clearing a text row removes the key**, restoring the plugin's default; toggling a switch writes `true`/`false` explicitly. Most edits apply to the running session immediately (the plugin is told through `Plugin::on_settings_changed`); a row declared startup-only carries a `⟲` marker (the help bar adds `restart to apply`) and persists without applying — the host skips the hook for it.

`/tps` remains a keyboard path for the same meter, as is its registered `tps:meter` shortcut: both flip this run only and never rewrite the file. `/settings` is where the choice is recorded.

## Provider instances

`providers.toml` is optional. A missing file contributes no instances; if flags or env then name an endpoint, codest synthesizes a single `openai-compat` instance for that run, and with nothing at all the route is empty and the TUI guides you to add a provider.

```toml
# ~/.codest/providers.toml
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"        # $VAR template — the key stays in your environment
fetch_models = true                # discover the model list from GET /models

  [[providers.models]]             # or declare models explicitly (takes precedence)
  id = "gpt-4o-mini"

[[providers]]
name = "local"
base_url = "http://127.0.0.1:8080/v1"
api_key = "test"
headers = { "X-Custom" = "v" }     # static headers / body overrides: most "special endpoints"
body_overrides = { temperature = 0.5 }   # need no plugin at all

  [providers.wire]                 # field-spelling-level quirks
  thinking_fields = ["reasoning"]  # thinking text field (default: reasoning_content)
  effort_field = "none"            # request field for /effort; "none" = never send it
```

See [multi-provider.md](multi-provider.md) for the full field reference: every `[[providers]]` and `[[providers.models]]` key, `$VAR` templates, discovery and the `/model` menu, the `/provider` wizard, and the `wire` quirks table.

## Profiles

```toml
# ~/.codest/profiles.toml — named presets of model + effort + tools + prompt
[profiles.code]
default = "openai/gpt-4o-mini"     # initial selection
effort = "high"                    # initial /effort ("off" = never send the effort field)
tools = ["fs"]                     # plugin ids; absent = all tools, [] = none

  [profiles.code.prompt]
  append = "You are a senior Rust engineer..."   # appended to the system prompt

[profiles.fast]
default = "local/qwen2.5"
effort = "low"
```

Profiles are *presets*, not a second config layer: CLI flags still win, and tool/prompt bindings are assembly-time (applied at startup only). Startup selection precedence: `--provider`/`--model` > active profile's `default` > `config.toml` `default` > first model of first instance.

See [profiles.md](profiles.md) for the full field reference, lifecycle semantics (session-time vs assembly-time), the tools binding, and effort handling.

## Session header

Endpoints that route or cache per session recognize the `session-id` header (codex's spelling). `wire.session_header` (default `"session-id"`) controls it per instance — `"none"` opts out, and a custom spelling is used verbatim:

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

A model-level `session_header` in `[[providers.models]]` overrides the instance's, including `"none"`. The header is applied after static `headers`, so the live session id wins over a hand-written same-name entry; the value sent is the active session's UUIDv7.

## Environment variables

| Variable | Meaning |
| --- | --- |
| `CST_MODEL` | Model id to start with (the `model` half of a selection). |
| `CST_PROVIDER` | Provider instance to start with. |
| `CST_BASE_URL` | OpenAI-compatible base URL, or a full `/chat/completions` URL. Runs its own `untitled` instance instead of rewriting a configured one. |
| `CST_API_KEY` | API key for the endpoint; overrides the selected instance. |
| `CST_PROFILE` | Profile from `~/.codest/profiles.toml` to start with. |
| `CST_TUI_MODE` | `fullscreen` (default) or `inline`. |
| `CODEST_HOME` | Root directory for all state, replacing `~/.codest`. |
| `CODEST_SRC` | codest checkout used by `codest plugin install`/`build` to link the wrapper crate against. |
| `OPENAI_API_KEY` | Fallback for `CST_API_KEY` when the synthesized single endpoint is in use. |
| `OPENAI_BASE_URL` | Fallback for `CST_BASE_URL`, consulted only when `providers.toml` is empty — exporting it never re-points a configured instance. |

`CODEST_SRC` is consulted by the installer after an explicit `--codest-src` and before the directory the binary was compiled from and the path recorded in `plugins.toml`; each candidate is verified to be a codest checkout, and a stale one degrades to the next. See [local-plugins.md](local-plugins.md).

## Next steps

- [getting-started.md](getting-started.md) — installation, quick start, the full CLI table.
- [multi-provider.md](multi-provider.md) — provider instances, wire quirks, discovery, the `/provider` wizard.
- [profiles.md](profiles.md) — presets, selection precedence, assembly-time vs session-time values.
- [local-plugins.md](local-plugins.md) — installed plugins and the wrapper crate.

[← Back to the README](../README.md)
