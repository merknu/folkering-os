use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let _arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    // Tell cargo to pass the linker script to the linker
    println!("cargo:rustc-link-arg=-Tlinker.ld");
    println!("cargo:rerun-if-changed=linker.ld");

    // Re-evaluate `option_env!` constants when these toggle. Without
    // these directives cargo's incremental cache silently keeps the
    // last-baked PROXY_IP / GEMINI_PORT / etc., which previously cost
    // four debug sessions on Issue #99 (daemon kept SYN'ing the SLIRP
    // gateway because the env var changed but the old IP stayed in
    // the binary). Same list lives in userspace/libfolk/build.rs and
    // userspace/draug-streamer/build.rs — keep all three in sync.
    println!("cargo:rerun-if-env-changed=FOLKERING_PROXY_IP");
    println!("cargo:rerun-if-env-changed=FOLKERING_PROXY_PORT");
    println!("cargo:rerun-if-env-changed=FOLKERING_GEMINI_PORT");

    // Resolve the shared HMAC key used by `kernel::jit` to sign CODE
    // frames sent to the Pi-side a64-stream-daemon. Same priority list
    // as tools/a64-streamer/build.rs so the two stay aligned.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("secret.key");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_secret = manifest
        .parent()
        .unwrap()
        .join("tools/a64-streamer/secret.key");

    let candidates: Vec<PathBuf> = vec![
        env::var("SECRET_KEY_PATH").ok().map(PathBuf::from).unwrap_or_default(),
        workspace_secret,
        dirs_next().join("secret.key"),
    ];

    for path in &candidates {
        if path.as_os_str().is_empty() { continue; }
        if path.exists() {
            let data = fs::read(path).expect("failed to read secret.key");
            assert_eq!(data.len(), 32, "secret.key must be exactly 32 bytes");
            fs::write(&dest, &data).expect("failed to write secret.key to OUT_DIR");
            println!("cargo:rerun-if-changed={}", path.display());
            println!("cargo:rustc-env=KERNEL_SECRET_KEY_PATH={}", dest.display());
            return;
        }
    }

    panic!(
        "secret.key not found for kernel HMAC signing. Searched: {:?}",
        candidates
    );
}

fn dirs_next() -> PathBuf {
    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".folkering")
    } else {
        PathBuf::new()
    }
}
