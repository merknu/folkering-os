//! `build-graph <root-dir>` — walks .rs files under root, emits a
//! CSR call-graph, and prints (vertices, edges, csr_bytes).
//!
//! Spike Day 1 H3-4 deliverable: run on tools/a64-encoder/src/ and
//! confirm vertex/edge counts plausibly match the codebase.

use std::env;
use std::path::PathBuf;

fn main() {
    let root = match env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: build-graph <root-dir>");
            std::process::exit(2);
        }
    };
    let g = folkering_codegraph::build_from_dir(&root)
        .expect("build_from_dir");
    println!("vertices: {}", g.names.len());
    println!("edges:    {}", g.col_indices.len());
    println!("csr_bytes: {} ({:.1} KB)",
             g.csr_bytes(),
             g.csr_bytes() as f64 / 1024.0);
}
