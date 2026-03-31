//! Bare-Metal Micro-Benchmarks for Folkering OS
//!
//! Measures critical performance paths with TSC precision.
//! Outputs markdown table to COM1 serial, then exits QEMU.
//!
//! Enable with: cargo build --release --features boot-test

/// Read TSC inline (no function call overhead)
#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Calibrate TSC ticks/μs by measuring a short busy loop against uptime
fn calibrate_tsc_simple() -> u64 {
    // Wait for uptime to tick (10ms boundary)
    let t0 = crate::timer::uptime_ms();
    while crate::timer::uptime_ms() == t0 { core::hint::spin_loop(); }

    // Measure TSC over 10ms (one tick)
    let tsc_start = rdtsc();
    let ms_start = crate::timer::uptime_ms();
    while crate::timer::uptime_ms() < ms_start + 10 { core::hint::spin_loop(); }
    let tsc_end = rdtsc();

    let tsc_delta = tsc_end - tsc_start;
    tsc_delta / 10_000 // ticks per microsecond
}

/// Benchmark 1: Damage coalescing cost
fn bench_damage_coalesce(ticks_per_us: u64) -> (u64, u64, u64) {
    use core::cmp::{min, max};

    // Inline damage tracker to avoid no_std issues
    #[derive(Clone, Copy)]
    struct Rect { x: u32, y: u32, w: u32, h: u32 }

    impl Rect {
        fn intersects(&self, o: &Rect) -> bool {
            self.x < o.x + o.w && self.x + self.w > o.x &&
            self.y < o.y + o.h && self.y + self.h > o.y
        }
        fn union(&self, o: &Rect) -> Rect {
            let x = min(self.x, o.x);
            let y = min(self.y, o.y);
            Rect { x, y,
                w: max(self.x + self.w, o.x + o.w) - x,
                h: max(self.y + self.h, o.y + o.h) - y }
        }
    }

    let screen_w: u32 = 1024;
    let screen_h: u32 = 768;
    let mut regions = alloc::vec::Vec::<Rect>::with_capacity(20);

    // Generate 500 overlapping rects
    let t_start = rdtsc();
    for i in 0..500u32 {
        let x = (i * 7) % screen_w;
        let y = (i * 11) % screen_h;
        let w = min(50, screen_w - x);
        let h = min(30, screen_h - y);
        if w == 0 || h == 0 { continue; }

        let mut merged = Rect { x, y, w, h };
        let mut j = 0;
        while j < regions.len() {
            if regions[j].intersects(&merged) {
                merged = regions[j].union(&merged);
                regions.swap_remove(j);
                j = 0;
            } else { j += 1; }
        }
        regions.push(merged);
        if regions.len() > 10 {
            // Collapse
            let mut bbox = regions[0];
            for r in &regions[1..] { bbox = bbox.union(r); }
            regions.clear();
            regions.push(bbox);
        }
    }
    let t_end = rdtsc();

    let total_us = (t_end - t_start) / ticks_per_us;
    let per_rect_us = total_us / 500;
    let final_rects = regions.len() as u64;

    (total_us, per_rect_us, final_rects)
}

