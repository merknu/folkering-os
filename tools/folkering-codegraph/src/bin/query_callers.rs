//! `query-callers <root-dir> <function-name>` — builds a CSR from
//! root then prints all callers of the named function. Used in
//! Day 2 to compare against the LLM-Gateway baseline.

use std::env;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let root = args.next().expect("usage: query-callers <root-dir> <fn-name>");
    let target = args.next().expect("usage: query-callers <root-dir> <fn-name>");
    let g = folkering_codegraph::build_from_dir(&PathBuf::from(root))
        .expect("build_from_dir");
    let Some(target_idx) = g.lookup(&target) else {
        eprintln!("function '{target}' not found in graph");
        std::process::exit(3);
    };
    let callers = g.callers_of(target_idx);
    for c in callers {
        println!("{}", g.names[c as usize]);
    }
}
