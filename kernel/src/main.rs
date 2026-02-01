//! Folkering OS Kernel Entry Point

#![no_std]
#![no_main]

use limine::BaseRevision;
use limine::request::{
    RequestsStartMarker, RequestsEndMarker,
    FramebufferRequest, MemoryMapRequest, HhdmRequest, RsdpRequest, ModuleRequest
};

// Import kernel library
extern crate folkering_kernel;

// Limine base revision
#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();

// Request framebuffer
#[used]
#[link_section = ".requests"]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

// Request memory map
#[used]
#[link_section = ".requests"]
static MEMORY_MAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

// Request Higher Half Direct Map
#[used]
#[link_section = ".requests"]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

// Request RSDP (ACPI root table)
#[used]
#[link_section = ".requests"]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

// Request boot modules (initrd)
#[used]
#[link_section = ".requests"]
static MODULE_REQUEST: ModuleRequest = ModuleRequest::new();

// Request markers
#[used]
#[link_section = ".requests_start_marker"]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".requests_end_marker"]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

// Framebuffer info statics (populated from Limine response, read by kernel_main)
use core::sync::atomic::AtomicUsize;
static FRAMEBUFFER_INFO: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_WIDTH: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_HEIGHT: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_PITCH: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_BPP: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_RED_SHIFT: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_GREEN_SHIFT: AtomicUsize = AtomicUsize::new(0);
static FRAMEBUFFER_BLUE_SHIFT: AtomicUsize = AtomicUsize::new(0);

/// Get framebuffer info for kernel_main to use
#[no_mangle]
pub fn get_framebuffer_info() -> (usize, usize, usize, usize, usize, usize, usize, usize) {
    use core::sync::atomic::Ordering::Relaxed;
    (
        FRAMEBUFFER_INFO.load(Relaxed),
        FRAMEBUFFER_WIDTH.load(Relaxed),
        FRAMEBUFFER_HEIGHT.load(Relaxed),
        FRAMEBUFFER_PITCH.load(Relaxed),
        FRAMEBUFFER_BPP.load(Relaxed),
        FRAMEBUFFER_RED_SHIFT.load(Relaxed),
        FRAMEBUFFER_GREEN_SHIFT.load(Relaxed),
        FRAMEBUFFER_BLUE_SHIFT.load(Relaxed),
    )
}

/// IDT Entry structure
#[derive(Copy, Clone)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn new() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn set_handler(&mut self, handler: unsafe extern "C" fn()) {
        let addr = handler as u64;
        self.offset_low = (addr & 0xFFFF) as u16;
        self.offset_mid = ((addr >> 16) & 0xFFFF) as u16;
        self.offset_high = ((addr >> 32) & 0xFFFFFFFF) as u32;
        self.selector = 0x08; // Kernel code segment
        self.ist = 0;
        self.type_attr = 0x8E; // Present, DPL=0, Interrupt Gate
        self.reserved = 0;
    }
}

/// IDT Descriptor for LIDT instruction
#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u64,
}

/// IDT with 256 entries
#[link_section = ".bss"]
static mut IDT: [IdtEntry; 256] = [IdtEntry::new(); 256];

/// 32KB kernel stack (allocated in BSS, automatically zeroed and mapped)
#[link_section = ".bss"]
static mut KERNEL_STACK: [u8; 32768] = [0; 32768];

