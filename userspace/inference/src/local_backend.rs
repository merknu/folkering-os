//! Local Burn-based inference backend.
//!
//! D.1 status: stub. Always returns `NotImplemented` so the router
//! transparently falls through to the proxy. D.2 wires Burn's tensor
//! API + a custom CPU `Backend` over the f32 math in `tensor_math`,
//! and D.3 layers in a quantized Qwen2.5-0.5B forward pass.
//!
//! The shape of `run` matches the proxy backend so swapping is a
//! one-line change in `router::dispatch` once we're ready.

use crate::ipc_msg::{InferenceWire, InferenceStatus};
use crate::router::Outcome;

pub fn run(_wire: &InferenceWire, _base_vaddr: usize) -> Outcome {
    // D.1: never. Router falls through to proxy.
    //
    // D.2 will:
    //   1. Match `wire.model` against a small whitelist (e.g. `"local:matmul-test"`,
    //      then `"qwen2.5-0.5b-q4"` once weights are loaded).
    //   2. Decode the prompt + tokenize via the legacy inference-server's
    //      tokenizer (still in tree, see project memory
    //      `folkering-bpe-tokenizer.md`).
    //   3. Run a Burn `Tensor` forward pass — first hand-written matmul
    //      via tensor_math, later via a real Burn `Backend` impl.
    //   4. Sample one token, write back via `wire.result_off`,
    //      return `Ok` with the byte count.
    //
    // The router-level fallback means callers see no behavior change
    // until both the model is loaded AND the local backend's matmul
    // is verified against a reference output.
    Outcome {
        status: InferenceStatus::NotImplemented,
        output_len: 0,
    }
}
