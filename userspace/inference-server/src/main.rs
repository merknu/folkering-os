//! Inference Server — Task 6 for Folkering OS
//!
//! Loads a GGUF model from VirtIO disk via mmap, runs transformer
//! inference, and serves requests via IPC.
//!
//! Boot sequence:
//! 1. Detect CPU features (AVX2, etc.)
//! 2. Initialize bump arena (8MB)
//! 3. Read FOLKDISK header for model location
//! 4. Mmap model data (zero-copy)
//! 5. Parse GGUF → zero-copy tensor views
//! 6. Build ModelWeights from GGUF tensors
//! 7. Initialize BPE tokenizer from GGUF vocab
//! 8. Allocate KV-cache
//! 9. Enter IPC service loop with full inference

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};
use libfolk::sys::ipc::{recv_async, reply_with_token, CallerToken};
use libfolk::sys::memory::{mmap_at, PROT_READ, PROT_WRITE};
use libfolk::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};
use libfolk::sys::block::{block_read, read_sector, SECTOR_SIZE, DATA_START_SECTOR};
use libfolk::sys::random::random_u32;
use libtensor::arena::BumpArena;
use libtensor::simd;
use libtensor::gguf::{GgufModel, GgufError};
use libtensor::transformer::{ModelConfig, ModelWeights, LayerWeights, YieldConfig, forward, argmax};
use libtensor::kv_cache::KvCacheManager;
use libtensor::tokenizer::BpeTokenizer;

entry!(main);

/// Inference server task ID (must match kernel spawn order)
pub const INFERENCE_TASK_ID: u32 = 6;

/// IPC opcodes for inference requests (must match libfolk::sys::inference)
pub const INFER_OP_PING: u64 = 0;
pub const INFER_OP_GENERATE: u64 = 1;
pub const INFER_OP_STATUS: u64 = 2;
pub const INFER_OP_ASK: u64 = 3;

/// Bump arena size: 8MB for intermediate computation buffers
const ARENA_SIZE: usize = 8 * 1024 * 1024;

/// Maximum GGUF model size we'll attempt to load (128MB)
const MAX_MODEL_SIZE: usize = 128 * 1024 * 1024;

/// Virtual address for model mmap region
const MODEL_MMAP_BASE: usize = 0x1_0000_0000;

/// Virtual address for mapping request/response shmem
/// Must not overlap with MMAP_BASE (0x4000_0000) region used by arena/KV-cache
const INFER_SHMEM_VADDR: usize = 0x20000000;

/// Maximum tokens to generate per request
const MAX_GEN_TOKENS: usize = 64;

/// KV-cache window size (power of 2)
const KV_WINDOW_SIZE: usize = 256;

/// Temperature for sampling (ULTRA 33)
const TEMPERATURE: f32 = 0.8;

/// Repetition penalty factor (ULTRA 33)
const REP_PENALTY: f32 = 1.15;

/// Top-P nucleus sampling threshold (ULTRA 33)
const TOP_P: f32 = 0.9;

/// Number of recent tokens to apply repetition penalty to
const REP_WINDOW: usize = 32;

