//! IPC handler implementations: synchronous (`handle_inference_request`),
//! async streaming (`handle_async_inference`), and shared memory reply
//! helper (`send_text_response`).

use libfolk::println;
use libfolk::sys::yield_cpu;
use libfolk::sys::ipc::{reply_with_token, CallerToken};
use libfolk::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};
use libtensor::arena::BumpArena;
use libtensor::tokenizer::BpeTokenizer;
use libtensor::transformer::{forward, YieldConfig};

use crate::chat::{tcg_breathe, wrap_chat_template};
use crate::config::read_control_sector;
use crate::consts::{
    DUMP_MAX_FLOATS, INFER_SHMEM_VADDR, KV_WINDOW_SIZE, MAX_GEN_TOKENS, RING_SHMEM_VADDR,
};
use crate::debug::{debug_dump_logits, debug_dump_tensor, write_health_sector};
use crate::inference::{build_weights_for_forward, InferenceEngine};
use crate::sampling::sample_with_penalties;
use crate::stream::{TokenRing, RING_DATA_MAX};

/// Handle an inference or ask request.
///
/// When engine is available: tokenize → prefill → generate → respond.
/// ULTRA 28: Sends IPC notification per token for streaming display.
/// ULTRA 30: TCG breathing room between layers.
/// ULTRA 31: Logit clamping and NaN sanitization.
/// ULTRA 33: Repetition penalty + Top-P sampling.
pub fn handle_inference_request(
    token: CallerToken,
    input_shmem: u32,
    input_len: usize,
    _is_rag: bool,
    engine: &mut Option<InferenceEngine>,
    arena: &BumpArena,
) {
    println!("[INFERENCE] IPC received: shmem={} len={} is_rag={}", input_shmem, input_len, _is_rag);

    if engine.is_none() {
        // Stub mode: return informative message
        send_text_response(token, b"[AI] No model loaded. Pack a GGUF model to enable inference.");
        return;
    }

    // Read prompt from input shmem
    let mut prompt_buf = [0u8; 1024];
    let mut prompt_len = 0usize;

    println!("[INFERENCE] Mapping input shmem {} at 0x{:X}", input_shmem, INFER_SHMEM_VADDR);
    if input_shmem > 0 && input_len > 0 {
        match shmem_map(input_shmem, INFER_SHMEM_VADDR) {
            Ok(()) => {
                let copy_len = input_len.min(prompt_buf.len());
                unsafe {
                    let src = INFER_SHMEM_VADDR as *const u8;
                    core::ptr::copy_nonoverlapping(src, prompt_buf.as_mut_ptr(), copy_len);
                }
                prompt_len = copy_len;
                let _ = shmem_unmap(input_shmem, INFER_SHMEM_VADDR);
                println!("[INFERENCE] Read {} bytes from shmem", prompt_len);
            }
            Err(_) => {
                println!("[INFERENCE] shmem_map FAILED for handle {}", input_shmem);
            }
        }
    }

    if prompt_len == 0 {
        println!("[INFERENCE] Empty prompt, sending stub response");
        send_text_response(token, b"[AI] Empty prompt.");
        return;
    }

    if let Ok(text) = core::str::from_utf8(&prompt_buf[..prompt_len]) {
        println!("[INFERENCE] Query: {}", text);
    } else {
        println!("[INFERENCE] Query: ({} raw bytes)", prompt_len);
    }

    // Wrap in ChatML template (ULTRA 41: system prompt injection)
    let mut template_buf = [0u8; 2048];
    let template_len = wrap_chat_template(&prompt_buf[..prompt_len], &mut template_buf);
    if template_len > 0 {
        println!("[INFERENCE] Chat template wrapped: {} bytes", template_len);
        prompt_buf[..template_len].copy_from_slice(&template_buf[..template_len]);
        prompt_len = template_len;
    }

    let eng = engine.as_mut().unwrap();
    println!("[INFERENCE] Resetting KV-cache, building tokenizer...");

    // Reset KV-cache for new conversation
    eng.kv_cache.reset();

    // Rebuild tokenizer (needs arena for offset/length tables)
    arena.reset();

    let tokenizer = match BpeTokenizer::new(
        eng.model_data,
        eng.vocab_offset,
        eng.vocab_size,
        eng.bos_id,
        eng.eos_id,
        eng.merges_offset,
        eng.merges_count,
        eng.unknown_token_id,
        eng.token_type_offset,
        arena,
    ) {
        Some(t) => t,
        None => {
            send_text_response(token, b"[AI] Tokenizer init failed.");
            return;
        }
    };

    // Save arena position after tokenizer init — reset_to this mark
    // so tokenizer offset/length tables are preserved across forward passes
    let arena_mark = arena.used();

    // Tokenize the prompt (starts with <|im_start|> = BOS, no extra prepend needed)
    let mut input_tokens = [0u32; 512];
    let total_prompt = tokenizer.encode(&prompt_buf[..prompt_len], &mut input_tokens);

    println!("[INFERENCE] Tokenized: {} tokens", total_prompt);

    // Build LayerWeights slice for transformer::forward
    let yield_cfg = YieldConfig::foreground();
    let cfg = read_control_sector();

    // Allocate response buffer (in a separate region)
    let mut response_buf = [0u8; 4096];
    let mut response_len = 0usize;

    // Track generated tokens for repetition penalty (ULTRA 33)
    let mut gen_tokens = [0u32; 512];
    let mut gen_count = 0usize;

    // === Prefill Phase ===
    // Process all prompt tokens through the model
    println!("[INFERENCE] Prefill: {} tokens", total_prompt);

    let mut last_logits_token: u32 = 0;

    for i in 0..total_prompt {
        arena.reset_to(arena_mark);

        // Build weights for this forward pass
        let (weights, _) = match build_weights_for_forward(eng, arena) {
            Some(w) => w,
            None => {
                send_text_response(token, b"[AI] Failed to build weights for forward pass.");
                return;
            }
        };

        let logits = match forward(
            input_tokens[i], i, &eng.config, &weights, &mut eng.kv_cache, arena, &yield_cfg,
            None,
        ) {
            Some(l) => l,
            None => {
                println!("[INFERENCE] Forward pass failed at prefill token {}", i);
                send_text_response(token, b"[AI] Forward pass failed during prefill.");
                return;
            }
        };

        // On the last prefill token, we need the logits for generation
        if i == total_prompt - 1 {
            debug_dump_logits(logits, "prefill_final_logits");

            // Phase B3: pass pre-allocated logits_buf instead of arena
            last_logits_token = sample_with_penalties(logits, &gen_tokens[..gen_count], eng.logits_buf, &cfg);
        }

        if i == 0 {
            debug_dump_logits(logits, "bos_logits");
        }

        // ULTRA 28: yield periodically during prefill
        if i % 4 == 0 {
            yield_cpu();
        }

        // ULTRA 30: TCG breathing room
        tcg_breathe();
    }

    println!("[INFERENCE] Prefill done, generating...");

    // === Generation Phase ===
    let mut pos = total_prompt;

    for gen_idx in 0..MAX_GEN_TOKENS {
        let next_token = if gen_idx == 0 {
            last_logits_token
        } else {
            // KV-cache overflow guard
            if pos >= KV_WINDOW_SIZE {
                println!("[INFERENCE] Context limit: pos={} >= KV={}", pos, KV_WINDOW_SIZE);
                break;
            }

            arena.reset_to(arena_mark);

            let (weights, _) = match build_weights_for_forward(eng, arena) {
                Some(w) => w,
                None => break,
            };

            let logits = match forward(
                gen_tokens[gen_count - 1], pos - 1, &eng.config, &weights, &mut eng.kv_cache, arena, &yield_cfg,
                None,
            ) {
                Some(l) => l,
                None => {
                    println!("[INFERENCE] Forward pass failed at gen token {}", gen_idx);
                    break;
                }
            };

            sample_with_penalties(logits, &gen_tokens[..gen_count], eng.logits_buf, &cfg)
        };

        // Check for EOS or ChatML stop tokens (ULTRA 39)
        if next_token == eng.eos_id
            || (eng.im_end_id != u32::MAX && next_token == eng.im_end_id)
            || (eng.im_start_id != u32::MAX && next_token == eng.im_start_id)
        {
            println!("[INFERENCE] Stop token {} at gen {}", next_token, gen_idx);
            break;
        }

        // Track for repetition penalty
        if gen_count < gen_tokens.len() {
            gen_tokens[gen_count] = next_token;
            gen_count += 1;
        }

        // Decode token to text
        let mut tok_buf = [0u8; 64];
        let tok_len = tokenizer.decode_token(next_token, &mut tok_buf);

        // Append to response
        if response_len + tok_len < response_buf.len() {
            response_buf[response_len..response_len + tok_len].copy_from_slice(&tok_buf[..tok_len]);
            response_len += tok_len;
        }

        pos += 1;

        // ULTRA 28: yield after each generated token
        yield_cpu();

        // ULTRA 30: TCG breathing room
        tcg_breathe();

        // Log progress periodically
        if gen_idx % 8 == 0 {
            println!("[INFERENCE] Generated {} tokens...", gen_idx + 1);
        }
    }

    println!("[INFERENCE] Generation complete: {} tokens, {} bytes", gen_count, response_len);

    // Send response
    if response_len > 0 {
        send_text_response(token, &response_buf[..response_len]);
    } else {
        send_text_response(token, b"[AI] (empty response)");
    }
}

