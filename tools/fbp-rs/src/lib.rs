//! Folkering Binary Protocol (FBP) — zero-copy deserializer + optional serializer.
//!
//! Wire format is documented in `../../folkering-proxy/docs/fbp-spec.md`.
//!
//! This crate is `#![no_std]`-compatible and has two builds:
//!
//! 1. **Default (parse-only).** No allocations, no std. Used by
//!    `folk_browser` WASM as a zero-copy deserializer. The returned
//!    `StateUpdate` borrows the caller's slice directly.
//! 2. **With `alloc` feature.** Enables the host-side serializer
//!    (`serialize_state_update`, `serialize_interaction_event`) that
//!    the `folkering-proxy` Rust server uses to produce FBP payloads.
//!    Still `#![no_std]` — it just pulls in `extern crate alloc`.
//!
//! The parse path is identical in both builds; the serializer is
//! only compiled when the feature is enabled.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "alloc")]
extern crate alloc;

use core::mem::size_of;

// ── Protocol constants (must match docs/fbp-spec.md) ────────────────

/// Human-readable spec revision. Not part of the wire format yet;
/// used as a sanity signal that the Python encoder and this Rust
/// decoder were built against the same version of the spec. Bump
/// both sides in lockstep whenever `docs/fbp-spec.md` changes.
pub const FBP_VERSION: &str = "0.1.0";

pub const FBP_MAGIC: u32 = 0x4B4C4F46; // "FOLK" little-endian

pub const MSG_DOM_STATE_UPDATE: u8 = 0x01;
pub const MSG_INTERACTION_EVENT: u8 = 0x02;

pub const ACTION_CLICK: u8 = 0x01;
pub const ACTION_TYPE: u8 = 0x02;
pub const ACTION_SCROLL: u8 = 0x03;
pub const ACTION_FOCUS: u8 = 0x04;
pub const ACTION_BLUR: u8 = 0x05;
pub const ACTION_KEY_DOWN: u8 = 0x06;

pub const HEADER_SIZE: usize = 32;
pub const NODE_SIZE: usize = 48;

// ── NodeFlags bitfield ──────────────────────────────────────────────

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeFlags(pub u16);

impl NodeFlags {
    pub const NONE: Self = NodeFlags(0);
    pub const IS_INTERACTABLE: Self = NodeFlags(1 << 0);
    pub const IS_VISIBLE: Self = NodeFlags(1 << 1);
    pub const IS_FOCUSABLE: Self = NodeFlags(1 << 2);
    pub const HAS_TEXT_INPUT: Self = NodeFlags(1 << 3);
    pub const IS_BUTTON: Self = NodeFlags(1 << 4);
    pub const IS_LINK: Self = NodeFlags(1 << 5);
    pub const IS_CHECKBOX: Self = NodeFlags(1 << 6);

    #[inline]
    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl core::ops::BitOr for NodeFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        NodeFlags(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for NodeFlags {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

// ── SemanticNode — 48-byte wire layout ──────────────────────────────

/// Byte-exact mirror of the Python `SemanticNode` wire layout.
///
/// Do NOT reorder fields — the `#[repr(C)]` + manual offsets must
/// match `docs/fbp-spec.md` exactly so that the deserializer can cast
/// the raw byte slice into `&[SemanticNode]` in one step.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SemanticNode {
    pub tag_offset: u32,       // +0
    pub tag_len: u16,          // +4
    pub flags: NodeFlags,      // +6
    pub text_offset: u32,      // +8
    pub text_len: u32,         // +12
    pub parent_id: u32,        // +16
    pub first_child_id: u32,   // +20
    pub last_child_id: u32,    // +24
    pub next_sibling_id: u32,  // +28
    pub bounds_x: i32,         // +32
    pub bounds_y: i32,         // +36
    pub bounds_w: u32,         // +40
    pub bounds_h: u32,         // +44
}

// Compile-time sanity check. If this fires, the struct layout has
// drifted from the spec and the deserializer will corrupt data.
const _: () = assert!(size_of::<SemanticNode>() == NODE_SIZE);

// ── Parse errors ────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    TooShort { have: usize, need: usize },
    BadMagic { got: u32 },
    WrongMsgType { got: u8 },
    StringPoolOverflow,
    NodeSliceMisaligned,
    NodeSliceOverflow,
}

// ── DOM_STATE_UPDATE zero-copy view ─────────────────────────────────

/// A borrowed view over a DOM_STATE_UPDATE payload.
///
/// All fields reference slices of the caller's input buffer; no data
/// is copied.
#[derive(Clone, Copy, Debug)]
pub struct StateUpdate<'a> {
    pub page_id: u64,
    pub viewport_w: u32,
    pub viewport_h: u32,
    pub string_pool: &'a [u8],
    pub nodes: &'a [SemanticNode],
}

