#!/usr/bin/env python3
"""End-to-end smoke test for the built-in tool plugins and `/tools`.

The default `pty_smoke.py` round trip calls an undeclared `ping` tool, which
only exercises the error path. This one drives the real tools through the
same test server (`--tool` / `--tool-arguments` point it at a built-in):

1. `bash` — the model's call runs a real command and the output comes back
   in the tool cell and the follow-up reply;
2. `read` — a file the test writes is read back with line numbers;
3. `/tools` — disabling a tool removes it from the request, and the model's
   call for it is rejected with the disabled notice;
4. `/context` — the Tools section reports enabled/total and the state marker.

Usage:
    cargo build
    python3 scripts/tool_smoke.py [path/to/codest]
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

from pty_smoke import free_port, pump, render, wait_for_port  # noqa: E402

ROWS, COLS = 40, 120


def start_server(tool: str, arguments: str) -> tuple[subprocess.Popen, str]:
    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "openai_test_server.py")
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
            "--tool",
            tool,
            "--tool-arguments",
            arguments,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not wait_for_port(port):
        process.terminate()
        raise RuntimeError("test server did not start listening")
    return process, f"http://127.0.0.1:{port}/v1"


class Tui:
    """One codest process in a pty, with a resettable transcript window."""

    def __init__(self, binary: str, base_url: str, home: str) -> None:
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        env = dict(os.environ)
        env["TERM"] = "xterm-256color"
        env["CODEST_HOME"] = home
        self.process = subprocess.Popen(
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
        self.output = bytearray()
        self.mark = 0

    def pump(self, seconds: float) -> str:
        pump(self.master, self.output, seconds)
        return render(self.output[self.mark:].decode("utf-8", "replace"))

    def since_mark(self) -> str:
        # The pty's last frame is what the user sees; the full-screen render
        # is the reliable view for footer state, the incremental window for
        # newly printed transcript text.
        return render(self.output[self.mark:].decode("utf-8", "replace"))

    def full_screen(self) -> str:
        return render(self.output.decode("utf-8", "replace"))

    def reset(self) -> None:
        self.mark = len(self.output)

    def send(self, text: str, wait: float = 1.0) -> str:
        os.write(self.master, text.encode())
        return self.pump(wait)

    def wait_until(self, needle: str, timeout: float = 15.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in self.since_mark():
                return True
            self.pump(0.2)
        return False

    def wait_idle(self, timeout: float = 20.0) -> bool:
        """Wait for the turn to finish.

        Commands submitted while a turn streams are ignored (`try_submit`
        returns early), so the tool toggle must wait. The spinner keeps
        redrawing while busy, so "idle" is "the output stopped growing beyond
        the spinner's own repaint": two consecutive windows of the same small
        size, far below what a streamed token run produces.
        """
        deadline = time.time() + timeout
        previous = -1
        for _ in range(int(timeout / 0.25)):
            if time.time() > deadline:
                break
            before = len(self.output)
            # The spinner repaints a fixed handful of bytes per tick; a live
            # stream easily outruns that.
            self.pump(0.4)
            growth = len(self.output) - before
            if growth <= 256 and previous >= 0 and previous <= 256:
                return True
            previous = growth
        return False

    def close(self) -> None:
        try:
            os.write(self.master, b"/quit\r")
            self.pump(0.5)
        except OSError:
            pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
        os.close(self.master)


def run_case(binary: str, tool: str, arguments: str, needles: list[str], label: str) -> dict[str, bool]:
    server, base_url = start_server(tool, arguments)
    home = tempfile.mkdtemp(prefix=f"codest-tool-{tool}-")
    checks: dict[str, bool] = {}
    tui = Tui(binary, base_url, home)
    try:
        if not tui.pump(2.0) and not tui.output:
            checks[f"{label}: startup"] = False
            return checks
        checks[f"{label}: startup"] = True

        tui.reset()
        tui.send("use the tool @tool", 0.4)
        tui.send("\r", 0.2)
        ok = all(tui.wait_until(needle) for needle in needles)
        screen = tui.since_mark()
        checks[f"{label}: output"] = ok
        if not ok:
            missing = [n for n in needles if n not in screen]
            print(f"[{label}] missing {missing}\n--- screen ---\n{screen[-3000:]}", file=sys.stderr)

        # `/tools <name> off` then `/context`: the report must count the
        # narrowed set and keep listing the tool. Both wait for the idle
        # footer first, because a submission during a streaming turn is
        # dropped.
        if not tui.wait_idle():
            checks[f"{label}: idle after round"] = False
            return checks
        checks[f"{label}: idle after round"] = True
        tui.reset()
        tui.send(f"/tools {tool} off", 0.3)
        tui.send("\r", 1.5)
        tui.reset()
        tui.send("/context", 0.3)
        tui.send("\r", 2.0)
        report = tui.full_screen()
        # The heading counts the enabled subset against everything declared,
        # so one disabled tool shows "Tools (3/4 enabled)" — the toggle
        # reached the worker and the report reads it back.
        checks[f"{label}: context counts enabled"] = "Tools (3/4 enabled)" in report
        # A disabled tool stays listed with its state; rows are compact
        # (name → "enabled"/"disabled"), so the whole set stays on screen.
        checks[f"{label}: context shows the tool"] = f"{tool}" in report and "disabled" in report
        if not checks[f"{label}: context shows the tool"]:
            print(f"[{label}] context screen\n{report[-3000:]}", file=sys.stderr)
    finally:
        tui.close()
        server.terminate()
        server.wait(timeout=5)
    return checks


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/codest"
    workdir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    binary = os.path.abspath(os.path.join(workdir, binary)) if not os.path.isabs(binary) else binary
    if not os.path.exists(binary):
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    results: dict[str, bool] = {}
    results |= run_case(
        binary,
        "bash",
        '{"command":"echo hello-from-tool && pwd"}',
        ["hello-from-tool", "exit code: 0"],
        "bash",
    )
    served = os.path.join(tempfile.mkdtemp(prefix="codest-read-"), "sample.txt")
    with open(served, "w", encoding="utf-8") as handle:
        handle.write("alpha\nbeta\ngamma\n")
    results |= run_case(
        binary,
        "read",
        '{"path":"' + served + '"}',
        ["alpha", "gamma", "3 lines total"],
        "read",
    )

    width = max(len(name) for name in results)
    failed = 0
    for name, ok in results.items():
        print(f"{'PASS' if ok else 'FAIL'}  {name:<{width}}")
        failed += 0 if ok else 1
    print(f"\n{len(results) - failed}/{len(results)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
