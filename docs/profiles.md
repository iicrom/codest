# Profiles

English | [简体中文](zh/profiles.md)

A profile is a named preset: which model to start with, which reasoning
effort, which plugin tools stay available, and an optional addition to the
system prompt. Define them in `~/.codest/profiles.toml`, start one with
`--profile <name>` (env `CST_PROFILE`), and switch between them at run time
with `/profile`.

## 1. Overview

```toml
# ~/.codest/profiles.toml
[profiles.code]
default = "openai/gpt-4o-mini"   # initial selection
effort = "high"                  # initial /effort
tools = ["fs"]                   # plugin ids whose tools stay on

  [profiles.code.prompt]
  append = "You are a senior Rust engineer..."   # appended to the system prompt

[profiles.fast]
default = "local/qwen2.5"
effort = "low"
```

A profile composes capabilities for one run; it declares no endpoints —
provider wiring lives in `providers.toml` (see
[multi-provider.md](multi-provider.md)). Values are **presets, not a second
configuration layer**: CLI flags still win over any of them, and a restored
session restores its own state instead.

## 2. Configuration reference

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | string | — | Initial model selection, `"provider/model"`. |
| `effort` | string | — | Initial reasoning effort, applied exactly like `/effort <level>`; `"off"` clears the override. |
| `tools` | string array | absent = keep every plugin's tools | Plugin ids whose tools are declared to the model; `[]` = no tools at all. |
| `[profiles.<name>.prompt]` `append` | string | — | Appended after the base prompt and the plugin fragments — the most specific text wins the last word. |

- Parsing is strict (`deny_unknown_fields`) at both the profile and the
  `prompt` level: a typo is a startup error naming the file.
- A bare `[profiles.fast]` with no fields is legal — all defaults, a pure
  label to switch to.
- Profiles are listed in deterministic (alphabetical) order.
- A missing `profiles.toml` simply means no profiles.

### Annotated example

```toml
[profiles.code]
default = "glm/glm-5.3"        # startup selection; beats config.toml's default,
                               # loses to --provider/--model
effort = "high"                # behaves exactly like /effort high at startup
tools = ["bash", "fs"]         # only these plugins' tools are sent to the model;
                               # the others keep commands, hooks, and prompt fragments

  [profiles.code.prompt]
  append = "你是资深 Rust 工程师…"   # appended after the base prompt and plugin fragments

[profiles.bare]                # a named label with no presets at all

[profiles.chat]
default = "openai/gpt-4o-mini"
effort = "off"                 # clear the effort override: never send the field
tools = []                     # no tools — for models without function calling
```

## 3. Lifecycle semantics

The four bindings split into two halves:

| Binding | Phase | Switchable at run time? |
| --- | --- | --- |
| `default`, `effort` | Session-time | Yes — `/profile <name>`, applied at the turn boundary |
| `tools`, `prompt.append` | Assembly-time (startup only) | No — restart, or `/new` plus a new profile |

- **Assembly-time.** At startup the active profile's `tools` filter is
  applied to the plugin host (Section 5) and its `prompt.append` is stitched
  into the system prompt. Both are fixed for the life of the run; switching
  profiles never reloads them. `/profile` itself says so in its help text.
