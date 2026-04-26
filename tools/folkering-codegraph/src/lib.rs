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

use std::collections::HashMap;
use std::path::Path;
use syn::visit::Visit;

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

    /// Look up a vertex by name. Tries exact qualified match first,
    /// then suffix match on the simple name (last `::` segment) so
    /// callers can pass either `pop_i32_slot` or
    /// `wasm_lower::stack::pop_i32_slot`. Returns the first match —
    /// for ambiguous simple names use [`Self::lookup_all`].
    pub fn lookup(&self, name: &str) -> Option<u32> {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return Some(i as u32);
        }
        self.names.iter().position(|n| {
            n.rsplit("::").next().is_some_and(|last| last == name)
        }).map(|i| i as u32)
    }

    /// All vertices whose simple name (last `::` segment) matches.
    /// Useful when the same function name exists in multiple modules
    /// and the caller wants every match.
    pub fn lookup_all(&self, simple_name: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for (i, n) in self.names.iter().enumerate() {
            if n.rsplit("::").next().is_some_and(|last| last == simple_name) {
                out.push(i as u32);
            }
        }
        out
    }

    /// Memory footprint in bytes (CSR arrays only, excluding name strings).
    pub fn csr_bytes(&self) -> usize {
        self.row_offsets.len() * 4 + self.col_indices.len() * 4
    }

    /// Vertices with zero in-degree — i.e. nothing in the graph calls
    /// them. Includes legitimate roots (public-API entry points,
    /// `main`, `#[test]` fns) plus any macro-targeted symbols the v0
    /// builder doesn't model. The spike's Q5 query asks the user to
    /// triage these manually.
    pub fn unreferenced(&self) -> Vec<u32> {
        let v = self.names.len();
        let mut referenced = vec![false; v];
        for &dst in &self.col_indices {
            referenced[dst as usize] = true;
        }
        (0..v as u32).filter(|i| !referenced[*i as usize]).collect()
    }
}

/// Build a CallGraph from every `.rs` file under `root` recursively.
/// Skips `target/`, `.git/`, and `node_modules/` build directories.
pub fn build_from_dir(root: &Path) -> Result<CallGraph, BuildError> {
    let mut b = Builder::default();

    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), "target" | ".git" | "node_modules")
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(path)?;
        let file = match syn::parse_file(&src) {
            Ok(f) => f,
            Err(e) => {
                // syn fails on a few macro-heavy files — record but
                // don't abort the build; the spike accepts partial
                // coverage and we report the count in metrics.
                b.parse_failures.push((path.display().to_string(), e.to_string()));
                continue;
            }
        };
        b.current_file = path.display().to_string();
        syn::visit::visit_file(&mut b, &file);
    }

    Ok(b.finish())
}

#[derive(Default)]
struct Builder {
    /// Stack of qualifier components: module names from `mod x { }`
    /// and impl-target type names from `impl Foo { }`. Pushed on
    /// entry, popped on exit, so each fn's qualified name reflects
    /// its lexical position.
    qualifier: Vec<String>,
    /// Stack of containing fn vertex indices. The top is the current
    /// caller for any call expression we encounter.
    fn_stack: Vec<usize>,
    /// All defined functions, in discovery order.
    fns: Vec<FnDef>,
    /// Raw edges: (caller_vertex_idx, callee_simple_name). Resolved
    /// to vertex indices in finish() once every fn is known.
    raw_edges: Vec<(usize, String)>,
    /// Files that syn failed to parse — typically heavy macro use.
    /// Reported for spike honesty about coverage.
    parse_failures: Vec<(String, String)>,
    /// Path of the file currently being walked. Folded into qualified
    /// names so two private fns in different files don't collide.
    current_file: String,
}

#[derive(Debug)]
struct FnDef {
    simple: String,
    qualified: String,
}

impl Builder {
    fn qualify(&self, simple: &str) -> String {
        let prefix = if self.qualifier.is_empty() {
            self.current_file.clone()
        } else {
            format!("{}::{}", self.current_file, self.qualifier.join("::"))
        };
        format!("{prefix}::{simple}")
    }

    fn push_fn(&mut self, simple: String) {
        let qualified = self.qualify(&simple);
        let idx = self.fns.len();
        self.fns.push(FnDef { simple, qualified });
        self.fn_stack.push(idx);
    }

    fn pop_fn(&mut self) {
        self.fn_stack.pop();
    }

