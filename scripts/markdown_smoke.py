#!/usr/bin/env python3
"""Visual check for the markdown transcript rendering.

The default test server echoes the user message, which cannot show markdown
structure. This script stands a tiny OpenAI-compatible server in front of
codest that *answers with markdown* (headings, lists, quotes, a table, a fenced
block, inline styles), then drives codest in a pseudo-terminal and prints the
rendered screen for both TUI modes.

Usage:
    cargo build
    python3 scripts/markdown_smoke.py [path/to/codest]
"""

from __future__ import annotations

import json
import os
import pty
import struct
import subprocess
import sys
import tempfile
import threading
import time
import fcntl
import termios
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from pty_smoke import free_port, pump  # noqa: E402
import pty_smoke  # noqa: E402

ROWS, COLS = 58, 110
# `render` builds its grid from the module's own dimensions; this script uses
# a taller terminal so the whole sample message fits on one screen.
pty_smoke.ROWS, pty_smoke.COLS = ROWS, COLS


def render(raw: str) -> str:
    return pty_smoke.render(raw)

REPLY = """\
# Markdown smoke test

A paragraph with **bold**, *italic*, `inline code`, and a \
[link](https://example.test/docs).

## Lists

- first bullet
- second bullet that is long enough to wrap onto a continuation row so the \
hanging indent is visible
  - nested bullet
1. ordered one
2. ordered two

> A quote that also wraps onto a second row to show the bar gutter working.

---

```rust
fn main() {
    // a comment, a string, and a number
    let greeting = "fenced code";
    let answer = 42;
    let long_call = compute_something(greeting, answer, "a long argument list");
}
```

```python
def greet(name):
    return f"hello {name}"
```

| column a | column b |
|----------|----------|
| short    | a longer value |
| x        | y |

- [x] a finished task
- [ ] an open task
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # noqa: D102 - silence
        pass

    def do_GET(self):  # noqa: N802
        if self.path.endswith("/models"):
            body = json.dumps(
                {"data": [{"id": "md-model", "context_length": 128000}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        # Stream in small chunks so the TUI exercises its streaming path.
        for start in range(0, len(REPLY), 12):
            chunk = REPLY[start : start + 12]
            payload = {
                "choices": [
                    {"index": 0, "delta": {"content": chunk}, "finish_reason": None}
                ]
            }
            self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.004)
        usage = {
            "choices": [],
            "usage": {"prompt_tokens": 42, "completion_tokens": 7},
        }
        self.wfile.write(f"data: {json.dumps(usage)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


def serve(port: int) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def run(binary: str, mode: str, port: int) -> str:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    # A scratch home keeps the real ~/.codest sessions out of the transcript.
    env["CODEST_HOME"] = tempfile.mkdtemp(prefix="codest-md-")
    process = subprocess.Popen(
        [
            binary,
            "--tui-mode",
            mode,
            "--base-url",
            f"http://127.0.0.1:{port}/v1",
            "--api-key",
            "test-key",
            "--model",
            "md-model",
        ],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)

    output = bytearray()
    try:
        pump(master, output, 1.5)
        os.write(master, b"show me markdown")
        pump(master, output, 0.4)
        os.write(master, b"\r")
        # Poll for the reply's last line instead of sleeping a fixed time: the
        # sample is large enough that a fixed window races the paint and makes
        # the checks flaky on slower builds.
        deadline = time.time() + 20.0
        while time.time() < deadline:
            pump(master, output, 0.25)
            if "an open task" in render(output.decode("utf-8", "replace")):
                break
        pump(master, output, 0.4)
        # First Ctrl+C clears the composer, the next two quit; the process can
        # exit before the last one lands, which closes the pty.
        for _ in range(3):
            try:
                os.write(master, b"\x03")
            except OSError:
                break
            pump(master, output, 0.4)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
    finally:
        try:
            os.close(master)
        except OSError:
            pass

    return render(output.decode("utf-8", "replace"))


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join("target", "debug", "codest")
    if not os.path.exists(binary):
        print(f"binary not found: {binary} (run `cargo build` first)", file=sys.stderr)
        return 2

    port = free_port()
    server = serve(port)
    try:
        screens = {}
        for mode in ("fullscreen", "inline"):
            screen = run(binary, mode, port)
            screens[mode] = screen
            print(f"--- {mode} ---")
            print(screen)

        checks = {
            "h1 heading text": "Markdown smoke test" in screens["fullscreen"],
            "bold/italic/code inline": "bold, italic, inline code" in screens["fullscreen"],
            "bullet marker": "• first bullet" in screens["fullscreen"],
            "nested bullet": "  • nested bullet" in screens["fullscreen"],
            "ordered marker": "1. ordered one" in screens["fullscreen"],
            "quote bar": "▎ A quote" in screens["fullscreen"],
            "rule": "───" in screens["fullscreen"],
            "code fence": "fn main() {" in screens["fullscreen"],
            "code label": "│ rust" in screens["fullscreen"],
            "second code label": "│ python" in screens["fullscreen"],
            "code body": "let answer = 42;" in screens["fullscreen"],
            "table header": "column a" in screens["fullscreen"],
            "table divider": "────" in screens["fullscreen"],
            "task checkbox": "[x] a finished task" in screens["fullscreen"],
            "no raw markers": "**bold**" not in screens["fullscreen"],
            "inline mode renders markdown": "Markdown smoke test" in screens["inline"],
            "inline code label": "│ rust" in screens["inline"],
            "inline no raw markers": "**bold**" not in screens["inline"],
        }
        print("--- checks ---")
        for name, passed in checks.items():
            print(f"{'PASS' if passed else 'FAIL'}  {name}")
        return 0 if all(checks.values()) else 1
    finally:
        server.shutdown()


if __name__ == "__main__":
    raise SystemExit(main())
