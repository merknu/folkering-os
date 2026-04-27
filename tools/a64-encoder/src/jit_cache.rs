//! Host-side disk cache for JIT'd module blobs.
//!
//! Skips the multi-millisecond [`compile_module`] cost on repeated
//! invocations with identical inputs. The cache key is
//! `SHA-256(JIT_VERSION ‖ wasm_bytes ‖ mem_base ‖ mem_size ‖ entrypoint)`
//! — every input that affects the emitted AArch64 bytes is in there.
//!
//! On-disk format `FJC1` (Folkering JIT Cache v1):
//!
//! ```text
//!   u32 magic              = 0x3143_4A46  ("FJC1" LE ASCII)
//!   u32 format_version     = 1
//!   u32 entrypoint_offset
//!   u32 n_funcs
//!   u32 function_offsets[n_funcs]
//!   u32 code_len
//!   u8  code[code_len]
//! ```
//!
//! No checksum, no compression — load is essentially memcpy. The
//! SHA-256-derived filename IS the integrity guarantee; if the bytes
//! were modified the SHA wouldn't match the filename and the entry
//! would simply not be looked up.
//!
//! Default cache dir is `~/.cache/folkering/jit/` on Unix and
//! `%LOCALAPPDATA%\folkering\jit\` on Windows. Pass `cache_dir =
//! None` to [`cached_compile_module`] to disable caching for one-off
//! callers (tests, benchmarks that want to measure cold compile).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::wasm_lower::{compile_module, LowerError, ModuleLayout};
use crate::wasm_module::Module;

/// JIT version — bumped every time the encoder generates *different*
/// AArch64 bytes for the same WASM input. Cache entries with another
/// version produce a SHA mismatch (because the version is mixed into
/// the key), so they're silently treated as misses and overwritten.
///
/// **Bump this when:**
///   * Any [`crate::Encoder`] method changes its emitted bytes.
///   * Any `wasm_lower` lowering rule changes its output sequence.
///   * Default constants that influence the prologue/epilogue change
///     (frame size, register allocation, scratch picks).
///
/// **Don't bump for:**
///   * Refactors that don't change emitted bytes.
///   * Comment / docs / test changes.
///   * Compiler upgrades — we cache the JIT output, not the JIT
///     binary, so rustc-version drift doesn't affect cache validity.
pub const JIT_VERSION: u32 = 1;

const MAGIC: u32 = 0x3143_4A46; // "FJC1" little-endian ASCII

/// Outcome of a [`cached_compile_module`] call. Useful for tooling
/// that wants to log cache hit rates or measure cold-vs-warm timing.
#[derive(Debug, Clone, Copy)]
pub enum CacheOutcome {
    /// Loaded from disk. `load_us` covers `read_to_end` + FJC1 decode.
    Hit { load_us: u64 },
    /// Compiled fresh. `compile_us` is the lowerer cost; `write_us`
    /// is the FJC1 encode + atomic disk write that follows.
    Miss { compile_us: u64, write_us: u64 },
    /// Caller passed `cache_dir = None`. No caching happened; the
    /// compile cost was paid in full.
    Disabled,
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
    Lower(LowerError),
}

impl From<io::Error> for CacheError {
    fn from(e: io::Error) -> Self { CacheError::Io(e) }
}

impl From<LowerError> for CacheError {
    fn from(e: LowerError) -> Self { CacheError::Lower(e) }
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Io(e) => write!(f, "cache I/O: {e}"),
            CacheError::BadMagic => write!(f, "not an FJC1 file (magic mismatch)"),
            CacheError::BadVersion(v) => write!(f, "unsupported FJC1 format version: {v}"),
            CacheError::Truncated => write!(f, "FJC1 blob truncated mid-record"),
            CacheError::Lower(e) => write!(f, "lower failed: {e:?}"),
        }
    }
}

impl std::error::Error for CacheError {}

