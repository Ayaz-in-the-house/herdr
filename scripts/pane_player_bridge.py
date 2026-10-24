#!/usr/bin/env python3
"""herdr pane player bridge — a carrier, nothing else.

    herdr server ──pane geometry-listen --json──▶ /geometry   (SSE)
                 ──pane audio-listen --pane X───┐
                 ──pane video-listen --pane X ──┴▶ /stream   (one connection, records)
                              --framed
    src/web/player/*  ─────────────────────────▶ /            (the page)

The page does the playing (src/web/player/player.mjs); this process only
spawns the listeners — real herdr clients, so capability, credit and
reconnect stay the server's — and tags every read with the pane it came
from. One connection for every pane because a browser opens six per host
and a per-pane request queues on the second tab (measured, M6).

    record    = b"HV" | u8 tag_len | listener tag | u32 BE len | bytes
                (tag = "<pane>#a" for the audio listener, "<pane>#v" for the
                 framed video listener: each listener is its own client and
                 numbers its streams from one — TP-PLAYER-DEMUX-05)
    heartbeat = b"HV" | 0 | u32 0                 (once a second; a closed
                                                    connection is seen on the
                                                    next write, not never)

Usage:  python3 scripts/pane_player_bridge.py            # live herdr, port 8787
        BRIDGE_ROOT=/tmp/herdr-scene python3 scripts/pane_player_bridge.py   # an isolated server
        BRIDGE_BIN=~/.cache/scratch-evict/herdr-x ...    # which binary talks to it
Then on the Mac: ssh -L 8787:localhost:8787 <fedora>, open http://localhost:8787/

Stdlib only. TP-PLAYER-BRIDGE-01.
"""

from __future__ import annotations

import json
import itertools
import signal
import os
import socket
import re
import struct
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote

REPO_ROOT = Path(__file__).resolve().parent.parent
PLAYER_DIR = REPO_ROOT / "src" / "web" / "player"
HEARTBEAT = b"HV" + bytes([0]) + struct.pack(">I", 0)
_STATIC = re.compile(r"^/player/([a-z]+\.mjs)$")
_PANE_ID = re.compile(r'"pane_id"\s*:\s*"([^"]+)"')


def record(tag: str, payload: bytes) -> bytes:
    ident = tag.encode()
    if len(ident) > 255:
        raise ValueError(f"listener tag {tag!r} is longer than a record can name")
    return b"HV" + bytes([len(ident)]) + ident + struct.pack(">I", len(payload)) + payload


def listener_commands(binary: str, pane_id: str) -> list[tuple[str, list[str]]]:
    """The two clients a pane needs, each with the tag its records carry."""
    return [
        (f"{pane_id}#a", [binary, "pane", "audio-listen", "--pane", pane_id]),
        (f"{pane_id}#v", [binary, "pane", "video-listen", "--pane", pane_id, "--framed"]),
    ]


def pane_ids(listing: str) -> list[str]:
    return _PANE_ID.findall(listing)


def select_panes(panes: list[str], wanted: str | None) -> list[str]:
    """`BRIDGE_PANES=w1:p4,w2:p9` keeps only those; unset or empty keeps every pane.

    A live server has dozens of panes and every one costs two listener clients;
    a proof names its pane rather than subscribing the whole session.
    """
    if not wanted or not wanted.strip():
        return list(panes)
    keep = {part.strip() for part in wanted.split(",") if part.strip()}
    return [pane for pane in panes if pane in keep]


class ReportStore:
    """The last measure report the page posted (`/report`), for a shell to read.

    The browser is on the far side of an ssh tunnel; the number that judges the
    player has to come back over the same wire the page already uses.
    """

    MAX_BYTES = 256 * 1024

    def __init__(self) -> None:
        self._latest = b"{}"
        self._lock = threading.Lock()

    def put(self, body: bytes) -> None:
        if len(body) > self.MAX_BYTES:
            raise ValueError(f"report of {len(body)} bytes exceeds {self.MAX_BYTES}")
        with self._lock:
            self._latest = body

    def latest(self) -> bytes:
        with self._lock:
            return self._latest


def static_path(url_path: str) -> Path | None:
    """A file under the player directory, or nothing. No traversal, no tests, no other type."""
    path = unquote(url_path)
    if path == "/":
        return PLAYER_DIR / "index.html"
    match = _STATIC.match(path)
    if not match:
        return None
    candidate = (PLAYER_DIR / match.group(1)).resolve()
    if candidate.parent != PLAYER_DIR.resolve():
        return None
    return candidate


