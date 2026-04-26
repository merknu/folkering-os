//! `dead-code --load <file>` — list every function the graph
//! considers unreferenced (zero in-degree). Includes legitimate
//! roots (main, #[test] fns, public API entry points) plus any
//! macro-targeted symbols the v0 builder doesn't model — output
//! needs human triage. v0 limitation acknowledged in the spike
//! charter.

use std::env;
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 || args[0] != "--load" {
        eprintln!("usage: dead-code --load <fcg1-file>");
        std::process::exit(2);
    }
    args.remove(0);
    let path = args.remove(0);

    let t_setup = Instant::now();
    let blob = std::fs::read(&path).expect("read FCG1 blob");
    let g = folkering_codegraph::CallGraph::read_from(&blob).expect("decode");
    let setup_ms = t_setup.elapsed().as_millis();

    let t_query = Instant::now();
    let dead = g.unreferenced();
    let lookup_us = t_query.elapsed().as_micros();

    for v in &dead {
        println!("{}", g.names[*v as usize]);
    }
    eprintln!(
        "[timing] mode=load setup={} ms lookup={} us unreferenced={}",
        setup_ms, lookup_us, dead.len()
    );
}
