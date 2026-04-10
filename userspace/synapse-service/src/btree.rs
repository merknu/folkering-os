//! SQLite B-tree cell insertion (write path).
//!
//! Phase B2: All direct `unsafe { SQLITE_STATE.data[off] }` accesses have
//! been replaced with `SafeSqliteBuffer` methods (`read_byte`, `write_slice`,
//! `read_be_u32`, etc.) — automatic bounds checking + dirty marking.
//!
//! - `sqlite_insert_file` — append a row to the `files` table
//! - `sqlite_insert_intent` — append a row to the `file_intents` table
//! - `overwrite_blob_inplace` — replace an existing BLOB without growing
//!
//! Encoding helpers (`pick_integer_type`, `integer_type_size`,
//! `encode_integer_value`) live in this file because they're only used by
//! the insert path.

use libfolk::println;
use libsqlite::{encode_varint, SqliteDb, Value};

use crate::cache::open_db;
use crate::state::{SafeSqliteBuffer, MAX_DB_SIZE};

// ── Encoding helpers ───────────────────────────────────────────────────

/// Pick the smallest SQLite integer type code for a value.
pub fn pick_integer_type(val: u64) -> u64 {
    if val == 0 { return 8; }  // type 8 = integer constant 0
    if val == 1 { return 9; }  // type 9 = integer constant 1
    if val <= 0xFF { return 1; }
    if val <= 0xFFFF { return 2; }
    if val <= 0xFFFFFF { return 3; }
    if val <= 0xFFFFFFFF { return 4; }
    if val <= 0xFFFFFFFFFF { return 5; }
    6
}

/// Get byte count for an integer type code.
pub fn integer_type_size(type_code: u64) -> usize {
    match type_code {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 => 8,
        8 | 9 => 0,
        _ => 0,
    }
}

/// Encode an integer value in big-endian for the given type code.
pub fn encode_integer_value(val: u64, type_code: u64, buf: &mut [u8]) -> usize {
    let size = integer_type_size(type_code);
    match size {
        0 => 0,
        1 => { buf[0] = val as u8; 1 }
        2 => { buf[0..2].copy_from_slice(&(val as u16).to_be_bytes()); 2 }
        3 => {
            let bytes = (val as u32).to_be_bytes();
            buf[0..3].copy_from_slice(&bytes[1..4]);
            3
        }
        4 => { buf[0..4].copy_from_slice(&(val as u32).to_be_bytes()); 4 }
        6 => {
            let bytes = val.to_be_bytes();
            buf[0..6].copy_from_slice(&bytes[2..8]);
            6
        }
        8 => { buf[0..8].copy_from_slice(&val.to_be_bytes()); 8 }
        _ => 0,
    }
}

// ── B-tree traversal helpers ───────────────────────────────────────────

/// Walk a B-tree by following the rightmost interior pointers until we
/// reach a leaf page. Returns the leaf page number, or 0 on error.
fn find_rightmost_leaf(buf: &SafeSqliteBuffer, root_page: u32, page_size: usize) -> u32 {
    let mut current_page = root_page;
    let page_count = buf.page_count() as usize;

    for _ in 0..10 {
        if current_page == 0 || (current_page as usize) > page_count {
            println!("[SYNAPSE] btree: page {} out of range (max {})", current_page, page_count);
            return 0;
        }
        let page_off = (current_page as usize - 1) * page_size;
        let hdr_off = if current_page == 1 { 100 } else { 0 };

        let page_type = match buf.read_byte(page_off + hdr_off) {
            Ok(b) => b,
            Err(_) => {
                println!("[SYNAPSE] btree: page {} beyond db_size", current_page);
                return 0;
            }
        };

        if page_type == 0x0d {
            return current_page; // leaf
        } else if page_type == 0x05 {
            // Interior — follow right-pointer
            let rp_offset = page_off + hdr_off + 8;
            match buf.read_be_u32(rp_offset) {
                Ok(rp) => current_page = rp,
                Err(_) => return 0,
            }
        } else {
            println!("[SYNAPSE] btree: unexpected page type 0x{:02x} at page {}",
                     page_type, current_page);
            return 0;
        }
    }
    0
}

