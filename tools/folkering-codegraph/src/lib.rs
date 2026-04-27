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

    // ── Serialization ────────────────────────────────────────────────
    //
    // Format `FCG1` (Folkering CodeGraph v1):
    //
    //   u32 magic     = 0x31474346  ("FCG1" little-endian ASCII)
    //   u32 version   = 1
    //   u32 n_verts
    //   u32 n_edges
    //   u32 row_offsets[n_verts + 1]      -- 4 * (V+1) bytes
    //   u32 col_indices[n_edges]          -- 4 *  E    bytes
    //   for each name (n_verts total):
    //     u32 byte_len
    //     UTF-8 bytes (no padding)
    //
    // No checksum, no compression — we want load-time as close to
    // memcpy as possible so the spike's Day 2 measurements isolate
    // the lookup cost from any artificial parsing overhead.

    const MAGIC: u32 = 0x3147_4346; // "FCG1"
    const VERSION: u32 = 1;

    /// Write the graph to a writer in the FCG1 binary format.
    pub fn write_to<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        let n_verts = self.names.len() as u32;
        let n_edges = self.col_indices.len() as u32;
        debug_assert_eq!(self.row_offsets.len() as u32, n_verts + 1);

        w.write_all(&Self::MAGIC.to_le_bytes())?;
        w.write_all(&Self::VERSION.to_le_bytes())?;
        w.write_all(&n_verts.to_le_bytes())?;
        w.write_all(&n_edges.to_le_bytes())?;

        // Bulk-write the two u32 arrays. on little-endian hosts this
        // is effectively memcpy; on big-endian we'd need byte-swapping
        // but Folkering's targets are AArch64-LE and x86_64-LE.
        for &v in &self.row_offsets { w.write_all(&v.to_le_bytes())?; }
        for &v in &self.col_indices { w.write_all(&v.to_le_bytes())?; }

        for name in &self.names {
            let bytes = name.as_bytes();
            w.write_all(&(bytes.len() as u32).to_le_bytes())?;
            w.write_all(bytes)?;
        }
        Ok(())
    }

    /// Read an FCG1-formatted blob back into a CallGraph. Validates
    /// magic + version, returns LoadError on mismatch or short data.
    pub fn read_from(buf: &[u8]) -> Result<Self, LoadError> {
        let mut p = 0usize;
        let magic = read_u32(buf, &mut p)?;
        if magic != Self::MAGIC { return Err(LoadError::BadMagic); }
        let version = read_u32(buf, &mut p)?;
        if version != Self::VERSION { return Err(LoadError::BadVersion(version)); }
        let n_verts = read_u32(buf, &mut p)? as usize;
        let n_edges = read_u32(buf, &mut p)? as usize;

        let row_count = n_verts + 1;
        let mut row_offsets = Vec::with_capacity(row_count);
        for _ in 0..row_count { row_offsets.push(read_u32(buf, &mut p)?); }
        let mut col_indices = Vec::with_capacity(n_edges);
        for _ in 0..n_edges { col_indices.push(read_u32(buf, &mut p)?); }

        let mut names = Vec::with_capacity(n_verts);
        for _ in 0..n_verts {
            let len = read_u32(buf, &mut p)? as usize;
            if p + len > buf.len() { return Err(LoadError::Truncated); }
            let s = std::str::from_utf8(&buf[p..p + len])
                .map_err(|_| LoadError::InvalidUtf8)?
                .to_string();
            p += len;
            names.push(s);
        }

        Ok(CallGraph { names, row_offsets, col_indices })
    }
}

fn read_u32(buf: &[u8], p: &mut usize) -> Result<u32, LoadError> {
    if *p + 4 > buf.len() { return Err(LoadError::Truncated); }
    let v = u32::from_le_bytes([buf[*p], buf[*p + 1], buf[*p + 2], buf[*p + 3]]);
    *p += 4;
    Ok(v)
}

#[derive(Debug)]
pub enum LoadError {
    BadMagic,
    BadVersion(u32),
    Truncated,
    InvalidUtf8,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::BadMagic => write!(f, "not an FCG1 file (magic mismatch)"),
            LoadError::BadVersion(v) => write!(f, "unsupported FCG version: {v}"),
            LoadError::Truncated => write!(f, "file truncated mid-record"),
            LoadError::InvalidUtf8 => write!(f, "invalid UTF-8 in name field"),
        }
    }
}

impl std::error::Error for LoadError {}

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
        b.enter_file(path.display().to_string());
        syn::visit::visit_file(&mut b, &file);
    }

    Ok(b.finish())
}