fn main() -> ! {
    println!("[INFERENCE] === ENTRY ===");
    let pid = get_pid();
    println!("[INFERENCE] Inference Server starting (Task {})", pid);

    // Step 1: Detect CPU features
    simd::detect_cpu_features();
    let has_avx2 = simd::has_avx2();
    println!("[INFERENCE] CPU features: AVX2={}", if has_avx2 { "yes" } else { "no" });

    // Step 2: Initialize bump arena
    let mut arena = BumpArena::uninit();
    match arena.init_mmap(ARENA_SIZE) {
        Ok(()) => println!("[INFERENCE] Arena allocated: {}KB", ARENA_SIZE / 1024),
        Err(()) => {
            println!("[INFERENCE] ERROR: Failed to allocate arena");
            loop { yield_cpu(); }
        }
    }

    // Step 3: Try to load model from VirtIO disk
    let model_result = load_model_from_disk();
    let mut has_model = false;
    let mut engine: Option<InferenceEngine> = None;

    match model_result {
        Ok((base_ptr, model_size)) => {
            println!("[INFERENCE] Model mmap'd at 0x{:X}, {}KB", base_ptr as usize, model_size / 1024);

            // Step 4: Parse GGUF
            // Safety: mmap'd data lives for entire process lifetime
            let model_slice: &'static [u8] = unsafe {
                core::slice::from_raw_parts(base_ptr, model_size)
            };

            match GgufModel::parse(model_slice) {
                Ok(model) => {
                    let meta = &model.metadata;
                    println!("[INFERENCE] Model: {} ({} layers, {} dim, {} heads)",
                        meta.architecture.as_str(),
                        meta.n_layers,
                        meta.embedding_dim,
                        meta.n_heads,
                    );
                    println!("[INFERENCE] Vocab: {}, Context: {}, Tensors: {}",
                        meta.vocab_size,
                        meta.context_length,
                        model.tensors.len(),
                    );
                    println!("[INFERENCE] BOS={}, EOS={}, vocab_offset={}",
                        meta.bos_token_id, meta.eos_token_id, meta.vocab_data_offset);

                    // Step 5: Build ModelWeights
                    match build_model_weights(&model) {
                        Some((config, weights_data, layer_data)) => {
                            println!("[INFERENCE] ModelWeights built: {} layers mapped", config.n_layers);

                            // Step 6: Build tokenizer (uses arena for offset table)
                            let tokenizer = BpeTokenizer::new(
                                model_slice,
                                meta.vocab_data_offset,
                                meta.vocab_size as usize,
                                meta.bos_token_id,
                                meta.eos_token_id,
                                &arena,
                            );

                            match tokenizer {
                                Some(tok) => {
                                    println!("[INFERENCE] Tokenizer initialized (vocab={})", tok.vocab_size());

                                    // Step 7: Allocate KV-cache
                                    let kv_result = unsafe {
                                        KvCacheManager::new(
                                            config.n_layers,
                                            config.n_kv_heads,
                                            config.head_dim,
                                            KV_WINDOW_SIZE,
                                        )
                                    };

                                    match kv_result {
                                        Ok(kv_cache) => {
                                            let kv_bytes = libtensor::kv_cache::KvCache::required_bytes(
                                                config.n_kv_heads, config.head_dim, KV_WINDOW_SIZE
                                            ) * config.n_layers;
                                            println!("[INFERENCE] KV-cache allocated: {}KB ({} layers × {}KB)",
                                                kv_bytes / 1024, config.n_layers, kv_bytes / config.n_layers / 1024);

                                            has_model = true;

                                            // Store engine state — we transmute lifetimes to 'static
                                            // since the mmap'd data lives for the entire process
                                            engine = Some(InferenceEngine {
                                                config,
                                                weights_data,
                                                layer_data,
                                                kv_cache,
                                                model_data: model_slice,
                                                vocab_offset: meta.vocab_data_offset,
                                                vocab_size: meta.vocab_size as usize,
                                                bos_id: meta.bos_token_id,
                                                eos_id: meta.eos_token_id,
                                            });

                                            println!("[INFERENCE] Model loaded successfully! Ready for inference.");
                                        }
                                        Err(()) => {
                                            println!("[INFERENCE] ERROR: KV-cache allocation failed");
                                        }
                                    }
                                }
                                None => {
                                    println!("[INFERENCE] WARNING: Tokenizer init failed (no vocab data?)");
                                }
                            }
                        }
                        None => {
                            println!("[INFERENCE] ERROR: Failed to map GGUF tensors to ModelWeights");
                        }
                    }
                }
                Err(e) => {
                    println!("[INFERENCE] GGUF parse error: {:?}", gguf_error_str(e));
                }
            }
        }
        Err(e) => {
            println!("[INFERENCE] No model found on disk ({}), running in stub mode", e);
        }
    }

    println!("[INFERENCE] Entering IPC service loop");

    // Step 8: IPC service loop
    loop {
        match recv_async() {
            Ok(msg) => {
                // Decode packed request: opcode in low 16 bits
                let opcode = msg.payload0 & 0xFFFF;
                let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
                let data_len = ((msg.payload0 >> 32) & 0xFFFFFFFF) as usize;

                match opcode {
                    INFER_OP_PING => {
                        let status = if has_model { 1u64 } else { 0u64 };
                        let _ = reply_with_token(msg.token, status, 0);
                    }
                    INFER_OP_STATUS => {
                        let has_model_u = if has_model { 1u64 } else { 0u64 };
                        let _ = reply_with_token(msg.token, has_model_u, ARENA_SIZE as u64);
                    }
                    INFER_OP_GENERATE | INFER_OP_ASK => {
                        handle_inference_request(
                            msg.token, shmem_handle, data_len,
                            opcode == INFER_OP_ASK, &mut engine, &arena,
                        );
                    }
                    _ => {
                        let _ = reply_with_token(msg.token, u64::MAX, 0);
                    }
                }
            }
            Err(_) => {
                yield_cpu();
            }
        }
    }
}

