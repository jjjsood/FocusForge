#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Classic-TIFF header reader, rect reader and writer -- stdlib only.

Why this exists rather than PIL or tifffile: the reference stack is 61 frames
of 268 MB each, and every question the fixture tooling asks is about a header
or a 512x512 window. An image library answers those by decoding the frame.
This module answers them by seeking: `read_info` touches the first IFD and
nothing else, and `read_rect` mmaps the file and copies `h` row slices out of
it, so a crop costs the pages it actually overlaps and no more. Never add a
"just read the whole image" convenience here -- that is the one operation this
module exists to make impossible.

Scope is deliberately narrow: classic TIFF (magic 42), uncompressed, chunky,
strip-based, 8- or 16-bit, first IFD only. Anything else raises
`UnsupportedTiff` with a message naming what it found. The reference stack is
inside that envelope twice over and in two different shapes:

  - the 61 source frames are little-endian `II`, RowsPerStrip=1 (5464 strips)
  - `Mantis_Stacked.tif` is big-endian `MM`, RowsPerStrip=8 (683 strips)

so both byte orders and an arbitrary RowsPerStrip are load-bearing, not
defensive generality. `read_rect` always returns NATIVE LITTLE-ENDIAN samples
regardless of the source's byte order, which is what lets a caller compare a
crop of the MM reference against a crop of an II frame without knowing either
file's header.

Usage:
    python3 tools/lib/rawtiff.py --selftest                       # must pass; no fixtures needed
    python3 tools/lib/rawtiff.py <file.tif>                       # print the parsed header
    python3 tools/lib/rawtiff.py --rect X,Y,W,H --out FILE <file.tif>
        # write read_rect(...)'s raw native-little-endian sample bytes to FILE
"""

from __future__ import annotations

import hashlib
import mmap
import os
import struct
import sys
import zlib
from dataclasses import dataclass, field


class UnsupportedTiff(Exception):
    """The file is not a TIFF this module can read, and says why."""


# TIFF field types -> (byte size, struct code). RATIONAL and the float types
# are sized so an IFD walk can skip them; they are never returned as values.
_TYPE_SIZE = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8}
_TYPE_CODE = {1: "B", 3: "H", 4: "I", 6: "b", 7: "B", 8: "h", 9: "i", 11: "f", 12: "d"}

TAG_IMAGE_WIDTH = 256
TAG_IMAGE_LENGTH = 257
TAG_BITS_PER_SAMPLE = 258
TAG_COMPRESSION = 259
TAG_PHOTOMETRIC = 262
TAG_IMAGE_DESCRIPTION = 270
TAG_STRIP_OFFSETS = 273
TAG_SAMPLES_PER_PIXEL = 277
TAG_ROWS_PER_STRIP = 278
TAG_STRIP_BYTE_COUNTS = 279
TAG_PLANAR_CONFIG = 284
TAG_PREDICTOR = 317
TAG_TILE_WIDTH = 322
TAG_TILE_LENGTH = 323
TAG_TILE_OFFSETS = 324
TAG_TILE_BYTE_COUNTS = 325
TAG_SAMPLE_FORMAT = 339


@dataclass
class TiffInfo:
    """Everything the first IFD says that a rect read or a rewrite needs."""

    path: str
    width: int
    height: int
    byte_order: str  # 'II' or 'MM'
    bits_per_sample: tuple
    samples_per_pixel: int
    compression: int
    photometric: int
    planar_config: int
    rows_per_strip: int
    strip_offsets: list = field(default_factory=list)
    strip_byte_counts: list = field(default_factory=list)
    sample_format: tuple = ()
    image_description: str | None = None
    file_size: int = 0

    @property
    def bytes_per_sample(self) -> int:
        return self.bits_per_sample[0] // 8

    @property
    def row_bytes(self) -> int:
        """Bytes of one full image row, chunky."""
        return self.width * self.samples_per_pixel * self.bytes_per_sample

    def describe(self) -> str:
        return (
            f"{self.path}\n"
            f"  {self.width}x{self.height} {self.samples_per_pixel}ch "
            f"{self.bits_per_sample[0]}-bit {self.byte_order} "
            f"compression={self.compression} planar={self.planar_config} "
            f"photometric={self.photometric}\n"
            f"  rows_per_strip={self.rows_per_strip} strips={len(self.strip_offsets)} "
            f"row_bytes={self.row_bytes} file_size={self.file_size}\n"
            f"  description={self.image_description!r}"
        )


def _endian(byte_order: str) -> str:
    return "<" if byte_order == "II" else ">"


def _read_entry(fh, endian: str, entry: tuple, file_size: int, path: str):
    """Resolve one IFD entry's value, inline (<=4 bytes) or via its offset."""
    ttype, count, payload = entry
    size = _TYPE_SIZE.get(ttype)
    if size is None:
        return None
    total = size * count
    if total <= 4:
        data = payload[:total]
    else:
        (offset,) = struct.unpack(endian + "I", payload)
        if offset + total > file_size:
            raise UnsupportedTiff(f"{path}: IFD value runs past end of file")
        fh.seek(offset)
        data = fh.read(total)
        if len(data) != total:
            raise UnsupportedTiff(f"{path}: short read for IFD value")
    if ttype == 2:
        return data.split(b"\x00", 1)[0].decode("utf-8", "replace")
    code = _TYPE_CODE.get(ttype)
    if code is None:
        return None
    return list(struct.unpack(f"{endian}{count}{code}", data))


