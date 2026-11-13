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

The global file holds two top-level keys plus two tables:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | string | — | Initial model selection, `"provider/model"`. |
| `max_tool_rounds` | integer | absent | Optional cap on model → tool → model rounds per user turn. |
| `[status_bar]` | table | segments on | The footer: `plugin_segments` master switch and `order` display priority for plugin segments. |
| `[plugins."<id>"]` | table | — | A plugin's private settings pocket; the host interprets no key. |

```toml
# ~/.codest/config.toml
default = "openai/gpt-4o-mini"
# max_tool_rounds = 64

[status_bar]
plugin_segments = true                # plugin segments in the footer (default on)
order = ["tps:rate", "tps:spark"]     # display priority, left to right

[plugins.tps]                         # the built-in tps meter's pocket
enabled = true                        # start with the meter on (default off)
```

`max_tool_rounds` is **unbounded by default**, following codex: the turn ends when the model stops calling tools, errors, or you press `Esc`. Set a value to re-enable the runaway guard — the turn then ends with a `tool round limit reached` error once that many rounds have completed. `64` is the commonly used guardrail.

`default` is the third step of the startup selection chain: `--provider`/`--model` > active profile's `default` > `config.toml` `default` > first model of first instance. It names one model of one instance as `"provider/model"`, parsed at the first slash, so model ids may themselves contain `/`.

**`[status_bar]`** controls only the plugin side of the footer — the host's own segments (model, effort, context, state) always render. `order` lists full segment keys (`plugin:key`) in display priority; unlisted segments keep their registration order, and keys nothing currently displays are ignored, so a stale entry costs nothing. On a narrow terminal the lowest-priority segments drop first.

**`[plugins."<id>"]`** is the per-plugin settings pocket — the whole table is handed to the plugin verbatim ([plugins.md](plugins.md), section 6); a plugin reads it through `PluginContext::plugin_settings()`. A table naming an unknown plugin id is a **startup error** (a silent no-op would hide a typo), so remove the table when you uninstall a plugin.

### `/settings`

`/settings` is the screen that edits this file: one card for the host's own options (`[status_bar] plugin_segments` today) and one card per plugin that declared settings, each row showing the stored value or the default. `↑`/`↓` walk the rows, `Enter` flips a switch / advances a list / opens a one-line editor, `←`/`→` step a list (and a switch) backwards/forwards, `Esc` closes; `/settings tps` opens with that card focused.

```text
 Settings › tps                                            3/4
 codest  ·  tps
╭─ codest ─ host settings ─────────────────────────────────╮
│ ❯ Plugin segments                                  [x]   │
╰──────────────────────────────────────────────────────────╯
╭─ tps ─ throughput meter ─────────────────────────────────╮
│   Tokens-per-second meter                          [x]   │
╰──────────────────────────────────────────────────────────╯
```

Every edit is written immediately — there is no unsaved-draft state — and a value lands in exactly the table it came from: a host row in its dotted `config.toml` path, a plugin row in that plugin's `[plugins."<id>"]` pocket. Three consequences are worth knowing:

* **The write is validated and atomic.** The rewritten document is parsed back into the strict `GlobalConfig` first; anything it would reject is refused with the reason in the notice line, and the file is replaced through a temporary sibling, so a failure never leaves a half-written `config.toml`.
* **Comments and hand formatting are lost on the first save.** The file becomes machine-managed from then on, exactly like a `/provider` wizard write.
* **Clearing a text row removes the key**, restoring the plugin's default; toggling a switch writes `true`/`false` explicitly. Most edits apply to the running session immediately (the plugin is told through `Plugin::on_settings_changed`); a row declared startup-only carries a `restart` badge and persists without applying.

`/tps` remains the keyboard shortcut for the same meter: it flips this run only and never rewrites the file. `/settings` is where the choice is recorded.

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
