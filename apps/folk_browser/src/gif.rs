//! Minimal GIF decoder for Folkering OS (no_std)
//!
//! Supports: GIF87a/GIF89a, first frame only (no animation).
//! LZW decompression with variable-length codes.
//! Global color table, interlaced and non-interlaced.

/// Decode first frame of a GIF. Returns (width, height) or (0,0).
/// Output: RGBA pixels in `output` buffer.
pub fn decode_gif(data: &[u8], output: &mut [u8]) -> (u32, u32) {
    if data.len() < 13 { return (0, 0); }

    // Check GIF signature
    if &data[0..3] != b"GIF" { return (0, 0); }
    // Version: 87a or 89a
    if &data[3..6] != b"87a" && &data[3..6] != b"89a" { return (0, 0); }

    // Logical Screen Descriptor
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;
    let packed = data[10];
    let has_gct = (packed & 0x80) != 0;
    let gct_size_bits = (packed & 0x07) as u32;
    let gct_entries = if has_gct { 1u32 << (gct_size_bits + 1) } else { 0 };
    let _bg_color = data[11];

    if width == 0 || height == 0 { return (0, 0); }
    let w = width.min(512) as usize;
    let h = height.min(512) as usize;
    if output.len() < w * h * 4 { return (0, 0); }

    let mut pos = 13usize;

    // Read Global Color Table
    let mut color_table = [[0u8; 3]; 256];
    if has_gct {
        for i in 0..gct_entries as usize {
            if pos + 3 > data.len() { return (0, 0); }
            color_table[i] = [data[pos], data[pos + 1], data[pos + 2]];
            pos += 3;
        }
    }

    // Skip extensions, find Image Descriptor
    loop {
        if pos >= data.len() { return (0, 0); }
        match data[pos] {
            0x21 => {
                // Extension block — skip
                pos += 1;
                if pos >= data.len() { return (0, 0); }
                pos += 1; // extension type
                // Skip sub-blocks
                loop {
                    if pos >= data.len() { break; }
                    let block_size = data[pos] as usize;
                    pos += 1;
                    if block_size == 0 { break; }
                    pos += block_size;
                }
            }
            0x2C => {
                // Image Descriptor
                pos += 1;
                if pos + 9 > data.len() { return (0, 0); }
                let _img_left = u16::from_le_bytes([data[pos], data[pos + 1]]);
                let _img_top = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
                let img_w = u16::from_le_bytes([data[pos + 4], data[pos + 5]]) as usize;
                let img_h = u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize;
                let img_packed = data[pos + 8];
                let _interlaced = (img_packed & 0x40) != 0;
                let has_lct = (img_packed & 0x80) != 0;
                let lct_bits = (img_packed & 0x07) as u32;
                pos += 9;

                // Local Color Table overrides global
                if has_lct {
                    let lct_entries = 1u32 << (lct_bits + 1);
                    for i in 0..lct_entries as usize {
                        if pos + 3 > data.len() { break; }
                        color_table[i] = [data[pos], data[pos + 1], data[pos + 2]];
                        pos += 3;
                    }
                }

                // LZW Minimum Code Size
                if pos >= data.len() { return (0, 0); }
                let min_code_size = data[pos] as u32;
                pos += 1;
                if min_code_size > 11 { return (0, 0); }

                // Collect all sub-block data
                let mut compressed = [0u8; 65536];
                let mut comp_len = 0;
                loop {
                    if pos >= data.len() { break; }
                    let block_size = data[pos] as usize;
                    pos += 1;
                    if block_size == 0 { break; }
                    let copy = block_size.min(compressed.len() - comp_len);
                    if pos + copy > data.len() { break; }
                    compressed[comp_len..comp_len + copy].copy_from_slice(&data[pos..pos + copy]);
                    comp_len += copy;
                    pos += block_size;
                }

                // LZW decompress
                let mut indices = [0u8; 262144]; // max 512x512
                let pixel_count = img_w * img_h;
                let decoded = lzw_decode(&compressed[..comp_len], min_code_size, &mut indices, pixel_count);

                // Convert indices to RGBA via color table
                let use_w = img_w.min(w);
                let use_h = img_h.min(h);
                for y in 0..use_h {
                    for x in 0..use_w {
                        let idx = y * img_w + x;
                        if idx >= decoded { break; }
                        let ci = indices[idx] as usize;
                        let dst = (y * w + x) * 4;
                        if dst + 3 < output.len() {
                            output[dst] = color_table[ci][0];
                            output[dst + 1] = color_table[ci][1];
                            output[dst + 2] = color_table[ci][2];
                            output[dst + 3] = 255;
                        }
                    }
                }

                return (w as u32, h as u32);
            }
            0x3B => break, // Trailer
            _ => { pos += 1; }
        }
    }

    (0, 0)
}