/// How a call site referenced its target. Drives the same-file-first
/// resolution heuristic in `finish()`: only `Local` calls (bare
/// `foo()`, `self.bar()`, `Self::baz()`) get the same-file shortcut.
/// `Qualified` calls (`mod_a::foo()`, `OtherType::new()`) carry an
/// explicit path that tells us the target lives somewhere specific —
/// suppressing the global RTA fall-back there would be a soundness
/// regression (the spike's previous bug, fixed in PR #38 review).
#[derive(Debug, Clone, Copy)]
enum CallSiteKind {
    Local,
    Qualified,
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
    /// Raw edges: (caller_vertex_idx, callee_simple_name, call_kind).
    /// Resolved to vertex indices in `finish()` once every fn is
    /// known. The `kind` decides whether same-file-first is safe.
    raw_edges: Vec<(usize, String, CallSiteKind)>,
    /// Files that syn failed to parse — typically heavy macro use.
    /// Reported for spike honesty about coverage.
    parse_failures: Vec<(String, String)>,
    /// Path of the file currently being walked. Folded into qualified
    /// names so two private fns in different files don't collide.
    current_file: String,
    /// Interned table of file paths the builder has seen. Each
    /// `FnDef` stores a `u32` index into this table instead of
    /// cloning `current_file` (~hundreds of bytes per fn for deep
    /// paths). For 4800 fns across ~150 files that's a ~1 MB
    /// allocation cut.
    file_table: Vec<String>,
    /// Reverse index: file path → file_id. Populated lazily as files
    /// are visited. Cleared by `finish()` since file_table alone
    /// suffices for resolution.
    file_id_by_path: HashMap<String, u32>,
    /// `file_id` for `current_file`. Cached so `push_fn` doesn't
    /// have to look it up per fn. Set whenever `current_file` changes.
    current_file_id: u32,
}

#[derive(Debug)]
struct FnDef {
    simple: String,
    qualified: String,
    /// Index into `Builder::file_table`. Used by `finish()` to
    /// prefer same-file matches for `Local` call sites — slashes the
    /// over-approximation from `new`/`default`/`from`/etc collisions
    /// without losing soundness on `mod_a::foo()`-style qualified
    /// calls (those go straight to global RTA).
    file_id: u32,
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

    /// Set `current_file` and refresh the cached `current_file_id`,
    /// interning the path if first-seen. Called once per .rs file
    /// before its `syn::visit::visit_file` traversal.
    fn enter_file(&mut self, path: String) {
        let id = match self.file_id_by_path.get(&path) {
            Some(&id) => id,
            None => {
                let id = self.file_table.len() as u32;
                self.file_table.push(path.clone());
                self.file_id_by_path.insert(path.clone(), id);
                id
            }
        };
        self.current_file = path;
        self.current_file_id = id;
    }

    fn push_fn(&mut self, simple: String) {
        let qualified = self.qualify(&simple);
        let idx = self.fns.len();
        self.fns.push(FnDef { simple, qualified, file_id: self.current_file_id });
        self.fn_stack.push(idx);
    }

    fn pop_fn(&mut self) {
        self.fn_stack.pop();
    }

    fn finish(self) -> CallGraph {
        // Two indexes for the resolution pass:
        //   - `by_name`: simple-name → all matching vertices (global)
        //   - `by_file_name`: (file_id, simple-name) → matches in file
        //
        // Resolution order per call site is kind-aware:
        //
        //   * `Local` (bare `foo()` / `self.bar()`): try same-file first,
        //     fall back to global. Closes the SPIKE_RESULTS.md memory
        //     caveat — `fn new` no longer multi-edges to every other
        //     crate's `new`.
        //
        //   * `Qualified` (`mod_a::foo()` / `Type::baz()`): skip the
        //     same-file shortcut entirely. The path tells us the call
        //     is meant for somewhere else; suppressing global here
        //     would silently drop real cross-file edges (the bug the
        //     PR #38 review caught).
        let mut by_name: HashMap<&str, Vec<u32>> = HashMap::new();
        let mut by_file_name: HashMap<(u32, &str), Vec<u32>> = HashMap::new();
        for (i, f) in self.fns.iter().enumerate() {
            by_name.entry(f.simple.as_str()).or_default().push(i as u32);
            by_file_name
                .entry((f.file_id, f.simple.as_str()))
                .or_default()
                .push(i as u32);
        }

        let mut edges: Vec<(u32, u32)> = Vec::new();
        for (caller, callee_name, kind) in &self.raw_edges {
            let caller_file_id = self.fns[*caller].file_id;
            let mut emitted = false;
            if matches!(kind, CallSiteKind::Local) {
                if let Some(local) =
                    by_file_name.get(&(caller_file_id, callee_name.as_str()))
                {
                    for &t in local {
                        edges.push((*caller as u32, t));
                    }
                    emitted = true;
                }
            }
            if !emitted {
                if let Some(targets) = by_name.get(callee_name.as_str()) {
                    for &t in targets {
                        edges.push((*caller as u32, t));
                    }
                }
                // Unresolved callees (std lib, external crates, intrinsics)
                // are silently dropped — they aren't vertices in the graph.
            }
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
            if let Some((callee, kind)) = extract_call_target(&e.func) {
                self.raw_edges.push((caller, callee, kind));
            }
        }
        syn::visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if let Some(&caller) = self.fn_stack.last() {
            // `recv.method()` has no path qualifier — semantically it's
            // a same-file-friendly local-style call. Same-file-first
            // resolution is safe.
            self.raw_edges.push((caller, e.method.to_string(), CallSiteKind::Local));
        }
        syn::visit::visit_expr_method_call(self, e);
    }
}