def read_info(path: str) -> TiffInfo:
    """Parse the first IFD of a classic TIFF. Reads a few hundred bytes."""
    file_size = os.path.getsize(path)
    with open(path, "rb") as fh:
        head = fh.read(8)
        if len(head) < 8:
            raise UnsupportedTiff(f"{path}: too short to be a TIFF")
        byte_order = head[:2].decode("ascii", "replace")
        if byte_order not in ("II", "MM"):
            raise UnsupportedTiff(f"{path}: not a TIFF (byte-order mark {head[:2]!r})")
        endian = _endian(byte_order)
        magic, ifd_offset = struct.unpack(endian + "HI", head[2:8])
        if magic == 43:
            raise UnsupportedTiff(
                f"{path}: BigTIFF (magic 43) -- this reader handles classic TIFF only"
            )
        if magic != 42:
            raise UnsupportedTiff(f"{path}: bad TIFF magic {magic} (expected 42)")

        fh.seek(ifd_offset)
        raw_count = fh.read(2)
        if len(raw_count) != 2:
            raise UnsupportedTiff(f"{path}: IFD offset {ifd_offset} is past end of file")
        (n_entries,) = struct.unpack(endian + "H", raw_count)
        raw = fh.read(12 * n_entries)
        if len(raw) != 12 * n_entries:
            raise UnsupportedTiff(f"{path}: truncated IFD ({n_entries} entries)")
        entries = {}
        for i in range(n_entries):
            tag, ttype, count = struct.unpack_from(endian + "HHI", raw, 12 * i)
            entries[tag] = (ttype, count, raw[12 * i + 8 : 12 * i + 12])

        def values(tag, default=None):
            if tag not in entries:
                return default
            got = _read_entry(fh, endian, entries[tag], file_size, path)
            return default if got is None else got

        def scalar(tag, default=None):
            got = values(tag)
            if got is None:
                return default
            return got[0] if isinstance(got, list) else got

        if TAG_TILE_WIDTH in entries:
            raise UnsupportedTiff(
                f"{path}: tiled TIFF (TileWidth present) -- this reader handles strips only"
            )

        width = scalar(TAG_IMAGE_WIDTH)
        height = scalar(TAG_IMAGE_LENGTH)
        if not width or not height:
            raise UnsupportedTiff(f"{path}: missing ImageWidth/ImageLength")

        spp = scalar(TAG_SAMPLES_PER_PIXEL, 1)
        bits = tuple(values(TAG_BITS_PER_SAMPLE, [1] * spp))
        compression = scalar(TAG_COMPRESSION, 1)
        planar = scalar(TAG_PLANAR_CONFIG, 1)

        if compression != 1:
            raise UnsupportedTiff(
                f"{path}: Compression={compression} -- only uncompressed (1) is supported"
            )
        if planar != 1:
            raise UnsupportedTiff(
                f"{path}: PlanarConfiguration={planar} -- only chunky (1) is supported"
            )
        if set(bits) not in ({8}, {16}):
            raise UnsupportedTiff(
                f"{path}: BitsPerSample={bits} -- only all-8 or all-16 is supported"
            )

        strip_offsets = values(TAG_STRIP_OFFSETS)
        strip_byte_counts = values(TAG_STRIP_BYTE_COUNTS)
        if not strip_offsets:
            raise UnsupportedTiff(f"{path}: no StripOffsets")

        # The TIFF default for RowsPerStrip is 2**32-1, i.e. "one strip holds
        # the whole image". Clamp so `r // rows_per_strip` is meaningful.
        rows_per_strip = scalar(TAG_ROWS_PER_STRIP, height)
        if rows_per_strip <= 0 or rows_per_strip > height:
            rows_per_strip = height

        expected_strips = (height + rows_per_strip - 1) // rows_per_strip
        if len(strip_offsets) != expected_strips:
            raise UnsupportedTiff(
                f"{path}: {len(strip_offsets)} StripOffsets but "
                f"{expected_strips} strips implied by RowsPerStrip={rows_per_strip}"
            )

        return TiffInfo(
            path=path,
            width=width,
            height=height,
            byte_order=byte_order,
            bits_per_sample=bits,
            samples_per_pixel=spp,
            compression=compression,
            photometric=scalar(TAG_PHOTOMETRIC, 1),
            planar_config=planar,
            rows_per_strip=rows_per_strip,
            strip_offsets=list(strip_offsets),
            strip_byte_counts=list(strip_byte_counts or []),
            sample_format=tuple(values(TAG_SAMPLE_FORMAT, [1] * spp)),
            image_description=values(TAG_IMAGE_DESCRIPTION),
            file_size=file_size,
        )


