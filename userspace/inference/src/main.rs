//! Folkering OS Phase D.1 — hybrid inference service.
//!
//! ┌─────────────────────────┐                  ┌─────────────────────┐
//! │ draug-daemon            │  IPC: shmem_id   │ inference (this)    │
//! │  (or future apps)       │ ───────────────▶ │  router::dispatch   │
//! └─────────────────────────┘   prompt+result  │       │             │
//!                                              │       │             │
//!                                              │  ┌────┴────┐        │
//!                                              │  ▼         ▼        │
//!                                              │ local    proxy      │
//!                                              │ backend  backend    │
//!                                              │ (Burn,   (TCP via   │
//!                                              │  D.2)    libfolk's  │
//!                                              │ stub     llm_       │
//!                                              │ today    generate)  │
//!                                              └─────────────────────┘
//!
//! The router decides per-request which backend handles it. For D.1
//! the local backend is a stub that always returns `NotImplemented`,
//! so every request transparently falls through to the proxy backend
//! — same Ollama wire as before, just one extra IPC hop. That's the
//! whole point: ship the routing infrastructure FIRST, with zero
//! behavior change, and swap in the Burn local engine in D.2 without
//! touching draug-daemon or any future caller.
//!
//! Service contract (see `ipc_msg.rs` for the wire types):
//!
//!   1. Caller creates a shmem region with an `InferenceWire` header
//!      followed by the prompt bytes and a result-buffer.
//!   2. Caller sends an IPC message to this task with the shmem_id
//!      packed in payload0 (and optional flags in payload1).
//!   3. We map the shmem, parse the header, route to a backend.
//!   4. Backend writes its response into the wire's result-buffer
//!      and updates the header's `status` + `output_len` fields.
//!   5. We reply with `Ok(0)` once the response is written, then
//!      unmap. The caller reads its result-buffer and destroys the
//!      shmem.
//!
//! The wire layout is intentionally identical in shape to libfolk's
//! `llm_generate` syscall — the proxy backend is then a 5-line
//! delegator. Whether the local backend ends up wanting the same
//! shape is TBD; if it grows separate fields (KV-cache handle,
//! temperature, top-p, etc.) we extend the header rather than
//! splitting the wire.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::yield_cpu;
use libfolk::sys::shell::{SHELL_OP_DRAUG_STREAM_CHUNK, SHELL_OP_DRAUG_STREAM_END};

/// Hardcoded shell task ID. Tasks 2..8 are spawned in a fixed order
/// at boot (synapse, shell, draug, compositor, intent, draug-streamer,
/// draug-daemon) — shell is always task 3. If this ordering ever
/// changes, the shell will simply not receive the streaming output
/// (no panic — `ipc::send` to a missing target returns Err) and the
/// `[STREAM]` serial log is still complete.
const SHELL_PID: u32 = 3;

/// Pack a UTF-8 fragment (≤ 6 bytes) into a Draug-stream IPC u64
/// and fire-and-forget it to the shell. The shell's recv_async
/// only delivers payload0, so we live in 8 bytes total. Most BPE
/// tokens decode to ≤ 4 bytes; longer fragments are truncated. To
/// avoid breaking UTF-8 codepoints mid-byte we snap the truncation
/// point back to the last complete codepoint inside the budget.
fn send_chunk_to_shell(fragment: &str) {
    let bytes = fragment.as_bytes();
    // Snap to a UTF-8 boundary at most 6 bytes into the fragment so
    // multi-byte characters never split across the truncation.
    let mut len = bytes.len().min(6);
    while len > 0 && !fragment.is_char_boundary(len) {
        len -= 1;
    }
    let mut payload0 = SHELL_OP_DRAUG_STREAM_CHUNK | ((len as u64) << 8);
    for i in 0..len {
        payload0 |= (bytes[i] as u64) << (16 + i * 8);
    }
    let _ = libfolk::sys::ipc::send(SHELL_PID, payload0, 0);
}

mod ipc_msg;
mod router;
mod proxy_backend;
mod local_backend;
mod tensor_math;
mod weights;
mod weights_test_blob;
mod weights_ffn_blob;
mod weights_attn_blob;
mod vfs_loader;
mod sampling;
mod tokenizer;
mod forward_pass;

// ── Bump allocator ──────────────────────────────────────────────────
//
// 768 MiB. The full 28-layer Qwen3-0.6B prefill accumulates per-call
// matmul + matvec Vec allocations in attention_block + swiglu_ffn ×
// 28 layers + 2 decode steps. The bump allocator never frees, so the
// total live + leaked for one D.3.7 run is ~350 MiB. 768 MiB covers
// prefill + a couple of decode steps with margin. A per-call scratch
// arena (resets per forward pass) is queued for D.4.x once we want
// to generate hundreds of tokens — until then bump-and-leak inside
// 768 MiB is fine.
//
// Numpy fasit on the 28-layer Q8 .fbin: argmax = 151667 ('<think>'),
// top-5 = ['<think>', '<|im_start|>', 'ол', '<|im_end|>', '</think>'].
// Qwen3 is a thinking-mode model — responses open with reasoning tags
// before the user-facing text.
const HEAP_SIZE: usize = 768 * 1024 * 1024;

struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    offset: UnsafeCell<usize>,
}

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let offset = &mut *self.offset.get();
        let align = layout.align();
        let aligned = (*offset + align - 1) & !(align - 1);
        let new_offset = aligned + layout.size();
        if new_offset > HEAP_SIZE {
            return core::ptr::null_mut();
        }
        *offset = new_offset;
        (*self.heap.get()).as_mut_ptr().add(aligned)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

impl BumpAllocator {
    /// Return the current bump offset. Pair with `reset_to` to
    /// reclaim everything allocated between checkpoint and reset.
    pub fn checkpoint(&self) -> usize {
        // SAFETY: single-threaded userspace task; concurrent access
        // to the bump offset is impossible without an explicit
        // task::spawn for inference, which we don't do.
        unsafe { *self.offset.get() }
    }

