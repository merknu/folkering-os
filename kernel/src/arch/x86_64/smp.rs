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
    /// Batch dimension. Input shape `[seq, k]`, output `[seq, n]`.
    /// seq=1 = decode, seq=14 = ChatML prefill.
    pub seq: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub quant_type: u8,
    /// 1 = activation row was pre-quantized into GLOBAL_QUANT_INPUT by
    /// BSP before dispatch (Q8 maddubs fast path, the common case).
    /// 0 = `input_ptr` is f32 and the worker must dequant the weights
    /// to f32 inline (Q8 dequant fallback, taken only when seq*k
    /// exceeds the 64 KiB pre-quant scratch). Ignored for non-Q8
    /// quant_types.
    pub pre_quantized: u8,
    /// Page table to load before executing this work. APs swap CR3
    /// to this so the userspace virtual addresses in the three ptr
    /// fields above resolve identically to how the BSP sees them —
    /// each access goes through the MMU per-page, so scattered
    /// shmem-backed weights (the 165 MiB Q8 model file is the
    /// motivating case) just work.
    pub task_cr3: u64,
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

    // Mask timer on APs — no preemption needed for compute workers
    write_volatile((apic_virt + 0x320) as *mut u32, 0x10000); // Masked

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

    // Capture this AP's boot-time CR3 — Limine set up a kernel-only
    // page table for the AP. We track `current_cr3` so we only emit
    // a `mov cr3, ...` when the target page table actually changes.
    // Writing the same value back into CR3 STILL flushes the TLB
    // (Intel SDM Vol. 3 §4.10.4.1: any write to CR3 invalidates
    // non-global TLB entries), so the previous unconditional
    // swap-out at end-of-job paid for a full TLB rebuild on every
    // single job — 196 jobs × 3 APs × 2 swaps × full rebuild was a
    // significant fraction of the 2.9 s/token wall-clock.
    //
    // With sticky CR3: AP swaps to the inference task's CR3 on the
    // first job and stays there for the rest of its life. TLB warms
    // up once per task, then weight reads stream through the L1/L2
    // dTLB at full bandwidth.
    let boot_cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) boot_cr3); }
    let mut current_cr3 = boot_cr3;

    loop {
        // PAUSE-based spin wait. Under WHPX each vCPU is a real thread —
        // PAUSE hints to the CPU that we're spin-waiting, reducing power.
        while WORK_READY[cpu_index].load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }

        // Swap to the task's page table so the userspace virtual
        // pointers in WORK_ITEMS resolve through the same MMU
        // mappings the BSP sees. Only swap if the target differs
        // from what's already loaded — otherwise we'd flush the TLB
        // for nothing.
        let work = unsafe { &WORK_ITEMS[cpu_index] };
        let target_cr3 = if work.task_cr3 != 0 { work.task_cr3 } else { boot_cr3 };
        unsafe {
            if target_cr3 != current_cr3 {
                core::arch::asm!("mov cr3, {}", in(reg) target_cr3);
                current_cr3 = target_cr3;
            }
            execute_gemm_work(work);
            // Stay in target_cr3 for the next job — same task means
            // same CR3 means warm TLB. If a different task ever
            // dispatches to this AP we'll swap on the next iteration.
        }

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
    seq: u32,
    quant_type: u8,
    task_cr3: u64,
) -> i64 {
    let num_aps = ONLINE_AP_COUNT.load(Ordering::Relaxed);
    if num_aps == 0 {
        return -1;
    }

    // APs now swap CR3 to the task's page table inside their work
    // loop, so userspace virtual pointers are valid as-is. No HHDM
    // pre-translation needed — the MMU handles per-page lookups,
    // which is the only thing that works for shmem-backed weights
    // whose physical pages are scattered across the heap.
    let n_usize = n as usize;

    // Skewed column split: BSP gets a smaller share than each AP
    // because BSP carries the dispatch coordination + input
    // quantization on top of its matmul work. Equal splits leave APs
    // idling at the barrier waiting for BSP. Empirical ratio for our
    // 4-vCPU layout (1 BSP + 3 APs) is 10/30 — BSP does ~10/100 of
    // total work, each AP does ~30/100. For other AP counts the
    // weights stay the same, denominator scales: total = 10 + 30*N.
    //
    // For num_aps=0 (uniprocessor fallback), BSP does everything
    // (handled below: bsp_cols falls out as n_usize).
    const BSP_WEIGHT: usize = 10;
    const AP_WEIGHT: usize = 30;
    let denom = BSP_WEIGHT + AP_WEIGHT * num_aps;
    let bsp_cols_target = if num_aps == 0 {
        n_usize
    } else {
        n_usize * BSP_WEIGHT / denom
    };
    let cols_per_ap = if num_aps == 0 {
        0
    } else {
        n_usize * AP_WEIGHT / denom
    };
    // Any leftover cols (integer-division remainder) go onto APs in
    // round-robin from worker 1 upward; BSP keeps its smaller share.
    let assigned = bsp_cols_target + cols_per_ap * num_aps;
    let leftover = n_usize - assigned;

    // BSP-side input quantization: walk the f32 activation row(s) once
    // here, fill GLOBAL_QUANT_INPUT, and signal workers via the
    // pre_quantized flag. All Q8 workers (BSP + APs) then read i8 lanes
    // from the shared buffer — no per-AP redundant quantization. APs
    // are still racing on weight loads + maddubs ALU, which is the
    // actual hot path.
    //
    // Falls back to dequant when seq*k > 64 KiB (unreachable on
    // Qwen3-0.6B today; worst case seq=14 × k=2816 ≈ 39 KiB).
    let pre_quantized: u8 = if quant_type == 0 {
        let seq_usize = (seq as usize).max(1);
        let k_usize = k as usize;
        if unsafe { quantize_input_global(input_ptr, seq_usize, k_usize) } {
            1
        } else {
            0
        }
    } else {
        0
    };

    let mut col = 0usize;
    let bsp_cols = bsp_cols_target;
    let bsp_col_start = col;
    col += bsp_cols;

    for i in 0..num_aps {
        let worker_idx = i + 1;
        // Distribute the integer-division remainder across the first
        // `leftover` APs (each gets one extra column). BSP keeps its
        // smaller deterministic share regardless.
        let extra = if i < leftover { 1 } else { 0 };
        let my_cols = cols_per_ap + extra;

        unsafe {
            WORK_ITEMS[worker_idx] = GemmWork {
                input_ptr,    // userspace virt — AP swaps CR3
                weight_ptr,
                output_ptr,
                k, n, seq,
                col_start: col as u32,
                col_end: (col + my_cols) as u32,
                quant_type,
                pre_quantized,
                task_cr3,
            };
        }

        WORK_DONE[worker_idx].store(0, Ordering::Release);
        WORK_READY[worker_idx].store(1, Ordering::Release);
        col += my_cols;
    }

    // BSP does its share. BSP is already running in task's CR3
    // (we're inside a syscall on this task), so userspace virt
    // pointers resolve through the MMU per-page just like the APs.
    let bsp_work = GemmWork {
        input_ptr, weight_ptr, output_ptr,
        k, n, seq,
        col_start: bsp_col_start as u32,
        col_end: (bsp_col_start + bsp_cols) as u32,
        quant_type,
        pre_quantized,
        task_cr3: 0, // BSP doesn't swap; it's already in task_cr3
    };

    unsafe { execute_gemm_work(&bsp_work); }

    // Wait for APs (with timeout). Quiet path; only TIMEOUT errors
    // print, since they signal a real bug. The per-step PGEMM noise
    // (entry / cols / done) was useful while debugging the ABI bug
    // in #175 but is pure overhead now — at SMP_DISPATCH_MIN_OUT_DIM
    // = 1024 we dispatch ~196 PGEMMs per token across 28 layers,
    // each writing five lines is ~1 KB serial per token = real
    // throughput drag.
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
        }
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

