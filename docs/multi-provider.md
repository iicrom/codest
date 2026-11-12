# Multi-provider framework

English | [简体中文](zh/multi-provider.md)

codest talks to any number of model endpoints at once: what you can
configure, what the TUI commands do, and how a contributor adds a wire
protocol.

## 1. Overview

Two concepts, deliberately separated:

| Concept | What it is | Where it lives |
| --- | --- | --- |
| **Protocol kind** | A compile-time `ProtocolFactory` — a `'static` value that knows one wire protocol and builds a client from instance config | Code: built-ins `openai-compat` and `test`, plus plugin-contributed factories |
| **Provider instance** | Pure configuration data naming a protocol kind by its `protocol` id | `~/.codest/providers.toml` (or the `/provider` wizard) |

- **A "custom endpoint" is not a special concept.** It is an `openai-compat`
  instance whose `base_url` points somewhere else.
- **Most "special endpoints" need no code at all.** `headers` covers
  disguises and auth variants, `body_overrides` fixes request-body fields,
  `wire` quirks rename the field spellings providers disagree on (Section
  5), and a different wire protocol is a plugin `ProtocolFactory` (Section
  6). All of it ends behind one trait, `ModelClient` — the worker only
  knows `stream_chat`; auth complexity stays inside `create()`.
- A **selection** names one model of one instance: `"provider/model"`,
  parsed at the *first* slash, so model ids may themselves contain `/`
  (e.g. `gateway/deepseek/deepseek-r1`).

## 2. Configuration reference

All files live under `~/.codest` (override the root with `CODEST_HOME`).
Every file is optional; a file that **exists but fails to parse — including
unknown fields** (`deny_unknown_fields`) — is a startup error naming the
file. Global settings live in `config.toml` (`default`, `max_tool_rounds`),
the instances below in `providers.toml`; `model-cache.json` is machine-managed.

### 2.1 `[[providers]]`

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | Unique instance name; also the `provider` half of a selection. Must not contain `/` and must not repeat. |
| `protocol` | string | `"openai-compat"` | Protocol kind id. Built-ins: `openai-compat`, `test`; plugins contribute more. |
| `base_url` | string | — | API root. Requests go to `{base_url}/chat/completions`; discovery to `{base_url}/models`. |
| `api_key` | string | — | Literal secret or a `$VAR` template (2.3). Unresolved, it fails the turn — never startup. |
| `models` | array of tables | `[]` | Explicit model declarations (2.2). |
| `fetch_models` | bool | `true` when no `models` are declared, `false` otherwise | Discover the list from the models API (Section 3). |
| `headers` | table | `{}` | Static headers sent with every request. |
| `body_overrides` | table | `{}` | JSON fields merged over the request body; overrides win over standard fields. |
| `wire` | table | — | `openai-compat` wire quirks (Section 5). |
| `options` | table | `{}` | Protocol-private pocket, handed to the factory verbatim (`option_str` / `option_u64` / `option_bool`). Unknown *top-level* keys are still rejected. |

### 2.2 `[[providers.models]]`

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `id` | string | required | Model id sent to the endpoint; unique within the instance, non-empty. |
| `name` | string | — | Display name (menus, status). |
| `context_window` | integer | — | Advisory: footer usage readout and `/model` rows. |
| `reasoning` | string array | — | Effort levels the model accepts, **lowest first**; feeds `/effort` (Section 5). |
| `default_level` | string | — | Effort adopted when this model is selected, until `/effort` overrides it. |
| `session_header` | string | — | Per-model override of `wire.session_header`; `"none"` sends no header for this model. |

Declared entries win over discovered twins, but fields a declaration leaves
unset are filled from discovery — a sparse `id`-only entry still gets the
context window the models API reports.

### 2.3 `$VAR` secret templates

A value starting with `$` names an environment variable, expanded at use
time and trimmed; a literal passes through. Unset or empty variables
resolve to nothing — the turn then fails with guidance; startup proceeds.

### 2.4 Flags, env, and startup rules

Flags and env: `--provider` / `CST_PROVIDER` (instance to start with),
`--model` / `CST_MODEL` (model id), `--base-url` / `CST_BASE_URL`
(OpenAI-compatible base URL, or a full `/chat/completions` URL), and
`--api-key` / `CST_API_KEY` (hidden in `--help`; `OPENAI_API_KEY` and
`OPENAI_BASE_URL` are further fallbacks).

- **No built-in default provider.** Only when `providers.toml` is
  empty/missing **and** flags/env actually name an endpoint does codest
  synthesize a single `openai-compat` instance (named `untitled`); with
  nothing at all the route is empty and the TUI guides you to add one.
