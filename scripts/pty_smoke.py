#!/usr/bin/env python3
"""Offline end-to-end smoke test for the codest TUI.

Spawns the compiled binary in a pseudo-terminal, types a message, waits for a
local OpenAI-compatible server to stream a reply, and renders the captured ANSI
output to a plain text grid so assertions are not confused by cursor-positioning
sequences.

The test server is `scripts/openai_test_server.py`; it is started automatically
on an ephemeral loopback port, so no external network access is required.

Both TUI modes are exercised by default: `fullscreen` must enter the alternate
screen and capture the mouse, while `inline` must leave the terminal's native
scrollback in place.

Usage:
    cargo build
    python3 scripts/pty_smoke.py [path/to/codest] [--mode fullscreen|inline|both]
"""

from __future__ import annotations

import argparse
import fcntl
import os
import pty
import re
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROWS = 24
COLS = 100
ESC_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


def pump(master: int, out: bytearray, seconds: float) -> bool:
    """Read output for `seconds`, answering cursor-position queries."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([master], [], [], 0.05)
        if master not in ready:
            continue
        try:
            data = os.read(master, 65536)
        except OSError:
            return False
        if not data:
            return False
        out.extend(data)
        while b"\x1b[6n" in out:
            index = out.find(b"\x1b[6n")
            del out[index : index + 4]
            os.write(master, b"\x1b[1;1R")
    return True


def scroll_up(grid: list[list[str]], amount: int) -> None:
    for _ in range(amount):
        grid.pop(0)
        grid.append([" "] * COLS)


def render(raw: str) -> str:
    """Minimal ANSI screen model, enough for ratatui's output."""
    if "\x1b[?1049l" in raw:
        raw = raw.rsplit("\x1b[?1049l", 1)[0]

    grid = [[" "] * COLS for _ in range(ROWS)]
    row = 0
    column = 0
    index = 0
    # Terminals defer wrapping at the right margin until the next printable
    # character; eagerly wrapping shifts every following row by one.
    wrap_pending = False

    while index < len(raw):
        char = raw[index]
        if char == "\x1b":
            match = ESC_RE.match(raw, index)
            if not match:
                index += 1
                continue
            sequence = match.group(0)
            final = sequence[-1]
            params = sequence[2:-1]

            if final in "Hf":
                parts = params.split(";")
                row = int(parts[0]) - 1 if parts and parts[0] else 0
                column = int(parts[1]) - 1 if len(parts) > 1 and parts[1] else 0
                wrap_pending = False
            elif final == "J":
                wrap_pending = False
                if params == "2":
                    grid = [[" "] * COLS for _ in range(ROWS)]
                elif params in ("", "0"):
                    grid[row][column:] = [" "] * (COLS - column)
                    for line in range(row + 1, ROWS):
                        grid[line] = [" "] * COLS
            elif final == "K":
                wrap_pending = False
                if params == "2":
                    grid[row] = [" "] * COLS
                elif params in ("", "0"):
                    grid[row][column:] = [" "] * (COLS - column)
            elif final == "A":
                row = max(0, row - 1)
                wrap_pending = False
            elif final == "B":
                row = min(ROWS - 1, row + 1)
                wrap_pending = False
            elif final == "C":
                column = min(COLS - 1, column + 1)
                wrap_pending = False
            elif final == "D":
                column = max(0, column - 1)
                wrap_pending = False
            elif final == "G":
                column = max(0, int(params or "1") - 1)
                wrap_pending = False
            elif final == "S":
                scroll_up(grid, max(1, int(params or "1")))
                wrap_pending = False

            index += len(sequence)
            continue

        if char == "\r":
            column = 0
            wrap_pending = False
        elif char == "\n":
            if row == ROWS - 1:
                scroll_up(grid, 1)
            else:
                row += 1
            wrap_pending = False
        elif ord(char) >= 32:
            if wrap_pending:
                column = 0
                if row == ROWS - 1:
                    scroll_up(grid, 1)
                else:
                    row += 1
                wrap_pending = False
            if 0 <= row < ROWS and 0 <= column < COLS:
                grid[row][column] = char
            if column == COLS - 1:
                wrap_pending = True
            else:
                column += 1
        index += 1

    return "\n".join("".join(line).rstrip() for line in grid).rstrip("\n")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_port(port: int, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(0.2)
            if sock.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.05)
    return False


