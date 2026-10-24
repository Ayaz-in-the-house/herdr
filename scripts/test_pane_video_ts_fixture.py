"""The committed encoder-transport fixture still says what its referee says.

`src/server/headless/mpegts.rs` asserts that it reads these bytes into the
pictures the manifest lists. That proves the reader agrees with the manifest;
it does not prove the manifest describes the bytes — a `.ts` and a `.json`
edited together would agree with each other and with nothing else. This
check re-describes the committed bytes with the referee and compares, and it
pins the properties the reader's design rests on: every picture's PES length
is written, so every picture is complete on its own last byte.
"""

from __future__ import annotations

import importlib.util
import json
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
REFEREE = REPO_ROOT / "scripts" / "pane_video_ts_fixture.py"

_spec = importlib.util.spec_from_file_location("pane_video_ts_fixture", REFEREE)
assert _spec is not None and _spec.loader is not None
_referee = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_referee)


class CommittedFixtureMatchesTheReferee(unittest.TestCase):
    def setUp(self) -> None:
        self.assertTrue(_referee.TS_PATH.exists(), "the fixture is missing")
        self.assertTrue(_referee.JSON_PATH.exists(), "the manifest is missing")
        self.bytes = _referee.TS_PATH.read_bytes()
        self.described = _referee.describe(self.bytes)
        self.committed = json.loads(_referee.JSON_PATH.read_text())

    def test_the_manifest_describes_the_bytes_that_are_committed(self):
        self.assertEqual(
            self.committed,
            self.described,
            "tests/fixtures/pane_video_frames.json no longer describes "
            "pane_video_frames.ts — one was edited by hand, or the referee "
            "changed and was not rerun",
        )

    def test_every_pictures_length_is_written(self):
        # The one property the whole transport choice rests on: a picture is
        # complete on its own last byte, never on the next picture's first.
        for picture in self.described["pictures"]:
            self.assertTrue(picture["length_written"], picture)
            self.assertEqual(picture["completed_by"], "length", picture)

    def test_the_pictures_cover_the_shapes_the_reader_must_handle(self):
        pictures = self.described["pictures"]
        self.assertEqual(len(pictures), _referee.FRAMES)
        self.assertTrue(pictures[0]["key"], "a stream starts on a key picture")
        self.assertTrue(
            any(not p["key"] for p in pictures),
            "and carries P pictures, or the key flag is never proved false",
        )
        self.assertTrue(
            all(p["starts_with_aud"] for p in pictures),
            "the muxer delimits every access unit",
        )
        sizes = [p["size"] for p in pictures]
        self.assertEqual(
            len(set(sizes)),
            len(sizes),
            "sizes all differ, or a reader that reused one length would pass",
        )
        self.assertTrue(
            any(size > _referee.PACKET for size in sizes),
            "at least one picture spans several packets",
        )

    def test_the_bytes_are_whole_packets_starting_on_sync(self):
        self.assertEqual(len(self.bytes) % _referee.PACKET, 0)
        for offset in range(0, len(self.bytes), _referee.PACKET):
            self.assertEqual(self.bytes[offset], _referee.SYNC, f"byte {offset}")


if __name__ == "__main__":
    unittest.main()