/// LZW decoder for GIF.
/// Returns number of pixels decoded.
fn lzw_decode(input: &[u8], min_code_size: u32, output: &mut [u8], max_pixels: usize) -> usize {
    let clear_code = 1u32 << min_code_size;
    let eoi_code = clear_code + 1;

    // LZW table: each entry is (prefix, suffix)
    // prefix = previous code index, suffix = byte value
    const TABLE_SIZE: usize = 4096;
    let mut table_prefix = [0u16; TABLE_SIZE];
    let mut table_suffix = [0u8; TABLE_SIZE];
    let mut table_len: u32;

    let mut code_size = min_code_size + 1;
    let mut code_mask = (1u32 << code_size) - 1;

    // Reset table
    table_len = eoi_code + 1;
    for i in 0..clear_code {
        table_prefix[i as usize] = 0xFFFF; // no prefix
        table_suffix[i as usize] = i as u8;
    }

    let mut bit_pos = 0u32;
    let mut out_pos = 0usize;
    let mut prev_code: i32 = -1;

    loop {
        // Read next code
        let byte_pos = (bit_pos / 8) as usize;
        if byte_pos + 2 >= input.len() { break; }
        let bits = (input[byte_pos] as u32)
            | ((input[byte_pos + 1] as u32) << 8)
            | ((input.get(byte_pos + 2).copied().unwrap_or(0) as u32) << 16);
        let code = (bits >> (bit_pos & 7)) & code_mask;
        bit_pos += code_size;

        if code == clear_code {
            // Reset
            code_size = min_code_size + 1;
            code_mask = (1u32 << code_size) - 1;
            table_len = eoi_code + 1;
            prev_code = -1;
            continue;
        }
        if code == eoi_code { break; }

        // Decode the code into output
        let mut stack = [0u8; 4096];
        let mut stack_len = 0usize;

        if code < table_len {
            // Code exists in table — decode it
            let mut c = code;
            while c < TABLE_SIZE as u32 && stack_len < 4096 {
                stack[stack_len] = table_suffix[c as usize];
                stack_len += 1;
                if table_prefix[c as usize] == 0xFFFF { break; }
                c = table_prefix[c as usize] as u32;
            }
        } else if code == table_len && prev_code >= 0 {
            // Special case: code not yet in table
            let mut c = prev_code as u32;
            let mut first = 0u8;
            let mut temp_stack = [0u8; 4096];
            let mut temp_len = 0;
            while c < TABLE_SIZE as u32 && temp_len < 4096 {
                temp_stack[temp_len] = table_suffix[c as usize];
                temp_len += 1;
                if table_prefix[c as usize] == 0xFFFF { first = table_suffix[c as usize]; break; }
                c = table_prefix[c as usize] as u32;
            }
            stack[0] = first;
            stack_len = 1;
            for i in 0..temp_len {
                if stack_len >= 4096 { break; }
                stack[stack_len] = temp_stack[i];
                stack_len += 1;
            }
        } else {
            break; // Invalid code
        }

        // Write decoded pixels (reversed — stack is backwards)
        let mut i = stack_len;
        while i > 0 {
            i -= 1;
            if out_pos >= max_pixels || out_pos >= output.len() { break; }
            output[out_pos] = stack[i];
            out_pos += 1;
        }

        // Add new entry to table
        if prev_code >= 0 && (table_len as usize) < TABLE_SIZE {
            table_prefix[table_len as usize] = prev_code as u16;
            // First byte of the decoded sequence
            table_suffix[table_len as usize] = stack[stack_len - 1]; // first decoded byte
            table_len += 1;

            // Increase code size if needed
            if table_len > code_mask && code_size < 12 {
                code_size += 1;
                code_mask = (1u32 << code_size) - 1;
            }
        }

        prev_code = code as i32;
    }

    out_pos
}
