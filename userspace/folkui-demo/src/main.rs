//! folkui-demo — end-to-end smoke test for the rapport's Del 1+4
//! pipeline.
//!
//! Drives the full producer half:
//! - libfolkui parses a literal DSML string
//! - layout fills bounds against a window-sized constraint
//! - the compiler emits display-list bytes
//! - libfolk::gfx::RingHandle creates a shmem-backed
//!   `IpcGraphicsRing`, grants it to the compositor task, and pushes
//!   the bytes per tick
//! - libfolk::sys::compositor::register_gfx_ring tells the compositor
//!   "drain this slot inside render_frame"
//!
//! Once this binary is running and registered, the compositor's
//! `gfx_rings::drain_all()` (#119/#120) walks the bytes and paints
//! pixels via `fill_rect`/`draw_char`. No FKUI, no AccessKit tree —
//! pure display-list pipeline.
//!
//! Scope of this PR: produces the binary. Wiring it into the boot
//! ramdisk is one folk-pack `--add` line away (in MCP server.py)
//! once this lands; the binary is intentionally idle-able so it
//! costs nothing to ship even when not auto-spawned.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::yield_cpu;
use libfolk::sys::compositor::{register_gfx_ring, register_input_ring, COMPOSITOR_TASK_ID};
use libfolk::gfx::RingHandle;
use libfolk::gfx::DisplayListBuilder;
use libfolk::gfx::input::{InputRingHandle, EventKind};
use libfolkui::{
    compile_diff_into, hit_test_id, layout, parse,
    AppState, DiffState, LayoutConstraint,
};

// ── Bump allocator ──────────────────────────────────────────────────
//
// Same pattern as draug-streamer / shell: 64 KiB heap in BSS. We
// allocate transient `Vec`s during DSML parse + layout + compile, so
// a real heap is convenient. The DSML tree, the laid-out DOM, and
// the display-list builder together need ~8 KiB on a typical frame;
// the rest is slack for the `Vec::with_capacity` re-allocs that
// happen the first frame.

const HEAP_SIZE: usize = 64 * 1024;

struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    offset: UnsafeCell<usize>,
}

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let offset = &mut *self.offset.get();
        let align = layout.align();
        let aligned = (*offset + align - 1) & !(align - 1);
        let new_offset = aligned + layout.size();
        if new_offset > HEAP_SIZE {
            core::ptr::null_mut()
        } else {
            *offset = new_offset;
            (*self.heap.get()).as_mut_ptr().add(aligned)
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    offset: UnsafeCell::new(0),
};

// ── DSML the agent would have produced ─────────────────────────────
//
// Hard-coded for the smoke test. A future demo replaces this with
// "ask Draug to author a UI" — that's the actual rapport endgame.
// This markup was AUTHORED BY DRAUG inside Folkering OS — qwen2.5-
// coder via Phase C v4 (Synapse VFS → MULTI_PATCH → cargo test).
// Pasted verbatim from
// /root/draug-sandbox/archive/multi-0006-calc-lib.rs.
//
// Every button has an `id` attribute so hit_test_id can route a
// click to the right calculator key.
const DEMO_MARKUP: &str = r##"
<Window x="100" y="60" width="260" height="320" bg_color="#1E2030" corner_radius="8">
    <VBox padding="12" spacing="6">
        <Text id="display" color="#9ECE6A" font_size="18" bind_text="display">0</Text>
        <HBox spacing="6">
            <Button id="btn_7" bg_color="#3A3A3A" corner_radius="4">7</Button>
            <Button id="btn_8" bg_color="#3A3A3A" corner_radius="4">8</Button>
            <Button id="btn_9" bg_color="#3A3A3A" corner_radius="4">9</Button>
            <Button id="btn_div" bg_color="#3A3A3A" corner_radius="4">/</Button>
        </HBox>
        <HBox spacing="6">
            <Button id="btn_4" bg_color="#3A3A3A" corner_radius="4">4</Button>
            <Button id="btn_5" bg_color="#3A3A3A" corner_radius="4">5</Button>
            <Button id="btn_6" bg_color="#3A3A3A" corner_radius="4">6</Button>
            <Button id="btn_mul" bg_color="#3A3A3A" corner_radius="4">*</Button>
        </HBox>
        <HBox spacing="6">
            <Button id="btn_1" bg_color="#3A3A3A" corner_radius="4">1</Button>
            <Button id="btn_2" bg_color="#3A3A3A" corner_radius="4">2</Button>
            <Button id="btn_3" bg_color="#3A3A3A" corner_radius="4">3</Button>
            <Button id="btn_sub" bg_color="#3A3A3A" corner_radius="4">-</Button>
        </HBox>
        <HBox spacing="6">
            <Button id="btn_0" bg_color="#3A3A3A" corner_radius="4">0</Button>
            <Button id="btn_clear" bg_color="#3A3A3A" corner_radius="4">C</Button>
            <Button id="btn_eq" bg_color="#3A3A3A" corner_radius="4">=</Button>
            <Button id="btn_add" bg_color="#3A3A3A" corner_radius="4">+</Button>
        </HBox>
    </VBox>