/// Update the right-pointer of the parent interior page that points to
/// the (now-full) old leaf, redirecting it to the new leaf.
/// Walks parent chain from root and marks each visited interior page dirty.
fn update_parent_right_pointer(
    buf: &mut SafeSqliteBuffer,
    root_page: u32,
    old_leaf: u32,
    new_leaf: u32,
    page_size: usize,
) {
    let mut parent_page = root_page;
    for _ in 0..10 {
        let pp_off = (parent_page as usize - 1) * page_size;
        let pp_hdr = if parent_page == 1 { 100 } else { 0 };
        let ptype = match buf.read_byte(pp_off + pp_hdr) {
            Ok(b) => b,
            Err(_) => return,
        };
        if ptype == 0x05 {
            let rp_off = pp_off + pp_hdr + 8;
            let right_child = match buf.read_be_u32(rp_off) {
                Ok(rp) => rp,
                Err(_) => return,
            };
            if right_child == old_leaf {
                let _ = buf.write_be_u32(rp_off, new_leaf);
                buf.mark_page_dirty(parent_page as usize - 1);
                println!("[SYNAPSE] Updated parent right-pointer to page {}", new_leaf);
                return;
            }
            buf.mark_page_dirty(parent_page as usize - 1);
            parent_page = right_child;
        } else {
            // Root is leaf — single-level tree, can't easily split
            println!("[SYNAPSE] Root is leaf — cannot split (need restructure)");
            return;
        }
    }
}

/// Allocate a new leaf page at `new_page_num`, write `cell_buf` into it,
/// and link it as the new rightmost leaf via parent right-pointer update.
/// Returns true on success.
fn allocate_new_leaf_page(
    buf: &mut SafeSqliteBuffer,
    root_page: u32,
    leaf_page: u32,
    cell_buf: &[u8],
    page_size: usize,
) -> bool {
    let page_count = buf.page_count() as usize;
    let new_page_num = page_count + 1;
    let new_page_offset = page_count * page_size;

    if new_page_offset + page_size > MAX_DB_SIZE {
        println!("[SYNAPSE] btree: DB buffer full ({} pages max)", MAX_DB_SIZE / page_size);
        return false;
    }

    let cell_len = cell_buf.len();
    let new_cell_start = page_size - cell_len;

    // Initialize new leaf page header (type 0x0d, cell_count=1)
    let np = new_page_offset;
    let _ = buf.write_byte(np, 0x0d);
    let _ = buf.write_byte(np + 1, 0);
    let _ = buf.write_byte(np + 2, 0);
    let _ = buf.write_be_u16(np + 3, 1); // cell_count = 1
    let _ = buf.write_be_u16(np + 5, new_cell_start as u16);
    let _ = buf.write_byte(np + 7, 0);

    // Write cell at end of page
    let _ = buf.write_slice(np + new_cell_start, cell_buf);

    // Write cell pointer at offset 8
    let _ = buf.write_be_u16(np + 8, new_cell_start as u16);

    // Update DB header: page_count + change counter
    buf.set_page_count(new_page_num as u32);
    buf.increment_change_counter();

    // Update parent interior page right-pointer
    update_parent_right_pointer(buf, root_page, leaf_page, new_page_num as u32, page_size);

    // Expand size to include new page
    buf.set_size(new_page_num * page_size);
    buf.mark_page_dirty(new_page_num - 1);

    println!("[SYNAPSE] New leaf page {} allocated", new_page_num);
    true
}

// ── Public insert API ──────────────────────────────────────────────────

