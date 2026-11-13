#!/usr/bin/env python3
"""Minimal OpenAI-compatible chat-completions server for local testing.

It implements just enough of `POST /chat/completions` for codest: it answers
with a help text, either as a normal JSON response or as an SSE stream when the
request sets `"stream": true`. No API key is required or checked, and no
external network access is performed.

Usage:
    python3 scripts/openai_test_server.py --port 8080
    codest --base-url http://127.0.0.1:8080/v1 --api-key test --model test-model
    cst    --base-url http://127.0.0.1:8080/v1 --api-key test --model test-model  # alias

Pass `--port 0` to bind an ephemeral port; the chosen URL is printed on stdout.

Tool-calling round trip: a keyword in the last user message turns the next
response into a `tool_calls` delta (fragmented arguments, the shape providers
stream) instead of text. `@bash` (`@pwsh` on Windows — the built-in shell tool
is the host's own), `@read`, `@write` and `@edit` call that built-in tool with
the text after the keyword (`@bash ls -la` → `bash` with
`{"command": "ls -la"}`); `@tool` calls whatever `--tool` / `--tool-arguments`
configured, which by default is the unregistered `ping` — no tool by that name
is in the binary, so its result is the `error: unknown tool …` text. codest
executes the call and feeds the result back as a `role: "tool"` message; the
following response summarizes the round.

Every message without a keyword gets the help reply, which lists these
keywords — so the server documents itself on the first turn.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable

TOKEN_RE = re.compile(r"\S+\s*|\s+")
CHUNK_CHARS = 4


def tokenize(text: str) -> list[str]:
    """Split text losslessly into small chunks for visible streaming.

    Whitespace stays attached to the preceding word, and long runs without
    spaces (for example CJK text) are cut every `CHUNK_CHARS` characters.
    """
    tokens: list[str] = []
    for piece in TOKEN_RE.findall(text):
        word = piece.rstrip()
        if not word:
            tokens.append(piece)
            continue
        trailing = piece[len(word) :]
        chunks = [word[i : i + CHUNK_CHARS] for i in range(0, len(word), CHUNK_CHARS)]
        chunks[-1] += trailing
        tokens.extend(chunks)
    return tokens or [""]


def last_user_text(messages: Any) -> str:
    if not isinstance(messages, list):
        return "(empty message)"
    for message in reversed(messages):
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            text = content
        elif isinstance(content, list):
            text = "".join(
                part.get("text", "")
                for part in content
                if isinstance(part, dict) and part.get("type") in ("text", "input_text")
            )
        else:
            text = ""
        text = text.strip()
        return text or "(empty message)"
    return "(empty message)"


def help_reply(messages: Any) -> str:
    """The default answer: what this server is and which keywords call tools.

    Anything the last user message does not turn into a tool call lands here —
    no keyword at all, or a keyword whose tail did not parse — so a plain
    `hello` is enough to learn the whole keyword table.
    """
    keywords = "\n".join(
        f"- `{trigger.keyword}` — {trigger.summary}" for trigger in TRIGGERS
    )
    note = ""
    match = find_trigger(last_user_text(messages))
    if match is not None and match[0].build is not None:
        note = (
            f"\n\nThe `{match[0].keyword}` keyword was there, but the text after "
            "it did not parse — its example above shows the shape."
        )
    return (
        "### codest test server\n\n"
        f'You said: "{last_user_text(messages)}"\n\n'
        "Nothing in that message turned into a tool call, so here is the help "
        "instead. This server is `scripts/openai_test_server.py`, a minimal "
        "OpenAI-compatible `/chat/completions` endpoint: it streams deltas over "
        "SSE without an API key and without any external network access.\n\n"
        "**Keywords that trigger a tool call**\n\n"
        f"{keywords}"
        f"{note}\n\n"
        "The call streams in fragments, the way real providers send it. codest "
        "reassembles it, runs the tool, and sends the result back as a "
        "`role: \"tool\"` message; the reply to that second request summarizes "
        "the round.\n\n"
        "**Other endpoints**\n\n"
        "- `GET /models` — OpenAI-shaped model list, feeding `fetch_models`\n"
        "- `GET /health` — `{\"status\":\"ok\"}`\n\n"
        "**Flags**\n\n"
        "- `--port 0` picks an ephemeral port and prints the URL on stdout\n"
        "- `--delay` spaces the streamed tokens; `--quiet` stops request logging\n"
        "- `--model` names the fallback model; `--tool` / `--tool-arguments` "
        "decide what `@tool` calls\n"
        "- `--dump-headers PATH` appends one JSON line per request with its "
        "headers, so a test can assert the session header reached the wire"
    )


def reasoning_for(body: dict[str, Any]) -> str:
    """The thinking text streamed before the reply as `reasoning_content`.

    It echoes back the `reasoning_effort` the request actually carried, so an
    `/effort` change is visible end-to-end in the codest transcript.
    """
    effort = body.get("reasoning_effort")
    effort_text = effort if isinstance(effort, str) and effort.strip() else "(not set)"
    return (
        "The test server is thinking: this text arrives as reasoning_content "
        f"deltas, which codest renders dim. reasoning_effort={effort_text}."
    )


TOOL_NAME = "ping"
# The `ping`-shaped argument object the default call streams in fragments;
# `--tool-arguments` replaces it for tools with a different schema.
TOOL_ARGUMENTS = '{"note":"called by the test server"}'

# The built-in shell tool is the host's own (`bash` on unix, `pwsh` on
# Windows); the keyword and its example follow so the same script works on
# either host.
SHELL_TOOL = "pwsh" if os.name == "nt" else "bash"
SHELL_EXAMPLE = "Get-ChildItem" if os.name == "nt" else "ls -la"


def first_word(rest: str) -> tuple[str, str]:
    """Split `rest` into its first whitespace-delimited word and the remainder."""
    head, _, tail = rest.partition(" ")
    return head, tail.strip()


def shell_arguments(rest: str) -> dict[str, Any]:
    return {"command": rest or SHELL_EXAMPLE}


def read_arguments(rest: str) -> dict[str, Any]:
    path, _ = first_word(rest)
    return {"path": path or "README.md", "limit": 40}


def write_arguments(rest: str) -> dict[str, Any] | None:
    """`@write <path> <content>`; `None` when the path is missing."""
    path, content = first_word(rest)
    if not path:
        return None
    return {"path": path, "content": content or "written by the test server"}


def edit_arguments(rest: str) -> dict[str, Any] | None:
    """`@edit <path> | <old> | <new>`; `None` when the shape does not parse."""
    parts = [part.strip() for part in rest.split("|")]
    if len(parts) != 3 or not all(parts):
        return None
    path, old_string, new_string = parts
    return {
        "path": path,
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": False,
    }


@dataclass(frozen=True)
class Trigger:
    """One keyword that turns the next response into a `tool_calls` round.

    `tool: None` means "the server's configured pair" (`--tool` /
    `--tool-arguments`); otherwise `build` derives the arguments from the text
    after the keyword, and `None` from it (an unparsable tail) falls back to
    the help reply.
    """

    keyword: str
    tool: str | None
    summary: str
    build: Callable[[str], dict[str, Any] | None] | None = None


# Order matters only if keywords could overlap; they do not. `@tool` first
# keeps the historical trigger — and the smoke scripts — working unchanged.
TRIGGERS: tuple[Trigger, ...] = (
    Trigger(
        keyword="@tool",
        tool=None,
        summary=(
            "call the configured tool (`--tool` / `--tool-arguments`; `ping` by "
            "default, which nothing implements)"
        ),
    ),
    Trigger(
        keyword=f"@{SHELL_TOOL}",
        tool=SHELL_TOOL,
        summary=f"run a shell command, e.g. `@{SHELL_TOOL} {SHELL_EXAMPLE}`",
        build=shell_arguments,
    ),
    Trigger(
        keyword="@read",
        tool="read",
        summary="read a file with line numbers, e.g. `@read README.md`",
        build=read_arguments,
    ),
    Trigger(
        keyword="@write",
        tool="write",
        summary="write a file, e.g. `@write notes.txt hello`",
        build=write_arguments,
    ),
    Trigger(
        keyword="@edit",
        tool="edit",
        summary="replace exact text, e.g. `@edit notes.txt | hello | goodbye`",
        build=edit_arguments,
    ),
)


def find_trigger(text: str) -> tuple[Trigger, str] | None:
    """The first keyword in `text`, with the text that follows it."""
    for trigger in TRIGGERS:
        at = text.find(trigger.keyword)
        if at >= 0:
            return trigger, text[at + len(trigger.keyword) :].strip()
    return None


def resolve_call(
    text: str, configured_tool: str, configured_arguments: str
) -> tuple[str, str] | None:
    """The `tool_calls` round `text` asks for as `(tool, arguments_json)`.

    The first keyword found wins and the text after it becomes the call's
    argument, so one message can drive any built-in tool. `None` means "no
    call": the caller answers with the help text instead.
    """
    match = find_trigger(text)
    if match is None:
        return None
    trigger, rest = match
    if trigger.build is None:
        return configured_tool, configured_arguments
    arguments = trigger.build(rest)
    if arguments is None:
        return None
    return trigger.tool or configured_tool, json.dumps(arguments, ensure_ascii=False)


def current_turn(messages: Any) -> list[dict[str, Any]]:
    """The messages the request appended after its last user message.

    Everything that decides the reply — whether a tool round is already in
    flight for this turn, and which round to summarize — belongs to that turn.
    Scanning the whole transcript instead would let one tool round answer
    every later turn with its own summary.
    """
    if not isinstance(messages, list):
        return []
    for index in range(len(messages) - 1, -1, -1):
        message = messages[index]
        if isinstance(message, dict) and message.get("role") == "user":
            return [item for item in messages[index + 1 :] if isinstance(item, dict)]
    return [item for item in messages if isinstance(item, dict)]


def turn_has_tool_round(messages: Any) -> bool:
    """Whether this turn already ran a tool round (calls or results)."""
    return any(
        message.get("role") == "tool" or isinstance(message.get("tool_calls"), list)
        for message in current_turn(messages)
    )


def last_tool_result(messages: Any) -> str:
    """The content of this turn's most recent `role: "tool"` message."""
    for message in reversed(current_turn(messages)):
        if message.get("role") == "tool":
            content = message.get("content")
            if isinstance(content, str):
                return content
            return json.dumps(content, ensure_ascii=False)
    return "(no tool result)"


