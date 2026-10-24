"""The committed media web-frame fixture still matches its generator.

The Rust unit tests assert that the encoder reproduces these bytes, and
`herdr-web` asserts that its decoder reads them. Neither notices if the bytes
themselves were edited by hand together with the manifest that describes them —
both would simply agree with the new, wrong file.

This check closes that: it rebuilds the fixture from the documented layout and
compares. It lives in python rather than in the Rust test binary because it also
guards the manifest, which is data the Rust side reads rather than produces.
"""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
GENERATOR = REPO_ROOT / "scripts" / "media_web_frame_fixture.py"

_spec = importlib.util.spec_from_file_location("media_web_frame_fixture", GENERATOR)
assert _spec is not None and _spec.loader is not None
_fixture = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_fixture)


class CommittedFixtureMatchesTheGenerator(unittest.TestCase):
    def test_bytes_on_disk_are_the_bytes_the_layout_produces(self):
        blob, _ = _fixture.build()
        self.assertTrue(_fixture.BIN_PATH.exists(), "the fixture is missing")
        self.assertEqual(
            _fixture.BIN_PATH.read_bytes(),
            blob,
            "tests/fixtures/media_web_frames.bin no longer matches "
            "scripts/media_web_frame_fixture.py — it was edited by hand, or the "
            "layout changed and the generator was not rerun",
        )

    def test_manifest_on_disk_describes_those_bytes(self):
        _, manifest = _fixture.build()
        import json

        on_disk = json.loads(_fixture.JSON_PATH.read_text())
        self.assertEqual(
            on_disk,
            manifest,
            "the manifest and the generator disagree; a manifest edited to match "
            "hand-edited bytes would make every other test agree with the wrong file",
        )

    def test_the_frames_cover_the_shapes_the_agreement_is_about(self):
        _, manifest = _fixture.build()
        kinds = [frame["kind"] for frame in manifest["frames"]]
        self.assertEqual(
            kinds,
            [_fixture.KIND_OPEN, _fixture.KIND_CHUNK, _fixture.KIND_CHUNK, _fixture.KIND_CLOSE],
            "one open, two chunks, one close — two chunks because a decoder that "
            "reads one length and applies it to the next frame passes with one",
        )
        lengths = [
            frame["payload_len"]
            for frame in manifest["frames"]
            if frame["kind"] == _fixture.KIND_CHUNK
        ]
        self.assertNotEqual(
            lengths[0],
            lengths[1],
            "the two chunks must differ in length, or a per-frame length is never proved",
        )

    def test_the_manifest_carries_no_second_copy_of_the_hash(self):
        # A hash stored beside the data it hashes travels with a hand-edited
        # copy and agrees with it. The pin belongs in test code, one per language.
        _, manifest = _fixture.build()
        self.assertNotIn(
            "sha256",
            manifest,
            "the sha256 pin belongs in the two test files, not in the data",
        )


if __name__ == "__main__":
    unittest.main()
