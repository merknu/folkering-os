//! SMP — Symmetric Multi-Processing
//!
//! Boots Application Processors via INIT-SIPI-SIPI, runs them as compute
//! workers for parallel GEMM. APs stay in kernel mode, spin-waiting for work.

use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use core::ptr::{read_volatile, write_volatile};

/// Maximum CPUs supported (must match boot.S stack allocation)
pub const MAX_CPUS: usize = 16;

/// Number of APs that have completed initialization
pub static AP_READY_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Physical address where AP trampoline is copied (SIPI vector = 0x08)
const TRAMPOLINE_PHYS: usize = 0x8000;

/// Per-AP stack size (16 KB each)
const AP_STACK_SIZE: usize = 16384;

/// AP stacks allocated in kernel BSS (15 APs × 16KB = 240KB)
#[repr(C, align(4096))]
struct ApStacks([u8; AP_STACK_SIZE * (MAX_CPUS - 1)]);
static mut AP_STACKS: ApStacks = ApStacks([0; AP_STACK_SIZE * (MAX_CPUS - 1)]);

// --- Work Distribution ---

/// Per-CPU work descriptors for parallel GEMM
#[repr(C)]
pub struct GemmWork {
    pub input_ptr: u64,   // *const f32 (in task address space)
    pub weight_ptr: u64,  // *const u8 (quantized weights)
    pub output_ptr: u64,  // *mut f32 (output buffer)
    pub k: u32,           // inner dimension
    pub n: u32,           // total output columns
    pub col_start: u32,   // first column for this worker
    pub col_end: u32,     // last column (exclusive)
    pub quant_type: u8,   // 0=Q6_K
}

static mut WORK_ITEMS: [GemmWork; MAX_CPUS] = unsafe { core::mem::zeroed() };

/// Per-CPU flags: BSP sets READY=1, AP clears READY=0 and sets DONE=1
static WORK_READY: [AtomicU8; MAX_CPUS] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_CPUS]
};
static WORK_DONE: [AtomicU8; MAX_CPUS] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_CPUS]
};

/// Page table (CR3) workers should load to access userspace memory
static WORKER_CR3: AtomicU64 = AtomicU64::new(0);

/// Number of usable APs (set during boot)
static ONLINE_AP_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn ap_count() -> usize {
    ONLINE_AP_COUNT.load(Ordering::Relaxed)
}

// --- AP Trampoline ---
// The trampoline is 16-bit real-mode code that transitions to 64-bit long mode.
// It's copied to physical 0x8000 before sending SIPI.
//
// Data block layout at 0x8000 (filled by BSP):
//   +0x00: u32 boot_flag    (AP sets to 1 when booted)
//   +0x04: u32 cpu_index    (0-based index among APs)
//   +0x08: u64 cr3          (kernel page table physical)
//   +0x10: u64 stack_top    (virtual address)
//   +0x18: u64 entry_fn     (ap_entry_rust pointer)
//   +0x20: u64 gdt_ptr      (6-byte GDTR: limit u16 + base u64, padded)

