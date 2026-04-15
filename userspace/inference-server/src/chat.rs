//! ChatML template wrapping + tokenizer helpers.

use libtensor::tokenizer::BpeTokenizer;

/// Wrap a user query in ChatML format with system prompt (ULTRA 41).
/// Returns number of bytes written to output.
pub fn wrap_chat_template(query: &[u8], output: &mut [u8]) -> usize {
    // NOTE: Newline before <|im_end|> ensures greedy tokenizer doesn't merge
    // the last text char with '<' (e.g. ".<" as a single token breaking <|im_end|>).
    let sys = b"<|im_start|>system\nYou are Folkering, a helpful AI on Folkering OS (bare-metal Rust, WASM apps).\nKeep <think> brief. Give direct answers.\nTools: <|tool|>read FILENAME<|/tool|>, <|tool|>write FILENAME CONTENT<|/tool|>, <|tool|>ask_gemini PROMPT<|/tool|>.\nUse ask_gemini for coding tasks. Apps must target WASM (no_std Rust or C compiled to .wasm).\n<|im_end|>\n";
    let user_pre = b"<|im_start|>user\n";
    let user_suf = b"\n<|im_end|>\n<|im_start|>assistant\n";

    let total = sys.len() + user_pre.len() + query.len() + user_suf.len();
    if total > output.len() {
        return 0;
    }

    let mut pos = 0;
    output[pos..pos + sys.len()].copy_from_slice(sys);
    pos += sys.len();
    output[pos..pos + user_pre.len()].copy_from_slice(user_pre);
    pos += user_pre.len();
    output[pos..pos + query.len()].copy_from_slice(query);
    pos += query.len();
    output[pos..pos + user_suf.len()].copy_from_slice(user_suf);
    pos += user_suf.len();
    pos
}

/// Find token ID for a specific string in the vocabulary (ULTRA 39).
/// Returns u32::MAX if not found.
pub fn find_token_id(tokenizer: &BpeTokenizer, needle: &[u8]) -> u32 {
    for id in 0..tokenizer.vocab_size() {
        if tokenizer.token_bytes(id as u32) == needle {
            return id as u32;
        }
    }
    u32::MAX
}

/// ULTRA 30: TCG breathing room — short busy-wait to let QEMU process interrupts.
/// ~1000 iterations of spin_loop ≈ ~1ms in QEMU TCG.
#[inline]
pub fn tcg_breathe() {
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
}