</Window>
"##;

/// Reserved virtual address for the producer's ring view. Picked to
/// stay clear of the `RING_BASE_VADDR=0x6000_0000_0000` zone the
/// compositor uses for *its* mappings — we live in the
/// per-task private half. 1 MiB strides match the compositor's
/// reservation, so a future per-task ring zone can mirror this layout
/// across both sides without renumbering.
const PRODUCER_RING_VADDR: usize = 0x4000_0000_0000;
/// Input ring lives in a separate per-task vaddr so the gfx and
/// input mappings don't collide. 1 MiB above the gfx ring's view.
const INPUT_RING_VADDR: usize = 0x4000_0010_0000;

entry!(main);

fn main() -> ! {
    println!("[FOLKUI-DEMO] starting up");

    // 1. Allocate + grant + register the ring. Failure on any step is
    //    fatal for the demo: there's no fallback rendering path here.
    let handle = match RingHandle::create_at(PRODUCER_RING_VADDR) {
        Ok(h) => h,
        Err(e) => {
            println!("[FOLKUI-DEMO] ring create failed: {:?}", e);
            idle_forever();
        }
    };
    println!("[FOLKUI-DEMO] ring created shmem_id={}", handle.id);

    if let Err(e) = handle.grant_to(COMPOSITOR_TASK_ID) {
        println!("[FOLKUI-DEMO] grant_to compositor failed: {:?}", e);
        idle_forever();
    }
    println!("[FOLKUI-DEMO] granted to compositor task {}", COMPOSITOR_TASK_ID);

    let slot = match register_gfx_ring(handle.id) {
        Ok(s) => s,
        Err(e) => {
            println!("[FOLKUI-DEMO] register_gfx_ring failed: {:?}", e);
            idle_forever();
        }
    };
    println!("[FOLKUI-DEMO] registered as compositor slot {}", slot);

    // 1b. Same dance for the input ring so the compositor can push
    //     mouse/key events back to us.
    let input = match InputRingHandle::create_at(INPUT_RING_VADDR) {
        Ok(h) => h,
        Err(e) => {
            println!("[FOLKUI-DEMO] input ring create failed: {:?}", e);
            idle_forever();
        }
    };
    if let Err(e) = input.grant_to(COMPOSITOR_TASK_ID) {
        println!("[FOLKUI-DEMO] input grant_to failed: {:?}", e);
        idle_forever();
    }
    if let Err(e) = register_input_ring(slot, input.id) {
        println!("[FOLKUI-DEMO] register_input_ring failed: {:?}", e);
        idle_forever();
    }
    println!("[FOLKUI-DEMO] input ring shmem={} bound to slot {}", input.id, slot);

    // 2. Parse + layout once. The DSML is static so we don't have to
    //    redo this every frame — only the display-list compile step
    //    runs in the loop. Conceptually the compiler also doesn't
    //    *need* to re-run, but doing so each tick exercises the full
    //    producer pipeline and keeps the ring drained.
    let mut tree = match parse(DEMO_MARKUP) {
        Ok(t) => t,
        Err(e) => {
            println!("[FOLKUI-DEMO] DSML parse failed: {:?}", e);
            idle_forever();
        }
    };
    layout(&mut tree, LayoutConstraint {
        x: 0, y: 0,
        max_w: 1024, max_h: 768, // matches the compositor's typical FB
    });

    // 3. Calculator state. The display string is what shows up on
    //    screen via bind_text="display". Each click resolves to a
    //    button id via hit_test_id, then the dispatch below mutates
    //    state.
    let mut state = AppState::new();
    let mut calc = Calc::new();
    state.set("display", calc.text());

    let mut clicks: u32 = 0;
    let mut builder = DisplayListBuilder::new();
    let mut diff = DiffState::new();
    let mut printed_once = false;

    loop {
        let mut state_dirty = false;

        // Drain pending click events. For each press, route it
        // through hit_test_id to find which calculator button (if
        // any) was clicked, then update the calculator state.
        while let Some(ev) = input.pop_event() {
            if ev.kind == EventKind::Mouse as u32 && ev.button == 1 && ev.down == 1 {
                clicks = clicks.wrapping_add(1);
                let hit = hit_test_id(&tree, ev.x, ev.y).unwrap_or("(none)");
                println!("[FOLKUI-DEMO] click #{} at ({},{}) hit={}",
                    clicks, ev.x, ev.y, hit);
                if calc.handle_button(hit) {
                    state.set("display", calc.text());
                    state_dirty = true;
                }
            }
        }
        // First-frame primer: state needs to be set so DiffState's
        // first walk sees the initial value.
        if !printed_once { state_dirty = true; }

        if state_dirty || !printed_once {
            compile_diff_into(&tree, &state, &mut diff, &mut builder);
            let bytes = builder.as_slice();
            if !printed_once {
                println!("[FOLKUI-DEMO] first display list = {} bytes", bytes.len());
                printed_once = true;
            }
            let ring = handle.as_ring();
            let _ = ring.push(bytes);
        }
        yield_cpu();
    }
}