// Trampoline binary (assembled inline)
core::arch::global_asm!(
    // Put trampoline code in a named section so we can find it
    ".section .rodata.ap_trampoline, \"a\"",
    ".global ap_trampoline_start",
    ".global ap_trampoline_end",
    "ap_trampoline_start:",

    // === 16-bit Real Mode ===
    ".code16",
    "cli",
    "cld",
    "xor ax, ax",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",

    // Load GDT from data block at 0x8020
    "lgdt [0x8020]",

    // Enable Protected Mode (PE bit in CR0)
    "mov eax, cr0",
    "or eax, 1",
    "mov cr0, eax",

    // Far jump to 32-bit code (code segment = 0x08 in our GDT)
    // Use .byte encoding for 16-bit far jump with 32-bit operand
    ".byte 0x66, 0xEA",          // operand-size prefix + far JMP
    ".long ap_trampoline_32 - ap_trampoline_start + 0x8000",
    ".word 0x08",                // code segment selector

    // === 32-bit Protected Mode ===
    ".code32",
    "ap_trampoline_32:",
    "mov ax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",

    // Enable PAE (CR4 bit 5)
    "mov eax, cr4",
    "or eax, (1 << 5)",
    "mov cr4, eax",

    // Load CR3 from data block (phys 0x8008, low 32 bits)
    "mov eax, [0x8008]",
    "mov cr3, eax",

    // Enable Long Mode via IA32_EFER MSR (set LME bit 8)
    "mov ecx, 0xC0000080",
    "rdmsr",
    "or eax, (1 << 8)",
    "wrmsr",

    // Enable Paging (CR0 bit 31) + keep PE (bit 0)
    "mov eax, cr0",
    "or eax, (1 << 31)",
    "mov cr0, eax",

    // Far jump to 64-bit code
    ".byte 0xEA",
    ".long ap_trampoline_64 - ap_trampoline_start + 0x8000",
    ".word 0x08",

    // === 64-bit Long Mode ===
    ".code64",
    "ap_trampoline_64:",

    // Set data segments
    "mov ax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov fs, ax",
    "mov gs, ax",
    "mov ss, ax",

    // Load stack from data block
    "mov rsp, [0x8010]",

    // Load entry function pointer
    "mov rax, [0x8018]",

    // Load cpu_index into RDI (first argument)
    "xor rdi, rdi",
    "mov edi, [0x8004]",

    // Signal boot complete
    "mov dword ptr [0x8000], 1",

    // Jump to Rust entry
    "jmp rax",

    "ap_trampoline_end:",
    ".code64",  // restore default code size
);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
}

// Minimal GDT for the trampoline (null + code64 + data64)
#[repr(C, align(16))]
struct TrampolineGdt {
    null: u64,
    code64: u64,
    data64: u64,
}

static TRAMPOLINE_GDT: TrampolineGdt = TrampolineGdt {
    null: 0,
    // Code segment: L=1 (long mode), P=1, DPL=0, S=1, Type=Execute/Read
    code64: 0x00AF_9A00_0000_FFFF,
    // Data segment: P=1, DPL=0, S=1, Type=Read/Write
    data64: 0x00CF_9200_0000_FFFF,
};

/// GDTR structure for LGDT instruction
#[repr(C, packed)]
struct GdtPtr {
    limit: u16,
    base: u64,
}

/// Install trampoline code at physical 0x8000 and set up identity mapping
pub fn install_trampoline() {
    let hhdm = crate::memory::paging::hhdm_offset();

    // Identity-map the first 2MB for the AP trampoline
    // (AP starts in real mode at physical 0x8000, needs identity mapping)
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    for page_addr in (0..0x200000usize).step_by(4096) {
        let _ = crate::memory::paging::map_page(page_addr, page_addr, flags);
    }

    // Copy trampoline code to physical 0x8000
    let trampoline_dst = (hhdm + TRAMPOLINE_PHYS) as *mut u8;
    let trampoline_src = unsafe { &ap_trampoline_start as *const u8 };
    let trampoline_size = unsafe {
        (&ap_trampoline_end as *const u8 as usize) - (&ap_trampoline_start as *const u8 as usize)
    };

    unsafe {
        core::ptr::copy_nonoverlapping(trampoline_src, trampoline_dst, trampoline_size);
    }

    // Write the GDT for trampoline at offset 0x20 from TRAMPOLINE_PHYS
    // GDTR format: u16 limit, u64 base (10 bytes)
    let gdt_phys = &TRAMPOLINE_GDT as *const TrampolineGdt as usize;
    // The GDT physical address = gdt_phys minus kernel virt base... but actually
    // the GDT is in kernel .rodata, which is in higher half. We need the PHYSICAL
    // address because LGDT runs before paging is enabled.
    // Since GDT is at a higher-half virtual address, we compute: phys = virt - kernel_virt_base
    // But actually, when the AP does LGDT in real mode, it uses linear addresses.
    // Before PE is set, LGDT uses a 16:32 pointer. After PE is set, LGDT uses
    // whatever segment base applies.
    //
    // Simplest: embed a copy of the GDT in the trampoline page itself.
    let gdt_dst = (hhdm + TRAMPOLINE_PHYS + 0x100) as *mut u64;
    unsafe {
        gdt_dst.write(TRAMPOLINE_GDT.null);
        gdt_dst.add(1).write(TRAMPOLINE_GDT.code64);
        gdt_dst.add(2).write(TRAMPOLINE_GDT.data64);
    }

    // Write GDTR at offset 0x20 (pointing to GDT copy at 0x8100)
    let gdtr_dst = (hhdm + TRAMPOLINE_PHYS + 0x20) as *mut u8;
    let gdt_limit: u16 = (3 * 8 - 1) as u16; // 3 entries × 8 bytes - 1
    let gdt_base: u64 = (TRAMPOLINE_PHYS + 0x100) as u64; // physical address of GDT copy
    unsafe {
        (gdtr_dst as *mut u16).write(gdt_limit);
        ((gdtr_dst as usize + 2) as *mut u64).write_unaligned(gdt_base);
    }

    crate::serial_str!("[SMP] Trampoline installed at phys 0x");
    crate::drivers::serial::write_hex(TRAMPOLINE_PHYS as u64);
    crate::serial_str!(", size=");
    crate::drivers::serial::write_dec(trampoline_size as u32);
    crate::serial_str!(" bytes\n");
}