    fn finish(self) -> CallGraph {
        let mut name_to_indices: HashMap<&str, Vec<u32>> = HashMap::new();
        for (i, f) in self.fns.iter().enumerate() {
            name_to_indices.entry(f.simple.as_str()).or_default().push(i as u32);
        }

        // Resolve raw edges. If a simple name matches multiple
        // definitions (e.g. `fn new` in many impl blocks), emit an
        // edge to every match — RTA-style over-approximation. Sound
        // for blast-radius queries; precision improves once we add
        // type-aware resolution post-spike.
        let mut edges: Vec<(u32, u32)> = Vec::new();
        for (caller, callee_name) in &self.raw_edges {
            if let Some(targets) = name_to_indices.get(callee_name.as_str()) {
                for &t in targets {
                    edges.push((*caller as u32, t));
                }
            }
            // Unresolved callees (std lib, external crates, intrinsics)
            // are silently dropped — they aren't vertices in our graph.
        }

        edges.sort_unstable();
        edges.dedup();

        let v = self.fns.len();
        let mut row_offsets = vec![0u32; v + 1];
        for &(src, _) in &edges {
            row_offsets[src as usize + 1] += 1;
        }
        for i in 1..=v {
            row_offsets[i] += row_offsets[i - 1];
        }
        let mut col_indices = vec![0u32; edges.len()];
        let mut cursor = row_offsets.clone();
        for &(src, dst) in &edges {
            col_indices[cursor[src as usize] as usize] = dst;
            cursor[src as usize] += 1;
        }

        let names = self.fns.into_iter().map(|f| f.qualified).collect();
        CallGraph { names, row_offsets, col_indices }
    }
}

impl<'ast> Visit<'ast> for Builder {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        self.qualifier.push(m.ident.to_string());
        syn::visit::visit_item_mod(self, m);
        self.qualifier.pop();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let type_name = self_type_simple_name(&i.self_ty)
            .unwrap_or_else(|| "?impl".to_string());
        self.qualifier.push(type_name);
        syn::visit::visit_item_impl(self, i);
        self.qualifier.pop();
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.push_fn(f.sig.ident.to_string());
        syn::visit::visit_item_fn(self, f);
        self.pop_fn();
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.push_fn(f.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, f);
        self.pop_fn();
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let Some(&caller) = self.fn_stack.last() {
            if let Some(callee) = extract_call_target(&e.func) {
                self.raw_edges.push((caller, callee));
            }
        }
        syn::visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if let Some(&caller) = self.fn_stack.last() {
            self.raw_edges.push((caller, e.method.to_string()));
        }
        syn::visit::visit_expr_method_call(self, e);
    }
}

/// Pull the simple name (last segment) from a callee expression.
/// `foo()`           → "foo"
/// `module::foo()`   → "foo"
/// `Self::foo()`     → "foo"
/// Returns None for non-Path callees (closure invocations, dynamic
/// fn pointers) — those don't land in the graph in v0.
fn extract_call_target(func: &syn::Expr) -> Option<String> {
    match func {
        syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// Pull the simple type name from an `impl <Self>` target.
fn self_type_simple_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
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

    /// End-to-end test of the syn-based builder on inline source.
    /// Verifies free fns, impl methods, direct calls, method calls,
    /// and that std/external calls don't produce phantom edges.
    #[test]
    fn builds_callgraph_from_inline_source() {
        let tmp = std::env::temp_dir().join(format!(
            "codegraph-test-{}", std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let src = r#"
            fn helper() -> i32 { 42 }

            struct Lowerer;
            impl Lowerer {
                fn pop_slot(&mut self) -> i32 { helper() }
                fn lower(&mut self) -> i32 {
                    let x = self.pop_slot();
                    let y = helper();
                    x + y
                }
            }

            fn entrypoint() {
                let mut l = Lowerer;
                let _ = l.lower();
            }
        "#;
        std::fs::write(tmp.join("src.rs"), src).unwrap();

        let g = build_from_dir(&tmp).expect("build");

        assert_eq!(g.names.len(), 4, "names = {:?}", g.names);

        let entry = g.lookup("entrypoint").expect("entrypoint");
        let lower = g.lookup("lower").expect("lower");
        assert!(g.neighbors(entry).contains(&lower),
                "entrypoint should call lower; got {:?}", g.neighbors(entry));

        let pop = g.lookup("pop_slot").expect("pop_slot");
        let help = g.lookup("helper").expect("helper");
        let lower_neighbors = g.neighbors(lower);
        assert!(lower_neighbors.contains(&pop));
        assert!(lower_neighbors.contains(&help));

        let helper_callers = g.callers_of(help);
        assert!(helper_callers.contains(&pop), "pop_slot calls helper");
        assert!(helper_callers.contains(&lower), "lower calls helper");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Multiple functions sharing a simple name should each be findable.
    /// `lookup` returns the first; `lookup_all` returns every match.
    /// A call that only matches by simple name produces edges to every
    /// candidate (RTA-style over-approximation).
    #[test]
    fn handles_name_collisions_via_lookup_all() {
        let tmp = std::env::temp_dir().join(format!(
            "codegraph-collide-{}", std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let src = r#"
            mod a {
                pub fn shared() -> i32 { 1 }
            }
            mod b {
                pub fn shared() -> i32 { 2 }
            }
            fn caller() -> i32 {
                a::shared() + b::shared()
            }
        "#;
        std::fs::write(tmp.join("src.rs"), src).unwrap();

        let g = build_from_dir(&tmp).expect("build");
        let all_shared = g.lookup_all("shared");
        assert_eq!(all_shared.len(), 2, "two `shared` fns: {:?}", g.names);

        let caller = g.lookup("caller").expect("caller");
        let neighbors = g.neighbors(caller);
        for v in &all_shared {
            assert!(neighbors.contains(v),
                    "caller should reach both shared() fns; got {:?}", neighbors);
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
