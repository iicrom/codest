#!/usr/bin/env python3
"""Offline end-to-end test for `codest plugin install|uninstall`.

Checks the whole install path against a real plugin checkout:

1. `codest plugin install <path>` writes `$CODEST_HOME/plugins.toml`, generates
   the wrapper crate, and builds a binary there;
2. the generated binary really has the plugin linked — `/status` lists its id
   and `/help` lists its commands;
3. the codest checkout itself is untouched (`git status --porcelain` is empty);
4. `codest plugin uninstall` regenerates a plugin-free wrapper, and the rebuilt
   binary no longer mentions the plugin.

No network access: the TUI is driven in its empty-route state, where commands
still work and no request is ever sent. A pseudo-terminal is required because
the TUI is the only front-end.

Usage:
    cargo build
    python3 scripts/plugin_smoke.py [--plugin ../cst-provider-qoder]
                                    [--plugin-id provider-qoder]
                                    [--plugin-command qoder]
"""

from __future__ import annotations

import argparse
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

ROWS = 40
COLS = 120
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


def host_env(home: str) -> dict[str, str]:
    """Env for the host CLI: a scratch home and no inherited endpoint."""
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["CODEST_HOME"] = home
    for key in list(env):
        if key.startswith("CST_") or key in ("OPENAI_API_KEY", "OPENAI_BASE_URL"):
            del env[key]
    return env


def run_cli(binary: str, env: dict[str, str], *args: str) -> subprocess.CompletedProcess:
    """Run the host binary with a subcommand, capturing output."""
    return subprocess.run(
        [binary, *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=900,
    )


def type_command(master: int, out: bytearray, command: str) -> str:
    """Send `command` + Enter and return the rendered screen afterwards."""
    os.write(master, command.encode())
    pump(master, out, 0.4)
    os.write(master, b"\r")
    pump(master, out, 1.2)
    return render(out.decode("utf-8", "replace"))


def probe_binary(binary: str, env: dict[str, str]) -> tuple[str, str]:
    """Run `/status` and `/help` in the TUI and return both screens."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    process = subprocess.Popen(
        [binary, "--tui-mode", "inline"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)
    out = bytearray()
    try:
        if not pump(master, out, 2.0):
            raise RuntimeError(
                "the generated binary exited during startup:\n"
                + out.decode("utf-8", "replace")[-4000:]
            )
        status = type_command(master, out, "/status")
        help_screen = type_command(master, out, "/help")
        os.write(master, b"\x03")
        pump(master, out, 0.6)
        return status, help_screen
    finally:
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        os.close(master)


def check(label: str, passed: bool, results: dict[str, bool]) -> None:
    results[label] = passed
    print(f"{'PASS' if passed else 'FAIL'}  {label}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/codest")
    parser.add_argument("--plugin", default="../cst-provider-qoder")
    parser.add_argument("--plugin-id", default="provider-qoder")
    parser.add_argument("--plugin-command", default="qoder")
    parser.add_argument(
        "--skip-git-check",
        action="store_true",
        help="skip `git status --porcelain` on the codest checkout",
    )
    options = parser.parse_args()

    binary = os.path.abspath(options.binary)
    if not os.path.exists(binary):
        print(f"binary not found: {binary}", file=sys.stderr)
        print("run `cargo build` first", file=sys.stderr)
        return 2

    checkout = Path(__file__).resolve().parent.parent
    # A relative `--plugin` is resolved against the current directory (where
    # the documented `python3 scripts/plugin_smoke.py` is run), not against
    # `scripts/`.
    plugin_path = os.path.abspath(options.plugin)
    if not os.path.isdir(plugin_path):
        print(f"plugin checkout not found: {plugin_path}", file=sys.stderr)
        return 2

    home = tempfile.mkdtemp(prefix="codest-plugin-")
    env = host_env(home)
    crate = Path(home) / "plugins" / "crate"
    generated = crate / "target" / "debug" / "codest"
    results: dict[str, bool] = {}
    print(f"home {home}")
    print(f"plugin {plugin_path}")

    if not options.skip_git_check:
        before = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=checkout,
            capture_output=True,
            text=True,
        ).stdout

    install = run_cli(binary, env, "plugin", "install", plugin_path)
    check("install exited cleanly", install.returncode == 0, results)
    if install.returncode != 0:
        print(install.stdout, file=sys.stderr)
        print(install.stderr, file=sys.stderr)
        return 1

    state = Path(home) / "plugins.toml"
    check("state file written", state.is_file(), results)
    check("wrapper crate generated", (crate / "Cargo.toml").is_file(), results)
    check("wrapper binary built", generated.is_file(), results)

    manifest = (crate / "Cargo.toml").read_text(encoding="utf-8")
    check(
        "manifest depends on the plugin by path",
        f'path = "{plugin_path}"' in manifest,
        results,
    )
    entry = (crate / "src" / "main.rs").read_text(encoding="utf-8")
    check(
        "generated code links the plugin's static",
        "&cst_provider_qoder::PLUGIN" in entry or "::PLUGIN" in entry,
        results,
    )

    listing = run_cli(binary, env, "plugin", "list")
    check("list names the plugin", "cst-provider-qoder" in listing.stdout, results)

    status, help_screen = probe_binary(str(generated), env)
    check("status lists the plugin id", options.plugin_id in status, results)
    check(
        f"help lists /{options.plugin_command}",
        f"/{options.plugin_command}" in help_screen,
        results,
    )
    if options.plugin_id not in status:
        print("--- status screen ---", file=sys.stderr)
        print(status, file=sys.stderr)

    uninstall = run_cli(binary, env, "plugin", "uninstall", "cst-provider-qoder")
    check("uninstall exited cleanly", uninstall.returncode == 0, results)
    regenerated = (crate / "src" / "main.rs").read_text(encoding="utf-8")
    check(
        "wrapper regenerated without the plugin",
        "static LOCAL_PLUGINS: &[&dyn Plugin] = &[]" in regenerated,
        results,
    )
    rebuilt = (crate / "target" / "debug" / "codest").read_bytes()
    check(
        "rebuilt binary no longer embeds the plugin",
        options.plugin_id.encode() not in rebuilt,
        results,
    )

    if not options.skip_git_check:
        after = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=checkout,
            capture_output=True,
            text=True,
        ).stdout
        check("codest checkout untouched", before == after, results)

    passed = all(results.values())
    print()
    print(f"{sum(results.values())}/{len(results)} checks passed")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
