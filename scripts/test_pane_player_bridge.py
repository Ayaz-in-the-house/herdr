"""The bridge is a carrier, and a carrier's only promises are about bytes.

It writes one record per listener read — `HV`, the pane id's length, the pane
id, a big-endian length, the bytes — and an empty record once a second so a
closed connection is noticed. The player's demux reads exactly that shape
(TP-PLAYER-DEMUX-01), so the writer is pinned here from the same table, and
the pair of listeners each pane gets is pinned too: audio and *framed* video,
or the video's `pts_us` never reaches the page. Static files are served from
the player directory and nowhere else.

TP-PLAYER-BRIDGE-01
"""

from __future__ import annotations

import json
import os
import socket
import struct
import tempfile
import threading
import unittest
from pathlib import Path

from scripts.pane_player_bridge import (
    HEARTBEAT,
    PLAYER_DIR,
    api_call,
    register_listener,
    terminate_listeners,
    listener_commands,
    pane_ids,
    pointer_params,
    record,
    static_path,
)


def _parse(buf: bytes) -> tuple[str, bytes, bytes]:
    """A reader written from the table, not from the writer under test."""
    assert buf[:2] == b"HV"
    id_len = buf[2]
    pane = buf[3 : 3 + id_len].decode()
    (length,) = struct.unpack(">I", buf[3 + id_len : 7 + id_len])
    start = 7 + id_len
    return pane, buf[start : start + length], buf[start + length :]


class RecordTests(unittest.TestCase):
    def test_a_record_names_its_pane_and_length(self) -> None:
        pane, payload, rest = _parse(record("w1:p3", b"\x4d\x01\x00\x00" + b"x" * 60))
        self.assertEqual(pane, "w1:p3")
        self.assertEqual(payload, b"\x4d\x01\x00\x00" + b"x" * 60)
        self.assertEqual(rest, b"")

    def test_the_heartbeat_is_an_empty_record(self) -> None:
        pane, payload, rest = _parse(HEARTBEAT)
        self.assertEqual((pane, payload, rest), ("", b"", b""))
        self.assertEqual(HEARTBEAT, b"HV\x00\x00\x00\x00\x00")

    def test_a_pane_id_longer_than_a_byte_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            record("p" * 256, b"")


class ListenerTests(unittest.TestCase):
    def test_each_pane_gets_an_audio_and_a_framed_video_listener(self) -> None:
        commands = listener_commands("/opt/herdr", "w1:p3")
        self.assertEqual(len(commands), 2)
        (audio_tag, audio), (video_tag, video) = commands
        # Each listener is its own client with its own stream numbering, so
        # the record names the listener, not just the pane (TP-PLAYER-DEMUX-05).
        self.assertEqual((audio_tag, video_tag), ("w1:p3#a", "w1:p3#v"))
        self.assertEqual(audio[:4], ["/opt/herdr", "pane", "audio-listen", "--pane"])
        self.assertEqual(audio[4], "w1:p3")
        self.assertEqual(video[:4], ["/opt/herdr", "pane", "video-listen", "--pane"])
        self.assertEqual(video[4], "w1:p3")
        # Without the flag the video listener writes bare pictures and the
        # page has no pts to schedule against.
        self.assertIn("--framed", video)

    def test_pane_ids_come_from_the_listing(self) -> None:
        listing = '{"panes":[{"pane_id":"w1:p1","title":"a"},{"pane_id":"w1:p3K","title":"b"}]}'
        self.assertEqual(pane_ids(listing), ["w1:p1", "w1:p3K"])
        self.assertEqual(pane_ids(""), [])


class StaticTests(unittest.TestCase):
    def test_static_paths_stay_inside_the_player_directory(self) -> None:
        self.assertEqual(static_path("/"), PLAYER_DIR / "index.html")
        self.assertEqual(static_path("/player/player.mjs"), PLAYER_DIR / "player.mjs")
        for url in ("/player/../../Cargo.toml", "/player/%2e%2e/x", "/player/test/demux.test.mjs", "/etc/passwd", "/player/nope.txt"):
            self.assertIsNone(static_path(url), url)

    def test_the_player_directory_is_the_checked_in_one(self) -> None:
        self.assertTrue((PLAYER_DIR / "index.html").is_file())
        self.assertTrue((PLAYER_DIR / "player.mjs").is_file())
        self.assertEqual(PLAYER_DIR, Path(__file__).resolve().parent.parent / "src" / "web" / "player")


