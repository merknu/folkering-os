//! Reactive state for `bind_text="..."` resolution.
//!
//! The agent emits markup once and updates state every frame. The
//! compiler resolves `<Text bind_text="key">` against this map at
//! emit-time, so apps don't have to mutate the DOM tree on each
//! tick — they just call `state.set(key, value)` and rebuild the
//! display list.
//!
//! Storage is a `Vec<(String, String)>` rather than `BTreeMap` for
//! the same reasons as `AttrMap`: bindings are typically tiny
//! (handful per app), linear scan wins on cache locality, and we
//! avoid `alloc::collections` in the dependency closure. Insertion
//! order is preserved so debug prints are stable.

extern crate alloc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Default, Clone)]
pub struct AppState {
    entries: Vec<(String, String)>,
}

impl AppState {
    /// Empty state — used as the "no-op" default by
    /// `compile_to_display_list`. Apps that don't need bindings can
    /// keep ignoring state entirely.
    pub const fn empty() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the value for `key`. `None` if no binding exists; the
    /// compiler treats that as "fall back to literal child text".
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Set or replace a binding. Allocates only when the key is new
    /// or the new value's length exceeds the existing capacity —
    /// steady-state updates with same-length values reuse the buffer.
    pub fn set(&mut self, key: &str, value: &str) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == key) {
            slot.1.clear();
            slot.1.push_str(value);
        } else {
            self.entries.push((key.to_string(), value.to_string()));
        }
    }

    /// Remove a binding. Returns `true` if it was present.
    pub fn remove(&mut self, key: &str) -> bool {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == key) {
            self.entries.swap_remove(pos);
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_round_trip() {
        let mut s = AppState::new();
        s.set("counter", "42");
        assert_eq!(s.get("counter"), Some("42"));
    }

    #[test]
    fn set_overwrites() {
        let mut s = AppState::new();
        s.set("k", "v1");
        s.set("k", "v2");
        assert_eq!(s.get("k"), Some("v2"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn missing_key_returns_none() {
        let s = AppState::new();
        assert!(s.get("nope").is_none());
    }

    #[test]
    fn remove_works() {
        let mut s = AppState::new();
        s.set("a", "1");
        s.set("b", "2");
        assert!(s.remove("a"));
        assert!(s.get("a").is_none());
        assert_eq!(s.get("b"), Some("2"));
    }
}
