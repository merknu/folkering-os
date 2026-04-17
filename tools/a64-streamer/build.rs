use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("secret.key");

    // Priority: SECRET_KEY_PATH env > local secret.key > ~/.folkering/secret.key
    let candidates: Vec<PathBuf> = vec![
        env::var("SECRET_KEY_PATH").ok().map(PathBuf::from).unwrap_or_default(),
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("secret.key"),
        dirs_next().join("secret.key"),
    ];

    for path in &candidates {
        if path.as_os_str().is_empty() {
            continue;
        }
        if path.exists() {
            let data = fs::read(path).expect("failed to read secret.key");
            assert_eq!(data.len(), 32, "secret.key must be exactly 32 bytes");
            fs::write(&dest, &data).expect("failed to write secret.key to OUT_DIR");
            println!("cargo:rerun-if-changed={}", path.display());
            println!("cargo:rustc-env=SECRET_KEY_PATH={}", dest.display());
            return;
        }
    }

    panic!(
        "secret.key not found. Create a 32-byte key:\n\
         \n  head -c 32 /dev/urandom > tools/a64-streamer/secret.key\n\
         \nor set SECRET_KEY_PATH env var to point to it.\n\
         Searched: {:?}",
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