/// Boot all Application Processors
pub fn boot_aps() {
    let ap_ids = super::acpi::ap_apic_ids();
    if ap_ids.is_empty() {
        crate::serial_str!("[SMP] No APs to boot\n");
        return;
    }

    let hhdm = crate::memory::paging::hhdm_offset();

    // Get kernel CR3
    let kernel_cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) kernel_cr3); }

    // AP stacks from our BSS allocation
    let stacks_base = unsafe { AP_STACKS.0.as_ptr() as usize };

    let trampoline_vector: u8 = (TRAMPOLINE_PHYS >> 12) as u8; // 0x08

    for (i, &apic_id) in ap_ids.iter().enumerate() {
        let data_base = hhdm + TRAMPOLINE_PHYS;

        // Stack for this AP: base + (i+1) * AP_STACK_SIZE
        // Each AP gets its own 16KB stack region, stack grows downward
        let stack_top = stacks_base + (i + 1) * AP_STACK_SIZE;

        // Fill data block
        unsafe {
            write_volatile((data_base + 0x00) as *mut u32, 0);              // boot_flag = 0
            write_volatile((data_base + 0x04) as *mut u32, (i + 1) as u32); // cpu_index (1-based, 0=BSP)
            write_volatile((data_base + 0x08) as *mut u64, kernel_cr3);     // CR3
            write_volatile((data_base + 0x10) as *mut u64, stack_top as u64); // stack
            write_volatile((data_base + 0x18) as *mut u64, ap_entry_rust as u64); // entry
        }

        crate::serial_str!("[SMP] Booting AP ");
        crate::drivers::serial::write_dec(apic_id as u32);
        crate::serial_str!(" (index ");
        crate::drivers::serial::write_dec((i + 1) as u32);
        crate::serial_str!(")...\n");

        // Send INIT-SIPI-SIPI
        send_init_sipi_sipi(apic_id, trampoline_vector);

        // Wait for AP to set boot flag (with timeout)
        let flag_ptr = (hhdm + TRAMPOLINE_PHYS) as *const u32;
        let mut booted = false;
        for _ in 0..10_000_000u64 {
            if unsafe { read_volatile(flag_ptr) } == 1 {
                booted = true;
                break;
            }
            core::hint::spin_loop();
        }

        if booted {
            crate::serial_str!("[SMP] AP ");
            crate::drivers::serial::write_dec(apic_id as u32);
            crate::serial_str!(" booted OK\n");
        } else {
            crate::serial_str!("[SMP] AP ");
            crate::drivers::serial::write_dec(apic_id as u32);
            crate::serial_str!(" FAILED (timeout)\n");
        }
    }

    // Wait for all APs to reach their worker loops
    let mut ready = 0;
    for _ in 0..50_000_000u64 {
        ready = AP_READY_COUNT.load(Ordering::Acquire);
        if ready >= ap_ids.len() {
            break;
        }
        core::hint::spin_loop();
    }

    ONLINE_AP_COUNT.store(ready, Ordering::Relaxed);

    crate::serial_str!("[SMP] ");
    crate::drivers::serial::write_dec(ready as u32);
    crate::serial_str!(" APs online as compute workers\n");
}

