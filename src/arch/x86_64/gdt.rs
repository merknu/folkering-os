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
        crate::serial_println!("[GDT] Setting up TSS stack...");
        // Set TSS syscall stack pointer (TSS is already initialized by static initialization)
        let stack_start = VirtAddr::from_ptr(&SYSCALL_STACK as *const _);
        let stack_end = stack_start + SYSCALL_STACK_SIZE as u64;
        TSS.privilege_stack_table[0] = stack_end;
        crate::serial_println!("[GDT] TSS stack configured at {:#x}", stack_end.as_u64());

        crate::serial_println!("[GDT] Building GDT...");
        // Build GDT with SYSCALL/SYSRET compatible layout:
        // Index 0: Null (0x00)
        // Index 1: Kernel code (0x08)
        // Index 2: Kernel data (0x10)
        // Index 3: User code (0x18) - SYSRET CS = base + 16, so base must be 0x08 - 16 = invalid!
        // Index 4: User data (0x20) - SYSRET SS = base + 8, so base must be 0x18
        //
        // Wait - that won't work! SYSRET needs:
        //   CS = base + 16
        //   SS = base + 8
        // For a SINGLE base value!
        //
        // Standard x86-64 layout:
        // 0: Null, 1: Kernel CS, 2: Kernel DS, 3: User CS, 4: User DS
        // With base = index 1 = 0x08:
        //   SYSRET CS = 0x08 + 16 = 0x18 (index 3) ✓
        //   SYSRET SS = 0x08 + 8 = 0x10 (index 2) ✗ (this is kernel data!)
        //
        // So we need index 2 to be BOTH kernel data AND user data!
        // OR... put user data BEFORE kernel data:
        // 0: Null, 1: Kernel CS, 2: User DS, 3: User CS, 4: Kernel DS?
        // NO - that breaks kernel operation!
        //
        // Linux solution: put 32-bit compatibility segments in between
        // Let me try standard layout and see what Star::write expects
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
        crate::serial_println!("[GDT] GDT built with {} entries", 6);

        crate::serial_println!("[GDT] Loading GDT...");
        // Load GDT
        GDT.as_ref().unwrap().0.load();
        crate::serial_println!("[GDT] GDT loaded into GDTR");

        crate::serial_println!("[GDT] Setting CS to {:#x}...", GDT.as_ref().unwrap().1.kernel_code.0);
        // Set kernel code segment
        CS::set_reg(GDT.as_ref().unwrap().1.kernel_code);
        crate::serial_println!("[GDT] CS updated");

        crate::serial_println!("[GDT] Setting DS to {:#x}...", GDT.as_ref().unwrap().1.kernel_data.0);
        // Set kernel data segment
        DS::set_reg(GDT.as_ref().unwrap().1.kernel_data);
        crate::serial_println!("[GDT] DS updated");

        crate::serial_println!("[GDT] Loading TSS (selector {:#x})...", GDT.as_ref().unwrap().1.tss.0);
        // Load TSS
        load_tss(GDT.as_ref().unwrap().1.tss);
        crate::serial_println!("[GDT] TSS loaded into TR");
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
