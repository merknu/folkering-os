#!/usr/bin/env python3
"""Patch the virtio-data.img SQLite database to add the file_intents table.

This script:
1. Extracts the SQLite DB from virtio-data.img (sector 2048)
2. Adds CREATE TABLE file_intents if missing
3. Writes the modified DB back to the same location
4. Updates the FOLKDISK header if needed

Safe to run multiple times (idempotent).
"""
import sqlite3
import struct
import sys
import os
import shutil
import tempfile

VIRTIO_IMG = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                          "boot", "virtio-data.img")
DB_SECTOR = 2048
SECTOR_SIZE = 512

def main():
    if not os.path.exists(VIRTIO_IMG):
        print(f"Error: {VIRTIO_IMG} not found")
        return 1

    print(f"Patching: {VIRTIO_IMG}")
    print(f"DB location: sector {DB_SECTOR} (offset {DB_SECTOR * SECTOR_SIZE})")

    # Step 1: Extract the SQLite DB
    with open(VIRTIO_IMG, "rb") as f:
        f.seek(DB_SECTOR * SECTOR_SIZE)
        # Read first page to get DB size
        first_page = f.read(4096)
        if first_page[:16] != b"SQLite format 3\x00":
            print("Error: No SQLite database at expected sector")
            return 1

        page_size = struct.unpack_from(">H", first_page, 16)[0]
        page_count = struct.unpack_from(">I", first_page, 28)[0]
        db_size = page_size * page_count
        print(f"Existing DB: {page_count} pages x {page_size} bytes = {db_size} bytes ({db_size // 1024}KB)")

        # Read full DB
        f.seek(DB_SECTOR * SECTOR_SIZE)
        db_data = f.read(db_size)

    # Step 2: Write to temp file and open with sqlite3
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.write(db_data)
    tmp.close()

    try:
        conn = sqlite3.connect(tmp.name)
        cursor = conn.cursor()

        # Check if file_intents already exists
        cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='file_intents'")
        if cursor.fetchone():
            print("file_intents table already exists! Nothing to do.")
            conn.close()
            os.unlink(tmp.name)
            return 0

        # Create the table
        print("Creating file_intents table...")
        cursor.execute("""
            CREATE TABLE file_intents (
                file_id INTEGER PRIMARY KEY,
                mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
                intent_json TEXT,
                schema_version INTEGER DEFAULT 1,
                FOREIGN KEY (file_id) REFERENCES files(id)
            )
        """)

        # Auto-populate with MIME types for existing files
        cursor.execute("SELECT id, name FROM files")
        files = cursor.fetchall()
        for fid, name in files:
            mime = "application/x-elf"  # Default for boot services
            if name and name.endswith(".wasm"):
                mime = "application/wasm"
            elif name and name.endswith(".db"):
                mime = "application/x-sqlite3"
            cursor.execute(
                "INSERT INTO file_intents (file_id, mime_type, intent_json, schema_version) VALUES (?, ?, ?, 1)",
                (fid, mime, f'{{"purpose":"{name}","type":"system_service"}}')
            )
            print(f"  Tagged: {name} -> {mime}")

        conn.commit()

        # Verify
        cursor.execute("SELECT COUNT(*) FROM file_intents")
        count = cursor.fetchone()[0]
        print(f"Created file_intents with {count} entries")

        conn.close()

        # Step 3: Read back modified DB
        with open(tmp.name, "rb") as f:
            new_db = f.read()

        new_size = len(new_db)
        print(f"New DB size: {new_size} bytes ({new_size // 1024}KB)")

        if new_size > db_size:
            # DB grew — need to check if there's room
            max_db_size = 2 * 1024 * 1024  # 2MB max
            if new_size > max_db_size:
                print(f"Error: New DB too large ({new_size} > {max_db_size})")
                return 1
            print(f"DB grew from {db_size} to {new_size} bytes")

        # Step 4: Write back to virtio-data.img
        # Backup first
        backup = VIRTIO_IMG + ".bak"
        if not os.path.exists(backup):
            print(f"Creating backup: {backup}")
            shutil.copy2(VIRTIO_IMG, backup)

        with open(VIRTIO_IMG, "r+b") as f:
            f.seek(DB_SECTOR * SECTOR_SIZE)
            f.write(new_db)
            # Pad to sector boundary
            remainder = new_size % SECTOR_SIZE
            if remainder:
                f.write(b"\x00" * (SECTOR_SIZE - remainder))

        # Update FOLKDISK header with new DB size
        sector_count = (new_size + SECTOR_SIZE - 1) // SECTOR_SIZE
        with open(VIRTIO_IMG, "r+b") as f:
            # Read header
            header = bytearray(f.read(64))
            if header[:8] == b"FOLKDISK":
                # Update synapse_db_sector (offset 16) and synapse_db_size (offset 24)
                struct.pack_into("<Q", header, 16, DB_SECTOR)
                struct.pack_into("<Q", header, 24, new_size)
                f.seek(0)
                f.write(bytes(header))
                print(f"Updated FOLKDISK header: sector={DB_SECTOR}, size={new_size}")

        print(f"\nDone! file_intents table added to virtio-data.img")
        print(f"Restart QEMU to activate.")
        return 0

    finally:
        if os.path.exists(tmp.name):
            os.unlink(tmp.name)


if __name__ == "__main__":
    sys.exit(main())
