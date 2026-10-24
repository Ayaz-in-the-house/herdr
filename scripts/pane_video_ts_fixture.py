#!/usr/bin/env python3
"""Golden fixture for the encoder→server transport (TP-VIDEO-AU-01).

The server reads its encoder's output as MPEG-TS, because a PES packet whose
length is written says where a picture ends *the moment its last byte arrives*
— measured 2026-09-02: 1.3 ms after the picture was handed to the encoder,
against 101 ms for fragmented MP4 and for Annex-B with access-unit delimiters,
both of which only reveal a picture's end when the next picture begins.

This file is the referee: an independent MPEG-TS demuxer, written from the
standard (ISO 13818-1 §2.4.3) and not from the Rust reader, that describes the
committed stream picture by picture. The Rust reader must agree with it, and a
fixture produced by ffmpeg's muxer (a third implementation) is what both are
measured against.

    python3 scripts/pane_video_ts_fixture.py --write   # record with ffmpeg + describe
    python3 scripts/pane_video_ts_fixture.py --check   # describe the committed bytes, compare
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TS_PATH = ROOT / "tests" / "fixtures" / "pane_video_frames.ts"
JSON_PATH = ROOT / "tests" / "fixtures" / "pane_video_frames.json"

PACKET = 188
SYNC = 0x47
PAT_PID = 0
H264_STREAM_TYPE = 0x1B
FRAMES = 5

# What the encoder is asked for. Small and software: the picture's content is
# irrelevant, the muxer's framing is the subject. `-omit_video_pes_length 0` is
# the one flag the whole design rests on.
RECORD = [
    "ffmpeg", "-hide_banner", "-loglevel", "error",
    "-f", "lavfi", "-i", "testsrc=size=64x64:rate=10", "-frames:v", str(FRAMES),
    "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "4", "-bf", "0",
    "-pix_fmt", "yuv420p", "-fps_mode", "passthrough",
    "-f", "mpegts", "-omit_video_pes_length", "0", "-muxdelay", "0", "-muxpreload", "0",
]


def nal_types(annex_b: bytes) -> list[int]:
    """NAL unit types in an Annex-B byte string, in order."""
    types = []
    i = 0
    n = len(annex_b)
    while i + 3 <= n:
        if annex_b[i] == 0 and annex_b[i + 1] == 0 and annex_b[i + 2] == 1:
            if i + 3 < n:
                types.append(annex_b[i + 3] & 0x1F)
            i += 3
        else:
            i += 1
    return types


def demux(ts: bytes) -> list[dict]:
    """Every video picture in the stream: size, whether its length was written,
    the NAL types it carries, and its PTS in 90 kHz ticks."""
    if len(ts) % PACKET:
        raise ValueError(f"{len(ts)} bytes is not a whole number of packets")
    pmt_pid = None
    video_pid = None
    pictures: list[dict] = []
    pending: dict | None = None

    def finish(reason: str) -> None:
        nonlocal pending
        if pending is None:
            return
        body = pending["body"]
        expected = pending["expected"]
        if expected is not None:
            body = body[:expected]
        pictures.append({
            "index": len(pictures),
            "size": len(body),
            "length_written": expected is not None,
            "completed_by": reason,
            "nal_types": nal_types(body),
            "starts_with_aud": body[:5] == b"\x00\x00\x00\x01\x09",
            "key": 5 in nal_types(body),
            "pts_90k": pending["pts"],
        })
        pending = None

    for offset in range(0, len(ts), PACKET):
        pkt = ts[offset:offset + PACKET]
        if pkt[0] != SYNC:
            raise ValueError(f"lost sync at byte {offset}")
        pusi = bool(pkt[1] & 0x40)
        pid = ((pkt[1] & 0x1F) << 8) | pkt[2]
        afc = (pkt[3] >> 4) & 0x3
        at = 4
        if afc & 0x2:
            at += 1 + pkt[4]
        if not afc & 0x1:
            continue
        payload = pkt[at:]
        if pid == PAT_PID:
            if not pusi:
                continue
            section = payload[1 + payload[0]:]
            section_length = ((section[1] & 0x0F) << 8) | section[2]
            programs = section[8:3 + section_length - 4]
            for i in range(0, len(programs), 4):
                number = (programs[i] << 8) | programs[i + 1]
                if number != 0:
                    pmt_pid = ((programs[i + 2] & 0x1F) << 8) | programs[i + 3]
                    break
        elif pid == pmt_pid:
            if not pusi:
                continue
            section = payload[1 + payload[0]:]
            section_length = ((section[1] & 0x0F) << 8) | section[2]
            info_length = ((section[10] & 0x0F) << 8) | section[11]
            streams = section[12 + info_length:3 + section_length - 4]
            i = 0
            while i + 5 <= len(streams):
                stream_type = streams[i]
                es_pid = ((streams[i + 1] & 0x1F) << 8) | streams[i + 2]
                es_info = ((streams[i + 3] & 0x0F) << 8) | streams[i + 4]
                if stream_type == H264_STREAM_TYPE and video_pid is None:
                    video_pid = es_pid
                i += 5 + es_info
        elif pid == video_pid:
            if pusi:
                finish("next-start")
                if payload[:3] != b"\x00\x00\x01":
                    raise ValueError(f"video PES without a start code at byte {offset}")
                pes_length = (payload[4] << 8) | payload[5]
                flags = payload[7]
                header_length = payload[8]
                pts = None
                if flags & 0x80:
                    h = payload[9:14]
                    pts = (((h[0] >> 1) & 0x7) << 30) | (h[1] << 22) | ((h[2] >> 1) << 15) | (h[3] << 7) | (h[4] >> 1)
                body = payload[9 + header_length:]
                expected = pes_length - 3 - header_length if pes_length else None
                pending = {"body": bytes(body), "expected": expected, "pts": pts}
            elif pending is not None:
                pending["body"] += bytes(payload)
            if pending is not None and pending["expected"] is not None and len(pending["body"]) >= pending["expected"]:
                finish("length")
    finish("end-of-stream")
    return pictures


def describe(ts: bytes) -> dict:
    pictures = demux(ts)
    return {
        "note": (
            "Five pictures from ffmpeg's MPEG-TS muxer with -omit_video_pes_length 0. "
            "Described by scripts/pane_video_ts_fixture.py, an independent demuxer written "
            "from ISO 13818-1, so that the Rust reader (src/server/headless/mpegts.rs) is "
            "measured against the standard and the muxer rather than against itself."
        ),
        "packet_bytes": PACKET,
        "total_bytes": len(ts),
        "packets": len(ts) // PACKET,
        "pictures": pictures,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--write", action="store_true", help="record with ffmpeg and describe")
    group.add_argument("--check", action="store_true", help="describe the committed bytes and compare")
    args = parser.parse_args()

    if args.write:
        TS_PATH.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(RECORD + [str(TS_PATH)], check=True)
        ts = TS_PATH.read_bytes()
        manifest = describe(ts)
        JSON_PATH.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"wrote {TS_PATH.relative_to(ROOT)} ({len(ts)} bytes, {len(manifest['pictures'])} pictures)")
        print(f"wrote {JSON_PATH.relative_to(ROOT)}")
        return 0

    failures = []
    if not TS_PATH.exists():
        failures.append(f"missing {TS_PATH.relative_to(ROOT)}")
    if not JSON_PATH.exists():
        failures.append(f"missing {JSON_PATH.relative_to(ROOT)}")
    if not failures:
        described = describe(TS_PATH.read_bytes())
        committed = json.loads(JSON_PATH.read_text())
        if described != committed:
            failures.append(
                f"{JSON_PATH.relative_to(ROOT)} does not describe {TS_PATH.relative_to(ROOT)} — "
                "one of them was edited by hand, or the referee changed and was not rerun"
            )
        if len(described["pictures"]) != FRAMES:
            failures.append(f"expected {FRAMES} pictures, the stream has {len(described['pictures'])}")
        if not all(p["length_written"] for p in described["pictures"]):
            failures.append("a picture's PES length is not written; the recording flags changed")
    for line in failures:
        print(f"FAIL: {line}", file=sys.stderr)
    if failures:
        return 1
    print(f"fixture OK — {TS_PATH.stat().st_size} bytes, {FRAMES} pictures, every length written")
    return 0


if __name__ == "__main__":
    sys.exit(main())
