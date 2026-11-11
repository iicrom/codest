#!/usr/bin/env python3
"""Assert the session header reaches the wire.

`session-id` is the one part of session identity a client can only get wrong on
the wire, so this drives a real `cst` against the local test server and inspects
the headers the server received:

  * default configuration sends `session-id: <uuid>` on the first request;
  * `wire.session_header = "none"` sends no session header at all;
  * `[[providers.models]] session_header = "none"` turns it off for one model
    while the instance default stays on.

Usage: python3 scripts/session_header_smoke.py [--binary target/debug/cst]
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import re
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

ROWS, COLS = 40, 120
SERVER = Path(__file__).resolve().parent / "openai_test_server.py"
UUID_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)


def start_server(dump_path: Path) -> tuple[subprocess.Popen, str]:
    process = subprocess.Popen(
        [
            sys.executable,
            str(SERVER),
            "--port",
            "0",
            "--delay",
            "0",
            "--quiet",
            "--dump-headers",
            str(dump_path),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    assert process.stdout is not None
    line = process.stdout.readline()
    match = re.search(r"http://[^\s]+", line)
    if not match:
        process.terminate()
        raise RuntimeError(f"test server did not report a URL: {line!r}")
    return process, match.group(0)


def pump(master: int, output: bytearray, seconds: float) -> bool:
    """Drain the pty for `seconds`; `False` once the child is gone."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            chunk = os.read(master, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        output.extend(chunk)
        time.sleep(0.01)
    return True


def drive(binary: str, env: dict[str, str], args: list[str]) -> str:
    """Run one turn in a pty and return everything the terminal received."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    process = subprocess.Popen(
        [binary, "--tui-mode", "inline", *args],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)
    output = bytearray()
    try:
        pump(master, output, 1.2)
        # The TUI asks for the cursor position at startup and waits for the
        # reply; `\r` is Enter (a bare `\n` only inserts a newline).
        try:
            os.write(master, b"\x1b[1;1R")
        except OSError:
            pass
        pump(master, output, 0.3)
        os.write(master, b"say hello")
        pump(master, output, 0.2)
        os.write(master, b"\r")
        pump(master, output, 2.5)
        for _ in range(3):
            if process.poll() is not None:
                break
            try:
                os.write(master, b"\x03")
            except OSError:
                break
            pump(master, output, 0.3)
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
    return output.decode("utf-8", "replace")


def records(dump_path: Path) -> list[dict]:
    if not dump_path.exists():
        return []
    out = []
    for line in dump_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


def session_headers(entry: dict) -> dict[str, str]:
    return {
        key.lower(): value
        for key, value in entry.get("headers", {}).items()
        if key.lower() == "session-id"
    }


def run_case(
    binary: str, label: str, providers_toml: str, config_toml: str
) -> tuple[bool, str]:
    home = Path(tempfile.mkdtemp(prefix="codest-session-header-"))
    dump = home / "headers.jsonl"
    server, base_url = start_server(dump)
    try:
        (home / "providers.toml").write_text(
            providers_toml.replace("__BASE_URL__", base_url), encoding="utf-8"
        )
        (home / "config.toml").write_text(config_toml, encoding="utf-8")
        env = {**os.environ, "TERM": "xterm-256color", "CODEST_HOME": str(home)}
        for key in list(env):
            if key.startswith("CST_"):
                del env[key]
        drive(binary, env, [])
        entries = [entry for entry in records(dump) if session_headers(entry)]
        all_entries = records(dump)
        if not all_entries:
            return False, "the client sent no request at all"
        if not entries:
            return True, "no session-id header (0 requests carried one)"
        value = next(iter(session_headers(entries[0]).values()))
        if not UUID_RE.match(value):
            return False, f"session-id is not a uuid v7: {value!r}"
        return True, f"session-id={value} on {len(entries)}/{len(all_entries)} requests"
    finally:
        server.terminate()
        try:
            server.wait(timeout=2)
        except subprocess.TimeoutExpired:
            server.kill()


DEFAULT_PROVIDERS = """\
[[providers]]
name = "pytest"
base_url = "__BASE_URL__"
api_key = "test-key"
fetch_models = true

  [[providers.models]]
  id = "test-model"
  context_window = 100000
"""

OPTED_OUT_PROVIDERS = """\
[[providers]]
name = "pytest"
base_url = "__BASE_URL__"
api_key = "test-key"
fetch_models = true
wire = { session_header = "none" }

  [[providers.models]]
  id = "test-model"
  context_window = 100000
"""

MODEL_OPTOUT_PROVIDERS = """\
[[providers]]
name = "pytest"
base_url = "__BASE_URL__"
api_key = "test-key"
fetch_models = true

  [[providers.models]]
  id = "test-model"
  context_window = 100000
  session_header = "none"
"""

CONFIG = 'default = "pytest/test-model"\n'


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/cst")
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    cases = [
        ("default sends session-id", DEFAULT_PROVIDERS, True),
        ("wire.session_header = none sends nothing", OPTED_OUT_PROVIDERS, False),
        ("a model opting out sends nothing", MODEL_OPTOUT_PROVIDERS, False),
    ]
    failures = 0
    for label, providers, expect_header in cases:
        ok, detail = run_case(binary, label, providers, CONFIG)
        sent = "0 requests carried one" not in detail
        good = ok and sent == expect_header
        print(f"{'PASS' if good else 'FAIL'}  {label}: {detail}")
        failures += 0 if good else 1
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
