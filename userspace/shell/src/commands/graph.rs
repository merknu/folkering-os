//! Static call-graph queries via the host-side proxy.
//!
//! Exercises the full GRAPH_CALLERS wire path:
//!   shell → libfolk::graph_callers → syscall 0x65 →
//!   kernel TCP to 10.0.2.2:14711 → folkering-proxy GRAPH_CALLERS
//!   handler → FCG1-backed CSR lookup → reply frame back up.
//!
//! Useful both as a developer tool ("who calls X in folkering-os?")
//! and as the canonical smoke test for the Phase 9 + GRAPH_CALLERS
//! chain. If this command works end-to-end, every layer is healthy.

use libfolk::println;
use libfolk::sys::graph_callers;

pub fn cmd_graph_callers<'a>(mut args: impl Iterator<Item = &'a str>) {
    let fn_name = match args.next() {
        Some(name) if !name.is_empty() => name,
        _ => {
            println!("usage: graph-callers <function-name>");
            println!("       graph-callers pop_i32_slot");
            println!("       graph-callers wasm_lower::stack::pop_i32_slot");
            return;
        }
    };

    let mut buf = [0u8; 4096];
    let res = match graph_callers(fn_name, &mut buf) {
        Some(r) => r,
        None => {
            println!("[graph-callers] syscall failed (TCP / proxy unreachable)");
            println!("[graph-callers] confirm folkering-proxy is running on 10.0.2.2:14711");
            println!("[graph-callers] and started with --codegraph <fcg1-blob>");
            return;
        }
    };

    match res.status {
        0 => {
            // OK — body is one caller per line, terminated with \n.
            let body_bytes = &buf[..res.output_len];
            let body = match core::str::from_utf8(body_bytes) {
                Ok(s) => s,
                Err(_) => {
                    println!("[graph-callers] proxy returned non-UTF-8 payload");
                    return;
                }
            };
            // Two passes — count, then print. Avoids needing
            // alloc::Vec in the shell crate (which doesn't pull in
            // the alloc crate).
            let mut count: u32 = 0;
            for line in body.split('\n') {
                if !line.trim().is_empty() { count += 1; }
            }
            if count == 0 {
                println!("[graph-callers] '{}' exists but has no callers", fn_name);
                return;
            }
            println!("[graph-callers] {} caller(s) of '{}':", count, fn_name);
            for line in body.split('\n') {
                let line = line.trim();
                if !line.is_empty() {
                    println!("  - {}", line);
                }
            }
        }
        1 => {
            println!("[graph-callers] '{}' not found in graph", fn_name);
            println!("[graph-callers] (case sensitive — try the qualified path,");
            println!("[graph-callers]  or check that the FCG1 blob covers this crate)");
        }
        2 => {
            println!("[graph-callers] proxy started without --codegraph");
            println!("[graph-callers] restart with: folkering-proxy --codegraph <fcg1>");
        }
        n => {
            println!("[graph-callers] unknown proxy status code: {}", n);
        }
    }
}
