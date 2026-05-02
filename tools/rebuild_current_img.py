"""One-shot helper: rebuild boot/current.img from base + fresh kernel/initrd
via WSL mcopy. Used when we want to deploy without starting QEMU."""
from pathlib import Path
import shutil
import subprocess

BOOT_DIR = Path('C:/Users/merkn/folkering/folkering-os/boot')
SRC = BOOT_DIR / 'folkering-deploy.img'
DST = BOOT_DIR / 'current.img'
KERNEL = BOOT_DIR / 'iso_root' / 'boot' / 'kernel.elf'
INITRD = BOOT_DIR / 'iso_root' / 'boot' / 'initrd.fpk'
FAT_OFFSET = 1048576

def win_to_wsl(p):
    s = str(p).replace('\\', '/')
    if len(s) >= 2 and s[1] == ':':
        s = '/mnt/' + s[0].lower() + s[2:]
    return s

shutil.copy2(SRC, DST)
wsl_dst = win_to_wsl(DST)

for src_file, fat_paths in [
    (KERNEL, ['::/boot/kernel.elf']),
    (INITRD, ['::/boot/initrd.fpk', '::/initrd.fpk']),
]:
    wsl_src = win_to_wsl(src_file)
    for fat_path in fat_paths:
        cmd = ['wsl', '-d', 'Ubuntu-22.04', '--', 'bash', '-c',
               f"export MTOOLS_SKIP_CHECK=1; mcopy -o -i '{wsl_dst}@@{FAT_OFFSET}' '{wsl_src}' '{fat_path}' 2>&1"]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        kb = src_file.stat().st_size // 1024
        print(f"  {fat_path}({kb}KB): rc={r.returncode} {r.stdout.strip() or 'OK'}")

print(f"current.img: {DST.stat().st_size // (1024*1024)} MB")
