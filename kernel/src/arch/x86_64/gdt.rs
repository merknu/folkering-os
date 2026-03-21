//! Global Descriptor Table (GDT) and Task State Segment (TSS)
//!
//! Sets up x86-64 segmentation for kernel/user mode and syscall support.

use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// Syscall stack size (16 KB)
const SYSCALL_STACK_SIZE: usize = 16 * 1024;

/// Syscall stack (used when transitioning from user to kernel mode)
static mut SYSCALL_STACK: [u8; SYSCALL_STACK_SIZE] = [0; SYSCALL_STACK_SIZE];

/// Task State Segment (static allocation)
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// Global Descriptor Table with all segments (static allocation)
static mut GDT: Option<(GlobalDescriptorTable, Selectors)> = None;

/// Segment selectors for all segments
struct Selectors {
    kernel_code: SegmentSelector,
    kernel_data: SegmentSelector,
    user_code: SegmentSelector,
    user_data: SegmentSelector,
    tss: SegmentSelector,
}

/// Initialize GDT and TSS
///
/// Must be called early during kernel initialization.
pub fn init() {
    use x86_64::instructions::segmentation::{CS, DS, Segment};
    use x86_64::instructions::tables::load_tss;

    unsafe {
        // Set TSS syscall stack pointer
        let stack_start = VirtAddr::from_ptr(&SYSCALL_STACK as *const _);
        let stack_end = stack_start + SYSCALL_STACK_SIZE as u64;

        // Debug: print TSS RSP0 value
        crate::drivers::serial::write_str("[GDT] TSS RSP0 = ");
        crate::drivers::serial::write_hex(stack_end.as_u64());
        crate::drivers::serial::write_newline();

        TSS.privilege_stack_table[0] = stack_end;

        // Build GDT layout compatible with SYSRET
        // Order: kernel_code (0x08), kernel_data (0x10), user_data (0x18), user_code (0x20)
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());  // 0x08
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());  // 0x10
        let user_data = gdt.append(Descriptor::user_data_segment());      // 0x18
        let user_code = gdt.append(Descriptor::user_code_segment());      // 0x20
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));

        let selectors = Selectors {
            kernel_code,
            kernel_data,
            user_code,
            user_data,
            tss: tss_selector,
        };

        GDT = Some((gdt, selectors));

        // Load GDT and configure segments
        GDT.as_ref().unwrap().0.load();
        CS::set_reg(GDT.as_ref().unwrap().1.kernel_code);
        DS::set_reg(GDT.as_ref().unwrap().1.kernel_data);

        // CRITICAL: Also set SS, ES, FS, GS to kernel data segment!
        // DS::set_reg only sets DS. Without this, SS remains whatever Limine set it to,
        // which causes #GP when timer interrupt tries to push to stack with invalid SS.
        let kernel_data_sel = GDT.as_ref().unwrap().1.kernel_data.0;
        core::arch::asm!(
            "mov ss, {0:x}",
            "mov es, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) kernel_data_sel,
            options(nostack, preserves_flags)
        );

        load_tss(GDT.as_ref().unwrap().1.tss);
    }

    crate::drivers::serial::write_str("[GDT] Done\n");
}

/// Get kernel code selector (for returning from interrupts)
pub fn kernel_code_selector() -> SegmentSelector {
    unsafe {
        GDT.as_ref().unwrap().1.kernel_code
    }
}

/// Get kernel data selector
pub fn kernel_data_selector() -> SegmentSelector {
    unsafe {
        GDT.as_ref().unwrap().1.kernel_data
    }
}

/// Get user code selector (for sysret)
pub fn user_code_selector() -> SegmentSelector {
    unsafe {
        GDT.as_ref().unwrap().1.user_code
    }
}

/// Get user data selector (for sysret)
pub fn user_data_selector() -> SegmentSelector {
    unsafe {
        GDT.as_ref().unwrap().1.user_data
    }
}