/// Generic exception handler - halt on any exception
unsafe extern "C" fn exception_handler() {
    serial_write("\n[EXCEPTION] CPU exception occurred (unknown vector)!\n");

    // Print debug marker to see where we crashed
    serial_write("[EXCEPTION] DEBUG_MARKER: ");
    let marker = folkering_kernel::arch::x86_64::syscall::get_debug_marker();
    write_hex(marker);
    serial_write("\n");

    serial_write("[EXCEPTION] Halting.\n");
    core::arch::asm!("cli");
    loop {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

// Specific vector handlers to identify which exception is firing
macro_rules! make_exception_handler {
    ($name:ident, $vector:expr, $msg:expr) => {
        unsafe extern "C" fn $name() {
            serial_write($msg);
            serial_write(" DEBUG_MARKER: ");
            let marker = folkering_kernel::arch::x86_64::syscall::get_debug_marker();
            write_hex(marker);
            serial_write("\n");
            serial_write("[EXCEPTION] Halting.\n");
            core::arch::asm!("cli");
            loop { core::arch::asm!("hlt", options(nomem, nostack)); }
        }
    };
}

make_exception_handler!(exc_de, 0, "\n[#DE] Division Error (Vector 0)!");
make_exception_handler!(exc_db, 1, "\n[#DB] Debug (Vector 1)!");
make_exception_handler!(exc_nmi, 2, "\n[#NMI] Non-Maskable Interrupt (Vector 2)!");
make_exception_handler!(exc_bp, 3, "\n[#BP] Breakpoint (Vector 3)!");
make_exception_handler!(exc_of, 4, "\n[#OF] Overflow (Vector 4)!");
make_exception_handler!(exc_br, 5, "\n[#BR] Bound Range Exceeded (Vector 5)!");
make_exception_handler!(exc_ud, 6, "\n[#UD] Undefined Opcode (Vector 6)!");
make_exception_handler!(exc_nm, 7, "\n[#NM] Device Not Available (Vector 7)!");
make_exception_handler!(exc_df, 8, "\n[#DF] Double Fault (Vector 8)!");
make_exception_handler!(exc_cso, 9, "\n[#CSO] Coprocessor Segment Overrun (Vector 9)!");
make_exception_handler!(exc_ts, 10, "\n[#TS] Invalid TSS (Vector 10)!");
make_exception_handler!(exc_np, 11, "\n[#NP] Segment Not Present (Vector 11)!");
make_exception_handler!(exc_ss, 12, "\n[#SS] Stack-Segment Fault (Vector 12)!");
make_exception_handler!(exc_reserved15, 15, "\n[RESERVED] Vector 15!");
make_exception_handler!(exc_mf, 16, "\n[#MF] x87 FPU Error (Vector 16)!");
make_exception_handler!(exc_ac, 17, "\n[#AC] Alignment Check (Vector 17)!");
make_exception_handler!(exc_mc, 18, "\n[#MC] Machine Check (Vector 18)!");
make_exception_handler!(exc_xm, 19, "\n[#XM] SIMD Exception (Vector 19)!");
make_exception_handler!(exc_ve, 20, "\n[#VE] Virtualization Exception (Vector 20)!");
make_exception_handler!(exc_cp, 21, "\n[#CP] Control Protection Exception (Vector 21)!");
make_exception_handler!(exc_reserved22, 22, "\n[RESERVED] Vector 22!");
make_exception_handler!(exc_reserved23, 23, "\n[RESERVED] Vector 23!");
make_exception_handler!(exc_reserved24, 24, "\n[RESERVED] Vector 24!");
make_exception_handler!(exc_reserved25, 25, "\n[RESERVED] Vector 25!");
make_exception_handler!(exc_reserved26, 26, "\n[RESERVED] Vector 26!");
make_exception_handler!(exc_reserved27, 27, "\n[RESERVED] Vector 27!");
make_exception_handler!(exc_hv, 28, "\n[#HV] Hypervisor Injection Exception (Vector 28)!");
make_exception_handler!(exc_vc, 29, "\n[#VC] VMM Communication Exception (Vector 29)!");
make_exception_handler!(exc_sx, 30, "\n[#SX] Security Exception (Vector 30)!");
make_exception_handler!(exc_reserved31, 31, "\n[RESERVED] Vector 31!");
// IRQ handlers

/// Timer interrupt handler (Vector 32) - PREEMPTIVE VERSION
///
/// This handler is called ~100 times per second (10ms interval).
/// It checks if we were in userspace (CS has RPL=3) and only attempts
/// preemption in that case. Kernel-mode interrupts just increment tick and return.
///
/// Stack layout from userspace interrupt (has privilege change):
///   [RSP+0]   rip    (pushed by CPU)
///   [RSP+8]   cs     (pushed by CPU)
///   [RSP+16]  rflags (pushed by CPU)
///   [RSP+24]  rsp    (pushed by CPU)
///   [RSP+32]  ss     (pushed by CPU)
///
/// Stack layout from kernel interrupt (no privilege change):
///   [RSP+0]   rip    (pushed by CPU)
///   [RSP+8]   cs     (pushed by CPU)
///   [RSP+16]  rflags (pushed by CPU)
#[unsafe(naked)]
extern "C" fn irq_timer() {
    core::arch::naked_asm!(
        // First, check if we came from userspace by looking at CS on stack
        // CS is at [RSP+8], and RPL is in bits 0-1
        "push rax",
        "mov rax, [rsp + 16]",     // CS is at RSP+8, but we pushed RAX (+8), so RSP+16
        "and rax, 3",              // Get RPL (Ring Privilege Level)
        "cmp rax, 3",              // Check if RPL == 3 (userspace)
        "pop rax",
        "je 1f",                   // Jump to preemptive path if from userspace

        // =========================================
        // KERNEL MODE PATH - Simple tick and return
        // =========================================
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",

        // Increment tick counter
        "call {tick_fn}",

        // Send EOI to APIC
        "call {eoi_fn}",

        // Restore registers
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Return from interrupt (kernel -> kernel, no RSP/SS on stack)
        "iretq",

        // =========================================
        // USERSPACE PATH - Full preemptive handling
        // =========================================
        "1:",
        // Save ALL registers for potential task switch
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // Align stack to 16 bytes before call (15 pushes = 120 bytes, need 8 more)
        "sub rsp, 8",

        // Pass pointer to saved context as first argument
        "lea rdi, [rsp + 8]",

        // Call preemption handler - returns pointer to Context to restore
        "call {preempt_fn}",

        // RAX now contains pointer to Context structure to restore from
        // Remove alignment padding
        "add rsp, 8",

        // Move returned context pointer to R11
        "mov r11, rax",

        // Discard our saved registers (we'll restore from context)
        "add rsp, 120",  // Skip all 15 pushed registers

        // Now RSP points to the CPU's interrupt frame (rip, cs, rflags, rsp, ss)
        // Overwrite it with values from the returned Context

        "mov rax, [r11 + 152]",   // SS from context
        "mov [rsp + 32], rax",
        "mov rax, [r11 + 0]",     // RSP from context
        "mov [rsp + 24], rax",
        "mov rax, [r11 + 136]",   // RFLAGS from context
        "mov [rsp + 16], rax",
        "mov rax, [r11 + 144]",   // CS from context
        "mov [rsp + 8], rax",
        "mov rax, [r11 + 128]",   // RIP from context
        "mov [rsp], rax",

        // Restore general purpose registers from context
        "mov rax, [r11 + 16]",
        "mov rbx, [r11 + 24]",
        "mov rcx, [r11 + 32]",
        "mov rdx, [r11 + 40]",
        "mov rsi, [r11 + 48]",
        "mov rdi, [r11 + 56]",
        "mov rbp, [r11 + 8]",
        "mov r8,  [r11 + 64]",
        "mov r9,  [r11 + 72]",
        "mov r10, [r11 + 80]",
        "mov r12, [r11 + 96]",
        "mov r13, [r11 + 104]",
        "mov r14, [r11 + 112]",
        "mov r15, [r11 + 120]",
        "mov r11, [r11 + 88]",

        // Return from interrupt (user -> user, has RSP/SS on stack)
        "iretq",

        tick_fn = sym folkering_kernel::timer::tick,
        eoi_fn = sym folkering_kernel::arch::x86_64::apic::send_eoi,
        preempt_fn = sym folkering_kernel::task::preempt::timer_preempt_handler,
    );
}
/// Keyboard interrupt handler (Vector 33 / IRQ1)
///
/// Reads scancode, translates to ASCII, buffers it.
/// PIC EOI is sent inside handle_interrupt().
#[unsafe(naked)]
extern "C" fn irq_keyboard() {
    core::arch::naked_asm!(
        // Save caller-saved registers
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",

        // Call keyboard driver's handle_interrupt() (includes PIC EOI)
        "call {kbd_fn}",

        // Restore registers
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Return from interrupt
        "iretq",

        kbd_fn = sym folkering_kernel::drivers::keyboard::handle_interrupt,
    );
}
/// Mouse interrupt handler (Vector 44 / IRQ12)
///
/// Reads mouse byte, processes packet, buffers event.
/// Sends EOI to both PICs (mouse is on PIC2).
#[unsafe(naked)]
extern "C" fn irq_mouse() {
    core::arch::naked_asm!(
        // Save caller-saved registers
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",

        // Call mouse driver's handle_interrupt() (includes PIC EOI)
        "call {mouse_fn}",

        // Restore registers
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Return from interrupt
        "iretq",

        mouse_fn = sym folkering_kernel::drivers::mouse::handle_interrupt,
    );
}

make_exception_handler!(irq_34, 34, "\n[IRQ2] Cascade (Vector 34)!");
make_exception_handler!(irq_35, 35, "\n[IRQ3] COM2 (Vector 35)!");
make_exception_handler!(irq_36, 36, "\n[IRQ4] COM1 (Vector 36)!");
// APIC spurious interrupt (usually 0xFF or 255)
make_exception_handler!(spurious_255, 255, "\n[SPURIOUS] APIC Spurious Interrupt (Vector 255)!");
// Also check some other common high vectors
make_exception_handler!(vec_128, 128, "\n[VEC128] INT 0x80 (Vector 128)!");
make_exception_handler!(vec_254, 254, "\n[VEC254] Vector 254!");

/// #GP (General Protection Fault) handler - vector 13
/// Stack layout: [error_code, rip, cs, rflags, rsp, ss]
#[unsafe(naked)]
extern "C" fn gp_handler() {
    core::arch::naked_asm!(
        // Save registers we'll use
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",

        // Print #GP header
        "mov rdi, {gp_msg}",
        "call {serial_write_fn}",

        // Print error code (at rsp+32 due to 4 pushes)
        "mov rdi, {err_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 32]",  // error_code
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RIP (at rsp+40)
        "mov rdi, {rip_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 40]",  // rip
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print CS (at rsp+48)
        "mov rdi, {cs_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 48]",  // cs
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RFLAGS (at rsp+56)
        "mov rdi, {rflags_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 56]",  // rflags
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RSP (at rsp+64)
        "mov rdi, {rsp_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 64]",  // rsp
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print SS (at rsp+72)
        "mov rdi, {ss_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 72]",  // ss
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print DEBUG_MARKER
        "mov rdi, {marker_msg}",
        "call {serial_write_fn}",
        "call {get_marker_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print debug RIP from syscall context
        "mov rdi, {dbg_rip_msg}",
        "call {serial_write_fn}",
        "call {get_dbg_rip_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print debug RSP from syscall context
        "mov rdi, {dbg_rsp_msg}",
        "call {serial_write_fn}",
        "call {get_dbg_rsp_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print debug RFLAGS from syscall context
        "mov rdi, {dbg_rflags_msg}",
        "call {serial_write_fn}",
        "call {get_dbg_rflags_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Halt
        "cli",
        "2:",
        "hlt",
        "jmp 2b",

        gp_msg = sym GP_MSG,
        err_msg = sym ERR_MSG,
        rip_msg = sym RIP_MSG,
        cs_msg = sym CS_MSG,
        rflags_msg = sym RFLAGS_MSG,
        rsp_msg = sym RSP_MSG,
        ss_msg = sym SS_MSG,
        marker_msg = sym MARKER_MSG,
        dbg_rip_msg = sym DBG_RIP_MSG,
        dbg_rsp_msg = sym DBG_RSP_MSG,
        dbg_rflags_msg = sym DBG_RFLAGS_MSG,
        newline = sym NEWLINE,
        serial_write_fn = sym serial_write_cstr,
        write_hex_fn = sym write_hex_from_reg,
        get_marker_fn = sym get_debug_marker_wrapper,
        get_dbg_rip_fn = sym get_debug_rip_wrapper,
        get_dbg_rsp_fn = sym get_debug_rsp_wrapper,
        get_dbg_rflags_fn = sym get_debug_rflags_wrapper,
    );
}

/// #PF (Page Fault) handler - vector 14
/// Stack layout: [error_code, rip, cs, rflags, rsp, ss]
/// CR2 contains the faulting address
#[unsafe(naked)]
extern "C" fn pf_handler() {
    core::arch::naked_asm!(
        // Save registers we'll use
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",

        // Print #PF header
        "mov rdi, {pf_msg}",
        "call {serial_write_fn}",

        // Print CR2 (faulting address)
        "mov rdi, {cr2_msg}",
        "call {serial_write_fn}",
        "mov rax, cr2",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print error code (at rsp+32 due to 4 pushes)
        "mov rdi, {err_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 32]",  // error_code
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RIP (at rsp+40)
        "mov rdi, {rip_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 40]",  // rip
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print CS (at rsp+48)
        "mov rdi, {cs_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 48]",  // cs
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print DEBUG_MARKER
        "mov rdi, {marker_msg}",
        "call {serial_write_fn}",
        "call {get_marker_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print DEBUG_RETURN_VAL (RAX before IRETQ)
        "mov rdi, {retval_msg}",
        "call {serial_write_fn}",
        "call {get_retval_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print DEBUG_HANDLER_RESULT (what Rust returned)
        "mov rdi, {handler_result_msg}",
        "call {serial_write_fn}",
        "call {get_handler_result_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Halt
        "cli",
        "2:",
        "hlt",
        "jmp 2b",

        pf_msg = sym PF_MSG,
        cr2_msg = sym CR2_MSG,
        err_msg = sym ERR_MSG,
        rip_msg = sym RIP_MSG,
        cs_msg = sym CS_MSG,
        marker_msg = sym MARKER_MSG,
        retval_msg = sym RETVAL_MSG,
        handler_result_msg = sym HANDLER_RESULT_MSG,
        newline = sym NEWLINE,
        serial_write_fn = sym serial_write_cstr,
        write_hex_fn = sym write_hex_from_reg,
        get_marker_fn = sym get_debug_marker_wrapper,
        get_retval_fn = sym get_debug_return_val_wrapper,
        get_handler_result_fn = sym get_debug_handler_result_wrapper,
    );
}

// Static strings for exception handlers
#[link_section = ".rodata"]
static GP_MSG: &[u8] = b"\n[#GP] General Protection Fault!\n\0";
#[link_section = ".rodata"]
static PF_MSG: &[u8] = b"\n[#PF] Page Fault!\n\0";
#[link_section = ".rodata"]
static CR2_MSG: &[u8] = b"  CR2 (fault addr): \0";
#[link_section = ".rodata"]
static ERR_MSG: &[u8] = b"  Error code: \0";
#[link_section = ".rodata"]
static RIP_MSG: &[u8] = b"  RIP: \0";
#[link_section = ".rodata"]
static CS_MSG: &[u8] = b"  CS: \0";
#[link_section = ".rodata"]
static RFLAGS_MSG: &[u8] = b"  RFLAGS: \0";
#[link_section = ".rodata"]
static RSP_MSG: &[u8] = b"  RSP: \0";
#[link_section = ".rodata"]
static SS_MSG: &[u8] = b"  SS: \0";
#[link_section = ".rodata"]
static MARKER_MSG: &[u8] = b"  DEBUG_MARKER: \0";
#[link_section = ".rodata"]
static RETVAL_MSG: &[u8] = b"  DEBUG_RETURN_VAL (RAX in asm): \0";
#[link_section = ".rodata"]
static HANDLER_RESULT_MSG: &[u8] = b"  DEBUG_HANDLER_RESULT (Rust return): \0";
#[link_section = ".rodata"]
static DBG_RIP_MSG: &[u8] = b"  syscall DEBUG_RIP: \0";
#[link_section = ".rodata"]
static DBG_RSP_MSG: &[u8] = b"  syscall DEBUG_RSP (CS): \0";
#[link_section = ".rodata"]
static DBG_RFLAGS_MSG: &[u8] = b"  syscall DEBUG_RFLAGS (SS): \0";
#[link_section = ".rodata"]
static NEWLINE: &[u8] = b"\n\0";

/// Write C-string (null-terminated) to serial
#[no_mangle]
unsafe extern "C" fn serial_write_cstr(s: *const u8) {
    let mut ptr = s;
    while *ptr != 0 {
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") *ptr,
            options(nostack)
        );
        ptr = ptr.add(1);
    }
}

/// Write hex from RDI register to serial
#[no_mangle]
unsafe extern "C" fn write_hex_from_reg(num: u64) {
    write_hex(num);
}

/// Wrapper to get debug marker (returns in RAX)
#[no_mangle]
unsafe extern "C" fn get_debug_marker_wrapper() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_marker()
}