// ============================================================================
// Inference Engine State
// ============================================================================

/// Holds all state needed for inference across IPC requests.
///
/// All `&[u8]` references point into the mmap'd model data which lives
/// for the entire process lifetime, so we use 'static.
struct InferenceEngine {
    config: ModelConfig,
    weights_data: WeightsData,
    layer_data: LayerDataVec,
    kv_cache: KvCacheManager,
    /// Raw GGUF data for tokenizer reconstruction
    model_data: &'static [u8],
    vocab_offset: usize,
    vocab_size: usize,
    bos_id: u32,
    eos_id: u32,
}

/// Non-layer weight data extracted from GGUF.
/// All slices point into mmap'd data ('static lifetime).
struct WeightsData {
    token_embed: &'static [u8],
    final_norm: &'static [u8],
    output_weight: &'static [u8],
    /// True if output_weight is Q8_0 (otherwise Q4_0)
    output_is_q8: bool,
}

/// Per-layer weight data. All slices point into mmap'd data.
struct LayerData {
    attn_norm: &'static [u8],
    wq: &'static [u8],
    wk: &'static [u8],
    wv: &'static [u8],
    wo: &'static [u8],
    ffn_norm: &'static [u8],
    w_gate: &'static [u8],
    w_up: &'static [u8],
    w_down: &'static [u8],
}

/// Fixed-capacity Vec for layer data (avoids heap allocation)
struct LayerDataVec {
    /// Raw storage for up to 64 LayerData values
    storage: [core::mem::MaybeUninit<LayerData>; 64],
    count: usize,
}

impl LayerDataVec {
    fn new() -> Self {
        Self {
            // MaybeUninit doesn't require initialization
            storage: unsafe { core::mem::MaybeUninit::uninit().assume_init() },
            count: 0,
        }
    }

    fn push(&mut self, data: LayerData) -> bool {
        if self.count >= 64 { return false; }
        self.storage[self.count] = core::mem::MaybeUninit::new(data);
        self.count += 1;
        true
    }

    fn get(&self, idx: usize) -> &LayerData {
        debug_assert!(idx < self.count);
        unsafe { self.storage[idx].assume_init_ref() }
    }
}

// ============================================================================
// GGUF → ModelWeights Mapping (Steg 4)
// ============================================================================