/// Handle async inference request with token streaming via TokenRing.
/// ULTRA 42: Rejects if already generating.
/// ULTRA 37: Atomic writes to TokenRing.
/// ULTRA 47: Only writes valid UTF-8 to ring.
/// ULTRA 48: Graceful truncation at ring buffer limit.
pub fn handle_async_inference(
    token: CallerToken,
    query_shmem: u32,
    query_len: usize,
    ring_shmem: u32,
    engine: &mut Option<InferenceEngine>,
    arena: &BumpArena,
) {
    use core::sync::atomic::Ordering;

    // ULTRA 42: Reentrancy guard
    if let Some(eng) = engine.as_ref() {
        if eng.is_generating {
            println!("[INFERENCE] BUSY — rejecting async request");
            let _ = reply_with_token(token, u64::MAX, 0);
            return;
        }
    }

    if engine.is_none() {
        let _ = reply_with_token(token, u64::MAX, 0);
        return;
    }

    // Reply immediately to free compositor (0 = OK)
    let _ = reply_with_token(token, 0, 0);

    let eng = engine.as_mut().unwrap();
    eng.is_generating = true;

    // Read query from shmem
    let mut prompt_buf = [0u8; 1024];
    let mut prompt_len = 0usize;

    if query_shmem > 0 && query_len > 0 {
        match shmem_map(query_shmem, INFER_SHMEM_VADDR) {
            Ok(()) => {
                let copy_len = query_len.min(prompt_buf.len());
                unsafe {
                    let src = INFER_SHMEM_VADDR as *const u8;
                    core::ptr::copy_nonoverlapping(src, prompt_buf.as_mut_ptr(), copy_len);
                }
                prompt_len = copy_len;
                let _ = shmem_unmap(query_shmem, INFER_SHMEM_VADDR);
            }
            Err(_) => {
                println!("[INFERENCE] async: shmem_map FAILED for query");
            }
        }
    }

    if prompt_len == 0 {
        eng.is_generating = false;
        return;
    }

    if let Ok(text) = core::str::from_utf8(&prompt_buf[..prompt_len]) {
        println!("[INFERENCE] Async query: {}", text);
    }

    // Wrap in ChatML template (ULTRA 41)
    let mut template_buf = [0u8; 2048];
    let template_len = wrap_chat_template(&prompt_buf[..prompt_len], &mut template_buf);
    if template_len > 0 {
        prompt_buf[..template_len].copy_from_slice(&template_buf[..template_len]);
        prompt_len = template_len;
    }

    // Map TokenRing shmem (ULTRA 43: at 0x22000000)
    if shmem_map(ring_shmem, RING_SHMEM_VADDR).is_err() {
        println!("[INFERENCE] async: ring shmem_map FAILED");
        eng.is_generating = false;
        return;
    }

    let ring = unsafe { &*(RING_SHMEM_VADDR as *mut TokenRing) };
    // Initialize ring
    ring.write_idx.store(0, Ordering::Release);
    ring.status.store(0, Ordering::Release);

    // Reset KV-cache and rebuild tokenizer
    eng.kv_cache.reset();
    arena.reset();

    let tokenizer = match BpeTokenizer::new(
        eng.model_data, eng.vocab_offset, eng.vocab_size,
        eng.bos_id, eng.eos_id, eng.merges_offset, eng.merges_count,
        eng.unknown_token_id, eng.token_type_offset, arena,
    ) {
        Some(t) => t,
        None => {
            ring.status.store(2, Ordering::Release);
            let _ = shmem_unmap(ring_shmem, RING_SHMEM_VADDR);
            eng.is_generating = false;
            return;
        }
    };

    let _arena_mark = arena.used();

    // Read control sector for sampling params + dump layer
    let cfg = read_control_sector();
    println!("[INFERENCE] Config: temp={:.2} top_p={:.2} top_k={} rep={:.2} dump_layer={}",
        cfg.temperature, cfg.top_p, cfg.top_k, cfg.rep_penalty, cfg.dump_layer);

    // Tokenize prompt with chat template
    let mut input_tokens = [0u32; 512];
    let total_prompt = tokenizer.encode(&prompt_buf[..prompt_len], &mut input_tokens);
    println!("[INFERENCE] Async tokenized: {} tokens", total_prompt);
    // Debug: print first 10 input token IDs
    if total_prompt >= 10 {
        println!("[TOKENS] {} {} {} {} {} {} {} {} {} {}",
            input_tokens[0], input_tokens[1], input_tokens[2], input_tokens[3],
            input_tokens[4], input_tokens[5], input_tokens[6], input_tokens[7],
            input_tokens[8], input_tokens[9]);
    }

    let yield_cfg = YieldConfig::foreground();

    let mut gen_tokens = [0u32; 512];
    let mut gen_count = 0usize;
    let mut last_logits_token: u32;

    // Attention dump: capture layer 0 attention weights during prefill
    // Buffer layout: [n_heads, total_prompt, total_prompt] = n_heads * seq^2 floats
    // Dump layer comes from control sector (cfg.dump_layer, default 0)
    let attn_buf_size = eng.config.n_heads * total_prompt * total_prompt;
    let attn_buf_fits = attn_buf_size <= DUMP_MAX_FLOATS; // fits in 128KB mailbox?
    // Allocate from arena BEFORE arena_mark so it persists across forward calls
    let mut attn_buf = if attn_buf_fits {
        arena.alloc_f32(attn_buf_size)
    } else {
        None
    };
    let arena_mark2 = arena.used(); // new mark after attn buffer

    // Prefill — 3-Phase Batched Architecture
    // Process prompt in batches of 8 for L2 cache reuse, then single-token
    // forward for the last token to get logits for sampling.
    const PREFILL_BATCH: usize = 8;

    if total_prompt > 1 {
        // Batched prefill for all tokens except the last
        let batch_end = total_prompt - 1; // last token needs logits
        let mut pos = 0;
        while pos < batch_end {
            arena.reset_to(arena_mark2);
            let (weights, _) = match build_weights_for_forward(eng, arena) {
                Some(w) => w,
                None => {
                    ring.status.store(2, Ordering::Release);
                    let _ = shmem_unmap(ring_shmem, RING_SHMEM_VADDR);
                    eng.is_generating = false;
                    return;
                }
            };

            let chunk_end = (pos + PREFILL_BATCH).min(batch_end);
            let chunk = &input_tokens[pos..chunk_end];

            use libtensor::transformer::forward_prefill_batch;
            if forward_prefill_batch(chunk, pos, &eng.config, &weights, &mut eng.kv_cache, arena, &yield_cfg).is_none() {
                ring.status.store(2, Ordering::Release);
                let _ = shmem_unmap(ring_shmem, RING_SHMEM_VADDR);
                eng.is_generating = false;
                return;
            }
            pos = chunk_end;
            tcg_breathe();
        }
    }

    // Final token: single-token forward to get logits for sampling
    {
        arena.reset_to(arena_mark2);
        let (weights, _) = match build_weights_for_forward(eng, arena) {
            Some(w) => w,
            None => {
                ring.status.store(2, Ordering::Release);
                let _ = shmem_unmap(ring_shmem, RING_SHMEM_VADDR);
                eng.is_generating = false;
                return;
            }
        };

        let last_idx = total_prompt - 1;
        let mut attn_dump_obj = attn_buf.as_deref_mut().map(|buf| {
            use libtensor::transformer::AttnDump;
            AttnDump {
                buffer: buf,
                dump_layer: cfg.dump_layer,
                n_heads: eng.config.n_heads,
                max_seq: total_prompt,
            }
        });

        let logits = match forward(
            input_tokens[last_idx], last_idx, &eng.config, &weights, &mut eng.kv_cache, arena, &yield_cfg,
            attn_dump_obj.as_mut(),
        ) {
            Some(l) => l,
            None => {
                ring.status.store(2, Ordering::Release);
                let _ = shmem_unmap(ring_shmem, RING_SHMEM_VADDR);
                eng.is_generating = false;
                return;
            }
        };
        last_logits_token = sample_with_penalties(logits, &gen_tokens[..gen_count], eng.logits_buf, &cfg);
    }

    // Dump attention weights to disk mailbox
    if let Some(ref buf) = attn_buf {
        debug_dump_tensor(
            "attn_layer0",
            &buf[..attn_buf_size],
            (eng.config.n_heads * total_prompt) as u32,
            total_prompt as u32,
        );
        println!("[INFERENCE] Attention dumped: layer {} ({} heads, {} seq)", cfg.dump_layer, eng.config.n_heads, total_prompt);
    }

    println!("[INFERENCE] Async prefill done, streaming tokens...");

    // Allocate prev_logits for MSE health monitoring (persists across gen loop)
    let mut prev_logits = if cfg.telemetry_mode > 0 {
        arena.alloc_f32(eng.config.vocab_size)
    } else {
        None
    };
    let mut has_prev_logits = false;
    let mut min_mse: f32 = f32::MAX;    // "Check Engine" light: worst (lowest) MSE
    let mut min_mse_step: u32 = 0;
    let mut health_gen_count: u32 = 0;
    let arena_mark_gen = arena.used();   // mark AFTER prev_logits

    // Tool close-tag detection: tail-buffer tracks last 9 bytes written
    const TOOL_CLOSE_TAG: [u8; 9] = *b"<|/tool|>";
    let mut tail_buf = [0u8; 9];
    let mut tail_pos: usize = 0;

    // Generation with streaming to TokenRing
    let mut pos = total_prompt;
    let mut write_idx: usize = 0;

    for gen_idx in 0..MAX_GEN_TOKENS {
        let next_token = if gen_idx == 0 {
            last_logits_token
        } else {
            // KV-cache overflow guard: prevent OOB in attention forward pass
            if pos >= KV_WINDOW_SIZE {
                let msg = b"\n[Context Limit Reached]";
                if write_idx + msg.len() < RING_DATA_MAX {
                    unsafe {
                        let dst = (RING_SHMEM_VADDR as *mut u8).add(16).add(write_idx);
                        core::ptr::copy_nonoverlapping(msg.as_ptr(), dst, msg.len());
                    }
                    write_idx += msg.len();
                    ring.write_idx.store(write_idx as u32, Ordering::Release);
                }
                println!("[INFERENCE] Context limit: pos={} >= KV={}", pos, KV_WINDOW_SIZE);
                break;
            }

            arena.reset_to(arena_mark_gen);
            let (weights, _) = match build_weights_for_forward(eng, arena) {
                Some(w) => w,
                None => break,
            };
            let logits = match forward(
                gen_tokens[gen_count - 1], pos - 1, &eng.config, &weights, &mut eng.kv_cache, arena, &yield_cfg,
                None,
            ) {
                Some(l) => l,
                None => break,
            };

            // Health monitoring: MSE between consecutive logits
            if let Some(ref mut prev) = prev_logits {
                if has_prev_logits {
                    let mut mse_sum = 0.0f64;
                    for j in 0..eng.config.vocab_size {
                        let d = (logits[j] - prev[j]) as f64;
                        mse_sum += d * d;
                    }
                    let mse = (mse_sum / eng.config.vocab_size as f64) as f32;
                    health_gen_count = gen_idx as u32;

                    // Track minimum MSE (worst collapse point)
                    if mse < min_mse {
                        min_mse = mse;
                        min_mse_step = gen_idx as u32;
                    }

                    // Serial warning on collapse (both modes)
                    if mse < cfg.drift_threshold {
                        println!("[HEALTH] Logit collapse! MSE={:.6} gen={}", mse, gen_idx);
                    }

                    // Disk write depends on mode
                    match cfg.telemetry_mode {
                        1 => {
                            // Anomalies only: write only if below threshold
                            if mse < cfg.drift_threshold {
                                write_health_sector(gen_idx as u32, gen_tokens[gen_count - 1], mse,
                                    cfg.drift_threshold, min_mse, min_mse_step, health_gen_count);
                            }
                        }
                        _ => {
                            // Continuous: always write
                            write_health_sector(gen_idx as u32, gen_tokens[gen_count - 1], mse,
                                cfg.drift_threshold, min_mse, min_mse_step, health_gen_count);
                        }
                    }
                }
                prev[..eng.config.vocab_size].copy_from_slice(logits);
                has_prev_logits = true;
            }

            sample_with_penalties(logits, &gen_tokens[..gen_count], eng.logits_buf, &cfg)
        };

        // Check for stop tokens (ULTRA 39)
        if next_token == eng.eos_id
            || (eng.im_end_id != u32::MAX && next_token == eng.im_end_id)
            || (eng.im_start_id != u32::MAX && next_token == eng.im_start_id)
        {
            println!("[INFERENCE] Async stop token {} at gen {}", next_token, gen_idx);
            break;
        }

        if gen_count < gen_tokens.len() {
            gen_tokens[gen_count] = next_token;
            gen_count += 1;
        }

        // Decode token to bytes
        let mut tok_buf = [0u8; 64];
        let tok_len = tokenizer.decode_token(next_token, &mut tok_buf);

        if tok_len > 0 {
            // ULTRA 48: Check ring buffer space before writing
            if write_idx + tok_len >= RING_DATA_MAX {
                println!("[INFERENCE] Ring buffer full at {} bytes", write_idx);
                break;
            }
            // Write decoded bytes directly to ring
            // Compositor uses from_utf8().unwrap_or("") for safe rendering (ULTRA 45)
            unsafe {
                let dst = (RING_SHMEM_VADDR as *mut u8)
                    .add(16) // skip header (write_idx + status + _pad)
                    .add(write_idx);
                core::ptr::copy_nonoverlapping(
                    tok_buf.as_ptr(), dst, tok_len
                );
            }
            write_idx += tok_len;
            // ULTRA 37: Release ordering so compositor sees data before updated index
            ring.write_idx.store(write_idx as u32, Ordering::Release);

            // Track last 9 bytes for <|/tool|> detection
            for &b in &tok_buf[..tok_len] {
                if tail_pos < 9 {
                    tail_buf[tail_pos] = b;
                    tail_pos += 1;
                } else {
                    tail_buf.copy_within(1..9, 0);
                    tail_buf[8] = b;
                }
            }

            // Check if we just completed a tool close tag
            if tail_pos >= 9 && tail_buf == TOOL_CLOSE_TAG {
                println!("[INFERENCE] Tool call detected at pos={}, pausing...", pos);
                ring.tool_state.store(1, Ordering::Release); // 1 = paused

                // Wait for compositor to execute tool and provide result
                let mut wait = 0u32;
                loop {
                    if ring.tool_state.load(Ordering::Acquire) == 2 { break; }
                    yield_cpu();
                    wait += 1;
                    if wait > 500_000 {
                        println!("[INFERENCE] Tool result timeout after ~10s");
                        ring.tool_state.store(0, Ordering::Release);
                        break;
                    }
                }

                if ring.tool_state.load(Ordering::Acquire) == 2 {
                    // Read tool result from ring
                    let result_len = ring.tool_result_len.load(Ordering::Acquire) as usize;
                    let available = RING_DATA_MAX.saturating_sub(write_idx);
                    let safe_len = result_len.min(available);

                    if safe_len > 0 {
                        let result_bytes = unsafe {
                            core::slice::from_raw_parts(
                                (RING_SHMEM_VADDR as *const u8).add(16).add(write_idx),
                                safe_len,
                            )
                        };

                        // Tokenize result for KV-cache injection
                        let mut result_tokens = [0u32; 256];
                        let n_result = tokenizer.encode(result_bytes, &mut result_tokens);

                        if n_result > 0 {
                            // Prefill all but last token
                            if n_result > 1 {
                                arena.reset_to(arena_mark_gen);
                                if let Some((weights, _)) = build_weights_for_forward(eng, arena) {
                                    use libtensor::transformer::forward_prefill_batch;
                                    let _ = forward_prefill_batch(
                                        &result_tokens[..n_result - 1], pos, &eng.config, &weights,
                                        &mut eng.kv_cache, arena, &yield_cfg,
                                    );
                                    pos += n_result - 1;
                                }
                            }

                            // Forward last token for fresh logits
                            arena.reset_to(arena_mark_gen);
                            if let Some((weights, _)) = build_weights_for_forward(eng, arena) {
                                if let Some(logits) = forward(
                                    result_tokens[n_result - 1], pos, &eng.config, &weights,
                                    &mut eng.kv_cache, arena, &yield_cfg, None,
                                ) {
                                    pos += 1;
                                    last_logits_token = sample_with_penalties(
                                        logits, &gen_tokens[..gen_count], eng.logits_buf, &cfg,
                                    );
                                    println!("[INFERENCE] Tool result injected: {} tokens, pos={}", n_result, pos);
                                }
                            }
                        }

                        // Advance write_idx past the result so compositor sees it
                        write_idx += safe_len;
                        ring.write_idx.store(write_idx as u32, Ordering::Release);
                    }

                    ring.tool_state.store(0, Ordering::Release);
                }
                tail_pos = 0;
                // Skip normal forward — we already have fresh logits from tool injection
                continue;
            }
        }

        pos += 1;
        yield_cpu();
        tcg_breathe();

        if gen_idx % 8 == 0 {
            println!("[INFERENCE] Async gen {} tokens, {} bytes streamed", gen_idx + 1, write_idx);
        }
        // Debug: log first 16 tokens as token_id + decoded text
        if gen_idx < 16 {
            let preview = core::str::from_utf8(&tok_buf[..tok_len]).unwrap_or("?");
            println!("[TOKEN] #{} id={} len={} {:?}", gen_idx, next_token, tok_len, preview);
        }
    }

    // Final health snapshot: write min_mse summary so Python reads the worst point
    if cfg.telemetry_mode > 0 && health_gen_count > 0 {
        write_health_sector(health_gen_count, 0, min_mse, cfg.drift_threshold,
            min_mse, min_mse_step, health_gen_count);
    }

    // Mark done
    ring.status.store(1, Ordering::Release);
    let _ = shmem_unmap(ring_shmem, RING_SHMEM_VADDR);
    eng.is_generating = false;

    println!("[INFERENCE] Async generation complete: {} tokens, {} bytes", gen_count, write_idx);
}

/// Send a text response via shmem IPC.
pub fn send_text_response(token: CallerToken, data: &[u8]) {
    match shmem_create(4096) {
        Ok(out_handle) => {
            if shmem_map(out_handle, INFER_SHMEM_VADDR).is_ok() {
                let copy_len = data.len().min(4096);
                unsafe {
                    let ptr = INFER_SHMEM_VADDR as *mut u8;
                    core::ptr::copy_nonoverlapping(data.as_ptr(), ptr, copy_len);
                }
                let _ = shmem_unmap(out_handle, INFER_SHMEM_VADDR);
                let _ = shmem_grant(out_handle, 3); // shell
                let _ = shmem_grant(out_handle, 4); // compositor

                let reply_val = ((copy_len as u64) << 32) | (out_handle as u64);
                let _ = reply_with_token(token, reply_val, 0);
                return;
            }
            let _ = shmem_destroy(out_handle);
        }
        Err(_) => {}
    }

    let _ = reply_with_token(token, 0, 0);
}
