// Folkering OS Host-Side Test Suite
//
// Tests pure logic from compositor crate that has no hardware dependencies.
// Runs with standard `cargo test` on the host machine.
//
// Usage: cd tests && cargo test

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
