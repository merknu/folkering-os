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
use libfolk::sys::compositor::{register_gfx_ring, COMPOSITOR_TASK_ID};
use libfolk::gfx::RingHandle;
use libfolk::gfx::DisplayListBuilder;
use libfolkui::{
    compile_into, layout, parse,
    AppState, LayoutConstraint,
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
const DEMO_MARKUP: &str = concat!(
    r##"<Window x="40" y="40" width="320" height="140" bg_color="#1E2030" corner_radius="8">"##,
    r##"  <VBox padding="16" spacing="8">"##,
    r##"    <Text color="#C0CAF5" font_size="18">Hello from libfolkui</Text>"##,
    r##"    <Text color="#A9B1D6" font_size="14" bind_text="counter">tick=0</Text>"##,
    r##"    <Button bg_color="#7AA2F7" corner_radius="6">Click me</Button>"##,
    r##"  </VBox>"##,
    r##"</Window>"##,
);

/// Reserved virtual address for the producer's ring view. Picked to
/// stay clear of the `RING_BASE_VADDR=0x6000_0000_0000` zone the
/// compositor uses for *its* mappings — we live in the
/// per-task private half. 1 MiB strides match the compositor's
/// reservation, so a future per-task ring zone can mirror this layout
/// across both sides without renumbering.
const PRODUCER_RING_VADDR: usize = 0x4000_0000_0000;

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

    // 3. Push display lists with a live-updating counter binding.
    //    Each tick we bump `counter`, set it on AppState, recompile,
    //    and push. The compiler resolves <Text bind_text="counter">
    //    against state, so the on-screen panel shows an incrementing
    //    value — proof that reactive bindings reach pixels.
    let mut state = AppState::new();
    let mut counter: u64 = 0;
    let mut buf = [0u8; 24]; // "tick=NNNNNNNNNNNNNNNN\0"
    // Single builder reused across frames — `compile_into` clears it
    // before re-filling, so the heap buffer's capacity stays warm and
    // we don't leak through the bump allocator (which never frees).
    let mut builder = DisplayListBuilder::new();
    let mut printed_once = false;

    loop {
        let written = format_counter(&mut buf, counter);
        // SAFETY: `format_counter` writes ASCII bytes only.
        let s = unsafe { core::str::from_utf8_unchecked(&buf[..written]) };
        state.set("counter", s);

        compile_into(&tree, &state, &mut builder);
        let bytes = builder.as_slice();
        if !printed_once {
            println!("[FOLKUI-DEMO] display list = {} bytes", bytes.len());
            printed_once = true;
        }

        let ring = handle.as_ring();
        // `Full` just means the consumer is behind. Drop the frame
        // and try next tick — apps shouldn't spin on the ring.
        let _ = ring.push(bytes);

        counter = counter.wrapping_add(1);
        yield_cpu();
    }
}

/// Render `counter` into `buf` as `b"tick=N"` ASCII. Returns the
/// number of bytes written. Uses a fixed-size scratch buffer
/// because we don't want to call `format!` (allocates) on every
/// frame.
fn format_counter(buf: &mut [u8], counter: u64) -> usize {
    const PREFIX: &[u8] = b"tick=";
    let mut i = 0;
    for &b in PREFIX {
        if i >= buf.len() { return i; }
        buf[i] = b;
        i += 1;
    }
    if counter == 0 {
        if i < buf.len() { buf[i] = b'0'; i += 1; }
        return i;
    }
    // Render digits backwards, then reverse in place.
    let start = i;
    let mut n = counter;
    while n > 0 && i < buf.len() {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    buf[start..i].reverse();
    i
}

fn idle_forever() -> ! {
    loop { yield_cpu(); }
}
