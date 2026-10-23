#!/usr/bin/env python3
"""Compare the Rust streaming_zip_conduit stream against pymobiledevice3's reference.

    python conduit_compare.py <rust_stream.bin> <app_dir>

The reference (pymobiledevice3 11.43.1) is proven to install on the watch. If the Rust stream
matches it record for record, the Rust conduit installs on the watch too. The metadata record is
compared as a parsed plist, not byte for byte, because the two plist serializers format XML
differently while the device only reads the parsed values; every other record is compared exactly.

Exit code 1 on any mismatch.
"""
import plistlib
import struct
import sys
from pathlib import Path

from pymobiledevice3.services.streaming_zip_conduit import (
    _CENTRAL_DIRECTORY_SIGNATURE,
    _app_directory_entries,
    _with_metadata,
)

LOCAL_HEADER = struct.Struct("<IHHHHHIIIHH")
LOCAL_SIG = 0x04034B50
METADATA_NAME = "META-INF/com.apple.ZipMetadata.plist"


def python_stream(app: Path) -> bytes:
    out = bytearray()
    for entry in _with_metadata(_app_directory_entries(app)):
        out += entry.header()
        for chunk in entry.chunks():
            out += chunk
    out += _CENTRAL_DIRECTORY_SIGNATURE
    return bytes(out)


def parse(stream: bytes):
    """Return [(name, extra_bytes, data_bytes), ...] in order, stopping at the central dir sig."""
    records = []
    i = 0
    while i + 4 <= len(stream):
        if stream[i : i + 4] == _CENTRAL_DIRECTORY_SIGNATURE:
            return records, True
        fields = LOCAL_HEADER.unpack_from(stream, i)
        if fields[0] != LOCAL_SIG:
            raise ValueError(f"bad local header signature at offset {i}: {fields[0]:#x}")
        size = fields[8]  # uncompressed == compressed (stored)
        name_len, extra_len = fields[9], fields[10]
        i += LOCAL_HEADER.size
        name = stream[i : i + name_len].decode()
        i += name_len
        extra = bytes(stream[i : i + extra_len])
        i += extra_len
        data = bytes(stream[i : i + size])
        i += size
        records.append((name, extra, data))
    return records, False


def main() -> int:
    rust = Path(sys.argv[1]).read_bytes()
    app = Path(sys.argv[2])
    ref = python_stream(app)

    rust_recs, rust_closed = parse(rust)
    ref_recs, ref_closed = parse(ref)

    problems = []
    if not rust_closed:
        problems.append("Rust stream does not end with the central-directory signature")
    if len(rust_recs) != len(ref_recs):
        problems.append(
            f"record count differs: rust={len(rust_recs)} ref={len(ref_recs)}\n"
            f"  rust names: {[r[0] for r in rust_recs]}\n"
            f"  ref  names: {[r[0] for r in ref_recs]}"
        )

    for idx, (r, f) in enumerate(zip(rust_recs, ref_recs)):
        if r[0] != f[0]:
            problems.append(f"record {idx} name differs: rust={r[0]!r} ref={f[0]!r}")
            continue
        if r[0] == METADATA_NAME:
            if plistlib.loads(r[2]) != plistlib.loads(f[2]):
                problems.append(
                    f"metadata plist differs:\n  rust={plistlib.loads(r[2])}\n  ref ={plistlib.loads(f[2])}"
                )
            continue
        if r[1] != f[1]:
            problems.append(f"record {r[0]!r} extra field differs: rust={r[1]!r} ref={f[1]!r}")
        if r[2] != f[2]:
            problems.append(
                f"record {r[0]!r} data differs: rust={len(r[2])}B ref={len(f[2])}B"
            )

    if problems:
        print("MISMATCH")
        for p in problems:
            print(" -", p)
        return 1
    print(f"OK: {len(rust_recs)} records match the reference")
    return 0


if __name__ == "__main__":
    sys.exit(main())