// Q8 block layout: `[scale_lo: f16][scale_hi: f16][q[0..32]: i8]` =
// 36 bytes. scale_lo applies to q[0..16], scale_hi applies to
// q[16..32]. Same convention as `userspace::inference::tensor_math`.
// (Diverges from llama.cpp's 34-byte single-scale Q8_0; we made the
// trade for ~50% less Q8 noise at +6% file size, which mattered for
// 36-layer Qwen3-4B argmax stability.)
const Q8_0_BLOCK_SIZE: usize = 36;
const Q8_0_BLOCK_VALUES: usize = 32;
const Q8_0_HALF: usize = 16;

unsafe fn execute_gemm_work(work: &GemmWork) {
    // quant_type: 0 = Q8_0, 1 = Q6_K. Q8_0 is the format the
    // inference task uses end-to-end (PRs #166/#168/#170); Q6_K is
    // legacy from libtensor / inference-server. AVX2 path required
    // for both — we already gate AVX2 enablement at boot (#165 +
    // smp.rs CR4 setup), so no fallback below.
    match work.quant_type {
        0 => {
            // Q8_0 path: when BSP pre-quantized the input row into
            // GLOBAL_QUANT_INPUT (the common case), all workers run
            // the int8 maddubs kernel reading from that shared buffer.
            // Otherwise (seq*k overflow, very rare), fall back to the
            // dequant-then-FMA kernel that reads the f32 input directly.
            if work.pre_quantized != 0 {
                execute_gemm_work_q8_maddubs(work);
            } else {
                execute_gemm_work_q8_avx2_dequant(work);
            }
        }
        _ => {
            if has_avx2() {
                execute_gemm_work_avx2(work);
            } else {
                execute_gemm_work_scalar(work);
            }
        }
    }
}