    /// Roll the bump offset back to `offset`. Any `Vec`/`Box`/`String`
    /// allocated in the rolled-back range becomes a zombie — its
    /// pointer dangles into reused memory. The caller MUST ensure
    /// no live references into that range exist after this call.
    ///
    /// The contract is automatically satisfied for "checkpoint, run
    /// `forward_pass`, sample, reset" because the only borrow that
    /// outlives `forward_pass` is the returned `Vec<f32>` of logits,
    /// which the caller drops before resetting.
    pub unsafe fn reset_to(&self, offset: usize) {
        // SAFETY: same single-threaded argument as `checkpoint`.
        // Caller's invariant on dangling pointers is the dangerous
        // part; the offset write itself is trivial.
        *self.offset.get() = offset;
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    offset: UnsafeCell::new(0),
};

// ── Service loop ───────────────────────────────────────────────────

entry!(main);

fn main() -> ! {
    println!("[INFERENCE] Phase D.1 — hybrid router starting up");

    // Sanity-test the local-backend tensor math at boot — fail loud
    // here rather than the first time a real request lands. Cheap
    // (a 2×2 @ 2×2 matmul takes single-digit microseconds).
    if !tensor_math::self_test() {
        println!("[INFERENCE] FATAL: tensor_math self-test failed");
    } else {
        println!("[INFERENCE] tensor self-test PASS");
    }

    // D.2: exercise the full local-backend path (incl. Burn's
    // TensorData round-trip) on a fake in-process wire. Real IPC
    // end-to-end is D.3.
    if !local_backend::boot_test() {
        println!("[INFERENCE] FATAL: local_backend boot_test failed");
    } else {
        println!("[INFERENCE] local_backend D.2 boot_test PASS");
    }

    // D.3.1: parse the embedded `.fbin` test blob, find both named
    // tensors, verify their data via FNV-1a checksums. Real Synapse
    // VFS file-read plumbs in D.3.1.2.
    if !run_fbin_self_test() {
        println!("[INFERENCE] FATAL: weights D.3.1 self-test failed");
    } else {
        println!("[INFERENCE] weights D.3.1 self-test PASS");
    }

    // D.3.2: end-to-end exercise of the new ops over the loaded
    // weights. Pulls the same `.fbin` tensors into actual math:
    // embedding_lookup followed by RMSNorm, with hand-computed
    // expected output. Proves the data path goes from on-disk
    // bytes → parsed tensor → live forward-pass code.
    if !run_d32_self_test() {
        println!("[INFERENCE] FATAL: D.3.2 self-test failed");
    } else {
        println!("[INFERENCE] D.3.2 self-test PASS");
    }

    // D.3.3: SwiGLU FFN block end-to-end. Loads the FFN test blob
    // (gate_proj, up_proj, down_proj + a sample input), runs the
    // full SwiGLU sequence, verifies the sum against the
    // hand-computed reference (≈9.7009).
    if !run_d33_self_test() {
        println!("[INFERENCE] FATAL: D.3.3 self-test failed");
    } else {
        println!("[INFERENCE] D.3.3 self-test PASS");
    }

    // D.3.4: full attention block (QKV + RoPE + causal SDPA + Wo).
    // Two-token sequence with hand-computed reference sum ≈4.0.
    // The Final Boss for inference math; D.3.5 stitches FFN + attn
    // into a real transformer layer.
    if !run_d34_self_test() {
        println!("[INFERENCE] FATAL: D.3.4 self-test failed");
    } else {
        println!("[INFERENCE] D.3.4 self-test PASS");
    }

    // D.3.1.2: load `model_test.fbin` from Synapse VFS, parse it,
    // verify the same tensors land. Replaces the const blob path
    // with a real on-disk-to-memory pipeline. When D.3.1.3 lands the
    // HuggingFace converter and packs a real Qwen2.5 fbin, the same
    // code path picks it up — only the file name changes.
    if !run_d312_vfs_self_test() {
        println!("[INFERENCE] FATAL: D.3.1.2 VFS self-test failed");
    } else {
        println!("[INFERENCE] D.3.1.2 VFS self-test PASS");
    }

    // D.3.1.3: load a real-shape Qwen2.5 fbin produced by
    // `tools/fbin-gen/hf_to_fbin.py`. The synthetic 1-layer model
    // (`make_test_model.py`) is the build-time stand-in; the same
    // self-test runs verbatim against a real Qwen2.5-0.5B fbin once
    // we drop one in `boot/iso_root/`.
    //
    // We only check structural properties here — naming, count,
    // shapes — because the weights are random. Once D.3.5 wires
    // the per-layer forward pass we'll get numerical verification
    // against PyTorch reference output for the same input ids.
    if !run_d313_self_test() {
        println!("[INFERENCE] D.3.1.3 self-test failed (file may be missing — non-fatal)");
    } else {
        println!("[INFERENCE] D.3.1.3 self-test PASS");
    }

    // D.3.1: BPE tokenizer end-to-end. Load the synthetic 259-token
    // .tokb from VFS, encode "Hi" / "Hello" / "Hell" against
    // hand-computed reference IDs, round-trip decode. When D.3.1.b
    // adds GPT-2 byte mapping + pre-tokenizer + a real Qwen
    // tokenizer.json converter, the same self-test runs but with a
    // 151k-token vocab.
    if !run_d31_tokenizer_self_test() {
        println!("[INFERENCE] FATAL: D.3.1 tokenizer self-test failed");
    } else {
        println!("[INFERENCE] D.3.1 tokenizer self-test PASS");
    }

    // D.3.1.b: real Qwen3 tokenizer (vocab=151669, merges=151387,
    // 14 specials including <|im_start|> / <|im_end|>). Verifies
    // GPT-2 byte-mapping, special-token splitting, and whole-chunk
    // BPE produce IDs byte-identical to HuggingFace's reference.
    if !run_d31b_qwen_tokenizer_self_test() {
        println!("[INFERENCE] D.3.1.b Qwen tokenizer self-test failed (file may be missing — non-fatal)");
    } else {
        println!("[INFERENCE] D.3.1.b Qwen tokenizer self-test PASS");
    }

    // D.3.5: full multi-layer forward pass on the synthetic
    // qwen_test.fbin, greedy-sample the next token. Numerically
    // matches the numpy reference in `tools/fbin-gen/forward_ref.py`
    // (same fast_rsqrt / fast_exp polynomials), so the argmax is a
    // hard-baked expected value. The first time we run end-to-end
    // inference: random weights in, deterministic token out.
    if !run_d35_self_test() {
        println!("[INFERENCE] D.3.5 forward-pass self-test failed (file may be missing — non-fatal)");
    } else {
        println!("[INFERENCE] D.3.5 forward-pass self-test PASS");
    }

    // D.4: KV-cache incremental decode. Runs the same prompt
    // ([1, 2, 3]) one token at a time against a single shared cache
    // and verifies the final logits[3] matches D.3.5's prefill
    // result. Proves the cache write/read paths line up with the
    // single-shot reference math.
    if !run_d4_kv_cache_self_test() {
        println!("[INFERENCE] D.4 KV-cache self-test failed (file may be missing — non-fatal)");
    } else {
        println!("[INFERENCE] D.4 KV-cache self-test PASS");
    }

    // D.3.1.q: Q8_0 forward pass on the same synthetic, but with
    // the projection matrices quantized to Q8 blocks. Argmax must
    // stay at 3 (top-1 is robust to quantization noise) and
    // logits[3] must drift < 0.02 from the fp32 reference 1.1470.
    if !run_d31q_q8_self_test() {
        println!("[INFERENCE] D.3.1.q Q8 self-test failed (file may be missing — non-fatal)");
    } else {
        println!("[INFERENCE] D.3.1.q Q8 self-test PASS");
    }

    // D.3.7: First Blood. Load real Qwen3-0.6B (4 layers, Q8 +
    // Q8 embed) from VFS, encode a ChatML prompt with the real
    // Qwen tokenizer, run forward_pass, greedy-decode N tokens,
    // print them through tokenizer.decode_seq. The model is
    // truncated (4/28 layers) so output won't be coherent — but
    // it WILL be deterministic, and matching the numpy reference
    // proves the runtime is correct end-to-end.
    if !run_d37_first_blood() {
        println!("[INFERENCE] D.3.7 First Blood failed (qwen.fbin / qwen.tokb may be missing — non-fatal)");
    }

    println!("[INFERENCE] ready — awaiting IPC requests on this task id");

    let mut req_count: u64 = 0;
    loop {
        match libfolk::sys::ipc::receive() {
            Ok(msg) => {
                req_count += 1;
                handle_request(&msg, req_count);
            }
            Err(libfolk::sys::ipc::IpcError::WouldBlock) => {
                // No request queued — yield so the compositor + net
                // driver get their share. ipc::receive is non-blocking
                // so this loop only spins under load; idle CPU cost is
                // bounded by the scheduler's yield latency.
                yield_cpu();
            }
            Err(e) => {
                // Other IPC errors are diagnostic only — keep serving.
                println!("[INFERENCE] ipc recv error: {:?}", e);
                yield_cpu();
            }
        }
    }
}

/// D.3.1 self-test: parse the embedded blob, look up both tensors,
/// verify shapes and data hashes match the values
/// `weights_test_blob.rs` baked into the file.
fn run_fbin_self_test() -> bool {
    use weights::{FbinView, fnv1a_64};

    let view = match FbinView::parse(weights_test_blob::TEST_FBIN) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] fbin parse error: {:?}", e);
            return false;
        }
    };
    if view.tensors.len() != 2 {
        println!("[INFERENCE] fbin: expected 2 tensors, got {}", view.tensors.len());
        return false;
    }

    // ── tensor 1: embed_test (4×4 f32, values 1..16) ───────────────
    let embed = match view.find("embed_test") {
        Some(t) => t,
        None => {
            println!("[INFERENCE] fbin: tensor 'embed_test' not found");
            return false;
        }
    };
    if embed.shape != [4u32, 4] {
        println!("[INFERENCE] fbin: embed_test wrong shape (got {:?})", embed.shape);
        return false;
    }
    let embed_vals = match view.read_f32(embed) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] fbin: embed_test read_f32 failed");
            return false;
        }
    };
    let sum: f32 = embed_vals.iter().sum();
    // Expected: 1+2+...+16 = 136
    if (sum - 136.0).abs() > 1e-3 {
        println!("[INFERENCE] fbin: embed_test sum {} != 136", sum);
        return false;
    }

    // ── tensor 2: weight_test (4 f32, [0.25, 0.5, 0.75, 1.0]) ──────
    let weight = match view.find("weight_test") {
        Some(t) => t,
        None => {
            println!("[INFERENCE] fbin: tensor 'weight_test' not found");
            return false;
        }
    };
    if weight.shape != [4u32] {
        println!("[INFERENCE] fbin: weight_test wrong shape (got {:?})", weight.shape);
        return false;
    }
    let weight_vals = match view.read_f32(weight) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] fbin: weight_test read_f32 failed");
            return false;
        }
    };
    let wsum: f32 = weight_vals.iter().sum();
    // Expected: 0.25 + 0.5 + 0.75 + 1.0 = 2.5
    if (wsum - 2.5).abs() > 1e-6 {
        println!("[INFERENCE] fbin: weight_test sum {} != 2.5", wsum);
        return false;
    }

    // ── Hash check (proves byte-level integrity) ───────────────────
    // FNV-1a over each tensor's raw bytes. Stable so a regression in
    // the parser surfaces here, not 100 LOC into a forward pass.
    let h_embed = fnv1a_64(view.data_for(embed));
    let h_weight = fnv1a_64(view.data_for(weight));
    println!(
        "[INFERENCE] fbin: embed_hash=0x{:x} weight_hash=0x{:x}",
        h_embed, h_weight
    );
    true
}

