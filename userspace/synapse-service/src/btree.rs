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

extern crate alloc;
use alloc::vec::Vec;

use libfolk::println;
use libsqlite::{encode_varint, SqliteDb, Value};

use crate::cache::open_db;
use crate::state::{SafeSqliteBuffer, MAX_DB_SIZE};

// ── SQLite overflow-page spill formula ─────────────────────────────────
//
// Phase 8: implement SQLite overflow pages so a single row can hold
// arbitrarily large BLOBs without exceeding a B-tree leaf page.
//
// For payloads larger than `maxLocal`, SQLite keeps `local_size` bytes
// inline in the cell and spills the rest into a linked list of
// overflow pages. The cell is then:
//
//     [payload_size varint]  (TOTAL payload length incl. overflow)
//     [rowid varint]
//     [local payload bytes]  (exactly `local_size` bytes)
//     [4-byte BE first overflow page number]
//
// Each overflow page is laid out as:
//
//     bytes 0..4        big-endian u32 next_page_number (0 = last)
//     bytes 4..usable   payload bytes (usable_size - 4 per page)
//
// The spill formula must match the one in `sqlite_io::read_sqlite_blob_large`
// exactly, or the reader will compute a different `local_size` and
// read garbage from the cell.

/// SQLite overflow spill — returns (local_bytes, overflow_bytes) for a
/// given total payload size and usable page size.
///
/// Formula (from SQLite's btree.c):
///
/// ```text
/// U = usable_size
/// X = U - 35               (max local payload per cell)
/// M = ((U - 12) * 32 / 255) - 23  (min local — bounds the worst case)
///
/// if payload <= X: local = payload
/// else:
///   n = M + ((payload - M) % (U - 4))
///   local = n if n <= X else M
/// ```
fn compute_overflow_split(payload_size: usize, usable_size: usize) -> (usize, usize) {
    let max_local = usable_size.saturating_sub(35);
    if payload_size <= max_local {
        return (payload_size, 0);
    }
    let min_local = (usable_size.saturating_sub(12) * 32 / 255).saturating_sub(23);
    let ring = usable_size.saturating_sub(4);
    let excess = payload_size.saturating_sub(min_local);
    let n = min_local + (excess % ring);
    let local = if n <= max_local { n } else { min_local };
    let overflow = payload_size - local;
    (local, overflow)
}

