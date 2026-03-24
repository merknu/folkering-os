//! SMP — Symmetric Multi-Processing via Limine SMP Protocol
//!
//! Uses Limine's built-in AP boot mechanism (goto_address) instead of manual
//! INIT-SIPI-SIPI. APs run as kernel-mode compute workers for parallel GEMM.

use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use core::ptr::{read_volatile, write_volatile};

/// Maximum CPUs supported
pub const MAX_CPUS: usize = 16;

/// Number of APs that have completed initialization
pub static AP_READY_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Number of usable AP workers
static ONLINE_AP_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn ap_count() -> usize {
    ONLINE_AP_COUNT.load(Ordering::Relaxed)
}

// --- Work Distribution ---

#[repr(C)]
pub struct GemmWork {
    pub input_ptr: u64,
    pub weight_ptr: u64,
    pub output_ptr: u64,
    pub k: u32,
    pub n: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub quant_type: u8,
}

static mut WORK_ITEMS: [GemmWork; MAX_CPUS] = unsafe { core::mem::zeroed() };

static WORK_READY: [AtomicU8; MAX_CPUS] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_CPUS]
};
static WORK_DONE: [AtomicU8; MAX_CPUS] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_CPUS]
};

// No WORKER_CR3 — APs keep Limine's page table. Pointers are HHDM-translated.

// --- Limine SMP Boot ---

/// Boot APs using Limine's SMP response.
/// Limine has already done INIT-SIPI-SIPI, set up 64-bit mode, GDT, IDT,
/// and allocated a 64KB stack per AP. We just write goto_address.
pub fn boot_aps_limine(smp: &limine::response::SmpResponse) {
    let cpus = smp.cpus();
    let bsp_lapic = smp.bsp_lapic_id();
    let mut ap_index = 0usize;

    for cpu in cpus.iter() {
        if cpu.lapic_id == bsp_lapic {
            continue; // Skip BSP
        }

        if ap_index >= MAX_CPUS - 1 {
            break;
        }

        // Store cpu_index in the `extra` field so AP knows its worker ID
        cpu.extra.store((ap_index + 1) as u64, Ordering::SeqCst);

        crate::serial_str!("[SMP] Starting AP ");
        crate::drivers::serial::write_dec(cpu.lapic_id);
        crate::serial_str!(" as worker ");
        crate::drivers::serial::write_dec((ap_index + 1) as u32);
        crate::drivers::serial::write_newline();

        // Write goto_address — Limine immediately jumps the AP to our function
        cpu.goto_address.write(ap_entry_limine);

        ap_index += 1;
    }

    // Wait for all APs to signal ready (with timeout)
    let expected = ap_index;
    for _ in 0..100_000_000u64 {
        if AP_READY_COUNT.load(Ordering::Acquire) >= expected {
            break;
        }
        core::hint::spin_loop();
    }

    let ready = AP_READY_COUNT.load(Ordering::Relaxed);
    ONLINE_AP_COUNT.store(ready, Ordering::Relaxed);

    crate::serial_str!("[SMP] ");
    crate::drivers::serial::write_dec(ready as u32);
    crate::serial_str!(" APs online as compute workers\n");
}

/// AP entry point called by Limine.
/// The AP is already in 64-bit mode with its own stack, GDT, and IDT.
unsafe extern "C" fn ap_entry_limine(cpu: &limine::mp::Cpu) -> ! {
    let cpu_index = cpu.extra.load(Ordering::SeqCst) as usize;

    // Enable LAPIC on this AP
    let apic_virt = super::apic::lapic_virt_addr();
    write_volatile((apic_virt + 0xF0) as *mut u32,
        read_volatile((apic_virt + 0xF0) as *const u32) | 0x1FF);
    write_volatile((apic_virt + 0x80) as *mut u32, 0);     // TPR = 0

    // Enable timer on AP (1ms interval) to wake from HLT
    // Use vector 48 (0x30) — NOT 32 which is BSP's preemption timer!
    // Vector 48 handler just sends EOI, no task preemption
    write_volatile((apic_virt + 0x3E0) as *mut u32, 0x3);  // Divide by 16
    write_volatile((apic_virt + 0x320) as *mut u32, 0x20000 | 48); // Periodic, vector 48
    write_volatile((apic_virt + 0x380) as *mut u32, 62500); // ~1ms at 1GHz/16

    // Enable SSE + AVX via CR4
    let mut cr4: u64;
    core::arch::asm!("mov {}, cr4", out(reg) cr4);
    cr4 |= (1 << 9)   // OSFXSR — enable FXSAVE/FXRSTOR
         | (1 << 10)   // OSXMMEXCPT — enable #XM for SIMD exceptions
         | (1 << 18);  // OSXSAVE — enable XSAVE/XGETBV (required for AVX)
    core::arch::asm!("mov cr4, {}", in(reg) cr4);

    // Enable AVX state in XCR0 (only if OSXSAVE worked)
    // XCR0 bit 0 = x87, bit 1 = SSE, bit 2 = AVX
    core::arch::asm!(
        "xor ecx, ecx",  // XCR0
        "xgetbv",
        "or eax, 7",     // Enable x87 + SSE + AVX
        "xsetbv",
        out("eax") _,
        out("ecx") _,
        out("edx") _,
    );

    // Set MXCSR
    let mxcsr: u32 = 0x1F80;
    core::arch::asm!("ldmxcsr [{}]", in(reg) &mxcsr, options(nostack));

    // Signal ready
    AP_READY_COUNT.fetch_add(1, Ordering::Release);

    // Enter worker spin-loop
    ap_worker_loop(cpu_index);
}