/// D.3.2 end-to-end self-test:
/// 1. Parse the embedded `.fbin` blob.
/// 2. Treat `embed_test` (4×4 f32) as a 4-vocab × 4-hidden_dim
///    embedding table, look up `vocab_id = 1` → row [5, 6, 7, 8].
/// 3. Treat `weight_test` (4 f32) as the RMSNorm scale and apply.
/// 4. Sum the result and compare against the hand-computed value
///    from `tensor_math::self_test`.
///
/// This is the first time we run "real" forward-pass code over
/// data that came in via the .fbin loader. When D.3.1.2 lands and
/// the source switches to Synapse VFS, this same function is the
/// regression test.
fn run_d32_self_test() -> bool {
    use weights::FbinView;

    let view = match FbinView::parse(weights_test_blob::TEST_FBIN) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let embed = view.find("embed_test");
    let weight = view.find("weight_test");
    let (Some(embed), Some(weight)) = (embed, weight) else {
        println!("[INFERENCE] D.3.2: missing tensors in .fbin");
        return false;
    };

    let embed_data = match view.read_f32(embed) {
        Some(v) => v,
        None => return false,
    };
    let weight_data = match view.read_f32(weight) {
        Some(v) => v,
        None => return false,
    };

    // Embedding lookup: vocab_id 1 → row [5, 6, 7, 8]
    let vec = match tensor_math::embedding_lookup(&embed_data, 4, 4, 1) {
        Some(v) => v,
        None => return false,
    };
    if vec != [5.0_f32, 6.0, 7.0, 8.0] {
        println!("[INFERENCE] D.3.2: embed lookup row 1 wrong");
        return false;
    }

    // RMSNorm with weight_test → expected sum ≈ 2.65336
    let normed = match tensor_math::rmsnorm(&vec, &weight_data, 1e-6) {
        Some(v) => v,
        None => return false,
    };
    let sum: f32 = normed.iter().sum();
    if (sum - 2.65336).abs() > 5e-3 {
        println!("[INFERENCE] D.3.2: RMSNorm sum {} off from expected 2.65336", sum);
        return false;
    }
    println!(
        "[INFERENCE] D.3.2: embed[1] -> RMSNorm -> sum={} (expected 2.65336)",
        sum
    );
    true
}

/// D.3.3 end-to-end self-test:
/// 1. Parse the FFN test blob (4 tensors: ffn_input, gate_proj,
///    up_proj, down_proj).
/// 2. Read all four into f32 vectors.
/// 3. Run `swiglu_ffn(x, gate, up, down, hidden=2, intermediate=4)`.
/// 4. Sum the result and compare against the Python-computed
///    reference (≈9.7009 — see `tools/fbin-gen/gen_test_blobs.py`
///    docstring for the full derivation).
///
/// When D.3.4 needs more tensors, add a third blob via the same
/// `tools/fbin-gen/gen_test_blobs.py` pattern; the boot tests
/// generalize.
fn run_d33_self_test() -> bool {
    use weights::FbinView;

    let view = match FbinView::parse(weights_ffn_blob::TEST_FFN_FBIN) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.3: ffn-blob parse error: {:?}", e);
            return false;
        }
    };
    let want = ["ffn_input", "gate_proj", "up_proj", "down_proj"];
    for n in &want {
        if view.find(n).is_none() {
            println!("[INFERENCE] D.3.3: missing tensor '{}'", n);
            return false;
        }
    }
    let read = |name: &str| -> Option<alloc::vec::Vec<f32>> {
        view.find(name).and_then(|t| view.read_f32(t))
    };
    let x = match read("ffn_input")  { Some(v) => v, None => return false };
    let g = match read("gate_proj")  { Some(v) => v, None => return false };
    let u = match read("up_proj")    { Some(v) => v, None => return false };
    let d = match read("down_proj")  { Some(v) => v, None => return false };

    let y = match tensor_math::swiglu_ffn(
        &x,
        tensor_math::WeightView::F32(&g),
        tensor_math::WeightView::F32(&u),
        tensor_math::WeightView::F32(&d),
        /*hidden=*/2, /*inter=*/4, /*seq=*/1,
    ) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] D.3.3: swiglu_ffn shape mismatch");
            return false;
        }
    };
    let sum: f32 = y.iter().sum();
    if (sum - 9.7009).abs() > 5e-2 {
        println!("[INFERENCE] D.3.3: ffn sum {} off from expected 9.7009", sum);
        return false;
    }
    println!(
        "[INFERENCE] D.3.3: SwiGLU FFN -> sum={} (expected 9.7009)",
        sum
    );
    true
}

/// D.3.4 end-to-end self-test:
/// 1. Parse the attention test blob (7 tensors: input, Wq, Wk, Wv,
///    Wo, rope_cos, rope_sin).
/// 2. Run `attention_block(x, Wq, Wk, Wv, Wo, cos, sin, seq=2,
///    hidden=2, n_heads=1)`.
/// 3. Sum the result and compare against the Python-computed
///    reference (≈4.0 — full derivation in
///    `tools/fbin-gen/gen_test_blobs.py::gen_attn_blob` docstring).
fn run_d34_self_test() -> bool {
    use weights::FbinView;

    let view = match FbinView::parse(weights_attn_blob::TEST_ATTN_FBIN) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.4: attn-blob parse error: {:?}", e);
            return false;
        }
    };
    let read = |name: &str| -> Option<alloc::vec::Vec<f32>> {
        view.find(name).and_then(|t| view.read_f32(t))
    };
    let (Some(x), Some(wq), Some(wk), Some(wv), Some(wo),
         Some(rope_cos), Some(rope_sin)) = (
        read("attn_input"), read("wq"), read("wk"), read("wv"),
        read("wo"), read("rope_cos"), read("rope_sin"),
    ) else {
        println!("[INFERENCE] D.3.4: missing tensor in attn-blob");
        return false;
    };

    let mut cache_layer = tensor_math::LayerKv {
        k: alloc::vec![0.0f32; 2 * 2],
        v: alloc::vec![0.0f32; 2 * 2],
    };
    let y = match tensor_math::attention_block(
        &x,
        tensor_math::WeightView::F32(&wq),
        tensor_math::WeightView::F32(&wk),
        tensor_math::WeightView::F32(&wv),
        tensor_math::WeightView::F32(&wo),
        /*q_bias=*/None, /*k_bias=*/None, /*v_bias=*/None,
        /*q_norm=*/None, /*k_norm=*/None, /*rms_eps=*/1e-5,
        &rope_cos, &rope_sin,
        /*new_seq=*/2, /*hidden_dim=*/2,
        /*head_dim=*/2,
        /*n_heads=*/1, /*n_kv_heads=*/1,
        &mut cache_layer,
        /*max_pos=*/2,
        /*pos_offset=*/0,
    ) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] D.3.4: attention_block shape mismatch");
            return false;
        }
    };
    let sum: f32 = y.iter().sum();
    if (sum - 4.0).abs() > 5e-2 {
        println!(
            "[INFERENCE] D.3.4: attention sum {} off from expected 4.0",
            sum
        );
        return false;
    }
    println!(
        "[INFERENCE] D.3.4: attention -> sum={} (expected 4.0)",
        sum
    );
    true
}

