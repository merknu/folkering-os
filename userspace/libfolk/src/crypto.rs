//! Minimal SHA-256 for no_std — Cryptographic Lineage
//!
//! Used to sign and verify LLM-generated WASM code.
//! Every WASM binary gets a signature: SHA256(prompt + wasm_hash + timestamp).
//! The OS refuses to execute unsigned or tampered code.

/// SHA-256 hash output (32 bytes)
pub type Sha256Hash = [u8; 32];

/// SHA-256 initial hash values
const H: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 round constants
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Compute SHA-256 hash of arbitrary data. No allocation needed.
pub fn sha256(data: &[u8]) -> Sha256Hash {
    let mut h = H;
    let bit_len = (data.len() as u64) * 8;

    // Process complete 64-byte blocks
    let mut offset = 0;
    while offset + 64 <= data.len() {
        compress(&mut h, &data[offset..offset + 64]);
        offset += 64;
    }

    // Pad the final block(s)
    let remaining = data.len() - offset;
    let mut block = [0u8; 128]; // max 2 blocks for padding
    block[..remaining].copy_from_slice(&data[offset..]);
    block[remaining] = 0x80; // append bit '1'

    if remaining < 56 {
        // Fits in one block
        block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut h, &block[..64]);
    } else {
        // Need two blocks
        compress(&mut h, &block[..64]);
        let mut pad2 = [0u8; 64];
        pad2[56..64].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut h, &pad2);
    }

    // Produce final hash
    let mut result = [0u8; 32];
    for i in 0..8 {
        result[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    result
}

/// SHA-256 compression function — processes one 64-byte block
fn compress(state: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];

    // Prepare message schedule
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }

    // Working variables
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *state;

    // 64 rounds
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(hh);
}

/// Format a SHA-256 hash as hex string (64 chars). Uses a fixed buffer.
pub fn hash_to_hex(hash: &Sha256Hash, buf: &mut [u8; 64]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for i in 0..32 {
        buf[i * 2] = HEX[(hash[i] >> 4) as usize];
        buf[i * 2 + 1] = HEX[(hash[i] & 0xf) as usize];
    }
}

/// Compute an intention signature: SHA256(prompt_bytes ++ wasm_hash ++ timestamp_bytes)
/// This binds the WASM binary to the intent that created it.
pub fn intention_signature(prompt: &[u8], wasm_hash: &Sha256Hash, timestamp: u64) -> Sha256Hash {
    // Concatenate: prompt + wasm_hash + timestamp
    // We hash incrementally by hashing the concatenation
    let ts_bytes = timestamp.to_le_bytes();
    let total_len = prompt.len() + 32 + 8;

    // Build a temporary buffer (max 4KB prompt + 40 bytes overhead)
    let mut buf = [0u8; 4136]; // 4096 + 32 + 8
    let usable = total_len.min(buf.len());
    let prompt_len = usable.saturating_sub(40).min(prompt.len());

    buf[..prompt_len].copy_from_slice(&prompt[..prompt_len]);
    buf[prompt_len..prompt_len + 32].copy_from_slice(wasm_hash);
    buf[prompt_len + 32..prompt_len + 40].copy_from_slice(&ts_bytes);

    sha256(&buf[..prompt_len + 40])
}

/// Verify that a WASM binary matches its claimed signature.
pub fn verify_signature(
    wasm_bytes: &[u8],
    prompt: &[u8],
    timestamp: u64,
    claimed_sig: &Sha256Hash,
) -> bool {
    let wasm_hash = sha256(wasm_bytes);
    let computed_sig = intention_signature(prompt, &wasm_hash, timestamp);
    computed_sig == *claimed_sig
}
