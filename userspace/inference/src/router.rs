//! Routing decision: local Burn backend vs proxy (Ollama via TCP).
//!
//! For D.1 the rule is intentionally trivial: try local, fall through
//! to proxy on `NotImplemented`. As D.2/D.3 land, the rule grows:
//!
//! - **Model whitelist.** Local backend only handles models we've
//!   pre-quantized + loaded into Synapse VFS. Ask for `gemma-3:1b`
//!   when only `qwen-coder` is loaded → proxy.
//! - **Prompt length.** Local backend has bounded KV-cache; if the
//!   prompt would overflow it, route to proxy.
//! - **Quality tier.** D.5 onward: callers can hint "I want fast"
//!   (local CPU) or "I want best" (proxy with the big model) via
//!   the request flags. Default is whichever is available.
//!
//! Today's logic is small enough to live inline; we'll factor a real
//! `RoutingPolicy` trait when the rules grow.

use libfolk::println;
use libfolk::sys::{shmem_map, shmem_unmap};

use crate::ipc_msg::{InferenceWire, InferenceStatus, WIRE_MAGIC, WIRE_VERSION};
use crate::{local_backend, proxy_backend};

/// Reserved virtual address for the inference task's request mapping.
/// One concurrent request — fine for D.1 since the IPC service loop
/// is sequential. When D.4 introduces streaming or pipelined requests
/// we'll need a second slot or a per-request mapping pool.
const REQ_VADDR: usize = 0x4100_0000_0000;

pub struct Outcome {
    pub status: InferenceStatus,
    pub output_len: u32,
}

pub fn dispatch(shmem_id: u32, _flags: u32) -> Outcome {
    // 1. Map the caller's shmem so we can read the wire + prompt and
    //    write the result back. Unmap on every exit path so the
    //    address stays free for the next request.
    if let Err(e) = shmem_map(shmem_id, REQ_VADDR) {
        println!("[INFERENCE] shmem_map failed: {:?}", e);
        return Outcome {
            status: InferenceStatus::BadRequest,
            output_len: 0,
        };
    }
    let outcome = handle_mapped();
    let _ = shmem_unmap(shmem_id, REQ_VADDR);
    outcome
}

fn handle_mapped() -> Outcome {
    // SAFETY: the shmem page was just mapped at REQ_VADDR. The header
    // sits at the very start; bounds-check the offsets it carries
    // before we trust them.
    let wire: &mut InferenceWire = unsafe { &mut *(REQ_VADDR as *mut InferenceWire) };

    if wire.magic != WIRE_MAGIC || wire.version != WIRE_VERSION {
        println!(
            "[INFERENCE] reject: bad magic/version (got 0x{:x}, v{})",
            { let m = wire.magic; m },
            { let v = wire.version; v },
        );
        wire.status = InferenceStatus::BadRequest as u16;
        wire.output_len = 0;
        return Outcome { status: InferenceStatus::BadRequest, output_len: 0 };
    }

    // Bounds: prompt and result must both lie within one page (4 KiB)
    // or one shmem region — the kernel-side mapping is single-page
    // for this address today; multi-page comes in D.3 when prompts
    // need more than ~3 KiB.
    const PAGE: u32 = 4096;
    let prompt_end = wire.prompt_off.saturating_add(wire.prompt_len);
    let result_end = wire.result_off.saturating_add(wire.result_max);
    if prompt_end > PAGE || result_end > PAGE
        || wire.prompt_off < core::mem::size_of::<InferenceWire>() as u32
        || wire.result_off < prompt_end
    {
        wire.status = InferenceStatus::BadRequest as u16;
        wire.output_len = 0;
        return Outcome { status: InferenceStatus::BadRequest, output_len: 0 };
    }

    // 2. Routing rule. D.1: try local, fall through to proxy on
    //    NotImplemented. The local backend always returns
    //    NotImplemented today — D.2 changes that.
    let local_attempt = local_backend::run(wire, REQ_VADDR);
    if local_attempt.status == InferenceStatus::Ok {
        wire.status = InferenceStatus::Ok as u16;
        wire.output_len = local_attempt.output_len;
        return local_attempt;
    }

    // 3. Fall through to proxy.
    let proxy_outcome = proxy_backend::run(wire, REQ_VADDR);
    wire.status = proxy_outcome.status as u16;
    wire.output_len = proxy_outcome.output_len;
    proxy_outcome
}