- **Session-time.** `/profile <name>` submits an op like `/model` does, so
  it lands at the next turn boundary and never interrupts a stream. The
  switch applies the declared selection first (switching models reseeds the
  effort from that model's `default_level`), then the declared `effort`,
  which gets the final say. An unresolvable selection is reported as an
  error but does not abort the rest of the preset — the profile still
  becomes active. `/profile` with no argument lists the available profiles
  (`*` marks the current one, with each one's selection and effort).
- **Persistence.** The startup profile is recorded in the session metadata;
  every switch appends a `profile_switched` record to the rollout. Restored
  sessions restore their **own** state — the model they last used, the
  reasoning effort (`/effort` or a profile's `effort`, an explicit `off`
  included), the tool set they last ran with, and the profile name they last
  used (shown as active in `/profile`) — while assembly-time bindings stay
  as they were at startup. A session that has never chosen an effort seeds it
  from the active profile's `effort` (an explicit `off` counts), otherwise
  from the model's `default_level`; a recorded choice is never overwritten by
  a model default.
- **CLI flags always win.** `--provider`/`--model` override any declared
  default; a profile is a preset, not an override of your flags.

## 4. Selection precedence

Which model a run starts with:

| Priority | Source |
| --- | --- |
| 1 | `--provider` / `--model` (env `CST_PROVIDER` / `CST_MODEL`) |
| 2 | Active profile's `default` |
| 3 | `config.toml` `default` |
| 4 | First model of the first instance |

- `--provider` alone starts on that provider's first declared model;
  `--model` alone starts on the first instance with that model id.
- An unknown **provider** is a hard startup error; an undeclared **model
  id** proceeds with a warning — the endpoint answers for whether the model
  exists, and discovery may fill it in later.
- `--profile <name>` naming nothing is **non-fatal**: a note on stderr
  lists the available names and the run continues without a profile.

## 5. Tools binding

- `tools` filters **by plugin id** at assembly time: the listed plugins keep
  tool declaration and execution eligibility, and every other plugin's tools
  are not sent to the model. It is not an uninstall — the excluded plugins
  keep their commands, hooks, and prompt fragments (`/help`, `/clear`, etc.
  always work).
- `tools` absent keeps everything; an empty list `[]` strips all tools (the
  way to run a model without function calling); an unknown plugin id in the
  list is a startup error — a typo must not silently strip the wrong tools.
- **Interplay with `/tools`:** the profile coarse-filters by plugin id at
  startup; `/tools` fine-tunes per tool at session time. A tool disabled
  with `/tools` is removed from the request and refused by an execution-time
  gate, and the set is recorded (`tools_switched`) and restored per session.
  Together: `tools = ["fs"]` in the profile, then `/tools read off` inside
  the session, if you want a read-only run.

## 6. Effort

- A profile's `effort` behaves exactly like typing `/effort <value>`: the
  value is applied verbatim, `"off"` (any casing) clears the override so no
  effort field is sent at all.
- The level vocabulary comes from the **active model**: its declared
  `reasoning` levels (lowest first, with its `default_level` always
  selectable), falling back to the built-in
  `["minimal", "low", "medium", "high"]`. `off` is always valid regardless
  of the list.
- On a profile switch the declared selection is applied first — adopting
  that model's `default_level` — and the profile's `effort` is applied
  last, so the profile wins.
- On the wire the effort rides the instance's `effort_field` (default
  `reasoning_effort`; `"none"` never sends it) — see the wire quirks
  reference in [multi-provider.md](multi-provider.md).

## 7. Design notes

- **Presets, not a second config layer.** There is one merge chain —
  built-in defaults < active profile < CLI flags — and no patch layers, no
  per-key overrides, no scripting. A profile that needs different *wire*
  behavior belongs in the provider instance instead (`wire`, `headers`,
  `body_overrides`); instances describe how to talk, profiles describe what
  the session wants.
- Profiles deliberately do not touch the plugin contract: tool filtering
  rides the existing host filter, and the prompt addition is stitched at
  assembly, so no hooks were added.
- Deeper background: [plugins.md](plugins.md) — the static plugin system;
  [multi-provider.md](multi-provider.md) — provider instances and
  protocols.

## 8. Related docs

| Doc | Content |
| --- | --- |
| [multi-provider.md](multi-provider.md) | Provider instances, wire quirks, `/model`, `/provider`. |
| [plugins.md](plugins.md) | The static plugin system; what a tool filter keeps and drops. |
| [../README.md](../README.md) | CLI reference and configuration overview. |
| [../src/core/session.rs](../src/core/session.rs) | `Op::SwitchProfile` semantics and rollout records. |
| [../crates/cst-plugin-api/src/config.rs](../crates/cst-plugin-api/src/config.rs) | `Profile` field definitions. |