impl<'a> StateUpdate<'a> {
    /// Resolve a node's tag name against the string pool.
    ///
    /// Out-of-range `tag_offset`/`tag_len` → empty slice. The parser
    /// validates the node-array envelope but not each node's string
    /// references, so a malformed payload could point past the pool;
    /// returning `&[]` lets a no_std consumer keep running instead of
    /// panicking into an infinite loop via its panic handler.
    #[inline]
    pub fn tag<'b>(&'b self, node: &SemanticNode) -> &'a [u8]
    where
        'a: 'b,
    {
        let off = node.tag_offset as usize;
        let len = node.tag_len as usize;
        match off.checked_add(len) {
            Some(end) if end <= self.string_pool.len() => &self.string_pool[off..end],
            _ => &[],
        }
    }

    /// Resolve a node's text content against the string pool.
    ///
    /// Same out-of-range policy as `tag()` — returns `&[]` rather than
    /// panicking on malformed offsets.
    #[inline]
    pub fn text<'b>(&'b self, node: &SemanticNode) -> &'a [u8]
    where
        'a: 'b,
    {
        let off = node.text_offset as usize;
        let len = node.text_len as usize;
        match off.checked_add(len) {
            Some(end) if end <= self.string_pool.len() => &self.string_pool[off..end],
            _ => &[],
        }
    }
}

/// Parse a DOM_STATE_UPDATE byte buffer into a zero-copy view.
pub fn parse_state_update(buf: &[u8]) -> Result<StateUpdate<'_>, ParseError> {
    if buf.len() < HEADER_SIZE {
        return Err(ParseError::TooShort {
            have: buf.len(),
            need: HEADER_SIZE,
        });
    }

    let magic = read_u32_le(buf, 0);
    if magic != FBP_MAGIC {
        return Err(ParseError::BadMagic { got: magic });
    }

    let msg_type = buf[4];
    if msg_type != MSG_DOM_STATE_UPDATE {
        return Err(ParseError::WrongMsgType { got: msg_type });
    }

    let page_id = read_u64_le(buf, 8);
    let pool_len = read_u32_le(buf, 16) as usize;
    let node_count = read_u32_le(buf, 20) as usize;
    let viewport_w = read_u32_le(buf, 24);
    let viewport_h = read_u32_le(buf, 28);

    // String pool starts right after the header.
    let pool_start = HEADER_SIZE;
    let pool_end = pool_start
        .checked_add(pool_len)
        .ok_or(ParseError::StringPoolOverflow)?;
    if pool_end > buf.len() {
        return Err(ParseError::TooShort {
            have: buf.len(),
            need: pool_end,
        });
    }
    let string_pool = &buf[pool_start..pool_end];

    // Pad to the next 8-byte boundary before the nodes array.
    let pad = (pool_end.wrapping_neg()) & 7;
    let nodes_offset = pool_end + pad;

    // Bounds check BEFORE we cast.
    let nodes_byte_len = node_count
        .checked_mul(NODE_SIZE)
        .ok_or(ParseError::NodeSliceOverflow)?;
    let nodes_end = nodes_offset
        .checked_add(nodes_byte_len)
        .ok_or(ParseError::NodeSliceOverflow)?;
    if nodes_end > buf.len() {
        return Err(ParseError::TooShort {
            have: buf.len(),
            need: nodes_end,
        });
    }

    // Alignment check. The producer is required to 8-align this
    // offset; if the caller's buffer isn't 8-aligned then the cast is
    // undefined behavior and we must reject.
    let node_slice_ptr = buf.as_ptr().wrapping_add(nodes_offset);
    if (node_slice_ptr as usize) & (core::mem::align_of::<SemanticNode>() - 1) != 0 {
        return Err(ParseError::NodeSliceMisaligned);
    }

    // SAFETY: we verified:
    //   - the byte range nodes_offset..nodes_end is inside buf
    //   - the starting pointer is aligned for SemanticNode
    //   - SemanticNode is #[repr(C)] with no padding bytes that depend
    //     on the producer zeroing (flags occupies bytes 6..8 so no
    //     hidden pad byte), so reading arbitrary bytes as a valid u32
    //     / u16 is safe.
    let nodes = unsafe {
        core::slice::from_raw_parts(node_slice_ptr as *const SemanticNode, node_count)
    };

    Ok(StateUpdate {
        page_id,
        viewport_w,
        viewport_h,
        string_pool,
        nodes,
    })
}