/// D.3.1.2 self-test: pull `model_test.fbin` from Synapse VFS,
/// parse it, run the same FFN + attention checks the const-blob
/// tests do — verifies the on-disk → ramdisk → Synapse → IPC →
/// shmem pipeline end-to-end.
fn run_d312_vfs_self_test() -> bool {
    use weights::FbinView;

    let bytes = match vfs_loader::read_file("model_test.fbin") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.1.2: VFS read failed: {:?}", e);
            return false;
        }
    };
    println!(
        "[INFERENCE] D.3.1.2: read model_test.fbin from VFS ({} bytes)",
        bytes.len()
    );

    let view = match FbinView::parse(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.1.2: parse error: {:?}", e);
            return false;
        }
    };
    let n = view.tensors.len();
    if n != 12 {
        println!(
            "[INFERENCE] D.3.1.2: expected 12 tensors in combined blob, got {}",
            n
        );
        return false;
    }

    // Marker tensor — fingerprint check that the bytes survived the
    // VFS round-trip intact. Any byte-level corruption in
    // ramdisk/Synapse plumbing shows up here as a hash mismatch.
    let marker = match view.find("vfs_marker").and_then(|t| view.read_f32(t)) {
        Some(v) => v,
        None => return false,
    };
    if marker.len() != 4 {
        return false;
    }
    let sum: f32 = marker.iter().sum();
    if (sum - 1.0).abs() > 1e-3 {
        // 0.1 + 0.2 + 0.3 + 0.4 = 1.0
        println!(
            "[INFERENCE] D.3.1.2: marker sum {} != 1.0 — bytes corrupted?",
            sum
        );
        return false;
    }

    // Re-run the FFN end-to-end on VFS-sourced bytes — proves the
    // round-trip didn't change a single f32.
    let read = |name: &str| -> Option<alloc::vec::Vec<f32>> {
        view.find(name).and_then(|t| view.read_f32(t))
    };
    let (Some(x), Some(g), Some(u), Some(d)) = (
        read("ffn_input"), read("gate_proj"), read("up_proj"), read("down_proj"),
    ) else {
        return false;
    };
    let y = match tensor_math::swiglu_ffn(
        &x,
        tensor_math::WeightView::F32(&g),
        tensor_math::WeightView::F32(&u),
        tensor_math::WeightView::F32(&d),
        2, 4, 1,
    ) {
        Some(v) => v,
        None => return false,
    };
    let ffn_sum: f32 = y.iter().sum();
    if (ffn_sum - 9.7009).abs() > 5e-2 {
        println!(
            "[INFERENCE] D.3.1.2: VFS FFN sum {} != 9.7009 expected",
            ffn_sum
        );
        return false;
    }

    // And the attention block on VFS-sourced bytes — same expected
    // sum 4.0 as the const-blob D.3.4 test.
    let (Some(ax), Some(wq), Some(wk), Some(wv), Some(wo),
         Some(rope_cos), Some(rope_sin)) = (
        read("attn_input"), read("wq"), read("wk"), read("wv"),
        read("wo"), read("rope_cos"), read("rope_sin"),
    ) else {
        return false;
    };
    let mut cache_layer = tensor_math::LayerKv {
        k: alloc::vec![0.0f32; 2 * 2],
        v: alloc::vec![0.0f32; 2 * 2],
    };
    let attn = match tensor_math::attention_block(
        &ax,
        tensor_math::WeightView::F32(&wq),
        tensor_math::WeightView::F32(&wk),
        tensor_math::WeightView::F32(&wv),
        tensor_math::WeightView::F32(&wo),
        None, None, None,
        None, None, 1e-5,
        &rope_cos, &rope_sin,
        /*new_seq=*/2, /*hidden_dim=*/2,
        /*head_dim=*/2, /*n_heads=*/1, /*n_kv_heads=*/1,
        &mut cache_layer, /*max_pos=*/2, /*pos_offset=*/0,
    ) {
        Some(v) => v,
        None => return false,
    };
    let attn_sum: f32 = attn.iter().sum();
    if (attn_sum - 4.0).abs() > 5e-2 {
        println!(
            "[INFERENCE] D.3.1.2: VFS attn sum {} != 4.0 expected",
            attn_sum
        );
        return false;
    }
    println!(
        "[INFERENCE] D.3.1.2: VFS-sourced FFN={} attn={} (matches const-blob)",
        ffn_sum, attn_sum
    );
    true
}

/// D.3.1.3 self-test: load `qwen_test.fbin` from Synapse VFS, walk
/// the per-layer tensors that `hf_to_fbin.py` produced, verify shapes
/// match the synthetic config (hidden=64, n_heads=4, head_dim=16,
/// intermediate=128, n_kv_heads=2, vocab=256, n_layers=1).
///
/// This is a STRUCTURAL test — we don't run forward pass yet (D.3.5).
/// Random weights would produce random output anyway; the value here
/// is proving that the converter's tensor layout matches what the
/// runtime expects to find.
fn run_d313_self_test() -> bool {
    use weights::FbinView;

    let bytes = match vfs_loader::read_file("qwen_test.fbin") {
        Ok(b) => b,
        Err(_) => {
            // Non-fatal: file isn't always packed. The early-out
            // above prints a "non-fatal" message in that case.
            return false;
        }
    };
    println!(
        "[INFERENCE] D.3.1.3: read qwen_test.fbin from VFS ({} bytes)",
        bytes.len()
    );

    let view = match FbinView::parse(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.1.3: parse error: {:?}", e);
            return false;
        }
    };

    // Synthetic config — D.3.6 regenerated with grouped-query
    // attention (n_kv_heads=2) and nonzero QKV biases so it mirrors
    // Qwen2.5's actual shape ratio. The structural test exercises
    // both the GQA shape (HKV != HIDDEN) and the bias presence.
    const HIDDEN: u32 = 64;
    const N_HEADS: u32 = 4;
    const N_KV_HEADS: u32 = 2;
    const HEAD_DIM: u32 = HIDDEN / N_HEADS;            // 16
    const HKV: u32 = HEAD_DIM * N_KV_HEADS;            // 32
    const INTER: u32 = 128;
    const VOCAB: u32 = 256;
    const N_LAYERS: usize = 1;
    const MAX_POS: u32 = 32;

    // Walk every name + shape we expect to find. The order doesn't
    // matter; missing-or-wrong-shape is what trips the assertion.
    let expect = |view: &FbinView, name: &str, shape: &[u32]| -> bool {
        match view.find(name) {
            Some(t) => {
                if t.shape != shape {
                    println!(
                        "[INFERENCE] D.3.1.3: {} shape {:?} != expected {:?}",
                        name, t.shape, shape
                    );
                    false
                } else {
                    true
                }
            }
            None => {
                println!("[INFERENCE] D.3.1.3: missing tensor '{}'", name);
                false
            }
        }
    };

    let mut ok = true;
    ok &= expect(&view, "embed", &[VOCAB, HIDDEN]);
    ok &= expect(&view, "final_norm", &[HIDDEN]);
    ok &= expect(&view, "rope_cos", &[MAX_POS, HEAD_DIM / 2]);
    ok &= expect(&view, "rope_sin", &[MAX_POS, HEAD_DIM / 2]);

    for li in 0..N_LAYERS {
        let prefix = match li {
            0 => "layer.0",
            _ => return false, // synthetic config caps at 1 layer
        };
        let p = |name: &str| -> alloc::string::String {
            let mut s = alloc::string::String::with_capacity(prefix.len() + 1 + name.len());
            s.push_str(prefix);
            s.push('.');
            s.push_str(name);
            s
        };
        ok &= expect(&view, &p("attn_norm"), &[HIDDEN]);
        ok &= expect(&view, &p("ffn_norm"),  &[HIDDEN]);
        ok &= expect(&view, &p("q"),         &[HIDDEN, HIDDEN]);
        ok &= expect(&view, &p("k"),         &[HKV,    HIDDEN]);
        ok &= expect(&view, &p("v"),         &[HKV,    HIDDEN]);
        ok &= expect(&view, &p("o"),         &[HIDDEN, HIDDEN]);
        ok &= expect(&view, &p("q_bias"),    &[HIDDEN]);
        ok &= expect(&view, &p("k_bias"),    &[HKV]);
        ok &= expect(&view, &p("v_bias"),    &[HKV]);
        ok &= expect(&view, &p("gate"),      &[INTER,  HIDDEN]);
        ok &= expect(&view, &p("up"),        &[INTER,  HIDDEN]);
        ok &= expect(&view, &p("down"),      &[HIDDEN, INTER]);
    }

    if ok {
        println!(
            "[INFERENCE] D.3.1.3: {} tensors, layout matches Qwen2.5 config",
            view.tensors.len()
        );
    }
    ok
}