- `--base-url` and `--api-key` are a **pair**: either alone synthesizes an
  *incomplete* instance gated at first use with a prompt naming the missing
  half, instead of sending a doomed request.
- A synthesized instance never invents a model id: without `--model` the
  list comes from discovery and `/model` picks one; with `--model` the
  selection is pinned but discovery still fills the menu. With
  `providers.toml` present, the flags **override the selected instance**.
- Validation is strict: unknown fields, duplicate names, a name containing
  `/`, an unknown `protocol` id, or empty/duplicate model ids all fail
  startup (the protocol error lists the known ids). An unknown **provider**
  in the selection is a hard error; an undeclared **model id** proceeds
  with a warning — the endpoint answers for whether a model exists.

### 2.5 Annotated example

```toml
# ~/.codest/providers.toml
[[providers]]
name = "openai"                        # unique, no '/' — the provider half of "openai/gpt-4o-mini"
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"            # $VAR template: the key stays in your environment
fetch_models = true                    # list comes from GET /models
  [[providers.models]]                 # explicit declarations take precedence...
  id = "gpt-4o-mini"
  context_window = 128000              # footer usage + /model rows
  reasoning = ["low", "medium", "high"]  # /effort vocabulary, lowest first
  default_level = "medium"             # adopted on selection until /effort overrides

[[providers]]
name = "local"                         # a keyless local server needs no plugin
base_url = "http://127.0.0.1:8080/v1"
headers = { "X-Custom-Header" = "value" }      # static headers
body_overrides = { temperature = 0.5 }          # merged over the request body
wire = { effort_field = "none" }                # endpoint rejects unknown request fields
  [[providers.models]]
  id = "qwen2.5"
  session_header = "none"                       # per-model override: no session header for this model
```

## 3. Model discovery and the `/model` menu

- At startup the last discovered lists are preloaded from
  `~/.codest/model-cache.json`, so the menu is instant and a failed refresh
  never empties it. `fetch_models` instances are then fetched in the
  background: `GET {base_url}/models` (OpenAI shape `{data: [{id, ...}]}`),
  entries without an id skipped, duplicates collapsed, 5-second timeout.
  Success and failure each produce one transcript notice — never a fatal
  error — and success writes the cache back. Both context-window spellings
  in the wild are accepted: `context_length` (OpenRouter) and
  `context_window`.
- The merged view per instance: declared models first (declaration order),
  then discovered models the config does not already declare; declared
  entries keep precedence but unset fields are filled from discovery.
- `/model` opens a picker with one row per `provider/model` selection —
  models of the same instance stay together, in declaration order — each
  row showing the context window when known and a marker on the active
  model. Keys: `↑`/`↓` move, `Enter` switch, `r` re-run discovery for the
  selected item's provider, `Esc` close. A switch applies at the **turn
  boundary** (ops are FIFO, never interrupting a stream): the footer label
  updates optimistically, the worker's `ModelSwitched` event is the
  authoritative confirmation, and a failed switch keeps the current model.
  Clients are built per turn, not cached: switching changes what the next
  turn's client reads, and the session id is never baked into a client.

## 4. The `/provider` wizard

`/provider` opens a wizard with three flows:

1. **Add an OpenAI-compatible endpoint.** Name → base URL → API key (`$VAR`
   template or literal; empty means keyless) → model ids (comma-separated;
   empty means discover them) → a one-shot background probe of `GET /models`
   (failure is not fatal; a note on confirm) → confirm. The instance is
   written to `providers.toml` and installed into the pool, selectable.
