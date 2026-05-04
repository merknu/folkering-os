//! sysmon-demo — second producer for the multi-window victory lap.
//!
//! Runs alongside folkui-demo (the calculator) on the same compositor.
//! Both register their own gfx ring; the compositor drains all slots
//! per frame and dispatches each app's display list, so we get two
//! windows on screen at the same time without touching the
//! compositor's main loop.
//!
//! What it draws: a 280×180 panel at (400, 60) — right of the calc's
//! (100..360) box, no overlap so a render-graph z-order pass isn't
//! load-bearing yet. Three live-updating stats bound via `bind_text`:
//!
//!   Memory:  43%
//!   Uptime:  120s
//!   Heap:    202 KB
//!
//! Updates once a second. Cheap; the diff path means each tick only
//! re-emits a `[DrawRect bg, DrawText new_value]` pair per binding
//! that actually changed (typically just `uptime_s`).

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, uptime, memory_stats};
use libfolk::sys::compositor::{register_gfx_ring, COMPOSITOR_TASK_ID};
use libfolk::gfx::RingHandle;
use libfolk::gfx::DisplayListBuilder;
use libfolkui::{
    compile_diff_into, layout, parse,
    AppState, DiffState, LayoutConstraint,
};

// ── Bump allocator (matches folkui-demo / shell pattern) ────────────

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

// ── Markup ──────────────────────────────────────────────────────────
//
// Window placed at (400, 60) so it doesn't overlap with the calculator
// at (100, 60)..(360, 380). Three rows of (label, value) using HBox
// + bind_text. font_size=14 matches the rest of the OS chrome.

const SYSMON_MARKUP: &str = r##"
<Window x="400" y="60" width="280" height="180" bg_color="#1E2030" corner_radius="8">
    <VBox padding="16" spacing="8">
        <Text color="#7AA2F7" font_size="14">SysMon</Text>
        <HBox spacing="8">
            <Text color="#C0CAF5" font_size="14">Memory:</Text>
            <Text color="#9ECE6A" font_size="14" bind_text="mem_pct">--</Text>
        </HBox>
        <HBox spacing="8">
            <Text color="#C0CAF5" font_size="14">Uptime:</Text>
            <Text color="#9ECE6A" font_size="14" bind_text="uptime_s">--</Text>
        </HBox>
        <HBox spacing="8">
            <Text color="#C0CAF5" font_size="14">Heap:</Text>
            <Text color="#9ECE6A" font_size="14" bind_text="heap_kb">--</Text>
        </HBox>
    </VBox>
</Window>
"##;

// Different vaddr from folkui-demo so the per-task private mappings
// don't get confused if both apps are spawned. (They live in separate
// address spaces, but reusing slot numbers is asking for tooling
// confusion later.)
const PRODUCER_RING_VADDR: usize = 0x4000_0020_0000;

entry!(main);