/// M12: Overwrite an existing BLOB in-place. Returns false if file not found
/// or new data is too large for the existing BLOB slot.
pub fn overwrite_blob_inplace(buf: &mut SafeSqliteBuffer, name: &str, new_data: &[u8]) -> bool {
    if !buf.is_valid() {
        return false;
    }

    // First pass: locate the blob using a SqliteDb view
    // (must drop the borrow before mutating)
    let (offset, old_len) = {
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return false,
        };
        let scanner = match db.table_scan("files") {
            Ok(s) => s,
            Err(_) => return false,
        };

        let mut found: Option<(usize, usize)> = None;
        for result in scanner {
            if let Ok(record) = result {
                if let Some(Value::Text(rec_name)) = record.get(1) {
                    if rec_name == name {
                        if let Some(Value::Blob(old_blob)) = record.get(4) {
                            if new_data.len() > old_blob.len() {
                                return false; // too large for in-place
                            }
                            // Compute byte offset of the blob within the buffer
                            let base = buf.loaded().as_ptr() as usize;
                            let blob_ptr = old_blob.as_ptr() as usize;
                            let off = blob_ptr - base;
                            found = Some((off, old_blob.len()));
                            break;
                        }
                    }
                }
            }
        }
        match found {
            Some(t) => t,
            None => return false,
        }
    };

    // Second pass: perform the in-place overwrite via SafeSqliteBuffer
    if buf.write_slice(offset, new_data).is_err() {
        return false;
    }
    // Zero remaining bytes if new data is shorter
    if new_data.len() < old_len {
        for i in new_data.len()..old_len {
            let _ = buf.write_byte(offset + i, 0);
        }
    }
    buf.increment_change_counter();
    true
}

