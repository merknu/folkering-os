"""Rebuild boot/current.img from base + fresh kernel/initrd via WSL
mcopy. Used to deploy without starting QEMU.

Two base images are supported:
  - boot/folkering-deploy.img      (64 MB — original, fits initrd
                                    + maybe one synthetic .fbin)
  - boot/folkering-deploy-512m.img (512 MB — D.3.7+, fits real-Qwen
                                    .fbin alongside initrd)

The 512m base is selected when boot/iso_root/qwen.fbin exists or
the FOLKERING_DEPLOY_LARGE env var is set to 1. Otherwise the
64 MB base is used.

Files mcopy'd into the FAT volume:
  - kernel.elf, initrd.fpk (always)
  - qwen.fbin              (when present, dropped at /qwen.fbin)
  - qwen.tokb              (when present, dropped at /qwen.tokb)

The kernel reads `qwen.fbin` / `qwen.tokb` via the same VFS path
as the smaller test fixtures inside initrd; folk-pack stores them
in the .fpk and Synapse loads on first request.
"""
from pathlib import Path
import os
import shutil
import subprocess

BOOT_DIR = Path('C:/Users/merkn/folkering/folkering-os/boot')
ISO_ROOT = BOOT_DIR / 'iso_root'
SRC_SMALL = BOOT_DIR / 'folkering-deploy.img'
SRC_LARGE = BOOT_DIR / 'folkering-deploy-512m.img'
DST = BOOT_DIR / 'current.img'
KERNEL = ISO_ROOT / 'boot' / 'kernel.elf'
INITRD = ISO_ROOT / 'boot' / 'initrd.fpk'
FAT_OFFSET = 1048576

want_large = (
    os.environ.get('FOLKERING_DEPLOY_LARGE') == '1'
    or (INITRD.exists() and INITRD.stat().st_size > 50 * 1024 * 1024)
    or (ISO_ROOT / 'qwen.fbin').exists()
)
SRC = SRC_LARGE if want_large else SRC_SMALL
if want_large and not SRC.exists():
    raise SystemExit(
        f"Large base image not found: {SRC}\n"
        "  Build it once: bash tools/build_deploy_512m.sh\n"
        "  (creates a 512 MB FAT32 image with Limine BIOS bootloader)"
    )

def win_to_wsl(p):
    s = str(p).replace('\\', '/')
    if len(s) >= 2 and s[1] == ':':
        s = '/mnt/' + s[0].lower() + s[2:]
    return s

shutil.copy2(SRC, DST)
wsl_dst = win_to_wsl(DST)
print(f"base: {SRC.name} -> current.img ({DST.stat().st_size // (1024*1024)} MB)")

copies = [
    (KERNEL, ['::/boot/kernel.elf']),
    (INITRD, ['::/boot/initrd.fpk', '::/initrd.fpk']),
]

for src_file, fat_paths in copies:
    wsl_src = win_to_wsl(src_file)
    for fat_path in fat_paths:
        cmd = ['wsl', '-d', 'Ubuntu-22.04', '--', 'bash', '-c',
               f"export MTOOLS_SKIP_CHECK=1; mcopy -o -i '{wsl_dst}@@{FAT_OFFSET}' '{wsl_src}' '{fat_path}' 2>&1"]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        mb = src_file.stat().st_size // (1024 * 1024)
        size_str = f"{mb}MB" if mb > 0 else f"{src_file.stat().st_size // 1024}KB"
        print(f"  {fat_path}({size_str}): rc={r.returncode} {r.stdout.strip() or 'OK'}")

print(f"current.img ready: {DST.stat().st_size // (1024*1024)} MB")
