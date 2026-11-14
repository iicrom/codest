# Commands and keybindings

English | [简体中文](zh/commands.md)

Slash commands are the local control surface of the TUI: they are parsed and executed in the front-end and are **never sent to the model**. Typing `/` opens a filtered candidate popup; `↑`/`↓` select, `Tab` completes, `Enter` completes and runs, `Esc` closes.

## Slash commands

| Command | Aliases | Behavior |
| --- | --- | --- |
| `/help` | `/h`, `/?` | Show the command list (including plugin-provided ones). |
| `/clear` | — | Clear the conversation context and on-screen transcript. |
| `/quit` | `/exit` | Quit codest. |
| `/status` | — | Show backend, mode, transcript size, context usage, cache hit rates, and loaded plugins. |
| `/usage` | — | Show detailed context consumption: a per-type composition of the window (system / prompt / assistant / thinking / tools / free), a token bar, cache hit rates, and the per-turn history. |
| `/context` | — | Show what the model actually receives: tools and their enabled state, the assembled system prompt (per-source), request config, and token usage. In a session: browse, delete, and edit context entries. |
| `/new` | — | Archive the current conversation and start a fresh session. |
| `/rename <name>` | — | Rename the current session (default name: its first user message). |
| `/sessions` | `/resume` | Open the session list; switch back and continue one. |
| `/continue` | — | Continue the current session without appending a new message — the recovery entry point after a crash or interrupt. |
| `/model` | — | Open the model menu to switch provider/model (`r` re-discovers models). |
| `/provider` | — | Open the provider wizard: add an OpenAI-compatible endpoint, sign in to a plugin provider, or list configured providers. |
| `/profile [name]` | — | Show the available profiles, or switch to one (tools and prompt additions apply at startup only). |
| `/effort [level]` | — | Show or set the reasoning effort (`minimal`/`low`/`medium`/`high` by default, `off` unsets); sent as `reasoning_effort`. |
| `/thinking` | — | Toggle whether thinking cards expand when settled (same as `Ctrl+T`). |
| `/tools [name] [on\|off]` | — | Show or set this session's enabled tools; disabled tools are removed from the request. |
| `/settings [section]` | — | Open the settings screen: host and plugin options, saved to `config.toml`. An argument focuses that section. |

Session-related commands, their turn-boundary rules, and what a switch restores are covered in [sessions.md](sessions.md); `/tools` and the tool loop in [tools.md](tools.md).

## Parsing

A submission is classified once, when it is sent:

- Leading and trailing whitespace is ignored; leading whitespace before the `/` is allowed (`  /help` works).
- The command name runs to the first whitespace and is matched **case-insensitively**, against primary names and aliases alike.
- Arguments are everything after that whitespace, trimmed. Each command declares its argument policy:

| Policy | Commands | A stray argument is |
| --- | --- | --- |
| none | `/help`, `/clear`, `/quit`, `/status`, `/context`, `/usage`, `/new`, `/sessions`, `/continue`, `/model`, `/provider`, `/thinking` | rejected with an error naming the command |
| optional | `/profile`, `/effort`, `/tools`, `/settings` | part of the command (a profile name, a level, a tool name, a section name) |
| required | `/rename` | — an empty argument list is rejected |

- A bare `/` (no name) and an unknown name are errors printed in the transcript, not sent to the model.
- Commands work even with no usable route configured: they need no model.

## Where the commands come from

The shipped table merges two groups, and parsing, the popup, and `/help` all read the same table:

| Group | Commands | Owned by |
| --- | --- | --- |
| Built-in plugins | `/help`, `/clear`, `/quit`, `/status`, `/continue` | `src/plugins/builtin/` — they double as plugin examples. |
| Front-end commands | `/new`, `/rename`, `/sessions`, `/model`, `/provider`, `/profile`, `/effort`, `/thinking`, `/tools`, `/context`, `/settings`, `/usage` | Core/TUI, dispatched before the plugin host. |

Plugin-contributed commands join the same table, so they get the popup and `/help` for free; names and aliases are validated and must be globally unique at startup. See [plugins.md](plugins.md).

## The candidate popup

- A candidate is one **name**: an alias is a separate candidate, listed directly after the command it belongs to, so `/h` and `/help` are two rows.
- Filtering is a **case-insensitive prefix match** on the primary name or any alias: `/RE` still matches `/rename` and `/resume`, and typing `/?` matches `/help` through its `?` alias.
- Candidates appear only while the command **name** is still being typed: the first whitespace ends the candidate window, so `/tools ba` shows no menu.
- The first non-blank character must be `/`; leading whitespace before it is ignored, and anything else is ordinary chat input.
- An empty query (a bare `/`) lists every command in registration order.
- The popup shows at most **6 rows**; the visible window follows the selection, and `↑`/`↓` wrap at both ends.
- A visible popup is authoritative: `Enter` runs the highlighted command even when only a prefix has been typed. `Esc` dismisses the popup and keeps the text; any edit re-opens it and re-anchors the selection to the first candidate.
- The popup works **mid-turn** too: a command typed while streaming runs as itself instead of queueing. It stays closed while a picker or wizard (`/sessions`, `/model`, `/effort`, `/tools`, `/provider`) is open.