/// Compile a WASM module, using a disk cache to skip work on repeated
/// invocations with identical inputs. Pass `cache_dir = None` to
/// disable caching (compile fresh, returns [`CacheOutcome::Disabled`]).
///
/// `wasm_bytes` is the raw module binary the cache key is derived
/// from. `module` is the parsed view — callers typically have both
/// already (parse → cached_compile_module is the natural pipeline).
///
/// Cache hits are byte-identical to the original compile output —
/// the test [`tests::second_compile_is_cache_hit`] enforces this.
/// A corrupted, truncated, or wrong-version cache entry silently
/// falls through to a recompile and atomic overwrite.
pub fn cached_compile_module(
    wasm_bytes: &[u8],
    module: &Module,
    mem_base: u64,
    mem_size: u32,
    entrypoint_fn_idx: u32,
    cache_dir: Option<&Path>,
) -> Result<(ModuleLayout, CacheOutcome), CacheError> {
    let Some(dir) = cache_dir else {
        let layout = compile_module(module, mem_base, mem_size, entrypoint_fn_idx)?;
        return Ok((layout, CacheOutcome::Disabled));
    };

    let key = compute_cache_key(wasm_bytes, mem_base, mem_size, entrypoint_fn_idx);
    let path = dir.join(format!("{key}.fjc1"));

    // Try a hit first. Any decode failure (missing, corrupted, version
    // mismatch) silently falls through to a recompile and overwrite.
    if let Ok(blob) = fs::read(&path) {
        let t = Instant::now();
        if let Ok(layout) = decode_fjc1(&blob) {
            return Ok((layout, CacheOutcome::Hit {
                load_us: t.elapsed().as_micros() as u64,
            }));
        }
    }

    // Miss: compile, serialize, atomic-write.
    let t_compile = Instant::now();
    let layout = compile_module(module, mem_base, mem_size, entrypoint_fn_idx)?;
    let compile_us = t_compile.elapsed().as_micros() as u64;

    let blob = encode_fjc1(&layout);
    fs::create_dir_all(dir)?;
    let t_write = Instant::now();
    // Write to a .tmp sibling first, then rename — guarantees that a
    // crash mid-write doesn't leave a half-formed blob that the next
    // run would treat as a (corrupted) hit.
    let tmp_path = dir.join(format!("{key}.fjc1.tmp"));
    fs::write(&tmp_path, &blob)?;
    fs::rename(&tmp_path, &path)?;
    let write_us = t_write.elapsed().as_micros() as u64;

    Ok((layout, CacheOutcome::Miss { compile_us, write_us }))
}

/// Default cache directory. `~/.cache/folkering/jit/` on Unix,
/// `%LOCALAPPDATA%\folkering\jit\` on Windows. Returns `None` if
/// neither environment variable is set (CI runners, etc.) — in that
/// case the caller can pass an explicit dir or disable caching.
pub fn default_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("folkering").join("jit"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME")
            .map(|d| PathBuf::from(d).join(".cache").join("folkering").join("jit"))
    }
}

// ── Internals ────────────────────────────────────────────────────────

