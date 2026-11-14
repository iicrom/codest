#!/usr/bin/env python3
"""Question-dialog smoke test: `ask_user_question` and its `@ask-user` trigger.

The model asks instead of guessing: the fake server's `@ask-user` keyword
turns the tail into an `ask_user_question` tool call, the TUI opens the modal
question dialog, and the answer goes back through the envelope into the
follow-up reply. The behaviours worth pinning at the terminal level are the
ones the unit tests can only assert on state:

* the trigger's tail parses into the call — `@ask-user <question> | <label> |
  <label>` opens the dialog with those options;
* the dialog renders the question, its options, and the runtime
  `Type something.` row while the tool is parked;
* the dialog is fully modal — typing while it is open leaks nothing into the
  composer the user cannot see;
* picking an option closes the dialog, the tool returns the envelope, and the
  follow-up reply quotes it;
* an unparsable tail never opens the dialog: the help reply says so, naming
  the keyword;
* `Esc` declines: the follow-up reply quotes the decline sentence instead.

Usage:
    cargo build
    python3 scripts/pty_ask_smoke.py [path/to/codest]
"""

from __future__ import annotations

import fcntl
import os
import pty
import struct
import subprocess
import sys
import tempfile
import termios
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_smoke as smoke

# The question the dialog is driven with; the answer the dialog sends back is
# asserted verbatim in the follow-up reply.
ASK_PROMPT = "@ask-user Which color? | red | blue"


def flat(screen: str) -> str:
    """The whole rendered screen with runs of spaces collapsed, so a needle
    can match across the cell grid."""
    return " ".join(screen.split())


def screen_of(output: bytearray) -> str:
    return smoke.render(output.decode("utf-8", "replace"))


def pump_until(
    master: int, output: bytearray, needle: str | None, timeout: float = 8.0
) -> str:
    """Read until the rendered screen contains `needle`, then return it."""
    deadline = time.time() + timeout
    screen = screen_of(output)
    while time.time() < deadline:
        smoke.pump(master, output, 0.1)
        screen = screen_of(output)
        if needle is None or needle in flat(screen):
            return screen
    return screen


def send(master: int, output: bytearray, keys: bytes, seconds: float = 0.25) -> None:
    os.write(master, keys)
    smoke.pump(master, output, seconds)


def start_server() -> tuple[subprocess.Popen, str]:
    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "openai_test_server.py")
    port = smoke.free_port()
    process = subprocess.Popen(
        [sys.executable, script, "--port", str(port), "--delay", "0.0", "--quiet"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not smoke.wait_for_port(port):
        process.terminate()
        raise RuntimeError("test server did not start listening")
    return process, f"http://127.0.0.1:{port}/v1"


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join("target", "debug", "codest")
    server, base_url = start_server()

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", smoke.ROWS, smoke.COLS, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["CODEST_HOME"] = tempfile.mkdtemp(prefix="codest-ask-")
    process = subprocess.Popen(
        [
            binary,
            "--tui-mode",
            "fullscreen",
            "--base-url",
            base_url,
            "--api-key",
            "test-key",
            "--model",
            "test-model",
        ],
        stdin=slave,
        stdout=slave,
        stderr=subprocess.DEVNULL,
        env=env,
        close_fds=True,
    )
    os.close(slave)

    output = bytearray()
    checks: dict[str, bool] = {}
    try:
        if not smoke.pump(master, output, 1.5):
            print("process exited during startup", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return 1

        # --- turn 1: answer the question ---------------------------------
        send(master, output, f"{ASK_PROMPT}\r".encode())
        screen = pump_until(master, output, "Questions")
        text = flat(screen)
        checks["dialog renders"] = "Questions" in text and "Which color?" in text
        checks["options render"] = "( ) red" in text and "( ) blue" in text
        checks["custom row appended"] = "Type something." in text
        checks["key hint shown"] = "Esc declines all" in text

        # The dialog owns the keyboard: typing goes nowhere.
        send(master, output, b"hello")
        text = flat(screen_of(output))
        checks["typing does not leak to the composer"] = "hello" not in text

        # Pick "blue" and let the tool come back with the envelope.
        send(master, output, b"\x1b[B")  # Down
        send(master, output, b"\r")  # Enter — auto-submits: one question
        screen = pump_until(master, output, "User has answered your questions")
        text = flat(screen)
        checks["dialog closed after the answer"] = "Questions" not in text
        checks["envelope quotes the answer"] = '"Which color?"="blue"' in text
        checks["follow-up quotes the envelope"] = (
            "fed the result back: User has answered your questions" in text
        )

        # --- turn 2: an unparsable tail never opens the dialog ------------
        send(master, output, b"@ask-user no options here\r")
        screen = pump_until(master, output, "did not parse")
        text = flat(screen)
        checks["unparsable tail answers with help"] = "did not parse" in text
        # The fallback note names the keyword ("The `@ask-user` keyword was
        # there, but ...") — a phrase the keyword table itself never carries.
        checks["unparsable tail names the keyword"] = "keyword was there" in text
        checks["unparsable tail opens no dialog"] = "Questions" not in text

        # --- turn 3: decline the question --------------------------------
        send(master, output, f"{ASK_PROMPT}\r".encode())
        pump_until(master, output, "Questions")
        send(master, output, b"\x1b")  # Esc — decline the whole form
        screen = pump_until(master, output, "User declined to answer questions")
        text = flat(screen)
        checks["decline reaches the model verbatim"] = (
            "fed the result back: User declined to answer questions" in text
        )

        server.terminate()
        server.wait(timeout=5)
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()

    failed = [name for name, ok in checks.items() if not ok]
    for name, ok in checks.items():
        print(f"{'PASS' if ok else 'FAIL'}  {name}")
    if failed:
        print(f"\n{len(failed)} check(s) failed; last screen:", file=sys.stderr)
        print(screen_of(output)[-2400:], file=sys.stderr)
        return 1
    print(f"\nall {len(checks)} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