/// Single shared scratch for BSP-hoisted f32 → i8 input quantization.
/// PR #182 had each AP redundantly walking the activation row to quantize
/// it locally — for a 4-AP dispatch over a single matmul that's 4×
/// duplicated work on the same bytes. With the input row pre-quantized
/// once on BSP into this buffer, APs just read i8 lanes via maddubs, no
/// per-AP quantization in the hot path.
///
/// 64 KiB is enough for the worst real case on Qwen3-0.6B (seq=14
/// prefill × k=2816 mlp_down ≈ 39 KiB). MAX_SEQ scales array (256 B)
/// holds per-row max-abs scale factors.
///
/// Single-static is safe because `dispatch_parallel_gemm` synchronously
/// waits for all APs before returning — there's never more than one
/// parallel-Q8 matmul in flight on this kernel.
const QUANT_BUF_BYTES: usize = 65536;
#[repr(C, align(32))]
struct QuantBuf {
    input: [i8; QUANT_BUF_BYTES],
    scales: [f32; MAX_SEQ],
}
static mut GLOBAL_QUANT_INPUT: QuantBuf = unsafe { core::mem::zeroed() };

/// BSP-side: pre-quantize the activation rows once into GLOBAL_QUANT_INPUT.
/// Returns true if the input fit; false signals the caller to fall back
/// to the dequant kernel (which reads the f32 input directly).
///
/// Must be called while in the inference task's CR3 — `input_ptr` is a
/// userspace virtual address and resolves through the task page table.
/// `dispatch_parallel_gemm` is invoked from inside the parallel-GEMM
/// syscall, so BSP is already in the right page table.
#[target_feature(enable = "avx2")]
unsafe fn quantize_input_global(input_ptr: u64, seq: usize, k: usize) -> bool {
    if seq * k > QUANT_BUF_BYTES {
        return false;
    }
    let a_f32 = core::slice::from_raw_parts(input_ptr as *const f32, seq * k);
    let buf = &mut *core::ptr::addr_of_mut!(GLOBAL_QUANT_INPUT);
    for s in 0..seq {
        let row = &a_f32[s * k..s * k + k];
        let mut max_abs = 0.0f32;
        for &v in row {
            let av = if v < 0.0 { -v } else { v };
            if av > max_abs { max_abs = av; }
        }
        let scale = if max_abs > 0.0 { max_abs / 127.0 } else { 1.0 };
        let inv_scale = 1.0 / scale;
        buf.scales[s] = scale;
        let dst = &mut buf.input[s * k..s * k + k];
        for i in 0..k {
            let q = (row[i] * inv_scale) as i32;
            dst[i] = if q > 127 { 127 } else if q < -127 { -127 } else { q as i8 };
        }
    }
    true
}