/// Tiny stack-only calculator state. Holds a printable display
/// buffer plus the in-progress arithmetic — enough for the four
/// classic operators with single-line precedence.
struct Calc {
    /// What `bind_text="display"` shows. Owns its bytes; we don't
    /// allocate a `String` per frame.
    display: [u8; 32],
    display_len: usize,
    /// Accumulator: the operand entered before the operator.
    acc: i64,
    /// Pending operator. `0` = none.
    pending_op: u8,
    /// `true` when the current digit-stream replaces, rather than
    /// extends, the display (e.g. right after pressing an operator
    /// or `=`).
    fresh_entry: bool,
}

impl Calc {
    fn new() -> Self {
        let mut c = Self {
            display: [0; 32],
            display_len: 0,
            acc: 0,
            pending_op: 0,
            fresh_entry: true,
        };
        c.set_display_i64(0);
        c
    }

    fn text(&self) -> &str {
        // SAFETY: only ASCII digits / minus / spaces are written.
        unsafe { core::str::from_utf8_unchecked(&self.display[..self.display_len]) }
    }

    fn set_display_i64(&mut self, mut n: i64) {
        self.display_len = 0;
        if n == 0 {
            self.display[self.display_len] = b'0';
            self.display_len += 1;
            return;
        }
        if n < 0 {
            self.display[self.display_len] = b'-';
            self.display_len += 1;
            n = -n;
        }
        let start = self.display_len;
        let mut tmp = n;
        while tmp > 0 && self.display_len < self.display.len() {
            self.display[self.display_len] = b'0' + (tmp % 10) as u8;
            self.display_len += 1;
            tmp /= 10;
        }
        self.display[start..self.display_len].reverse();
    }

    /// Parse `display` as i64. Used when applying an operator —
    /// `acc <op> current_display = result`.
    fn read_display_i64(&self) -> i64 {
        let s = self.text();
        let mut n: i64 = 0;
        let bytes = s.as_bytes();
        let (sign, start) = if !bytes.is_empty() && bytes[0] == b'-' { (-1i64, 1) } else { (1, 0) };
        for &b in &bytes[start..] {
            if b.is_ascii_digit() {
                n = n.saturating_mul(10).saturating_add((b - b'0') as i64);
            }
        }
        sign * n
    }

    fn append_digit(&mut self, d: u8) {
        if self.fresh_entry {
            self.display_len = 0;
            self.fresh_entry = false;
        }
        // Avoid leading zero like "03"; replace the lone "0" with the
        // new digit.
        if self.display_len == 1 && self.display[0] == b'0' {
            self.display[0] = b'0' + d;
            return;
        }
        if self.display_len < self.display.len() {
            self.display[self.display_len] = b'0' + d;
            self.display_len += 1;
        }
    }

    fn apply_pending(&mut self) {
        let cur = self.read_display_i64();
        let result = match self.pending_op {
            b'+' => self.acc.saturating_add(cur),
            b'-' => self.acc.saturating_sub(cur),
            b'*' => self.acc.saturating_mul(cur),
            b'/' => if cur == 0 { 0 } else { self.acc / cur },
            _    => cur,
        };
        self.set_display_i64(result);
        self.acc = result;
    }

    /// Returns `true` when state changed (caller redraws).
    fn handle_button(&mut self, id: &str) -> bool {
        let digit = match id {
            "btn_0" => Some(0u8),
            "btn_1" => Some(1),
            "btn_2" => Some(2),
            "btn_3" => Some(3),
            "btn_4" => Some(4),
            "btn_5" => Some(5),
            "btn_6" => Some(6),
            "btn_7" => Some(7),
            "btn_8" => Some(8),
            "btn_9" => Some(9),
            _ => None,
        };
        if let Some(d) = digit {
            self.append_digit(d);
            return true;
        }
        match id {
            "btn_add" | "btn_sub" | "btn_mul" | "btn_div" => {
                self.apply_pending();
                self.pending_op = match id {
                    "btn_add" => b'+',
                    "btn_sub" => b'-',
                    "btn_mul" => b'*',
                    "btn_div" => b'/',
                    _ => 0,
                };
                self.fresh_entry = true;
                true
            }
            "btn_eq" => {
                self.apply_pending();
                self.pending_op = 0;
                self.fresh_entry = true;
                true
            }
            "btn_clear" => {
                self.acc = 0;
                self.pending_op = 0;
                self.fresh_entry = true;
                self.set_display_i64(0);
                true
            }
            _ => false,
        }
    }
}

fn idle_forever() -> ! {
    loop { yield_cpu(); }
}