def _swap16(buf: bytearray) -> None:
    """Swap every 16-bit pair in place, independently of host endianness."""
    buf[0::2], buf[1::2] = buf[1::2], buf[0::2]


def read_rect(path: str, x: int, y: int, w: int, h: int, info: TiffInfo | None = None) -> bytes:
    """Return the w*h chunky pixel rect at (x, y) as native little-endian bytes.

    Row r of an uncompressed strip TIFF lives in strip `r // rows_per_strip` at
    row offset `(r % rows_per_strip) * row_bytes`, so the rect is h seeks into
    an mmap and nothing else -- no decode, no full-image buffer.
    """
    if info is None:
        info = read_info(path)
    if w <= 0 or h <= 0:
        raise ValueError(f"rect {w}x{h} must have positive width and height")
    if x < 0 or y < 0 or x + w > info.width or y + h > info.height:
        raise ValueError(
            f"rect {x},{y},{w},{h} is outside {info.width}x{info.height} in {path}"
        )

    bps = info.bytes_per_sample
    stride = info.samples_per_pixel * bps
    row_bytes = info.row_bytes
    rect_row_bytes = w * stride
    rps = info.rows_per_strip
    out = bytearray()

    with open(path, "rb") as fh:
        with mmap.mmap(fh.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            for r in range(y, y + h):
                strip = r // rps
                row_in_strip = r % rps
                if info.strip_byte_counts:
                    need = (row_in_strip + 1) * row_bytes
                    if need > info.strip_byte_counts[strip]:
                        raise UnsupportedTiff(
                            f"{path}: strip {strip} holds "
                            f"{info.strip_byte_counts[strip]} bytes, row {r} needs {need}"
                        )
                start = info.strip_offsets[strip] + row_in_strip * row_bytes + x * stride
                chunk = mm[start : start + rect_row_bytes]
                if len(chunk) != rect_row_bytes:
                    raise UnsupportedTiff(f"{path}: row {r} runs past end of file")
                out += chunk

    if bps == 2 and info.byte_order == "MM":
        _swap16(out)
    return bytes(out)


def _lzw_encode(data: bytes) -> bytes:
    """Encode TIFF's MSB-first LZW stream with clear and end codes."""
    clear = 256
    end = 257
    codes = []
    width = 9
    next_code = 258
    dictionary = {bytes([value]): value for value in range(256)}
    codes.append((clear, width))
    if not data:
        codes.append((end, width))
    else:
        word = bytes([data[0]])
        for value in data[1:]:
            extended = word + bytes([value])
            if extended in dictionary:
                word = extended
                continue
            codes.append((dictionary[word], width))
            if next_code < 4096:
                dictionary[extended] = next_code
                next_code += 1
                if next_code == (1 << width) and width < 12:
                    width += 1
            else:
                codes.append((clear, width))
                dictionary = {bytes([item]): item for item in range(256)}
                width = 9
                next_code = 258
            word = bytes([value])
        codes.append((dictionary[word], width))
        codes.append((end, width))

    out = bytearray()
    bits = 0
    count = 0
    for code, code_width in codes:
        bits = (bits << code_width) | code
        count += code_width
        while count >= 8:
            count -= 8
            out.append((bits >> count) & 0xFF)
    if count:
        out.append((bits << (8 - count)) & 0xFF)
    return bytes(out)


def _horizontal_predict(data: bytes, width: int, height: int, spp: int, bits: int) -> bytes:
    """Return predictor-2 differences from native little-endian packed samples."""
    bps = bits // 8
    row_bytes = width * spp * bps
    out = bytearray(data)
    if bits == 8:
        for row in range(height):
            start = row * row_bytes
            for index in range(width * spp - 1, spp - 1, -1):
                pos = start + index
                out[pos] = (out[pos] - out[pos - spp]) & 0xFF
        return bytes(out)
    for row in range(height):
        start = row * row_bytes
        for index in range(width * spp - 1, spp - 1, -1):
            pos = start + index * 2
            previous = start + (index - spp) * 2
            value = (
                struct.unpack_from("<H", out, pos)[0]
                - struct.unpack_from("<H", out, previous)[0]
            ) & 0xFFFF
            struct.pack_into("<H", out, pos, value)
    return bytes(out)


def _disk_bytes(data: bytes, bits: int, byte_order: str) -> bytes:
    out = bytearray(data)
    if bits == 16 and byte_order == "MM":
        _swap16(out)
    return bytes(out)


def _build_tiff(
    width: int,
    height: int,
    pixels: bytes,
    bits: int,
    spp: int,
    description: str | None,
    byte_order: str,
    rows_per_strip: int,
    compression: int,
    predictor: int,
    tile: tuple[int, int] | None,
) -> bytes:
    """Serialise chunky classic TIFF with optional compressed strips or tiles."""
    if bits not in (8, 16):
        raise ValueError(f"bits={bits}: only 8 or 16 supported")
    if spp < 1:
        raise ValueError(f"spp={spp}: need at least one sample per pixel")
    if byte_order not in ("II", "MM"):
        raise ValueError(f"byte_order={byte_order!r}: expected 'II' or 'MM'")
    if rows_per_strip < 1:
        raise ValueError(f"rows_per_strip={rows_per_strip}: must be positive")
    if compression not in (1, 5, 8, 32946):
        raise ValueError(f"compression={compression}: expected 1, 5, 8, or 32946")
    if predictor not in (1, 2):
        raise ValueError(f"predictor={predictor}: expected 1 or 2")
    if tile is not None and (len(tile) != 2 or tile[0] < 1 or tile[1] < 1):
        raise ValueError(f"tile={tile!r}: expected positive (width, height)")
    bps = bits // 8
    row_bytes = width * spp * bps
    if len(pixels) != row_bytes * height:
        raise ValueError(
            f"pixels is {len(pixels)} bytes, expected {row_bytes * height} "
            f"for {width}x{height}x{spp} at {bits}-bit"
        )
    endian = _endian(byte_order)
    n_strips = (height + rows_per_strip - 1) // rows_per_strip

    # PhotometricInterpretation 2 (RGB) needs three samples. A single-sample
    # image -- a Z-Map index plane -- must say 1 (BlackIsZero) or the file is
    # invalid and readers reject it.
    photometric = 2 if spp >= 3 else 1

    entries = []  # [tag, type, count, value bytes]

    def add(tag, ttype, vals):
        if ttype == 2:
            data = vals.encode("utf-8") + b"\x00"
            count = len(data)
        else:
            code = {3: "H", 4: "I"}[ttype]
            count = len(vals)
            data = struct.pack(f"{endian}{count}{code}", *vals)
        entries.append([tag, ttype, count, data])

    add(TAG_IMAGE_WIDTH, 4, [width])
    add(TAG_IMAGE_LENGTH, 4, [height])
    add(TAG_BITS_PER_SAMPLE, 3, [bits] * spp)
    add(TAG_COMPRESSION, 3, [compression])
    add(TAG_PHOTOMETRIC, 3, [photometric])
    if description is not None:
        add(TAG_IMAGE_DESCRIPTION, 2, description)
    add(TAG_SAMPLES_PER_PIXEL, 3, [spp])
    if predictor == 2:
        add(TAG_PREDICTOR, 3, [predictor])
    if tile is None:
        add(TAG_STRIP_OFFSETS, 4, [0] * n_strips)
        add(TAG_ROWS_PER_STRIP, 4, [rows_per_strip])
        add(TAG_STRIP_BYTE_COUNTS, 4, [0] * n_strips)
    else:
        tile_width, tile_height = tile
        tiles_across = (width + tile_width - 1) // tile_width
        tiles_down = (height + tile_height - 1) // tile_height
        n_tiles = tiles_across * tiles_down
        add(TAG_TILE_WIDTH, 4, [tile_width])
        add(TAG_TILE_LENGTH, 4, [tile_height])
        add(TAG_TILE_OFFSETS, 4, [0] * n_tiles)
        add(TAG_TILE_BYTE_COUNTS, 4, [0] * n_tiles)
    add(TAG_PLANAR_CONFIG, 3, [1])
    add(TAG_SAMPLE_FORMAT, 3, [1] * spp)
    entries.sort(key=lambda e: e[0])

    # Layout: header, IFD, out-of-line values, pixels. Every value begins on a
    # word boundary, as TIFF requires.
    n = len(entries)
    cursor = 8 + 2 + 12 * n + 4
    cursor += cursor % 2
    offsets = {}
    for tag, _ttype, _count, data in entries:
        if len(data) > 4:
            offsets[tag] = cursor
            cursor += len(data) + len(data) % 2
    pixel_start = cursor

    segments = []
    if tile is None:
        for strip in range(n_strips):
            first = strip * rows_per_strip
            rows = min(rows_per_strip, height - first)
            segment = pixels[first * row_bytes : (first + rows) * row_bytes]
            segments.append((segment, width, rows))
    else:
        tile_width, tile_height = tile
        for y in range(0, height, tile_height):
            for x in range(0, width, tile_width):
                segment = bytearray(tile_width * tile_height * spp * bps)
                rows = min(tile_height, height - y)
                columns = min(tile_width, width - x)
                for row in range(rows):
                    source = ((y + row) * width + x) * spp * bps
                    target = row * tile_width * spp * bps
                    segment[target : target + columns * spp * bps] = pixels[
                        source : source + columns * spp * bps
                    ]
                segments.append((bytes(segment), tile_width, tile_height))

    encoded = []
    for segment, segment_width, segment_height in segments:
        if predictor == 2:
            segment = _horizontal_predict(segment, segment_width, segment_height, spp, bits)
        segment = _disk_bytes(segment, bits, byte_order)
        if compression == 5:
            segment = _lzw_encode(segment)
        elif compression in (8, 32946):
            segment = zlib.compress(segment, level=9)
        encoded.append(segment)

    segment_offsets = []
    cursor = pixel_start
    for segment in encoded:
        segment_offsets.append(cursor)
        cursor += len(segment)
    for entry in entries:
        if entry[0] in (TAG_STRIP_OFFSETS, TAG_TILE_OFFSETS):
            entry[3] = struct.pack(f"{endian}{len(segment_offsets)}I", *segment_offsets)
        elif entry[0] in (TAG_STRIP_BYTE_COUNTS, TAG_TILE_BYTE_COUNTS):
            entry[3] = struct.pack(f"{endian}{len(encoded)}I", *(len(segment) for segment in encoded))

    buf = bytearray(pixel_start)
    buf[0:2] = byte_order.encode("ascii")
    struct.pack_into(endian + "HI", buf, 2, 42, 8)
    struct.pack_into(endian + "H", buf, 8, n)
    pos = 10
    for tag, ttype, count, data in entries:
        struct.pack_into(endian + "HHI", buf, pos, tag, ttype, count)
        if len(data) <= 4:
            buf[pos + 8 : pos + 8 + len(data)] = data
        else:
            struct.pack_into(endian + "I", buf, pos + 8, offsets[tag])
            buf[offsets[tag] : offsets[tag] + len(data)] = data
        pos += 12
    struct.pack_into(endian + "I", buf, pos, 0)  # no next IFD

    return bytes(buf) + b"".join(encoded)


def write_tiff(
    path: str,
    width: int,
    height: int,
    pixels: bytes,
    *,
    bits: int = 16,
    spp: int = 3,
    description: str | None = None,
    byte_order: str = "II",
    rows_per_strip: int = 1,
    compression: int = 1,
    predictor: int = 1,
    tile: tuple[int, int] | None = None,
) -> None:
    """Write chunky classic TIFF. `pixels` is always native little-endian.

    Deterministic: the same arguments produce the same bytes, with no timestamp
    or generator tag anywhere, which is what lets `make-fixtures.sh --check`
    compare a regenerated fixture byte for byte.

    The defaults are the shape the fixtures use and the shape the mmap reader is
    written against: little-endian, one strip per row. `byte_order='MM'` and a
    larger `rows_per_strip` exist because the reference result the project
    compares against -- `Mantis_Stacked.tif` -- is big-endian with
    RowsPerStrip=8, and a reader that only ever sees II/rps=1 files has not been
    tested against the layout it will actually meet. The last strip is short
    when the height is not a multiple of `rows_per_strip`, and StripByteCounts
    says so.
    """
    with open(path, "wb") as fh:
        fh.write(
            _build_tiff(
                width,
                height,
                pixels,
                bits,
                spp,
                description,
                byte_order,
                rows_per_strip,
                compression,
                predictor,
                tile,
            )
        )


def sha256_file(path: str) -> str:
    """Full-file sha256. Streams, but still reads every byte -- see sha256_head."""
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_prefix(path: str, n: int = 12) -> str:
    """Hex prefix of the full-file sha256.

    For the small generated outputs and for `--check`. NEVER call this on a
    reference-stack frame by default: each is 268 MB, and 61 of them is 16 GB
    of reads to answer a provenance question. Use `sha256_head` for those.
    """
    return sha256_file(path)[:n]


def sha256_head(path: str, nbytes: int = 65536, n: int = 16) -> str:
    """Hex prefix of the sha256 of the file's first `nbytes`.

    Cheap and honest provenance for a source frame: it pins the header, the
    camera metadata and the first strips, it is one page-aligned read, and the
    README that records it says exactly what it covers rather than implying a
    whole-file digest.
    """
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read(nbytes)).hexdigest()[:n]


