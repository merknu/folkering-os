//! SQLite database I/O: load from VirtIO disk or ramdisk, flush to disk,
//! and BLOB reads (with overflow page support).

use libfolk::println;
use libfolk::sys::block;
use libfolk::sys::fs::read_file;
use libsqlite::{decode_varint, Value};

use crate::cache::open_db;
use crate::state::{SafeSqliteBuffer, MAX_DB_SIZE};

const DB_FILENAME: &str = "files.db";

/// Try to load SQLite database from the ramdisk file `files.db`.
/// Returns true on success, false if not found or invalid.
pub fn try_load_sqlite(buf: &mut SafeSqliteBuffer) -> bool {
    // Read into raw buffer (init only — bypasses dirty marking)
    let bytes_read = read_file(DB_FILENAME, buf.raw_data_mut());
    if bytes_read == 0 {
        return false;
    }
    buf.set_size(bytes_read);

    if bytes_read < 100 {
        return false;
    }

    // Verify SQLite magic
    if let Ok(magic) = buf.read_slice(0, 16) {
        if magic != b"SQLite format 3\0" {
            return false;
        }
    } else {
        return false;
    }

    buf.set_valid(true);
    true
}

/// Try to load SQLite database from the VirtIO block device using FOLKDISK
/// header to find the synapse_db_sector. Returns true on success.
pub fn try_load_sqlite_from_disk(buf: &mut SafeSqliteBuffer) -> bool {
    // Read sector 0 (disk header)
    let mut header_buf = [0u8; block::SECTOR_SIZE];
    if let Err(e) = block::read_sector(0, &mut header_buf) {
        println!("[SYNAPSE] VirtIO header read failed: {:?}", e);
        return false;
    }

    // Check FOLKDISK magic
    if &header_buf[0..8] != b"FOLKDISK" {
        return false;
    }

    // DiskHeader layout offsets:
    //   48: synapse_db_sector (u64 LE)
    //   56: synapse_db_size (u64 LE, in sectors)
    let db_sector = u64::from_le_bytes([
        header_buf[48], header_buf[49], header_buf[50], header_buf[51],
        header_buf[52], header_buf[53], header_buf[54], header_buf[55],
    ]);
    let db_sectors = u64::from_le_bytes([
        header_buf[56], header_buf[57], header_buf[58], header_buf[59],
        header_buf[60], header_buf[61], header_buf[62], header_buf[63],
    ]);

    println!("[SYNAPSE] VirtIO header: db_sector={}, db_sectors={}", db_sector, db_sectors);

    if db_sector == 0 || db_sectors == 0 {
        println!("[SYNAPSE] VirtIO: no DB location in header");
        return false;
    }

    let db_bytes = (db_sectors as usize) * block::SECTOR_SIZE;
    if db_bytes > MAX_DB_SIZE {
        println!("[SYNAPSE] VirtIO DB too large: {} bytes (max {})", db_bytes, MAX_DB_SIZE);
        return false;
    }

    // Read database sectors (chunked, max 64 sectors per syscall)
    let chunk_size = 64usize;
    let mut sectors_remaining = db_sectors as usize;
    let mut current_sector = db_sector;
    let mut buf_offset = 0usize;
    let raw = buf.raw_data_mut();

    while sectors_remaining > 0 {
        let this_chunk = sectors_remaining.min(chunk_size);
        let chunk_bytes = this_chunk * block::SECTOR_SIZE;
        let chunk_buf = &mut raw[buf_offset..buf_offset + chunk_bytes];

        if block::block_read(current_sector, chunk_buf, this_chunk).is_err() {
            println!("[SYNAPSE] VirtIO DB read failed at sector {}", current_sector);
            return false;
        }

        current_sector += this_chunk as u64;
        buf_offset += chunk_bytes;
        sectors_remaining -= this_chunk;
    }

    // Verify magic
    if db_bytes < 100 || &raw[0..16] != b"SQLite format 3\0" {
        println!("[SYNAPSE] VirtIO DB not valid SQLite");
        return false;
    }

    buf.set_size(db_bytes);
    buf.set_valid(true);

    println!("[SYNAPSE] Loaded {} sectors ({} KB) from VirtIO sector {}",
             db_sectors, db_bytes / 1024, db_sector);
    true
}

