//! `jit_cache_demo` — show that the second compile of the same WASM
//! module is served from the disk cache instead of paying the full
//! compile cost. Times both runs and reports the speedup.
//!
//! Usage:
//!   cargo run --example jit_cache_demo --release [path/to/module.wasm]
//!
//! With no argument, defaults to the attention demo module that
//! ships with the `wasm-attention` example. Run it twice to see the
//! second invocation hit the cache; pass `--no-cache` to compare
//! against an uncached compile.

use std::path::PathBuf;

use a64_encoder::{
    cached_compile_module, default_cache_dir, parse_module_full, CacheOutcome,
};

const DEFAULT_WASM: &str =
    "examples/wasm-attention/target/wasm32-unknown-unknown/release/attention_wasm.wasm";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_cache = args.iter().any(|a| a == "--no-cache");
    let wasm_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_WASM));

    let wasm_bytes = match std::fs::read(&wasm_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[demo] failed to read {}: {e}", wasm_path.display());
            eprintln!("[demo] hint: build it first via");
            eprintln!("[demo]   cd examples/wasm-attention");
            eprintln!("[demo]   cargo build --release --target wasm32-unknown-unknown");
            std::process::exit(2);
        }
    };
    let module = parse_module_full(&wasm_bytes).expect("parse_module_full");

    println!("[demo] WASM: {} ({} B, {} fns)",
             wasm_path.display(), wasm_bytes.len(), module.bodies.len());

    // Two consecutive compiles. First should be Miss, second should be Hit
    // unless --no-cache is passed.
    let cache_dir = if no_cache {
        println!("[demo] cache: DISABLED (--no-cache)");
        None
    } else {
        let dir = default_cache_dir().expect("HOME / LOCALAPPDATA must be set");
        println!("[demo] cache: {}", dir.display());
        Some(dir)
    };

    let mem_base = 0;
    let mem_size = 4 * 1024 * 1024;
    let entrypoint = (module.bodies.len() - 1) as u32;

    let (_, o1) = cached_compile_module(
        &wasm_bytes, &module, mem_base, mem_size, entrypoint, cache_dir.as_deref(),
    ).expect("first compile");
    println!("[demo] first  call : {}", describe(&o1));

    let (_, o2) = cached_compile_module(
        &wasm_bytes, &module, mem_base, mem_size, entrypoint, cache_dir.as_deref(),
    ).expect("second compile");
    println!("[demo] second call : {}", describe(&o2));

    if let (CacheOutcome::Miss { compile_us, .. }, CacheOutcome::Hit { load_us })
        = (o1, o2)
    {
        let ratio = compile_us as f64 / load_us.max(1) as f64;
        println!("[demo] speedup     : {ratio:.1}× (compile {compile_us} µs → cache load {load_us} µs)");
    }
}

fn describe(o: &CacheOutcome) -> String {
    match o {
        CacheOutcome::Hit { load_us } => format!("HIT     ({load_us} µs load)"),
        CacheOutcome::Miss { compile_us, write_us } =>
            format!("MISS    ({compile_us} µs compile + {write_us} µs write)"),
        CacheOutcome::Disabled => "DISABLED (compiled fresh, not cached)".into(),
    }
}