def _selftest() -> int:
    import tempfile

    failures = []

    def check(label, got, want):
        if got != want:
            failures.append(f"{label}: got {got!r}, want {want!r}")

    with tempfile.TemporaryDirectory() as tmp:
        # --- 16-bit RGB, II, one row per strip -------------------------------
        w, h, spp = 5, 3, 3
        px = bytearray()
        for y in range(h):
            for x in range(w):
                for c in range(spp):
                    px += struct.pack("<H", 1000 * (y + 1) + 10 * x + c)
        px = bytes(px)

        p = os.path.join(tmp, "ii.tif")
        write_tiff(p, w, h, px, bits=16, spp=spp, description="rawtiff selftest")

        info = read_info(p)
        check("ii width", info.width, w)
        check("ii height", info.height, h)
        check("ii byte_order", info.byte_order, "II")
        check("ii bits", info.bits_per_sample, (16, 16, 16))
        check("ii spp", info.samples_per_pixel, spp)
        check("ii compression", info.compression, 1)
        check("ii planar", info.planar_config, 1)
        check("ii photometric", info.photometric, 2)
        check("ii rps", info.rows_per_strip, 1)
        check("ii strips", len(info.strip_offsets), h)
        check("ii counts", info.strip_byte_counts, [w * spp * 2] * h)
        check("ii sample_format", info.sample_format, (1, 1, 1))
        check("ii description", info.image_description, "rawtiff selftest")
        check("ii file_size", info.file_size, os.path.getsize(p))

        check("ii full rect", read_rect(p, 0, 0, w, h), px)

        def want_rect(x0, y0, rw, rh):
            out = bytearray()
            for yy in range(y0, y0 + rh):
                start = (yy * w + x0) * spp * 2
                out += px[start : start + rw * spp * 2]
            return bytes(out)

        check("ii 2x2 rect", read_rect(p, 1, 1, 2, 2), want_rect(1, 1, 2, 2))
        check("ii 1x1 corner", read_rect(p, 4, 2, 1, 1), want_rect(4, 2, 1, 1))

        # Deterministic: same arguments, same bytes.
        p2 = os.path.join(tmp, "ii2.tif")
        write_tiff(p2, w, h, px, bits=16, spp=spp, description="rawtiff selftest")
        check("deterministic bytes", sha256_file(p2), sha256_file(p))

        # --- same pixels as big-endian MM with RowsPerStrip=2 ----------------
        # The layout Mantis_Stacked.tif actually has, in miniature. read_rect
        # must byte-swap to little-endian and must resolve rows that sit at a
        # nonzero offset inside their strip. Written through the public writer,
        # so this exercises the path make_fixtures.py uses rather than a
        # hand-built file that could drift away from it.
        pmm = os.path.join(tmp, "mm.tif")
        write_tiff(pmm, w, h, px, bits=16, spp=spp, byte_order="MM", rows_per_strip=2)

        mm_info = read_info(pmm)
        check("mm byte_order", mm_info.byte_order, "MM")
        check("mm rps", mm_info.rows_per_strip, 2)
        check("mm strips", len(mm_info.strip_offsets), 2)
        check("mm counts", mm_info.strip_byte_counts, [2 * w * spp * 2, 1 * w * spp * 2])
        check("mm description", mm_info.image_description, None)
        # On disk the samples are big-endian, so a raw compare must NOT match.
        with open(pmm, "rb") as fh:
            raw = fh.read()
        check("mm stored big-endian", px in raw, False)
        # Row 1 is at offset 1 inside strip 0; row 2 is row 0 of strip 1.
        check("mm full rect swaps", read_rect(pmm, 0, 0, w, h), px)
        check("mm mid-strip row", read_rect(pmm, 1, 1, 2, 2), want_rect(1, 1, 2, 2))
        check("mm strip boundary", read_rect(pmm, 0, 2, w, 1), want_rect(0, 2, w, 1))
        # The point of the two writers being one writer: whatever the on-disk
        # layout, read_rect hands back the same samples.
        check("mm equals ii", read_rect(pmm, 0, 0, w, h), read_rect(p, 0, 0, w, h))
        check("mm equals ii rect", read_rect(pmm, 1, 0, 3, 3), read_rect(p, 1, 0, 3, 3))
        # h=3, rps=2 -> the last strip holds one row, and must say so.
        check("mm short last strip", mm_info.strip_byte_counts[-1], w * spp * 2)

        # A ragged rps on the II side too: 3 rows at rps=8 is a single short strip.
        pragged = os.path.join(tmp, "ragged.tif")
        write_tiff(pragged, w, h, px, bits=16, spp=spp, rows_per_strip=8)
        ragged = read_info(pragged)
        check("ragged strips", len(ragged.strip_offsets), 1)
        check("ragged counts", ragged.strip_byte_counts, [h * w * spp * 2])
        check("ragged rect", read_rect(pragged, 0, 0, w, h), px)

        # --- 8-bit single channel -------------------------------------------
        g = bytes(range(w * h))
        pg = os.path.join(tmp, "gray.tif")
        write_tiff(pg, w, h, g, bits=8, spp=1, description=None)
        ginfo = read_info(pg)
        check("gray bits", ginfo.bits_per_sample, (8,))
        check("gray spp", ginfo.samples_per_pixel, 1)
        check("gray photometric", ginfo.photometric, 1)
        check("gray rect", read_rect(pg, 2, 1, 3, 2), bytes([7, 8, 9, 12, 13, 14]))

        # --- bounds and refusals ---------------------------------------------
        for label, x0, y0, rw, rh in (
            ("negative x", -1, 0, 1, 1),
            ("past right", 4, 0, 2, 1),
            ("past bottom", 0, 2, 1, 2),
            ("zero width", 0, 0, 0, 1),
        ):
            try:
                read_rect(p, x0, y0, rw, rh)
            except ValueError:
                pass
            else:
                failures.append(f"{label}: read_rect accepted an out-of-range rect")

        bad = os.path.join(tmp, "big.tif")
        with open(bad, "wb") as fh:
            fh.write(b"II" + struct.pack("<HI", 43, 8) + b"\x00" * 16)
        try:
            read_info(bad)
        except UnsupportedTiff as exc:
            check("bigtiff message", "BigTIFF" in str(exc), True)
        else:
            failures.append("BigTIFF: read_info accepted magic 43")

        notiff = os.path.join(tmp, "no.tif")
        with open(notiff, "wb") as fh:
            fh.write(b"\x89PNG\r\n\x1a\n")
        try:
            read_info(notiff)
        except UnsupportedTiff:
            pass
        else:
            failures.append("non-TIFF: read_info accepted a PNG header")

        # --- --rect/--out subcommand -------------------------------------------
        rect_out = os.path.join(tmp, "rect_out.bin")
        rc = main(["rawtiff.py", "--rect", "1,1,2,2", "--out", rect_out, p])
        check("rect/out exit code", rc, 0)
        with open(rect_out, "rb") as fh:
            written = fh.read()
        check("rect/out bytes match read_rect", written, read_rect(p, 1, 1, 2, 2))

        bad_rc = main(["rawtiff.py", "--rect", "not-a-rect", "--out", rect_out, p])
        check("rect/out bad argument exit code", bad_rc, 2)

        # --- hashing ----------------------------------------------------------
        check("sha256_prefix length", len(sha256_prefix(p)), 12)
        check("sha256_prefix agrees", sha256_prefix(p, 64), sha256_file(p))
        check("sha256_head length", len(sha256_head(p)), 16)
        check(
            "sha256_head of short file",
            sha256_head(p),
            hashlib.sha256(open(p, "rb").read()).hexdigest()[:16],
        )

    if failures:
        print("rawtiff selftest FAILED:")
        for line in failures:
            print(f"  - {line}")
        return 1
    print("rawtiff selftest OK")
    return 0


