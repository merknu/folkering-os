//! Inference Server binary — Task 6 for Folkering OS.
//!
//! Thin entry point: boot, load model, allocate state, then loop on
//! `recv_async()` and dispatch by opcode. All real logic lives in the
//! `inference_server` library crate.
//!
//! Boot sequence:
//! 1. Detect CPU features (AVX2, etc.)
//! 2. Initialize bump arena (16MB)
//! 3. Read FOLKDISK header for model location
//! 4. Mmap model data (zero-copy)
//! 5. Parse GGUF → zero-copy tensor views
//! 6. Build ModelWeights from GGUF tensors
//! 7. Initialize BPE tokenizer from GGUF vocab
//! 8. Allocate KV-cache
//! 9. Phase B3: mmap pre-allocated logits buffer
//! 10. Enter IPC service loop with full inference

#![no_std]
#![no_main]

extern crate alloc;

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};
use libfolk::sys::ipc::{recv_async, reply_with_token};
use libfolk::sys::memory::{mmap_at, PROT_READ, PROT_WRITE};
use libtensor::arena::BumpArena;
use libtensor::simd;
use libtensor::gguf::GgufModel;
use libtensor::tokenizer::BpeTokenizer;
use libtensor::kv_cache::KvCacheManager;

use inference_server::allocator::BumpAllocator;
use inference_server::chat::find_token_id;
use inference_server::consts::{
    ARENA_SIZE, INFER_OP_ASK, INFER_OP_ASK_ASYNC, INFER_OP_GENERATE, INFER_OP_PING,
    INFER_OP_STATUS, KV_WINDOW_SIZE, LOGITS_BUF_SIZE, LOGITS_BUF_VADDR,
};
use inference_server::gguf_loader::{gguf_error_str, load_model_from_disk};
use inference_server::handlers::{handle_async_inference, handle_inference_request};
use inference_server::inference::InferenceEngine;
use inference_server::weights::build_model_weights;

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator::new();

entry!(main);

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
                    println!("[INFERENCE] head_dim={}, kv_heads={}, rope_base={}, inter={}",
                        meta.head_dim, meta.n_kv_heads, meta.rope_base as u32, meta.intermediate_size);
                    println!("[INFERENCE] BOS={}, EOS={}, vocab_offset={}, merges={}",
                        meta.bos_token_id, meta.eos_token_id, meta.vocab_data_offset, meta.merges_count);

                    // Step 5: Build ModelWeights
                    match build_model_weights(&model) {
                        Some((config, weights_data, layer_data)) => {
                            println!("[INFERENCE] ModelWeights built: {} layers mapped", config.n_layers);

                            // Step 6: Build tokenizer (uses arena for offset+merge tables)
                            let tokenizer = BpeTokenizer::new(
                                model_slice,
                                meta.vocab_data_offset,
                                meta.vocab_size as usize,
                                meta.bos_token_id,
                                meta.eos_token_id,
                                meta.merges_data_offset,
                                meta.merges_count as usize,
                                meta.unknown_token_id,
                                meta.token_type_offset,
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

                                            // Phase B3: pre-allocate logits buffer at LOGITS_BUF_VADDR.
                                            // 1 MB region holds up to 256K-token vocab. Sampler reuses
                                            // this every token instead of bumping the BumpArena.
                                            let logits_buf: &'static mut [f32] = match mmap_at(
                                                LOGITS_BUF_VADDR, LOGITS_BUF_SIZE, PROT_READ | PROT_WRITE,
                                            ) {
                                                Ok(_) => {
                                                    let count = LOGITS_BUF_SIZE / 4;
                                                    println!("[INFERENCE] Logits buffer pre-allocated: {} f32 slots @ 0x{:X}",
                                                        count, LOGITS_BUF_VADDR);
                                                    unsafe {
                                                        core::slice::from_raw_parts_mut(
                                                            LOGITS_BUF_VADDR as *mut f32, count
                                                        )
                                                    }
                                                }
                                                Err(_) => {
                                                    println!("[INFERENCE] ERROR: logits buffer mmap failed");
                                                    loop { yield_cpu(); }
                                                }
                                            };

                                            has_model = true;

                                            // ULTRA 39: Find ChatML stop token IDs
                                            let im_end_id = find_token_id(&tok, b"<|im_end|>");
                                            let im_start_id = find_token_id(&tok, b"<|im_start|>");
                                            println!("[INFERENCE] ChatML tokens: im_end={}, im_start={}", im_end_id, im_start_id);

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
                                                merges_offset: meta.merges_data_offset,
                                                merges_count: meta.merges_count as usize,
                                                unknown_token_id: meta.unknown_token_id,
                                                token_type_offset: meta.token_type_offset,
                                                im_end_id,
                                                im_start_id,
                                                is_generating: false,
                                                logits_buf,
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

    // Step 8: IPC service loop — thin dispatch by opcode
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
                    INFER_OP_ASK_ASYNC => {
                        // ULTRA 42 + async streaming: decode packed payload
                        let query_shmem = ((msg.payload0 >> 16) & 0xFFFF) as u32;
                        let query_len = ((msg.payload0 >> 32) & 0xFFFF) as usize;
                        let ring_shmem = ((msg.payload0 >> 48) & 0xFFFF) as u32;
                        handle_async_inference(
                            msg.token, query_shmem, query_len, ring_shmem,
                            &mut engine, &arena,
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
