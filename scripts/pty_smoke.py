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
from pathlib import Path

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


def continue_check(binary, mode, env, endpoint_args, expected_label, expected_context):
    """Second run with --continue over the same CODEST_HOME: the restored
    session must bring back both its last model (footer label) and its
    recorded usage (footer context segment)."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    process = subprocess.Popen(
        [binary, "--tui-mode", mode, "--continue", *endpoint_args],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)
    output = bytearray()
    before_quit = 0
    try:
        if not pump(master, output, 1.5):
            print(f"[{mode}] continue run exited during startup", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"continue restore": False}
        # Snapshot before quitting: inline mode clears its pane on exit, so
        # only the pre-quit raw still shows the footer.
        before_quit = len(output)
        for _ in range(3):
            # The process quits on the second Ctrl+C; writing to the closed
            # master raises EIO, so stop as soon as it is gone.
            if process.poll() is not None:
                break
            try:
                os.write(master, b"\x03")
            except OSError:
                break
            pump(master, output, 0.4)
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
    screen = render(output[:before_quit].decode("utf-8", "replace"))
    print(f"--- {mode} screen (continue) ---")
    print(screen)
    passed = (
        f"codest · {expected_label}" in screen
        and expected_context in screen
        and (process.poll() is not None or process.returncode == 0)
    )
    print(f"{'PASS' if passed else 'FAIL'}  continue restore")
    return {"continue restore": passed}


def run_case(binary: str, mode: str, base_url: str, variant: str = "flags") -> dict[str, bool]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    # Keep smoke-test sessions out of the user's real ~/.codest.
    home = tempfile.mkdtemp(prefix="codest-pty-")
    env["CODEST_HOME"] = home
    if variant == "config":
        # The multi-provider path: a providers.toml instance pointing at the
        # test server plus a config.toml default selection. The flag/env
        # fallback stays covered by the flag-driven case below. `test-model`
        # declares its context_window statically; `second-model` is left to
        # discovery, which picks up the `context_window` the server serves —
        # so both metadata paths show up in the footer.
        with open(os.path.join(home, "providers.toml"), "w", encoding="utf-8") as handle:
            handle.write(
                "[[providers]]\n"
                'name = "pytest"\n'
                f'base_url = "{base_url}"\n'
                'api_key = "test-key"\n'
                "fetch_models = true\n"
                "\n"
                "  [[providers.models]]\n"
                '  id = "test-model"\n'
                "  context_window = 100000\n"
            )
        with open(os.path.join(home, "config.toml"), "w", encoding="utf-8") as handle:
            handle.write('default = "pytest/test-model"\n')
    elif variant in ("empty", "partial"):
        # `empty`: nothing configured at all — an empty route (`no model`
        # footer) that prompts instead of sending. `partial`: only one half
        # of the base_url/api_key pair — prompts name the missing piece.
        for key in list(env):
            if key.startswith("CST_") or key in ("OPENAI_API_KEY", "OPENAI_BASE_URL"):
                del env[key]
    if variant == "flags":
        endpoint_args = ["--base-url", base_url, "--api-key", "test-key", "--model", "test-model"]
    elif variant == "partial":
        # One half of the pair only: codest must run, arm the route, and ask
        # for the missing `api_key` at use time.
        endpoint_args = ["--base-url", base_url, "--model", "test-model"]
    else:
        endpoint_args = []
    process = subprocess.Popen(
        [
            binary,
            "--tui-mode",
            mode,
            *endpoint_args,
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

        if variant in ("empty", "partial"):
            # Empty route: commands keep working, messages get guidance and
            # the composer keeps its content, and no turn is ever queued.
            # Partial: the guidance names the missing half of the pair.
            os.write(master, b"/help")
            pump(master, output, 0.4)
            os.write(master, b"\r")
            pump(master, output, 0.5)
            os.write(master, b"hello from pty")
            pump(master, output, 0.4)
            os.write(master, b"\r")
            if not pump(master, output, 1.0):
                print(f"[{mode}] process exited on the empty-route send", file=sys.stderr)
                print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
                return {"empty_send": False}
            raw_after = output.decode("utf-8", "replace")
            screen = render(raw_after)
            print(f"--- {mode} screen (empty route) ---")
            print(screen)
            print(f"--- {mode} checks ---")
            if variant == "empty":
                checks = {
                    "no model footer": "codest · no model" in screen,
                    "no model banner": "backend: no model" in raw_after,
                    "commands work": "/clear" in screen and "/quit" in screen,
                    "guidance on send": "no model is configured" in screen,
                    "composer preserved": "› hello from pty" in screen,
                    "idle state": "ready" in screen,
                    "clean exit": True,  # set below after quitting
                }
            else:
                checks = {
                    "partial footer": "codest · untitled/test-model" in screen,
                    "commands work": "/clear" in screen and "/quit" in screen,
                    "guidance names missing key": "missing `api_key`" in screen,
                    "composer preserved": "› hello from pty" in screen,
                    "idle state": "ready" in screen,
                    "clean exit": True,  # set below after quitting
                }
            # Quit: first Ctrl+C clears the untouched composer, the next two quit.
            for _ in range(3):
                os.write(master, b"\x03")
                pump(master, output, 0.4)
            checks["clean exit"] = process.poll() is not None or process.wait(timeout=2) == 0
            for name, passed in checks.items():
                print(f"{'PASS' if passed else 'FAIL'}  {name}")
            return checks

        # Snapshot the startup screen before the first turn: the footer's
        # window-only placeholder (`—/100.0k`) is only visible until the
        # first measured turn overwrites it.
        startup_end = len(output)

        # First turn: a single-line message.
        os.write(master, b"hello from pty")
        pump(master, output, 0.4)
        os.write(master, b"\r")
        if not pump(master, output, 3.0):
            print(f"[{mode}] process exited while streaming", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"streaming": False}
        turn1_end = len(output)

        if variant == "config":
            # `/model` picker: the discovery notice may have landed already;
            # open the menu, move to the fetched second model, switch.
            os.write(master, b"/model")
            pump(master, output, 0.4)
            os.write(master, b"\r")
            pump(master, output, 0.5)
            menu_end = len(output)
            os.write(master, b"\x1b[B")
            pump(master, output, 0.2)
            os.write(master, b"\r")
            if not pump(master, output, 1.0):
                print(f"[{mode}] process exited in the model menu", file=sys.stderr)
                print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
                return {"model_menu": False}
            raw_menu = output[menu_end:].decode("utf-8", "replace")

        # Second turn: grow the composer with Ctrl+J before sending, which
        # exercises the inline pane's height changes.
        os.write(master, b"line one")
        pump(master, output, 0.3)
        os.write(master, b"\n")
        pump(master, output, 0.3)
        os.write(master, b"line two")
        pump(master, output, 0.3)
        os.write(master, b"\r")
        if not pump(master, output, 5.0):
            print(f"[{mode}] process exited on the second turn", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"streaming": False}
        turn2_end = len(output)

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
    raw_startup = output[:startup_end].decode("utf-8", "replace")
    raw_turn1 = output[:turn1_end].decode("utf-8", "replace")
    # The second turn is checked before the wheel section: the scroll test
    # legitimately moves the viewport to the top, which pushes the tail of
    # the transcript below the fold in longer (config) transcripts.
    raw_turn2 = output[:turn2_end].decode("utf-8", "replace")
    raw_before_quit = output[:before_quit].decode("utf-8", "replace")
    screen_startup = render(raw_startup)
    screen_turn1 = render(raw_turn1)
    screen_turn2 = render(raw_turn2)
    screen = render(raw_before_quit)
    if os.environ.get("PTY_RAW_OUT"):
        with open(os.environ["PTY_RAW_OUT"], "w", encoding="utf-8") as handle:
            handle.write(raw)
    print(f"--- {mode} screen (after turn 1) ---")
    print(screen_turn1)
    print(f"--- {mode} screen (before quit) ---")
    print(screen)
    print(f"--- {mode} checks ---")

    if variant == "config":
        expected_label = "pytest/second-model"
    else:
        # The flags variant synthesizes its instance under the literal name
        # `untitled` (see cli.rs `synthesized_openai_instance`).
        expected_label = "untitled/test-model"
    checks = {
        "first user message": "› hello from pty" in screen_turn1,
        "first reply": 'You said: "hello from pty"' in screen_turn1,
        "second user message": "› line one" in screen_turn2,
        "second reply": 'You said: "line one' in screen_turn2,
        "no stray cursor": "▌" not in screen,
        "test server footer": f"codest · {expected_label}" in screen,
        "idle state": "ready" in screen,
        "mode notice": f"mode: {mode}" in raw_turn1,
        "clean exit": process.returncode == 0,
    }
    if variant == "config":
        # Discovery + picker + switch: the test server lists test-model and
        # second-model; the menu switched to the second one.
        checks["discovery notice"] = "model discovery for pytest found 2 models" in raw
        checks["menu lists fetched model"] = "pytest/test-model" in raw_menu
        checks["menu lists second model"] = "pytest/second-model" in raw_menu
        # The optimistic "switched to …" notice lands in the transcript, but
        # on a 24-row pty it can sit below the fold; the footer label is the
        # authoritative evidence that the picker switch happened.
        checks["switch notice"] = f"codest · {expected_label}" in screen
        # Declared context_window + the streamed usage chunk: the footer
        # shows the measured tokens against the window. The total is visible
        # from the first frame, before anything is measured.
        checks["context window before first turn"] = "· —/100.0k" in screen_startup
        # After the picker switch the active model is the *discovered*
        # second-model, whose window came from the server's context_window
        # field — the discovery path, distinct from the declared one above.
        checks["context window footer"] = "· 49/200.0k (0%)" in screen
    else:
        # The flags instance discovers its models: `test-model`'s window comes
        # from the server's `context_length` field, so the footer shows the
        # measured tokens against it (42 in + 7 out per streamed turn).
        checks["context window footer (discovered)"] = "· 49/128.0k (0%)" in screen
    # The rollout records each completed turn's usage with the serving
    # model; the test server reports 42 in / 7 out per turn.
    rollout_text = "".join(
        path.read_text(encoding="utf-8")
        for path in Path(home).glob("sessions/*/rollout-*.jsonl")
    )
    checks["rollout records turn usage"] = (
        '"type":"turn_usage"' in rollout_text and '"input":42' in rollout_text
    )
    # Second run with --continue over the same home: the restored session
    # brings back the model it last ran on plus its recorded usage. The
    # discovered second-model's window survives via the model cache and
    # re-discovery alike.
    if variant == "config":
        checks.update(
            continue_check(
                binary, mode, env, endpoint_args, "pytest/second-model", "· 49/200.0k (0%)"
            )
        )
    else:
        checks.update(
            continue_check(
                binary, mode, env, endpoint_args, "untitled/test-model", "· 49/128.0k (0%)"
            )
        )
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


def tool_smoke(binary: str, mode: str, base_url: str) -> dict[str, bool]:
    """The tool-calling round trip: `@tool` makes the test server stream a
    `ping` tool_call; no tool is registered, so codest feeds the unknown-tool
    error back and the model's next reply summarizes the round."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    home = tempfile.mkdtemp(prefix="codest-pty-tool-")
    env["CODEST_HOME"] = home
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
    turn_start = 0
    turn_end = 0
    before_quit = 0
    try:
        if not pump(master, output, 1.5):
            print(f"[{mode}] tool smoke exited during startup", file=sys.stderr)
            print(output.decode("utf-8", "replace")[-4000:], file=sys.stderr)
            return {"tool startup": False}
        turn_start = len(output)
        os.write(master, b"use the ping tool @tool")
        pump(master, output, 0.4)
        os.write(master, b"\r")
        # Snapshot the moment the ⚙ tool cell renders (it scrolls off as the
        # summary reply streams in), then wait for the idle footer: the round
        # spans two streamed requests, so durations are unreliable.
        deadline = time.time() + 10
        tool_end = 0
        while time.time() < deadline:
            if "⚙" in output[turn_start:].decode("utf-8", "replace"):
                tool_end = len(output)
                break
            if not pump(master, output, 0.2):
                break
        # `/context` needs an idle agent. The turn's `ready` footer does not
        # survive the render of a full-screen repaint, so wait on the output
        # rate instead: the spinner's fixed per-tick repaint is far smaller
        # than a live stream.
        previous = -1
        for _ in range(80):
            before = len(output)
            if not pump(master, output, 0.4):
                break
            growth = len(output) - before
            if growth <= 256 and 0 <= previous <= 256:
                break
            previous = growth
        turn_end = len(output)
        # `/context` needs an idle agent; the turn is done by now. The report
        # is printed into a transcript that is already taller than the
        # 24-row grid, so the visible frame shows only its tail. Assertions
        # therefore read the *rendered transcript text* (via `screen_turn`
        # plus the report's own stream) rather than assuming what fits.
        os.write(master, b"/context")
        pump(master, output, 0.3)
        os.write(master, b"\r")
        pump(master, output, 1.2)
        # The report parks the viewport on its first line (the useful part),
        # so its tail — Request, Usage — is one PgDn away. Scroll once and
        # capture that frame too, the way a reader would.
        before_quit = len(output)
        for _ in range(2):
            os.write(master, b"\x1b[6~")  # PgDn
            pump(master, output, 0.2)
        report_tail_end = len(output)
        for _ in range(3):
            if process.poll() is not None:
                break
            try:
                os.write(master, b"\x03")
            except OSError:
                break
            pump(master, output, 0.4)
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
    screen_tool = render(output[:tool_end].decode("utf-8", "replace"))
    screen_turn = render(output[:turn_end].decode("utf-8", "replace"))
    raw_before_quit = output[:before_quit].decode("utf-8", "replace")
    screen = render(raw_before_quit)
    # The report is taller than the grid and parks on its first line, so the
    # frame right after `/context` shows its head and the frame after PgDn its
    # tail. Fullscreen repaints only the visible window and inline prints to
    # the terminal's scrollback, so the assertions read the plain text of the
    # whole capture (escape sequences and cursor moves stripped) plus the
    # scrolled frame — the closest a pty test gets to scrolling back.
    plain = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07", "", raw_before_quit)
    report_tail = render(output[:report_tail_end].decode("utf-8", "replace"))
    report_screen = raw_before_quit + "\n" + plain + "\n" + report_tail

    print(f"--- {mode} screen (tool cell) ---")
    print(screen_tool)
    print(f"--- {mode} screen (tool round) ---")
    print(screen)
    rollout_text = "".join(
        path.read_text(encoding="utf-8")
        for path in Path(home).glob("sessions/*/rollout-*.jsonl")
    )
    checks = {
        "tool call rendered": '⚙ ping {"note":"called by the test server"}' in screen_tool,
        "unknown tool surfaced": "unknown tool `ping`" in screen_turn,
        "round trip reply": "tool-calling round trip of the test server" in screen_turn,
        # The report is taller than the grid, so these read the transcript
        # text (`report_screen`), not the visible frame.
        "context report title": "▪ context" in report_screen,
        "context shows sections": "System prompt" in report_screen,
        "context shows request": "/chat/completions" in report_screen,
        "context shows tools": "Tools (" in report_screen,
        "clean exit": process.poll() is not None or process.returncode == 0,
    }
    checks["rollout records tool round"] = (
        '"type":"assistant_tool_calls"' in rollout_text
        and '"type":"tool_result"' in rollout_text
        and '"name":"ping"' in rollout_text
    )
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
    parser.add_argument(
        "--skip-tool",
        action="store_true",
        help="skip the tool-calling round-trip scenario",
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
            for variant in ("flags", "config", "empty", "partial"):
                label = f"{mode}/{variant}"
                print(f"=== {label} ===")
                checks = run_case(args.binary, mode, base_url, variant=variant)
                passed = passed and all(checks.values())
                print()
            if not args.skip_tool:
                print(f"=== {mode}/tool ===")
                checks = tool_smoke(args.binary, mode, base_url)
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
