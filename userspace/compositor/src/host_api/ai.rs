//! AI and inference host functions for WASM apps
//! LLM generation, tensor inspection, tokenization, semantic queries.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;
use super::HostState;

pub fn register(linker: &mut Linker<HostState>) {
    // Tries LOCAL brain first (zero latency, zero network).
    // Falls back to Ollama proxy for complex queries.
    // "Spinal cord" = local, "Cerebral cortex" = cloud.
    let _ = linker.func_wrap("env", "folk_slm_generate",
        |mut caller: Caller<HostState>, prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if prompt_len <= 0 || prompt_len > 2048 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut prompt_buf = alloc::vec![0u8; prompt_len as usize];
            if mem.read(&caller, prompt_ptr as usize, &mut prompt_buf).is_err() { return -1; }
            let prompt = match alloc::str::from_utf8(&prompt_buf) {
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Try LOCAL brain first (zero latency, zero network)
            if let Some(local_response) = crate::slm_runtime::brain().generate(&prompt) {
                let bytes = local_response.as_bytes();
                let copy_len = bytes.len().min(max_len as usize);
                if mem.write(&mut caller, buf_ptr as usize, &bytes[..copy_len]).is_ok() {
                    return copy_len as i32;
                }
            }

            // Fallback: route to proxy (Ollama FAST tier)
            let full_prompt = alloc::format!("__SLM_GENERATE__{}", prompt);
            let gemini_buf_size = (max_len as usize).max(8192);
            let mut response = alloc::vec![0u8; gemini_buf_size];
            let bytes = libfolk::sys::ask_gemini(&full_prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize).min(gemini_buf_size);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // Phase 11: PromptLab — Inference with per-token logit analysis
    // folk_slm_generate_with_logits(prompt_ptr, prompt_len, out_ptr, max_len) -> i32
    // Runs inference AND returns structured PLAB result with per-token confidence.
    // After text generation, reads TDMP tensor mailbox for last-token logits,
    // computes softmax for top-K probabilities, and estimates per-word confidence.
    //
    // PLAB wire format (written to out_ptr):
    //   [0-3]   magic "PLAB"
    //   [4-7]   text_len: u32
    //   [8-11]  token_count: u32
    //   [12-15] flags: u32 (bit0=has_real_logits_for_last_token)
    //   [16..16+text_len] UTF-8 text (padded to 4-byte boundary)
    //   Then token_count × 24-byte entries:
    //     [0-1]  start: u16 (byte offset in text)
    //     [2-3]  len: u16
    //     [4-7]  prob: f32 (0.0-1.0)
    //     [8-11] alt1_prob: f32
    //     [12-15] alt2_prob: f32
    //     [16-19] alt3_prob: f32
    //     [20-23] reserved
    let _ = linker.func_wrap("env", "folk_slm_generate_with_logits",
        |mut caller: Caller<HostState>, prompt_ptr: i32, prompt_len: i32, out_ptr: i32, max_len: i32| -> i32 {
            if prompt_len <= 0 || prompt_len > 4096 || max_len < 64 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // Read prompt from WASM memory
            let mut prompt_buf = alloc::vec![0u8; prompt_len as usize];
            if mem.read(&caller, prompt_ptr as usize, &mut prompt_buf).is_err() { return -1; }
            let prompt = match alloc::str::from_utf8(&prompt_buf) {
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Step 1: Run inference (same path as folk_slm_generate)
            let mut gen_buf = alloc::vec![0u8; 2048];
            let gen_len;

            // Try local brain first
            if let Some(local_resp) = crate::slm_runtime::brain().generate(&prompt) {
                let bytes = local_resp.as_bytes();
                let copy = bytes.len().min(gen_buf.len());
                gen_buf[..copy].copy_from_slice(&bytes[..copy]);
                gen_len = copy;
            } else {
                // Fallback to proxy
                let full_prompt = alloc::format!("__SLM_GENERATE__{}", prompt);
                let bytes = libfolk::sys::ask_gemini(&full_prompt, &mut gen_buf);
                if bytes == 0 { return -1; }
                gen_len = bytes;
            }

            // Step 2: Split generated text into word-tokens
            let text = &gen_buf[..gen_len];
            let mut tokens: alloc::vec::Vec<(u16, u16)> = alloc::vec::Vec::new(); // (start, len)
            {
                let mut i = 0usize;
                while i < gen_len {
                    // Skip whitespace
                    while i < gen_len && (text[i] == b' ' || text[i] == b'\n' || text[i] == b'\t') {
                        i += 1;
                    }
                    if i >= gen_len { break; }
                    let word_start = i;
                    // Consume word
                    while i < gen_len && text[i] != b' ' && text[i] != b'\n' && text[i] != b'\t' {
                        i += 1;
                    }
                    if i > word_start && tokens.len() < 128 {
                        tokens.push((word_start as u16, (i - word_start) as u16));
                    }
                }
            }

            // Step 3: Try to read TDMP tensor mailbox for real logits
            let mut has_real_logits = false;
            let mut last_token_probs = [0.0f32; 4]; // top-4 softmax probs
            {
                let mut hdr = [0u8; 512];
                if libfolk::sys::block::read_sector(1, &mut hdr).is_ok() {
                    if hdr[0] == b'T' && hdr[1] == b'D' && hdr[2] == b'M' && hdr[3] == b'P' {
                        // Read summary floats from header (offset 112, up to 100 × f32)
                        // These are the first 100 logit values — find top-4
                        let mut top4: [(f32, usize); 4] = [(-1e30, 0); 4];
                        for j in 0..100 {
                            let off = 112 + j * 4;
                            if off + 4 > 512 { break; }
                            let v = f32::from_le_bytes([hdr[off], hdr[off+1], hdr[off+2], hdr[off+3]]);
                            // Insert into top4 if larger than smallest
                            if v > top4[3].0 {
                                top4[3] = (v, j);
                                // Bubble sort
                                for k in (1..4).rev() {
                                    if top4[k].0 > top4[k-1].0 {
                                        top4.swap(k, k-1);
                                    }
                                }
                            }
                        }
                        // Compute softmax on top-4
                        let max_val = top4[0].0;
                        let mut sum = 0.0f32;
                        let mut exps = [0.0f32; 4];
                        for k in 0..4 {
                            // Clamp to prevent overflow
                            let x = (top4[k].0 - max_val).max(-20.0);
                            // Fast exp approximation: e^x ≈ (1 + x/256)^256
                            let mut e = 1.0 + x / 16.0;
                            e = e * e; e = e * e; e = e * e; e = e * e; // ^16
                            exps[k] = e;
                            sum += e;
                        }
                        if sum > 0.0 {
                            for k in 0..4 {
                                last_token_probs[k] = exps[k] / sum;
                            }
                            has_real_logits = true;
                        }
                    }
                }
            }

            // Step 4: Assign per-token probabilities
            // Last token gets real logits (if available), others get heuristic estimates
            let token_count = tokens.len();

            // Build PLAB buffer
            let text_padded = (gen_len + 3) & !3; // align to 4
            let total_size = 16 + text_padded + token_count * 24;
            if total_size > max_len as usize { return -1; }

            let mut out = alloc::vec![0u8; total_size];

            // Header
            out[0..4].copy_from_slice(b"PLAB");
            out[4..8].copy_from_slice(&(gen_len as u32).to_le_bytes());
            out[8..12].copy_from_slice(&(token_count as u32).to_le_bytes());
            let flags: u32 = if has_real_logits { 1 } else { 0 };
            out[12..16].copy_from_slice(&flags.to_le_bytes());

            // Text
            out[16..16 + gen_len].copy_from_slice(text);

            // Token entries
            let entries_start = 16 + text_padded;
            for (idx, &(start, len)) in tokens.iter().enumerate() {
                let off = entries_start + idx * 24;
                out[off..off+2].copy_from_slice(&start.to_le_bytes());
                out[off+2..off+4].copy_from_slice(&len.to_le_bytes());

                if idx == token_count - 1 && has_real_logits {
                    // Last token: real TDMP probabilities
                    out[off+4..off+8].copy_from_slice(&last_token_probs[0].to_le_bytes());
                    out[off+8..off+12].copy_from_slice(&last_token_probs[1].to_le_bytes());
                    out[off+12..off+16].copy_from_slice(&last_token_probs[2].to_le_bytes());
                    out[off+16..off+20].copy_from_slice(&last_token_probs[3].to_le_bytes());
                } else {
                    // Heuristic: common short words get high confidence,
                    // longer/rarer words get lower confidence
                    let word_len = len as f32;
                    let base = if word_len <= 3.0 { 0.92 } else if word_len <= 6.0 { 0.78 } else { 0.55 };
                    // Add slight variation based on position
                    let pos_factor = 1.0 - (idx as f32 * 0.003).min(0.15);
                    let prob = (base * pos_factor).max(0.1).min(0.99);
                    out[off+4..off+8].copy_from_slice(&prob.to_le_bytes());
                    out[off+8..off+12].copy_from_slice(&(prob * 0.3).to_le_bytes());
                    out[off+12..off+16].copy_from_slice(&(prob * 0.15).to_le_bytes());
                    out[off+16..off+20].copy_from_slice(&(prob * 0.08).to_le_bytes());
                }
            }

            // Write to WASM memory
            if mem.write(&mut caller, out_ptr as usize, &out).is_ok() {
                total_size as i32
            } else { -1 }
        },
    );

    // Splits input text into approximate BPE-style tokens.
    // Output format: [token_count:u32] then per token: [start:u16, len:u16]
    // Returns bytes written.
    let _ = linker.func_wrap("env", "folk_tokenize",
        |mut caller: Caller<HostState>, text_ptr: i32, text_len: i32, out_ptr: i32, max_len: i32| -> i32 {
            if text_len <= 0 || text_len > 2048 || max_len < 8 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let mut text = alloc::vec![0u8; text_len as usize];
            if mem.read(&caller, text_ptr as usize, &mut text).is_err() { return -1; }

            // BPE-style tokenization heuristic:
            // Split on: spaces, punctuation boundaries, CamelCase, digit/letter boundaries
            let mut tokens: alloc::vec::Vec<(u16, u16)> = alloc::vec::Vec::new();
            let mut i = 0usize;

            while i < text.len() && tokens.len() < 256 {
                let start = i;

                if text[i] == b' ' || text[i] == b'\n' || text[i] == b'\t' {
                    // Whitespace token
                    while i < text.len() && (text[i] == b' ' || text[i] == b'\n' || text[i] == b'\t') { i += 1; }
                } else if text[i].is_ascii_punctuation() {
                    // Single punctuation token
                    i += 1;
                } else if text[i].is_ascii_digit() {
                    // Number run
                    while i < text.len() && text[i].is_ascii_digit() { i += 1; }
                } else if text[i].is_ascii_alphabetic() {
                    // Word token — split at case boundaries (CamelCase)
                    i += 1;
                    let mut word_len = 1;
                    while i < text.len() && text[i].is_ascii_alphabetic() && word_len < 12 {
                        // Split at lowercase→uppercase boundary
                        if text[i].is_ascii_uppercase() && i > start + 1 && text[i-1].is_ascii_lowercase() {
                            break;
                        }
                        i += 1;
                        word_len += 1;
                    }
                    // Further split long words at ~4 char boundaries (subword)
                    if i - start > 6 {
                        let mid = start + (i - start) / 2;
                        tokens.push((start as u16, (mid - start) as u16));
                        tokens.push((mid as u16, (i - mid) as u16));
                        continue;
                    }
                } else {
                    // Unknown byte
                    i += 1;
                }

                if i > start {
                    tokens.push((start as u16, (i - start) as u16));
                }
            }

            // Pack output: [count:u32] [start:u16, len:u16] * count
            let count = tokens.len();
            let out_size = 4 + count * 4;
            if out_size > max_len as usize { return -1; }

            let mut out = alloc::vec![0u8; out_size];
            out[0..4].copy_from_slice(&(count as u32).to_le_bytes());
            for (ti, (s, l)) in tokens.iter().enumerate() {
                let off = 4 + ti * 4;
                out[off..off+2].copy_from_slice(&s.to_le_bytes());
                out[off+2..off+4].copy_from_slice(&l.to_le_bytes());
            }

            if mem.write(&mut caller, out_ptr as usize, &out).is_ok() {
                out_size as i32
            } else { -1 }
        },
    );

    // Phase 15: Tensor Write — Modify weights in TDMP mailbox (DANGEROUS)
    // folk_tensor_write(sector_offset, byte_offset, value_bits) -> i32
    // Writes a single f32 value to the TDMP data sectors on VirtIO-blk.
    // sector_offset: 1+ (data sectors, 0=header is read-only)
    // byte_offset: offset within the sector (0-508, must be 4-aligned)
    // value_bits: f32 reinterpreted as i32 (IEEE 754 bits)
    // Returns 0 on success, -1 on error.
    // WARNING: Modifying live tensor data while inference runs WILL corrupt output.
    // The write uses block_write which is serialized by the kernel's block device lock.
    let _ = linker.func_wrap("env", "folk_tensor_write",
        |_caller: Caller<HostState>, sector_offset: i32, byte_offset: i32, value_bits: i32| -> i32 {
            if sector_offset < 1 || sector_offset > 256 { return -1; } // Can't write header
            if byte_offset < 0 || byte_offset > 508 || byte_offset % 4 != 0 { return -1; }

            // Read the sector, modify the float, write it back
            let disk_sector = 1u64 + sector_offset as u64; // TDMP header at sector 1
            let mut buf = [0u8; 512];
            if libfolk::sys::block::read_sector(disk_sector, &mut buf).is_err() {
                return -1;
            }

            // Write the f32 value at the specified offset
            let off = byte_offset as usize;
            let bytes = value_bits.to_le_bytes();
            buf[off] = bytes[0];
            buf[off+1] = bytes[1];
            buf[off+2] = bytes[2];
            buf[off+3] = bytes[3];

            if libfolk::sys::block::write_sector(disk_sector, &buf).is_err() {
                return -1;
            }
            0
        },
    );

    // Semantic network request: "Get weather in Oslo" → OS translates to API call.
    // The LLM proxy interprets the intent, calls the appropriate API, and returns
    // structured data. The app never needs to know HTTP headers or JSON parsing.
    let _ = linker.func_wrap("env", "folk_intent_fetch",
        |mut caller: Caller<HostState>, query_ptr: i32, query_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if query_len <= 0 || query_len > 512 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut query_buf = alloc::vec![0u8; query_len as usize];
            if mem.read(&caller, query_ptr as usize, &mut query_buf).is_err() { return -1; }
            let query = match alloc::str::from_utf8(&query_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            // Send as MCP ChatRequest with __INTENT_FETCH__ prefix
            // Proxy will: interpret intent → call API → return structured result
            let prompt = alloc::format!("__INTENT_FETCH__{}", query);
            let gemini_buf_size = (max_len as usize).max(8192);
            let mut response = alloc::vec![0u8; gemini_buf_size];
            let bytes = libfolk::sys::ask_gemini(&prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize).min(gemini_buf_size);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // Phase 10: Tensor Inspection — Read inference tensor mailbox from VirtIO-blk
    // folk_tensor_read(buf_ptr, buf_len, sector_offset) -> i32
    // Reads from the TDMP (Tensor DuMP) disk mailbox written by the inference server.
    //   sector_offset=0: Header sector (512 bytes) — magic, stats, shape, 100 summary floats
    //   sector_offset=1+: Data sectors with raw f32 values (up to 256 sectors, 128KB)
    // Returns bytes read, or -1 on error.
    let _ = linker.func_wrap("env", "folk_tensor_read",
        |mut caller: Caller<HostState>, buf_ptr: i32, buf_len: i32, sector_offset: i32| -> i32 {
            if buf_len <= 0 || sector_offset < 0 || sector_offset > 256 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            // TDMP header is at sector 1, data starts at sector 2
            let disk_sector = 1u64 + sector_offset as u64;
            let sectors_to_read = ((buf_len as usize) + 511) / 512;
            let sectors_to_read = sectors_to_read.min(257 - sector_offset as usize);
            let total_bytes = sectors_to_read * 512;
            let mut read_buf = alloc::vec![0u8; total_bytes];
            if libfolk::sys::block::block_read(disk_sector, &mut read_buf, sectors_to_read).is_err() {
                return -1;
            }
            let copy_len = total_bytes.min(buf_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &read_buf[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_semantic_extract(html_ptr, html_len, buf_ptr, max_len) -> i32
    // Takes raw HTML, strips boilerplate (script/style/nav), sends to LLM
    // for semantic extraction. Returns clean markdown text.
    let _ = linker.func_wrap("env", "folk_semantic_extract",
        |mut caller: Caller<HostState>, html_ptr: i32, html_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if html_len <= 0 || html_len > 8192 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // Read HTML from WASM memory
            let mut html_buf = alloc::vec![0u8; html_len as usize];
            if mem.read(&caller, html_ptr as usize, &mut html_buf).is_err() { return -1; }
            let html = match alloc::str::from_utf8(&html_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };

            // Pre-process: strip <script>, <style>, <nav>, <header>, <footer>, <noscript> content
            let mut cleaned = String::with_capacity(html.len());
            let mut skip_depth: i32 = 0;
            let mut i = 0;
            let bytes = html.as_bytes();
            while i < bytes.len() {
                if bytes[i] == b'<' {
                    // Check for opening skip tags
                    let rest = &html[i..];
                    let is_skip_open = rest.len() > 7 && (
                        rest[..7].eq_ignore_ascii_case("<script") ||
                        rest[..6].eq_ignore_ascii_case("<style") ||
                        rest[..4].eq_ignore_ascii_case("<nav") ||
                        rest[..7].eq_ignore_ascii_case("<header") ||
                        rest[..7].eq_ignore_ascii_case("<footer") ||
                        rest[..9].eq_ignore_ascii_case("<noscript")
                    );
                    let is_skip_close = rest.len() > 8 && (
                        rest[..9].eq_ignore_ascii_case("</script") ||
                        rest[..8].eq_ignore_ascii_case("</style") ||
                        rest[..5].eq_ignore_ascii_case("</nav") ||
                        rest[..9].eq_ignore_ascii_case("</header") ||
                        rest[..9].eq_ignore_ascii_case("</footer") ||
                        rest[..11].eq_ignore_ascii_case("</noscript")
                    );

                    if is_skip_open {
                        skip_depth += 1;
                        // Skip to end of tag
                        while i < bytes.len() && bytes[i] != b'>' { i += 1; }
                        i += 1;
                        continue;
                    }
                    if is_skip_close {
                        skip_depth -= 1;
                        if skip_depth < 0 { skip_depth = 0; }
                        while i < bytes.len() && bytes[i] != b'>' { i += 1; }
                        i += 1;
                        continue;
                    }

                    if skip_depth > 0 {
                        i += 1;
                        continue;
                    }

                    // Strip all other HTML tags but keep text content
                    while i < bytes.len() && bytes[i] != b'>' { i += 1; }
                    i += 1;
                    // Add space to separate tag content
                    if !cleaned.is_empty() && !cleaned.ends_with(' ') && !cleaned.ends_with('\n') {
                        cleaned.push(' ');
                    }
                } else if skip_depth == 0 {
                    cleaned.push(bytes[i] as char);
                    i += 1;
                } else {
                    i += 1;
                }
            }

            // Trim excessive whitespace
            let trimmed: String = {
                let mut result = String::with_capacity(cleaned.len());
                let mut last_was_space = false;
                for ch in cleaned.chars() {
                    if ch == '\n' || ch == '\r' || ch == '\t' {
                        if !last_was_space {
                            result.push('\n');
                            last_was_space = true;
                        }
                    } else if ch == ' ' {
                        if !last_was_space {
                            result.push(' ');
                            last_was_space = true;
                        }
                    } else {
                        result.push(ch);
                        last_was_space = false;
                    }
                }
                result
            };

            // Truncate to fit in gemini prompt (keep first ~3KB of clean text)
            let text_for_llm = if trimmed.len() > 3000 { &trimmed[..3000] } else { &trimmed };

            // Build semantic extraction prompt
            let prompt = alloc::format!(
                "__SLM_GENERATE__You are a semantic web browser. Extract the core information from this raw web page text. Remove all boilerplate, menus, ads, and navigation. Format the pure content as clean readable text with headings marked by '# '. Keep it concise.\n\nPage text:\n{}",
                text_for_llm
            );

            let gemini_buf_size = (max_len as usize).max(8192);
            let mut response = alloc::vec![0u8; gemini_buf_size];
            let bytes = libfolk::sys::ask_gemini(&prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize).min(gemini_buf_size);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );
}