/// Insert a new row into the `files` table.
///
/// Cell format: `[payload_size: varint][rowid: varint][record]`
/// Record body columns: id (NULL placeholder), name (TEXT), kind (INT=1),
/// size (INT), data (BLOB), embedding (NULL).
pub fn sqlite_insert_file(
    buf: &mut SafeSqliteBuffer,
    rowid: i64,
    name: &str,
    content: &[u8],
) -> bool {
    if !buf.is_valid() {
        return false;
    }

    // Phase 1: Get table root + page_size from a temporary SqliteDb view
    let (root_page, page_size) = {
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return false,
        };
        let root = match db.find_table_root("files") {
            Ok(p) => p,
            Err(_) => return false,
        };
        (root, db.page_size() as usize)
    };

    // Phase 2: Walk B-tree to find rightmost leaf
    let leaf_page = find_rightmost_leaf(buf, root_page, page_size);
    if leaf_page == 0 {
        return false;
    }

    let page_offset = (leaf_page as usize - 1) * page_size;
    let header_offset = if leaf_page == 1 { 100 } else { 0 };
    let hdr = page_offset + header_offset;

    // Verify it's actually a leaf
    if buf.read_byte(hdr).unwrap_or(0) != 0x0d {
        println!("[SYNAPSE] insert: traversal failed, not a leaf");
        return false;
    }

    let cell_count = buf.read_be_u16(hdr + 3).unwrap_or(0) as usize;
    let cell_content_start = {
        let raw = buf.read_be_u16(hdr + 5).unwrap_or(0);
        if raw == 0 { page_size } else { raw as usize }
    };

    // Build the record body
    let name_bytes = name.as_bytes();
    let id_type: u64 = 0;
    let name_type = 13 + (name_bytes.len() as u64) * 2;
    let kind_type: u64 = 9; // constant 1
    let size_type = pick_integer_type(content.len() as u64);
    let data_type = 12 + (content.len() as u64) * 2;
    let embed_type: u64 = 0;

    let mut hdr_buf = [0u8; 32];
    let mut hdr_pos = 0usize;
    hdr_pos += 1; // header_size placeholder
    hdr_pos += encode_varint(id_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(name_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(kind_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(size_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(data_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(embed_type, &mut hdr_buf[hdr_pos..]);

    let header_size = hdr_pos;
    if header_size > 127 {
        println!("[SYNAPSE] insert: header too large");
        return false;
    }
    hdr_buf[0] = header_size as u8;

    let size_int_size = integer_type_size(size_type);
    let record_body_size = header_size + name_bytes.len() + size_int_size + content.len();

    // Build the full cell: [payload_size: varint][rowid: varint][record body]
    const MAX_CELL: usize = 16448;
    if record_body_size + 20 > MAX_CELL {
        println!("[SYNAPSE] insert: cell too large ({} bytes)", record_body_size);
        return false;
    }
    let mut cell_buf = [0u8; MAX_CELL];
    let mut cell_pos = 0usize;

    cell_pos += encode_varint(record_body_size as u64, &mut cell_buf[cell_pos..]);
    cell_pos += encode_varint(rowid as u64, &mut cell_buf[cell_pos..]);

    cell_buf[cell_pos..cell_pos + header_size].copy_from_slice(&hdr_buf[..header_size]);
    cell_pos += header_size;

    // name (TEXT)
    cell_buf[cell_pos..cell_pos + name_bytes.len()].copy_from_slice(name_bytes);
    cell_pos += name_bytes.len();

    // size (integer)
    let mut int_buf = [0u8; 8];
    let int_len = encode_integer_value(content.len() as u64, size_type, &mut int_buf);
    cell_buf[cell_pos..cell_pos + int_len].copy_from_slice(&int_buf[..int_len]);
    cell_pos += int_len;

    // data (BLOB)
    cell_buf[cell_pos..cell_pos + content.len()].copy_from_slice(content);
    cell_pos += content.len();

    let cell_len = cell_pos;

    // Check free space on the leaf page
    let pointer_array_end = header_offset + 8 + cell_count * 2;
    let free_space = cell_content_start - pointer_array_end;

    if cell_len + 2 > free_space {
        // Leaf full — allocate a new leaf page
        println!("[SYNAPSE] Leaf full (need {}, have {}) — allocating new page",
                 cell_len + 2, free_space);
        return allocate_new_leaf_page(buf, root_page, leaf_page, &cell_buf[..cell_len], page_size);
    }

    // Normal path: write into existing leaf page
    let new_cell_offset = cell_content_start - cell_len;
    let _ = buf.write_slice(page_offset + new_cell_offset, &cell_buf[..cell_len]);

    // Write cell pointer (BE u16) at end of pointer array
    let ptr_offset = page_offset + header_offset + 8 + cell_count * 2;
    let _ = buf.write_be_u16(ptr_offset, new_cell_offset as u16);

    // Update page header: cell_count += 1, cell_content_start = new_cell_offset
    let _ = buf.write_be_u16(hdr + 3, (cell_count + 1) as u16);
    let _ = buf.write_be_u16(hdr + 5, new_cell_offset as u16);

    buf.increment_change_counter();
    buf.mark_page_dirty(leaf_page as usize - 1);

    true
}

/// Insert an intent record into the `file_intents` B-tree.
/// Columns: file_id (rowid), mime_type (TEXT), intent_json (TEXT), version (INT=1).
pub fn sqlite_insert_intent(
    buf: &mut SafeSqliteBuffer,
    file_id: i64,
    mime_type: &str,
    intent_json: &str,
) -> bool {
    if !buf.is_valid() {
        return false;
    }

    let (root_page, page_size) = {
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return false,
        };
        let root = match db.find_table_root("file_intents") {
            Ok(p) => p,
            Err(_) => {
                println!("[SYNAPSE] insert_intent: file_intents table not found");
                return false;
            }
        };
        (root, db.page_size() as usize)
    };

    let leaf_page = find_rightmost_leaf(buf, root_page, page_size);
    if leaf_page == 0 {
        return false;
    }

    let page_offset = (leaf_page as usize - 1) * page_size;
    let header_offset = if leaf_page == 1 { 100 } else { 0 };
    let hdr = page_offset + header_offset;

    if buf.read_byte(hdr).unwrap_or(0) != 0x0d {
        return false;
    }

    let cell_count = buf.read_be_u16(hdr + 3).unwrap_or(0) as usize;
    let cell_content_start = {
        let raw = buf.read_be_u16(hdr + 5).unwrap_or(0);
        if raw == 0 { page_size } else { raw as usize }
    };

    let mime_bytes = mime_type.as_bytes();
    let json_bytes = intent_json.as_bytes();

    let id_type: u64 = 0;
    let mime_type_code = 13 + (mime_bytes.len() as u64) * 2;
    let json_type_code = if json_bytes.is_empty() { 0 } else { 13 + (json_bytes.len() as u64) * 2 };
    let version_type: u64 = 9; // constant 1

    let mut hdr_buf = [0u8; 16];
    let mut hdr_pos = 0usize;
    hdr_pos += 1;
    hdr_pos += encode_varint(id_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(mime_type_code, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(json_type_code, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(version_type, &mut hdr_buf[hdr_pos..]);

    let header_size = hdr_pos;
    if header_size > 127 { return false; }
    hdr_buf[0] = header_size as u8;

    let record_body_size = header_size + mime_bytes.len() + json_bytes.len();

    let mut cell_buf = [0u8; 2048];
    let mut cell_pos = 0usize;
    cell_pos += encode_varint(record_body_size as u64, &mut cell_buf[cell_pos..]);
    cell_pos += encode_varint(file_id as u64, &mut cell_buf[cell_pos..]);

    cell_buf[cell_pos..cell_pos + header_size].copy_from_slice(&hdr_buf[..header_size]);
    cell_pos += header_size;

    cell_buf[cell_pos..cell_pos + mime_bytes.len()].copy_from_slice(mime_bytes);
    cell_pos += mime_bytes.len();
    if !json_bytes.is_empty() {
        cell_buf[cell_pos..cell_pos + json_bytes.len()].copy_from_slice(json_bytes);
        cell_pos += json_bytes.len();
    }

    let cell_len = cell_pos;
    let pointer_array_end = header_offset + 8 + cell_count * 2;
    let free_space = cell_content_start - pointer_array_end;

    if cell_len + 2 > free_space {
        // Leaf full — allocate new page
        return allocate_new_leaf_page(buf, root_page, leaf_page, &cell_buf[..cell_len], page_size);
    }

    let new_cell_offset = cell_content_start - cell_len;
    let _ = buf.write_slice(page_offset + new_cell_offset, &cell_buf[..cell_len]);

    let ptr_offset = page_offset + header_offset + 8 + cell_count * 2;
    let _ = buf.write_be_u16(ptr_offset, new_cell_offset as u16);

    let _ = buf.write_be_u16(hdr + 3, (cell_count + 1) as u16);
    let _ = buf.write_be_u16(hdr + 5, new_cell_offset as u16);

    buf.increment_change_counter();
    buf.mark_page_dirty(leaf_page as usize - 1);

    true
}

/// Read intent for a file_id from `file_intents`. Returns `(mime_len, json_len)`
/// where the bytes are written into `out_buf` as `[mime][json]`.
pub fn sqlite_read_intent(buf: &SafeSqliteBuffer, file_id: i64, out_buf: &mut [u8]) -> Option<(usize, usize)> {
    let db = open_db(buf)?;
    let scanner = db.table_scan("file_intents").ok()?;

    for result in scanner {
        if let Ok(record) = result {
            if record.rowid == file_id {
                let mut pos = 0usize;

                let mime_len = if let Some(Value::Text(s)) = record.get(1) {
                    let b = s.as_bytes();
                    let n = b.len().min(out_buf.len());
                    out_buf[..n].copy_from_slice(&b[..n]);
                    pos = n;
                    n
                } else { 0 };

                let json_len = if let Some(Value::Text(s)) = record.get(2) {
                    let b = s.as_bytes();
                    let n = b.len().min(out_buf.len() - pos);
                    out_buf[pos..pos + n].copy_from_slice(&b[..n]);
                    n
                } else { 0 };

                return Some((mime_len, json_len));
            }
        }
    }
    None
}
