"""Pure Python FAT32 file injector — bypasses WSL/mtools entirely."""
import struct, sys, os

def fat32_inject(img_path, part_offset, dest_path, src_path):
    """Overwrite a file in a FAT32 partition with new content."""
    with open(src_path, 'rb') as f:
        new_data = f.read()

    with open(img_path, 'r+b') as f:
        # Read BPB
        f.seek(part_offset)
        bpb = f.read(512)
        bps = struct.unpack_from('<H', bpb, 11)[0]
        spc = bpb[13]
        reserved = struct.unpack_from('<H', bpb, 14)[0]
        num_fats = bpb[16]
        fat_size = struct.unpack_from('<I', bpb, 36)[0]
        root_cluster = struct.unpack_from('<I', bpb, 44)[0]

        cluster_size = bps * spc
        fat_start = part_offset + reserved * bps
        data_start = fat_start + num_fats * fat_size * bps

        def cluster_offset(c):
            return data_start + (c - 2) * cluster_size

        def read_fat(c):
            f.seek(fat_start + c * 4)
            return struct.unpack('<I', f.read(4))[0] & 0x0FFFFFFF

        def write_fat(c, val):
            for fat_num in range(num_fats):
                offset = fat_start + fat_num * fat_size * bps + c * 4
                f.seek(offset)
                f.write(struct.pack('<I', val & 0x0FFFFFFF))

        def get_cluster_chain(start):
            chain = []
            c = start
            while c >= 2 and c < 0x0FFFFFF8:
                chain.append(c)
                c = read_fat(c)
                if len(chain) > 100000:
                    break
            return chain

        def read_dir_cluster(c):
            f.seek(cluster_offset(c))
            return f.read(cluster_size)

        def find_entry(dir_cluster, name_parts):
            """Find a file/dir entry. name_parts = ['BOOT', 'KERNEL  ELF']"""
            target = name_parts[0]
            remaining = name_parts[1:]

            chain = get_cluster_chain(dir_cluster)
            for cc in chain:
                data = read_dir_cluster(cc)
                for i in range(0, len(data), 32):
                    entry = data[i:i+32]
                    if entry[0] == 0x00:  # end of directory
                        return None
                    if entry[0] == 0xE5:  # deleted
                        continue
                    if entry[11] == 0x0F:  # LFN entry
                        continue

                    name = entry[0:11].decode('ascii', errors='replace').rstrip()
                    # Compare 8.3 name
                    short_name = entry[0:8].decode('ascii', errors='replace').rstrip()
                    short_ext = entry[8:11].decode('ascii', errors='replace').rstrip()
                    full_83 = f"{short_name}.{short_ext}" if short_ext else short_name
                    full_83_nospace = f"{short_name}{short_ext}"

                    is_dir = (entry[11] & 0x10) != 0

                    entry_cluster = struct.unpack_from('<H', entry, 26)[0]
                    entry_cluster |= struct.unpack_from('<H', entry, 20)[0] << 16
                    entry_size = struct.unpack_from('<I', entry, 28)[0]

                    if short_name.upper() == target.upper() or full_83.upper() == target.upper():
                        if remaining:
                            if is_dir:
                                return find_entry(entry_cluster, remaining)
                        else:
                            return (cc, i, entry_cluster, entry_size, entry)
            return None

        # Parse destination path into 8.3 components
        parts = dest_path.strip('/').split('/')
        fat_parts = []
        for p in parts:
            if '.' in p:
                name, ext = p.rsplit('.', 1)
                fat_parts.append(name.upper()[:8])
            else:
                fat_parts.append(p.upper()[:8])

        # For the search, use the original names
        result = find_entry(root_cluster, [p.upper() for p in parts])
        if result is None:
            print(f"ERROR: '{dest_path}' not found in FAT32")
            return False

        dir_cluster, dir_offset, file_start_cluster, old_size, entry = result

        # Get existing cluster chain
        chain = get_cluster_chain(file_start_cluster)
        old_clusters = len(chain)
        new_clusters_needed = (len(new_data) + cluster_size - 1) // cluster_size
        if new_clusters_needed == 0:
            new_clusters_needed = 1

        # Allocate more clusters if needed
        if new_clusters_needed > old_clusters:
            # Find free clusters
            extra_needed = new_clusters_needed - old_clusters
            free_clusters = []
            search_start = 2
            while len(free_clusters) < extra_needed:
                fat_val = read_fat(search_start)
                if fat_val == 0:
                    free_clusters.append(search_start)
                search_start += 1
                if search_start > 0x0FFFFFF0:
                    print("ERROR: No free clusters!")
                    return False

            # Link new clusters to chain
            if chain:
                write_fat(chain[-1], free_clusters[0])
            for i, c in enumerate(free_clusters):
                if i + 1 < len(free_clusters):
                    write_fat(c, free_clusters[i + 1])
                else:
                    write_fat(c, 0x0FFFFFF8)  # end of chain
            chain.extend(free_clusters)

        # Free excess clusters if new file is smaller
        elif new_clusters_needed < old_clusters:
            # Mark end of new chain
            write_fat(chain[new_clusters_needed - 1], 0x0FFFFFF8)
            # Free remaining clusters
            for c in chain[new_clusters_needed:]:
                write_fat(c, 0)
            chain = chain[:new_clusters_needed]

        # Write new file data
        for i, c in enumerate(chain):
            offset = i * cluster_size
            chunk = new_data[offset:offset + cluster_size]
            if len(chunk) < cluster_size:
                chunk = chunk + b'\x00' * (cluster_size - len(chunk))
            f.seek(cluster_offset(c))
            f.write(chunk)

        # Update directory entry size
        new_entry = bytearray(entry)
        struct.pack_into('<I', new_entry, 28, len(new_data))
        f.seek(cluster_offset(dir_cluster) + dir_offset)
        f.write(bytes(new_entry))

        print(f"OK: '{dest_path}' updated ({old_size} -> {len(new_data)} bytes, {len(chain)} clusters)")
        return True

if __name__ == '__main__':
    IMG = r'C:\Users\merkn\folkering\folkering-os\boot\current.img'
    PART = 1048576
    BOOT = r'C:\Users\merkn\folkering\folkering-os\boot\iso_root\boot'

    fat32_inject(IMG, PART, '/boot/kernel.elf', f'{BOOT}\\kernel.elf')
    fat32_inject(IMG, PART, '/boot/initrd.fpk', f'{BOOT}\\initrd.fpk')
    fat32_inject(IMG, PART, '/initrd.fpk', f'{BOOT}\\initrd.fpk')
