//! Minimal GGUF v3 parser — only extracts vocab metadata for tokenizer init.

pub struct VocabMeta {
    pub vocab_offset: usize,
    pub vocab_size: usize,
    pub bos_id: u32,
    pub eos_id: u32,
}

pub fn parse(data: &[u8]) -> Option<VocabMeta> {
    if data.len() < 20 || &data[0..4] != b"GGUF" {
        return None;
    }
    let _version = u32_le(data, 4);
    let n_tensors = u64_le(data, 8) as usize;
    let n_metadata = u64_le(data, 16) as usize;

    let mut pos = 24;
    let mut bos_id: u32 = 1;
    let mut eos_id: u32 = 2;
    let mut vocab_offset: usize = 0;
    let mut vocab_size: usize = 0;

    // Parse metadata key-value pairs
    for _ in 0..n_metadata {
        let (key, new_pos) = read_string(data, pos)?;
        pos = new_pos;
        let val_type = u32_le(data, pos);
        pos += 4;

        match key.as_str() {
            "tokenizer.ggml.bos_token_id" => {
                bos_id = read_u32_val(data, pos, val_type)?;
                pos = skip_value(data, pos, val_type)?;
            }
            "tokenizer.ggml.eos_token_id" => {
                eos_id = read_u32_val(data, pos, val_type)?;
                pos = skip_value(data, pos, val_type)?;
            }
            "tokenizer.ggml.tokens" => {
                // Array type: elem_type (u32) + count (u64) + elements
                if val_type != 9 {
                    pos = skip_value(data, pos, val_type)?;
                    continue;
                }
                let elem_type = u32_le(data, pos);
                pos += 4;
                let count = u64_le(data, pos) as usize;
                pos += 8;
                vocab_size = count;
                vocab_offset = pos; // offset to first string element
                // Skip all string elements
                for _ in 0..count {
                    pos = skip_value(data, pos, elem_type)?;
                }
            }
            _ => {
                pos = skip_value(data, pos, val_type)?;
            }
        }
    }

    // Skip tensor info (we don't need it)
    let _ = n_tensors;

    if vocab_offset == 0 || vocab_size == 0 {
        return None;
    }

    Some(VocabMeta { vocab_offset, vocab_size, bos_id, eos_id })
}

fn u32_le(data: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]])
}

fn u64_le(data: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes([
        data[pos], data[pos+1], data[pos+2], data[pos+3],
        data[pos+4], data[pos+5], data[pos+6], data[pos+7],
    ])
}

fn read_string(data: &[u8], pos: usize) -> Option<(String, usize)> {
    let len = u64_le(data, pos) as usize;
    let start = pos + 8;
    if start + len > data.len() { return None; }
    let s = String::from_utf8_lossy(&data[start..start + len]).to_string();
    Some((s, start + len))
}

fn read_u32_val(data: &[u8], pos: usize, val_type: u32) -> Option<u32> {
    match val_type {
        4 => Some(u32_le(data, pos)),   // UINT32
        5 => Some(u32_le(data, pos)),   // INT32
        0 => Some(data[pos] as u32),    // UINT8
        _ => None,
    }
}

fn skip_value(data: &[u8], pos: usize, val_type: u32) -> Option<usize> {
    match val_type {
        0 | 1 | 7 => Some(pos + 1),     // UINT8, INT8, BOOL
        2 | 3     => Some(pos + 2),       // UINT16, INT16
        4 | 5 | 6 => Some(pos + 4),       // UINT32, INT32, FLOAT32
        10 | 11 | 12 => Some(pos + 8),    // UINT64, INT64, FLOAT64
        8 => {                             // STRING
            let len = u64_le(data, pos) as usize;
            Some(pos + 8 + len)
        }
        9 => {                             // ARRAY
            let elem_type = u32_le(data, pos);
            let count = u64_le(data, pos + 4) as usize;
            let mut p = pos + 12;
            for _ in 0..count {
                p = skip_value(data, p, elem_type)?;
            }
            Some(p)
        }
        _ => None,
    }
}
