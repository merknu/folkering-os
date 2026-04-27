//! Auto-build the WASM drivers that compositor `include_bytes!`s.
//!
//! For each entry in `DRIVERS`, if the .wasm is missing or older than its
//! .rs source, invoke `rustc` to rebuild. Build flags are byte-deterministic
//! against the previously hand-built blobs (verified during initial check-in).
//!
//! The same recipe lives in `tools/build-drivers.sh` for manual invocation;
//! both end up calling `rustc` with identical flags. Keeping it in build.rs
//! means a fresh clone builds correctly without a separate setup step.

use std::path::{Path, PathBuf};
use std::process::Command;

// (source filename, output wasm filename). Bootstrap output keeps the
// historical `_v1` suffix because compositor pins to that path for its
// legacy fallback driver.
const DRIVERS: &[(&str, &str)] = &[
    ("e1000_bootstrap.rs", "e1000_bootstrap_v1.wasm"),
    ("e1000_v2.rs",        "e1000_v2.wasm"),
    ("virtio_net_v1.rs",   "virtio_net_v1.wasm"),
];

fn main() {
    // From userspace/compositor/, the drivers/ folder is two levels up.
    let drivers_dir = Path::new("..").join("..").join("drivers");
    let drivers_dir = match drivers_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            // Don't fail the build — fall back to the existing wasms (if any).
            // Useful for IDE / out-of-tree builds that can't see drivers/.
            eprintln!("cargo:warning=drivers/ dir not found ({e}); skipping wasm rebuild");
            return;
        }
    };

    for (src_name, out_name) in DRIVERS {
        let src = drivers_dir.join(src_name);
        let out = drivers_dir.join(out_name);

        // Tell cargo to rerun us if either source or output changes on disk.
        println!("cargo:rerun-if-changed={}", src.display());
        println!("cargo:rerun-if-changed={}", out.display());

        if !src.exists() {
            eprintln!("cargo:warning=missing driver source: {}", src.display());
            continue;
        }

        if needs_rebuild(&src, &out) {
            if let Err(e) = build_driver(&src, &out) {
                // Hard fail — compositor's include_bytes! will fail next anyway.
                panic!("driver build failed for {src_name}: {e}");
            }
        }
    }
}

fn needs_rebuild(src: &Path, out: &Path) -> bool {
    let Ok(out_meta) = out.metadata() else { return true; };
    let Ok(src_meta) = src.metadata() else { return true; };
    let (Ok(out_t), Ok(src_t)) = (out_meta.modified(), src_meta.modified()) else {
        return true;
    };
    out_t < src_t
}

fn build_driver(src: &Path, out: &Path) -> Result<(), String> {
    eprintln!("cargo:warning=building {} -> {}", src.display(), out.display());

    let status = Command::new("rustc")
        .args([
            "--target", "wasm32-unknown-unknown",
            "--edition", "2021",
            "--crate-type", "cdylib",
            "-C", "opt-level=z",
            "-C", "strip=symbols",
            "-C", "link-arg=--no-entry",
            "-C", "link-arg=--strip-all",
            "-o",
        ])
        .arg(out)
        .arg(src)
        .status()
        .map_err(|e| format!("failed to spawn rustc: {e}"))?;

    if !status.success() {
        return Err(format!("rustc exited with {status}"));
    }
    Ok(())
}

#[allow(dead_code)]
fn _drivers_constant_keeps_pathbuf_alive() -> PathBuf {
    // Suppresses an "unused import" warning when only DRIVERS is used.
    PathBuf::new()
}