// ── INTERACTION_EVENT decoding ──────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct InteractionEvent<'a> {
    pub action: u8,
    pub node_id: u32,
    pub data: &'a [u8],
}

pub fn parse_interaction_event(buf: &[u8]) -> Result<InteractionEvent<'_>, ParseError> {
    if buf.len() < 12 {
        return Err(ParseError::TooShort {
            have: buf.len(),
            need: 12,
        });
    }
    if buf[0] != MSG_INTERACTION_EVENT {
        return Err(ParseError::WrongMsgType { got: buf[0] });
    }
    let action = buf[1];
    // buf[2..4] = padding
    let node_id = read_u32_le(buf, 4);
    let data_len = read_u32_le(buf, 8) as usize;
    let end = 12usize
        .checked_add(data_len)
        .ok_or(ParseError::NodeSliceOverflow)?;
    if end > buf.len() {
        return Err(ParseError::TooShort {
            have: buf.len(),
            need: end,
        });
    }
    Ok(InteractionEvent {
        action,
        node_id,
        data: &buf[12..end],
    })
}

// ── Little-endian byte readers ──────────────────────────────────────

#[inline]
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off], buf[off + 1], buf[off + 2], buf[off + 3],
        buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7],
    ])
}

// ── Optional serializer (enabled via the "alloc" feature) ──────────
//
// Host-side only. Used by folkering-proxy to turn its extracted DOM
// into an FBP byte payload. The folk_browser WASM build does NOT
// enable this feature — it's parse-only.

#[cfg(feature = "alloc")]
pub mod encode {
    //! FBP encoder — mirrors the byte layout asserted by the
    //! deserializer above. Takes owned inputs (`&str`, `Vec<Node>`)
    //! and produces an `alloc::vec::Vec<u8>` ready to be written
    //! to a socket.

    use super::{
        NodeFlags, FBP_MAGIC, HEADER_SIZE, MSG_DOM_STATE_UPDATE,
        MSG_INTERACTION_EVENT, NODE_SIZE,
    };
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Owned version of a SemanticNode used by the serializer.
    ///
    /// The deserializer's `SemanticNode` holds `u32` offsets into an
    /// interned string pool; the encoder needs the actual strings so
    /// it can build that pool. All topology indices are 1-based
    /// (0 = None) to match the wire format directly.
    #[derive(Clone, Debug, Default)]
    pub struct OwnedNode {
        pub tag: String,
        pub text: String,
        pub flags: NodeFlags,
        pub parent: u32,
        pub first_child: u32,
        pub last_child: u32,
        pub next_sibling: u32,
        pub bounds_x: i32,
        pub bounds_y: i32,
        pub bounds_w: u32,
        pub bounds_h: u32,
    }