/// Benchmark 2: Intent parser / string extraction cost
fn bench_intent_parse(ticks_per_us: u64) -> (u64, u64) {
    // Simulate a large DeepSeek-R1 response with <think> block
    let mut payload = alloc::string::String::with_capacity(4096);
    payload.push_str("<think>\n");
    for i in 0..100 {
        payload.push_str("Step ");
        // Simple number formatting
        if i >= 100 { payload.push((b'0' + (i / 100) as u8) as char); }
        if i >= 10 { payload.push((b'0' + ((i / 10) % 10) as u8) as char); }
        payload.push((b'0' + (i % 10) as u8) as char);
        payload.push_str(": I need to analyze the window layout.\n");
    }
    payload.push_str("</think>\n");
    payload.push_str(r#"{"action": "move_window", "window_id": 3, "x": 200, "y": 150}"#);

    // Run 100 parse iterations
    let t_start = rdtsc();
    for _ in 0..100 {
        // Inline think-strip + JSON extract
        let trimmed = payload.trim();
        let effective = if let Some(_) = trimmed.find("<think>") {
            if let Some(end) = trimmed.find("</think>") {
                trimmed[end + 8..].trim()
            } else { trimmed }
        } else { trimmed };

        // Extract action field
        if let Some(start) = effective.find("\"action\":") {
            let rest = &effective[start + 9..];
            let rest = rest.trim_start();
            if rest.starts_with('"') {
                let inner = &rest[1..];
                let end = inner.find('"').unwrap_or(0);
                let _action = &inner[..end];
            }
        }
    }
    let t_end = rdtsc();

    let total_us = (t_end - t_start) / ticks_per_us;
    let per_parse_us = total_us / 100;

    (total_us, per_parse_us)
}

/// Benchmark 3: Syscall round-trip cost
fn bench_syscall_cost(ticks_per_us: u64) -> (u64, u64) {
    let iterations = 10_000u64;

    let t_start = rdtsc();
    for _ in 0..iterations {
        // SYS_GET_PID = syscall 0 in libfolk
        unsafe {
            core::arch::asm!(
                "mov rax, 0",   // SYS_GET_PID (or SYS_YIELD=1)
                "syscall",
                out("rax") _,
                out("rcx") _,
                out("r11") _,
                options(nomem, nostack)
            );
        }
    }
    let t_end = rdtsc();

    let total_us = (t_end - t_start) / ticks_per_us;
    let per_call_us = total_us / iterations;

    (total_us, per_call_us)
}

extern crate alloc;

/// Run all benchmarks and output results to serial
pub fn run_benchmarks() {
    crate::serial_strln!("");
    crate::serial_strln!("# Folkering OS Bare-Metal Benchmarks");
    crate::serial_strln!("");

    // Calibrate TSC
    crate::serial_str!("Calibrating TSC... ");
    let tpu = calibrate_tsc_simple();
    crate::serial_str!("OK (");
    crate::drivers::serial::write_dec(tpu as u32);
    crate::serial_strln!(" ticks/us)");
    crate::serial_strln!("");

    if tpu == 0 {
        crate::serial_strln!("ERROR: TSC calibration failed (timer not ticking)");
        crate::test_harness::exit_failure();
    }

    // Header
    crate::serial_strln!("| Benchmark                   | Total (us) | Per-op (us) | Notes           |");
    crate::serial_strln!("|-----------------------------|------------|-------------|-----------------|");

    // Bench 1: Damage coalescing
    let (d_total, d_per, d_rects) = bench_damage_coalesce(tpu);
    crate::serial_str!("| Damage coalesce (500 rects) | ");
    print_padded(d_total, 10);
    crate::serial_str!(" | ");
    print_padded(d_per, 11);
    crate::serial_str!(" | ");
    crate::drivers::serial::write_dec(d_rects as u32);
    crate::serial_strln!(" final rects   |");

    // Bench 2: Intent parse
    let (i_total, i_per) = bench_intent_parse(tpu);
    crate::serial_str!("| Intent parse (4KB, x100)    | ");
    print_padded(i_total, 10);
    crate::serial_str!(" | ");
    print_padded(i_per, 11);
    crate::serial_strln!(" | think+JSON      |");

    // Bench 3: Syscall cost (skipped — must run from userspace ring 3)
    crate::serial_strln!("| Syscall round-trip (10000x) | N/A        | N/A         | needs userspace |");

    crate::serial_strln!("");
    crate::serial_strln!("[BENCH] All benchmarks complete.");

    crate::test_harness::exit_success();
}

fn print_padded(val: u64, _width: usize) {
    crate::drivers::serial::write_dec(val as u32);
}
