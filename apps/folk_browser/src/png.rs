//! Minimal PNG decoder for Folkering OS (no_std, no alloc dependencies)
//!
//! Supports: 8-bit RGB and RGBA, non-interlaced only.
//! Reads IHDR for dimensions, concatenates IDAT chunks, inflates zlib,
//! applies row filters (None, Sub, Up, Average, Paeth).
//!
//! Returns RGBA pixel data in a caller-provided buffer.

/// Decode a PNG image from raw bytes into RGBA pixels.
/// Returns (width, height) on success, (0, 0) on failure.
/// `output` must be large enough: width * height * 4 bytes.
pub fn decode_png(data: &[u8], output: &mut [u8]) -> (u32, u32) {
    // Check PNG signature
    if data.len() < 8 || &data[0..8] != b"\x89PNG\r\n\x1A\n" {
        return (0, 0);
    }

    let mut pos = 8;
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut bit_depth: u8 = 0;
    let mut color_type: u8 = 0;
    let mut compressed_data = [0u8; 65536]; // max 64KB compressed
    let mut comp_len = 0usize;

    // Parse chunks
    while pos + 12 <= data.len() {
        let chunk_len = u32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
        let chunk_type = &data[pos+4..pos+8];
        let chunk_data_start = pos + 8;
        let chunk_data_end = chunk_data_start + chunk_len;

        if chunk_data_end > data.len() { break; }

        if chunk_type == b"IHDR" && chunk_len >= 13 {
            let cd = &data[chunk_data_start..];
            width = u32::from_be_bytes([cd[0], cd[1], cd[2], cd[3]]);
            height = u32::from_be_bytes([cd[4], cd[5], cd[6], cd[7]]);
            bit_depth = cd[8];
            color_type = cd[9];
            // We only support 8-bit RGB(A)
            if bit_depth != 8 || (color_type != 2 && color_type != 6) {
                return (0, 0);
            }
        } else if chunk_type == b"IDAT" {
            let copy = chunk_len.min(compressed_data.len() - comp_len);
            compressed_data[comp_len..comp_len + copy]
                .copy_from_slice(&data[chunk_data_start..chunk_data_start + copy]);
            comp_len += copy;
        } else if chunk_type == b"IEND" {
            break;
        }

        pos = chunk_data_end + 4; // skip CRC
    }

    if width == 0 || height == 0 || comp_len < 6 { return (0, 0); }

    // Channels per pixel
    let channels: usize = if color_type == 6 { 4 } else { 3 }; // RGBA or RGB
    let stride = width as usize * channels + 1; // +1 for filter byte
    let raw_size = stride * height as usize;

    // Inflate zlib data (skip 2-byte zlib header)
    let mut inflated = [0u8; 131072]; // max 128KB raw (enough for ~180x180 RGBA)
    let inf_len = inflate(&compressed_data[2..comp_len], &mut inflated);
    if inf_len < raw_size { return (0, 0); }

    // Apply row filters and convert to RGBA
    let out_stride = width as usize * 4;
    let needed = out_stride * height as usize;
    if output.len() < needed { return (0, 0); }

    let bpp = channels;
    let ptr = inflated.as_mut_ptr();

    for y in 0..height as usize {
        let row_start = y * stride;
        let filter = inflated[row_start];
        let row_data_start = row_start + 1;
        let row_len = width as usize * channels;

        // Apply filter using raw pointers to avoid borrow conflicts
        unsafe {
            match filter {
                0 => {} // None
                1 => { // Sub
                    for x in bpp..row_len {
                        let v = *ptr.add(row_data_start + x);
                        let left = *ptr.add(row_data_start + x - bpp);
                        *ptr.add(row_data_start + x) = v.wrapping_add(left);
                    }
                }
                2 => { // Up
                    if y > 0 {
                        let prev_start = (y - 1) * stride + 1;
                        for x in 0..row_len {
                            let v = *ptr.add(row_data_start + x);
                            let up = *ptr.add(prev_start + x);
                            *ptr.add(row_data_start + x) = v.wrapping_add(up);
                        }
                    }
                }
                3 => { // Average
                    for x in 0..row_len {
                        let v = *ptr.add(row_data_start + x);
                        let left = if x >= bpp { *ptr.add(row_data_start + x - bpp) as u16 } else { 0 };
                        let up = if y > 0 { *ptr.add((y - 1) * stride + 1 + x) as u16 } else { 0 };
                        *ptr.add(row_data_start + x) = v.wrapping_add(((left + up) / 2) as u8);
                    }
                }
                4 => { // Paeth
                    for x in 0..row_len {
                        let v = *ptr.add(row_data_start + x);
                        let left = if x >= bpp { *ptr.add(row_data_start + x - bpp) } else { 0 };
                        let up = if y > 0 { *ptr.add((y - 1) * stride + 1 + x) } else { 0 };
                        let up_left = if y > 0 && x >= bpp { *ptr.add((y - 1) * stride + 1 + x - bpp) } else { 0 };
                        *ptr.add(row_data_start + x) = v.wrapping_add(paeth_predictor(left, up, up_left));
                    }
                }
                _ => {}
            }

            // Convert to RGBA output
            for x in 0..width as usize {
                let src = row_data_start + x * channels;
                let dst = y * out_stride + x * 4;
                output[dst] = *ptr.add(src);
                output[dst + 1] = *ptr.add(src + 1);
                output[dst + 2] = *ptr.add(src + 2);
                output[dst + 3] = if channels == 4 { *ptr.add(src + 3) } else { 255 };
            }
        }
    }

    (width, height)
}

fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let pa = (p - a as i16).unsigned_abs();
    let pb = (p - b as i16).unsigned_abs();
    let pc = (p - c as i16).unsigned_abs();
    if pa <= pb && pa <= pc { a }
    else if pb <= pc { b }
    else { c }
}

// ── Minimal DEFLATE inflate (RFC 1951) ──────────────────────────────────

/// Inflate DEFLATE-compressed data. Returns bytes written to output.
fn inflate(input: &[u8], output: &mut [u8]) -> usize {
    let mut reader = BitReader::new(input);
    let mut out_pos = 0usize;

    loop {
        let bfinal = reader.read_bits(1);
        let btype = reader.read_bits(2);

        match btype {
            0 => {
                // Stored (uncompressed)
                reader.align();
                let len = reader.read_bits(16) as usize;
                let _nlen = reader.read_bits(16);
                for _ in 0..len {
                    if out_pos >= output.len() { return out_pos; }
                    output[out_pos] = reader.read_bits(8) as u8;
                    out_pos += 1;
                }
            }
            1 | 2 => {
                // Fixed or dynamic Huffman
                let (lit_lens, dist_lens) = if btype == 1 {
                    fixed_huffman_lengths()
                } else {
                    decode_dynamic_huffman(&mut reader)
                };

                let lit_table = build_huffman_table(&lit_lens);
                let dist_table = build_huffman_table(&dist_lens);

                loop {
                    let sym = decode_huffman(&mut reader, &lit_table);
                    if sym == 256 { break; } // End of block
                    if sym < 256 {
                        if out_pos >= output.len() { return out_pos; }
                        output[out_pos] = sym as u8;
                        out_pos += 1;
                    } else {
                        // Length/distance pair
                        let length = decode_length(&mut reader, sym);
                        let dist_sym = decode_huffman(&mut reader, &dist_table);
                        let distance = decode_distance(&mut reader, dist_sym);

                        for _ in 0..length {
                            if out_pos >= output.len() { return out_pos; }
                            let src = if distance <= out_pos { out_pos - distance } else { 0 };
                            output[out_pos] = output[src];
                            out_pos += 1;
                        }
                    }
                }
            }
            _ => break, // Invalid
        }

        if bfinal != 0 { break; }
    }
    out_pos
}

struct BitReader<'a> { data: &'a [u8], pos: usize, bit: u8, current: u8 }
impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, pos: 0, bit: 0, current: 0 } }
    fn read_bits(&mut self, n: u8) -> u32 {
        let mut val = 0u32;
        for i in 0..n {
            if self.bit == 0 {
                self.current = if self.pos < self.data.len() { self.data[self.pos] } else { 0 };
                self.pos += 1;
            }
            val |= (((self.current >> self.bit) & 1) as u32) << i;
            self.bit = (self.bit + 1) & 7;
        }
        val
    }
    fn align(&mut self) { if self.bit != 0 { self.bit = 0; } }
}

// Huffman tables (simplified: max 15-bit codes, 320 symbols)
const MAX_SYMBOLS: usize = 320;
struct HuffTable { min_code: [u32; 16], offset: [u16; 16], symbols: [u16; MAX_SYMBOLS] }

