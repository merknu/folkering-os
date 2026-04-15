//! Rendering — desktop, WASM fullscreen, overlays, present/flush.
//!
//! Phase C2 split this 1708-line monster into focused submodules. The two
//! 1000+ line mega-functions (`render_frame`, `present_and_flush`) became
//! orchestrators that call into:
//!
//! - `wasm_layer` — Streaming tick-tock + WASM fullscreen render path
//! - `desktop`    — Omnibar (with results panel) + folder/app launcher grid
//! - `windows`    — Window manager composite + spatial port pipelining
//! - `statusbar`  — Always-on-top clock, RAM, IQE telemetry, Alt+Tab HUD,
//!                  RAM history popup, targeted damage tracking
//! - `present`    — present_and_flush (shadow→FB, GPU flush, VGA mirror)
//!
//! `RenderContext` packages the previously 17-parameter call into a single
//! `&mut` reference (mirroring the `DispatchContext` pattern from C1).

extern crate alloc;

use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{
    Category, CursorState, IqeState, InputState, McpState, RenderState, StreamState,
    WasmState,
};
use compositor::window_manager::WindowManager;

mod wasm_layer;
mod desktop;
mod windows;
mod statusbar;
mod present;

pub use present::{present_and_flush, CursorColors};

// ── Layout / Color Constants ──────────────────────────────────────────

/// Layout/color constants needed for rendering.
/// Computed once in main() and passed in via `RenderContext`.
pub struct RenderLayout {
    pub folk_dark: u32,
    pub folk_accent: u32,
    pub white: u32,
    pub gray: u32,
    pub dark_gray: u32,
    pub omnibar_border: u32,
    pub text_box_x: usize,
    pub text_box_y: usize,
    pub text_box_w: usize,
    pub text_box_h: usize,
    pub chars_per_line: usize,
    pub results_x: usize,
    pub results_y: usize,
    pub results_w: usize,
    pub results_h: usize,
    pub folder_w: usize,
    pub folder_h: usize,
    pub folder_gap: usize,
    pub app_tile_w: usize,
    pub app_tile_h: usize,
    pub app_tile_gap: usize,
    pub app_tile_cols: usize,
    pub cursor_w: usize,
    pub cursor_h: usize,
}

pub(crate) const TEXT_PADDING: usize = 12;

pub struct RenderResult {
    pub did_work: bool,
    pub shadow_modified: bool,
}

/// All mutable state needed by the rendering pipeline, packed into one
/// struct so the call site doesn't have to thread 17 parameters.
pub struct RenderContext<'a> {
    pub fb: &'a mut FramebufferView,
    pub wm: &'a mut WindowManager,
    pub wasm: &'a mut WasmState,
    pub input: &'a InputState,
    pub render: &'a mut RenderState,
    pub mcp: &'a mut McpState,
    pub iqe: &'a IqeState,
    pub damage: &'a mut DamageTracker,
    pub draug: &'a mut DraugDaemon,
    pub categories: &'a [Category],
    pub layout: &'a RenderLayout,
    pub cursor: &'a CursorState,
    pub cursor_drawn: bool,
    pub cursor_bg: &'a mut [u32],
    pub ram_history: &'a [u8],
    pub ram_history_idx: usize,
    pub ram_history_count: usize,
}

/// Orchestrate one frame: streaming → WASM fullscreen → desktop → windows
/// → statusbar → damage tracking → cursor.
///
/// Each stage may early-out if its preconditions aren't met (e.g. desktop
/// rendering is skipped while a WASM app owns the framebuffer).
pub fn render_frame(ctx: &mut RenderContext) -> RenderResult {
    let mut did_work = false;

    // Stage 1 — Semantic Streams: Tick-Tock co-scheduling (if active)
    let is_streaming =
        ctx.wasm.streaming_upstream.is_some() && ctx.wasm.streaming_downstream.is_some();
    if is_streaming {
        wasm_layer::render_streaming(ctx);
        did_work = true;
    }

    let wasm_fullscreen =
        ctx.wasm.active_app.as_ref().map_or(false, |a| a.active) || is_streaming;

    // Stage 2 — WASM fullscreen mode (when an app owns the screen)
    if wasm_fullscreen {
        if wasm_layer::render_fullscreen_app(ctx) {
            did_work = true;
        }
    }

    // Stage 3 — Desktop UI (only when no WASM app is fullscreen)
    if !wasm_fullscreen {
        desktop::render_omnibar(ctx);
        desktop::render_app_launcher(ctx);
    }

    // Stage 4 — Window compositing + spatial pipelining
    if !wasm_fullscreen && ctx.wm.has_visible() {
        if windows::render_windows(ctx) {
            did_work = true;
        }
    }

    // Stage 5 — Always-on-top overlays (HUD, statusbar, RAM graph)
    statusbar::render_alt_tab_hud(ctx);
    statusbar::render_statusbar(ctx);
    if ctx.input.show_ram_graph && ctx.ram_history_count > 1 {
        statusbar::render_ram_graph(ctx);
    }

    // Stage 6 — Targeted damage tracking + cursor save
    statusbar::add_targeted_damage(ctx, wasm_fullscreen);
    if ctx.cursor_drawn {
        statusbar::save_cursor_bg(ctx);
    }

    RenderResult {
        did_work,
        shadow_modified: true,
    }
}

/// Inline rdtsc — duplicated from main.rs for use in profiling annotations.
#[inline(always)]
#[allow(dead_code)]
pub(crate) fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}