2. **Sign in.** Listed only when a plugin contributes sign-in entries.
   Pick a target (a configured instance of the plugin's protocol, or the
   plugin's default route) and a method — browser authorization or a secret
   prompt (e.g. a PAT); `Esc` cancels. Success on a not-yet-configured
   route installs the plugin's defaults; the credential lives in the
   plugin's own store.
3. **Configured providers.** A read-only list: name, protocol, base URL,
   model count, sign-in badge.

Write-back: the new instance set is validated exactly like startup, then
written **atomically** (temporary sibling file + rename). One consequence:
**comments and layout are lost** — there is no `toml_edit` dependency, so
after the first wizard write the file is machine-formatted. Keep
hand-formatted files hand-edited; the wizard only writes when you confirm.

## 5. Wire quirks reference

`[providers.wire]` covers exactly the three wire-shape points that have no
safe fallback; everything else (usage token spellings, context-window
spellings, body fixes) is tolerated in code or expressible via `body_overrides`.

| Quirk | Default | Meaning | Opt-out |
| --- | --- | --- | --- |
| `thinking_fields` | `["reasoning_content"]` | Delta fields tried in order as thinking text; first hit wins. | Empty list disables thinking parsing. |
| `effort_field` | `"reasoning_effort"` | Top-level request field carrying the `/effort` value. | `"none"` never sends the field. |
| `session_header` | `"session-id"` | Header carrying the session id (the value is the active session's UUIDv7, passed per turn). | `"none"` sends no header. |

- A model's own `models.session_header` overrides the instance's; `"none"`
  at either level suppresses the header (per model, or per instance).
- Field names are **data, not logic**: any spelling is allowed on purpose;
  only empty names are rejected, at startup.
- Merge order on the wire: standard fields < `body_overrides` < per-turn
  effort — an interactive `/effort` beats a static `body_overrides` value
  for that one field. Session headers are applied after `headers`, so the
  live session id wins over a hand-written same-name entry.

### `/effort` interplay

- `/effort [level]` sets the per-session effort, sent under the instance's
  `effort_field`. `off` is always valid and clears the override (no field
  is sent at all). An endpoint that rejects unknown request fields: set
  `effort_field = "none"` (and `thinking_fields = []` if it also chokes on
  thinking deltas you do not need).
- The level vocabulary comes from the **active model**: its declared
  `reasoning` list (its `default_level` always selectable even when the
  list omits it), falling back to the built-in
  `["minimal", "low", "medium", "high"]`. Selecting a model adopts its
  `default_level` until `/effort` (or a profile, see
  [profiles.md](profiles.md)) overrides it.

## 6. Custom wire protocols via plugins

The built-ins are `openai-compat` and `test` — an in-process echo backend,
fully offline, for demos and tests (`protocol = "test"`). A plugin
contributes a new protocol kind through `Plugin::protocols()`:

```rust
struct VendorFactory;

impl ProtocolFactory for VendorFactory {
    fn id(&self) -> &'static str { "vendor-protocol" }
    fn display_name(&self) -> &'static str { "Vendor" }

    // Per-instance state (URL, key, headers, options) arrives through
    // `instance`; auth, signing, retries live behind this boundary.
    fn create(&self, instance: &ProviderInstance, model: &str)
        -> Result<Arc<dyn ModelClient>, ModelError> { /* build the client */ }

    // optional: models discovery (GET /models for openai-compat)
    // fn list_models<'a>(&'a self, instance: &'a ProviderInstance, cancel)
    //     -> Option<ListModelsFuture<'a>>
}

impl Plugin for Vendor {
    fn id(&self) -> &'static str { "vendor" }
    fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
        static F: &[&'static dyn ProtocolFactory] = &[&VendorFactory];
        F
    }
}
```

An instance of it is then plain TOML:

```toml
[[providers]]
name = "vendor"
protocol = "vendor-protocol"     # resolved against built-in + plugin factories
[providers.options]              # protocol-private pocket
endpoint = "https://vendor.example/api"   # read via option_str
max_output_tokens = 131072       # option_u64; option_bool too
```

The contract (`ProtocolFactory`, `ModelClient`, `ProviderInstance`, the SSE
decoder) lives in the separate `crates/cst-plugin-api` crate, so a plugin
depends on the contract, not the host binary. To be honest: protocol
plugins give **maintainer modularity** — vendor code stays out of core,
feature-trimmable — not end-user runtime extensibility. Third parties do
recompile, but that is one command (`codest plugin install …`), not a host
patch; the registry and pool internals live in `src/core/providers.rs`.
See [plugins.md](plugins.md) / [local-plugins.md](local-plugins.md).

## 7. Design notes

- **Why no default provider:** a default `api.openai.com` or fallback model
  id would send your key and prompts somewhere you did not choose; an empty
  route that explains itself is cheaper. The synthesized `untitled`
  instance exists only because flags/env *explicitly* name an endpoint.
- **Why the protocol/instance split:** a `'static` plugin contract cannot
  carry runtime configuration, and per-vendor auth must not leak into core.
  Compile-time factories × config-data instances pin both: one factory
  serves every instance of its protocol, and per-instance state arrives
  through `create()`. Background reading (design notes, Chinese):
  [provider.md](provider.md) — the `/provider` wizard — and
  [session-id.md](session-id.md) — the session id and its header.

## 8. Related docs

| Doc | Content |
| --- | --- |
| [profiles.md](profiles.md) | Named presets of model + effort + tools + prompt. |
| [plugins.md](plugins.md) | The static plugin system and its contracts. |
| [local-plugins.md](local-plugins.md) | `codest plugin install` and third-party providers. |
| [../README.md](../README.md) | Install, CLI reference, configuration overview. |