def _log(message: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {message}", file=sys.stderr, flush=True)


def _environment() -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    root = os.environ.get("BRIDGE_ROOT")
    if root:
        env["XDG_CONFIG_HOME"] = f"{root}/config"
        env["XDG_STATE_HOME"] = f"{root}/state"
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    return env


BIN = os.path.expanduser(os.environ.get("BRIDGE_BIN", "~/.local/bin/herdr"))
PORT = int(os.environ.get("BRIDGE_PORT", "8787"))
ENV = _environment()
MIME = {".html": "text/html; charset=utf-8", ".mjs": "text/javascript; charset=utf-8"}
PANES = os.environ.get("BRIDGE_PANES")
# The socket the pointer goes back through: an isolated root names its own.
SOCKET = os.environ.get("BRIDGE_SOCKET") or os.path.join(
    ENV.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "herdr", "herdr.sock"
)
POINTER_KINDS = ("down", "up", "move", "wheel_up", "wheel_down")
_REQUEST_IDS = itertools.count(1)


def pointer_params(body: bytes) -> dict:
    """The page's `/pointer` body → `pane.send_pointer` params, or ValueError.

    Refused here, before the socket is touched: a point on or past the
    picture's edge is a press the user did not make. TP-PLAYER-INPUT-01
    """
    try:
        data = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as err:
        raise ValueError(f"not json: {err}") from None
    if not isinstance(data, dict):
        raise ValueError("body is not an object")
    pane = data.get("pane")
    kind = data.get("kind")
    if not isinstance(pane, str) or not pane:
        raise ValueError("pane missing")
    if kind not in POINTER_KINDS:
        raise ValueError(f"kind {kind!r} unknown")
    try:
        x, y, width, height = (int(data[key]) for key in ("x", "y", "width", "height"))
    except (KeyError, TypeError, ValueError):
        raise ValueError("picture point missing") from None
    if width <= 0 or height <= 0 or not 0 <= x < width or not 0 <= y < height:
        raise ValueError("point outside its picture")
    button = int(data.get("button") or 0)
    if button not in (0, 1, 2):
        raise ValueError("button must be 0, 1 or 2")
    return {"pane_id": pane, "kind": kind, "button": button, "x": x, "y": y, "width": width, "height": height}


def api_call(socket_path: str, method: str, params: dict, timeout: float = 2.0) -> dict:
    """One request on the socket API, one reply line, as the server answered it."""
    request = {"id": f"bridge-{next(_REQUEST_IDS)}", "method": method, "params": params}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(socket_path)
        sock.sendall((json.dumps(request) + "\n").encode("utf-8"))
        buf = b""
        while not buf.endswith(b"\n"):
            part = sock.recv(65536)
            if not part:
                break
            buf += part
    return json.loads(buf.decode("utf-8") or "{}")
REPORTS = ReportStore()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # noqa: D401 — quiet by design
        pass

    def reply_json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):  # noqa: N802
        path = self.path.split("?", 1)[0]
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        if path == "/pointer":
            try:
                params = pointer_params(body)
            except ValueError as err:
                return self.reply_json(400, {"error": str(err)})
            try:
                reply = api_call(SOCKET, "pane.send_pointer", params)
            except OSError as err:
                return self.reply_json(502, {"error": f"socket: {err}"})
            return self.reply_json(200, reply)
        status = 204
        if path == "/report":
            try:
                REPORTS.put(body)
            except ValueError:
                status = 413
        else:
            status = 404
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):  # noqa: N802
        path = self.path.split("?", 1)[0]
        if path == "/geometry":
            return self.stream_geometry()
        if path == "/stream":
            return self.stream_all()
        if path == "/report":
            body = REPORTS.latest()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        file = static_path(path)
        if file is None or not file.is_file():
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = file.read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", MIME.get(file.suffix, "application/octet-stream"))
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def stream_geometry(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        proc = subprocess.Popen(
            [BIN, "pane", "geometry-listen", "--json"],
            env=ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        _log(f"geometry listener up (pid {proc.pid})")
        try:
            for line in iter(proc.stdout.readline, b""):
                line = line.strip()
                if line:
                    self.wfile.write(b"data: " + line + b"\n\n")
                    self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            proc.terminate()
            _log("geometry listener down")

    def stream_all(self):
        listing = subprocess.run([BIN, "pane", "list"], env=ENV, capture_output=True, text=True).stdout
        panes = select_panes(pane_ids(listing), PANES)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        lock = threading.Lock()
        alive = [True]
        procs: list[subprocess.Popen] = []

        def pump(tag: str, proc: subprocess.Popen) -> None:
            try:
                while alive[0]:
                    # `read1`: a pipe `read(n)` waits for the full n and stalls a live stream.
                    block = proc.stdout.read1(65536)
                    if not block:
                        break
                    with lock:
                        self.wfile.write(record(tag, block))
                        self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                alive[0] = False

        threads = []
        for pane_id in panes:
            for tag, argv in listener_commands(BIN, pane_id):
                proc = subprocess.Popen(argv, env=ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                procs.append(proc)
                register_listener(proc)
                thread = threading.Thread(target=pump, args=(tag, proc), daemon=True)
                thread.start()
                threads.append(thread)
        _log(f"stream up: {len(panes)} panes, {len(procs)} listeners")
        try:
            while alive[0] and any(t.is_alive() for t in threads):
                time.sleep(1.0)
                try:
                    with lock:
                        self.wfile.write(HEARTBEAT)
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    alive[0] = False
        finally:
            for proc in procs:
                proc.terminate()
            _log(f"stream down: {panes}")


# Every listener the bridge ever spawned, so a signal can end them all: a
# bridge killed with SIGTERM left its `audio-listen`/`video-listen` children
# alive (ppid 1) and the server counted them as listeners for the rest of the
# day (fork ledger E-162, lesson E74). TP-PLAYER-BRIDGE-03
LISTENERS: list[subprocess.Popen] = []


def register_listener(proc: subprocess.Popen) -> None:
    LISTENERS.append(proc)


def terminate_listeners() -> int:
    """End every listener still running; returns how many were told to stop."""
    ended = 0
    for proc in list(LISTENERS):
        if proc.poll() is None:
            proc.terminate()
            ended += 1
    LISTENERS.clear()
    return ended


def _on_signal(signum, _frame):  # noqa: ANN001
    terminate_listeners()
    raise SystemExit(128 + int(signum))


def main() -> int:
    if not Path(BIN).is_file():
        _log(f"herdr binary not found: {BIN}")
        return 2
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, _on_signal)
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    server.daemon_threads = True
    _log(f"player at http://127.0.0.1:{PORT}/  (herdr: {BIN}, root: {os.environ.get('BRIDGE_ROOT', 'live')}, panes: {PANES or 'all'})")
    try:
        server.serve_forever()
    except (KeyboardInterrupt, SystemExit):
        pass
    finally:
        terminate_listeners()
    return 0


if __name__ == "__main__":
    sys.exit(main())