fn compute_cache_key(
    wasm_bytes: &[u8],
    mem_base: u64,
    mem_size: u32,
    entrypoint_fn_idx: u32,
) -> String {
    let mut h = Sha256::new();
    h.update(JIT_VERSION.to_le_bytes());
    h.update(wasm_bytes);
    h.update(mem_base.to_le_bytes());
    h.update(mem_size.to_le_bytes());
    h.update(entrypoint_fn_idx.to_le_bytes());
    let digest = h.finalize();
    // First 16 bytes (128 bits) hex — collision probability is
    // negligible for the spike's usage and keeps filenames readable.
    let mut hex = String::with_capacity(32);
    for b in &digest[..16] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

fn encode_fjc1(layout: &ModuleLayout) -> Vec<u8> {
    let n_funcs = layout.function_offsets.len() as u32;
    let code_len = layout.code.len() as u32;
    let mut out = Vec::with_capacity(20 + (n_funcs as usize) * 4 + layout.code.len());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // FJC1 format version
    out.extend_from_slice(&layout.entrypoint_offset.to_le_bytes());
    out.extend_from_slice(&n_funcs.to_le_bytes());
    for &off in &layout.function_offsets {
        out.extend_from_slice(&off.to_le_bytes());
    }
    out.extend_from_slice(&code_len.to_le_bytes());
    out.extend_from_slice(&layout.code);
    out
}

fn decode_fjc1(buf: &[u8]) -> Result<ModuleLayout, CacheError> {
    let mut p = 0usize;
    let magic = read_u32(buf, &mut p)?;
    if magic != MAGIC { return Err(CacheError::BadMagic); }
    let ver = read_u32(buf, &mut p)?;
    if ver != 1 { return Err(CacheError::BadVersion(ver)); }
    let entrypoint_offset = read_u32(buf, &mut p)?;
    let n_funcs = read_u32(buf, &mut p)? as usize;
    let mut function_offsets = Vec::with_capacity(n_funcs);
    for _ in 0..n_funcs {
        function_offsets.push(read_u32(buf, &mut p)?);
    }
    let code_len = read_u32(buf, &mut p)? as usize;
    if p + code_len > buf.len() { return Err(CacheError::Truncated); }
    let code = buf[p..p + code_len].to_vec();
    Ok(ModuleLayout { code, function_offsets, entrypoint_offset })
}

fn read_u32(buf: &[u8], p: &mut usize) -> Result<u32, CacheError> {
    if *p + 4 > buf.len() { return Err(CacheError::Truncated); }
    let v = u32::from_le_bytes([buf[*p], buf[*p + 1], buf[*p + 2], buf[*p + 3]]);
    *p += 4;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm_lower::WasmOp;
    use crate::wasm_module::{FuncSig, FunctionBody, Module};

    /// Smallest possible module — one fn, returns `i32.const 42`.
    /// Suffices for all cache-shape tests; we don't need a complex
    /// program here, only a deterministic one.
    fn trivial_module() -> Module {
        let sig = FuncSig {
            params: alloc::vec::Vec::new(),
            results: alloc::vec![0x7F], // i32
        };
        let body = FunctionBody {
            num_locals: 0,
            local_types: alloc::vec::Vec::new(),
            ops: alloc::vec![WasmOp::I32Const(42), WasmOp::End],
        };
        Module {
            types: alloc::vec![sig],
            func_types: alloc::vec![0],
            globals: alloc::vec::Vec::new(),
            exports: alloc::vec::Vec::new(),
            data: alloc::vec::Vec::new(),
            bodies: alloc::vec![body],
        }
    }

    #[test]
    fn fjc1_roundtrip_preserves_layout() {
        let m = trivial_module();
        let layout = compile_module(&m, 0, 64 * 1024, 0).expect("compile");
        let blob = encode_fjc1(&layout);
        let layout2 = decode_fjc1(&blob).expect("decode");
        assert_eq!(layout.code, layout2.code);
        assert_eq!(layout.function_offsets, layout2.function_offsets);
        assert_eq!(layout.entrypoint_offset, layout2.entrypoint_offset);
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let buf = vec![0xFFu8; 32];
        assert!(matches!(decode_fjc1(&buf), Err(CacheError::BadMagic)));
    }

    #[test]
    fn cache_key_changes_with_mem_base() {
        let k1 = compute_cache_key(b"abc", 0, 64 * 1024, 0);
        let k2 = compute_cache_key(b"abc", 0xBABE, 64 * 1024, 0);
        assert_ne!(k1, k2, "different mem_base must produce different keys");
    }

    #[test]
    fn cache_key_changes_with_wasm_bytes() {
        let k1 = compute_cache_key(b"abc", 0, 64 * 1024, 0);
        let k2 = compute_cache_key(b"abd", 0, 64 * 1024, 0);
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_changes_with_entrypoint() {
        let k1 = compute_cache_key(b"abc", 0, 64 * 1024, 0);
        let k2 = compute_cache_key(b"abc", 0, 64 * 1024, 1);
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_stable_for_same_inputs() {
        let k1 = compute_cache_key(b"abc", 0, 64 * 1024, 0);
        let k2 = compute_cache_key(b"abc", 0, 64 * 1024, 0);
        assert_eq!(k1, k2);
    }

    #[test]
    fn second_compile_is_cache_hit() {
        let tmp = std::env::temp_dir().join(format!("fjit-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let m = trivial_module();
        let wasm_bytes = b"FAKE_WASM_FOR_TEST_KEY";
        let (l1, o1) = cached_compile_module(wasm_bytes, &m, 0, 64 * 1024, 0, Some(&tmp))
            .expect("first compile");
        assert!(matches!(o1, CacheOutcome::Miss { .. }), "first call must miss");

        let (l2, o2) = cached_compile_module(wasm_bytes, &m, 0, 64 * 1024, 0, Some(&tmp))
            .expect("second compile");
        assert!(matches!(o2, CacheOutcome::Hit { .. }),
                "second call must hit, got {o2:?}");

        // Hit must produce byte-identical output. Without this property
        // the cache is a correctness hazard, not just a perf hazard.
        assert_eq!(l1.code, l2.code);
        assert_eq!(l1.function_offsets, l2.function_offsets);
        assert_eq!(l1.entrypoint_offset, l2.entrypoint_offset);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disabled_cache_returns_disabled_outcome() {
        let m = trivial_module();
        let (_, o) = cached_compile_module(b"any", &m, 0, 64 * 1024, 0, None)
            .expect("compile");
        assert!(matches!(o, CacheOutcome::Disabled));
    }

    /// A corrupted / truncated / wrong-version blob at the cache path
    /// must NOT panic — it should silently recompile and overwrite.
    /// Anything else is a denial-of-service hazard if a stale or
    /// malformed file lands in the cache dir.
    #[test]
    fn corrupted_blob_falls_through_to_recompile() {
        let tmp = std::env::temp_dir().join(format!("fjit-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let m = trivial_module();
        let wasm_bytes = b"key_for_corruption_test";
        let key = compute_cache_key(wasm_bytes, 0, 64 * 1024, 0);
        let path = tmp.join(format!("{key}.fjc1"));
        std::fs::write(&path, b"this is not a valid fjc1 blob").unwrap();

        let (_, o) = cached_compile_module(wasm_bytes, &m, 0, 64 * 1024, 0, Some(&tmp))
            .expect("compile despite corrupt cache");
        assert!(matches!(o, CacheOutcome::Miss { .. }),
                "corrupted hit must fall through to recompile, got {o:?}");

        // After recompile the file should now decode cleanly.
        let blob = std::fs::read(&path).unwrap();
        assert!(decode_fjc1(&blob).is_ok(), "recompile should overwrite with valid blob");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
