# Terminal UI

English | [简体中文](zh/tui.md)

codest has two ways to occupy a terminal. `fullscreen` gives the app the whole screen and draws the transcript in a viewport it scrolls itself; `inline` leaves the terminal in charge and appends settled output to its native scrollback. Everything else — Markdown rendering, thinking cards, tool cards, the composer, the command menu — behaves the same in both, because both modes render the same transcript cells and differ only in where those cells land. This page covers the display side; the key table lives in [commands.md](commands.md).

## TUI modes

| Mode | Behavior |
| --- | --- |
| `fullscreen` (default) | Alternate screen; the app owns the display; transcript scrolls with `PgUp`/`PgDn` or the mouse wheel. Mouse reporting is on, at 3 lines per wheel notch (`Alt`+wheel = 15); hold `Shift` and drag to use the terminal's native selection. |
| `inline` | No alternate screen; settled transcript lines are written to the terminal's own scrollback; only the input box and status line remain at the bottom. No mouse reporting, so selection and copy stay native. |

The two differ in one contract: in `fullscreen` the app repaints a viewport, so it can redraw anything at any time; in `inline` a line that has reached the scrollback is gone from the app's reach and cannot be revised. That is why `inline` writes only lines the renderer considers final (see [Streaming commit rule](#streaming-commit-rule)).

```bash
codest --tui-mode inline     # regular is an alias of inline
CST_TUI_MODE=inline codest
```

`--tui-mode` and `CST_TUI_MODE` accept `fullscreen` and `inline`; the CLI also accepts `regular` as a spelling of `inline`, matching the vocabulary of other terminal agents.

### Scrolling

In `fullscreen` the transcript lives in an application-scrolled viewport: the mouse wheel moves 3 lines per notch, `Alt`+wheel moves 15, and `PgUp`/`PgDn` move 5 lines per press. Because the app repaints the viewport, the transcript can be longer than the screen and still be browsed in both directions.

In `inline` the app never scrolls anything: `PgUp`/`PgDn` are handed straight to the terminal, which scrolls its own scrollback. While a turn runs, the inline pane keeps at most four rows of the still-streaming tail pinned to the bottom — a fixed height, so a block growing row by row cannot scroll its own already-drawn head into the scrollback and later duplicate it. Once the block settles, its lines are promoted into the scrollback for real.

### Screen layout

Both modes arrange the same four bands, top to bottom: transcript, menu band, composer, footer.

- **Transcript.** User turns carry a `› ` prefix; assistant answers open with `• ` and indent continuations under the text; errors use `! `; notices are dim and unprefixed; an aborted turn leaves a `■ interrupted` marker. Thinking cards, tool cards, and plugin reports such as `/context` are cells in the same list. The transcript is per session: switching sessions switches the rendered view, and a background session catches up when it becomes active.
- **Menu band.** The slash-command candidate popup, or one of the pickers (`/sessions`, `/model`, `/effort`, `/tools`) while one is open. It collapses to zero rows when nothing needs it.
- **Composer.** A multi-line editor with a single top border and a `› ` prompt in its own two-column gutter, capped at eight visible rows; longer input scrolls inside it. Bracketed paste is enabled, so a multi-line paste lands as text instead of a burst of keystrokes.
- **Footer.** The left side reads `codest · <provider/model> · effort <level> · <context>`, then a `│` and the state word — `ready`, or a spinner with `working · Esc interrupt` while a turn runs. The context readout is always present: `used/window (n%)` once both are known, `—/window` when only the window is, `used tok` when only usage is, and `—` when neither is known yet. It tracks each streaming round as it reports usage, so a long tool round never leaves it stale, and the completed turn settles it. The right side is the hint line for the current context; a transient message such as `thinking expanded` takes its place for a couple of seconds.

