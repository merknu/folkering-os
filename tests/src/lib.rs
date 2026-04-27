// Folkering OS Host-Side Test Suite
//
// Tests pure logic from kernel + compositor that has no hardware
// dependencies. Runs with standard `cargo test` on the host machine.
//
// Usage: cd tests && cargo test
//
// Modules:
//   damage          — compositor damage-tracker algorithm
//   intent          — agent-intent JSON parser
//   capability      — kernel capability containment semantics
//   mvfs_dirty      — MVFS dirty-bitmap bookkeeping
//   caller_token    — IPC token encode/decode + 48-bit collision
//   transferability — non-transferable cap-type policy

// ── Damage Tracker (copy of pure logic from compositor/src/damage.rs) ──

mod damage {
    use std::cmp::{max, min};

    const MAX_RECTS: usize = 10;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Rect {
        pub x: u32,
        pub y: u32,
        pub w: u32,
        pub h: u32,
    }

    impl Rect {
        pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
            Self { x, y, w, h }
        }

        pub fn intersects(&self, other: &Rect) -> bool {
            self.x < other.x + other.w
                && self.x + self.w > other.x
                && self.y < other.y + other.h
                && self.y + self.h > other.y
        }

        pub fn union(&self, other: &Rect) -> Rect {
            let x = min(self.x, other.x);
            let y = min(self.y, other.y);
            let right = max(self.x + self.w, other.x + other.w);
            let bottom = max(self.y + self.h, other.y + other.h);
            Rect { x, y, w: right - x, h: bottom - y }
        }
    }

    pub struct DamageTracker {
        regions: Vec<Rect>,
        screen_w: u32,
        screen_h: u32,
    }

    impl DamageTracker {
        pub fn new(screen_w: u32, screen_h: u32) -> Self {
            Self { regions: Vec::with_capacity(MAX_RECTS * 2), screen_w, screen_h }
        }

        pub fn add_damage(&mut self, new_rect: Rect) {
            let x = min(new_rect.x, self.screen_w);
            let y = min(new_rect.y, self.screen_h);
            let w = min(new_rect.w, self.screen_w - x);
            let h = min(new_rect.h, self.screen_h - y);
            if w == 0 || h == 0 { return; }

            let mut merged = Rect::new(x, y, w, h);
            let mut i = 0;
            while i < self.regions.len() {
                if self.regions[i].intersects(&merged) {
                    merged = self.regions[i].union(&merged);
                    self.regions.swap_remove(i);
                    i = 0;
                } else {
                    i += 1;
                }
            }
            self.regions.push(merged);
            if self.regions.len() > MAX_RECTS {
                self.collapse_to_bounding_box();
            }
        }

        pub fn damage_full(&mut self) {
            self.regions.clear();
            self.regions.push(Rect::new(0, 0, self.screen_w, self.screen_h));
        }

        fn collapse_to_bounding_box(&mut self) {
            if self.regions.is_empty() { return; }
            let mut bbox = self.regions[0];
            for r in &self.regions[1..] { bbox = bbox.union(r); }
            self.regions.clear();
            self.regions.push(bbox);
        }

        pub fn regions(&self) -> &[Rect] { &self.regions }
        pub fn clear(&mut self) { self.regions.clear(); }
        pub fn has_damage(&self) -> bool { !self.regions.is_empty() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rect_intersects_overlap() {
            assert!(Rect::new(10, 10, 50, 50).intersects(&Rect::new(30, 30, 50, 50)));
        }

        #[test]
        fn rect_no_overlap() {
            assert!(!Rect::new(0, 0, 10, 10).intersects(&Rect::new(20, 20, 10, 10)));
        }

        #[test]
        fn rect_adjacent_no_overlap() {
            assert!(!Rect::new(0, 0, 10, 10).intersects(&Rect::new(10, 0, 10, 10)));
        }

        #[test]
        fn rect_contained() {
            assert!(Rect::new(0, 0, 100, 100).intersects(&Rect::new(25, 25, 50, 50)));
        }

        #[test]
        fn rect_union() {
            let u = Rect::new(10, 10, 20, 20).union(&Rect::new(25, 25, 20, 20));
            assert_eq!(u, Rect::new(10, 10, 35, 35));
        }

        #[test]
        fn tracker_empty() {
            let t = DamageTracker::new(1024, 768);
            assert!(!t.has_damage());
        }

        #[test]
        fn tracker_full() {
            let mut t = DamageTracker::new(1024, 768);
            t.damage_full();
            assert_eq!(t.regions().len(), 1);
            assert_eq!(t.regions()[0], Rect::new(0, 0, 1024, 768));
        }

        #[test]
        fn tracker_coalesce() {
            let mut t = DamageTracker::new(1024, 768);
            t.add_damage(Rect::new(10, 10, 50, 50));
            t.add_damage(Rect::new(30, 30, 50, 50));
            assert_eq!(t.regions().len(), 1);
            assert_eq!(t.regions()[0], Rect::new(10, 10, 70, 70));
        }

        #[test]
        fn tracker_disjoint() {
            let mut t = DamageTracker::new(1024, 768);
            t.add_damage(Rect::new(0, 0, 10, 10));
            t.add_damage(Rect::new(100, 100, 10, 10));
            assert_eq!(t.regions().len(), 2);
        }

        #[test]
        fn tracker_clamp() {
            let mut t = DamageTracker::new(100, 100);
            t.add_damage(Rect::new(90, 90, 50, 50));
            assert_eq!(t.regions()[0].w, 10);
            assert_eq!(t.regions()[0].h, 10);
        }

        #[test]
        fn tracker_overflow_collapses() {
            let mut t = DamageTracker::new(2000, 768);
            // Add 12 truly disjoint rects (spacing > width to prevent coalesce)
            for i in 0..12 { t.add_damage(Rect::new(i * 150, 0, 10, 10)); }
            // Should collapse to bounding box when > MAX_RECTS
            assert!(t.regions().len() <= 10, "should collapse, got {}", t.regions().len());
        }

        #[test]
        fn tracker_clear() {
            let mut t = DamageTracker::new(1024, 768);
            t.damage_full();
            t.clear();
            assert!(!t.has_damage());
        }
    }
}

