#!/usr/bin/env python3
"""Copy files to FAT32 disk image using pyfatfs"""

import sys
import os

try:
    from pyfatfs.PyFat import PyFat
    from pyfatfs.FATDirectoryEntry import FATDirectoryEntry
except ImportError:
    print("Installing pyfatfs...")
    import subprocess
    subprocess.check_call([sys.executable, "-m", "pip", "install", "pyfatfs"])
    from pyfatfs.PyFat import PyFat
    from pyfatfs.FATDirectoryEntry import FATDirectoryEntry

def copy_file_to_img(img_path, src_path, dest_path):
    """Copy a file into the FAT32 image"""
    with open(img_path, "r+b") as f:
        fat = PyFat(encoding="utf-8")
        fat.open_file(f)

        # Create directory if needed
        dest_dir = os.path.dirname(dest_path)
        if dest_dir and dest_dir != "/":
            parts = dest_path.split("/")
            current_path = ""
            for part in parts[:-1]:
                if part:
                    current_path += "/" + part
                    try:
                        fat.mkdir(current_path)
                    except:
                        pass  # Directory exists

        # Copy file
        with open(src_path, "rb") as src:
            data = src.read()
            fat.put_file(dest_path, data)

        fat.close()

if __name__ == "__main__":
    img = "/tmp/boot.img"
    base = "/mnt/c/Users/merkn/OneDrive/Dokumenter/Meray_vault/Meray/Projects/Folkering-OS/code/kernel"

    files_to_copy = [
        (f"{base}/iso_root/boot/kernel.elf", "/boot/kernel.elf"),
        (f"{base}/limine.conf", "/limine.conf"),
        (f"{base}/iso_root/boot/limine-bios.sys", "/boot/limine-bios.sys"),
    ]

    for src, dest in files_to_copy:
        print(f"Copying {os.path.basename(src)} -> {dest}")
        copy_file_to_img(img, src, dest)

    print("All files copied successfully!")