/// Allocate a chain of overflow pages holding `data`. Each page stores
/// `(usable_size - 4)` bytes of payload plus a 4-byte big-endian next
/// pointer at the front.
///
/// Returns the first page number of the chain, or None if the DB
/// buffer can't grow enough to hold the required pages.
///
/// `page_size` is the full on-disk page stride; `usable_size` is the
/// byte window SQLite treats as addressable (`page_size - reserved`).
/// Reserved bytes live at the TAIL of each page, so the next-page
/// pointer and overflow payload both fit in `[0, usable_size)` while
/// page offsets advance by `page_size`.
fn allocate_overflow_chain(
    buf: &mut SafeSqliteBuffer,
    data: &[u8],
    page_size: usize,
    usable_size: usize,
) -> Option<u32> {
    if data.is_empty() {
        return None;
    }
    let chunk_size = usable_size.saturating_sub(4);
    if chunk_size == 0 {
        return None;
    }
    let pages_needed = data.len().div_ceil(chunk_size);
    let start_page = (buf.page_count() as usize) + 1;
    let end_page = start_page + pages_needed - 1;
    let required_bytes = end_page * page_size;
    if required_bytes > MAX_DB_SIZE {
        println!(
            "[SYNAPSE] overflow: DB buffer full (need {} bytes, max {})",
            required_bytes, MAX_DB_SIZE
        );
        return None;
    }

    let mut data_pos = 0usize;
    for i in 0..pages_needed {
        let page_num = start_page + i;
        let page_off = (page_num - 1) * page_size;
        let next_page: u32 = if i + 1 < pages_needed {
            (start_page + i + 1) as u32
        } else {
            0
        };
        // Write the next-page pointer (4-byte BE u32).
        if buf.write_be_u32(page_off, next_page).is_err() {
            return None;
        }
        // Write the chunk payload.
        let chunk_end = (data_pos + chunk_size).min(data.len());
        let chunk = &data[data_pos..chunk_end];
        if buf.write_slice(page_off + 4, chunk).is_err() {
            return None;
        }
        // Zero any trailing bytes in the usable region of the page so
        // stale disk content can't leak into the read path. The
        // reserved tail (usable_size..page_size) is left untouched.
        if chunk.len() < chunk_size {
            let zero_start = page_off + 4 + chunk.len();
            let zero_end = page_off + usable_size;
            for off in zero_start..zero_end {
                let _ = buf.write_byte(off, 0);
            }
        }
        data_pos = chunk_end;
        buf.mark_page_dirty(page_num - 1);
    }

    // Grow the header page count and the buffer's logical size.
    buf.set_page_count(end_page as u32);
    buf.set_size(end_page * page_size);
    buf.increment_change_counter();

    println!(
        "[SYNAPSE] overflow chain: {} page(s) {}..{} holding {} bytes",
        pages_needed, start_page, end_page, data.len()
    );
    Some(start_page as u32)
}

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
/// Cell format for inline payloads:
///     [payload_size: varint][rowid: varint][record body]
///
/// Cell format for payloads that exceed `maxLocal`:
///     [payload_size: varint][rowid: varint][first `local_size` bytes of record body][first overflow page: u32 BE]
///
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

    // Phase 1: Get table root + page_size + usable_size from a
    // temporary SqliteDb view.
    let (root_page, page_size, usable_size) = {
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return false,
        };
        let root = match db.find_table_root("files") {
            Ok(p) => p,
            Err(_) => return false,
        };
        let ps = db.page_size() as usize;
        let us = ps - db.header().reserved_bytes as usize;
        (root, ps, us)
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

    // Phase 3: Build the record body header (type codes)
    let name_bytes = name.as_bytes();
    let id_type: u64 = 0;
    let name_type = 13 + (name_bytes.len() as u64) * 2;
    let kind_type: u64 = 9; // constant 1
    let size_type = pick_integer_type(content.len() as u64);
    let data_type = 12 + (content.len() as u64) * 2;
    let embed_type: u64 = 0;

    let mut hdr_scratch = [0u8; 32];
    let mut hdr_pos = 0usize;
    hdr_pos += 1; // header_size placeholder
    hdr_pos += encode_varint(id_type, &mut hdr_scratch[hdr_pos..]);
    hdr_pos += encode_varint(name_type, &mut hdr_scratch[hdr_pos..]);
    hdr_pos += encode_varint(kind_type, &mut hdr_scratch[hdr_pos..]);
    hdr_pos += encode_varint(size_type, &mut hdr_scratch[hdr_pos..]);
    hdr_pos += encode_varint(data_type, &mut hdr_scratch[hdr_pos..]);
    hdr_pos += encode_varint(embed_type, &mut hdr_scratch[hdr_pos..]);

    let record_hdr_size = hdr_pos;
    if record_hdr_size > 127 {
        println!("[SYNAPSE] insert: header too large");
        return false;
    }
    hdr_scratch[0] = record_hdr_size as u8;

    let size_int_size = integer_type_size(size_type);
    let payload_total = record_hdr_size + name_bytes.len() + size_int_size + content.len();

    // Phase 4: Assemble the full payload in a Vec so we can hand any
    // tail portion off to the overflow-page allocator without sliding
    // around inside a fixed stack buffer.
    let mut payload: Vec<u8> = Vec::with_capacity(payload_total);
    payload.extend_from_slice(&hdr_scratch[..record_hdr_size]);
    payload.extend_from_slice(name_bytes);
    let mut int_buf = [0u8; 8];
    let int_len = encode_integer_value(content.len() as u64, size_type, &mut int_buf);
    payload.extend_from_slice(&int_buf[..int_len]);
    payload.extend_from_slice(content);
    debug_assert_eq!(payload.len(), payload_total);

    // Phase 5: Decide on the overflow split.
    let (local_size, overflow_size) = compute_overflow_split(payload_total, usable_size);

    // Phase 6: If the payload spills, allocate + write the overflow
    // chain BEFORE we touch the leaf page, so the leaf insert can
    // reference the chain head.
    let first_overflow_page: u32 = if overflow_size > 0 {
        match allocate_overflow_chain(buf, &payload[local_size..], page_size, usable_size) {
            Some(p) => p,
            None => {
                println!("[SYNAPSE] insert: overflow allocation failed");
                return false;
            }
        }
    } else {
        0
    };

    // Phase 7: Build the actual leaf cell. Its body is:
    //   [payload_size varint][rowid varint][local bytes][optional 4-byte BE overflow ptr]
    //
    // `payload_size` is the TOTAL payload length (inline + overflow),
    // not just the local portion. The SQLite reader recomputes
    // `local_size` from this total via the same spill formula.
    let mut cell_buf: Vec<u8> =
        Vec::with_capacity(local_size + 9 + 9 + 4);
    let mut varint_scratch = [0u8; 9];
    let ps_len = encode_varint(payload_total as u64, &mut varint_scratch);
    cell_buf.extend_from_slice(&varint_scratch[..ps_len]);
    let rowid_len = encode_varint(rowid as u64, &mut varint_scratch);
    cell_buf.extend_from_slice(&varint_scratch[..rowid_len]);
    cell_buf.extend_from_slice(&payload[..local_size]);
    if overflow_size > 0 {
        cell_buf.extend_from_slice(&first_overflow_page.to_be_bytes());
    }

    let cell_len = cell_buf.len();

    // Phase 8: Fit the cell into the leaf page or allocate a new
    // leaf. With overflow, `cell_len` is bounded well under
    // pageSize so the `allocate_new_leaf_page` path is safe (its
    // cell-into-4096-byte-page slot never underflows).
    let pointer_array_end = header_offset + 8 + cell_count * 2;
    let free_space = cell_content_start - pointer_array_end;

    if cell_len + 2 > free_space {
        println!(
            "[SYNAPSE] Leaf full (need {}, have {}) — allocating new leaf (payload={}, local={}, ovfl={})",
            cell_len + 2, free_space, payload_total, local_size, overflow_size
        );
        return allocate_new_leaf_page(buf, root_page, leaf_page, &cell_buf, page_size);
    }

    // Normal path: write into existing leaf page
    let new_cell_offset = cell_content_start - cell_len;
    let _ = buf.write_slice(page_offset + new_cell_offset, &cell_buf);

    // Write cell pointer (BE u16) at end of pointer array
    let ptr_offset = page_offset + header_offset + 8 + cell_count * 2;
    let _ = buf.write_be_u16(ptr_offset, new_cell_offset as u16);

    // Update page header: cell_count += 1, cell_content_start = new_cell_offset
    let _ = buf.write_be_u16(hdr + 3, (cell_count + 1) as u16);
    let _ = buf.write_be_u16(hdr + 5, new_cell_offset as u16);

    buf.increment_change_counter();
    buf.mark_page_dirty(leaf_page as usize - 1);

    if overflow_size > 0 {
        println!(
            "[SYNAPSE] insert_file: '{}' rowid={} payload={} local={} overflow={} first_ovfl_page={}",
            name, rowid, payload_total, local_size, overflow_size, first_overflow_page
        );
    }
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

