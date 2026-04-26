//! Folkering CodeGraph — CSR call-graph for Rust source.
//!
//! Spike status (2026-04-26): time-boxed to 12 hours. Goal is to
//! validate or reject the hypothesis that a precomputed CSR call-
//! graph beats LLM-Gateway-mediated retrieval for the question
//! "find callers of function X" in Draug's workflow. See
//! `SPIKE_RESULTS.md` for the pre-committed test queries and the
//! kill/expand decision matrix.
//!
//! v0 scope: direct named calls only (`foo()`, `self::bar()`).
//! Out of scope until expand: indirect calls via trait objects,
//! macro-generated calls (`println!`, `assert_eq!`, …), closures,
//! call-graph for WASM binaries (use `parse_module_full` for that).

#![forbid(unsafe_code)]

use std::path::Path;

/// Compressed Sparse Row representation of the forward call-graph.
/// `row_offsets[i]` is the start index in `col_indices` for the
/// outgoing edges of vertex `i`; `row_offsets[i+1]` is the end.
#[derive(Debug, Clone)]
pub struct CallGraph {
    /// Function name (qualified `module::path::name`) for each vertex.
    pub names: Vec<String>,
    /// Length `|V| + 1`. `row_offsets[V]` equals the total edge count.
    pub row_offsets: Vec<u32>,
    /// Length `|E|`. Each entry is a destination vertex index.
    pub col_indices: Vec<u32>,
}

impl CallGraph {
    /// Outgoing-edge slice for vertex `v`. O(1) via row_offsets.
    pub fn neighbors(&self, v: u32) -> &[u32] {
        let s = self.row_offsets[v as usize] as usize;
        let e = self.row_offsets[v as usize + 1] as usize;
        &self.col_indices[s..e]
    }

    /// All callers of `target` (forward CSR scan; O(V + E)).
    /// Returns indices into `self.names`. v0 doesn't keep a CSC,
    /// so this iterates every edge once — fine for the spike.
    pub fn callers_of(&self, target: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for v in 0..(self.names.len() as u32) {
            if self.neighbors(v).contains(&target) {
                out.push(v);
            }
        }
        out
    }

    /// Look up vertex index by qualified name.
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.names.iter().position(|n| n == name).map(|i| i as u32)
    }

    /// Memory footprint in bytes (CSR arrays only, excluding name strings).
    pub fn csr_bytes(&self) -> usize {
        self.row_offsets.len() * 4 + self.col_indices.len() * 4
    }
}

/// Build a CallGraph from every `.rs` file in `root` recursively.
/// Skips `target/`, `.git/`, and similar build directories.
pub fn build_from_dir(_root: &Path) -> Result<CallGraph, BuildError> {
    // TODO Day 1 H1-2: implement via syn::visit::Visit
    todo!("CSR builder is the first hour of the spike — see TaskList #16")
}

#[derive(Debug)]
pub enum BuildError {
    Io(std::io::Error),
    SynParse(syn::Error, String),
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self { BuildError::Io(e) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-crafted 4-vertex graph to exercise the CSR API independently
    /// of the syn-based builder. If `neighbors` or `callers_of` regress,
    /// this test catches it before any builder bug muddies the picture.
    #[test]
    fn csr_neighbors_and_callers() {
        // a → b, a → c, b → c, d → b
        // Edges sorted by source: a(0)→b,c ; b(1)→c ; c(2)→ ; d(3)→b
        let g = CallGraph {
            names: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            row_offsets: vec![0, 2, 3, 3, 4],
            col_indices: vec![1, 2, 2, 1],
        };
        assert_eq!(g.neighbors(0), &[1, 2]);
        assert_eq!(g.neighbors(1), &[2]);
        assert_eq!(g.neighbors(2), &[] as &[u32]);
        assert_eq!(g.callers_of(2), vec![0, 1]);
        assert_eq!(g.callers_of(1), vec![0, 3]);
    }
}