// --- INIT-SIPI-SIPI ---

const APIC_ICR_LOW: usize = 0x300;
const APIC_ICR_HIGH: usize = 0x310;

fn send_init_sipi_sipi(apic_id: u8, vector: u8) {
    let apic_virt = super::apic::lapic_virt_addr();

    unsafe {
        // INIT IPI
        write_volatile((apic_virt + APIC_ICR_HIGH) as *mut u32, (apic_id as u32) << 24);
        write_volatile((apic_virt + APIC_ICR_LOW) as *mut u32, 0x4500); // INIT | Level Assert
        busy_wait_us(10_000); // 10ms

        // INIT de-assert
        write_volatile((apic_virt + APIC_ICR_HIGH) as *mut u32, (apic_id as u32) << 24);
        write_volatile((apic_virt + APIC_ICR_LOW) as *mut u32, 0x8500); // INIT | Level De-assert
        busy_wait_us(10_000); // 10ms

        // First SIPI
        write_volatile((apic_virt + APIC_ICR_HIGH) as *mut u32, (apic_id as u32) << 24);
        write_volatile((apic_virt + APIC_ICR_LOW) as *mut u32, 0x0600 | (vector as u32)); // STARTUP
        busy_wait_us(200); // 200μs

        // Second SIPI (per Intel spec)
        write_volatile((apic_virt + APIC_ICR_HIGH) as *mut u32, (apic_id as u32) << 24);
        write_volatile((apic_virt + APIC_ICR_LOW) as *mut u32, 0x0600 | (vector as u32)); // STARTUP
        busy_wait_us(200); // 200μs
    }
}

fn busy_wait_us(us: u64) {
    // Conservative: ~1000 iterations per μs at 1GHz+ (covers WHPX and TCG)
    let iterations = us * 1000;
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}

// --- AP Entry Point ---

/// Rust entry point for Application Processors.
/// Called from trampoline with cpu_index in RDI.
#[no_mangle]
pub extern "C" fn ap_entry_rust(cpu_index: u64) -> ! {
    // 1. Enable LAPIC on this AP
    let apic_virt = super::apic::lapic_virt_addr();
    unsafe {
        // Enable APIC + set spurious vector to 0xFF (bit 8 = APIC enable)
        let svr = read_volatile((apic_virt + 0xF0) as *const u32);
        write_volatile((apic_virt + 0xF0) as *mut u32, svr | 0x1FF);
        // TPR = 0 (accept all interrupts)
        write_volatile((apic_virt + 0x80) as *mut u32, 0);
        // Mask timer (APs don't need preemption)
        write_volatile((apic_virt + 0x320) as *mut u32, 0x10000);
    }

    // 2. Enable SSE/AVX via CR4
    unsafe {
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4);
        cr4 |= (1 << 9) | (1 << 10);  // OSFXSR + OSXMMEXCPT
        core::arch::asm!("mov cr4, {}", in(reg) cr4);
    }

    // 3. Set MXCSR to default (mask all FP exceptions)
    unsafe {
        let mxcsr: u32 = 0x1F80;
        core::arch::asm!("ldmxcsr [{}]", in(reg) &mxcsr, options(nostack));
    }

    // 4. Signal ready
    AP_READY_COUNT.fetch_add(1, Ordering::Release);

    // 5. Enter worker spin-loop
    ap_worker_loop(cpu_index as usize);
}

// --- Worker Loop ---

fn ap_worker_loop(cpu_index: usize) -> ! {
    loop {
        // Spin-wait for work (PAUSE instruction, WHPX-safe)
        while WORK_READY[cpu_index].load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }

        // Load task's page table for userspace memory access
        let cr3 = WORKER_CR3.load(Ordering::Acquire);
        if cr3 != 0 {
            unsafe {
                core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack));
            }
        }

        // Execute GEMM work
        let work = unsafe { &WORK_ITEMS[cpu_index] };
        unsafe { execute_gemm_work(work); }

        // Signal completion
        WORK_DONE[cpu_index].store(1, Ordering::Release);
        WORK_READY[cpu_index].store(0, Ordering::Release);
    }
}