// ════════════════════════════════════════════════════════════════════════
// Phase 9: Bi-Temporal Knowledge Graph operations
// ════════════════════════════════════════════════════════════════════════
//
// The `entities` and `edges` tables are created by folk-pack at pack
// time (real SQLite's DDL), but synapse-service's custom btree writer
// is responsible for INSERT, UPDATE (valid_to rewrite), and graph
// traversal. All inserts go through the standard rowid-btree append
// path shared with `sqlite_insert_file` / `sqlite_insert_intent`, so
// they automatically benefit from Phase 8 overflow pages for free.

/// Pick the next rowid for a table by scanning it once and returning
/// `max(rowid) + 1`. For a few hundred rows this is acceptable; a
/// future pass could cache the high-water mark in a state struct.
fn next_rowid_for(buf: &SafeSqliteBuffer, table: &str) -> i64 {
    let db = match open_db(buf) {
        Some(db) => db,
        None => return 1,
    };
    let scanner = match db.table_scan(table) {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let mut max_rowid: i64 = 0;
    for result in scanner {
        if let Ok(record) = result {
            if record.rowid > max_rowid {
                max_rowid = record.rowid;
            }
        }
    }
    max_rowid + 1
}

/// Insert a new row into the `entities` table.
///
/// Record columns: (rowid implicit), entity_id, name, entity_type,
/// properties (NULL), created_at (8-byte BE int).
pub fn sqlite_insert_entity(
    buf: &mut SafeSqliteBuffer,
    entity_id: &str,
    name: &str,
    entity_type: &str,
    created_at: i64,
) -> Option<i64> {
    if !buf.is_valid() { return None; }

    let (root_page, page_size) = {
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return None,
        };
        let root = match db.find_table_root("entities") {
            Ok(p) => p,
            Err(_) => {
                println!("[SYNAPSE] insert_entity: entities table not found");
                return None;
            }
        };
        (root, db.page_size() as usize)
    };

    let leaf_page = find_rightmost_leaf(buf, root_page, page_size);
    if leaf_page == 0 { return None; }

    let page_offset = (leaf_page as usize - 1) * page_size;
    let header_offset = if leaf_page == 1 { 100 } else { 0 };
    let hdr = page_offset + header_offset;

    if buf.read_byte(hdr).unwrap_or(0) != 0x0d { return None; }

    let cell_count = buf.read_be_u16(hdr + 3).unwrap_or(0) as usize;
    let cell_content_start = {
        let raw = buf.read_be_u16(hdr + 5).unwrap_or(0);
        if raw == 0 { page_size } else { raw as usize }
    };

    let rowid = next_rowid_for(buf, "entities");

    let eid_bytes = entity_id.as_bytes();
    let name_bytes = name.as_bytes();
    let type_bytes = entity_type.as_bytes();

    let eid_type = 13 + (eid_bytes.len() as u64) * 2;
    let name_type = 13 + (name_bytes.len() as u64) * 2;
    let entity_type_code = 13 + (type_bytes.len() as u64) * 2;
    let props_type: u64 = 0; // NULL
    let created_at_type: u64 = 6; // 8-byte BE int

    let mut hdr_buf = [0u8; 32];
    let mut hdr_pos = 0usize;
    hdr_pos += 1;
    hdr_pos += encode_varint(eid_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(name_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(entity_type_code, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(props_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(created_at_type, &mut hdr_buf[hdr_pos..]);
    let record_hdr_size = hdr_pos;
    if record_hdr_size > 127 { return None; }
    hdr_buf[0] = record_hdr_size as u8;

    let body_size = record_hdr_size + eid_bytes.len() + name_bytes.len() + type_bytes.len() + 8;

    let mut cell_buf: Vec<u8> = Vec::with_capacity(body_size + 20);
    let mut scratch = [0u8; 9];
    let ps_len = encode_varint(body_size as u64, &mut scratch);
    cell_buf.extend_from_slice(&scratch[..ps_len]);
    let rowid_len = encode_varint(rowid as u64, &mut scratch);
    cell_buf.extend_from_slice(&scratch[..rowid_len]);

    cell_buf.extend_from_slice(&hdr_buf[..record_hdr_size]);
    cell_buf.extend_from_slice(eid_bytes);
    cell_buf.extend_from_slice(name_bytes);
    cell_buf.extend_from_slice(type_bytes);
    cell_buf.extend_from_slice(&(created_at as u64).to_be_bytes());

    let cell_len = cell_buf.len();
    let pointer_array_end = header_offset + 8 + cell_count * 2;
    let free_space = cell_content_start.saturating_sub(pointer_array_end);

    if cell_len + 2 > free_space {
        if !allocate_new_leaf_page(buf, root_page, leaf_page, &cell_buf, page_size) {
            return None;
        }
        return Some(rowid);
    }

    let new_cell_offset = cell_content_start - cell_len;
    let _ = buf.write_slice(page_offset + new_cell_offset, &cell_buf);
    let ptr_offset = page_offset + header_offset + 8 + cell_count * 2;
    let _ = buf.write_be_u16(ptr_offset, new_cell_offset as u16);
    let _ = buf.write_be_u16(hdr + 3, (cell_count + 1) as u16);
    let _ = buf.write_be_u16(hdr + 5, new_cell_offset as u16);

    buf.increment_change_counter();
    buf.mark_page_dirty(leaf_page as usize - 1);

    println!("[SYNAPSE] insert_entity: rowid={} id={} name={}", rowid, entity_id, name);
    Some(rowid)
}

/// Insert a new row into the `edges` table.
///
/// Columns: edge_id(TEXT), subject_id(TEXT), predicate(TEXT),
/// object_id(TEXT), valid_from(INT 8B), valid_to(INT 8B, 0 = active),
/// confidence(REAL 8B), source_zid(NULL).
///
/// `valid_to` is stored as an 8-byte BE integer at a deterministic
/// offset inside the cell so the supersession path can rewrite it in
/// place via `sqlite_expire_edge` without reconstructing the cell.
pub fn sqlite_insert_edge(
    buf: &mut SafeSqliteBuffer,
    edge_id: &str,
    subject_id: &str,
    predicate: &str,
    object_id: &str,
    valid_from: i64,
    valid_to: i64,
    confidence: f64,
) -> Option<i64> {
    if !buf.is_valid() { return None; }

    let (root_page, page_size) = {
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return None,
        };
        let root = match db.find_table_root("edges") {
            Ok(p) => p,
            Err(_) => {
                println!("[SYNAPSE] insert_edge: edges table not found");
                return None;
            }
        };
        (root, db.page_size() as usize)
    };

    let leaf_page = find_rightmost_leaf(buf, root_page, page_size);
    if leaf_page == 0 { return None; }

    let page_offset = (leaf_page as usize - 1) * page_size;
    let header_offset = if leaf_page == 1 { 100 } else { 0 };
    let hdr = page_offset + header_offset;

    if buf.read_byte(hdr).unwrap_or(0) != 0x0d { return None; }

    let cell_count = buf.read_be_u16(hdr + 3).unwrap_or(0) as usize;
    let cell_content_start = {
        let raw = buf.read_be_u16(hdr + 5).unwrap_or(0);
        if raw == 0 { page_size } else { raw as usize }
    };

    let rowid = next_rowid_for(buf, "edges");

    let eid_bytes = edge_id.as_bytes();
    let subj_bytes = subject_id.as_bytes();
    let pred_bytes = predicate.as_bytes();
    let obj_bytes = object_id.as_bytes();

    let eid_type = 13 + (eid_bytes.len() as u64) * 2;
    let subj_type = 13 + (subj_bytes.len() as u64) * 2;
    let pred_type = 13 + (pred_bytes.len() as u64) * 2;
    let obj_type = 13 + (obj_bytes.len() as u64) * 2;
    let vf_type: u64 = 6;     // 8-byte BE int
    let vt_type: u64 = 6;     // 8-byte BE int
    let conf_type: u64 = 7;   // 8-byte IEEE float
    let src_zid_type: u64 = 0; // NULL

    let mut hdr_buf = [0u8; 32];
    let mut hdr_pos = 0usize;
    hdr_pos += 1;
    hdr_pos += encode_varint(eid_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(subj_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(pred_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(obj_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(vf_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(vt_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(conf_type, &mut hdr_buf[hdr_pos..]);
    hdr_pos += encode_varint(src_zid_type, &mut hdr_buf[hdr_pos..]);
    let record_hdr_size = hdr_pos;
    if record_hdr_size > 127 { return None; }
    hdr_buf[0] = record_hdr_size as u8;

    let body_size = record_hdr_size
        + eid_bytes.len() + subj_bytes.len() + pred_bytes.len() + obj_bytes.len()
        + 8 + 8 + 8;

    let mut cell_buf: Vec<u8> = Vec::with_capacity(body_size + 20);
    let mut scratch = [0u8; 9];
    let ps_len = encode_varint(body_size as u64, &mut scratch);
    cell_buf.extend_from_slice(&scratch[..ps_len]);
    let rowid_len = encode_varint(rowid as u64, &mut scratch);
    cell_buf.extend_from_slice(&scratch[..rowid_len]);

    cell_buf.extend_from_slice(&hdr_buf[..record_hdr_size]);
    cell_buf.extend_from_slice(eid_bytes);
    cell_buf.extend_from_slice(subj_bytes);
    cell_buf.extend_from_slice(pred_bytes);
    cell_buf.extend_from_slice(obj_bytes);
    cell_buf.extend_from_slice(&(valid_from as u64).to_be_bytes());
    cell_buf.extend_from_slice(&(valid_to as u64).to_be_bytes());
    cell_buf.extend_from_slice(&confidence.to_be_bytes());

    let cell_len = cell_buf.len();
    let pointer_array_end = header_offset + 8 + cell_count * 2;
    let free_space = cell_content_start.saturating_sub(pointer_array_end);

    if cell_len + 2 > free_space {
        if !allocate_new_leaf_page(buf, root_page, leaf_page, &cell_buf, page_size) {
            return None;
        }
        return Some(rowid);
    }

    let new_cell_offset = cell_content_start - cell_len;
    let _ = buf.write_slice(page_offset + new_cell_offset, &cell_buf);
    let ptr_offset = page_offset + header_offset + 8 + cell_count * 2;
    let _ = buf.write_be_u16(ptr_offset, new_cell_offset as u16);
    let _ = buf.write_be_u16(hdr + 3, (cell_count + 1) as u16);
    let _ = buf.write_be_u16(hdr + 5, new_cell_offset as u16);

    buf.increment_change_counter();
    buf.mark_page_dirty(leaf_page as usize - 1);

    println!("[SYNAPSE] insert_edge: rowid={} {} -{}- {} (vf={} vt={})",
        rowid, subject_id, predicate, object_id, valid_from, valid_to);
    Some(rowid)
}

/// Scan the `edges` table for an active row matching `(subject, predicate)`
/// and rewrite its `valid_to` column in place with the given timestamp.
///
/// Returns the number of rows expired (0 if no active match found, 1
/// if one was updated).
///
/// **How the byte offset is computed**: `Value::Text(s)` in libsqlite's
/// Record holds a `&'a str` that points directly into the DB buffer.
/// Once we've located the matching row, we use the `object_id` string
/// (the 4th column) as an anchor — its end pointer marks the start of
/// the `valid_from` field. `valid_from` is always 8 bytes (INT type 6),
/// so `valid_to` starts 8 bytes after the object_id end. We rewrite
/// the 8-byte BE integer there with the new expiry timestamp.
pub fn sqlite_expire_edge(
    buf: &mut SafeSqliteBuffer,
    subject_id: &str,
    predicate: &str,
    expire_at: i64,
) -> usize {
    if !buf.is_valid() { return 0; }

    // Phase 1: scan to find the matching active edge and derive the
    // byte offset of its valid_to column. We must drop the SqliteDb
    // borrow before mutating the buffer, so we snapshot the offset.
    let target_offset: Option<usize> = {
        let base_ptr = buf.loaded().as_ptr() as usize;
        let db = match SqliteDb::open(buf.loaded()) {
            Ok(db) => db,
            Err(_) => return 0,
        };
        let scanner = match db.table_scan("edges") {
            Ok(s) => s,
            Err(_) => return 0,
        };

        let mut found: Option<usize> = None;
        for result in scanner {
            let record = match result {
                Ok(r) => r,
                Err(_) => continue,
            };
            // Columns: 0=edge_id, 1=subject_id, 2=predicate, 3=object_id,
            //          4=valid_from, 5=valid_to, 6=confidence, 7=source_zid
            let subj_ok = matches!(record.get(1), Some(Value::Text(s)) if s == subject_id);
            let pred_ok = matches!(record.get(2), Some(Value::Text(s)) if s == predicate);
            let is_active = matches!(record.get(5), Some(Value::Integer(0)));
            if !(subj_ok && pred_ok && is_active) { continue; }

            // Anchor on object_id's string slice — it borrows directly
            // from buf.data, so we can compute its absolute byte offset.
            let obj_str: &str = match record.get(3) {
                Some(Value::Text(s)) => s,
                _ => continue,
            };
            let obj_end_ptr = obj_str.as_ptr() as usize + obj_str.len();
            let obj_end_off = obj_end_ptr - base_ptr;
            // valid_from occupies the next 8 bytes; valid_to follows.
            let valid_to_off = obj_end_off + 8;
            found = Some(valid_to_off);
            break;
        }
        found
    };

    match target_offset {
        Some(off) => {
            let bytes = (expire_at as u64).to_be_bytes();
            match buf.write_slice(off, &bytes) {
                Ok(_) => {
                    buf.increment_change_counter();
                    println!("[SYNAPSE] expire_edge: {} -{}- expired at ts={} (offset={})",
                        subject_id, predicate, expire_at, off);
                    1
                }
                Err(_) => {
                    println!("[SYNAPSE] expire_edge: write_slice failed at offset {}", off);
                    0
                }
            }
        }
        None => 0,
    }
}

/// One hop discovered by the graph walker.
#[derive(Clone, Debug)]
pub struct GraphWalkHop {
    pub entity_id: alloc::string::String,
    pub depth: u32,
}

/// Hand-rolled BFS replacement for SQLite's `WITH RECURSIVE` query.
///
/// Starts at `start_entity_id`, expands through `edges` where
/// `valid_to == 0` (currently active), and caps traversal at
/// `max_depth` hops. Traversal is bidirectional — an edge with the
/// current node on either side is expanded, mirroring the research
/// report's CTE.
///
/// Returns the discovered entities in BFS order, each tagged with the
/// depth at which it was first reached. The starting entity is
/// NOT included in the result.
pub fn sqlite_graph_walk(
    buf: &SafeSqliteBuffer,
    start_entity_id: &str,
    max_depth: u32,
) -> Vec<GraphWalkHop> {
    let mut result: Vec<GraphWalkHop> = Vec::new();
    if max_depth == 0 { return result; }

    let db = match open_db(buf) {
        Some(db) => db,
        None => return result,
    };

    // Visited set keeps cycle prevention O(n). Using a Vec here
    // because HashSet needs hashing infrastructure we don't want to
    // drag into synapse-service; linear scans are fine for demo
    // workloads.
    let mut visited: Vec<alloc::string::String> = Vec::new();
    visited.push(alloc::string::String::from(start_entity_id));

    // Frontier = entities whose neighbours we still need to expand.
    // (entity_id, depth)
    let mut frontier: Vec<(alloc::string::String, u32)> = Vec::new();
    frontier.push((alloc::string::String::from(start_entity_id), 0));

    while let Some((current, depth)) = frontier.pop() {
        if depth >= max_depth { continue; }

        // One full scan of `edges` per BFS expansion. This is O(E)
        // per hop — fine for the demo where the graph is tiny. A
        // real deployment would add an index-backed iterator.
        let scanner = match db.table_scan("edges") {
            Ok(s) => s,
            Err(_) => break,
        };
        for row in scanner {
            let record = match row { Ok(r) => r, Err(_) => continue };

            let subj: &str = match record.get(1) { Some(Value::Text(s)) => s, _ => continue };
            let obj: &str = match record.get(3) { Some(Value::Text(s)) => s, _ => continue };
            let vt: i64 = match record.get(5) { Some(Value::Integer(v)) => v, _ => continue };
            if vt != 0 { continue; } // temporal filter: only active edges

            // Expand if `current` is on either side of the edge.
            let neighbour: Option<&str> = if subj == current.as_str() {
                Some(obj)
            } else if obj == current.as_str() {
                Some(subj)
            } else {
                None
            };
            let nb = match neighbour {
                Some(n) => n,
                None => continue,
            };

            // Cycle prevention: skip already visited.
            if visited.iter().any(|v| v.as_str() == nb) { continue; }
            visited.push(alloc::string::String::from(nb));
            result.push(GraphWalkHop {
                entity_id: alloc::string::String::from(nb),
                depth: depth + 1,
            });
            frontier.push((alloc::string::String::from(nb), depth + 1));
        }
    }

    result
}