/// Pull the simple name (last segment) from a callee expression and
/// classify the call site:
/// * `foo()`           → ("foo", Local)        — bare ident, 1 segment
/// * `Self::foo()`     → ("foo", Local)        — `Self::` is same-impl
/// * `self::foo()`     → ("foo", Local)        — `self::` is same-mod
/// * `Type::new()`     → ("new", Local)        — PascalCase prefix
///                                                (Rust convention: types).
///                                                Same-file-first wins for
///                                                the common `A::new()`
///                                                case, but falls through
///                                                to global if no match.
/// * `module::foo()`   → ("foo", Qualified)    — snake_case prefix
///                                                (modules); resolution
///                                                must reach across files,
///                                                so skip same-file shortcut.
/// * `crate::foo()` / `super::foo()` → ("foo", Qualified) — explicit
///                                                cross-module reach.
/// Returns None for non-Path callees (closure invocations, dynamic
/// fn pointers) — those don't land in the graph in v0.
fn extract_call_target(func: &syn::Expr) -> Option<(String, CallSiteKind)> {
    match func {
        syn::Expr::Path(p) => {
            let last = p.path.segments.last()?.ident.to_string();
            let kind = if p.path.segments.len() <= 1 {
                CallSiteKind::Local
            } else {
                let first = p.path.segments.first()?.ident.to_string();
                // `Self::` / `self::` mean "this impl/this module";
                // PascalCase first segment is a type by Rust convention
                // — the type's defining file is the most likely target.
                // Anything else (snake_case modules, `crate`, `super`)
                // is reaching across files: must go through global RTA
                // or we'd silently drop the cross-file edge.
                let local_like = matches!(first.as_str(), "Self" | "self")
                    || first.chars().next().is_some_and(|c| c.is_uppercase());
                if local_like {
                    CallSiteKind::Local
                } else {
                    CallSiteKind::Qualified
                }
            };
            Some((last, kind))
        }
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

    /// FCG1 serialization roundtrip — every field must survive
    /// write_to → read_from intact, including the order of edges
    /// within each row (which determines lookup correctness).
    #[test]
    fn fcg1_roundtrip_preserves_graph() {
        let g = CallGraph {
            names: vec![
                "first".into(),
                "second".into(),
                "third_with_unicode_äø".into(),
                "fourth".into(),
            ],
            row_offsets: vec![0, 2, 3, 3, 4],
            col_indices: vec![1, 2, 2, 1],
        };
        let mut buf = Vec::new();
        g.write_to(&mut buf).unwrap();
        let g2 = CallGraph::read_from(&buf).expect("read_from");
        assert_eq!(g.names, g2.names);
        assert_eq!(g.row_offsets, g2.row_offsets);
        assert_eq!(g.col_indices, g2.col_indices);
        assert_eq!(g2.callers_of(2), vec![0, 1]);
    }

    /// Bad magic must fail loudly rather than producing a phantom graph.
    #[test]
    fn fcg1_rejects_bad_magic() {
        let buf = vec![0xFFu8; 32];
        assert!(matches!(CallGraph::read_from(&buf), Err(LoadError::BadMagic)));
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

    /// Same-file-first edge resolution: when a simple name like `new`
    /// or `from` exists in MANY files but a caller has a local match,
    /// the global RTA fall-back must NOT fire — that's the whole
    /// point of the heuristic that closes the SPIKE_RESULTS.md
    /// 618 KB > 500 KB caveat.
    #[test]
    fn prefers_same_file_match_over_global() {
        let tmp = std::env::temp_dir().join(format!(
            "codegraph-samefile-{}", std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // file_a defines `new` and a caller that uses it.
        std::fs::write(tmp.join("a.rs"), r#"
            struct A;
            impl A {
                pub fn new() -> A { A }
            }
            fn caller_a() -> A { A::new() }
        "#).unwrap();

        // file_b defines its own `new` (and `caller_b` uses it).
        // file_c defines a third `new` with no caller — pure noise.
        std::fs::write(tmp.join("b.rs"), r#"
            struct B;
            impl B {
                pub fn new() -> B { B }
            }
            fn caller_b() -> B { B::new() }
        "#).unwrap();
        std::fs::write(tmp.join("c.rs"), r#"
            struct C;
            impl C { pub fn new() -> C { C } }
        "#).unwrap();

        let g = build_from_dir(&tmp).expect("build");

        // 5 fns: A::new, caller_a, B::new, caller_b, C::new (no caller).
        // `impl A { fn new }` produces A::new; the test files are
        // standalone Rust per-file, so qualified names embed the file
        // path. We assert by simple-name lookup.
        let all_new = g.lookup_all("new");
        assert_eq!(all_new.len(), 3, "expected 3 `new` fns total: {:?}", g.names);

        let caller_a = g.lookup("caller_a").expect("caller_a");
        let caller_b = g.lookup("caller_b").expect("caller_b");

        // Critical assertion: caller_a's neighbors include EXACTLY
        // ONE `new` (the same-file one) — NOT all 3. Without same-
        // file-first this would emit 3 edges per call site.
        let a_neighbors = g.neighbors(caller_a);
        let a_new_hits: Vec<u32> = a_neighbors.iter()
            .copied()
            .filter(|v| g.names[*v as usize].rsplit("::").next() == Some("new"))
            .collect();
        assert_eq!(a_new_hits.len(), 1,
                   "caller_a should reach exactly one `new` (same-file), got {:?}",
                   a_new_hits.iter().map(|v| &g.names[*v as usize]).collect::<Vec<_>>());

        let b_neighbors = g.neighbors(caller_b);
        let b_new_hits: Vec<u32> = b_neighbors.iter()
            .copied()
            .filter(|v| g.names[*v as usize].rsplit("::").next() == Some("new"))
            .collect();
        assert_eq!(b_new_hits.len(), 1,
                   "caller_b should reach exactly one `new` (same-file), got {:?}",
                   b_new_hits.iter().map(|v| &g.names[*v as usize]).collect::<Vec<_>>());

        // The two same-file targets are distinct (a's new ≠ b's new).
        assert_ne!(a_new_hits[0], b_new_hits[0]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR #38 soundness regression guard: a qualified call like
    /// `mod_a::shared()` MUST resolve cross-file even when the caller's
    /// own file defines a same-named local fn. Without the kind-aware
    /// fix, same-file-first would silently swallow the qualified call
    /// and edge it to the wrong target — a soundness bug Copilot
    /// caught in review.
    #[test]
    fn qualified_call_resolves_cross_file_even_with_local_shadow() {
        let tmp = std::env::temp_dir().join(format!(
            "codegraph-qualified-{}", std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // file_a defines `mod_a::shared` AND a sibling `shared` fn that
        // could shadow it under naive same-file-first. The caller uses
        // the *qualified* form `mod_a::shared()` — that path explicitly
        // names mod_a, so we must NOT collapse it to the same-file
        // free fn.
        std::fs::write(tmp.join("a.rs"), r#"
            mod mod_a {
                pub fn shared() -> i32 { 1 }
            }
            fn shared() -> i32 { 999 }
            fn caller() -> i32 { mod_a::shared() }
        "#).unwrap();

        let g = build_from_dir(&tmp).expect("build");
        let caller = g.lookup("caller").expect("caller");
        let neighbors = g.neighbors(caller);

        // Both `shared` candidates exist; the qualified call goes to
        // global RTA which (in v0) emits edges to every name match.
        // The critical assertion is that we DO reach `mod_a::shared` —
        // not just the local same-file `shared`.
        let names: Vec<&str> = neighbors.iter()
            .map(|v| g.names[*v as usize].as_str())
            .collect();
        let has_mod_a_shared = names.iter().any(|n| n.contains("mod_a") && n.ends_with("::shared"));
        assert!(has_mod_a_shared,
                "qualified call mod_a::shared() must reach mod_a::shared; got {:?}",
                names);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Cross-file calls (the case where same-file lookup is empty)
    /// must still work via the global RTA fall-back.
    #[test]
    fn falls_back_to_global_when_no_local_match() {
        let tmp = std::env::temp_dir().join(format!(
            "codegraph-fallback-{}", std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // file_a defines `helper`, NO local caller.
        std::fs::write(tmp.join("a.rs"), r#"
            pub fn helper() -> i32 { 42 }
        "#).unwrap();

        // file_b has a caller that calls `helper` — must resolve
        // cross-file via the global fall-back.
        std::fs::write(tmp.join("b.rs"), r#"
            fn caller() -> i32 { helper() }
        "#).unwrap();

        let g = build_from_dir(&tmp).expect("build");
        let helper = g.lookup("helper").expect("helper");
        let caller = g.lookup("caller").expect("caller");
        assert!(g.neighbors(caller).contains(&helper),
                "caller should reach helper via cross-file fall-back; got {:?}",
                g.neighbors(caller));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
