//! UI State Serialization — Token-Optimized for LLM Context
//!
//! Converts the compositor's window tree into a compact textual representation
//! that can be included in Gemini API prompts. Optimized for minimal token usage.
//!
//! Format (Compact Markdown):
//! ```
//! Screen: 1280x800
//! Window(ID:2, Z:1, Title:"Terminal", Bounds:600x400@100x100)
//!   Text: "root@folkering:~#"
//! Window(ID:4, Z:2, Title:"Clock", Bounds:100x100@800x600)
//! Omnibar: "ask anything..."
//! ```

extern crate alloc;
use alloc::string::String;
use alloc::format;

/// Maximum serialized UI state size (prevents token bloat)
const MAX_UI_STATE: usize = 2048;

/// Serialized window info for the UI tree
pub struct WindowInfo {
    pub id: u32,
    pub z_index: u32,
    pub title: String,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub visible_text: String,
}

/// Serialize a list of windows into compact markdown for LLM context.
/// Returns a string optimized for minimal token consumption.
pub fn serialize_ui_state(
    screen_w: u32,
    screen_h: u32,
    windows: &[WindowInfo],
    omnibar_text: &str,
) -> String {
    let mut out = String::with_capacity(MAX_UI_STATE);

    out.push_str(&format!("Screen: {}x{}\n", screen_w, screen_h));

    for win in windows {
        out.push_str(&format!(
            "Window(ID:{}, Z:{}, Title:\"{}\", Bounds:{}x{}@{}x{})\n",
            win.id, win.z_index, win.title, win.w, win.h, win.x, win.y
        ));

        if !win.visible_text.is_empty() {
            // Truncate visible text to save tokens
            let text = if win.visible_text.len() > 100 {
                &win.visible_text[..100]
            } else {
                &win.visible_text
            };
            out.push_str(&format!("  Text: \"{}\"\n", text));
        }
    }

    if !omnibar_text.is_empty() {
        out.push_str(&format!("Omnibar: \"{}\"\n", omnibar_text));
    }

    // Safety clamp
    if out.len() > MAX_UI_STATE {
        out.truncate(MAX_UI_STATE);
    }

    out
}
