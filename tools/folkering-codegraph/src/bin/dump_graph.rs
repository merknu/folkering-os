//! `dump-graph <root-dir> <out-file>` — builds a CSR call-graph
//! from `root-dir` and serializes it to `out-file` in the FCG1
//! binary format. Intended to be run once per Folkering build;
//! the resulting blob is what `query-callers --load` consumes
//! during Day 2 measurements so the build cost (~3.9s) doesn't
//! contaminate the per-query latency numbers.

use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut args = env::args().skip(1);
    let root = PathBuf::from(
        args.next().expect("usage: dump-graph <root-dir> <out-file>"),
    );
    let out = PathBuf::from(
        args.next().expect("usage: dump-graph <root-dir> <out-file>"),
    );

    let t_build = Instant::now();
    let g = folkering_codegraph::build_from_dir(&root)
        .expect("build_from_dir");
    let build_ms = t_build.elapsed().as_millis();

    let t_write = Instant::now();
    let mut w = BufWriter::new(File::create(&out).expect("create out file"));
    g.write_to(&mut w).expect("write_to");
    drop(w);
    let write_ms = t_write.elapsed().as_millis();

    let on_disk = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);

    println!("vertices:   {}", g.names.len());
    println!("edges:      {}", g.col_indices.len());
    println!("csr_bytes:  {} ({:.1} KB)",
             g.csr_bytes(), g.csr_bytes() as f64 / 1024.0);
    println!("on_disk:    {} bytes ({:.1} KB)", on_disk, on_disk as f64 / 1024.0);
    println!("build:      {} ms", build_ms);
    println!("serialize:  {} ms", write_ms);
    println!("→ {}", out.display());
}