def _parse_rect(spec: str) -> tuple:
    parts = spec.split(",")
    if len(parts) != 4:
        raise ValueError(f"--rect wants X,Y,W,H, got {spec!r}")
    try:
        x, y, w, h = (int(p) for p in parts)
    except ValueError as exc:
        raise ValueError(f"--rect wants four integers, got {spec!r}") from exc
    return x, y, w, h


def main(argv) -> int:
    if len(argv) == 2 and argv[1] == "--selftest":
        return _selftest()
    if len(argv) == 2 and not argv[1].startswith("-"):
        print(read_info(argv[1]).describe())
        return 0
    if len(argv) == 6 and argv[1] == "--rect" and argv[3] == "--out":
        try:
            x, y, w, h = _parse_rect(argv[2])
        except ValueError as exc:
            print(f"rawtiff.py: {exc}", file=sys.stderr)
            return 2
        out_path = argv[4]
        in_path = argv[5]
        data = read_rect(in_path, x, y, w, h)
        with open(out_path, "wb") as fh:
            fh.write(data)
        print(f"wrote {len(data)} bytes")
        return 0
    print(
        "usage: rawtiff.py --selftest | rawtiff.py <file.tif> | "
        "rawtiff.py --rect X,Y,W,H --out FILE <file.tif>",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