fn main() -> ! {
    println!("[SYSMON-DEMO] starting up");

    let handle = match RingHandle::create_at(PRODUCER_RING_VADDR) {
        Ok(h) => h,
        Err(e) => {
            println!("[SYSMON-DEMO] ring create failed: {:?}", e);
            idle_forever();
        }
    };
    println!("[SYSMON-DEMO] ring created shmem_id={}", handle.id);

    if let Err(e) = handle.grant_to(COMPOSITOR_TASK_ID) {
        println!("[SYSMON-DEMO] grant_to compositor failed: {:?}", e);
        idle_forever();
    }

    let slot = match register_gfx_ring(handle.id) {
        Ok(s) => s,
        Err(e) => {
            println!("[SYSMON-DEMO] register_gfx_ring failed: {:?}", e);
            idle_forever();
        }
    };
    println!("[SYSMON-DEMO] registered as compositor slot {}", slot);

    let mut tree = match parse(SYSMON_MARKUP) {
        Ok(t) => t,
        Err(e) => {
            println!("[SYSMON-DEMO] parse failed: {:?}", e);
            idle_forever();
        }
    };
    layout(&mut tree, LayoutConstraint {
        x: 0, y: 0,
        max_w: 1280, max_h: 800,
    });

    let mut state = AppState::new();
    let mut diff = DiffState::new();
    let mut builder = DisplayListBuilder::new();

    // String buffers for the value formatters. Bump-allocated so we
    // can't free them; we re-use the same fixed-capacity String each
    // tick by clearing it before formatting. AppState::set copies the
    // value, so reusing our buffer doesn't alias.
    use alloc::string::String;
    let mut mem_buf: String = String::with_capacity(8);
    let mut up_buf: String = String::with_capacity(16);
    let mut heap_buf: String = String::with_capacity(16);

    let mut last_tick_s: u64 = 0;
    let mut last_mem_pct: u32 = 0xFFFF;
    let mut last_heap_kb: u32 = 0xFFFF;
    let mut printed_first = false;

    loop {
        // 1 Hz update cadence — anything finer is noise on the eye and
        // wastes the diff cache. The compositor still polls the ring
        // every frame; we just don't push new bytes when nothing
        // changed.
        let now_ms = uptime();
        let now_s = now_ms / 1000;
        if !printed_first || now_s != last_tick_s {
            last_tick_s = now_s;

            // Memory: percentage of physical memory in use.
            let (_used_mb, _total_mb, mem_pct) = memory_stats();
            // Heap: per-task bump usage in KB. We can't read the
            // kernel's allocator from userspace, so we instead read
            // libfolk's reported heap stats which surface PMM-side
            // numbers. The point is just to show *something* live —
            // pick the metric that actually changes between ticks.
            let heap_kb = heap_kb_estimate();

            // Always update on first tick to prime the diff cache;
            // afterwards only push if a value actually changed, so
            // the wire stays quiet on idle frames.
            let changed = !printed_first
                || mem_pct != last_mem_pct
                || heap_kb != last_heap_kb;

            if changed {
                mem_buf.clear();
                push_u32_pct(&mut mem_buf, mem_pct);
                state.set("mem_pct", mem_buf.as_str());

                heap_buf.clear();
                push_u32_kb(&mut heap_buf, heap_kb);
                state.set("heap_kb", heap_buf.as_str());

                last_mem_pct = mem_pct;
                last_heap_kb = heap_kb;
            }

            // Uptime always changes — set it every tick.
            up_buf.clear();
            push_u32_s(&mut up_buf, now_s as u32);
            state.set("uptime_s", up_buf.as_str());

            compile_diff_into(&tree, &state, &mut diff, &mut builder);
            let bytes = builder.as_slice();
            if !printed_first {
                println!("[SYSMON-DEMO] first display list = {} bytes", bytes.len());
                printed_first = true;
            }
            let ring = handle.as_ring();
            let _ = ring.push(bytes);
        }
        yield_cpu();
    }
}

fn heap_kb_estimate() -> u32 {
    // libfolk surfaces PMM stats — `memory_stats()` returns
    // (used_mb, total_mb, used_pct). Convert used MB to KB so the
    // value moves more visibly. Slight approximation: 1 MB ≈ 1024 KB
    // is fine for a status-panel readout.
    let (used_mb, _total_mb, _pct) = libfolk::sys::memory_stats();
    used_mb.saturating_mul(1024)
}

// ── Tiny u32 → ASCII formatters ─────────────────────────────────────
//
// AppState wants a `&str`. We keep a String buffer per binding and
// re-fill it each tick. Manual decimal because libfolk doesn't have
// `core::fmt` for our bump heap, and pulling it in just for these
// three labels is overkill.

fn push_u32(s: &mut alloc::string::String, mut v: u32) {
    if v == 0 { s.push('0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        s.push(buf[i] as char);
    }
}

fn push_u32_pct(s: &mut alloc::string::String, v: u32) {
    push_u32(s, v.min(100));
    s.push('%');
}

fn push_u32_s(s: &mut alloc::string::String, v: u32) {
    push_u32(s, v);
    s.push('s');
}

fn push_u32_kb(s: &mut alloc::string::String, v: u32) {
    push_u32(s, v);
    s.push_str(" KB");
}

fn idle_forever() -> ! {
    loop { yield_cpu(); }
}
