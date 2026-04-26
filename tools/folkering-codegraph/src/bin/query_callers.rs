//! `query-callers` — print all callers of a named function.
//!
//! Two modes:
//!   query-callers <root-dir>     <fn-name>   — build CSR fresh
//!   query-callers --load <file>  <fn-name>   — load FCG1 blob
//!
//! The `--load` mode is what Day 2 measurements use: build the
//! graph once with `dump-graph`, then time per-query latency
//! without the multi-second build cost in the way.
//!
//! Stderr always emits a one-line `[timing]` summary so the spike
//! harness can scrape it without parsing prose. Format:
//!   [timing] mode=<build|load> setup=<ms> lookup=<us> callers=<n>

use std::env;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage:");
        eprintln!("  query-callers <root-dir>     <fn-name>");
        eprintln!("  query-callers --load <file>  <fn-name>");
        std::process::exit(2);
    }

    let load_mode = args[0] == "--load";
    if load_mode { args.remove(0); }
    let path_arg = args.remove(0);
    let target = args.remove(0);

    let t_setup = Instant::now();
    let g = if load_mode {
        let blob = std::fs::read(&path_arg).expect("read FCG1 blob");
        folkering_codegraph::CallGraph::read_from(&blob).expect("decode FCG1")
    } else {
        folkering_codegraph::build_from_dir(&PathBuf::from(&path_arg))
            .expect("build_from_dir")
    };
    let setup_ms = t_setup.elapsed().as_millis();

    let t_query = Instant::now();
    let Some(target_idx) = g.lookup(&target) else {
        eprintln!("function '{target}' not found in graph");
        std::process::exit(3);
    };
    let callers = g.callers_of(target_idx);
    let lookup_us = t_query.elapsed().as_micros();

    for c in &callers {
        println!("{}", g.names[*c as usize]);
    }

    eprintln!(
        "[timing] mode={} setup={} ms lookup={} us callers={}",
        if load_mode { "load" } else { "build" },
        setup_ms, lookup_us, callers.len()
    );
}