def called_tools(messages: Any) -> str:
    """The tool names this turn's `tool_calls` asked for, backticked."""
    names: list[str] = []
    for message in current_turn(messages):
        for call in message.get("tool_calls") or []:
            function = call.get("function") if isinstance(call, dict) else None
            name = function.get("name") if isinstance(function, dict) else None
            if isinstance(name, str) and name and name not in names:
                names.append(name)
    return ", ".join(f"`{name}`" for name in names) or "a"


def tool_call_reply(messages: Any) -> str:
    """The text answer that follows a completed tool round."""
    return (
        f"The previous round asked for the {called_tools(messages)} tool. "
        "codest executed it and\n"
        f"fed the result back: {last_tool_result(messages)}\n\n"
        "This completes the tool-calling round trip of the test server."
    )


class ChatHandler(BaseHTTPRequestHandler):
    server_version = "codest-test-server/0.1"
    protocol_version = "HTTP/1.0"

    def do_GET(self) -> None:  # noqa: N802 - http.server naming
        path = self.path.split("?", 1)[0].rstrip("/")
        if path == "/health":
            self._send_json(200, {"status": "ok"})
        elif path.endswith("/models"):
            # OpenAI-shaped directory for codest's `fetch_models` discovery;
            # `default_level` feeds the /effort picker, the context-window
            # fields feed the footer/status display. The two entries spell
            # the window differently on purpose: `context_length` is what
            # OpenRouter returns, `context_window` the plain spelling.
            self._send_json(
                200,
                {
                    "object": "list",
                    "data": [
                        {
                            "id": "test-model",
                            "name": "Test Model",
                            "default_level": "high",
                            "context_length": 128000,
                        },
                        {
                            "id": "second-model",
                            "context_window": 200000,
                        },
                    ],
                },
            )
        else:
            self._send_json(404, {"error": {"message": f"unknown path: {self.path}"}})

    def do_POST(self) -> None:  # noqa: N802 - http.server naming
        path = self.path.split("?", 1)[0].rstrip("/")
        if not path.endswith("/chat/completions"):
            self._send_json(404, {"error": {"message": f"unknown path: {self.path}"}})
            return

        self._record_headers()

        try:
            body = self._read_json()
        except ValueError as error:
            self._send_json(400, {"error": {"message": str(error)}})
            return

        if not isinstance(body, dict):
            self._send_json(400, {"error": {"message": "request body must be an object"}})
            return

        if body.get("stream"):
            self._send_stream(body)
        else:
            self._send_completion(body)

    def _record_headers(self) -> None:
        """Append this request's headers as one JSON line to `--dump-headers`.

        The session header is the one thing a client can only get wrong on the
        wire, so the smoke tests assert it here rather than trusting the body.
        """
        sink = getattr(self.server, "dump_headers", None)
        if not sink:
            return
        record = {"path": self.path, "headers": {key: value for key, value in self.headers.items()}}
        try:
            with open(sink, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(record, ensure_ascii=False) + "\n")
        except OSError as error:
            sys.stderr.write(f"codest-test-server: cannot write header dump: {error}\n")

    def _read_json(self) -> Any:
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError as error:
            raise ValueError("invalid Content-Length header") from error
        raw = self.rfile.read(length) if length > 0 else b"{}"
        try:
            return json.loads(raw)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid JSON body: {error}") from error

    def _send_json(self, status: int, payload: Any) -> None:
        data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _send_completion(self, body: dict[str, Any]) -> None:
        """The non-streaming twin of `_send_stream`: same trigger, same reply."""
        messages = body.get("messages")
        call = self._call_for(messages)
        if call is not None:
            name, arguments = call
            message: dict[str, Any] = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call-test-1",
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }
                ],
            }
            finish_reason = "tool_calls"
        else:
            message = {"role": "assistant", "content": self._text_reply(messages)}
            finish_reason = "stop"
        self._send_json(
            200,
            {
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": self._model(body),
                "choices": [
                    {
                        "index": 0,
                        "message": message,
                        "finish_reason": finish_reason,
                    }
                ],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            },
        )

    def _call_for(self, messages: Any) -> tuple[str, str] | None:
        """The tool call this turn asks for, if any.

        A turn that already carries a tool round never triggers another one:
        the keyword stays in the user message on the follow-up request, and
        without this guard the round would never end. The check is scoped to
        the current turn, so a later message triggers its own call (or the
        help reply) instead of re-answering the old round.
        """
        if turn_has_tool_round(messages):
            return None
        return resolve_call(
            last_user_text(messages),
            getattr(self.server, "tool_name", TOOL_NAME),
            getattr(self.server, "tool_arguments", TOOL_ARGUMENTS),
        )

    @staticmethod
    def _text_reply(messages: Any) -> str:
        """The text answer: this turn's round summary, else the help."""
        return (
            tool_call_reply(messages)
            if turn_has_tool_round(messages)
            else help_reply(messages)
        )

    def _send_stream(self, body: dict[str, Any]) -> None:
        model = self._model(body)
        created = int(time.time())
        delay = getattr(self.server, "delay", 0.0)
        messages = body.get("messages")

        # Tool round trip: a keyword asks for a call; a turn that already ran
        # one gets the summary instead (so the loop always terminates). No
        # keyword → the help reply.
        call = self._call_for(messages)
        if call is not None:
            self._send_tool_call_stream(body, call)
            return
        reply = self._text_reply(messages)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        try:
            # Reasoning deltas first, mirroring reasoning-tuned backends whose
            # `reasoning_content` precedes the visible reply.
            for token in tokenize(reasoning_for(body)):
                self._send_chunk(
                    {
                        "id": "chatcmpl-test",
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"reasoning_content": token},
                                "finish_reason": None,
                            }
                        ],
                    }
                )
                if delay > 0:
                    time.sleep(delay)

            for token in tokenize(reply):
                self._send_chunk(
                    {
                        "id": "chatcmpl-test",
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"content": token},
                                "finish_reason": None,
                            }
                        ],
                    }
                )
                if delay > 0:
                    time.sleep(delay)

            self._send_chunk(
                {
                    "id": "chatcmpl-test",
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                }
            )
            # A final usage report, mirroring `stream_options.include_usage`:
            # an empty choices array plus the token counts. Fixed values keep
            # the smoke test's footer assertions stable.
            self._send_chunk(
                {
                    "id": "chatcmpl-test",
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 42,
                        "completion_tokens": 7,
                        "total_tokens": 49,
                    },
                }
            )
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            # The client interrupted the turn or quit; that is not an error.
            return

    def _send_tool_call_stream(self, body: dict[str, Any], call: tuple[str, str]) -> None:
        """Stream one `tool_calls` round for the keyword that asked for it.

        The call's arguments arrive in fragments across chunks (the shape
        real providers stream), so the client's reassembly is exercised too.
        """
        model = self._model(body)
        created = int(time.time())
        delay = getattr(self.server, "delay", 0.0)
        name, arguments = call

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        # The arguments stream in two fragments to exercise reassembly. The
        # trigger picked both the name and the argument object (`@tool` falls
        # back to `--tool` / `--tool-arguments`).
        split = arguments.index(":") + 1 if ":" in arguments else len(arguments)
        call_chunks: list[list[dict[str, Any]]] = [
            [
                {
                    "index": 0,
                    "id": "call-test-1",
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                }
            ],
            [{"index": 0, "function": {"arguments": arguments[:split]}}],
            [{"index": 0, "function": {"arguments": arguments[split:]}}],
        ]
        try:
            for calls in call_chunks:
                self._send_chunk(
                    {
                        "id": "chatcmpl-test",
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": None}],
                    }
                )
                if delay > 0:
                    time.sleep(delay)

            self._send_chunk(
                {
                    "id": "chatcmpl-test",
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                }
            )
            self._send_chunk(
                {
                    "id": "chatcmpl-test",
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 30,
                        "completion_tokens": 5,
                        "total_tokens": 35,
                    },
                }
            )
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return

    def _send_chunk(self, payload: dict[str, Any]) -> None:
        data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.wfile.write(b"data: " + data + b"\n\n")
        self.wfile.flush()

    def _model(self, body: dict[str, Any]) -> str:
        model = body.get("model")
        if isinstance(model, str) and model.strip():
            return model
        return getattr(self.server, "default_model", "test-model")

    def log_message(self, format: str, *args: Any) -> None:
        if not getattr(self.server, "quiet", False):
            sys.stderr.write(f"codest-test-server: {format % args}\n")


class TestServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(
        self,
        address: tuple[str, int],
        *,
        delay: float,
        default_model: str,
        quiet: bool,
        tool_name: str = TOOL_NAME,
        tool_arguments: str = TOOL_ARGUMENTS,
        dump_headers: str | None = None,
    ) -> None:
        super().__init__(address, ChatHandler)
        self.delay = delay
        self.default_model = default_model
        self.quiet = quiet
        self.tool_name = tool_name
        self.tool_arguments = tool_arguments
        # Where to append one JSON line per request with its headers; the
        # smoke tests read it to prove the session header reaches the wire.
        self.dump_headers = dump_headers


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Minimal OpenAI-compatible chat-completions server for codest tests."
    )
    parser.add_argument("--host", default="127.0.0.1", help="bind address (default: 127.0.0.1)")
    parser.add_argument(
        "--port", type=int, default=8080, help="bind port, 0 for an ephemeral port (default: 8080)"
    )
    parser.add_argument(
        "--delay",
        type=float,
        default=0.02,
        help="seconds to wait between streamed tokens (default: 0.02)",
    )
    parser.add_argument(
        "--model", default="test-model", help="model name used when the request omits one"
    )
    parser.add_argument("--quiet", action="store_true", help="do not log requests")
    parser.add_argument(
        "--tool",
        default=TOOL_NAME,
        help=f"tool the `@tool` keyword calls (default: {TOOL_NAME})",
    )
    parser.add_argument(
        "--tool-arguments",
        default=TOOL_ARGUMENTS,
        help='JSON arguments for the `@tool` call (default: the ping-shaped object)',
    )
    parser.add_argument(
        "--dump-headers",
        default=None,
        metavar="PATH",
        help="append one JSON line per request with its headers (for header assertions)",
    )
    args = parser.parse_args()

    server = TestServer(
        (args.host, args.port),
        delay=max(0.0, args.delay),
        default_model=args.model,
        quiet=args.quiet,
        tool_name=args.tool,
        tool_arguments=args.tool_arguments,
        dump_headers=args.dump_headers,
    )
    host, port = server.server_address[:2]
    print(f"codest test server listening on http://{host}:{port}/v1", flush=True)

    try:
        server.serve_forever(poll_interval=0.2)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        print("codest test server stopped", file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
