#!/usr/bin/env python3
"""Regenerate the content-server fixtures used by src/cdn.rs tests.

The point is to check fumes' decoder against independent implementations of
each layer: `openssl enc` for AES, Python's lzma / zipfile / compression.zstd
for compression, and a hand-written protobuf encoder for the manifest.
Needs Python 3.14+ (compression.zstd) and openssl on PATH.

    python3 tests/fixtures/gen.py
"""

import base64
import io
import lzma
import random
import struct
import subprocess
import zipfile
import zlib
from compression import zstd
from pathlib import Path

HERE = Path(__file__).parent
KEY = bytes(range(32))
IV = bytes(range(0xA0, 0xB0))


def aes(mode, data, iv=None, pad=True):
    cmd = ["openssl", "enc", f"-aes-256-{mode}", "-K", KEY.hex()]
    if iv is not None:
        cmd += ["-iv", iv.hex()]
    if not pad:
        cmd.append("-nopad")
    return subprocess.run(cmd, input=data, capture_output=True, check=True).stdout


def steam_encrypt(data):
    """IV block encrypted with ECB, then CBC with PKCS#7."""
    return aes("ecb", IV, pad=False) + aes("cbc", data, iv=IV)


def plain_chunk():
    rng = random.Random(1)
    text = b"".join(b"line %d of a fairly compressible game file\n" % i for i in range(3000))
    noise = bytes(rng.getrandbits(8) for _ in range(40_000))
    return text + noise + text[:1000]


def vzip(data):
    alone = lzma.compress(data, format=lzma.FORMAT_ALONE)
    props, stream = alone[:5], alone[13:]  # drop the 8-byte size field
    crc = zlib.crc32(data)
    return b"VZa" + struct.pack("<I", 0) + props + stream + struct.pack("<II", crc, len(data)) + b"zv"


def vzstd(data):
    crc = zlib.crc32(data)
    frame = zstd.compress(data)
    return b"VSZa" + struct.pack("<I", crc) + frame + struct.pack("<III", crc, len(data), 0) + b"zsv"


def zipped(data, name="z"):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(name, data)
    return buf.getvalue()


# --- minimal protobuf encoding ---

def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def field_varint(num, n):
    return varint(num << 3) + varint(n)


def field_bytes(num, data):
    if isinstance(data, str):
        data = data.encode()
    return varint(num << 3 | 2) + varint(len(data)) + data


def field_fixed32(num, n):
    return varint(num << 3 | 5) + struct.pack("<I", n)


def enc_name(name):
    return base64.b64encode(steam_encrypt(name.encode() + b"\0")).decode()


def chunk_msg(sha, crc, offset, size):
    return (field_bytes(1, sha) + field_fixed32(2, crc) + field_varint(3, offset)
            + field_varint(4, size) + field_varint(5, size // 2))


def mapping(name, size, flags, chunks=(), link=None):
    msg = field_bytes(1, enc_name(name)) + field_varint(2, size) + field_varint(3, flags)
    msg += field_bytes(5, bytes(20))
    for c in chunks:
        msg += field_bytes(6, chunk_msg(*c))
    if link:
        msg += field_bytes(7, enc_name(link))
    return msg


def manifest():
    mib = 1024 * 1024
    exe_chunks = [  # deliberately out of order
        (bytes([3] * 20), 3, 2 * mib, 2_500_000 - 2 * mib),
        (bytes([1] * 20), 1, 0, mib),
        (bytes([2] * 20), 2, mib, mib),
    ]
    payload = b"".join(field_bytes(1, m) for m in [
        mapping("bin\\win64\\game.exe", 2_500_000, 32, exe_chunks),
        mapping("data", 0, 64),
        mapping("data\\pak0.pak", 10, 0, [(bytes([4] * 20), 4, 0, 10)]),
        mapping("link", 0, 512, link="bin/win64/game.exe"),
    ])
    metadata = (field_varint(1, 4001) + field_varint(2, 1234567890123456789)
                + field_varint(3, 1700000000) + field_varint(4, 1))
    signature = field_bytes(1, b"\x00" * 16)
    body = b""
    for magic, section in [(0x71F617D0, payload), (0x1F4812BE, metadata), (0x1B81B817, signature)]:
        body += struct.pack("<II", magic, len(section)) + section
    body += struct.pack("<I", 0x32C415AB)
    return zipped(body)


def main():
    data = plain_chunk()
    (HERE / "chunk.plain").write_bytes(data)
    (HERE / "chunk.vzip.enc").write_bytes(steam_encrypt(vzip(data)))
    (HERE / "chunk.zip.enc").write_bytes(steam_encrypt(zipped(data)))
    (HERE / "chunk.vzstd.enc").write_bytes(steam_encrypt(vzstd(data)))
    (HERE / "manifest.zip").write_bytes(manifest())


if __name__ == "__main__":
    main()