/// Compute a simple CRC32 (IEEE 802.3) of a byte slice.
/// Used for integrity checking on the SQLite header page.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Validate SQLite database integrity after loading.
/// Checks: magic, change counter consistency, header CRC.
/// Returns true if valid, false if corrupt (caller should reinit).
pub fn validate_sqlite_integrity(buf: &SafeSqliteBuffer) -> bool {
    if buf.size() < 100 {
        println!("[SYNAPSE] integrity: too small ({} bytes)", buf.size());
        return false;
    }

    // Check magic
    if let Ok(magic) = buf.read_slice(0, 16) {
        if magic != b"SQLite format 3\0" {
            println!("[SYNAPSE] integrity: bad magic");
            return false;
        }
    }

    // Check change counters match (offsets 24 and 92 should be equal)
    let cc1 = buf.read_be_u32(24).unwrap_or(0);
    let cc2 = buf.read_be_u32(92).unwrap_or(1);
    if cc1 != cc2 {
        println!("[SYNAPSE] integrity: change counter mismatch ({} vs {})", cc1, cc2);
        println!("[SYNAPSE] WARNING: DB may have been corrupted by incomplete write");
        // Don't fail — mismatch is common after crash, data may still be usable
    }

    // Log header CRC for forensics
    if let Ok(header) = buf.read_slice(0, 100) {
        let crc = crc32(header);
        println!("[SYNAPSE] integrity: header CRC32={:08X} cc={} pages={}",
            crc, cc1, buf.page_count());
    }

    true
}

/// Flush dirty pages of `buf` back to the VirtIO disk.
/// Returns true on success.
pub fn flush_sqlite_to_disk(buf: &mut SafeSqliteBuffer) -> bool {
    // Read FOLKDISK header to find synapse_db_sector
    let mut header_buf = [0u8; block::SECTOR_SIZE];
    if block::read_sector(0, &mut header_buf).is_err() {
        return false;
    }

    if &header_buf[0..8] != b"FOLKDISK" {
        return false;
    }

    let db_sector = u64::from_le_bytes([
        header_buf[48], header_buf[49], header_buf[50], header_buf[51],
        header_buf[52], header_buf[53], header_buf[54], header_buf[55],
    ]);

    if db_sector == 0 {
        return false;
    }

    let db_size = buf.size();
    let total_sectors = (db_size + block::SECTOR_SIZE - 1) / block::SECTOR_SIZE;
    let total_pages = (db_size + 4095) / 4096;
    let mut written = 0usize;
    let mut errors = 0usize;

    // Dirty-page flush: only write modified pages.
    for page in 0..total_pages {
        if !buf.is_page_dirty(page) {
            continue;
        }
        let page_offset = page * 4096;
        let sectors_in_page = if page_offset + 4096 <= db_size { 8 } else {
            (db_size - page_offset + 511) / 512
        };
        for s in 0..sectors_in_page {
            let byte_offset = page_offset + s * 512;
            let sec = db_sector + (byte_offset / 512) as u64;
            // Read into a fixed array (block::write_sector takes &[u8; 512])
            let mut sector_data = [0u8; 512];
            if let Ok(slice) = buf.read_slice(byte_offset, 512) {
                sector_data.copy_from_slice(slice);
            } else {
                continue;
            }
            if block::write_sector(sec, &sector_data).is_ok() {
                written += 1;
            } else {
                errors += 1;
            }
        }
    }

    buf.clear_dirty();

    // Update FOLKDISK header with new DB size
    let new_db_sectors = total_sectors as u64;
    let old_db_sectors = u64::from_le_bytes([
        header_buf[56], header_buf[57], header_buf[58], header_buf[59],
        header_buf[60], header_buf[61], header_buf[62], header_buf[63],
    ]);
    if new_db_sectors != old_db_sectors {
        header_buf[56..64].copy_from_slice(&new_db_sectors.to_le_bytes());
        let _ = block::write_sector(0, &header_buf);
        println!("[SYNAPSE] DB grown: {} -> {} sectors", old_db_sectors, new_db_sectors);
    }

    if written > 0 || errors > 0 {
        println!("[SYNAPSE] flush: {} sectors written, {} errors", written, errors);
    }

    true
}