def start_test_server() -> tuple[subprocess.Popen, str]:
    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "openai_test_server.py")
    if not os.path.exists(script):
        raise RuntimeError(f"test server script not found: {script}")
    port = free_port()
    process = subprocess.Popen(
        [
            sys.executable,
            script,
            "--port",
            str(port),
            "--delay",
            "0.005",
            "--quiet",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not wait_for_port(port):
        process.terminate()
        raise RuntimeError("test server did not start listening")
    return process, f"http://127.0.0.1:{port}/v1"


def run_case(binary: str, mode: str, base_url: str) -> dict[str, bool]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    # Keep smoke-test sessions out of the user's real ~/.codest.
    env["CODEST_HOME"] = tempfile.mkdtemp(prefix="codest-pty-")
    process = subprocess.Popen(
        [
            binary,
            "--tui-mode",
            mode,
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
    turn1_end = 0
    before_quit = 0
    try:
        if not pump(master, output, 1.5):
            print(f"[{mode}] process exited during startup", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"startup": False}

        # First turn: a single-line message.
        os.write(master, b"hello from pty")
        pump(master, output, 0.4)
        os.write(master, b"\r")
        if not pump(master, output, 3.0):
            print(f"[{mode}] process exited while streaming", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"streaming": False}
        turn1_end = len(output)

        # Second turn: grow the composer with Ctrl+J before sending, which
        # exercises the inline pane's height changes.
        os.write(master, b"line one")
        pump(master, output, 0.3)
        os.write(master, b"\n")
        pump(master, output, 0.3)
        os.write(master, b"line two")
        pump(master, output, 0.3)
        os.write(master, b"\r")
        if not pump(master, output, 3.0):
            print(f"[{mode}] process exited on the second turn", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"streaming": False}

        # Fullscreen owns the mouse wheel and should scroll the transcript
        # back to the first turn after the second one overflowed the screen.
        if mode == "fullscreen":
            for _ in range(8):
                os.write(master, b"\x1b[<64;10;10M")
            pump(master, output, 0.8)

        os.write(master, b"\x03")
        pump(master, output, 0.3)
        before_quit = len(output)
        os.write(master, b"\x03")
        pump(master, output, 0.8)
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

    raw = output.decode("utf-8", "replace")
    raw_turn1 = output[:turn1_end].decode("utf-8", "replace")
    raw_before_quit = output[:before_quit].decode("utf-8", "replace")
    screen_turn1 = render(raw_turn1)
    screen = render(raw_before_quit)
    if os.environ.get("PTY_RAW_OUT"):
        with open(os.environ["PTY_RAW_OUT"], "w", encoding="utf-8") as handle:
            handle.write(raw)
    print(f"--- {mode} screen (after turn 1) ---")
    print(screen_turn1)
    print(f"--- {mode} screen (before quit) ---")
    print(screen)
    print(f"--- {mode} checks ---")

    checks = {
        "first user message": "› hello from pty" in screen_turn1,
        "first reply": 'You said: "hello from pty"' in screen_turn1,
        "second user message": "› line one" in screen,
        "second reply": 'You said: "line one' in screen,
        "no stray cursor": "▌" not in screen,
        "test server footer": "codest · test-model" in screen,
        "idle state": "ready" in screen,
        "mode notice": f"mode: {mode}" in raw_turn1,
        "clean exit": process.returncode == 0,
    }
    if mode == "fullscreen":
        checks["alternate screen"] = "\x1b[?1049h" in raw
        checks["mouse capture"] = "\x1b[?1000h" in raw
        checks["wheel scroll"] = "› hello from pty" in screen
    else:
        checks["native scrollback"] = "\x1b[?1049h" not in raw
        checks["no mouse capture"] = "\x1b[?1000h" not in raw

    for name, passed in checks.items():
        print(f"{'PASS' if passed else 'FAIL'}  {name}")
    return checks


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/codest")
    parser.add_argument(
        "--mode",
        choices=["fullscreen", "inline", "both"],
        default="both",
        help="which TUI mode to exercise (default: both)",
    )
    args = parser.parse_args()

    if not os.path.exists(args.binary):
        print(f"binary not found: {args.binary} (run `cargo build` first)", file=sys.stderr)
        return 2

    modes = ["fullscreen", "inline"] if args.mode == "both" else [args.mode]
    server, base_url = start_test_server()
    passed = True
    try:
        for mode in modes:
            checks = run_case(args.binary, mode, base_url)
            passed = passed and all(checks.values())
            print()
    finally:
        server.terminate()
        try:
            server.wait(timeout=2)
        except subprocess.TimeoutExpired:
            server.kill()
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
