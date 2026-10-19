#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Pack PNGs into a macOS .icns icon suite.

ImageMagick on this machine has no ICNS coder, so the container is written
here. The format is a flat sequence of typed chunks: the magic `icns`, a
big-endian u32 holding the size of the whole file, then for each icon a
four-byte OSType, a big-endian u32 holding that chunk's own size including
its eight-byte header, and the payload. Every type used here takes a PNG
payload, which macOS has accepted since 10.7 — no RLE encoder is needed.

Usage:
    write-icns.py OUT.icns TYPE=FILE.png [TYPE=FILE.png ...]

The result is read back and verified before the file is considered written,
so a truncated or mistyped suite fails here rather than on a Mac.
"""

import struct
import sys
from pathlib import Path

MAGIC = b"icns"
HEADER = 8

# OSType -> the pixel size macOS expects behind it. Retina types name the same
# pixel count as a larger non-retina type on purpose; that is how the format
# distinguishes "256 points" from "128 points at 2x".
SIZES = {
    "icp4": 16, "icp5": 32, "icp6": 64,
    "ic07": 128, "ic08": 256, "ic09": 512, "ic10": 1024,
    "ic11": 32, "ic12": 64, "ic13": 256, "ic14": 512,
}

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


def png_size(data: bytes) -> tuple[int, int]:
    """Width and height straight out of the IHDR, without a PNG library."""
    if not data.startswith(PNG_MAGIC) or data[12:16] != b"IHDR":
        raise ValueError("not a PNG")
    return struct.unpack(">II", data[16:24])


def build(entries: list[tuple[str, bytes]]) -> bytes:
    body = b"".join(
        ostype.encode("ascii") + struct.pack(">I", HEADER + len(payload)) + payload
        for ostype, payload in entries
    )
    return MAGIC + struct.pack(">I", HEADER + len(body)) + body


def verify(blob: bytes, entries: list[tuple[str, bytes]]) -> None:
    """Walk the written bytes back the way a reader would."""
    if blob[:4] != MAGIC:
        raise ValueError("magic is not 'icns'")
    total = struct.unpack(">I", blob[4:8])[0]
    if total != len(blob):
        raise ValueError(f"declared length {total} but file is {len(blob)}")

    seen, off = [], HEADER
    while off < total:
        ostype = blob[off:off + 4].decode("ascii")
        length = struct.unpack(">I", blob[off + 4:off + 8])[0]
        if length < HEADER or off + length > total:
            raise ValueError(f"chunk {ostype} has an impossible length {length}")
        seen.append((ostype, blob[off + HEADER:off + length]))
        off += length

    if [t for t, _ in seen] != [t for t, _ in entries]:
        raise ValueError("chunk order does not match what was written")
    for ostype, payload in seen:
        w, h = png_size(payload)
        if (w, h) != (SIZES[ostype], SIZES[ostype]):
            raise ValueError(f"{ostype} holds {w}x{h}, expected {SIZES[ostype]} square")


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2

    out = Path(argv[1])
    entries: list[tuple[str, bytes]] = []
    for arg in argv[2:]:
        ostype, _, path = arg.partition("=")
        if ostype not in SIZES:
            print(f"write-icns: unknown icon type {ostype!r}", file=sys.stderr)
            return 1
        data = Path(path).read_bytes()
        w, h = png_size(data)
        if (w, h) != (SIZES[ostype], SIZES[ostype]):
            print(f"write-icns: {path} is {w}x{h}, but {ostype} needs "
                  f"{SIZES[ostype]}x{SIZES[ostype]}", file=sys.stderr)
            return 1
        entries.append((ostype, data))

    blob = build(entries)
    verify(blob, entries)
    out.write_bytes(blob)
    print(f"write-icns: {out} — {len(entries)} icons, {len(blob)} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
