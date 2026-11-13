#!/usr/bin/env python3
"""Full-frame screen smoke test: `/settings` and `/context` navigation.

Both screens now share one frame (src/tui/screen.rs), so the behaviours worth
pinning at the terminal level are the ones the unit tests can only assert on
state:

* `/settings` renders cards and its `n/m` counter, and `↑` on the first row
  wraps to the last;
* the mouse wheel scrolls the list without moving the focus — the counter
  stays where it was while the visible rows change;
* `/context` renders the entry list plus its inspector panel, and wraps the
  same way.

Usage:
    cargo build
    python3 scripts/pty_screen_smoke.py [path/to/codest]
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_smoke as smoke
import pty_tps_smoke as tps

# A wheel notch: SGR mouse reporting, button 64 = wheel up, 65 = wheel down.
WHEEL_UP = b"\x1b[<64;10;10M"
WHEEL_DOWN = b"\x1b[<65;10;10M"


def flat(screen: str) -> str:
    """The whole rendered screen with runs of spaces collapsed, so a needle
    can match across the cell grid."""
    return " ".join(screen.split())


def counter(screen: str) -> str | None:
    """The `n/m` counter the frame pins to the header's right edge."""
    for token in flat(screen).split():
        if len(token) >= 3 and token[0].isdigit() and "/" in token:
            left, _, right = token.partition("/")
            if left.isdigit() and right.isdigit():
                return token
    return None


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join("target", "debug", "codest")
    # A local streaming server gives the session a route, so `/context` can
    # open its in-session viewer rather than the pre-session report.
    server, base_url = tps.start_server(0.0)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", smoke.ROWS, smoke.COLS, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["CODEST_HOME"] = tempfile.mkdtemp(prefix="codest-screen-")
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
        # Startup diagnostics go to stderr; keeping them off the PTY keeps the
        # rendered screen free of pre-TUI chatter the screen model would
        # mistake for content.
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

        # --- /settings ---------------------------------------------------
        os.write(master, b"/settings")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\r")
        smoke.pump(master, output, 0.6)
        screen = smoke.render(output.decode("utf-8", "replace"))
        text = flat(screen)
        checks["settings renders"] = "Settings" in screen
        checks["host card shown"] = "Plugin status segments" in text
        checks["plugin card shown"] = "throughput meter" in text
        first = counter(screen)
        checks["counter on first row"] = first is not None and first.startswith("1/")
        checks["help bar keys"] = "Enter toggle" in text

        # ↑ on the first row wraps to the last.
        os.write(master, b"\x1b[A")
        smoke.pump(master, output, 0.4)
        screen = smoke.render(output.decode("utf-8", "replace"))
        last = counter(screen)
        checks["up wraps to the last row"] = (
            first is not None and last is not None and last != first and last.startswith("2/")
        )

        # The wheel scrolls the list but leaves the focus (the counter) alone.
        before = counter(screen)
        os.write(master, WHEEL_DOWN)
        smoke.pump(master, output, 0.4)
        screen = smoke.render(output.decode("utf-8", "replace"))
        checks["wheel keeps the focus"] = counter(screen) == before

        # Esc returns to the chat.
        os.write(master, b"\x1b")
        smoke.pump(master, output, 0.4)
        screen = smoke.render(output.decode("utf-8", "replace"))
        checks["settings closes"] = "Settings" not in screen

        # --- /context ----------------------------------------------------
        # Without a transcript `/context` prints the prompt report; the
        # viewer needs messages, so this pins that the command works and
        # does not crash after the settings screen closed.
        os.write(master, b"/context")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\r")
        smoke.pump(master, output, 0.6)
        screen = smoke.render(output.decode("utf-8", "replace"))
        checks["context renders"] = "Tools" in screen or "System prompt" in screen

        # With no messages the report is a transcript cell, not the viewer,
        # so close it and drive real turns through the test server to get
        # one. Enough rounds that the entry list is taller than the viewport
        # — at 24 rows the list area holds 13 entries, so the wheel has
        # nowhere to go below that.
        if server is not None:
            for index in range(10):
                os.write(master, f"smoke turn {index}".encode())
                smoke.pump(master, output, 0.05)
                os.write(master, b"\r")
                smoke.pump(master, output, 0.8)
            os.write(master, b"/context")
            smoke.pump(master, output, 0.3)
            os.write(master, b"\r")
            smoke.pump(master, output, 0.8)
            screen = smoke.render(output.decode("utf-8", "replace"))
            text = flat(screen)
            checks["context viewer shows the list"] = "system + transcript" in text
            checks["context viewer shows the inspector"] = "chars · transcript #" in text
            checks["context help bar"] = "u undo" in text
            first = counter(screen)
            checks["context counter"] = first is not None and first.startswith("1/")
            os.write(master, b"\x1b[A")
            smoke.pump(master, output, 0.4)
            screen = smoke.render(output.decode("utf-8", "replace"))
            last = counter(screen)
            checks["context up wraps to the last entry"] = (
                first is not None
                and last is not None
                and last != first
                and last.split("/")[0] == last.split("/")[1]
            )
            os.write(master, b"\x1b[B")
            smoke.pump(master, output, 0.4)
            screen = smoke.render(output.decode("utf-8", "replace"))
            checks["context down wraps back to the first"] = counter(screen) == first

            # The wheel scrolls the list while the counter (the selection)
            # stays put. This is the case that regressed before: the next
            # frame pulled the viewport back to the selection.
            rows_before = screen.splitlines()
            os.write(master, WHEEL_DOWN)
            smoke.pump(master, output, 0.5)
            rows_after = smoke.render(output.decode("utf-8", "replace")).splitlines()
            checks["context wheel scrolls the list"] = rows_before[:6] != rows_after[:6]
            checks["context wheel keeps the selection"] = counter(
                "\n".join(rows_after)
            ) == counter("\n".join(rows_before))

        os.write(master, b"\x03")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\x03")
        smoke.pump(master, output, 0.5)
    finally:
        try:
            process.terminate()
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
        server.terminate()

    failed = [name for name, ok in checks.items() if not ok]
    for name, ok in checks.items():
        print(("PASS  " if ok else "FAIL  ") + name)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