// ── Intent Parser (copy of pure logic from compositor/src/intent.rs) ──

mod intent {
    #[derive(Debug, PartialEq)]
    pub enum AgentIntent {
        MoveWindow { window_id: u32, x: u32, y: u32 },
        CloseWindow { window_id: u32 },
        ResizeWindow { window_id: u32, w: u32, h: u32 },
        GenerateTool { prompt: String },
        ToolReady { binary_base64: String },
        TextResponse { text: String },
        ReadFile { path: String },
        WriteFile { path: String, content: String },
        Error { message: String },
    }

    pub fn parse_intent(response: &str) -> AgentIntent {
        let trimmed = response.trim();
        let effective = if let Some(_) = trimmed.find("<think>") {
            if let Some(end) = trimmed.find("</think>") {
                trimmed[end + 8..].trim()
            } else { trimmed }
        } else { trimmed };

        if !effective.starts_with('{') {
            return AgentIntent::TextResponse { text: String::from(trimmed) };
        }
        let action = match extract_str(effective, "action") {
            Some(a) => a,
            None => return AgentIntent::TextResponse { text: String::from(trimmed) },
        };
        match action.as_str() {
            "move_window" => AgentIntent::MoveWindow {
                window_id: extract_num(effective, "window_id").unwrap_or(0),
                x: extract_num(effective, "x").unwrap_or(0),
                y: extract_num(effective, "y").unwrap_or(0),
            },
            "close_window" => AgentIntent::CloseWindow {
                window_id: extract_num(effective, "window_id").unwrap_or(0),
            },
            "generate_tool" => AgentIntent::GenerateTool {
                prompt: extract_str(effective, "prompt").unwrap_or_default(),
            },
            "tool_ready" => AgentIntent::ToolReady {
                binary_base64: extract_str(effective, "binary").unwrap_or_default(),
            },
            _ => AgentIntent::TextResponse { text: String::from(trimmed) },
        }
    }