fn build_huffman_table(lengths: &[u8]) -> HuffTable {
    let mut bl_count = [0u32; 16];
    for &l in lengths { if (l as usize) < 16 { bl_count[l as usize] += 1; } }
    bl_count[0] = 0;

    let mut next_code = [0u32; 16];
    let mut code = 0u32;
    for bits in 1..16 {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }

    let mut table = HuffTable {
        min_code: [0; 16], offset: [0; 16], symbols: [0; MAX_SYMBOLS],
    };

    let mut sym_idx = 0u16;
    for bits in 1..16usize {
        table.min_code[bits] = next_code[bits];
        table.offset[bits] = sym_idx;
        for (i, &l) in lengths.iter().enumerate() {
            if l as usize == bits {
                if (sym_idx as usize) < MAX_SYMBOLS {
                    table.symbols[sym_idx as usize] = i as u16;
                    sym_idx += 1;
                }
            }
        }
    }
    table
}

fn decode_huffman(reader: &mut BitReader, table: &HuffTable) -> u32 {
    let mut code = 0u32;
    for bits in 1..16usize {
        code = (code << 1) | reader.read_bits(1);
        let count = code.wrapping_sub(table.min_code[bits]);
        let idx = table.offset[bits] as usize + count as usize;
        if idx < MAX_SYMBOLS && code < table.min_code[bits] + (table.offset.get(bits + 1).copied().unwrap_or(table.offset[bits]) - table.offset[bits]) as u32 {
            return table.symbols[idx] as u32;
        }
    }
    0
}

fn fixed_huffman_lengths() -> ([u8; MAX_SYMBOLS], [u8; 32]) {
    let mut lit = [0u8; MAX_SYMBOLS];
    for i in 0..=143 { lit[i] = 8; }
    for i in 144..=255 { lit[i] = 9; }
    for i in 256..=279 { lit[i] = 7; }
    for i in 280..=287 { lit[i] = 8; }
    let mut dist = [0u8; 32];
    for i in 0..30 { dist[i] = 5; }
    (lit, dist)
}

fn decode_dynamic_huffman(reader: &mut BitReader) -> ([u8; MAX_SYMBOLS], [u8; 32]) {
    let hlit = reader.read_bits(5) as usize + 257;
    let hdist = reader.read_bits(5) as usize + 1;
    let hclen = reader.read_bits(4) as usize + 4;

    const ORDER: [usize; 19] = [16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1,15];
    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen { cl_lengths[ORDER[i]] = reader.read_bits(3) as u8; }

    let cl_table = build_huffman_table(&cl_lengths);

    let total = hlit + hdist;
    let mut lengths = [0u8; MAX_SYMBOLS + 32];
    let mut i = 0;
    while i < total {
        let sym = decode_huffman(reader, &cl_table);
        match sym {
            0..=15 => { lengths[i] = sym as u8; i += 1; }
            16 => {
                let rep = reader.read_bits(2) as usize + 3;
                let val = if i > 0 { lengths[i - 1] } else { 0 };
                for _ in 0..rep { if i < lengths.len() { lengths[i] = val; i += 1; } }
            }
            17 => { let rep = reader.read_bits(3) as usize + 3; i += rep; }
            18 => { let rep = reader.read_bits(7) as usize + 11; i += rep; }
            _ => { i += 1; }
        }
    }

    let mut lit = [0u8; MAX_SYMBOLS];
    for j in 0..hlit.min(MAX_SYMBOLS) { lit[j] = lengths[j]; }
    let mut dist = [0u8; 32];
    for j in 0..hdist.min(32) { dist[j] = lengths[hlit + j]; }
    (lit, dist)
}

const LEN_BASE: [u16; 29] = [3,4,5,6,7,8,9,10,11,13,15,17,19,23,27,31,35,43,51,59,67,83,99,115,131,163,195,227,258];
const LEN_EXTRA: [u8; 29] = [0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,0];

fn decode_length(reader: &mut BitReader, sym: u32) -> usize {
    let idx = (sym - 257) as usize;
    if idx >= 29 { return 0; }
    LEN_BASE[idx] as usize + reader.read_bits(LEN_EXTRA[idx]) as usize
}

const DIST_BASE: [u16; 30] = [1,2,3,4,5,7,9,13,17,25,33,49,65,97,129,193,257,385,513,769,1025,1537,2049,3073,4097,6145,8193,12289,16385,24577];
const DIST_EXTRA: [u8; 30] = [0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13];

fn decode_distance(reader: &mut BitReader, sym: u32) -> usize {
    let idx = sym as usize;
    if idx >= 30 { return 1; }
    DIST_BASE[idx] as usize + reader.read_bits(DIST_EXTRA[idx]) as usize
}
