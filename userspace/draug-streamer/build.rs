//! Mirrors `tools/a64-streamer/build.rs` because draug-streamer
//! `#[path]`-includes auth.rs from a64-streamer rather than depending
//! on it as a Cargo crate. That bypass means a64-streamer's build.rs
//! never runs, so we have to set up `OUT_DIR/secret.key` ourselves.
//!
//! Without this file: `cargo build` fails with
//!   error: environment variable `OUT_DIR` not defined at compile time
//!     --> .../tools/a64-streamer/src/auth.rs:49
//!         include_bytes!(concat!(env!("OUT_DIR"), "/secret.key"));

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    // Re-evaluate `option_env!` constants when these toggle (#99).
    // draug-streamer hard-codes the SLIRP target unless overridden;
    // without these directives cargo silently kept the old value
    // baked in across rebuilds.
    println!("cargo:rerun-if-env-changed=FOLKERING_STREAMER_IP");
    println!("cargo:rerun-if-env-changed=FOLKERING_STREAMER_PORT");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("secret.key");

    // Same priority as a64-streamer/build.rs:
    //   1. SECRET_KEY_PATH env var
    //   2. tools/a64-streamer/secret.key (the canonical source)
    //   3. ~/.folkering/secret.key
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let canonical = manifest.join("../../tools/a64-streamer/secret.key");
    let candidates: Vec<PathBuf> = vec![
        env::var("SECRET_KEY_PATH").ok().map(PathBuf::from).unwrap_or_default(),
        canonical,
        dirs_next().join("secret.key"),
    ];

    for path in &candidates {
        if path.as_os_str().is_empty() { continue; }
        if path.exists() {
            let data = fs::read(path).expect("failed to read secret.key");
            assert_eq!(data.len(), 32, "secret.key must be exactly 32 bytes");
            fs::write(&dest, &data).expect("failed to write secret.key to OUT_DIR");
            println!("cargo:rerun-if-changed={}", path.display());
            return;
        }
    }

    panic!(
        "secret.key not found. Create a 32-byte key at \
         tools/a64-streamer/secret.key, or set SECRET_KEY_PATH. \
         Searched: {:?}", candidates
    );
}

fn dirs_next() -> PathBuf {
    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".folkering")
    } else {
        PathBuf::new()
    }
}