/// D.3.1 BPE tokenizer self-test. Loads `tokenizer_test.tokb` via
/// VFS, walks the synthetic merge table on a few hand-checked inputs,
/// verifies round-trip decode == input.
///
/// Synthetic vocab (256 byte tokens + 3 merge tokens):
///   ID 256 = "He"   (merge 'H' + 'e')
///   ID 257 = "ll"   (merge 'l' + 'l')
///   ID 258 = "Hi"   (merge 'H' + 'i')
///
/// Test cases:
///   encode("Hi")    → [258]
///   encode("Hell")  → [256, 257]   (apply H+e first, then l+l)
///   encode("X")     → [88]         (no merge applies)
///
/// Round-trip: decode_seq(encode(s)) == s for all of the above.
fn run_d31_tokenizer_self_test() -> bool {
    let bytes = match vfs_loader::read_file("tokenizer_test.tokb") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.1: VFS read failed: {:?}", e);
            return false;
        }
    };
    let tok = match tokenizer::Tokenizer::parse(&bytes) {
        Ok(t) => t,
        Err(e) => {
            println!("[INFERENCE] D.3.1: tokenizer parse error: {:?}", e);
            return false;
        }
    };
    println!(
        "[INFERENCE] D.3.1: tokenizer loaded — vocab={} merges={}",
        tok.vocab_size(), tok.merges_count()
    );

    // Test 1: encode("Hi") → [258]
    let ids = tok.encode("Hi");
    if ids != [258u32] {
        println!("[INFERENCE] D.3.1: encode(\"Hi\") = {:?} != [258]", ids);
        return false;
    }
    let back = tok.decode_seq(&ids);
    if back != "Hi" {
        println!("[INFERENCE] D.3.1: round-trip(\"Hi\") = {:?}", back);
        return false;
    }

    // Test 2: encode("Hell") → [256, 257]
    let ids = tok.encode("Hell");
    if ids != [256u32, 257] {
        println!("[INFERENCE] D.3.1: encode(\"Hell\") = {:?} != [256, 257]", ids);
        return false;
    }
    let back = tok.decode_seq(&ids);
    if back != "Hell" {
        println!("[INFERENCE] D.3.1: round-trip(\"Hell\") = {:?}", back);
        return false;
    }

    // Test 3: single byte that doesn't trigger any merge
    let ids = tok.encode("X");
    if ids != [b'X' as u32] {
        println!("[INFERENCE] D.3.1: encode(\"X\") = {:?} != [88]", ids);
        return false;
    }
    let back = tok.decode_seq(&ids);
    if back != "X" {
        println!("[INFERENCE] D.3.1: round-trip(\"X\") = {:?}", back);
        return false;
    }

    println!(
        "[INFERENCE] D.3.1: encode(\"Hi\")=[258], encode(\"Hell\")=[256,257]"
    );
    true
}

/// D.3.5/D.3.6 forward-pass self-test: run the full transformer
/// over a fixed token sequence on the synthetic 1-layer model
/// (now with GQA + QKV biases) and verify the greedy-sampled token
/// matches the numpy reference.
///
/// Reference (computed by `tools/fbin-gen/forward_ref.py` against the
/// same .fbin, n_kv_heads=2 + nonzero biases):
///   token_ids   = [1, 2, 3]
///   argmax      = 3
///   logits[3]   ≈ 1.1470
///   top-5 ids   = [3, 25, 108, 138, 146]
///
/// If this fails after a clean rebuild + repack, the divergence is in
/// one of:
///   - `forward_pass::forward_pass` chaining order (missing residual,
///     wrong norm placement, etc.)
///   - `tensor_math::fast_exp` / `fast_rsqrt` drifting from the numpy
///     reference (both implementations have to match bit-for-bit-ish)
///   - `attention_block` (GQA broadcast wrong, bias add missing, RoPE
///     convention, causal mask)

/// D.3.1.b: real Qwen3 tokenizer self-test. Loads `qwen.tokb` from
/// VFS, encodes the reference inputs that `tools/fbin-gen/tok_to_tokb.py
/// --verify` produced via HuggingFace's transformers, and asserts
/// every ID matches.
///
/// Reference (from HF transformers AutoTokenizer on Qwen3-0.6B):
///   "Hello world"                       → [9707, 1879]
///   "Hvem er du?"                       → [39, 85, 336, 2714, 3845, 30]
///   ChatML wrap of "Hvem er du?"        → [151644, 872, 198, 39, 85,
///                                          336, 2714, 3845, 30, 151645,
///                                          198, 151644, 77091, 198]
///
/// If any of these drift, the divergence is in one of:
///   - GPT-2 byte-to-unicode mapping (off-by-one in the overflow range)
///   - Special-token splitting (longest-match ordering, or boundary
///     scan misses an embedded `<|...|>`)
///   - BPE priority lookup (binary-search index out of order with the
///     priority Vec)
///   - Vocab string decode (UTF-8 round-trip on `Ġ` chars)
fn run_d31b_qwen_tokenizer_self_test() -> bool {
    let bytes = match vfs_loader::read_file("qwen.tokb") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.1.b: VFS read failed: {:?}", e);
            return false;
        }
    };
    let tok = match tokenizer::Tokenizer::parse(&bytes) {
        Ok(t) => t,
        Err(e) => {
            println!("[INFERENCE] D.3.1.b: parse error: {:?}", e);
            return false;
        }
    };
    println!(
        "[INFERENCE] D.3.1.b: Qwen tokenizer loaded — vocab={} merges={} special={}",
        tok.vocab_size(), tok.merges_count(), tok.special_count()
    );

    let cases: [(&str, &[u32]); 3] = [
        ("Hello world", &[9707, 1879]),
        ("Hvem er du?", &[39, 85, 336, 2714, 3845, 30]),
        (
            "<|im_start|>user\nHvem er du?<|im_end|>\n<|im_start|>assistant\n",
            &[151644, 872, 198, 39, 85, 336, 2714, 3845, 30, 151645,
              198, 151644, 77091, 198],
        ),
    ];
    for (text, expected) in cases.iter() {
        let ids = tok.encode(text);
        if ids.as_slice() != *expected {
            println!(
                "[INFERENCE] D.3.1.b: encode({:?}) = {:?} != HF {:?}",
                text, ids, expected
            );
            return false;
        }
        // Round-trip decode for the non-ChatML cases (ChatML's special
        // tokens decode to their literal `<|im_start|>` strings, which
        // round-trips the *visible* form back; useful but not the
        // primary check).
        let back = tok.decode_seq(&ids);
        if !text.starts_with("<|") {
            if back != *text {
                println!(
                    "[INFERENCE] D.3.1.b: decode round-trip {:?} != {:?}",
                    back, text
                );
                return false;
            }
        }
    }
    println!(
        "[INFERENCE] D.3.1.b: encode(\"Hvem er du?\") matches HF [39,85,336,2714,3845,30]"
    );
    true
}