// --- Parallel GEMM Dispatch (called from syscall handler) ---

/// Dispatch parallel GEMM across all available cores.
/// Returns 0 on success, -1 if no APs available.
pub fn dispatch_parallel_gemm(
    input_ptr: u64,
    weight_ptr: u64,
    output_ptr: u64,
    k: u32,
    n: u32,
    quant_type: u8,
    task_cr3: u64,
) -> i64 {
    let num_aps = ONLINE_AP_COUNT.load(Ordering::Relaxed);
    if num_aps == 0 {
        return -1; // No APs, fallback to sequential
    }

    let total_workers = num_aps + 1; // APs + BSP
    let n_usize = n as usize;
    let cols_per_worker = n_usize / total_workers;
    let remainder = n_usize % total_workers;

    // Set worker CR3
    WORKER_CR3.store(task_cr3, Ordering::Release);

    // Distribute columns to APs (workers 1..total_workers)
    let mut col = 0usize;
    // Worker 0 = BSP (handled below)
    let bsp_cols = cols_per_worker + if 0 < remainder { 1 } else { 0 };
    let bsp_col_start = col;
    col += bsp_cols;

    for i in 0..num_aps {
        let worker_idx = i + 1; // AP workers are 1-indexed
        let extra = if worker_idx < remainder { 1 } else { 0 };
        let my_cols = cols_per_worker + extra;

        unsafe {
            WORK_ITEMS[worker_idx] = GemmWork {
                input_ptr,
                weight_ptr,
                output_ptr,
                k,
                n,
                col_start: col as u32,
                col_end: (col + my_cols) as u32,
                quant_type,
            };
        }

        WORK_DONE[worker_idx].store(0, Ordering::Release);
        // This Release pairs with the Acquire in ap_worker_loop
        WORK_READY[worker_idx].store(1, Ordering::Release);

        col += my_cols;
    }

    // BSP does its share directly (already on the task's page table)
    let bsp_work = GemmWork {
        input_ptr,
        weight_ptr,
        output_ptr,
        k,
        n,
        col_start: bsp_col_start as u32,
        col_end: (bsp_col_start + bsp_cols) as u32,
        quant_type,
    };
    unsafe { execute_gemm_work(&bsp_work); }

    // Wait for all APs
    for i in 0..num_aps {
        let worker_idx = i + 1;
        while WORK_DONE[worker_idx].load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
    }

    0
}

// --- GEMM Execution (runs on each core) ---

// Q6_K constants (must match libtensor)
const Q6_K_BLOCK_SIZE: usize = 210;
const Q6_K_BLOCK_VALUES: usize = 256;

#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn execute_gemm_work(work: &GemmWork) {
    let k = work.k as usize;
    let n = work.n as usize;
    let col_start = work.col_start as usize;
    let col_end = work.col_end as usize;

    let n_blocks = k / Q6_K_BLOCK_VALUES;
    let q6k_row_bytes = n_blocks * Q6_K_BLOCK_SIZE;

    let a_f32 = core::slice::from_raw_parts(work.input_ptr as *const f32, k);
    let b_q6k = core::slice::from_raw_parts(work.weight_ptr as *const u8, n * q6k_row_bytes);
    let c = core::slice::from_raw_parts_mut(work.output_ptr as *mut f32, n);

    let mut dequant_buf = [0.0f32; Q6_K_BLOCK_VALUES];

    for col in col_start..col_end {
        let b_col_offset = col * q6k_row_bytes;
        let mut acc = 0.0f32;

        for blk in 0..n_blocks {
            let blk_start = b_col_offset + blk * Q6_K_BLOCK_SIZE;
            dequantize_q6_k_block(&b_q6k[blk_start..blk_start + Q6_K_BLOCK_SIZE], &mut dequant_buf);

            let a_base = blk * Q6_K_BLOCK_VALUES;
            acc += dot_f32_avx2(a_f32.as_ptr().add(a_base), dequant_buf.as_ptr(), 256);
        }

        c[col] = acc;
    }
}