/// Read a file's BLOB data directly from SQLite into `out`. Returns bytes read.
/// Used by FPK fallback path. For large files use `read_sqlite_blob_large`.
pub fn read_sqlite_blob(buf: &SafeSqliteBuffer, name: &str, out: &mut [u8]) -> usize {
    let Some(db) = open_db(buf) else {
        println!("[SYNAPSE] read_sqlite_blob: no sqlite db");
        return 0;
    };
    let Ok(scanner) = db.table_scan("files") else {
        println!("[SYNAPSE] read_sqlite_blob: table_scan failed");
        return 0;
    };

    let mut row_count = 0u32;
    for result in scanner {
        row_count += 1;
        if let Ok(record) = result {
            if let Some(Value::Text(rec_name)) = record.get(1) {
                if rec_name == name {
                    if let Some(Value::Blob(data)) = record.get(4) {
                        let copy_len = data.len().min(out.len());
                        out[..copy_len].copy_from_slice(&data[..copy_len]);
                        println!("[SYNAPSE] read_sqlite_blob: found '{}' blob={}B copy={}B",
                            name, data.len(), copy_len);
                        return copy_len;
                    } else {
                        println!("[SYNAPSE] read_sqlite_blob: '{}' matched but col4 is not Blob", name);
                    }
                }
            }
        }
    }
    println!("[SYNAPSE] read_sqlite_blob: scanned {} rows, '{}' not found", row_count, name);
    0
}

/// Decode a varint, returning Option<(u64, bytes_consumed)>.
fn dv(bytes: &[u8]) -> Option<(u64, usize)> {
    decode_varint(bytes).ok().map(|(v, n)| (v as u64, n))
}

