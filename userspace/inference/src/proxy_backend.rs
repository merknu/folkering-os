//! Proxy backend — delegates to libfolk's `llm_generate` syscall,
//! which TCPs out to folkering-proxy on the host (default
//! `10.0.2.2:14711` SLIRP, or whatever `FOLKERING_PROXY_IP` is built
//! into the kernel).
//!
//! This is the "transparent fallback" arm of D.1. With this in place,
//! draug-daemon can stop calling `llm_generate` directly and start
//! talking to this task via IPC, without any change in observable
//! behavior — same prompts, same model selection, same Ollama
//! responses, just one extra IPC hop. That hop is what lets D.2 swap
//! in a local engine for some-or-all requests later, transparently.

extern crate alloc;

use libfolk::println;

use crate::ipc_msg::{InferenceWire, InferenceStatus};
use crate::router::Outcome;

pub fn run(wire: &InferenceWire, base_vaddr: usize) -> Outcome {
    // Pull `model` out of the null-padded wire field.
    let model = model_str(&wire.model);

    // Prompt and result regions are slices into the mapped shmem page.
    // SAFETY: bounds were validated by the router before we get here.
    let prompt: &[u8] = unsafe {
        core::slice::from_raw_parts(
            (base_vaddr + wire.prompt_off as usize) as *const u8,
            wire.prompt_len as usize,
        )
    };
    let result: &mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(
            (base_vaddr + wire.result_off as usize) as *mut u8,
            wire.result_max as usize,
        )
    };

    // The wire prompt is bytes; libfolk's `llm_generate` wants a `&str`.
    // We assume UTF-8 — the caller is responsible for validating
    // that, since most prompts originate from Rust `String`s anyway.
    // If this assumption breaks we'll surface it as `BadRequest`.
    let prompt_str = match core::str::from_utf8(prompt) {
        Ok(s) => s,
        Err(_) => {
            return Outcome {
                status: InferenceStatus::BadRequest,
                output_len: 0,
            };
        }
    };

    println!(
        "[INFERENCE/proxy] model={} prompt_len={} -> proxy",
        model, prompt.len()
    );

    match libfolk::sys::llm_generate(model, prompt_str, result) {
        Some(p) if p.status == 0 => Outcome {
            status: InferenceStatus::Ok,
            output_len: p.output_len as u32,
        },
        Some(p) => {
            println!(
                "[INFERENCE/proxy] non-zero proxy status {} (output_len {})",
                p.status, p.output_len
            );
            Outcome {
                status: InferenceStatus::ProxyFailed,
                output_len: p.output_len as u32,
            }
        }
        None => {
            println!("[INFERENCE/proxy] syscall failed (proxy unreachable?)");
            Outcome {
                status: InferenceStatus::ProxyFailed,
                output_len: 0,
            }
        }
    }
}

fn model_str(buf: &[u8; 64]) -> &str {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("")
}
