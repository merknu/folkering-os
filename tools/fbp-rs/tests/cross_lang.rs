//! Cross-language FBP round-trip test.
//!
//! Loads `tests/fixtures/cross_lang_fixture.fbp` (a committed binary
//! artifact produced by the Python encoder in the separate
//! `folkering-proxy` repo) and verifies that the Rust deserializer
//! reads back exactly the same fields as the Python side wrote.
//!
//! If this test starts failing, it means the Python encoder and the
//! Rust decoder have drifted — either the spec changed or one side
//! has a bug. To regenerate the fixture, run
//! `python tests/gen_cross_fixture.py` from the folkering-proxy repo;
//! it will write to both its own `fixtures/` directory AND to
//! `<this dir>/tests/fixtures/` if it exists.
//!
//! This crate does NOT depend on folkering-proxy being present on
//! disk — the fixture lives inside this tree as a committed binary.

use fbp_rs::{
    parse_state_update, NodeFlags, ACTION_CLICK, FBP_MAGIC, FBP_VERSION, HEADER_SIZE,
    NODE_SIZE,
};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    // tools/fbp-rs/tests/cross_lang.rs → tests/fixtures/ relative to the crate manifest
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("cross_lang_fixture.fbp");
    p
}

#[test]
fn sanity_constants() {
    assert_eq!(FBP_MAGIC, 0x4B4C4F46);
    assert_eq!(HEADER_SIZE, 32);
    assert_eq!(NODE_SIZE, 48);
    // FBP_VERSION is a string sanity marker (not a wire field yet).
    // Bump both sides together when the spec changes.
    assert_eq!(FBP_VERSION, "0.1.0");
}

#[test]
fn cross_lang_state_update_round_trip() {
    let path = fixture_path();
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "Failed to read fixture {}: {e}. \
             Run `python tests/gen_cross_fixture.py` in the folkering-proxy repo.",
            path.display()
        )
    });

    // Align the loaded byte buffer onto a Vec<u64> storage so that the
    // underlying pointer is guaranteed 8-byte aligned. Otherwise Rust's
    // Vec<u8> allocator only guarantees 1-byte alignment, which would
    // cause `parse_state_update` to correctly reject the slice with
    // `NodeSliceMisaligned` on strict platforms.
    let mut aligned: Vec<u64> = vec![0u64; (bytes.len() + 7) / 8];
    // SAFETY: we're just copying `bytes.len()` bytes from bytes.as_ptr()
    // into the start of the aligned u64 buffer. The dst cast is fine
    // because Vec<u64> data is properly aligned and has enough storage.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            aligned.as_mut_ptr() as *mut u8,
            bytes.len(),
        );
    }
    let aligned_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(aligned.as_ptr() as *const u8, bytes.len())
    };

    let parsed = parse_state_update(aligned_bytes).expect("parse fixture");

    assert_eq!(parsed.page_id, 0x0123456789ABCDEF);
    assert_eq!(parsed.viewport_w, 1024);
    assert_eq!(parsed.viewport_h, 768);
    assert_eq!(parsed.nodes.len(), 4);

    // Node 0: <html>
    let html = &parsed.nodes[0];
    assert_eq!(parsed.tag(html), b"html");
    assert_eq!(parsed.text(html), b"");
    assert!(html.flags.contains(NodeFlags::IS_VISIBLE));
    assert_eq!(html.parent_id, 0);
    assert_eq!(html.first_child_id, 2);
    assert_eq!(html.last_child_id, 2);
    assert_eq!(html.bounds_w, 1024);
    assert_eq!(html.bounds_h, 768);

    // Node 1: <body>
    let body = &parsed.nodes[1];
    assert_eq!(parsed.tag(body), b"body");
    assert_eq!(body.parent_id, 1);
    assert_eq!(body.first_child_id, 3);
    assert_eq!(body.last_child_id, 4);

    // Node 2: <h1>Hello folkering</h1>
    let h1 = &parsed.nodes[2];
    assert_eq!(parsed.tag(h1), b"h1");
    assert_eq!(parsed.text(h1), b"Hello folkering");
    assert_eq!(h1.parent_id, 2);
    assert_eq!(h1.next_sibling_id, 4);
    assert_eq!(h1.bounds_x, 20);
    assert_eq!(h1.bounds_y, 20);
    assert_eq!(h1.bounds_w, 400);
    assert_eq!(h1.bounds_h, 40);

    // Node 3: <a>Click me</a>
    let link = &parsed.nodes[3];
    assert_eq!(parsed.tag(link), b"a");
    assert_eq!(parsed.text(link), b"Click me");
    assert!(link.flags.contains(NodeFlags::IS_VISIBLE));
    assert!(link.flags.contains(NodeFlags::IS_LINK));
    assert!(link.flags.contains(NodeFlags::IS_INTERACTABLE));
    assert_eq!(link.parent_id, 2);
    assert_eq!(link.bounds_x, 20);
    assert_eq!(link.bounds_y, 80);
    assert_eq!(link.bounds_w, 120);
    assert_eq!(link.bounds_h, 24);
}

// A quick sanity check on InteractionEvent parsing, built from a
// hand-rolled buffer (no Python fixture needed for this one).
#[test]
fn interaction_event_hand_built() {
    // serialize_interaction_event(ACTION_CLICK, node_id=7, data=b"")
    let mut buf = [0u8; 12];
    buf[0] = fbp_rs::MSG_INTERACTION_EVENT;
    buf[1] = ACTION_CLICK;
    buf[4..8].copy_from_slice(&7u32.to_le_bytes());
    let ev = fbp_rs::parse_interaction_event(&buf).unwrap();
    assert_eq!(ev.action, ACTION_CLICK);
    assert_eq!(ev.node_id, 7);
    assert!(ev.data.is_empty());
}
