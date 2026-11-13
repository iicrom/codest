# cst-plugin-api

The contract between [`codest`](../..) and the plugins it links in.

A provider plugin can be maintained as a separate repository: the host depends
on the plugin (to link it into the binary), the plugin depends on this crate
(to implement the contracts), and neither needs to depend on the other. Without
this split, "link a third-party plugin into the host" would be a Cargo
dependency cycle.

```text
codest ──depends on──▶ cst-provider-qoder ──depends on──▶ cst-plugin-api
   └──────────────────────── depends on ────────────────────────┘
```

## What lives here

Types only — no HTTP client, no configuration loading, no terminal code:

| Module | Contents |
|---|---|
| `plugin` | `Plugin`, `PluginEffect`, `PluginInfo`, `PluginContext`, `PluginBridge`, `PluginTask`, `ReportBlock` |
| `event` | `PluginEvent`, `PluginEventKind`, `EventSubscription`, `DeltaKind` |
| `status` | `StatusSegment`, `StatusTone` |
| `providers` | `ProtocolFactory`, `ListModelsFuture` |
| `model` | `ModelClient`, `StreamItem`, `TurnOptions`, `ModelError` |
| `message` | `Message`, `Role` |
| `config` | `ProviderInstance`, `ModelInfo`, `WireQuirks`, `PluginSettings` |
| `settings` | `SettingsSection`, `SettingField`, `SettingKind`, `SettingOption` |
| `commands` | `CommandSpec`, `CommandTable`, `Invocation`, `ArgsPolicy` |
| `tools` | `ToolSpec`, `ToolCall`, `ToolExecutor`, `ToolCallAccumulator` |
| `sse` | `SseDecoder`, `SseFrame` |
| `auth` | `AuthProvider`, `AuthMethod`, `AuthStatus`, `SignInFuture`, `SignInNotifier` |

The host re-exports each module under its historical path (`codest::core::model`,
`codest::plugins::Plugin`, …), so this split renamed nothing.

## Writing against it

A plugin exports one or more `static` values implementing `Plugin`. It usually
also implements `ProtocolFactory` (returned from `Plugin::protocols`) and
declares sign-in entries (returned from `Plugin::auth_providers`). A plugin with
anything to configure additionally returns a `SettingsSection` from
`Plugin::settings` — the host renders it in `/settings`, writes an edit into the
plugin's `[plugins."<id>"]` pocket in `config.toml`, and calls
`Plugin::on_settings_changed` with the new pocket. See
[`../../docs/local-plugins.md`](../../docs/local-plugins.md) and the reference
implementation at `../../../cst-provider-qoder`.

## Versioning

`PLUGIN_API_VERSION` is `1`.

- Adding a trait method **with a default implementation** does not bump it.
- Changing an existing signature or its meaning does — plugins must follow.
- The host and a plugin agree through **Cargo versions**, not a runtime
  handshake: an incompatible contract fails to compile rather than failing at
  startup. Depend on a range (`version = "0.1"`), not an exact version.