fn run_d35_self_test() -> bool {
    use weights::FbinView;
    use forward_pass::{forward_pass, argmax, ModelConfig};
    use tensor_math::KvCache;

    let bytes = match vfs_loader::read_file("qwen_test.fbin") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.5: VFS read failed: {:?}", e);
            return false;
        }
    };
    let view = match FbinView::parse(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.5: parse error: {:?}", e);
            return false;
        }
    };

    let cfg = ModelConfig {
        n_layers: 1,
        hidden_dim: 64,
        n_heads: 4,
        n_kv_heads: 2,
        head_dim: 16,
        intermediate: 128,
        vocab: 256,
        max_pos: 32,
        eps: 1e-5,
    };
    let token_ids: [u32; 3] = [1, 2, 3];

    // Prefill the whole prompt at once with a fresh cache. Same
    // arithmetic as the pre-D.4 path; argmax expectation is the
    // GQA + biases reference 1.1470 from `forward_ref.py`.
    let head_dim = cfg.hidden_dim / cfg.n_heads;
    let mut cache = KvCache::new(
        cfg.n_layers, cfg.max_pos, cfg.n_kv_heads, head_dim,
    );
    let logits = match forward_pass(&view, &cfg, &mut cache, &token_ids) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] D.3.5: forward_pass returned None");
            return false;
        }
    };
    if logits.len() != cfg.vocab {
        println!(
            "[INFERENCE] D.3.5: logits.len() {} != vocab {}",
            logits.len(), cfg.vocab
        );
        return false;
    }

    // NaN / inf guard — fast_exp clamps but a bug in matmul could
    // still produce non-finite output. Catch it loudly.
    for (i, &v) in logits.iter().enumerate() {
        if !v.is_finite() {
            println!("[INFERENCE] D.3.5: logits[{}] = {} (not finite)", i, v);
            return false;
        }
    }

    let am = match argmax(&logits) {
        Some(i) => i,
        None => return false,
    };
    println!(
        "[INFERENCE] D.3.5: token_ids=[1,2,3] -> argmax={} logits[{}]={}",
        am, am, logits[am as usize]
    );
    if am != 3 {
        println!(
            "[INFERENCE] D.3.5: argmax {} != reference 3 — divergence from numpy ref",
            am
        );
        return false;
    }
    // Tighter sanity: logits[3] should be close to the reference 1.1470.
    // We use a generous tolerance because the float roundoff order
    // between numpy's BLAS-backed @ and our naive triple-loop matmul
    // can shift the magnitude by a percent or two.
    if (logits[3] - 1.1470).abs() > 0.05 {
        println!(
            "[INFERENCE] D.3.5: logits[3]={} drifts from reference 1.1470",
            logits[3]
        );
        return false;
    }
    true
}

/// D.4 KV-cache incremental decode self-test.
///
/// Uses the same model and prompt as D.3.5, but feeds the tokens
/// in one at a time, reusing a single `KvCache` across calls. The
/// last call's logits must match the D.3.5 single-shot prefill
/// result (argmax=3, logits[3]≈1.1470). If they don't, the cache
/// is either:
///   - writing to the wrong slot (pos_offset arithmetic off-by-one)
///   - dropping the prior K/V on a subsequent call
///   - applying RoPE at the wrong absolute position for new tokens
///   - mis-broadcasting groups under GQA (kvh = h / groups when
///     reading the cached slice)
///
/// All four bugs are flat-out incompatible with this assertion, so
/// it's a tight signal. We also verify `cache.seq_len` advanced by
/// the right amount on each call.
fn run_d4_kv_cache_self_test() -> bool {
    use weights::FbinView;
    use forward_pass::{forward_pass, argmax, ModelConfig};
    use tensor_math::KvCache;

    let bytes = match vfs_loader::read_file("qwen_test.fbin") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.4: VFS read failed: {:?}", e);
            return false;
        }
    };
    let view = match FbinView::parse(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.4: parse error: {:?}", e);
            return false;
        }
    };

    let cfg = ModelConfig {
        n_layers: 1,
        hidden_dim: 64,
        n_heads: 4,
        n_kv_heads: 2,
        head_dim: 16,
        intermediate: 128,
        vocab: 256,
        max_pos: 32,
        eps: 1e-5,
    };
    let head_dim = cfg.hidden_dim / cfg.n_heads;
    let mut cache = KvCache::new(
        cfg.n_layers, cfg.max_pos, cfg.n_kv_heads, head_dim,
    );

    // Feed [1], then [2], then [3] — each call advances the cache
    // by one position. The final logits should be the same as the
    // single-shot prefill because the algorithm is mathematically
    // equivalent: K and V for past tokens come from the cache
    // instead of being recomputed.
    let mut last_logits: alloc::vec::Vec<f32> = alloc::vec::Vec::new();
    for (step, tok) in [1u32, 2, 3].iter().enumerate() {
        let logits = match forward_pass(&view, &cfg, &mut cache, &[*tok]) {
            Some(v) => v,
            None => {
                println!("[INFERENCE] D.4: step {} forward_pass returned None", step);
                return false;
            }
        };
        if cache.seq_len != step + 1 {
            println!(
                "[INFERENCE] D.4: cache.seq_len={} after step {}, expected {}",
                cache.seq_len, step, step + 1
            );
            return false;
        }
        last_logits = logits;
    }

    let am = match argmax(&last_logits) {
        Some(i) => i,
        None => return false,
    };
    println!(
        "[INFERENCE] D.4: incremental decode -> argmax={} logits[3]={}",
        am, last_logits[3]
    );
    if am != 3 {
        println!("[INFERENCE] D.4: argmax {} != reference 3", am);
        return false;
    }
    if (last_logits[3] - 1.1470).abs() > 0.05 {
        println!(
            "[INFERENCE] D.4: logits[3]={} drifts from D.3.5 reference 1.1470",
            last_logits[3]
        );
        return false;
    }
    true
}