// --- Worker Loop ---

fn ap_worker_loop(cpu_index: usize) -> ! {
    crate::serial_str!("[AP");
    crate::drivers::serial::write_dec(cpu_index as u32);
    crate::serial_str!("] Worker loop entered\n");

    // Enable interrupts so APIC timer can wake us from HLT
    unsafe { core::arch::asm!("sti"); }

    loop {
        // HLT-based wait: timer wakes us every ~1ms, we check flag, then HLT again
        while WORK_READY[cpu_index].load(Ordering::Acquire) == 0 {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
        }

        // NO CR3 swap! Pointers in WORK_ITEMS are already HHDM-translated
        // by the BSP in dispatch_parallel_gemm.

        let work = unsafe { &WORK_ITEMS[cpu_index] };
        unsafe { execute_gemm_work(work); }

        crate::serial_str!("[AP");
        crate::drivers::serial::write_dec(cpu_index as u32);
        crate::serial_str!("] Work done\n");

        WORK_DONE[cpu_index].store(1, Ordering::Release);
        WORK_READY[cpu_index].store(0, Ordering::Release);
    }
}

// --- Parallel GEMM Dispatch ---

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
        return -1;
    }

    // Translate userspace pointers to HHDM addresses so APs can access them
    // without switching CR3. BSP walks the task's page table to find physical
    // addresses, then adds HHDM offset.
    let hhdm = crate::memory::paging::hhdm_offset() as u64;
    let input_hhdm = virt_to_hhdm(task_cr3, input_ptr, hhdm);
    let weight_hhdm = virt_to_hhdm(task_cr3, weight_ptr, hhdm);
    let output_hhdm = virt_to_hhdm(task_cr3, output_ptr, hhdm);

    if input_hhdm == 0 || weight_hhdm == 0 || output_hhdm == 0 {
        crate::serial_str!("[PGEMM] HHDM translation failed!\n");
        return -1;
    }

    let total_workers = num_aps + 1;
    let n_usize = n as usize;
    let cols_per_worker = n_usize / total_workers;
    let remainder = n_usize % total_workers;

    let mut col = 0usize;
    let bsp_cols = cols_per_worker + if 0 < remainder { 1 } else { 0 };
    let bsp_col_start = col;
    col += bsp_cols;

    for i in 0..num_aps {
        let worker_idx = i + 1;
        let extra = if worker_idx < remainder { 1 } else { 0 };
        let my_cols = cols_per_worker + extra;

        unsafe {
            WORK_ITEMS[worker_idx] = GemmWork {
                input_ptr: input_hhdm,   // HHDM-translated for APs
                weight_ptr: weight_hhdm,
                output_ptr: output_hhdm,
                k, n,
                col_start: col as u32,
                col_end: (col + my_cols) as u32,
                quant_type,
            };
        }

        WORK_DONE[worker_idx].store(0, Ordering::Release);
        WORK_READY[worker_idx].store(1, Ordering::Release);
        col += my_cols;
    }

    // BSP does its share
    let bsp_work = GemmWork {
        input_ptr, weight_ptr, output_ptr,
        k, n,
        col_start: bsp_col_start as u32,
        col_end: (bsp_col_start + bsp_cols) as u32,
        quant_type,
    };

    crate::serial_str!("[PGEMM] BSP cols ");
    crate::drivers::serial::write_dec(bsp_col_start as u32);
    crate::serial_str!("-");
    crate::drivers::serial::write_dec((bsp_col_start + bsp_cols) as u32);
    crate::serial_str!(" AVX2=");
    crate::drivers::serial::write_dec(if has_avx2() { 1 } else { 0 });
    crate::drivers::serial::write_newline();

    unsafe { execute_gemm_work(&bsp_work); }

    crate::serial_str!("[PGEMM] BSP done, waiting APs...\n");

    // Wait for APs (with timeout)
    let mut all_done = true;
    for i in 0..num_aps {
        let mut done = false;
        for _ in 0..500_000_000u64 {
            if WORK_DONE[i + 1].load(Ordering::Acquire) != 0 {
                done = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !done {
            crate::serial_str!("[PGEMM] AP ");
            crate::drivers::serial::write_dec((i + 1) as u32);
            crate::serial_str!(" TIMEOUT\n");
            all_done = false;
        }
    }

    if all_done {
        crate::serial_str!("[PGEMM] All workers done\n");
    }

    0
}

// --- Page Table Walk: userspace virt → HHDM virt ---

/// Walk a 4-level page table to translate a userspace virtual address to
/// an HHDM virtual address. Returns 0 on failure (unmapped).
fn virt_to_hhdm(cr3: u64, virt: u64, hhdm: u64) -> u64 {
    let pml4_phys = cr3 & !0xFFF;
    let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
    let pd_idx = ((virt >> 21) & 0x1FF) as usize;
    let pt_idx = ((virt >> 12) & 0x1FF) as usize;
    let page_off = (virt & 0xFFF) as usize;

    unsafe {
        // PML4 → PDPT
        let pml4 = (hhdm + pml4_phys) as *const u64;
        let pml4e = *pml4.add(pml4_idx);
        if pml4e & 1 == 0 { return 0; } // Not present

        // PDPT → PD (or 1GB hugepage)
        let pdpt_phys = pml4e & 0x000F_FFFF_FFFF_F000;
        let pdpt = (hhdm + pdpt_phys) as *const u64;
        let pdpte = *pdpt.add(pdpt_idx);
        if pdpte & 1 == 0 { return 0; }
        if pdpte & (1 << 7) != 0 {
            // 1GB hugepage
            let page_phys = pdpte & 0x000F_FFFF_C000_0000;
            return hhdm + page_phys + (virt & 0x3FFF_FFFF);
        }

        // PD → PT (or 2MB hugepage)
        let pd_phys = pdpte & 0x000F_FFFF_FFFF_F000;
        let pd = (hhdm + pd_phys) as *const u64;
        let pde = *pd.add(pd_idx);
        if pde & 1 == 0 { return 0; }
        if pde & (1 << 7) != 0 {
            // 2MB hugepage
            let page_phys = pde & 0x000F_FFFF_FFE0_0000;
            return hhdm + page_phys + (virt & 0x1F_FFFF);
        }

        // PT → Physical page
        let pt_phys = pde & 0x000F_FFFF_FFFF_F000;
        let pt = (hhdm + pt_phys) as *const u64;
        let pte = *pt.add(pt_idx);
        if pte & 1 == 0 { return 0; }
        let page_phys = pte & 0x000F_FFFF_FFFF_F000;
        hhdm + page_phys + page_off as u64
    }
}

// --- GEMM Execution ---

const Q6_K_BLOCK_SIZE: usize = 210;
const Q6_K_BLOCK_VALUES: usize = 256;

/// Check if AVX2 is supported via CPUID
fn has_avx2() -> bool {
    let result: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) result,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
        );
    }
    result & (1 << 5) != 0
}