/// Wrapper to get debug RIP
#[no_mangle]
unsafe extern "C" fn get_debug_rip_wrapper() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_rip()
}

/// Wrapper to get debug RSP (abused for CS in yield path)
#[no_mangle]
unsafe extern "C" fn get_debug_rsp_wrapper() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_rsp()
}

/// Wrapper to get debug RFLAGS (abused for SS in yield path)
#[no_mangle]
unsafe extern "C" fn get_debug_rflags_wrapper() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_rflags()
}

/// Wrapper to get debug return value (RAX before IRETQ)
#[no_mangle]
unsafe extern "C" fn get_debug_return_val_wrapper() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_return_val()
}

/// Wrapper to get handler result (what Rust returned)
#[no_mangle]
unsafe extern "C" fn get_debug_handler_result_wrapper() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_handler_result()
}

/// Write a string to COM1 serial port
unsafe fn serial_write(s: &str) {
    for &byte in s.as_bytes() {
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") byte,
            options(nostack)
        );
    }
}

/// Write a hex number to serial (minimal implementation)
unsafe fn write_hex(mut num: u64) {
    serial_write("0x");
    let hex_chars = b"0123456789ABCDEF";
    let mut buffer = [0u8; 16];
    let mut i = 0;

    if num == 0 {
        serial_write("0");
        return;
    }

    while num > 0 {
        buffer[i] = hex_chars[(num & 0xF) as usize];
        num >>= 4;
        i += 1;
    }

    // Print in reverse order
    while i > 0 {
        i -= 1;
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") buffer[i],
            options(nostack)
        );
    }
}