```text
/co
▸ /context                 before the first message: show the prompt report; in a session: browse, delete, and edit the context
  /continue                continue the current session without a new message (resume an interrupted turn)
```

Each row is `▸ /name` (fixed-width name column), the command summary, and — for an alias — `(alias of /name)`; a primary name lists its aliases instead.

## Mid-turn input

The composer stays live while a turn streams, and what `Enter` does depends on the state:

- **Idle:** `Enter` submits a new turn.
- **Generating:** `Enter` **queues** the message — it runs as its own turn the moment the current one finishes. `Alt+Enter` **steers** instead: the message joins the running turn at its next step boundary (after the current tool round, before the next request), without waiting for the turn to end. On a turn with no tool rounds the boundary never comes, so a steered message is handed back and requeued automatically.
- Pending messages show as rows above the composer (`⏳ queued · after this turn`, `⚡ steer · next boundary`) until the core reports them delivered; only then do they become transcript cells, so the record never shows a message above the answer still streaming.
- A command typed mid-turn runs as itself — steering is reserved for messages. The exceptions that would strand a streaming turn's output (`/new`, `/sessions`, `/clear`) are refused with a footer flash until the agent is idle again.
- Config changes made mid-turn (`/model`, `/effort`, `/profile`, `/tools`) apply from the next turn; `/model` says so in its notice.

## The report window

`/status` and `/usage` open a **floating window** anchored to the bottom of the frame instead of printing into the transcript. It floats over whatever is streaming — the output underneath keeps repainting without disturbing it — scrolls with `↑`/`↓`/`PgUp`/`PgDn`/`Home`/`End`, and closes with `Esc`, `Enter`, or `q`, leaving nothing behind in the transcript.

```text
/co
▸ /context                 before the first message: show the prompt report; in a session: browse, delete, and edit the context
  /continue                continue the current session without a new message (resume an interrupted turn)
```

Each row is `▸ /name` (fixed-width name column), the command summary, and — for an alias — `(alias of /name)`; a primary name lists its aliases instead.

## Keybindings

| Key | Behavior |
| --- | --- |
| `Enter` | Send; queue the message while generating; run the selected command when the candidate popup is open. |
| `Alt+Enter` | Deliver now: send when idle, steer into the running turn's next step boundary while generating. |
| `Tab` | Complete the selected command. |
| `Ctrl+J` / `Shift+Enter` / `Ctrl+Enter` | Insert a newline. |
| `↑` / `↓` | Select candidates; move the cursor in multi-line input; browse input history when single-line. |
| `Ctrl+P` / `Ctrl+N` | Browse input history. |
| `←` / `→` / `Home` / `End` | Cursor movement. |
| `Backspace` / `Delete` | Delete a character. |
| `Ctrl+W` / `Alt+Backspace` | Delete the previous word. |
| `Ctrl+U` / `Ctrl+K` | Kill to line start / end. |
| Mouse wheel (fullscreen) | Scroll the transcript (3 lines per notch; `Alt`+wheel = 15). |
| `PageUp` / `PageDown` | Scroll the transcript (fullscreen); handed to the terminal in inline mode. |
| `Ctrl+End` | Jump to the newest line and resume following (fullscreen); `End` alone in inline mode. |
| `Esc` | Close the candidate popup; interrupt the turn while generating. |
| `Ctrl+T` | Expand/collapse thinking cards. |
| `Ctrl+O` | Expand/collapse tool output (folded preview shows the first 3 lines). |
| `Ctrl+C` | Interrupt while generating; clear input; double-press on empty input to quit. |
| `Ctrl+D` | Quit on empty input. |

### Notes

- **Fullscreen vs inline.** `PageUp`/`PageDown` belong to the app only in `fullscreen`, where it owns the alternate screen. In `inline` the terminal owns the scrollback, so those keys are passed through to it. `Ctrl+End` follows the same rule: it jumps the fullscreen viewport back to the newest line and re-arms follow mode, and in `inline` it stays a plain end-of-input cursor move.
- **`Ctrl+T` is `/thinking`.** Both toggle whether thinking cards are expanded when settled; the card itself is always recorded.
- **`Ctrl+O` folds tool output.** The card body shows the first 3 lines plus `… +N lines (ctrl+o to expand)`; `Ctrl+O` applies to all tool cards in the transcript.
- **`Ctrl+C` has three states.** While generating it interrupts the turn (queued messages keep their place and run next); with text in the composer it clears the input; on empty input it must be pressed twice in quick succession to quit.
- **`Ctrl+D`** quits only on empty input — with text present it does nothing.

## Related

- [sessions.md](sessions.md) — session commands, turn-boundary rules, persistence, recovery.
- [tools.md](tools.md) — `/tools`, the tool loop, and tool cards.
- [configuration.md](configuration.md) — `config.toml`, `providers.toml`, `profiles.toml`, session header.
- [tui.md](tui.md) — fullscreen vs inline rendering, Markdown and thinking cards.

[← Back to the README](../README.md)
