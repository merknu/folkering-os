//! Global Descriptor Table (GDT) and Task State Segment (TSS)
//!
//! Sets up x86-64 segmentation for kernel/user mode and syscall support.

use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;
use spin::Mutex;

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
        TSS.privilege_stack_table[0] = stack_end;

        // Build GDT with standard x86-64 layout
        // 0: Null, 1: Kernel CS (0x08), 2: Kernel DS (0x10), 3: User CS (0x18), 4: User DS (0x20), 5: TSS
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
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
        load_tss(GDT.as_ref().unwrap().1.tss);
    }
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