unsafe fn execute_gemm_work(work: &GemmWork) {
    if has_avx2() {
        execute_gemm_work_avx2(work);
    } else {
        execute_gemm_work_scalar(work);
    }
}

#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn execute_gemm_work_avx2(work: &GemmWork) {
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

/// Scalar GEMM fallback (no AVX2)
unsafe fn execute_gemm_work_scalar(work: &GemmWork) {
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
            for i in 0..Q6_K_BLOCK_VALUES {
                acc += a_f32[a_base + i] * dequant_buf[i];
            }
        }
        c[col] = acc;
    }
}

fn dequantize_q6_k_block(block: &[u8], out: &mut [f32; 256]) {
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
        let q_val = q as i32 - 32;
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
        if mant == 0 { return f32::from_bits(sign << 31); }
        let mut m = mant;
        let mut e = 0i32;
        while m & 0x400 == 0 { m <<= 1; e -= 1; }
        m &= 0x3FF;
        let f_exp = ((127 - 15 + 1 + e) as u32) & 0xFF;
        return f32::from_bits((sign << 31) | (f_exp << 23) | (m << 13));
    }
    if exp == 31 { return f32::from_bits((sign << 31) | (0xFF << 23) | (mant << 13)); }
    let f_exp = (exp + 127 - 15) & 0xFF;
    f32::from_bits((sign << 31) | (f_exp << 23) | (mant << 13))
}