    /// Simple interner: identical strings are stored once, nodes
    /// reference them via (offset, length).
    struct StringInterner {
        buf: Vec<u8>,
        // We use a small hand-rolled lookup keyed on string hash
        // to avoid pulling in ahash/fxhash. core::hash::SipHasher
        // is the default Hasher in stable Rust.
        entries: Vec<(u64, u32, u16)>, // (hash, offset, len)
    }

    impl StringInterner {
        fn new() -> Self {
            Self { buf: Vec::new(), entries: Vec::new() }
        }

        fn intern(&mut self, s: &str) -> (u32, u32) {
            if s.is_empty() {
                return (0, 0);
            }
            let bytes = s.as_bytes();
            let h = fnv1a(bytes);
            for (existing_hash, offset, len) in &self.entries {
                if *existing_hash == h
                    && *len as usize == bytes.len()
                    && &self.buf[*offset as usize..*offset as usize + *len as usize]
                        == bytes
                {
                    return (*offset, *len as u32);
                }
            }
            let offset = self.buf.len() as u32;
            let len = bytes.len().min(u16::MAX as usize) as u16;
            self.buf.extend_from_slice(&bytes[..len as usize]);
            self.entries.push((h, offset, len));
            (offset, len as u32)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.buf
        }
    }

    // FNV-1a 64-bit — same non-cryptographic hash the Python side uses
    // for the adblock token buckets. Good enough for string dedup.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    #[inline]
    fn pad_to_8(len: usize) -> usize {
        (len.wrapping_neg()) & 7
    }

    /// Encode a `DOM_STATE_UPDATE` message.
    pub fn serialize_state_update(
        nodes: &[OwnedNode],
        viewport_w: u32,
        viewport_h: u32,
        page_id: u64,
    ) -> Vec<u8> {
        // Pass 1: intern every string so we know the pool length
        // before we start packing.
        let mut interner = StringInterner::new();
        let mut refs: Vec<((u32, u32), (u32, u32))> = Vec::with_capacity(nodes.len());
        for n in nodes {
            let tag = interner.intern(&n.tag);
            let text = interner.intern(&n.text);
            refs.push((tag, text));
        }

        let pool = interner.into_bytes();
        let pool_len = pool.len();
        let pad = pad_to_8(HEADER_SIZE + pool_len);
        let node_count = nodes.len();
        let total = HEADER_SIZE + pool_len + pad + node_count * NODE_SIZE;

        let mut out: Vec<u8> = Vec::with_capacity(total);
        out.resize(total, 0);

        // Header
        out[0..4].copy_from_slice(&FBP_MAGIC.to_le_bytes());
        out[4] = MSG_DOM_STATE_UPDATE;
        // bytes 5..8 are already zero (padding)
        out[8..16].copy_from_slice(&page_id.to_le_bytes());
        out[16..20].copy_from_slice(&(pool_len as u32).to_le_bytes());
        out[20..24].copy_from_slice(&(node_count as u32).to_le_bytes());
        out[24..28].copy_from_slice(&viewport_w.to_le_bytes());
        out[28..32].copy_from_slice(&viewport_h.to_le_bytes());

        // String pool
        out[HEADER_SIZE..HEADER_SIZE + pool_len].copy_from_slice(&pool);
        // Pad bytes are already zero from resize.

        // Nodes
        let nodes_start = HEADER_SIZE + pool_len + pad;
        for (i, node) in nodes.iter().enumerate() {
            let base = nodes_start + i * NODE_SIZE;
            let ((tag_off, tag_len), (text_off, text_len)) = refs[i];
            // u16 len for tag, u32 for text — matches the spec
            out[base + 0..base + 4].copy_from_slice(&tag_off.to_le_bytes());
            out[base + 4..base + 6].copy_from_slice(&(tag_len as u16).to_le_bytes());
            out[base + 6..base + 8].copy_from_slice(&node.flags.0.to_le_bytes());
            out[base + 8..base + 12].copy_from_slice(&text_off.to_le_bytes());
            out[base + 12..base + 16].copy_from_slice(&text_len.to_le_bytes());
            out[base + 16..base + 20].copy_from_slice(&node.parent.to_le_bytes());
            out[base + 20..base + 24].copy_from_slice(&node.first_child.to_le_bytes());
            out[base + 24..base + 28].copy_from_slice(&node.last_child.to_le_bytes());
            out[base + 28..base + 32].copy_from_slice(&node.next_sibling.to_le_bytes());
            out[base + 32..base + 36].copy_from_slice(&node.bounds_x.to_le_bytes());
            out[base + 36..base + 40].copy_from_slice(&node.bounds_y.to_le_bytes());
            out[base + 40..base + 44].copy_from_slice(&node.bounds_w.to_le_bytes());
            out[base + 44..base + 48].copy_from_slice(&node.bounds_h.to_le_bytes());
        }

        out
    }