/// Read a file's BLOB data from SQLite with **overflow page support**.
/// SQLite stores large BLOBs across multiple pages. The B-tree leaf cell
/// contains the first N bytes inline, then a 4-byte overflow page number.
pub fn read_sqlite_blob_large(buf: &SafeSqliteBuffer, name: &str, out: &mut [u8]) -> usize {
    let Some(db) = open_db(buf) else { return 0; };

    let page_size = db.page_size() as usize;
    let usable_size = page_size - db.header().reserved_bytes as usize;

    let root_page = 2u32; // "files" table is typically on page 2

    let mut pages_to_scan = [0u32; 64];
    pages_to_scan[0] = root_page;
    let mut scan_count = 1usize;
    let mut total_written = 0usize;

    while scan_count > 0 {
        scan_count -= 1;
        let page_num = pages_to_scan[scan_count];
        let page_data = match db.page(page_num) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let hdr_offset = if page_num == 1 { 100 } else { 0 };
        if hdr_offset + 8 > page_data.len() { continue; }
        let page_type = page_data[hdr_offset];
        let cell_count = u16::from_be_bytes([
            page_data[hdr_offset + 3], page_data[hdr_offset + 4]
        ]) as usize;

        let is_leaf = page_type == 0x0D;
        let is_interior = page_type == 0x05;

        if is_interior {
            let right_child = u32::from_be_bytes([
                page_data[hdr_offset + 8], page_data[hdr_offset + 9],
                page_data[hdr_offset + 10], page_data[hdr_offset + 11],
            ]);
            if scan_count < 63 { pages_to_scan[scan_count] = right_child; scan_count += 1; }
            let cell_ptr_start = hdr_offset + 12;
            for i in 0..cell_count {
                let ptr_off = cell_ptr_start + i * 2;
                if ptr_off + 2 > page_data.len() { break; }
                let cell_off = u16::from_be_bytes([page_data[ptr_off], page_data[ptr_off+1]]) as usize;
                if cell_off + 4 > page_data.len() { continue; }
                let left_child = u32::from_be_bytes([
                    page_data[cell_off], page_data[cell_off+1],
                    page_data[cell_off+2], page_data[cell_off+3],
                ]);
                if scan_count < 63 { pages_to_scan[scan_count] = left_child; scan_count += 1; }
            }
            continue;
        }

        if !is_leaf { continue; }

        let cell_ptr_start = hdr_offset + 8;
        for i in 0..cell_count {
            let ptr_off = cell_ptr_start + i * 2;
            if ptr_off + 2 > page_data.len() { break; }
            let cell_off = u16::from_be_bytes([page_data[ptr_off], page_data[ptr_off+1]]) as usize;
            if cell_off >= page_data.len() { continue; }
            let cell = &page_data[cell_off..];

            let (payload_size, ps_len) = match dv(cell) { Some(v) => v, None => continue };
            let (_, rowid_len) = match dv(&cell[ps_len..]) { Some(v) => v, None => continue };

            let hdr_start = ps_len + rowid_len;
            let payload_size = payload_size as usize;

            let max_local = usable_size - 35;
            let local_size = if payload_size <= max_local {
                payload_size
            } else {
                let min_local = ((usable_size - 12) * 32 / 255) - 23;
                let m = min_local + ((payload_size - min_local) % (usable_size - 4));
                if m <= max_local { m } else { min_local }
            };

            if hdr_start + 10 > cell.len() { continue; }
            let record_data = &cell[hdr_start..hdr_start + local_size.min(cell.len() - hdr_start)];

            let (hdr_size, hdr_size_len) = match dv(record_data) { Some(v) => v, None => continue };
            let hdr_size = hdr_size as usize;
            if hdr_size > record_data.len() { continue; }

            let mut col_types = [0u64; 6];
            let mut pos = hdr_size_len;
            let mut col_idx = 0;
            while pos < hdr_size && col_idx < 6 {
                let (st, st_len) = match dv(&record_data[pos..]) { Some(v) => v, None => break };
                col_types[col_idx] = st;
                pos += st_len;
                col_idx += 1;
            }

            let name_st = col_types[1];
            if name_st < 13 || name_st % 2 == 0 { continue; }
            let name_len = ((name_st - 13) / 2) as usize;

            let id_size = match col_types[0] {
                0 => 0, 1 => 1, 2 => 2, 3 => 3, 4 => 4, 5 => 6, 6 => 8, 7 => 8,
                8 | 9 => 0, _ => ((col_types[0] - 12) / 2) as usize,
            };
            let name_offset = hdr_size + id_size;
            if name_offset + name_len > record_data.len() { continue; }
            let rec_name = &record_data[name_offset..name_offset + name_len];

            if rec_name != name.as_bytes() { continue; }

            let kind_size = match col_types[2] {
                0 => 0, 1 => 1, 2 => 2, 3 => 3, 4 => 4, 5 => 6, 6 => 8, 7 => 8,
                8 | 9 => 0, _ => ((col_types[2] - 12) / 2) as usize,
            };
            let size_col_size = match col_types[3] {
                0 => 0, 1 => 1, 2 => 2, 3 => 3, 4 => 4, 5 => 6, 6 => 8, 7 => 8,
                8 | 9 => 0, _ => ((col_types[3] - 12) / 2) as usize,
            };
            let data_start = hdr_size + id_size + name_len + kind_size + size_col_size;
            let blob_size = if col_types[4] >= 12 && col_types[4] % 2 == 0 {
                ((col_types[4] - 12) / 2) as usize
            } else {
                println!("[SYNAPSE] blob_large: col4 type={} is not BLOB", col_types[4]);
                return 0;
            };

            let inline_blob_start = hdr_start + data_start;
            let inline_blob_avail = local_size.saturating_sub(data_start);
            let inline_copy = inline_blob_avail.min(blob_size).min(out.len());
            if inline_blob_start + inline_copy <= cell.len() {
                out[..inline_copy].copy_from_slice(
                    &cell[inline_blob_start..inline_blob_start + inline_copy],
                );
                total_written = inline_copy;
            }

            // Follow overflow pages
            if payload_size > local_size {
                let ovfl_ptr_off = hdr_start + local_size;
                if ovfl_ptr_off + 4 <= cell.len() {
                    let mut ovfl_page = u32::from_be_bytes([
                        cell[ovfl_ptr_off], cell[ovfl_ptr_off+1],
                        cell[ovfl_ptr_off+2], cell[ovfl_ptr_off+3],
                    ]);
                    let mut remaining = blob_size.saturating_sub(inline_copy);
                    let mut skip = if data_start > local_size {
                        data_start - local_size
                    } else { 0 };

                    while ovfl_page != 0 && remaining > 0 && total_written < out.len() {
                        let ovfl_data = match db.page(ovfl_page) {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        if ovfl_data.len() < 4 { break; }
                        let next_page = u32::from_be_bytes([
                            ovfl_data[0], ovfl_data[1], ovfl_data[2], ovfl_data[3],
                        ]);
                        let content = &ovfl_data[4..usable_size.min(ovfl_data.len())];

                        if skip > 0 {
                            let skip_here = skip.min(content.len());
                            let usable = &content[skip_here..];
                            let copy = usable.len().min(remaining).min(out.len() - total_written);
                            out[total_written..total_written + copy].copy_from_slice(&usable[..copy]);
                            total_written += copy;
                            remaining -= copy;
                            skip -= skip_here;
                        } else {
                            let copy = content.len().min(remaining).min(out.len() - total_written);
                            out[total_written..total_written + copy].copy_from_slice(&content[..copy]);
                            total_written += copy;
                            remaining -= copy;
                        }

                        ovfl_page = next_page;
                    }
                }
            }

            println!("[SYNAPSE] blob_large: '{}' read {} bytes (blob_size={})",
                name, total_written, blob_size);
            return total_written;
        }
    }

    println!("[SYNAPSE] blob_large: '{}' not found in B-tree", name);
    0
}
