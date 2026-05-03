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
    compile_diff_into, layout, parse,
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
// This markup was AUTHORED BY DRAUG inside Folkering OS — generated
// by qwen2.5-coder via the Phase C v3 pipeline (Synapse VFS →
// MULTI_PATCH → cargo test → 3 passed). Pasted in verbatim from
// /root/draug-sandbox/archive/multi-0005-sysmon-lib.rs. Folkering
// is now showing a UI its own AI agent designed.
const DEMO_MARKUP: &str = r##"
<Window x="40" y="40" width="320" height="160" bg_color="#1E2030" corner_radius="8">
    <VBox padding="16" spacing="8">
        <HBox spacing="8">
            <Text color="#C0CAF5" font_size="14">CPU</Text>
            <VBox flex-grow="1"/>
            <Text color="#9ECE6A" font_size="14" bind_text="cpu_pct">--</Text>
        </HBox>
        <HBox spacing="8">
            <Text color="#C0CAF5" font_size="14">Memory</Text>
            <VBox flex-grow="1"/>
            <Text color="#9ECE6A" font_size="14" bind_text="mem_pct">--</Text>
        </HBox>
        <HBox spacing="8">
            <Text color="#C0CAF5" font_size="14">Uptime</Text>
            <VBox flex-grow="1"/>
            <Text color="#9ECE6A" font_size="14" bind_text="uptime">--</Text>
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

    // 3. Per-tick: read live system stats, bind them to the three
    //    keys Draug's markup expects (cpu_pct, mem_pct, uptime),
    //    recompile the diff'd display list, push to the ring.
    let mut state = AppState::new();
    let mut tick: u64 = 0;
    let mut cpu_buf = [0u8; 16];
    let mut mem_buf = [0u8; 16];
    let mut up_buf  = [0u8; 32];
    let mut builder = DisplayListBuilder::new();
    let mut diff = DiffState::new();
    let mut printed_once = false;

    loop {
        // CPU% — synapse-style "we don't have a CPU sampler yet so
        // approximate with tick activity". A real one would read
        // per-task scheduler counters; this is enough to prove the
        // binding pipeline.
        let cpu_pct = ((tick % 100) as u32) as u8;
        let cpu_n = format_pct(&mut cpu_buf, cpu_pct);
        let cpu_s = unsafe { core::str::from_utf8_unchecked(&cpu_buf[..cpu_n]) };
        state.set("cpu_pct", cpu_s);

        // Memory% from the kernel.
        let (_used, _total, mem_pct) = libfolk::sys::memory_stats();
        let mem_n = format_pct(&mut mem_buf, mem_pct.min(100) as u8);
        let mem_s = unsafe { core::str::from_utf8_unchecked(&mem_buf[..mem_n]) };
        state.set("mem_pct", mem_s);

        // Uptime in seconds.
        let secs = libfolk::sys::uptime() / 1000;
        let up_n = format_uptime(&mut up_buf, secs);
        let up_s = unsafe { core::str::from_utf8_unchecked(&up_buf[..up_n]) };
        state.set("uptime", up_s);

        compile_diff_into(&tree, &state, &mut diff, &mut builder);
        let bytes = builder.as_slice();
        if !printed_once {
            println!("[FOLKUI-DEMO] first display list = {} bytes", bytes.len());
            printed_once = true;
        }

        let ring = handle.as_ring();
        // `Full` just means the consumer is behind. Drop the frame
        // and try next tick — apps shouldn't spin on the ring.
        let _ = ring.push(bytes);

        tick = tick.wrapping_add(1);
        yield_cpu();
    }
}

/// Format `pct` as `"NN%"`. Returns the number of bytes written.
fn format_pct(buf: &mut [u8], pct: u8) -> usize {
    let mut i = 0;
    if pct >= 100 {
        if i < buf.len() { buf[i] = b'1'; i += 1; }
        if i < buf.len() { buf[i] = b'0'; i += 1; }
        if i < buf.len() { buf[i] = b'0'; i += 1; }
    } else if pct >= 10 {
        if i < buf.len() { buf[i] = b'0' + pct / 10; i += 1; }
        if i < buf.len() { buf[i] = b'0' + pct % 10; i += 1; }
    } else {
        if i < buf.len() { buf[i] = b'0' + pct; i += 1; }
    }
    if i < buf.len() { buf[i] = b'%'; i += 1; }
    i
}

/// Format `seconds` as `"Hh Mm Ss"` for short uptimes, `"Ds Hh Mm"`
/// for longer. Avoids `format!` to stay alloc-light.
fn format_uptime(buf: &mut [u8], total_secs: u64) -> usize {
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    let mut i = 0;
    let mut emit_num = |b: &mut [u8], i: &mut usize, n: u64| {
        if n == 0 {
            if *i < b.len() { b[*i] = b'0'; *i += 1; }
            return;
        }
        let start = *i;
        let mut x = n;
        while x > 0 && *i < b.len() {
            b[*i] = b'0' + (x % 10) as u8;
            x /= 10;
            *i += 1;
        }
        b[start..*i].reverse();
    };
    if days > 0 {
        emit_num(buf, &mut i, days);
        if i < buf.len() { buf[i] = b'd'; i += 1; }
        if i < buf.len() { buf[i] = b' '; i += 1; }
        emit_num(buf, &mut i, hours);
        if i < buf.len() { buf[i] = b'h'; i += 1; }
    } else if hours > 0 {
        emit_num(buf, &mut i, hours);
        if i < buf.len() { buf[i] = b'h'; i += 1; }
        if i < buf.len() { buf[i] = b' '; i += 1; }
        emit_num(buf, &mut i, mins);
        if i < buf.len() { buf[i] = b'm'; i += 1; }
    } else {
        emit_num(buf, &mut i, mins);
        if i < buf.len() { buf[i] = b'm'; i += 1; }
        if i < buf.len() { buf[i] = b' '; i += 1; }
        emit_num(buf, &mut i, secs);
        if i < buf.len() { buf[i] = b's'; i += 1; }
    }
    i
}

fn idle_forever() -> ! {
    loop { yield_cpu(); }
}