A terminal resize redraws on the next frame; in `fullscreen` everything re-wraps to the new width, while in `inline` only the rows that have not yet reached the scrollback do (see [Known limitations](development.md#known-limitations)).

## Markdown rendering

Assistant replies render as Markdown. User input, notifications, errors, and thinking content stay plain text — only the model's answer goes through the renderer. Supported syntax:

- headings, with the level reflected in the style;
- lists, including nesting and a hanging indent;
- quotes, drawn with a `▎` rail;
- tables, aligned to their content and compressed when too wide for the pane;
- task checkboxes;
- links, with the text styled and the URL shown dim in parentheses;
- images, shown as their alt text;
- inline styles (bold, italic, code, strikethrough);
- fenced and indented code blocks, with a language label and syntax highlighting.

Inside the transcript every line of an answer is indented under an assistant gutter, so the Markdown structure — headings, bullets, quote rails, table rules, fence labels — keeps its own markers within that indent. Wrapping happens after rendering and is Unicode-width aware, so a wide character or a joined emoji does not shift the following line.

The pipeline is a single pass: `pulldown-cmark` emits events, a handler turns each event into styled ratatui spans, and the result is wrapped and prefixed. Only tables, strikethrough, and task lists are turned on beyond CommonMark. Tables are laid out twice in effect — first to measure each column, then with the widest column shrunk when the row would overflow — which is why a wide table compresses rather than wrapping mid-cell. Images get a `🖼 ` marker followed by their alt text, and a bare autolink is not followed by its destination in parentheses, since the label already is the URL.

### Streaming commit rule

While a reply streams, only *settled* top-level blocks are committed to the scrollback. The still-growing last block is redrawn in the bottom pane on every delta. A block is not settled until the parser can no longer change lines it has already produced: an unclosed `**`, half a table, or an open code fence would all alter lines that were already printed, and in `inline` mode there is no way to take them back. In `inline` the terminal owns the scrollback, so this commit rule is what keeps history stable across a long answer.

### Code highlighting

Highlighting goes through a `TokenKind` interface ([`src/tui/highlight.rs`](../src/tui/highlight.rs)), so the engine behind it is swappable and the renderer never sees engine types. The current engine is `syntect` built with the pure-Rust `regex-fancy` feature: no oniguruma, no C toolchain. The syntax set comes from `two-face`, which covers languages syntect's own dump lacks (TypeScript, TOML, Dockerfile, and others).

The interface is deliberately small: a block of source lines goes in, one list of `TokenKind` segments per line comes out, and a separate palette function maps each kind to a color. Nothing above the seam knows syntect exists, and another engine only has to produce the same kinds: plain, keyword, type, function, string, number, comment, constant, and attribute.

Unknown languages or blocks over 128 KiB degrade to plain text but keep the language label. The same fallback catches pathological single lines, so a huge paste stays readable instead of stalling the frame that renders it. Highlighted blocks are cached by language and content with entry and byte caps — while a fence streams in, every delta is a new variant of the live block, and the caps are what keep memory flat.

## Thinking cards

Model thinking renders as a collapsible card:

```text
⚓ thinking · 4.2s
  │ dim italic reasoning…
```

The header is `⚓ thinking` with a duration chip in the thinking color (purple); the body is italic text under a `│` rail. `Ctrl+T` or `/thinking` toggles whether settled thinking cards show their body, and a folded card names its own expand key. While reasoning streams, the card is a live variant: a braille spinner replaces the anchor glyph, the duration chip waits until the stream stops, and a block cursor rides the last body row.

Thinking text is read from `reasoning_content` by default, and a provider instance can rename the field with `wire.thinking_fields`. Cards are display metadata only: they are replayed on resume so the transcript looks the same, but thinking never re-enters the model context — the model is not shown its own reasoning as conversation. The rollout records each reasoning block with the wall-clock duration behind the header chip and attaches it to the assistant message it preceded, which is how a replay puts the card back above the right answer.

Fold state is per frame, not per card: `Ctrl+T` flips one global flag that every card renders against. A card already written to the scrollback in `inline` mode keeps whatever state it had at that moment — scrollback cannot be rewritten to honor a later toggle.

## Tool cards

Every tool call renders as its own card:

```text
● bash(cargo build) · 4.2s   # `pwsh(...)` on a Windows host
 ⎿  Compiling codest
    … +3 lines (ctrl+o to expand)
```

The header is `● name(args) · duration`, with the name in the tool's category color and the `⎿` spine tinted to match. The body previews the first 3 lines; `Ctrl+O` expands and collapses the full output. While the call is still running, a braille spinner occupies the status-glyph slot and an elapsed-time row hangs under the spine. Failures swap `●` for `✗` and render the message in the error color in place of output. When the argument summary had to fold (a multi-line command, typically), expanding restores the full arguments above the output.

Tool activity is display-only — it never surfaces as agent text. The conversation carries the wire messages (the assistant's `tool_calls` and each tool's result); the card is front-end bookkeeping. A replayed session rebuilds the cards from the rollout and shows a duration chip only for calls this run measured.

## Implementation map

| Concern | File |
| --- | --- |
| Terminal setup, event loop, both render paths, menus, footer | [`../src/tui/app.rs`](../src/tui/app.rs) |
| Transcript cells (user, assistant, error, notice, interrupted, both cards, reports) | [`../src/tui/chat.rs`](../src/tui/chat.rs) |
| Per-session view state: cells, scroll, fold mirrors, inline flush bookkeeping | [`../src/tui/session.rs`](../src/tui/session.rs) |
| Markdown → ratatui lines, incl. tables, fences, links | [`../src/tui/markdown.rs`](../src/tui/markdown.rs) |
| Code tokenization behind `TokenKind` | [`../src/tui/highlight.rs`](../src/tui/highlight.rs) |
| Wrapping and cursor-to-row mapping | [`../src/tui/wrap.rs`](../src/tui/wrap.rs) |
| Composer editing | [`../src/tui/composer.rs`](../src/tui/composer.rs) |
| Full-frame `/context` viewer | [`../src/tui/context_view.rs`](../src/tui/context_view.rs) |
| `/provider` wizard step machine | [`../src/tui/provider_wizard.rs`](../src/tui/provider_wizard.rs) |
| Palette | [`../src/tui/theme.rs`](../src/tui/theme.rs) |

Startup puts three notices at the top of the active session's transcript: a one-line banner, a `backend: <provider/model> · mode: <mode>` line, and a keybinding hint that differs by mode. On a resume the same three are prefixed above the restored transcript, followed by a count of restored sessions; on a plain start with saved sessions available, a hint points at `/sessions`. They are ordinary notice cells — a `/clear` removes them with the rest of the transcript.

## Related

- [commands.md](commands.md) — every slash command and keybinding, including `Ctrl+T` and `Ctrl+O`.
- [configuration.md](configuration.md) — `wire.thinking_fields`, provider instances, and the rest of the file reference.
- [tools.md](tools.md) — the shell (`bash`/`pwsh`) and `fs` tools whose calls render as these cards.
- [../src/tui/app.rs](../src/tui/app.rs) — the event loop and the two rendering paths.
- [../src/tui/chat.rs](../src/tui/chat.rs) — the transcript cells, including both card kinds.

[← Back to the README](../README.md)