/// Build tensor name like "blk.5.attn_q.weight" into a stack buffer
fn tensor_name<'a>(buf: &'a mut [u8; 64], prefix: &str, layer: usize, suffix: &str) -> &'a str {
    let mut pos = 0;
    for b in prefix.bytes() {
        if pos >= 63 { break; }
        buf[pos] = b;
        pos += 1;
    }
    // Write layer number
    if layer >= 100 {
        buf[pos] = b'0' + (layer / 100) as u8; pos += 1;
        buf[pos] = b'0' + ((layer / 10) % 10) as u8; pos += 1;
        buf[pos] = b'0' + (layer % 10) as u8; pos += 1;
    } else if layer >= 10 {
        buf[pos] = b'0' + (layer / 10) as u8; pos += 1;
        buf[pos] = b'0' + (layer % 10) as u8; pos += 1;
    } else {
        buf[pos] = b'0' + layer as u8; pos += 1;
    }
    for b in suffix.bytes() {
        if pos >= 63 { break; }
        buf[pos] = b;
        pos += 1;
    }
    core::str::from_utf8(&buf[..pos]).unwrap_or("")
}

/// Build ModelWeights from parsed GGUF model.
///
/// The model's tensor data references point into mmap'd memory which lives
/// for the process lifetime, so we transmute to 'static.
fn build_model_weights(model: &GgufModel)
    -> Option<(ModelConfig, WeightsData, LayerDataVec)>
{
    let meta = &model.metadata;

    let config = ModelConfig {
        n_layers: meta.n_layers as usize,
        n_heads: meta.n_heads as usize,
        n_kv_heads: meta.n_kv_heads as usize,
        embed_dim: meta.embedding_dim as usize,
        head_dim: meta.head_dim as usize,
        intermediate_size: meta.intermediate_size as usize,
        vocab_size: meta.vocab_size as usize,
        max_seq_len: meta.context_length as usize,
        rope_base: meta.rope_base,
        rms_norm_eps: meta.rms_norm_eps,
    };

    // Find global tensors
    let token_embed = model.tensor("token_embd.weight")?;
    let final_norm = model.tensor("output_norm.weight")?;

    // output.weight may be tied to token_embd.weight
    let output_weight = model.tensor("output.weight")
        .unwrap_or(token_embed);

    println!("[INFERENCE]   token_embd: {:?} {:?}", token_embed.shape, token_embed.dtype);
    println!("[INFERENCE]   output_norm: {:?} {:?}", final_norm.shape, final_norm.dtype);
    println!("[INFERENCE]   output: {:?} {:?} {}", output_weight.shape, output_weight.dtype,
        if model.tensor("output.weight").is_none() { "(tied)" } else { "" });

    // Detect output weight dtype
    let output_is_q8 = output_weight.dtype == libtensor::gguf::GgufDtype::Q8_0;

    // Safety: tensor data points into mmap'd memory that lives for the entire process
    let weights_data = WeightsData {
        token_embed: unsafe { core::mem::transmute::<&[u8], &'static [u8]>(token_embed.data) },
        final_norm: unsafe { core::mem::transmute::<&[u8], &'static [u8]>(final_norm.data) },
        output_weight: unsafe { core::mem::transmute::<&[u8], &'static [u8]>(output_weight.data) },
        output_is_q8,
    };

    // Build per-layer weights
    let mut layer_data = LayerDataVec::new();
    for i in 0..config.n_layers {
        let mut buf = [0u8; 64];

        let attn_norm = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_norm.weight"))?;
        let wq = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_q.weight"))?;
        let wk = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_k.weight"))?;
        let wv = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_v.weight"))?;
        let wo = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_output.weight"))?;
        let ffn_norm = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_norm.weight"))?;
        let w_gate = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_gate.weight"))?;
        let w_up = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_up.weight"))?;
        let w_down = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_down.weight"))?;

        if i == 0 {
            println!("[INFERENCE]   blk.0.attn_q: {:?} {:?}", wq.shape, wq.dtype);
            println!("[INFERENCE]   blk.0.ffn_gate: {:?} {:?}", w_gate.shape, w_gate.dtype);
        }

        // Safety: tensor data points into mmap'd memory (process lifetime)
        unsafe {
            layer_data.push(LayerData {
                attn_norm: core::mem::transmute::<&[u8], &'static [u8]>(attn_norm.data),
                wq: core::mem::transmute::<&[u8], &'static [u8]>(wq.data),
                wk: core::mem::transmute::<&[u8], &'static [u8]>(wk.data),
                wv: core::mem::transmute::<&[u8], &'static [u8]>(wv.data),
                wo: core::mem::transmute::<&[u8], &'static [u8]>(wo.data),
                ffn_norm: core::mem::transmute::<&[u8], &'static [u8]>(ffn_norm.data),
                w_gate: core::mem::transmute::<&[u8], &'static [u8]>(w_gate.data),
                w_up: core::mem::transmute::<&[u8], &'static [u8]>(w_up.data),
                w_down: core::mem::transmute::<&[u8], &'static [u8]>(w_down.data),
            });
        }
    }

    println!("[INFERENCE]   All {} layers mapped successfully", config.n_layers);

    Some((config, weights_data, layer_data))
}

// ============================================================================
// Inference Request Handler (Steg 5)
// ============================================================================

/// Handle an inference or ask request.
///
/// When engine is available: tokenize → prefill → generate → respond.
/// ULTRA 28: Sends IPC notification per token for streaming display.
/// ULTRA 30: TCG breathing room between layers.
/// ULTRA 31: Logit clamping and NaN sanitization.
/// ULTRA 33: Repetition penalty + Top-P sampling.
fn handle_inference_request(
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

    // Tokenize the prompt
    let mut input_tokens = [0u32; 512];
    let n_prompt = tokenizer.encode(&prompt_buf[..prompt_len], &mut input_tokens[1..]);

    // Prepend BOS token
    input_tokens[0] = eng.bos_id;
    let total_prompt = n_prompt + 1;

    println!("[INFERENCE] Tokenized: {} tokens (+ BOS = {} total)", n_prompt, total_prompt);

    // Build LayerWeights slice for transformer::forward
    // We need to reconstruct LayerWeights from LayerData
    let config = &eng.config;
    let yield_cfg = YieldConfig::foreground();

    // Allocate response buffer (in a separate region)
    let mut response_buf = [0u8; 4096];
    let mut response_len = 0usize;

    // Track generated tokens for repetition penalty (ULTRA 33)
    let mut gen_tokens = [0u32; 128];
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
            input_tokens[i], i, config, &weights, &mut eng.kv_cache, arena, &yield_cfg,
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
            // Sample next token from these logits
            last_logits_token = sample_with_penalties(logits, &gen_tokens[..gen_count], arena);
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
            // Forward pass for the previously generated token
            arena.reset_to(arena_mark);

            let (weights, _) = match build_weights_for_forward(eng, arena) {
                Some(w) => w,
                None => break,
            };

            let logits = match forward(
                gen_tokens[gen_count - 1], pos - 1, config, &weights, &mut eng.kv_cache, arena, &yield_cfg,
            ) {
                Some(l) => l,
                None => {
                    println!("[INFERENCE] Forward pass failed at gen token {}", gen_idx);
                    break;
                }
            };

            sample_with_penalties(logits, &gen_tokens[..gen_count], arena)
        };

        // Check for EOS
        if next_token == eng.eos_id {
            println!("[INFERENCE] EOS at token {}", gen_idx);
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

/// Build ModelWeights + LayerWeights for a single forward pass.
///
/// LayerWeights contain &[f32] references for norm weights, which requires
/// casting from the raw &[u8] GGUF data.
fn build_weights_for_forward<'a>(
    eng: &InferenceEngine,
    arena: &'a BumpArena,
) -> Option<(ModelWeights<'a>, &'a [LayerWeights<'a>])> {
    let config = &eng.config;
    let n_layers = config.n_layers;

    // Allocate LayerWeights array in arena
    let layer_weights = arena.alloc_slice::<LayerWeights>(n_layers)?;

    for i in 0..n_layers {
        let ld = eng.layer_data.get(i);

        layer_weights[i] = LayerWeights {
            attn_norm: bytes_as_f32(ld.attn_norm),
            wq: ld.wq,
            wk: ld.wk,
            wv: ld.wv,
            wo: ld.wo,
            ffn_norm: bytes_as_f32(ld.ffn_norm),
            w_gate: ld.w_gate,
            w_up: ld.w_up,
            w_down: ld.w_down,
        };
    }

    let weights = ModelWeights {
        token_embed: eng.weights_data.token_embed,
        layers: layer_weights,
        final_norm: bytes_as_f32(eng.weights_data.final_norm),
        output_weight: eng.weights_data.output_weight,
        output_is_q8: eng.weights_data.output_is_q8,
    };

    Some((weights, layer_weights))
}

/// Cast a &[u8] slice to &[f32] (GGUF guarantees alignment for F32 tensors)
#[inline]
fn bytes_as_f32(data: &[u8]) -> &[f32] {
    let ptr = data.as_ptr() as *const f32;
    let len = data.len() / 4;
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

/// Sample next token with repetition penalty and top-P (ULTRA 33, 31)
fn sample_with_penalties(logits: &[f32], recent_tokens: &[u32], arena: &BumpArena) -> u32 {
    let vocab_size = logits.len();

    // Allocate a mutable copy of logits for manipulation
    let logits_copy = match arena.alloc_f32(vocab_size) {
        Some(l) => l,
        None => return argmax(logits),
    };
    logits_copy.copy_from_slice(logits);

    // ULTRA 31: Sanitize logits — clamp and NaN check
    for v in logits_copy.iter_mut() {
        if v.is_nan() || v.is_infinite() {
            *v = -100.0;
        } else if *v > 100.0 {
            *v = 100.0;
        } else if *v < -100.0 {
            *v = -100.0;
        }
    }

    // ULTRA 33: Repetition penalty
    let penalty_window = recent_tokens.len().min(REP_WINDOW);
    if penalty_window > 0 {
        let start = recent_tokens.len().saturating_sub(REP_WINDOW);
        for &tok in &recent_tokens[start..] {
            if (tok as usize) < vocab_size {
                if logits_copy[tok as usize] > 0.0 {
                    logits_copy[tok as usize] /= REP_PENALTY;
                } else {
                    logits_copy[tok as usize] *= REP_PENALTY;
                }
            }
        }
    }

    // Apply temperature
    if TEMPERATURE > 0.0 && TEMPERATURE != 1.0 {
        let inv_t = 1.0 / TEMPERATURE;
        for v in logits_copy.iter_mut() {
            *v *= inv_t;
        }
    }

    // ULTRA 33: Top-P (nucleus) sampling
    // 1. Softmax
    libtensor::ops::softmax(logits_copy);

    // 2. Sort indices by probability (descending) — use simple selection
    //    For efficiency, we only need to find the nucleus set (cumsum >= TOP_P)
    let mut cumsum = 0.0f32;
    let mut nucleus_count = 0usize;

    // Find top probs by iteratively finding max
    // Use a small buffer for the nucleus set
    let mut nucleus_ids = [0u32; 128];
    let mut nucleus_probs = [0.0f32; 128];
    // Mark picked probs as -1 in logits_copy after picking
    let max_nucleus = 128;

    for n in 0..max_nucleus {
        // Find max remaining prob
        let mut best_idx = 0usize;
        let mut best_prob = -1.0f32;
        for j in 0..vocab_size {
            if logits_copy[j] > best_prob {
                best_prob = logits_copy[j];
                best_idx = j;
            }
        }

        if best_prob <= 0.0 {
            break;
        }

        nucleus_ids[n] = best_idx as u32;
        nucleus_probs[n] = best_prob;
        nucleus_count = n + 1;
        logits_copy[best_idx] = -1.0; // mark as used

        cumsum += best_prob;
        if cumsum >= TOP_P {
            break;
        }
    }

    if nucleus_count == 0 {
        return 0; // fallback
    }

    // Renormalize nucleus probabilities
    let mut sum = 0.0f32;
    for i in 0..nucleus_count {
        sum += nucleus_probs[i];
    }
    if sum > 0.0 {
        for i in 0..nucleus_count {
            nucleus_probs[i] /= sum;
        }
    }

    // Sample from nucleus using kernel RNG
    let r = (random_u32() as f32) / (u32::MAX as f32);
    let mut cum = 0.0f32;
    for i in 0..nucleus_count {
        cum += nucleus_probs[i];
        if r < cum {
            return nucleus_ids[i];
        }
    }

    nucleus_ids[0] // fallback
}

/// ULTRA 30: TCG breathing room — short busy-wait to let QEMU process interrupts
#[inline]
fn tcg_breathe() {
    // ~1000 iterations of spin_loop ≈ ~1ms in QEMU TCG
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
}

/// Send a text response via shmem IPC
fn send_text_response(token: CallerToken, data: &[u8]) {
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

// ============================================================================
// Model Loading (Steg 2)
// ============================================================================

/// Attempt to load a GGUF model from VirtIO disk.
///
/// Strategy:
/// 1. Read sector 0 (FOLKDISK header) for model_sector/model_size
/// 2. If header has model info, use it directly
/// 3. Otherwise, fall back to scanning for GGUF magic
///
/// ULTRA 35: Mmap size rounded up to 4KB boundary.
///
/// Returns (pointer, size) on success.
fn load_model_from_disk() -> Result<(*const u8, usize), &'static str> {
    let mut header_buf = [0u8; SECTOR_SIZE];

    // Read sector 0 of the VirtIO data disk (FOLKDISK header)
    if read_sector(0, &mut header_buf).is_err() {
        return Err("cannot read sector 0");
    }

    // Check FOLKDISK magic
    let has_folkdisk = &header_buf[0..8] == b"FOLKDISK";

    let mut model_start_sector: u64 = 0;
    let mut model_size: usize = 0;

    if has_folkdisk {
        // Parse model_sector from offset 64 and model_size from offset 72
        let ms = u64::from_le_bytes([
            header_buf[64], header_buf[65], header_buf[66], header_buf[67],
            header_buf[68], header_buf[69], header_buf[70], header_buf[71],
        ]);
        let mz = u64::from_le_bytes([
            header_buf[72], header_buf[73], header_buf[74], header_buf[75],
            header_buf[76], header_buf[77], header_buf[78], header_buf[79],
        ]);

        if ms > 0 && mz > 0 {
            model_start_sector = ms;
            model_size = mz as usize;
            println!("[INFERENCE] FOLKDISK header: model @ sector {}, {} bytes ({} KB)",
                model_start_sector, model_size, model_size / 1024);
        }
    }

    // Fallback: scan first 64 sectors for GGUF magic
    if model_start_sector == 0 {
        println!("[INFERENCE] No model in header, scanning for GGUF magic...");
        let gguf_magic = [0x47u8, 0x55, 0x46, 0x47]; // "GGUF" in LE

        for sector in 0..64u64 {
            let mut scan_buf = [0u8; SECTOR_SIZE];
            if read_sector(DATA_START_SECTOR + sector, &mut scan_buf).is_err() {
                continue;
            }
            if scan_buf[0..4] == gguf_magic {
                model_start_sector = DATA_START_SECTOR + sector;
                // Unknown size — will read until zeros
                break;
            }
        }

        if model_start_sector == 0 {
            return Err("no GGUF magic found");
        }
    }

    // Determine mmap size
    // ULTRA 35: Round up to 4KB boundary
    let mmap_size = if model_size > 0 {
        (model_size + 4095) & !4095
    } else {
        MAX_MODEL_SIZE // unknown size, allocate max
    };

    if mmap_size > MAX_MODEL_SIZE {
        return Err("model too large");
    }

    // Allocate mmap region in chunks (kernel limits mmap to 16MB per call)
    const MMAP_CHUNK: usize = 16 * 1024 * 1024; // 16MB per mmap call
    let mut mapped = 0usize;
    while mapped < mmap_size {
        let chunk = (mmap_size - mapped).min(MMAP_CHUNK);
        let addr = MODEL_MMAP_BASE + mapped;
        if mmap_at(addr, chunk, PROT_READ | PROT_WRITE).is_err() {
            println!("[INFERENCE] mmap failed at offset {}MB", mapped / (1024 * 1024));
            return Err("mmap failed");
        }
        mapped += chunk;
    }
    let model_ptr = MODEL_MMAP_BASE as *mut u8;
    println!("[INFERENCE] Mapped {}MB in {} chunks", mmap_size / (1024 * 1024), (mmap_size + MMAP_CHUNK - 1) / MMAP_CHUNK);

    // Read model data from disk
    let sectors_to_read = if model_size > 0 {
        (model_size + SECTOR_SIZE - 1) / SECTOR_SIZE
    } else {
        MAX_MODEL_SIZE / SECTOR_SIZE
    };

    let mut total_read = 0usize;
    let mut sector = model_start_sector;

    // ULTRA 36: Read in 64-sector DMA bursts (32KB per VirtIO request)
    let burst_sectors = 64usize;
    let burst_bytes = burst_sectors * SECTOR_SIZE;
    let total_sectors = sectors_to_read;
    let mut last_progress_mb = 0usize;
    let mut remaining = total_sectors;
    println!("[INFERENCE] Reading {} sectors ({} MB) via DMA bursts...",
        total_sectors, model_size / (1024 * 1024));

    while remaining > 0 {
        let n = remaining.min(burst_sectors);
        let buf = unsafe {
            core::slice::from_raw_parts_mut(model_ptr.add(total_read), n * SECTOR_SIZE)
        };

        match block_read(sector, buf, n) {
            Ok(()) => {
                total_read += n * SECTOR_SIZE;
                sector += n as u64;
                remaining -= n;

                // Progress logging every 4MB
                let current_mb = total_read / (1024 * 1024);
                if current_mb >= last_progress_mb + 4 {
                    println!("[INFERENCE] Loaded {}MB / {}MB", current_mb, model_size / (1024 * 1024));
                    last_progress_mb = current_mb;
                    yield_cpu();
                }

                // If we don't know model_size, check for zero sectors
                if model_size == 0 && total_read > SECTOR_SIZE * 2 {
                    let last = &buf[(n - 1) * SECTOR_SIZE..n * SECTOR_SIZE];
                    if last.iter().all(|&b| b == 0) {
                        total_read -= SECTOR_SIZE;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    if total_read == 0 {
        return Err("no data read");
    }

    // Use exact model_size if known, otherwise use total_read
    let final_size = if model_size > 0 { model_size } else { total_read };

    // Debug: check first 16 bytes of loaded data
    let first_bytes = unsafe { core::slice::from_raw_parts(model_ptr, 16.min(final_size)) };
    println!("[INFERENCE] First bytes: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
        first_bytes[0], first_bytes[1], first_bytes[2], first_bytes[3],
        first_bytes[4], first_bytes[5], first_bytes[6], first_bytes[7]);

    Ok((model_ptr as *const u8, final_size))
}

fn gguf_error_str(e: GgufError) -> &'static str {
    match e {
        GgufError::InvalidMagic => "invalid magic",
        GgufError::UnsupportedVersion(_) => "unsupported version",
        GgufError::TruncatedData => "truncated data",
        GgufError::InvalidMetadata => "invalid metadata",
        GgufError::InvalidTensor => "invalid tensor",
    }
}

// ============================================================================
// Bump Allocator for GGUF parsing (Vec allocations)
// ============================================================================

const HEAP_SIZE: usize = 128 * 1024; // 128KB for GGUF metadata + tensor index

struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    next: UnsafeCell<usize>,
}

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let next = &mut *self.next.get();
        let heap = &mut *self.heap.get();

        let align = layout.align();
        let aligned = (*next + align - 1) & !(align - 1);
        let new_next = aligned + layout.size();

        if new_next > HEAP_SIZE {
            core::ptr::null_mut()
        } else {
            *next = new_next;
            heap.as_mut_ptr().add(aligned)
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't deallocate
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    next: UnsafeCell::new(0),
};
