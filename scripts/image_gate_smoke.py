#!/usr/bin/env python3
"""Assert `read_image` is only declared to models that accept image input.

The capability gate has three parts (declaration, execution, and stripping
the transcript's images from the request), and the first one is the only part
that is directly observable on the wire. This drives a real `cst` against the
local test server — which records every request body — and reads the `tools`
array of the first chat request:

  * `vision = true` declares `read_image`;
  * `vision = false` does not;
  * an entry that says nothing about image input does not either, because an
    undeclared model is treated as text-only (the gate fails closed).

The last case is what makes the rule safe: an endpoint whose models API
ignores the capability cannot accidentally be handed an image tool.

Usage: python3 scripts/image_gate_smoke.py [--binary target/debug/cst]
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import struct
import subprocess
import sys
import tempfile
import termios
from pathlib import Path

ROWS, COLS = 40, 120
SERVER = Path(__file__).resolve().parent / "openai_test_server.py"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from pty_smoke import pump  # noqa: E402  (answers the cursor-position query)

PROVIDERS = """\
[[providers]]
name = "gate"
base_url = "__BASE_URL__"
api_key = "test-key"
fetch_models = false

  [[providers.models]]
  id = "test-model"
__VISION__"""

CONFIG = 'default = "gate/test-model"\n'


def start_server(dump_path: Path) -> subprocess.Popen[str]:
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
    return process


def reported_url(process: subprocess.Popen[str]) -> str:
    assert process.stdout is not None
    line = process.stdout.readline()
    marker = line.split("http://", 1)
    if len(marker) != 2:
        raise RuntimeError(f"test server did not report a URL: {line!r}")
    return "http://" + marker[1].split()[0]


def drive_once(binary: str, home: str) -> None:
    """Run one turn in a pty; the server records the request body."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", CODEST_HOME=home)
    process = subprocess.Popen(
        [binary, "--tui-mode", "fullscreen", "--provider", "gate", "--model", "test-model"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)
    output = bytearray()
    try:
        if not pump(master, output, 1.5):
            raise RuntimeError("the TUI exited during startup")
        # `\r` is Enter (a bare `\n` only inserts a newline).
        os.write(master, b"say hello")
        pump(master, output, 0.2)
        os.write(master, b"\r")
        pump(master, output, 2.5)
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


def tool_names(dump_path: Path) -> list[str] | None:
    """The `tools` array of the first chat request, or `None` if none came."""
    if not dump_path.exists():
        return None
    for line in dump_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not str(record.get("path", "")).endswith("/chat/completions"):
            continue
        body = record.get("body") or {}
        return [
            entry.get("function", {}).get("name", "")
            for entry in body.get("tools") or []
        ]
    return None


def run_case(binary: str, label: str, vision_line: str, expect: bool) -> tuple[bool, str]:
    workdir = Path(tempfile.mkdtemp(prefix="cst-image-gate-"))
    dump_path = workdir / "requests.jsonl"
    server = start_server(dump_path)
    try:
        base_url = reported_url(server)
        (workdir / "providers.toml").write_text(
            PROVIDERS.replace("__BASE_URL__", base_url).replace("__VISION__", vision_line),
            encoding="utf-8",
        )
        (workdir / "config.toml").write_text(CONFIG, encoding="utf-8")
        drive_once(binary, str(workdir))
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()

    names = tool_names(dump_path)
    if names is None:
        return False, f"{label}: no chat request was recorded"
    declared = "read_image" in names
    detail = f"{label}: declared={declared} tools={len(names)}"
    return declared == expect, detail


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/cst")
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    cases = [
        ("vision = true declares read_image", "  vision = true\n", True),
        ("vision = false withholds it", "  vision = false\n", False),
        ("an undeclared model withholds it", "", False),
    ]
    failures = 0
    for label, vision_line, expect in cases:
        ok, detail = run_case(binary, label, vision_line, expect)
        print(f"{'PASS' if ok else 'FAIL'}  {detail}")
        failures += 0 if ok else 1
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
