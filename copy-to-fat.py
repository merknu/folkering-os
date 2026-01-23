#!/usr/bin/env python3
"""
Simple FAT32 file copier - no external dependencies
"""
import struct
import sys
import os

def copy_file_to_fat(img_path, src_file, dest_name):
    """Copy a file to FAT32 image root directory"""
    with open(img_path, 'r+b') as img:
        # Read boot sector
        img.seek(0)
        boot_sector = img.read(512)

        # Parse FAT32 boot sector
        bytes_per_sector = struct.unpack('<H', boot_sector[11:13])[0]
        sectors_per_cluster = boot_sector[13]
        reserved_sectors = struct.unpack('<H', boot_sector[14:16])[0]
        num_fats = boot_sector[16]
        sectors_per_fat = struct.unpack('<I', boot_sector[36:40])[0]
        root_cluster = struct.unpack('<I', boot_sector[44:48])[0]

        print(f"FAT32 Info: {bytes_per_sector} bytes/sector, {sectors_per_cluster} sectors/cluster")
        print(f"Reserved: {reserved_sectors}, FATs: {num_fats}, Sectors/FAT: {sectors_per_fat}")
        print(f"Root cluster: {root_cluster}")

        # Calculate data area start
        fat_start = reserved_sectors * bytes_per_sector
        data_start = fat_start + (num_fats * sectors_per_fat * bytes_per_sector)

        # Read root directory
        root_offset = data_start + ((root_cluster - 2) * sectors_per_cluster * bytes_per_sector)
        img.seek(root_offset)

        # Read source file
        with open(src_file, 'rb') as src:
            file_data = src.read()

        file_size = len(file_data)
        print(f"Copying {src_file} ({file_size} bytes) as {dest_name}")

        # Find free directory entry
        for i in range(16):  # Check first 16 entries
            entry_offset = root_offset + (i * 32)
            img.seek(entry_offset)
            first_byte = img.read(1)[0]

            if first_byte == 0x00 or first_byte == 0xE5:  # Free entry
                # Create 8.3 filename
                name_8_3 = dest_name.upper().ljust(11, ' ')[:11]

                # Find free cluster
                clusters_needed = (file_size + (sectors_per_cluster * bytes_per_sector) - 1) // (sectors_per_cluster * bytes_per_sector)
                start_cluster = 3  # Start from cluster 3 (2 is root)

                # Write directory entry
                img.seek(entry_offset)
                entry = bytearray(32)
                entry[0:11] = name_8_3.encode('ascii')
                entry[11] = 0x20  # Archive attribute
                entry[26:28] = struct.pack('<H', start_cluster & 0xFFFF)
                entry[20:22] = struct.pack('<H', (start_cluster >> 16) & 0xFFFF)
                entry[28:32] = struct.pack('<I', file_size)
                img.write(bytes(entry))

                # Write file data
                cluster_offset = data_start + ((start_cluster - 2) * sectors_per_cluster * bytes_per_sector)
                img.seek(cluster_offset)
                img.write(file_data)

                # Update FAT
                img.seek(fat_start + (start_cluster * 4))
                img.write(struct.pack('<I', 0x0FFFFFFF))  # End of chain

                print(f"✓ Copied successfully!")
                return True

        print("✗ No free directory entries")
        return False

if __name__ == "__main__":
    img_path = "/tmp/boot.img"
    base_path = "/mnt/c/Users/merkn/OneDrive/Dokumenter/Meray_vault/Meray/Projects/Folkering-OS/code/kernel"

    files = [
        (f"{base_path}/limine.conf", "LIMINE.CFG"),
    ]

    print("Copying files to boot.img...")
    print("=" * 50)

    for src, dest in files:
        if os.path.exists(src):
            copy_file_to_fat(img_path, src, dest)
        else:
            print(f"✗ Source file not found: {src}")

    print("=" * 50)
    print("Done!")