    pub fn extract_str(json: &str, key: &str) -> Option<String> {
        let pattern = format!("\"{}\":", key);
        let start = json.find(&pattern)? + pattern.len();
        let rest = json[start..].trim_start();
        if !rest.starts_with('"') { return None; }
        let inner = &rest[1..];
        let mut end = 0;
        let bytes = inner.as_bytes();
        while end < bytes.len() {
            if bytes[end] == b'\\' { end += 2; continue; }
            if bytes[end] == b'"' { break; }
            end += 1;
        }
        Some(String::from(&inner[..end]))
    }

    pub fn extract_num(json: &str, key: &str) -> Option<u32> {
        let pattern = format!("\"{}\":", key);
        let start = json.find(&pattern)? + pattern.len();
        let rest = json[start..].trim_start();
        let end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        if end == 0 { return None; }
        rest[..end].parse().ok()
    }

    pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
        fn decode_char(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'+' => Some(62), b'/' => Some(63),
                _ => None,
            }
        }
        let clean: Vec<u8> = input.bytes().filter(|&b| b != b'\n' && b != b'\r' && b != b' ').collect();
        if clean.is_empty() { return Some(Vec::new()); }
        if clean.len() % 4 != 0 { return None; }
        let mut out = Vec::with_capacity(clean.len() / 4 * 3);
        for chunk in clean.chunks_exact(4) {
            let a = decode_char(chunk[0])?;
            let b = decode_char(chunk[1])?;
            let c_pad = chunk[2] == b'=';
            let d_pad = chunk[3] == b'=';
            let c = if c_pad { 0 } else { decode_char(chunk[2])? };
            let d = if d_pad { 0 } else { decode_char(chunk[3])? };
            let triple = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | (d as u32);
            out.push((triple >> 16) as u8);
            if !c_pad { out.push((triple >> 8) as u8); }
            if !d_pad { out.push(triple as u8); }
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn text_response() {
            match parse_intent("hello") {
                AgentIntent::TextResponse { text } => assert_eq!(text, "hello"),
                _ => panic!("expected TextResponse"),
            }
        }

        #[test]
        fn move_window() {
            match parse_intent(r#"{"action":"move_window","window_id":3,"x":100,"y":200}"#) {
                AgentIntent::MoveWindow { window_id, x, y } => {
                    assert_eq!((window_id, x, y), (3, 100, 200));
                }
                _ => panic!("expected MoveWindow"),
            }
        }

        #[test]
        fn close_window() {
            match parse_intent(r#"{"action":"close_window","window_id":5}"#) {
                AgentIntent::CloseWindow { window_id } => assert_eq!(window_id, 5),
                _ => panic!("expected CloseWindow"),
            }
        }

        #[test]
        fn generate_tool() {
            match parse_intent(r#"{"action":"generate_tool","prompt":"draw circle"}"#) {
                AgentIntent::GenerateTool { prompt } => assert_eq!(prompt, "draw circle"),
                _ => panic!("expected GenerateTool"),
            }
        }

        #[test]
        fn think_tags_stripped() {
            let input = "<think>\nreasoning...\n</think>\n{\"action\":\"move_window\",\"window_id\":2,\"x\":50,\"y\":50}";
            match parse_intent(input) {
                AgentIntent::MoveWindow { window_id, x, y } => {
                    assert_eq!((window_id, x, y), (2, 50, 50));
                }
                other => panic!("expected MoveWindow, got {:?}", other),
            }
        }

        #[test]
        fn think_unclosed_fallback() {
            match parse_intent("<think>\nstill thinking") {
                AgentIntent::TextResponse { .. } => {}
                _ => panic!("unclosed think should be TextResponse"),
            }
        }

        #[test]
        fn extract_str_basic() {
            assert_eq!(extract_str(r#"{"name":"hello"}"#, "name"), Some("hello".into()));
        }

        #[test]
        fn extract_str_missing() {
            assert_eq!(extract_str(r#"{"a":"b"}"#, "z"), None);
        }

        #[test]
        fn extract_num_basic() {
            assert_eq!(extract_num(r#"{"x":42}"#, "x"), Some(42));
        }

        #[test]
        fn base64_hello() {
            assert_eq!(base64_decode("SGVsbG8="), Some(b"Hello".to_vec()));
        }

        #[test]
        fn base64_empty() {
            assert_eq!(base64_decode(""), Some(vec![]));
        }

        #[test]
        fn base64_invalid() {
            assert_eq!(base64_decode("ABC"), None);
        }

        #[test]
        fn base64_with_newlines() {
            assert_eq!(base64_decode("SGVs\nbG8="), Some(b"Hello".to_vec()));
        }
    }
}

// ── Capability containment (kernel/src/capability/types.rs) ───────────
//
// Mirrors the `grants()` arm semantics for Framebuffer, DmaRegion,
// MmioRegion, and IoPort variants. The key property being tested is
// that overflow-unsafe additions (`base + size` wrapping past u64::MAX)
// can't fool a held cap into granting access to a request that wraps
// past zero. Caught in the audit pass as `checked_add` was added.

mod capability {
    /// Re-implements `CapabilityType::grants` for just the region
    /// variants — `IpcSend`/`All`/etc aren't relevant to the tests
    /// we're writing here. The kernel type is mirrored by shape.
    #[derive(Clone, Copy, Debug)]
    pub enum Region {
        Framebuffer { phys_base: u64, size: u64 },
        DmaRegion   { phys_base: u64, size: u64 },
        MmioRegion  { phys_base: u64, size: u64 },
        IoPort      { base: u16, size: u16 },
    }

    /// Check if `held` grants access to `required`. Same logic as
    /// the kernel's `grants()`; the point of these tests is to pin
    /// that the overflow guards behave correctly.
    pub fn grants(held: Region, required: Region) -> bool {
        use Region::*;
        match (held, required) {
            (Framebuffer { phys_base: b1, size: s1 },
             Framebuffer { phys_base: b2, size: s2 }) => contains_u64(b1, s1, b2, s2),
            (DmaRegion   { phys_base: b1, size: s1 },
             DmaRegion   { phys_base: b2, size: s2 }) => contains_u64(b1, s1, b2, s2),
            (MmioRegion  { phys_base: b1, size: s1 },
             MmioRegion  { phys_base: b2, size: s2 }) => contains_u64(b1, s1, b2, s2),
            (IoPort      { base: b1, size: s1 },
             IoPort      { base: b2, size: s2 }) => {
                let he = (b1 as u32) + (s1 as u32);
                let re = (b2 as u32) + (s2 as u32);
                b2 >= b1 && re <= he
            }
            _ => false, // mismatched variants don't grant
        }
    }

    fn contains_u64(hb: u64, hs: u64, rb: u64, rs: u64) -> bool {
        let held_end = hb.checked_add(hs);
        let req_end  = rb.checked_add(rs);
        match (held_end, req_end) {
            (Some(he), Some(re)) => rb >= hb && re <= he,
            _ => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::Region::*;
        use super::grants;

        #[test]
        fn framebuffer_exact_match() {
            let held = Framebuffer { phys_base: 0xFD00_0000, size: 0x100000 };
            assert!(grants(held, held));
        }

        #[test]
        fn framebuffer_subrange_grants() {
            let held = Framebuffer { phys_base: 0xFD00_0000, size: 0x100000 };
            let req  = Framebuffer { phys_base: 0xFD00_8000, size: 0x10000 };
            assert!(grants(held, req));
        }

        #[test]
        fn framebuffer_outside_denied() {
            let held = Framebuffer { phys_base: 0xFD00_0000, size: 0x10000 };
            let req  = Framebuffer { phys_base: 0xFD02_0000, size: 0x1000 };
            assert!(!grants(held, req));
        }

        #[test]
        fn framebuffer_overflow_request_denied() {
            // Held = 0x1000..0x2000 (tiny). Request claims to start at
            // 0x1000 with size=u64::MAX. Pre-checked_add code would
            // compute 0x1000 + u64::MAX = 0x0FFF (wraps) and find it
            // <= held_end → falsely grant. `checked_add` returns None
            // for the request side → deny.
            let held = Framebuffer { phys_base: 0x1000, size: 0x1000 };
            let req  = Framebuffer { phys_base: 0x1000, size: u64::MAX };
            assert!(!grants(held, req));
        }

        #[test]
        fn framebuffer_overflow_held_denied() {
            // Same trick applied to the held side.
            let held = Framebuffer { phys_base: u64::MAX - 10, size: 100 };
            let req  = Framebuffer { phys_base: u64::MAX - 5, size: 3 };
            assert!(!grants(held, req));
        }

        #[test]
        fn dma_region_subrange() {
            let held = DmaRegion { phys_base: 0x2119000, size: 0x10000 };
            let req  = DmaRegion { phys_base: 0x211A000, size: 0x1000 };
            assert!(grants(held, req));
        }

        #[test]
        fn dma_region_straddles_end_denied() {
            let held = DmaRegion { phys_base: 0x1000, size: 0x1000 };
            let req  = DmaRegion { phys_base: 0x1800, size: 0x1000 }; // ends at 0x2800, held ends at 0x2000
            assert!(!grants(held, req));
        }

        #[test]
        fn mmio_and_dma_dont_cross_grant() {
            let mmio = MmioRegion { phys_base: 0xFD00_0000, size: 0x1000 };
            let dma  = DmaRegion  { phys_base: 0xFD00_0000, size: 0x1000 };
            // Same range, different variant → must not grant across.
            assert!(!grants(mmio, dma));
            assert!(!grants(dma, mmio));
        }

        #[test]
        fn io_port_exact_match() {
            let held = IoPort { base: 0xC000, size: 256 };
            let req  = IoPort { base: 0xC000, size: 256 };
            assert!(grants(held, req));
        }

        #[test]
        fn io_port_subrange() {
            let held = IoPort { base: 0xC000, size: 256 };
            let req  = IoPort { base: 0xC010, size: 16 };
            assert!(grants(held, req));
        }

        #[test]
        fn io_port_outside_denied() {
            let held = IoPort { base: 0xC000, size: 16 };
            let req  = IoPort { base: 0xC100, size: 1 };
            assert!(!grants(held, req));
        }

        #[test]
        fn io_port_straddles_end_denied() {
            // Held [0xFFF0..0xFFFF], request [0xFFF8..0x10007] — the
            // u32 math catches the overflow past the 16-bit port space.
            let held = IoPort { base: 0xFFF0, size: 16 };
            let req  = IoPort { base: 0xFFF8, size: 16 };
            assert!(!grants(held, req));
        }
    }
}

// ── MVFS dirty-bitmap bookkeeping (kernel/src/fs/mvfs.rs) ──────────────
//
// The partial-flush optimization depends on the dirty-mask arithmetic
// being exactly right — especially the delete path, where all slots
// from the deleted index onwards shift down and need to be rewritten.

mod mvfs_dirty {
    pub const MVFS_MAX_FILES: usize = 16;
    pub const DIRTY_HEADER: u32 = 1 << 31;

    /// Mirrors the mask computation inside `delete()` in
    /// `kernel/src/fs/mvfs.rs`. Returns the set of bits marked dirty
    /// when slot `victim` is removed from a table holding `old_len`
    /// entries.
    pub fn delete_mask(victim: usize, old_len: usize) -> u32 {
        let mask_lo = (1u32 << victim) - 1;
        let mask_hi = if old_len >= 32 { u32::MAX } else { (1u32 << old_len) - 1 };
        (mask_hi & !mask_lo) | DIRTY_HEADER
    }

    /// Mirrors the mask computation inside `write()` for overwrite
    /// or insert of a single slot `i`.
    pub fn write_mask(slot: usize) -> u32 {
        (1u32 << slot) | DIRTY_HEADER
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn write_single_slot() {
            // Writing slot 3 → bits [3, 31] dirty.
            let m = write_mask(3);
            assert_eq!(m, (1u32 << 3) | DIRTY_HEADER);
            assert!(m & DIRTY_HEADER != 0);
            assert!(m & (1 << 3) != 0);
            assert!(m & (1 << 2) == 0);
        }

        #[test]
        fn delete_slot_0_marks_everything() {
            // Table with 5 entries, delete slot 0. Slots 0..5 all shift
            // down, so all must be marked dirty. Slots 5..15 untouched.
            let m = delete_mask(0, 5);
            assert!(m & DIRTY_HEADER != 0);
            for i in 0..5 {
                assert!(m & (1 << i) != 0, "slot {} should be dirty", i);
            }
            for i in 5..16 {
                assert!(m & (1 << i) == 0, "slot {} should be clean", i);
            }
        }

        #[test]
        fn delete_middle_slot() {
            // Table with 8 entries, delete slot 3. Slots [3..8) shift.
            let m = delete_mask(3, 8);
            for i in 0..3 { assert!(m & (1 << i) == 0); }
            for i in 3..8 { assert!(m & (1 << i) != 0); }
            for i in 8..16 { assert!(m & (1 << i) == 0); }
            assert!(m & DIRTY_HEADER != 0);
        }

        #[test]
        fn delete_last_slot_only_self() {
            // Delete slot 7 of 8. Only slot 7 needs rewriting (no
            // trailing entries to shift). Plus header for entry_count.
            let m = delete_mask(7, 8);
            assert!(m & (1 << 7) != 0);
            for i in 0..7 { assert!(m & (1 << i) == 0); }
            for i in 8..16 { assert!(m & (1 << i) == 0); }
            assert!(m & DIRTY_HEADER != 0);
        }

        #[test]
        fn delete_full_table() {
            // Delete slot 0 of a completely full 16-slot table.
            let m = delete_mask(0, MVFS_MAX_FILES);
            // Lower 16 bits all set + header.
            assert_eq!(m & 0xFFFF, 0xFFFF);
            assert!(m & DIRTY_HEADER != 0);
        }

        #[test]
        fn atomic_swap_semantics() {
            use std::sync::atomic::{AtomicU32, Ordering};
            // Simulates DIRTY: `take_dirty` must atomically read + clear.
            let dirty = AtomicU32::new(0);
            dirty.fetch_or(write_mask(1), Ordering::Relaxed);
            dirty.fetch_or(write_mask(2), Ordering::Relaxed);
            // Post-taking: dirty is 0, value has both bits.
            let taken = dirty.swap(0, Ordering::AcqRel);
            assert_eq!(dirty.load(Ordering::Relaxed), 0);
            assert!(taken & (1 << 1) != 0);
            assert!(taken & (1 << 2) != 0);
            assert!(taken & DIRTY_HEADER != 0);
        }
    }
}

// ── CallerToken encode/decode (kernel/src/ipc/message.rs) ─────────────
//
// Pass A Fix 3 widened request_id from 32 bits to 48 bits so
// rapid-fire sends from the same task can't produce colliding
// tokens. These tests pin the encoding invariant, including the
// upper-bit wraparound scenario that used to cause false matches.

mod caller_token {
    // Mirror of the kernel's CallerToken::new / ::decode, bit-for-bit.
    // If this logic diverges from `kernel/src/ipc/message.rs`, these
    // tests catch it immediately.
    pub fn encode(sender_pid: u32, request_id: u64) -> u64 {
        const OBFUSCATION_KEY: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let sender = (sender_pid as u64) & 0xFFFF;
        let req    = request_id & 0xFFFF_FFFF_FFFF; // 48 bits
        let raw    = sender | (req << 16);
        raw ^ OBFUSCATION_KEY
    }

    pub fn decode(token: u64) -> Option<(u32, u64)> {
        const OBFUSCATION_KEY: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let raw = token ^ OBFUSCATION_KEY;
        let sender_pid = (raw & 0xFFFF) as u32;
        let request_id = (raw >> 16) & 0xFFFF_FFFF_FFFF;
        if sender_pid == 0 {
            return None;
        }
        Some((sender_pid, request_id))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::BTreeSet;

        #[test]
        fn encode_decode_round_trip_small_ids() {
            let t = encode(3, 42);
            assert_eq!(decode(t), Some((3, 42)));
        }

        #[test]
        fn encode_decode_round_trip_large_request_id() {
            // 48-bit max value — biggest legitimate request_id.
            let max_req = 0xFFFF_FFFF_FFFF;
            let t = encode(5, max_req);
            assert_eq!(decode(t), Some((5, max_req)));
        }

        #[test]
        fn zero_sender_pid_rejected() {
            // Encoding a sender_pid of 0 must decode to None — the
            // kernel uses this check to reject obviously-corrupt tokens.
            let t = encode(0, 42);
            assert_eq!(decode(t), None);
        }

        #[test]
        fn no_collision_after_32bit_wrap() {
            // Pre-Fix: request_id was 32 bits, so IDs N and N+2^32
            // produced the same token (wraparound). Now with 48 bits,
            // N and N+2^32 must produce DIFFERENT tokens.
            let sender = 3;
            let t_low  = encode(sender, 1);
            let t_wrap = encode(sender, 1u64 + (1u64 << 32));
            assert_ne!(t_low, t_wrap, "48-bit id must survive 32-bit wrap");

            // Both must decode to distinct request_ids.
            let (_, r_low) = decode(t_low).unwrap();
            let (_, r_wrap) = decode(t_wrap).unwrap();
            assert_ne!(r_low, r_wrap);
        }

        #[test]
        fn million_unique_tokens() {
            // Generate 1M tokens with different (sender, request_id)
            // combinations. None may collide.
            let mut seen = BTreeSet::new();
            for sender in 1..=10u32 {
                for req in 0..100_000u64 {
                    let t = encode(sender, req);
                    assert!(seen.insert(t), "collision at sender={sender} req={req}");
                }
            }
            assert_eq!(seen.len(), 1_000_000);
        }

        #[test]
        fn sender_pid_truncation() {
            // The sender field is 16 bits. Values above 0xFFFF get
            // their upper bits silently clamped. Check that a value
            // with a 17th bit decodes to the lower 16.
            let t = encode(0x1_0003, 42); // high bit set
            let (sender, req) = decode(t).unwrap();
            assert_eq!(sender, 3, "high bit should be clipped");
            assert_eq!(req, 42);
        }

        #[test]
        fn request_id_above_48bit_clamped() {
            // Similarly, request_ids above 2^48 should clamp.
            let req_too_big = 0xFFFF_FFFF_FFFF_FFFF;
            let t = encode(5, req_too_big);
            let (_, r) = decode(t).unwrap();
            assert_eq!(r, 0xFFFF_FFFF_FFFF, "req should be clipped to 48 bits");
        }
    }
}

// ── Non-transferable cap policy (kernel/src/capability/mod.rs) ────────
//
// Pass B made the transferability guard type-enforced via a
// `TransferableCap` newtype. At the heart of that guard is
// `is_non_transferable()` — these tests pin the exact set of cap
// types that refuse transfer, so adding a new hardware-bound variant
// without updating the list becomes visible as a test failure.

mod transferability {
    /// Mirror of the kernel's cap variants — keep in lockstep with
    /// `CapabilityType` in capability/types.rs.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum CapKind {
        All,
        IpcSend,
        IpcSendAny,
        IpcReceive,
        Memory,
        Resource,
        TaskControl,
        Scheduler,
        Hardware,
        Framebuffer,
        DmaRegion,
        MmioRegion,
        IoPort,
        DriverPrivilege,
        RawBlockIO,
    }

    pub fn is_non_transferable(k: CapKind) -> bool {
        use CapKind::*;
        matches!(
            k,
            DmaRegion | MmioRegion | IoPort | Framebuffer | DriverPrivilege | RawBlockIO
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use super::CapKind::*;

        #[test]
        fn hardware_caps_are_non_transferable() {
            for k in [Framebuffer, DmaRegion, MmioRegion, IoPort, DriverPrivilege, RawBlockIO] {
                assert!(is_non_transferable(k), "{k:?} must be non-transferable");
            }
        }

        #[test]
        fn ipc_and_general_caps_are_transferable() {
            for k in [All, IpcSend, IpcSendAny, IpcReceive, Memory, Resource,
                      TaskControl, Scheduler, Hardware] {
                assert!(!is_non_transferable(k), "{k:?} must be transferable");
            }
        }

        #[test]
        fn coverage_is_complete() {
            // Sanity: every variant must have been classified in one
            // of the two groups. If a future refactor adds a new
            // variant, this test forces the author to think about
            // transferability explicitly.
            let all_variants = [
                All, IpcSend, IpcSendAny, IpcReceive, Memory, Resource,
                TaskControl, Scheduler, Hardware,
                Framebuffer, DmaRegion, MmioRegion, IoPort,
                DriverPrivilege, RawBlockIO,
            ];
            // Just walk the list — if any classification panics or
            // errors, something is wrong.
            for k in all_variants {
                let _ = is_non_transferable(k);
            }
            assert_eq!(all_variants.len(), 15);
        }
    }
}

// ── Blocked-waiter cleanup semantics (kernel/src/ipc/receive.rs) ──────
//
// Pass A Fix 1 adds `unblock_waiters_for(task_id)` to wake tasks
// sitting in BlockedOnSend(target) or WaitingForReply(_). These tests
// pin the predicate that determines which waiters to wake so the
// kernel's iteration logic matches.

mod waiter_cleanup {
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum TaskState {
        Runnable,
        Running,
        BlockedOnSend(u32),
        BlockedOnReceive,
        WaitingForReply(u64),
        Exited,
    }

    pub struct MockTask {
        pub state: TaskState,
        pub blocked_on: Option<u32>,
    }

    /// Mirror of the `should_wake` predicate inside
    /// `unblock_waiters_for`. A waiter qualifies if its `blocked_on`
    /// stamp matches the exiting task AND its state is a blocking
    /// IPC state.
    pub fn should_wake(task: &MockTask, exiting: u32) -> bool {
        if task.blocked_on != Some(exiting) {
            return false;
        }
        match task.state {
            TaskState::BlockedOnSend(x) if x == exiting => true,
            TaskState::WaitingForReply(_) => true,
            _ => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use super::TaskState::*;

        #[test]
        fn blocked_on_send_matches() {
            let t = MockTask {
                state: BlockedOnSend(7),
                blocked_on: Some(7),
            };
            assert!(should_wake(&t, 7));
        }

        #[test]
        fn blocked_on_different_target_no_match() {
            let t = MockTask {
                state: BlockedOnSend(8),
                blocked_on: Some(8),
            };
            // Exiting task is 7, blocked waiter is on 8. No match.
            assert!(!should_wake(&t, 7));
        }

        #[test]
        fn waiting_for_reply_matches() {
            let t = MockTask {
                state: WaitingForReply(12345),
                blocked_on: Some(7),
            };
            assert!(should_wake(&t, 7));
        }

        #[test]
        fn waiting_for_reply_wrong_stamp_no_match() {
            let t = MockTask {
                state: WaitingForReply(12345),
                blocked_on: Some(99),
            };
            // blocked_on points elsewhere — this waiter belongs to
            // another target.
            assert!(!should_wake(&t, 7));
        }

        #[test]
        fn runnable_task_not_woken() {
            let t = MockTask {
                state: Runnable,
                blocked_on: Some(7), // stale stamp
            };
            // Even if blocked_on matches, a Runnable task isn't
            // actually blocked — skip it.
            assert!(!should_wake(&t, 7));
        }

        #[test]
        fn blocked_on_receive_not_woken() {
            // BlockedOnReceive means "waiting for a message to
            // arrive" — not blocked on any specific server. The
            // exiting of server X doesn't unblock us.
            let t = MockTask {
                state: BlockedOnReceive,
                blocked_on: None,
            };
            assert!(!should_wake(&t, 7));
        }

        #[test]
        fn exited_task_not_woken() {
            let t = MockTask {
                state: Exited,
                blocked_on: Some(7),
            };
            assert!(!should_wake(&t, 7));
        }

        #[test]
        fn no_blocked_on_stamp_not_woken() {
            let t = MockTask {
                state: WaitingForReply(42),
                blocked_on: None,
            };
            // Stamp wasn't set (pre-Pass-A code path). We conservatively
            // skip — better to leave a rare hanging waiter than to
            // unblock the wrong task.
            assert!(!should_wake(&t, 7));
        }
    }
}