if __name__ == "__main__":
    unittest.main()


class PaneFilterAndReportTests(unittest.TestCase):
    """TP-PLAYER-BRIDGE-02 — a live server has dozens of panes; the bridge
    listens only where it is told, and the page's measure report is kept so a
    shell on the server side can read what the browser saw."""

    def test_the_pane_filter_keeps_only_the_named_panes(self) -> None:
        from scripts.pane_player_bridge import select_panes
        listing = ["w1:p1", "w1:p4", "w2:p9"]
        self.assertEqual(select_panes(listing, "w1:p4,w2:p9"), ["w1:p4", "w2:p9"])
        self.assertEqual(select_panes(listing, " w1:p4 , nope "), ["w1:p4"])
        self.assertEqual(select_panes(listing, None), listing)
        self.assertEqual(select_panes(listing, ""), listing)

    def test_the_last_report_is_kept_and_served_as_posted(self) -> None:
        from scripts.pane_player_bridge import ReportStore
        store = ReportStore()
        self.assertEqual(store.latest(), b"{}")
        store.put(b'{"bips": 3}')
        store.put(b'{"bips": 4}')
        self.assertEqual(store.latest(), b'{"bips": 4}')
        with self.assertRaises(ValueError):
            store.put(b"x" * (256 * 1024 + 1))

class PointerTests(unittest.TestCase):
    """TP-PLAYER-INPUT-01"""

    def test_a_page_pointer_becomes_a_pane_send_pointer_call(self) -> None:
        seen: list[dict] = []
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "herdr.sock")
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(path)
            server.listen(1)

            def serve() -> None:
                conn, _ = server.accept()
                with conn:
                    buf = b""
                    while not buf.endswith(b"\n"):
                        part = conn.recv(65536)
                        if not part:
                            break
                        buf += part
                    request = json.loads(buf)
                    seen.append(request)
                    reply = {"id": request["id"], "result": {"type": "pointer_sent", "delivered": "program"}}
                    conn.sendall(json.dumps(reply).encode("utf-8") + b"\n")

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            body = json.dumps({"pane": "w1:p3", "kind": "down", "button": 2, "x": 10, "y": 20, "width": 640, "height": 360})
            reply = api_call(path, "pane.send_pointer", pointer_params(body.encode("utf-8")))
            thread.join(2)
            server.close()
        self.assertEqual(seen[0]["method"], "pane.send_pointer")
        self.assertEqual(
            seen[0]["params"],
            {"pane_id": "w1:p3", "kind": "down", "button": 2, "x": 10, "y": 20, "width": 640, "height": 360},
        )
        self.assertEqual(reply["result"]["delivered"], "program")

    def test_a_pointer_body_without_its_picture_is_refused(self) -> None:
        for body in (
            b"{}",
            b'{"pane":"w1:p3","kind":"down"}',
            b'{"pane":"w1:p3","kind":"down","x":640,"y":0,"width":640,"height":360}',
            b'{"pane":"w1:p3","kind":"fly","x":1,"y":1,"width":2,"height":2}',
            b'{"pane":"","kind":"down","x":1,"y":1,"width":2,"height":2}',
            b'{"pane":"w1:p3","kind":"down","button":7,"x":1,"y":1,"width":2,"height":2}',
            b"not json",
        ):
            with self.assertRaises(ValueError, msg=body):
                pointer_params(body)


# TP-PLAYER-BRIDGE-03
class ListenerLifetimeTests(unittest.TestCase):
    """TP-PLAYER-BRIDGE-03"""

    def test_a_signal_ends_every_listener_the_bridge_spawned(self) -> None:
        import subprocess
        import sys as _sys
        procs = [subprocess.Popen([_sys.executable, "-c", "import time; time.sleep(30)"]) for _ in range(2)]
        for proc in procs:
            register_listener(proc)
        self.assertEqual(terminate_listeners(), 2)
        for proc in procs:
            self.assertIsNotNone(proc.wait(timeout=5), "a listener told to stop stops")
        self.assertEqual(terminate_listeners(), 0, "nothing is left to end twice")

