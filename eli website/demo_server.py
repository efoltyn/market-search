#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from http.server import BaseHTTPRequestHandler, SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parent
DEMO_DIR = ROOT / ".demo"
CACHE_DIR = DEMO_DIR / "cache"
OUT_DIR = DEMO_DIR / "out"


def _json_response(handler: BaseHTTPRequestHandler, status: int, payload: dict[str, Any]) -> None:
    raw = json.dumps(payload, indent=2).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json; charset=utf-8")
    handler.send_header("Content-Length", str(len(raw)))
    handler.send_header("Access-Control-Allow-Origin", "*")
    handler.send_header("Access-Control-Allow-Headers", "Content-Type")
    handler.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
    handler.end_headers()
    handler.wfile.write(raw)


def _find_eli_bin() -> str | None:
    override = os.environ.get("ELI_BIN")
    if override:
        p = Path(override)
        if p.exists():
            return str(p)

    candidates = [
        ROOT.parent / "eli" / "target" / "debug" / "eli",
        ROOT.parent / "eli" / "target" / "release" / "eli",
        ROOT.parent / "eli" / "target" / "debug" / "eli-cli",
        ROOT.parent / "eli" / "target" / "release" / "eli-cli",
    ]
    for c in candidates:
        if c.exists():
            return str(c)

    which = shutil.which("eli")
    return which


def _sanitize_filename(s: str) -> str:
    keep = []
    for ch in s:
        if ch.isalnum() or ch in ("-", "_", "."):
            keep.append(ch)
        else:
            keep.append("_")
    out = "".join(keep).strip("._")
    return out or "out"


def _rewrite_timeseries_args(args: list[str], allow_net: bool) -> list[str]:
    rewritten: list[str] = []
    it = iter(range(len(args)))

    skip_next = False
    for i in it:
        if skip_next:
            skip_next = False
            continue

        token = args[i]
        if token in ("--cache-dir", "--out", "--provider", "--max-points-per-ticker"):
            skip_next = True
            continue
        rewritten.append(token)

    # Default to mock for the web demo unless explicitly enabled.
    provider = "mock" if not allow_net else None

    # Derive a stable output name (tickers/range/granularity if present).
    raw = "timeseries"
    for flag in ("--tickers", "--range", "--granularity", "--as-of"):
        if flag in args:
            try:
                idx = args.index(flag)
                raw += f"_{flag.lstrip('-')}_{args[idx + 1]}"
            except Exception:
                pass
    out_name = _sanitize_filename(raw) + ".json"

    # Use relative paths so the CLI prints a fetchable `path` (served by this server).
    rewritten.extend(["--cache-dir", ".demo/cache"])
    rewritten.extend(["--max-points-per-ticker", "4000"])
    if provider is not None:
        rewritten.extend(["--provider", provider])
    rewritten.extend(["--out", f".demo/out/{out_name}"])
    return rewritten


def _validate_and_build_argv(cmd: str, eli_bin: str, allow_net: bool) -> tuple[list[str], str | None]:
    cmd = cmd.strip()
    if not cmd:
        raise ValueError("empty command")
    if len(cmd) > 500:
        raise ValueError("command too long")

    try:
        user_argv = shlex.split(cmd)
    except ValueError as e:
        raise ValueError(f"cannot parse command: {e}") from e

    if not user_argv:
        raise ValueError("empty command")

    if user_argv[0] not in ("eli", "eli-cli"):
        raise ValueError("only 'eli' commands are allowed")

    # Allow basic info commands.
    if len(user_argv) == 2 and user_argv[1] in ("--help", "-h", "--version", "-V"):
        return [eli_bin, user_argv[1]], None

    # Only allow safe, non-interactive finance commands by default.
    if len(user_argv) >= 2 and user_argv[1] == "finance":
        if len(user_argv) == 3 and user_argv[2] in ("--help", "-h"):
            return [eli_bin, "finance", "--help"], None
        if len(user_argv) >= 3 and user_argv[2] == "timeseries":
            rewritten = _rewrite_timeseries_args(user_argv[3:], allow_net=allow_net)
            return [eli_bin, "finance", "timeseries", *rewritten], "timeseries"

        # Everything else is blocked by default (news hits the network, chat/tui are interactive, etc).
        raise ValueError("only 'eli finance timeseries' is enabled in the web demo")

    raise ValueError("unsupported command (try: 'eli --help' or 'eli finance timeseries ...')")


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, directory=str(ROOT), **kwargs)

    def log_message(self, fmt: str, *args: Any) -> None:
        sys.stderr.write("%s - - [%s] %s\n" % (self.client_address[0], self.log_date_time_string(), fmt % args))

    def do_OPTIONS(self) -> None:
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.end_headers()

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path == "/api/health":
            eli_bin = _find_eli_bin()
            _json_response(
                self,
                200,
                {
                    "ok": True,
                    "eli_bin": eli_bin,
                    "root": str(ROOT),
                    "allow_net": os.environ.get("ELI_WEB_DEMO_ALLOW_NET") == "1",
                },
            )
            return

        super().do_GET()

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path != "/api/run":
            _json_response(self, 404, {"ok": False, "error": "not found"})
            return

        eli_bin = _find_eli_bin()
        if not eli_bin:
            _json_response(
                self,
                500,
                {
                    "ok": False,
                    "error": "eli binary not found. Set ELI_BIN or build the project (eli/target/debug/eli).",
                },
            )
            return

        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = 0
        if length <= 0 or length > 50_000:
            _json_response(self, 400, {"ok": False, "error": "invalid request body"})
            return

        raw = self.rfile.read(length)
        try:
            payload = json.loads(raw.decode("utf-8"))
        except Exception:
            _json_response(self, 400, {"ok": False, "error": "body must be JSON"})
            return

        cmd = payload.get("cmd")
        if not isinstance(cmd, str):
            _json_response(self, 400, {"ok": False, "error": "missing 'cmd' string"})
            return

        allow_net = os.environ.get("ELI_WEB_DEMO_ALLOW_NET") == "1"
        try:
            argv, kind = _validate_and_build_argv(cmd, eli_bin, allow_net=allow_net)
        except ValueError as e:
            _json_response(self, 400, {"ok": False, "error": str(e)})
            return

        DEMO_DIR.mkdir(parents=True, exist_ok=True)
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        OUT_DIR.mkdir(parents=True, exist_ok=True)

        # Run from the website directory so `.demo/out/...` is directly fetchable by the page.
        start = time.time()
        try:
            proc = subprocess.run(
                argv,
                cwd=str(ROOT),
                capture_output=True,
                text=True,
                timeout=20,
            )
        except subprocess.TimeoutExpired:
            _json_response(self, 504, {"ok": False, "error": "command timed out"})
            return
        finally:
            duration_ms = int((time.time() - start) * 1000)

        _json_response(
            self,
            200,
            {
                "ok": proc.returncode == 0,
                "exit_code": proc.returncode,
                "duration_ms": duration_ms,
                "argv": argv,
                "kind": kind,
                "stdout": proc.stdout,
                "stderr": proc.stderr,
            },
        )


def main() -> int:
    parser = argparse.ArgumentParser(description="Local web demo server for eli website.")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    args = parser.parse_args()

    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"Serving {ROOT} at http://{args.host}:{args.port}")
    print("Live terminal enabled at /api/run (restricted to `eli finance timeseries`).")
    print("Tip: set ELI_BIN=/path/to/eli to override which binary is used.")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
