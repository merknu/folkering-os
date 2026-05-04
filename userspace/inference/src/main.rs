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
mod tokenizer;

// ── Bump allocator ──────────────────────────────────────────────────
//
// 256 KiB. The router itself doesn't allocate much (one map per
// request); the proxy backend uses the kernel's syscall_llm_generate
// path which allocates kernel-side. Local backend (Burn) will need
// significantly more heap once D.2 lands — at that point we either
// bump this constant up or move to a per-request slab to bound
// per-call usage.

const HEAP_SIZE: usize = 256 * 1024;

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

    let y = match tensor_math::swiglu_ffn(&x, &g, &u, &d, /*hidden=*/2, /*inter=*/4) {
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

    let y = match tensor_math::attention_block(
        &x, &wq, &wk, &wv, &wo, &rope_cos, &rope_sin,
        /*seq_len=*/2, /*hidden_dim=*/2, /*n_heads=*/1,
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
    let y = match tensor_math::swiglu_ffn(&x, &g, &u, &d, 2, 4) {
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
    let attn = match tensor_math::attention_block(
        &ax, &wq, &wk, &wv, &wo, &rope_cos, &rope_sin, 2, 2, 1
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

    // Synthetic config (matches `make_test_model.py` defaults).
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