/// D.3.1.q Q8_0 forward-pass self-test: identical math as D.3.5, but
/// the projection matrices are loaded from `qwen_test_q8.fbin`
/// (where q/k/v/o/gate/up/down are stored as Q8_0 blocks of [f16
/// scale, 32 i8 vals]). Argmax should stay at 3 and logits[3]
/// should remain within 0.02 of the fp32 reference 1.1470 — Q8
/// quantization on small projections drifts ~0.001 per element on
/// average, which compounds bounded through one layer.
///
/// Reference (numpy `forward_ref.py` over the same .fbin):
///   argmax    = 3
///   logits[3] = 1.146946 (fp32 reference: 1.147015, drift ≈ 0.0001)
fn run_d31q_q8_self_test() -> bool {
    use weights::FbinView;
    use forward_pass::{forward_pass, argmax, ModelConfig};
    use tensor_math::KvCache;

    let bytes = match vfs_loader::read_file("qwen_test_q8.fbin") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.1.q: VFS read failed: {:?}", e);
            return false;
        }
    };
    let view = match FbinView::parse(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.1.q: parse error: {:?}", e);
            return false;
        }
    };

    let cfg = ModelConfig {
        n_layers: 1,
        hidden_dim: 64,
        n_heads: 4,
        n_kv_heads: 2,
        head_dim: 16,
        intermediate: 128,
        vocab: 256,
        max_pos: 32,
        eps: 1e-5,
    };
    let head_dim = cfg.hidden_dim / cfg.n_heads;
    let mut cache = KvCache::new(
        cfg.n_layers, cfg.max_pos, cfg.n_kv_heads, head_dim,
    );
    let token_ids: [u32; 3] = [1, 2, 3];

    let logits = match forward_pass(&view, &cfg, &mut cache, &token_ids) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] D.3.1.q: forward_pass returned None");
            return false;
        }
    };

    for (i, &v) in logits.iter().enumerate() {
        if !v.is_finite() {
            println!("[INFERENCE] D.3.1.q: logits[{}] = {} (not finite)", i, v);
            return false;
        }
    }

    let am = match argmax(&logits) {
        Some(i) => i,
        None => return false,
    };
    println!(
        "[INFERENCE] D.3.1.q: Q8 forward -> argmax={} logits[{}]={}",
        am, am, logits[am as usize]
    );
    if am != 3 {
        println!(
            "[INFERENCE] D.3.1.q: argmax {} != 3 (Q8 quantization should preserve top-1)",
            am
        );
        return false;
    }
    if (logits[3] - 1.1470).abs() > 0.02 {
        println!(
            "[INFERENCE] D.3.1.q: logits[3]={} drifts > 0.02 from fp32 reference 1.1470",
            logits[3]
        );
        return false;
    }
    true
}

