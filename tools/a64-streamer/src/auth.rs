//! CODE-frame authentication via HMAC-SHA256.
//!
//! The Pi-side daemon refuses to mmap+execute any CODE frame that
//! doesn't carry a valid 32-byte HMAC-SHA256 tag over the code
//! bytes, computed with the shared secret embedded in both the
//! daemon and the client. This is the single gate that prevents
//! anyone on the LAN from shipping arbitrary A64 machine code to
//! our worker — without it, the framed TCP protocol would be a
//! fully-open RCE surface.
//!
//! The shared key is a 32-byte random blob stored in
//! `tools/a64-streamer/secret.key` and `include_bytes!`'d into both
//! the daemon crate and the draug-streamer Folkering userspace
//! binary. In a real deployment the key would be rotated and
//! distributed out-of-band; for this repo, committing a development
//! key keeps the build reproducible and the security model visible.
//!
//! Tag format appended to a CODE frame's payload:
//!
//! ```text
//!   +--------+-------------+--------+-------+
//!   | HEADER | code bytes  |  HMAC  |       |
//!   |  5 B   |   N bytes   | 32 B   |       |
//!   +--------+-------------+--------+-------+
//!                     ^                ^
//!                     |                |
//!                 signed by       computed over
//!               clients as tag     code bytes only
//! ```
//!
//! The frame's u32 length covers both the code bytes and the tag
//! (`N + 32`). The daemon splits on the final 32 bytes.
//!
//! Module is `no_std`-clean by construction — uses only `hmac` and
//! `sha2` with default features disabled, so it compiles into the
//! Folkering userspace binary without pulling std. (The inner
//! attribute would be invalid at this position anyway; the surrounding
//! crate controls the no_std gate.)

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Shared 32-byte HMAC-SHA256 key. Embedded from
/// `tools/a64-streamer/secret.key`. The daemon and every client must
/// read the same file so their computed tags match; rotating the
/// key means rebuilding all participants.
pub const SHARED_KEY: &[u8; 32] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/a64-streamer/secret.key"));

/// Length of the HMAC-SHA256 tag appended to signed payloads.
pub const TAG_LEN: usize = 32;

/// Compute the 32-byte HMAC-SHA256 tag over `data` using the shared
/// key. Callers append the tag to the frame payload before sending.
pub fn sign(data: &[u8]) -> [u8; TAG_LEN] {
    type HmacSha256 = Hmac<Sha256>;
    // `new_from_slice` accepts any key length; for our fixed 32-byte
    // key this never errors.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(SHARED_KEY)
        .expect("HMAC can take a 32-byte key");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&result);
    out
}

/// Verify that `tag` is a valid HMAC-SHA256 of `data` under the
/// shared key. Returns `true` iff the tag matches; comparison is
/// constant-time (via `hmac::Mac::verify_slice`) to prevent timing-
/// based tag extraction.
pub fn verify(data: &[u8], tag: &[u8]) -> bool {
    if tag.len() != TAG_LEN {
        return false;
    }
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(SHARED_KEY)
        .expect("HMAC can take a 32-byte key");
    mac.update(data);
    mac.verify_slice(tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_roundtrip() {
        let msg = b"hello folkering";
        let tag = sign(msg);
        assert!(verify(msg, &tag), "freshly-signed tag must verify");
    }

    #[test]
    fn tamper_detected() {
        let msg = b"hello folkering";
        let tag = sign(msg);
        let tampered: &[u8] = b"hello folkerinG"; // one bit flipped
        assert!(!verify(tampered, &tag), "altered message must fail");
    }

    #[test]
    fn bad_tag_length_rejected() {
        let msg = b"anything";
        assert!(!verify(msg, &[0u8; 16]), "short tag rejected");
        assert!(!verify(msg, &[0u8; 64]), "long tag rejected");
    }

    #[test]
    fn wrong_tag_rejected() {
        let msg = b"hello";
        let mut tag = sign(msg);
        tag[0] ^= 1; // flip a bit
        assert!(!verify(msg, &tag), "altered tag must fail");
    }
}
