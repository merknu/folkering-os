//! Probe what the parser sees in the compiled mlp_wasm.wasm.
//!
//! Just parses the module and prints out function bodies, op counts,
//! and the first/last few ops. Helps us see if the parser handles
//! real Rust-emitted WASM, or if we hit unsupported opcodes.

use a64_encoder::parse_module;

fn main() {
    let wasm_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/wasm-mlp/target/wasm32-unknown-unknown/release/mlp_wasm.wasm".to_string());

    let bytes = std::fs::read(&wasm_path).expect("read wasm");
    println!("[probe] {} bytes loaded from {}", bytes.len(), wasm_path);

    match parse_module(&bytes) {
        Ok(bodies) => {
            println!("[probe] parsed {} function bodies", bodies.len());
            for (i, body) in bodies.iter().enumerate() {
                println!(
                    "[probe] fn[{i}]: {} locals (types: {:?}), {} ops",
                    body.num_locals,
                    body.local_types,
                    body.ops.len()
                );
                let n = body.ops.len();
                let head = n.min(8);
                let tail_start = n.saturating_sub(8);
                for (j, op) in body.ops.iter().take(head).enumerate() {
                    println!("  [{j:3}] {op:?}");
                }
                if n > 16 {
                    println!("  ...");
                    for (j, op) in body.ops.iter().enumerate().skip(tail_start) {
                        println!("  [{j:3}] {op:?}");
                    }
                }
            }
        }
        Err(e) => {
            println!("[probe] PARSE ERROR: {e:?}");
            println!("[probe] hex dump (first 64 B):");
            for (i, b) in bytes.iter().take(64).enumerate() {
                if i % 16 == 0 { print!("\n  {i:04x}:"); }
                print!(" {b:02x}");
            }
            println!();
        }
    }
}
