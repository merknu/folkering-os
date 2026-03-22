//! Host-side tokenizer test CLI.
//!
//! Includes the REAL libtensor tokenizer via #[path] — no source duplication.
//! Uses a heap-backed BumpArena shim so the no_std tokenizer code compiles on host.
//!
//! Usage: echo "hello" | tok-test [path/to/model.gguf]

mod arena;

// Include the ACTUAL libtensor tokenizer source — NOT a copy.
// Any changes to the real tokenizer are automatically tested.
#[path = "../../../userspace/libtensor/src/tokenizer.rs"]
mod tokenizer;

mod gguf_mini;

use std::io::{self, Read};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let gguf_path = args.get(1)
        .map(|s| s.as_str())
        .unwrap_or("boot/model.gguf");

    // Load GGUF file
    let gguf_data = std::fs::read(gguf_path)
        .unwrap_or_else(|e| {
            eprintln!("Failed to read {}: {}", gguf_path, e);
            std::process::exit(1);
        });

    // Parse vocab metadata
    let meta = gguf_mini::parse(&gguf_data)
        .unwrap_or_else(|| {
            eprintln!("Failed to parse GGUF metadata");
            std::process::exit(1);
        });

    eprintln!("Vocab: {} tokens, bos={}, eos={}, offset={}, merges={}",
        meta.vocab_size, meta.bos_id, meta.eos_id, meta.vocab_offset, meta.merges_count);

    // Initialize arena — scales with vocab size (152K vocab needs ~5MB)
    let arena_size = if meta.vocab_size > 100_000 { 8 * 1024 * 1024 } else { 4 * 1024 * 1024 };
    let arena = arena::BumpArena::new(arena_size);

    // Initialize tokenizer with BPE merge support
    let tok = tokenizer::BpeTokenizer::new(
        &gguf_data, meta.vocab_offset, meta.vocab_size,
        meta.bos_id, meta.eos_id,
        meta.merges_offset, meta.merges_count,
        &arena,
    ).unwrap_or_else(|| {
        eprintln!("Failed to initialize tokenizer");
        std::process::exit(1);
    });

    // Read input from stdin
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input).unwrap();

    // Encode
    let mut tokens = [0u32; 4096];
    let n = tok.encode(&input, &mut tokens);

    // Output as JSON array
    print!("[");
    for i in 0..n {
        if i > 0 { print!(","); }
        print!("{}", tokens[i]);
    }
    println!("]");
}