    /// Encode an `INTERACTION_EVENT` message.
    pub fn serialize_interaction_event(
        action: u8,
        node_id: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + data.len());
        out.push(MSG_INTERACTION_EVENT);
        out.push(action);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&node_id.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }
}

#[cfg(feature = "alloc")]
pub use encode::{serialize_interaction_event, serialize_state_update, OwnedNode};

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;

    #[test]
    fn node_size_is_48() {
        assert_eq!(size_of::<SemanticNode>(), NODE_SIZE);
    }

    #[test]
    fn empty_state_update_header_only() {
        // Hand-build a minimum-size payload: 32-byte header, pool_len=0,
        // node_count=0, viewport 1024x768, page_id=0xdeadbeef12345678.
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&FBP_MAGIC.to_le_bytes());
        buf[4] = MSG_DOM_STATE_UPDATE;
        buf[8..16].copy_from_slice(&0xdeadbeef_12345678u64.to_le_bytes());
        buf[16..20].copy_from_slice(&0u32.to_le_bytes()); // pool_len
        buf[20..24].copy_from_slice(&0u32.to_le_bytes()); // node_count
        buf[24..28].copy_from_slice(&1024u32.to_le_bytes());
        buf[28..32].copy_from_slice(&768u32.to_le_bytes());

        let parsed = parse_state_update(&buf).expect("parse empty");
        assert_eq!(parsed.page_id, 0xdeadbeef_12345678);
        assert_eq!(parsed.viewport_w, 1024);
        assert_eq!(parsed.viewport_h, 768);
        assert_eq!(parsed.string_pool, &[] as &[u8]);
        assert_eq!(parsed.nodes.len(), 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xAAAAAAAAu32.to_le_bytes());
        match parse_state_update(&buf) {
            Err(ParseError::BadMagic { got }) => assert_eq!(got, 0xAAAAAAAA),
            other => panic!("expected BadMagic, got {:?}", other),
        }
    }

    #[test]
    fn rejects_wrong_msg_type() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&FBP_MAGIC.to_le_bytes());
        buf[4] = 0x99;
        match parse_state_update(&buf) {
            Err(ParseError::WrongMsgType { got }) => assert_eq!(got, 0x99),
            other => panic!("expected WrongMsgType, got {:?}", other),
        }
    }

    #[test]
    fn too_short_header() {
        let buf = [0u8; 5];
        assert!(matches!(
            parse_state_update(&buf),
            Err(ParseError::TooShort { .. })
        ));
    }

    #[test]
    fn interaction_event_click() {
        // serialize_interaction_event(ACTION_CLICK, node_id=42, data=b"")
        let mut buf = [0u8; 12];
        buf[0] = MSG_INTERACTION_EVENT;
        buf[1] = ACTION_CLICK;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        // data_len = 0 already from zero-init
        let ev = parse_interaction_event(&buf).expect("parse click");
        assert_eq!(ev.action, ACTION_CLICK);
        assert_eq!(ev.node_id, 42);
        assert_eq!(ev.data, &[] as &[u8]);
    }

    #[test]
    fn interaction_event_type_with_payload() {
        let payload = b"hei folkering";
        let total = 12 + payload.len();
        let mut buf = vec![0u8; total];
        buf[0] = MSG_INTERACTION_EVENT;
        buf[1] = ACTION_TYPE;
        buf[4..8].copy_from_slice(&7u32.to_le_bytes());
        buf[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        buf[12..].copy_from_slice(payload);

        let ev = parse_interaction_event(&buf).expect("parse type");
        assert_eq!(ev.action, ACTION_TYPE);
        assert_eq!(ev.node_id, 7);
        assert_eq!(ev.data, payload);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn serialize_then_parse_roundtrip() {
        use crate::encode::{serialize_state_update, OwnedNode};
        use std::string::ToString;

        // 3-node mini-tree: html → body → h1
        let nodes = std::vec![
            OwnedNode {
                tag: "html".to_string(),
                text: "".to_string(),
                flags: NodeFlags::IS_VISIBLE,
                first_child: 2,
                last_child: 2,
                bounds_w: 1024,
                bounds_h: 768,
                ..OwnedNode::default()
            },
            OwnedNode {
                tag: "body".to_string(),
                text: "".to_string(),
                flags: NodeFlags::IS_VISIBLE,
                parent: 1,
                first_child: 3,
                last_child: 3,
                bounds_w: 1024,
                bounds_h: 768,
                ..OwnedNode::default()
            },
            OwnedNode {
                tag: "h1".to_string(),
                text: "Hello".to_string(),
                flags: NodeFlags::IS_VISIBLE,
                parent: 2,
                bounds_x: 10,
                bounds_y: 20,
                bounds_w: 300,
                bounds_h: 40,
                ..OwnedNode::default()
            },
        ];

        // Align the bytes so the parser's 8-byte alignment check is
        // satisfied when we re-parse.
        let bytes = serialize_state_update(&nodes, 1024, 768, 0xCAFEBABE);
        let mut aligned: std::vec::Vec<u64> = std::vec![0u64; (bytes.len() + 7) / 8];
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                aligned.as_mut_ptr() as *mut u8,
                bytes.len(),
            );
        }
        let slice = unsafe {
            std::slice::from_raw_parts(aligned.as_ptr() as *const u8, bytes.len())
        };

        let parsed = parse_state_update(slice).expect("parse round-trip");
        assert_eq!(parsed.page_id, 0xCAFEBABE);
        assert_eq!(parsed.viewport_w, 1024);
        assert_eq!(parsed.viewport_h, 768);
        assert_eq!(parsed.nodes.len(), 3);

        // html
        let html = &parsed.nodes[0];
        assert_eq!(parsed.tag(html), b"html");
        assert_eq!(html.first_child_id, 2);
        assert_eq!(html.bounds_w, 1024);

        // body
        let body = &parsed.nodes[1];
        assert_eq!(parsed.tag(body), b"body");
        assert_eq!(body.parent_id, 1);

        // h1
        let h1 = &parsed.nodes[2];
        assert_eq!(parsed.tag(h1), b"h1");
        assert_eq!(parsed.text(h1), b"Hello");
        assert_eq!(h1.parent_id, 2);
        assert_eq!(h1.bounds_x, 10);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn serialize_interaction_event_matches_parser() {
        use crate::encode::serialize_interaction_event as enc;
        let bytes = enc(ACTION_CLICK, 42, b"");
        let ev = parse_interaction_event(&bytes).expect("parse click");
        assert_eq!(ev.action, ACTION_CLICK);
        assert_eq!(ev.node_id, 42);
        assert_eq!(ev.data, b"");

        let payload = b"hello folkering";
        let bytes = enc(ACTION_TYPE, 7, payload);
        let ev = parse_interaction_event(&bytes).expect("parse type");
        assert_eq!(ev.action, ACTION_TYPE);
        assert_eq!(ev.node_id, 7);
        assert_eq!(ev.data, payload);
    }
}

#[cfg(all(test, not(feature = "alloc")))]
extern crate alloc;
#[cfg(test)]
extern crate std;
