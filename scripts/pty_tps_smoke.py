#!/usr/bin/env python3
"""TPS-meter smoke test: `/tps` on, one streamed turn, footer readouts.

Reuses scripts/pty_smoke.py's PTY plumbing and ANSI screen model. The local
streaming test server runs with a per-token delay slow enough to cross the
meter's 500 ms noise floor, so the live readout appears mid-stream and the
settled turn retires it.

The assertions pin the meter's reporting contract, not just "something
rendered":

* while streaming, the footer carries a live `N t/s` readout;
* after the turn, that number is gone — a completed turn leaves no frozen
  rate behind. One sample is not a trend, so the first turn shows no
  sparkline either (a lone block plus a lone number is exactly the sticky
  footer this test was written to catch);
* `/new` swaps the conversation, so the meter carries nothing across.

Usage:
    cargo build
    python3 scripts/pty_tps_smoke.py [path/to/codest]
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


def footer(screen: str) -> str:
    """The status line: the last non-empty row of the rendered screen."""
    for line in reversed(screen.splitlines()):
        if line.strip():
            return line
    return ""


def start_server(delay: float) -> tuple[subprocess.Popen, str]:
    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "openai_test_server.py")
    port = smoke.free_port()
    process = subprocess.Popen(
        [sys.executable, script, "--port", str(port), "--delay", str(delay), "--quiet"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not smoke.wait_for_port(port):
        process.terminate()
        raise RuntimeError("test server did not start listening")
    return process, f"http://127.0.0.1:{port}/v1"


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join("target", "debug", "codest")
    server, base_url = start_server(0.02)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", smoke.ROWS, smoke.COLS, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    # Keep the smoke session out of the user's real ~/.codest.
    env["CODEST_HOME"] = tempfile.mkdtemp(prefix="codest-tps-")
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
        stderr=slave,
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

        # Enable the meter: `/tps` prints a notice.
        os.write(master, b"/tps")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\r")
        smoke.pump(master, output, 0.5)
        screen = smoke.render(output.decode("utf-8", "replace"))
        checks["meter on notice"] = "tps meter on" in screen

        # One streamed turn. Capture mid-stream: the live readout needs a
        # decode span past the 500 ms floor, which the slowed server gives.
        os.write(master, b"hello from pty")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\r")
        smoke.pump(master, output, 1.5)
        mid = smoke.render(output.decode("utf-8", "replace"))
        print("--- screen (mid-stream) ---")
        print(mid)
        checks["live t/s while streaming"] = " t/s" in mid and "working" in mid

        # Let the turn settle. The rate must retire: a completed turn leaves
        # no frozen number in the footer.
        smoke.pump(master, output, 14.0)
        done = smoke.render(output.decode("utf-8", "replace"))
        print("--- screen (settled) ---")
        print(done)
        settled = footer(done)
        checks["live rate retires with the turn"] = " t/s" not in settled
        # One turn is one sample: no sparkline yet, and certainly no lone
        # frozen number.
        checks["no sparkline after a single turn"] = not any(
            block in settled for block in "▁▂▃▄▅▆▇█"
        )

        # A second turn gives the sparkline something to compare: two samples
        # render as blocks, and still no number.
        os.write(master, b"second turn")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\r")
        smoke.pump(master, output, 14.0)
        two = footer(smoke.render(output.decode("utf-8", "replace")))
        print("--- footer (two turns) ---")
        print(two)
        checks["sparkline after two turns"] = any(
            block in two for block in "▁▂▃▄▅▆▇█"
        ) and " t/s" not in two

        # `/new` switches the conversation; the meter keeps running but its
        # history belongs to the session that just left.
        os.write(master, b"/new")
        smoke.pump(master, output, 0.3)
        os.write(master, b"\r")
        smoke.pump(master, output, 1.0)
        fresh = footer(smoke.render(output.decode("utf-8", "replace")))
        checks["a new session carries no samples"] = not any(
            block in fresh for block in "▁▂▃▄▅▆▇█"
        ) and " t/s" not in fresh

        # Quit: the first Ctrl+C interrupts a running turn or clears the
        # composer, the next two quit.
        for _ in range(3):
            if process.poll() is not None:
                break
            try:
                os.write(master, b"\x03")
            except OSError:
                break
            smoke.pump(master, output, 0.5)
        checks["clean exit"] = process.poll() is not None or process.wait(timeout=2) == 0
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
        try:
            os.close(master)
        except OSError:
            pass
        server.terminate()

    failed = [name for name, passed in checks.items() if not passed]
    for name, passed in checks.items():
        print(f"{'PASS' if passed else 'FAIL'}  {name}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