/// Q8_0 batched GEMM column-strip executor for one AP. Caller
/// provides:
///   - input  shape `[seq, k]`   row-major f32
///   - weights shape `[n, k]`    row-major Q8_0 (34 B per 32-elem block)
///   - output shape `[seq, n]`   row-major f32
/// We compute `c[s, col] = sum_i (a[s, i] * dequant(B[col, i]))` for
/// each `col` in `[col_start, col_end)` and each `s` in `[0, seq)`.
///
/// Per-block dequant is reused across all `seq` rows: the weight
/// row's bytes are loaded + sign-extended + scaled ONCE per (col,
/// block), then 4 FMAs are issued per `s` within that block. This
/// amortises ~the entire dequant cost across the batch — the
/// motivating case is prefill at seq = 14 where naive per-row
/// dispatch did 14× redundant dequant work.
///
/// Per-s accumulators live on the stack (a fixed-size array of
/// `__m256` vectors). MAX_SEQ = 64 is comfortably larger than
/// the ChatML prefill (seq = 14) and uses 64 × 32 = 2 KiB of
/// stack per executor — well within budget.
const MAX_SEQ: usize = 64;

/// Q8_0 batched GEMM via int8 multiply-accumulate (`_mm256_maddubs_epi16`),
/// reading the pre-quantized activation row from GLOBAL_QUANT_INPUT.
///
/// Two-signed-i8 dot product via the standard sign-fold trick:
///   `|w|` as the unsigned operand, `x · sign(w)` as the signed operand,
/// then `maddubs(|w|, x·sign(w)) = w·x` pairwise summed into i16 lanes.
/// Q8_0 quantizer clamps weights to [-127, +127] (skipping -128) so the
/// `sign_epi8(-128, -128)` overflow case never fires.
///
/// 32 i8 lanes per maddubs instruction vs 8 f32 lanes per FMA — and the
/// 4× input-bandwidth saving (32 B i8 vs 128 B f32 per block) matters
/// just as much as the lane count on a TLB-warmed shmem-backed weight
/// stream. With BSP-side input quantization (this PR), the per-AP
/// quantization redundancy from PR #182 is gone — APs walk only the
/// weight stream and the shared int8 input.
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn execute_gemm_work_q8_maddubs(work: &GemmWork) {
    use core::arch::x86_64::*;
    let k = work.k as usize;
    let n = work.n as usize;
    let seq = (work.seq as usize).max(1).min(MAX_SEQ);
    let col_start = work.col_start as usize;
    let col_end = work.col_end as usize;
    let n_blocks = k / Q8_0_BLOCK_VALUES;
    let row_bytes = n_blocks * Q8_0_BLOCK_SIZE;

    let b_q8 = core::slice::from_raw_parts(work.weight_ptr as *const u8, n * row_bytes);
    let c = core::slice::from_raw_parts_mut(work.output_ptr as *mut f32, seq * n);

    // Read the pre-quantized input that BSP filled before dispatching us.
    // Single static, safe because dispatch_parallel_gemm waits for all
    // APs before returning — never more than one matmul in flight.
    let buf = &*core::ptr::addr_of!(GLOBAL_QUANT_INPUT);
    let q_input = &buf.input[..seq * k];
    let row_scales = &buf.scales[..seq];

    let ones16 = _mm256_set1_epi16(1);
    let mut acc: [__m256; MAX_SEQ] = [_mm256_setzero_ps(); MAX_SEQ];

    for col in col_start..col_end {
        let row_off = col * row_bytes;
        for s in 0..seq {
            acc[s] = _mm256_setzero_ps();
        }

        for b in 0..n_blocks {
            let block_off = row_off + b * Q8_0_BLOCK_SIZE;
            // Two block scales: lo for q[0..16], hi for q[16..32].
            // Build a __m256 with [lo,lo,lo,lo,hi,hi,hi,hi]: after
            // maddubs+madd, the 8 i32 lanes correspond to
            //   lanes 0..4 → low 16 bytes of the block (q[0..16])
            //   lanes 4..8 → high 16 bytes (q[16..32])
            // so blending the two scales at lane-4 boundary applies
            // each scale to the correct half. Verified by walking the
            // `_mm256_maddubs_epi16` semantics: low/high 128-bit
            // halves operate independently, and `_mm256_madd_epi16`
            // sums adjacent i16 pairs without crossing the 128-bit
            // boundary, so the lane→half mapping is preserved.
            let scale_lo = f16_to_f32(u16::from_le_bytes([
                b_q8[block_off],
                b_q8[block_off + 1],
            ]));
            let scale_hi = f16_to_f32(u16::from_le_bytes([
                b_q8[block_off + 2],
                b_q8[block_off + 3],
            ]));
            let scale_lo_v = _mm256_set1_ps(scale_lo);
            let scale_hi_v = _mm256_set1_ps(scale_hi);
            let scale_split = _mm256_blend_ps(scale_lo_v, scale_hi_v, 0xF0);

            // Load 32 i8 weights, derive |w| once per (col, block) —
            // reused across all `seq` rows. `xs_signed` depends on
            // sign(w) too, so we re-fold per s.
            let w = _mm256_loadu_si256(
                b_q8.as_ptr().add(block_off + 4) as *const __m256i,
            );
            let w_abs = _mm256_sign_epi8(w, w);

            let a_base = b * Q8_0_BLOCK_VALUES;
            for s in 0..seq {
                let xs = _mm256_loadu_si256(
                    q_input.as_ptr().add(s * k + a_base) as *const __m256i,
                );
                // sign(w) folded onto x: maddubs(|w|, x·sign(w)) = w·x.
                let xs_signed = _mm256_sign_epi8(xs, w);
                // 32 × i8·i8 → 16 × i16 (pairwise sums; saturation
                // can't fire here: |w·x| ≤ 127² = 16129, so
                // |sum_pair| ≤ 32258 < 32767).
                let prod16 = _mm256_maddubs_epi16(w_abs, xs_signed);
                // 16 × i16 → 8 × i32 (multiply by 1, sum adjacent).
                let prod32 = _mm256_madd_epi16(prod16, ones16);
                // Convert to f32 and FMA into the row's accumulator,
                // scaled by per-half block scale. Per-row input scale
                // is applied once at the end of (col, s).
                let prod_f32 = _mm256_cvtepi32_ps(prod32);
                acc[s] = _mm256_fmadd_ps(prod_f32, scale_split, acc[s]);
            }
        }

        for s in 0..seq {
            let v = acc[s];
            let lo = _mm256_castps256_ps128(v);
            let hi = _mm256_extractf128_ps(v, 1);
            let s4 = _mm_add_ps(lo, hi);
            let s4_hi = _mm_movehdup_ps(s4);
            let s2 = _mm_add_ps(s4, s4_hi);
            let s2_hi = _mm_movehl_ps(s4_hi, s2);
            let s1 = _mm_add_ss(s2, s2_hi);
            c[s * n + col] = _mm_cvtss_f32(s1) * row_scales[s];
        }
    }
}

