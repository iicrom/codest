#!/usr/bin/env python3
"""Minimal OpenAI-compatible chat-completions server for local testing.

It implements just enough of `POST /chat/completions` for codest: it echoes the
last user message back, either as a normal JSON response or as an SSE stream
when the request sets `"stream": true`. No API key is required or checked, and
no external network access is performed.

Usage:
    python3 scripts/openai_test_server.py --port 8080
    codest --base-url http://127.0.0.1:8080/v1 --api-key test --model test-model
    cst    --base-url http://127.0.0.1:8080/v1 --api-key test --model test-model  # alias

Pass `--port 0` to bind an ephemeral port; the chosen URL is printed on stdout.

Tool-calling round trip: when the last user message contains `@tool`, the next
streamed response is a `tool_calls` delta (a `ping` call with fragmented
arguments) instead of text. codest executes it — no tool is registered in the
binary, so the result is the `error: unknown tool …` text — and feeds it back
as a `role: "tool"` message. The following response then summarizes what the
tool round reported, completing the loop end-to-end.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

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


def reply_for(messages: Any) -> str:
    return (
        f'You said: "{last_user_text(messages)}"\n\n'
        "This reply was streamed by scripts/openai_test_server.py over an "
        "OpenAI-compatible /chat/completions endpoint. No external network or "
        "API key was used.\n\n"
        "The TUI rendered these deltas as they arrived."
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


TOOL_TRIGGER = "@tool"
TOOL_NAME = "ping"


def conversation_has_tool_round(messages: Any) -> bool:
    """Whether the transcript already carries a tool round (calls or results)."""
    if not isinstance(messages, list):
        return False
    for message in messages:
        if not isinstance(message, dict):
            continue
        if message.get("role") == "tool" or isinstance(message.get("tool_calls"), list):
            return True
    return False


def last_tool_result(messages: Any) -> str:
    """The content of the most recent `role: "tool"` message, for the summary."""
    if isinstance(messages, list):
        for message in reversed(messages):
            if isinstance(message, dict) and message.get("role") == "tool":
                content = message.get("content")
                if isinstance(content, str):
                    return content
                return json.dumps(content, ensure_ascii=False)
    return "(no tool result)"


def tool_requested(messages: Any) -> bool:
    """The `@tool` trigger in the last user message asks for a call round."""
    return TOOL_TRIGGER in last_user_text(messages)


def tool_call_reply(messages: Any) -> str:
    """The text answer that follows a completed tool round."""
    return (
        "The previous round asked for the `ping` tool. codest executed it and\n"
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
        reply = reply_for(body.get("messages"))
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
                        "message": {"role": "assistant", "content": reply},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            },
        )

    def _send_stream(self, body: dict[str, Any]) -> None:
        model = self._model(body)
        created = int(time.time())
        delay = getattr(self.server, "delay", 0.0)
        messages = body.get("messages")

        # Tool round trip: `@tool` asks for a call; a transcript that already
        # carries one gets the summary instead (so the loop always terminates).
        if tool_requested(messages) and not conversation_has_tool_round(messages):
            self._send_tool_call_stream(body)
            return
        reply = (
            tool_call_reply(messages)
            if conversation_has_tool_round(messages)
            else reply_for(messages)
        )

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

    def _send_tool_call_stream(self, body: dict[str, Any]) -> None:
        """Stream one `tool_calls` round for the `@tool` trigger.

        The call's arguments arrive in fragments across chunks (the shape
        real providers stream), so the client's reassembly is exercised too.
        """
        model = self._model(body)
        created = int(time.time())
        delay = getattr(self.server, "delay", 0.0)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        call_chunks: list[list[dict[str, Any]]] = [
            [
                {
                    "index": 0,
                    "id": "call-test-1",
                    "type": "function",
                    "function": {"name": TOOL_NAME, "arguments": ""},
                }
            ],
            [{"index": 0, "function": {"arguments": '{"note":'}}],
            [{"index": 0, "function": {'arguments': '"called by the test server"}'}}],
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
    ) -> None:
        super().__init__(address, ChatHandler)
        self.delay = delay
        self.default_model = default_model
        self.quiet = quiet


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
    args = parser.parse_args()

    server = TestServer(
        (args.host, args.port),
        delay=max(0.0, args.delay),
        default_model=args.model,
        quiet=args.quiet,
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