/// Write a single byte as 2 hex digits
unsafe fn write_hex_byte(byte: u8) {
    let hex_chars = b"0123456789ABCDEF";
    let high = hex_chars[(byte >> 4) as usize];
    let low = hex_chars[(byte & 0xF) as usize];
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") high,
        options(nostack)
    );
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") low,
        options(nostack)
    );
}

/// Initialize IDT with generic exception handlers
#[inline(never)]
unsafe fn init_idt() {
    // VERY FIRST THING: print to verify function is called
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") b'I',
        options(nostack)
    );
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") b'D',
        options(nostack)
    );
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") b'T',
        options(nostack)
    );
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") b'\n',
        options(nostack)
    );
    // Set all IDT entries to the generic exception handler
    serial_write("[IDT] Setting generic handler for all vectors...\n");
    let generic_addr = exception_handler as *const () as u64;
    serial_write("[IDT] Generic handler at: ");
    write_hex(generic_addr);
    serial_write("\n");

    for entry in &mut IDT {
        entry.set_handler(exception_handler);
    }

    serial_write("[IDT] Setting specific exception handlers...\n");

    // Print address of specific handler for comparison
    let exc_ud_addr = exc_ud as *const () as u64;
    serial_write("[IDT] exc_ud (#UD) handler at: ");
    write_hex(exc_ud_addr);
    serial_write("\n");

    // Set specific handlers for each exception vector so we can identify them
    IDT[0].set_handler(core::mem::transmute(exc_de as *const ()));   // #DE
    IDT[1].set_handler(core::mem::transmute(exc_db as *const ()));   // #DB
    IDT[2].set_handler(core::mem::transmute(exc_nmi as *const ()));  // NMI
    IDT[3].set_handler(core::mem::transmute(exc_bp as *const ()));   // #BP
    IDT[4].set_handler(core::mem::transmute(exc_of as *const ()));   // #OF
    IDT[5].set_handler(core::mem::transmute(exc_br as *const ()));   // #BR
    IDT[6].set_handler(core::mem::transmute(exc_ud as *const ()));   // #UD

    // Verify IDT[6] was set correctly
    let stored_low = IDT[6].offset_low as u64;
    let stored_mid = (IDT[6].offset_mid as u64) << 16;
    let stored_high = (IDT[6].offset_high as u64) << 32;
    let stored_addr = stored_low | stored_mid | stored_high;
    serial_write("[IDT] IDT[6] handler stored as: ");
    write_hex(stored_addr);
    serial_write("\n");
    if stored_addr == exc_ud_addr {
        serial_write("[IDT] IDT[6] matches exc_ud - OK\n");
    } else {
        serial_write("[IDT] IDT[6] MISMATCH!\n");
    }

    IDT[7].set_handler(core::mem::transmute(exc_nm as *const ()));   // #NM
    IDT[8].set_handler(core::mem::transmute(exc_df as *const ()));   // #DF
    IDT[9].set_handler(core::mem::transmute(exc_cso as *const ()));  // #CSO
    IDT[10].set_handler(core::mem::transmute(exc_ts as *const ())); // #TS
    IDT[11].set_handler(core::mem::transmute(exc_np as *const ())); // #NP
    IDT[12].set_handler(core::mem::transmute(exc_ss as *const ())); // #SS
    IDT[13].set_handler(core::mem::transmute(gp_handler as *const ()));  // #GP
    IDT[14].set_handler(core::mem::transmute(pf_handler as *const ()));  // #PF
    IDT[15].set_handler(core::mem::transmute(exc_reserved15 as *const ())); // Reserved
    IDT[16].set_handler(core::mem::transmute(exc_mf as *const ())); // #MF
    IDT[17].set_handler(core::mem::transmute(exc_ac as *const ())); // #AC
    IDT[18].set_handler(core::mem::transmute(exc_mc as *const ())); // #MC
    IDT[19].set_handler(core::mem::transmute(exc_xm as *const ())); // #XM
    IDT[20].set_handler(core::mem::transmute(exc_ve as *const ())); // #VE
    IDT[21].set_handler(core::mem::transmute(exc_cp as *const ())); // #CP
    IDT[22].set_handler(core::mem::transmute(exc_reserved22 as *const ()));
    IDT[23].set_handler(core::mem::transmute(exc_reserved23 as *const ()));
    IDT[24].set_handler(core::mem::transmute(exc_reserved24 as *const ()));
    IDT[25].set_handler(core::mem::transmute(exc_reserved25 as *const ()));
    IDT[26].set_handler(core::mem::transmute(exc_reserved26 as *const ()));
    IDT[27].set_handler(core::mem::transmute(exc_reserved27 as *const ()));
    IDT[28].set_handler(core::mem::transmute(exc_hv as *const ())); // #HV
    IDT[29].set_handler(core::mem::transmute(exc_vc as *const ())); // #VC
    IDT[30].set_handler(core::mem::transmute(exc_sx as *const ())); // #SX
    IDT[31].set_handler(core::mem::transmute(exc_reserved31 as *const ()));
    // IRQ handlers (32+)
    IDT[32].set_handler(core::mem::transmute(irq_timer as *const ())); // Timer

    // DEBUG: Print IDT[32] raw bytes to verify format
    serial_write("[IDT] IDT[32] raw bytes: ");
    let idt32_ptr = &IDT[32] as *const IdtEntry as *const u8;
    for i in 0..16 {
        let byte = core::ptr::read_volatile(idt32_ptr.add(i));
        write_hex_byte(byte);
        serial_write(" ");
    }
    serial_write("\n");
    serial_write("[IDT] IDT[32] selector: ");
    write_hex(IDT[32].selector as u64);
    serial_write(", type_attr: ");
    write_hex(IDT[32].type_attr as u64);
    serial_write(", ist: ");
    write_hex(IDT[32].ist as u64);
    serial_write("\n");

    IDT[33].set_handler(core::mem::transmute(irq_keyboard as *const ()));
    IDT[34].set_handler(core::mem::transmute(irq_34 as *const ()));
    IDT[35].set_handler(core::mem::transmute(irq_35 as *const ()));
    IDT[36].set_handler(core::mem::transmute(irq_36 as *const ()));
    // IRQ12 = Mouse (on PIC2, vector 32+12=44)
    IDT[44].set_handler(core::mem::transmute(irq_mouse as *const ()));
    // Special vectors
    IDT[128].set_handler(core::mem::transmute(vec_128 as *const ())); // INT 0x80
    IDT[254].set_handler(core::mem::transmute(vec_254 as *const ()));
    IDT[255].set_handler(core::mem::transmute(spurious_255 as *const ())); // APIC spurious

    // Create IDT descriptor
    let idt_desc = IdtDescriptor {
        limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: IDT.as_ptr() as u64,
    };

    // Load IDT
    core::arch::asm!(
        "lidt [{}]",
        in(reg) &idt_desc,
        options(readonly, nostack, preserves_flags)
    );
}


