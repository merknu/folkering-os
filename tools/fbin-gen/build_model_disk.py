#!/usr/bin/env python3
"""Build a model-disk raw image from a `.fbin` file.

The output is a flat block-device image that maps a single `.fbin`
to a dedicated VirtIO block device. Folkering OS attaches it as
virtio2 (or any free disk slot), Synapse VFS recognises the FMDL
header on the first sector, and `read_file_shmem("qwen.fbin")`
streams the named tensor file directly from disk into shmem on
demand — bypassing initrd entirely.

Why this matters: a real Qwen3-0.6B Q8 .fbin is ~232 MB. Forcing
that through initrd means Limine loads it into RAM at boot, which
defeats the whole point of running on edge hardware. With the
model living on its own block device, the OS only pages in the
tensors the current forward pass needs. A 1 GB Pi 5 stays
comfortable; swapping the model is as easy as swapping the SD card.

## On-disk layout

```text
sector 0..7  (4 KiB header):
    +0x000  magic         u32   = b"FMDL"
    +0x004  version       u16   = 1
    +0x006  reserved      u16   = 0
    +0x008  filename      [u8; 256]  NUL-padded UTF-8
    +0x108  data_offset   u64   = 4096 (header size)
    +0x110  data_len      u64   = .fbin size in bytes
    +0x118  fbin_hash     [u8; 32]  reserved for FNV/SHA later
    +0x138  reserved zeros up to 0x1000

sector 8.. (4 KiB-aligned):
    raw .fbin bytes
```

The kernel-side VFS adapter (queued for D.3.7.virtio next session)
reads sector 0, parses the header, exposes the named file via the
existing Synapse `read_file_shmem` interface. Today this script
just produces the image; deploying it as virtio2 on Proxmox is the
follow-up work.

## Usage

    python tools/fbin-gen/build_model_disk.py \\
        --input boot/iso_root/qwen.fbin \\
        --filename qwen.fbin \\
        --out boot/qwen-model.img

To swap models later: rebuild the .fbin → rebuild the .img →
`dd` onto the model disk LVM volume. The kernel side never needs
to know about it; new model just appears on next boot.
"""

import argparse
import struct
import sys
from pathlib import Path


MAGIC = b"FMDL"
VERSION = 1
HEADER_SIZE = 4096
SECTOR_SIZE = 512
FILENAME_BYTES = 256


def build_image(fbin_path: Path, filename: str, out_path: Path):
    fbin_bytes = fbin_path.read_bytes()
    fbin_len = len(fbin_bytes)

    fname = filename.encode("utf-8")
    if len(fname) > FILENAME_BYTES:
        raise SystemExit(
            f"filename {filename!r} exceeds {FILENAME_BYTES} bytes "
            f"(got {len(fname)}). Pick a shorter name."
        )
    fname_padded = fname + b"\x00" * (FILENAME_BYTES - len(fname))

    header = bytearray(HEADER_SIZE)
    header[0:4] = MAGIC
    struct.pack_into("<HH", header, 4, VERSION, 0)
    header[8:8 + FILENAME_BYTES] = fname_padded
    struct.pack_into("<Q", header, 0x108, HEADER_SIZE)         # data_offset
    struct.pack_into("<Q", header, 0x110, fbin_len)            # data_len
    # 0x118..0x138 reserved for content hash; left zero.

    # Pad fbin section to sector alignment so the disk image's
    # logical end sits on a block boundary. Avoids partial-sector
    # reads at the trailing edge.
    pad_len = (-fbin_len) % SECTOR_SIZE

    with out_path.open("wb") as f:
        f.write(header)
        f.write(fbin_bytes)
        if pad_len:
            f.write(b"\x00" * pad_len)

    total = HEADER_SIZE + fbin_len + pad_len
    print(
        f"[build_model_disk] wrote {out_path} "
        f"({total:,} bytes total: {HEADER_SIZE} header + "
        f"{fbin_len:,} payload + {pad_len} pad). "
        f"filename={filename!r}"
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--input", required=True, type=Path,
                    help="Path to the .fbin file to wrap")
    ap.add_argument("--filename", required=True,
                    help="VFS filename Synapse exposes (e.g., qwen.fbin)")
    ap.add_argument("--out", required=True, type=Path,
                    help="Output disk image path (.img)")
    args = ap.parse_args()

    if not args.input.exists():
        raise SystemExit(f"input not found: {args.input}")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    build_image(args.input, args.filename, args.out)


if __name__ == "__main__":
    main()
