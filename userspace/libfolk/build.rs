//! Tells cargo to invalidate libfolk's compilation cache when
//! FOLKERING_PROXY_IP / FOLKERING_PROXY_PORT change between builds.
//!
//! Without this, `option_env!` in `proxy_config.rs` silently keeps
//! the IP / port baked at the time of last source-change rebuild —
//! which is how Issue #99 (daemon SYN'ing the SLIRP gateway forever)
//! survived four debug sessions before the actual cause was spotted.
//! Same list lives in `kernel/build.rs` and
//! `userspace/draug-streamer/build.rs`; keep all three in sync.

fn main() {
    println!("cargo:rerun-if-env-changed=FOLKERING_PROXY_IP");
    println!("cargo:rerun-if-env-changed=FOLKERING_PROXY_PORT");
}