/// Kernel entry point
#[no_mangle]
unsafe extern "C" fn kmain() -> ! {
    // CRITICAL: Clear BSS BEFORE switching to our custom stack
    // (because our stack is IN the BSS section!)
    extern "C" {
        static mut __bss_start: u8;
        static mut __bss_end: u8;
    }

    let bss_start = &raw mut __bss_start;
    let bss_end = &raw mut __bss_end;
    let bss_size = bss_end as usize - bss_start as usize;
    core::ptr::write_bytes(bss_start, 0, bss_size);

    // NOW switch to our 32KB kernel stack (which was just zeroed)
    // Limine's default stack is tiny (~500 bytes) - not enough for task spawning
    // Our 32KB stack is allocated in BSS section (KERNEL_STACK static array)

    // Get stack top address (stack grows DOWN, so top = base + size)
    let stack_top_addr = KERNEL_STACK.as_ptr().add(KERNEL_STACK.len()) as u64;

    core::arch::asm!(
        "mov rsp, {0}",
        "mov rbp, {0}",
        in(reg) stack_top_addr,
    );

    // Disable interrupts
    core::arch::asm!("cli");

    // Enable SSE/FPU (required for Rust code that uses SIMD for struct initialization)
    // Without this, instructions like movaps will cause #UD
    core::arch::asm!(
        // Read CR0
        "mov rax, cr0",
        // Clear EM (bit 2) - no x87 emulation
        // Clear TS (bit 3) - no task switched (don't trap on FPU access)
        // Set MP (bit 1) - monitor coprocessor
        "and ax, 0xFFFB",  // Clear EM (bit 2)
        "and ax, 0xFFF7",  // Clear TS (bit 3)
        "or ax, 0x2",      // Set MP (bit 1)
        "mov cr0, rax",

        // Read CR4
        "mov rax, cr4",
        // Set OSFXSR (bit 9) - enable SSE
        // Set OSXMMEXCPT (bit 10) - enable SSE exceptions
        "or rax, 0x600",    // Set bits 9 and 10
        // Clear SMEP (bit 20) and SMAP (bit 21) to allow kernel access to user pages
        // This is needed for syscalls that copy data to/from userspace
        "btr rax, 20",      // Clear SMEP
        "btr rax, 21",      // Clear SMAP
        "mov cr4, rax",
        out("rax") _,
    );

    // Write boot message
    serial_write("\n\n[Folkering OS] Kernel booted successfully!\n");

    // Initialize IDT first (critical for stability)
    serial_write("[Folkering OS] Setting up IDT...\n");

    // Debug: print X directly before calling init_idt
    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b'X', options(nostack));

    init_idt();

    // Debug: print Y directly after calling init_idt
    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b'Y', options(nostack));
    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b'\n', options(nostack));

    serial_write("[Folkering OS] IDT loaded\n");

    // Build BootInfo structure from Limine responses
    serial_write("[Folkering OS] Building boot information...\n");

    // Get HHDM offset
    let hhdm_offset = if let Some(hhdm) = HHDM_REQUEST.get_response() {
        hhdm.offset() as usize
    } else {
        serial_write("[ERROR] No HHDM response!\n");
        halt_loop();
    };

    // Get RSDP address
    let rsdp_addr = if let Some(rsdp) = RSDP_REQUEST.get_response() {
        rsdp.address() as usize
    } else {
        0
    };

    // Try to get memory map entries directly
    // The Limine crate returns the entries as a slice, which should be accessible
    // since Limine has already set up page tables with HHDM mapping
    let (memory_map_slice, total_mem, usable_mem) = if let Some(mmap_response) = MEMORY_MAP_REQUEST.get_response() {
        serial_write("[DEBUG] Got memory map response\n");

        // Get entries - this returns a slice
        let entries = mmap_response.entries();
        serial_write("[DEBUG] Got entries slice\n");

        // The memory map slice is already accessible through HHDM
        // Just pass it directly to BootInfo
        let mut total = 0u64;
        let mut usable = 0u64;

        // Try to calculate totals - access entries one by one
        let len = entries.len();
        serial_write("[DEBUG] Entry count: ");
        // Can't call write_number since it's deleted, just continue

        for entry in entries.iter() {
            total += entry.length;
            if entry.entry_type == limine::memory_map::EntryType::USABLE {
                usable += entry.length;
            }
        }

        serial_write("[DEBUG] Memory map parsed successfully\n");
        (entries, total as usize, usable as usize)
    } else {
        serial_write("[ERROR] No memory map response!\n");
        let empty: &[&limine::memory_map::Entry] = &[];
        (empty, 0, 0)
    };

    // Get initrd module (first module = Folk-Pack image)
    // Limine loads modules into physical memory and provides HHDM-mapped virtual addresses
    let (initrd_start, initrd_size) = if let Some(mod_resp) = MODULE_REQUEST.get_response() {
        let modules = mod_resp.modules();
        if !modules.is_empty() {
            let module = &modules[0];
            let virt_addr = module.addr() as usize;
            let size = module.size() as usize;
            serial_write("[BOOT] Found initrd module at virt ");
            write_hex(virt_addr as u64);
            serial_write(", size ");
            write_hex(size as u64);
            serial_write(" bytes\n");
            (virt_addr, size)
        } else {
            serial_write("[BOOT] No boot modules found (no initrd)\n");
            (0, 0)
        }
    } else {
        serial_write("[BOOT] ModuleRequest not responded (no initrd)\n");
        (0, 0)
    };

    // Get framebuffer info for graphics support
    // Store in static for kernel_main to access later
    if let Some(fb_resp) = FRAMEBUFFER_REQUEST.get_response() {
        let mut framebuffers = fb_resp.framebuffers();
        if let Some(fb) = framebuffers.next() {
            serial_write("[BOOT] Found framebuffer:\n");
            serial_write("  Address: ");
            write_hex(fb.addr() as u64);
            serial_write("\n  Resolution: ");
            write_hex(fb.width() as u64);
            serial_write("x");
            write_hex(fb.height() as u64);
            serial_write(" @ ");
            write_hex(fb.bpp() as u64);
            serial_write("bpp\n  Pitch: ");
            write_hex(fb.pitch() as u64);
            serial_write(" bytes/line\n");

            // Store framebuffer info in static for later use
            FRAMEBUFFER_INFO.store(fb.addr() as usize, core::sync::atomic::Ordering::Relaxed);
            FRAMEBUFFER_WIDTH.store(fb.width() as usize, core::sync::atomic::Ordering::Relaxed);
            FRAMEBUFFER_HEIGHT.store(fb.height() as usize, core::sync::atomic::Ordering::Relaxed);
            FRAMEBUFFER_PITCH.store(fb.pitch() as usize, core::sync::atomic::Ordering::Relaxed);
            FRAMEBUFFER_BPP.store(fb.bpp() as usize, core::sync::atomic::Ordering::Relaxed);
            let r_shift = fb.red_mask_shift();
            let g_shift = fb.green_mask_shift();
            let b_shift = fb.blue_mask_shift();
            serial_write("  Color shifts: R=");
            write_hex(r_shift as u64);
            serial_write(" G=");
            write_hex(g_shift as u64);
            serial_write(" B=");
            write_hex(b_shift as u64);
            serial_write("\n");
            FRAMEBUFFER_RED_SHIFT.store(r_shift as usize, core::sync::atomic::Ordering::Relaxed);
            FRAMEBUFFER_GREEN_SHIFT.store(g_shift as usize, core::sync::atomic::Ordering::Relaxed);
            FRAMEBUFFER_BLUE_SHIFT.store(b_shift as usize, core::sync::atomic::Ordering::Relaxed);
        } else {
            serial_write("[BOOT] No framebuffers available\n");
        }
    } else {
        serial_write("[BOOT] Framebuffer request not responded\n");
    }

    let boot_info = folkering_kernel::boot::BootInfo {
        bootloader_name: "Limine",
        bootloader_version: "8.7.0",
        memory_total: total_mem,
        memory_usable: usable_mem,
        kernel_phys_base: 0x1ff50000, // Approximate from Limine output
        kernel_virt_base: 0xFFFF_FFFF_8000_0000,
        hhdm_offset,
        rsdp_addr,
        memory_map: memory_map_slice,
        initrd_start,
        initrd_size,
    };

    serial_write("[Folkering OS] Boot info ready, calling kernel_main...\n\n");

    // Call main kernel initialization
    folkering_kernel::kernel_main_with_boot_info(&boot_info);
}

/// Halt loop for errors
fn halt_loop() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