/// Original dequant-to-f32 + FMA path. Kept as a fallback for the
/// (currently unreachable) case where `seq * k > QUANT_BUF_BYTES`.
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn execute_gemm_work_q8_avx2_dequant(work: &GemmWork) {
    use core::arch::x86_64::*;
    let k = work.k as usize;
    let n = work.n as usize;
    let seq = (work.seq as usize).max(1).min(MAX_SEQ);
    let col_start = work.col_start as usize;
    let col_end = work.col_end as usize;
    let n_blocks = k / Q8_0_BLOCK_VALUES;
    let row_bytes = n_blocks * Q8_0_BLOCK_SIZE;

    let a_f32 = core::slice::from_raw_parts(work.input_ptr as *const f32, seq * k);
    let b_q8 = core::slice::from_raw_parts(work.weight_ptr as *const u8, n * row_bytes);
    let c = core::slice::from_raw_parts_mut(work.output_ptr as *mut f32, seq * n);

    let mut acc: [__m256; MAX_SEQ] = [_mm256_setzero_ps(); MAX_SEQ];

    for col in col_start..col_end {
        let row_off = col * row_bytes;
        for s in 0..seq {
            acc[s] = _mm256_setzero_ps();
        }

        for b in 0..n_blocks {
            let block_off = row_off + b * Q8_0_BLOCK_SIZE;
            // Two scales: lo for q[0..16] (raw_lo), hi for q[16..32]
            // (raw_hi). Same layout/semantics as the maddubs path
            // and userspace `matmul_batch_q8_avx2`.
            let scale_lo = f16_to_f32(u16::from_le_bytes([
                b_q8[block_off],
                b_q8[block_off + 1],
            ]));
            let scale_hi = f16_to_f32(u16::from_le_bytes([
                b_q8[block_off + 2],
                b_q8[block_off + 3],
            ]));
            let scale_lo_v = _mm256_set1_ps(scale_lo);
            let scale_hi_v = _mm256_set1_ps(scale_hi);

            let q_ptr = b_q8.as_ptr().add(block_off + 4) as *const __m128i;
            let raw_lo = _mm_loadu_si128(q_ptr);
            let raw_hi = _mm_loadu_si128(q_ptr.add(1));
            let deq0 = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(raw_lo)), scale_lo_v);
            let deq1 = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_srli_si128(raw_lo, 8))), scale_lo_v);
            let deq2 = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(raw_hi)), scale_hi_v);
            let deq3 = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_srli_si128(raw_hi, 8))), scale_hi_v);

            let a_base = b * Q8_0_BLOCK_VALUES;
            for s in 0..seq {
                let xs_ptr = a_f32.as_ptr().add(s * k + a_base);
                let xs0 = _mm256_loadu_ps(xs_ptr);
                let xs1 = _mm256_loadu_ps(xs_ptr.add(8));
                let xs2 = _mm256_loadu_ps(xs_ptr.add(16));
                let xs3 = _mm256_loadu_ps(xs_ptr.add(24));
                acc[s] = _mm256_fmadd_ps(deq0, xs0, acc[s]);
                acc[s] = _mm256_fmadd_ps(deq1, xs1, acc[s]);
                acc[s] = _mm256_fmadd_ps(deq2, xs2, acc[s]);
                acc[s] = _mm256_fmadd_ps(deq3, xs3, acc[s]);
            }
        }

        for s in 0..seq {
            let v = acc[s];
            let lo = _mm256_castps256_ps128(v);
            let hi = _mm256_extractf128_ps(v, 1);
            let s4 = _mm_add_ps(lo, hi);
            let s4_hi = _mm_movehdup_ps(s4);
            let s2 = _mm_add_ps(s4, s4_hi);
            let s2_hi = _mm_movehl_ps(s4_hi, s2);
            let s1 = _mm_add_ss(s2, s2_hi);
            c[s * n + col] = _mm_cvtss_f32(s1);
        }
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