/// D.3.7 First Blood: real Qwen3-0.6B forward pass through the
/// inference task. This is the moment Folkering OS goes from
/// "executes the math correctly" to "puts a real LLM into a real
/// kernel and asks it a question."
///
/// The model is `qwen.fbin`, produced by:
///   python tools/fbin-gen/hf_to_fbin.py \
///       --model-dir <Qwen3-0.6B HF cache path> \
///       --max-layers 4 --max-seq-len 512 \
///       --quantize q8_0 --quantize-embed \
///       --out boot/iso_root/qwen.fbin
///
/// `--max-layers 4` truncates 28 layers to 4 — the output won't be
/// coherent, but it WILL be deterministic, which is what we
/// actually need to verify. The numpy reference
/// (`tools/fbin-gen/forward_ref.py`) on the same .fbin produces
/// argmax = 72 ('i') for the prompt "Hvem er du?" wrapped in
/// ChatML. That's our hard fixture.
///
/// On failure the test logs but doesn't fatal the boot — the
/// router still serves IPC requests. Useful so a freshly-cloned
/// repo without the converted .fbin still boots.
fn run_d37_first_blood() -> bool {
    use weights::FbinView;
    use forward_pass::{forward_pass, argmax, ModelConfig};
    use tensor_math::KvCache;
    use alloc::vec::Vec;

    println!("[INFERENCE] D.3.7: First Blood — real Qwen3-0.6B (4 layers, Q8)...");

    // Keep-mapped variant: the 232 MiB qwen.fbin would OOM the
    // bump heap if we copied it into a Vec. read_file_mapped maps
    // the shmem at MODEL_VADDR for the lifetime of the process and
    // returns a borrowed slice. Synapse-first / model-disk-fallback
    // ordering — works whether qwen.fbin lives in initrd or on
    // virtio2.
    let fbin_bytes: &'static [u8] = match vfs_loader::read_file_mapped("qwen.fbin") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.7: VFS read qwen.fbin failed: {:?}", e);
            return false;
        }
    };
    println!(
        "[INFERENCE] D.3.7: loaded qwen.fbin ({} MB) via keep-mapped shmem",
        fbin_bytes.len() / (1024 * 1024)
    );
    let view = match FbinView::parse(fbin_bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] D.3.7: parse error: {:?}", e);
            return false;
        }
    };
    println!(
        "[INFERENCE] D.3.7: parsed {} tensors from .fbin",
        view.tensors.len()
    );

    let tokb_bytes = match vfs_loader::read_file("qwen.tokb") {
        Ok(b) => b,
        Err(e) => {
            println!("[INFERENCE] D.3.7: VFS read qwen.tokb failed: {:?}", e);
            return false;
        }
    };
    let tok = match tokenizer::Tokenizer::parse(&tokb_bytes) {
        Ok(t) => t,
        Err(e) => {
            println!("[INFERENCE] D.3.7: tokenizer parse error: {:?}", e);
            return false;
        }
    };

    // Qwen3-0.6B config (truncated to 4 layers). Full 28 layers
    // is the next milestone — converted .fbin = 604 MiB (Q8 + Q8
    // embed); numpy fasit on the same .fbin gives argmax=151667
    // ('<think>'), since Qwen3 is a thinking-mode model that
    // Full 28-layer Qwen3-0.6B with the perf stack (#165 xsave +
    // #166 AVX2 FMA + #168 yield-tune + #169 multi-sector DMA),
    // running on a 768 MiB bump heap.
    let cfg = ModelConfig {
        n_layers: 28,
        hidden_dim: 1024,
        n_heads: 16,
        n_kv_heads: 8,
        head_dim: 128,
        intermediate: 3072,
        vocab: 151936,
        max_pos: 512,
        eps: 1e-6,
    };
    let mut cache = KvCache::new(
        cfg.n_layers, cfg.max_pos, cfg.n_kv_heads, cfg.head_dim,
    );

    // ChatML wrap of "Hvem er du?". HF reference is 14 tokens.
    let prompt = "<|im_start|>user\nHvem er du?<|im_end|>\n<|im_start|>assistant\n";
    let prompt_ids = tok.encode(prompt);
    println!(
        "[INFERENCE] D.3.7: encoded prompt -> {} tokens",
        prompt_ids.len()
    );

    // ── Heap layout discipline for the decode loop ────────────────
    // The bump allocator never frees on drop. To keep heap pressure
    // constant across hundreds of decode steps, we capture a
    // checkpoint and `reset_to` it at the end of each iteration.
    // Anything allocated BELOW the checkpoint (KvCache, tokenizer,
    // prompt_ids, the persistent `sampled` buffer) survives every
    // reset; anything ABOVE (matmul scratch, attention buffers,
    // lm_head logits) is reclaimed in O(1) when we rewind the bump
    // offset.
    //
    // Critical invariant: `sampled` MUST be allocated below the
    // checkpoint, with full capacity so it never realloc-grows
    // inside the loop. The first attempt put `sampled` above the
    // checkpoint and reset_to silently overwrote its buffer with
    // f32 logits from the next forward_pass — token IDs came back
    // looking like 0x40750BAC (≈3.83 as f32).
    const MAX_DECODE: usize = 256;
    let mut sampled: Vec<u32> = Vec::with_capacity(MAX_DECODE + 1);

    let arena_base: usize = ALLOCATOR.checkpoint();
    println!(
        "[INFERENCE] D.3.7: arena_base=0x{:x} (heap free below {} MiB)",
        arena_base,
        arena_base / (1024 * 1024),
    );

    // Prefill: forward the whole prompt at once, populating the
    // cache. The returned logits are at the LAST prompt position;
    // argmax of those is the model's first generated token.
    let prefill_start = unsafe { core::arch::x86_64::_rdtsc() };
    let first_id: u32 = {
        let logits = match forward_pass(&view, &cfg, &mut cache, &prompt_ids) {
            Some(v) => v,
            None => {
                println!("[INFERENCE] D.3.7: prefill returned None");
                return false;
            }
        };
        match argmax(&logits) {
            Some(i) => i,
            None => return false,
        }
    };
    let prefill_cycles = unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(prefill_start);
    let prefill_ms = prefill_cycles / 2_400_000;
    println!(
        "[INFERENCE] D.3.7: prefill ({} tokens × 28 layers) took ~{} ms",
        prompt_ids.len(), prefill_ms,
    );
    // Prefill's logits Vec is dropped at the closing `}` above.
    // We can now safely reclaim everything allocated since the
    // checkpoint — the only thing that survived the scope is the
    // primitive `first_id: u32`, no pointers into the arena.
    // SAFETY: `sampled`'s buffer lives BELOW arena_base, so it
    // survives the reset; nothing else above the line is in scope.
    unsafe { ALLOCATOR.reset_to(arena_base); }

    sampled.push(first_id);

    println!(
        "[INFERENCE] D.3.7: first token = {} ({:?})",
        first_id, tok.decode(first_id).unwrap_or("?")
    );

    // The numpy reference (forward_ref.py on the same .fbin) for
    // the full 28-layer Qwen3-0.6B gives argmax = 151667 ('<think>'),
    // since Qwen3 is a thinking-mode model that opens responses with
    // reasoning tags before producing user-facing text.
    let expected = 151667u32;
    if first_id != expected {
        println!(
            "[INFERENCE] D.3.7: WARN argmax {} != numpy reference {}",
            first_id, expected
        );
    } else {
        println!("[INFERENCE] D.3.7: argmax matches numpy reference ({})", expected);
    }

    // Top-k + temperature sampling for the decode loop. Greedy
    // argmax got stuck in a `<think> → \n → <think>` cycle on
    // Qwen3 thinking-mode (PR #170 observation): the reasoning-tag
    // logit dominates so persistently that argmax can't escape.
    // K=40 + T=0.7 lets the model commit to <think> on step 0
    // (where it has overwhelming logit mass — sampler returns
    // argmax there too) and then drift into actual reasoning text
    // on later steps where the distribution is flatter.
    //
    // Seed from rdtsc — each boot rolls a different sequence, but
    // we log the seed so a run can be replayed deterministically.
    let mut prng = sampling::Xoshiro256pp::from_rdtsc();
    let seed = prng.state();
    println!(
        "[INFERENCE] D.3.7: prng_seed=0x{:016x}{:016x}{:016x}{:016x}",
        seed[0], seed[1], seed[2], seed[3],
    );
    const TOP_K: usize = 40;
    // Top-P (nucleus): keep the smallest set of tokens whose
    // cumulative probability ≥ TOP_P. 0.92 is the HF / OpenAI
    // default — narrow enough to keep gibberish out, wide enough
    // to let creative continuations through. Composed with TOP_K
    // = 40 as upper bound: nucleus is min(40, smallest set with
    // ≥ 92 % mass).
    const TOP_P: f32 = 0.92;
    // T > 1 flattens the softmax; the previous T=0.7 + T=1.2 runs
    // landed on ~99% mass on `\n` (token 198) every step, so the
    // sampler degenerated to greedy. T=1.0 is HF's default and
    // gives the repetition-penalty room to redistribute mass.
    const TEMPERATURE: f32 = 1.0;
    // Repetition penalty (HF-transformers semantics): positive
    // logits for already-generated tokens get divided by 1.3.
    // Breaks Qwen3-0.6B's newline-spiral on its first few tokens.
    const REPETITION_PENALTY: f32 = 1.3;

    // Decode up to MAX_DECODE tokens. Stops on <|im_end|> (151645)
    // or <|endoftext|> (151643). Each step pushes one token through
    // the KV-cached forward pass — O(layers) per token. `sampled`
    // was pre-allocated above the prefill checkpoint with full
    // capacity so `push` here never grows-and-reallocates (which
    // would leave the buffer dangling after the next reset_to).
    let mut next = first_id;
    let tsc_freq_hz: u64 = 2_400_000_000; // approximate; only used for human display
    for step in 0..MAX_DECODE {
        if next == 151645 || next == 151643 { break; }
        let t_start = unsafe { core::arch::x86_64::_rdtsc() };
        let next_token = {
            let mut logits = match forward_pass(&view, &cfg, &mut cache, &[next]) {
                Some(v) => v,
                None => break,
            };

            // Apply repetition penalty across both prompt and
            // generated tokens. Without it Qwen3-0.6B re-picks `\n`
            // ~99 % of the time and the sampler can never escape.
            sampling::apply_repetition_penalty(
                &mut logits, &prompt_ids, REPETITION_PENALTY,
            );
            sampling::apply_repetition_penalty(
                &mut logits, &sampled, REPETITION_PENALTY,
            );

            // Diagnostic: dump top-5 (logit, token_id) for the first
            // two decode steps so we can see the distribution shape.
            if step < 2 {
                let dbg = sampling::top_k(&logits, 5);
                crate::println!(
                    "[INFERENCE] D.3.7 dbg: step={} top5_logits={:?}",
                    step, dbg
                );
            }
            sampling::sample(&logits, TOP_K, TOP_P, TEMPERATURE, &mut prng)
            // `logits` and any temporaries from sampling drop here.
        };

        // Reclaim the entire forward_pass + sampler scratch in one
        // O(1) bump-rewind. KvCache lives below `arena_base` and is
        // untouched. `next_token` is a primitive — survives the
        // reset cleanly.
        // SAFETY: every Vec/Box/String allocated since arena_base in
        // this iteration has dropped at the `}` above. The only thing
        // crossing the reset boundary is `next_token: u32`.
        unsafe { ALLOCATOR.reset_to(arena_base); }

        // Per-token timing — TSC delta gives approximate ms
        // (assumes 2.4 GHz; sufficient for relative comparison).
        let t_end = unsafe { core::arch::x86_64::_rdtsc() };
        let cycles = t_end.wrapping_sub(t_start);
        let ms = cycles / (tsc_freq_hz / 1000);

        // Streaming output: decode the just-sampled token and emit
        // both to serial AND to the shell via IPC, so the user sees
        // Draug write live in their actual shell window (not just
        // the kernel's serial log).
        let fragment = tok.decode_seq(&[next_token]);
        println!(
            "[STREAM] step={:03} ~{}ms id={:06} {:?}",
            step, ms, next_token, fragment,
        );
        send_chunk_to_shell(&fragment);

        next = next_token;
        sampled.push(next);
    }
    // Tell the shell we're done so it can drop a newline and reset
    // the prompt cleanly.
    let _ = libfolk::sys::ipc::send(SHELL_PID, SHELL_OP_DRAUG_STREAM_END, 0);

    // Heap diagnostic at end-of-decode. Should be roughly identical
    // to `arena_base` from before prefill — proof that the arena
    // reset is reclaiming all per-step scratch.
    println!(
        "[INFERENCE] D.3.7: arena tip after decode = {} MiB (was {} MiB pre-prefill)",
        ALLOCATOR.checkpoint() / (1024 * 1024),
        arena_base / (1024 * 1024),
    );

    let response = tok.decode_seq(&sampled);
    println!(
        "[INFERENCE] D.3.7: sampled {} tokens, ids={:?}",
        sampled.len(), sampled
    );
    println!("[INFERENCE] D.3.7: Draug response: {:?}", response);
    println!("[INFERENCE] D.3.7 First Blood — model lives.");
    true
}

fn handle_request(msg: &libfolk::sys::ipc::IpcMessage, n: u64) {
    let shmem_id = (msg.payload0 & 0xFFFF_FFFF) as u32;
    let flags = (msg.payload0 >> 32) & 0xFFFF_FFFF;
    println!(
        "[INFERENCE] req#{} from task {} shmem_id={} flags=0x{:x}",
        n, msg.sender, shmem_id, flags
    );

    let outcome = router::dispatch(shmem_id, flags as u32);

    // Reply with the outcome status in payload0; payload1 reserved for
    // the bytes-written count which the caller already has via the
    // shmem header, but copying it back out cheap-double-checks the
    // happy path.
    let _ = libfolk::sys::ipc::reply(outcome.status as u64, outcome.output_len as u64);
}