/// AVX2 FMA dot product — processes 256 floats with 4-way unroll
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn dot_f32_avx2(a: *const f32, b: *const f32, count: usize) -> f32 {
    use core::arch::x86_64::*;
    let mut acc0 = _mm256_setzero_ps();
    let mut acc1 = _mm256_setzero_ps();
    let mut acc2 = _mm256_setzero_ps();
    let mut acc3 = _mm256_setzero_ps();
    let mut i = 0;
    while i + 32 <= count {
        acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i)),      _mm256_loadu_ps(b.add(i)),      acc0);
        acc1 = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i + 8)),  _mm256_loadu_ps(b.add(i + 8)),  acc1);
        acc2 = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i + 16)), _mm256_loadu_ps(b.add(i + 16)), acc2);
        acc3 = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i + 24)), _mm256_loadu_ps(b.add(i + 24)), acc3);
        i += 32;
    }
    while i + 8 <= count {
        acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i)), _mm256_loadu_ps(b.add(i)), acc0);
        i += 8;
    }
    acc0 = _mm256_add_ps(acc0, acc1);
    acc2 = _mm256_add_ps(acc2, acc3);
    acc0 = _mm256_add_ps(acc0, acc2);
    let hi = _mm256_extractf128_ps(acc0, 1);
    let lo = _mm256_castps256_ps128(acc0);
    let sum128 = _mm_add_ps(lo, hi);
    let shuf = _mm_movehdup_ps(sum128);
    let sums = _mm_add_ps(sum128, shuf);
    let shuf2 = _mm_movehl_ps(sums, sums);
    let final_sum = _mm_add_ss(sums, shuf2);
    _mm_cvtss_f32(final_sum)
}

/// Dequantize a Q6_K block (210 bytes → 256 f32 values)
/// Layout: 128 bytes QL (4-bit low) + 64 bytes QH (2-bit high) + 8 f16 scales + 8 i8 d values
fn dequantize_q6_k_block(block: &[u8], out: &mut [f32; 256]) {
    // Q6_K: 256 values in 210 bytes
    // Byte layout:
    //   [0..128]:   ql (low 4 bits, packed pairs)
    //   [128..192]: qh (high 2 bits, packed quads)
    //   [192..208]: scales (16 × i8)
    //   [208..210]: d (f16 super-block scale)
    let ql = &block[0..128];
    let qh = &block[128..192];
    let scales = &block[192..208];
    let d_bytes = [block[208], block[209]];
    let d = f16_to_f32(u16::from_le_bytes(d_bytes));

    for i in 0..256 {
        let ql_byte = ql[i / 2];
        let ql_val = if i % 2 == 0 { ql_byte & 0x0F } else { (ql_byte >> 4) & 0x0F };

        let qh_byte = qh[i / 4];
        let qh_shift = (i % 4) * 2;
        let qh_val = (qh_byte >> qh_shift) & 0x03;

        let q = ((qh_val as i8) << 4) | (ql_val as i8);
        let q_val = q as i32 - 32; // Q6_K uses offset 32

        let scale_idx = i / 16;
        let sc = scales[scale_idx] as i8;

        out[i] = d * (sc as f32) * (q_val as f32);
    }
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let mant = (h & 0x3FF) as u32;

    if exp == 0 {
        if mant == 0 {
            return f32::from_bits(sign << 31);
        }
        // Subnormal
        let mut m = mant;
        let mut e = 0i32;
        while m & 0x400 == 0 {
            m <<= 1;
            e -= 1;
        }
        m &= 0x3FF;
        let f_exp = ((127 - 15 + 1 + e) as u32) & 0xFF;
        return f32::from_bits((sign << 31) | (f_exp << 23) | (m << 13));
    }
    if exp == 31 {
        let f_mant = mant << 13;
        return f32::from_bits((sign << 31) | (0xFF << 23) | f_mant);
    }
    let f_exp = (exp + 127 - 15) & 0xFF;
    f32::from_bits((sign << 31) | (f_exp << 23) | (mant << 13))
}
