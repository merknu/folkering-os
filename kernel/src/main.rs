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

    fn set_handler_addr(&mut self, addr: u64) {
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

/// Heartbeat print from kernel-mode timer path.
/// Fires every 500 kernel ticks (~5 seconds) to confirm kernel is alive.
/// Presence of [HB] lines = all tasks have crashed (timer fires only in kernel mode).
/// Absence of [HB] lines + presence of [IRQ_CTX] lines = tasks running normally.
#[no_mangle]
pub extern "C" fn kernel_timer_heartbeat() {
    use core::sync::atomic::{AtomicU64, Ordering};
    static HB_TICKS: AtomicU64 = AtomicU64::new(0);
    let tick = HB_TICKS.fetch_add(1, Ordering::Relaxed);
    if tick % 500 == 0 {
        folkering_kernel::serial_str!("[HB] kernel_ticks=");
        folkering_kernel::drivers::serial::write_dec(tick as u32);
        folkering_kernel::serial_str!(" uptime_ms=");
        folkering_kernel::drivers::serial::write_dec(folkering_kernel::timer::uptime_ms() as u32);
        folkering_kernel::serial_str!(" debug_marker=");
        folkering_kernel::drivers::serial::write_hex(
            folkering_kernel::arch::x86_64::syscall::DEBUG_MARKER
                .load(core::sync::atomic::Ordering::Relaxed)
        );
        folkering_kernel::drivers::serial::write_newline();
    }
}

/// Debug helper: called from irq_timer asm after timer_preempt_handler returns.
/// Writes a single 'U' byte DIRECTLY to UART 0x3F8 on every call — bypasses mutex.
/// If UART works: serial log fills with 'U'. If not: UART itself is broken.
#[no_mangle]
pub unsafe extern "C" fn debug_after_preempt_handler(_ctx: u64) {
    // No-op in production — enable for debugging context switch issues
}

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

        // Periodic heartbeat print (every 500 ticks) so serial logger can detect kernel-only mode
        "call {heartbeat_fn}",

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

        // FXSAVE: save x87 FPU + XMM0-15 + MXCSR for the preempted task.
        // FXSAVE_CURRENT_PTR is set by timer_preempt_handler to the current task's
        // fxsave_area before returning, so before the call it still points at the
        // task being preempted.
        "mov rax, qword ptr [rip + {fxsave_ptr}]",
        "test rax, rax",
        "jz 2f",              // skip if pointer is NULL (no task yet)
        "fxsave64 [rax]",     // save FPU/SSE state to current task's fxsave_area
        "2:",

        // Align stack to 16 bytes before call (15 pushes = 120 bytes, need 8 more)
        "sub rsp, 8",

        // Pass pointer to saved context as first argument
        "lea rdi, [rsp + 8]",

        // Call preemption handler - returns pointer to Context to restore.
        "call {preempt_fn}",

        // RAX now contains pointer to Context structure to restore from
        // Remove alignment padding
        "add rsp, 8",

        // Move returned context pointer to R11
        "mov r11, rax",

        // Debug: print CS and RIP we are about to iretq to.
        // r11 is caller-saved so debug_ctx_fn may clobber it — push/pop to preserve.
        "push r11",
        "mov rdi, r11",
        "call {debug_ctx_fn}",
        "pop r11",

        // FXRSTOR: restore FPU/SSE state for the task we are switching TO.
        // timer_preempt_handler has already updated FXSAVE_CURRENT_PTR to point
        // at the new task's fxsave_area, so we restore from there.
        "mov rax, qword ptr [rip + {fxsave_ptr}]",
        "test rax, rax",
        "jz 3f",
        "fxrstor64 [rax]",    // restore XMM state for the incoming task
        "3:",

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
        heartbeat_fn = sym kernel_timer_heartbeat,
        preempt_fn = sym folkering_kernel::task::preempt::timer_preempt_handler,
        fxsave_ptr = sym folkering_kernel::task::task::FXSAVE_CURRENT_PTR,
        debug_ctx_fn = sym debug_after_preempt_handler,
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
        // Save registers we'll use (rdi/rsi first so we can print their original values)
        "push rdi",
        "push rsi",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        // Stack after 6 pushes: [rsp+0]=rdx,[rsp+8]=rcx,[rsp+16]=rbx,[rsp+24]=rax,
        //   [rsp+32]=rsi,[rsp+40]=rdi,[rsp+48]=error_code,[rsp+56]=rip,
        //   [rsp+64]=cs,[rsp+72]=rflags,[rsp+80]=user_rsp,[rsp+88]=ss

        // Print #GP header
        "mov rdi, {gp_msg}",
        "call {serial_write_fn}",

        // Print error code (at rsp+48 due to 6 pushes)
        "mov rdi, {err_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 48]",  // error_code
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RIP (at rsp+56)
        "mov rdi, {rip_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 56]",  // rip
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print CS (at rsp+64)
        "mov rdi, {cs_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 64]",  // cs
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RFLAGS (at rsp+72)
        "mov rdi, {rflags_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 72]",  // rflags
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print RSP (at rsp+80)
        "mov rdi, {rsp_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 80]",  // rsp
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print SS (at rsp+88)
        "mov rdi, {ss_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 88]",  // ss
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print original RDI (at rsp+40) - likely 'self' pointer in fill_rect
        "mov rdi, {saved_rdi_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 40]",  // original RDI
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print original RSI (at rsp+32) - likely 'x' arg in fill_rect
        "mov rdi, {saved_rsi_msg}",
        "call {serial_write_fn}",
        "mov rdi, [rsp + 32]",  // original RSI
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

        // Print Context.r14 value from last syscall/yield restore (KEY DIAGNOSTIC)
        "mov rdi, {ctx_r14_msg}",
        "call {serial_write_fn}",
        "call {get_ctx_r14_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Print Context.rsp value (user RSP stored in Context)
        "mov rdi, {ctx_rsp_msg}",
        "call {serial_write_fn}",
        "call {get_ctx_rsp_fn}",
        "mov rdi, rax",
        "call {write_hex_fn}",
        "mov rdi, {newline}",
        "call {serial_write_fn}",

        // Draw panic screen (rdi=msg, rsi=rip, rdx=cs, rcx=err_code, r8=0 for CR2)
        "cli",
        "mov rdi, {gp_panic_msg}",
        "mov rsi, [rsp + 56]",  // rip
        "mov rdx, [rsp + 64]",  // cs
        "mov rcx, [rsp + 48]",  // error_code
        "xor r8, r8",           // cr2 = 0 for #GP
        "call {panic_screen_fn}",

        // Halt
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
        saved_rdi_msg = sym SAVED_RDI_MSG,
        saved_rsi_msg = sym SAVED_RSI_MSG,
        newline = sym NEWLINE,
        serial_write_fn = sym serial_write_cstr,
        write_hex_fn = sym write_hex_from_reg,
        get_marker_fn = sym get_debug_marker_wrapper,
        get_dbg_rip_fn = sym get_debug_rip_wrapper,
        get_dbg_rsp_fn = sym get_debug_rsp_wrapper,
        get_dbg_rflags_fn = sym get_debug_rflags_wrapper,
        ctx_r14_msg = sym CTX_R14_MSG,
        ctx_rsp_msg = sym CTX_RSP_MSG,
        get_ctx_r14_fn = sym get_debug_context_r14_local,
        get_ctx_rsp_fn = sym get_debug_context_rsp_local,
        gp_panic_msg = sym GP_PANIC_MSG,
        panic_screen_fn = sym panic_screen_wrapper,
    );
}

#[link_section = ".rodata"]
static GP_PANIC_MSG: &str = "#GP - General Protection Fault\0";

/// Wrapper for panic_screen callable from naked asm
/// ABI: rdi=msg_ptr, rsi=rip, rdx=cs, rcx=err_code, r8=cr2
#[no_mangle]
unsafe extern "C" fn panic_screen_wrapper(msg_ptr: *const u8, rip: u64, cs: u64, err: u64, cr2: u64) {
    use core::sync::atomic::{AtomicBool, Ordering};
    static IN_PANIC: AtomicBool = AtomicBool::new(false);

    // Prevent recursive panic (e.g., #PF during panic screen drawing)
    if IN_PANIC.swap(true, Ordering::SeqCst) {
        // Already in panic handler — just print to serial and halt
        folkering_kernel::serial_println!("[PANIC] RECURSIVE FAULT during panic handler!");
        folkering_kernel::serial_println!("  RIP={:#x} CS={:#x} ERR={:#x} CR2={:#x}", rip, cs, err, cr2);
        loop { core::arch::asm!("cli; hlt"); }
    }

    // Switch to kernel page table (PML4) to ensure HHDM framebuffer mapping is accessible
    // The current CR3 might point to a user task's page table after a context switch
    let kernel_cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) kernel_cr3);
    // If CR3 points to a user task PML4, the HHDM might still be mapped
    // (kernel PML4 entries are shared). But just in case, we proceed carefully.

    // Convert C-string pointer to &str
    let mut len = 0;
    while *msg_ptr.add(len) != 0 { len += 1; }
    let msg = core::str::from_utf8_unchecked(core::slice::from_raw_parts(msg_ptr, len));
    panic_screen(msg, rip, cs, err, cr2);
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

        // Dump ALL callee-saved registers to serial for debugging
        // The 4 pushes (rax,rbx,rcx,rdx) are on stack but we still have
        // the original values of rsi,rdi,rbp,r8-r15 in their registers!
        // (The serial_write/hex functions are caller-saved convention)
        // So let's save them NOW and call a Rust dump function
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
        // Stack: [r15 r14 r13 r12 r11 r10 r9 r8 rbp rdi rsi | rdx rcx rbx rax | err rip cs rflags rsp ss]
        // Offsets from rsp: rsi=+80, rdi=+72, rbp=+64, ...
        // But rdi/rsi/etc were already clobbered by the serial calls above.
        // Let's just pass cr2, rip, and the error code from the interrupt frame
        "mov rdi, cr2",              // arg1: faulting address
        "mov rsi, [rsp + 128]",      // arg2: rip (11 pushes × 8 = 88, + err at 88, rip at 96... let me calculate)
        // 11 new pushes (88) + 4 old pushes (32) = 120 bytes above error_code
        // error_code at [rsp + 120], rip at [rsp + 128], cs at [rsp + 136]
        "mov rdx, [rsp + 136]",      // arg3: cs
        "mov rcx, [rsp + 120]",      // arg4: error code
        "call {dump_pf_fn}",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",

        // Draw panic screen
        "mov rdi, {pf_panic_msg}",  // panic message string
        "mov rsi, [rsp + 40]",      // rip (offset 40 after 4 pushes + error_code)
        "mov rdx, [rsp + 48]",      // cs
        "mov rcx, [rsp + 32]",      // error_code
        "mov r8, cr2",              // cr2 = faulting address
        "call {panic_screen_fn}",

        // Halt
        "cli",
        "2:",
        "hlt",
        "jmp 2b",

        dump_pf_fn = sym dump_pf_context,
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
        pf_panic_msg = sym PF_PANIC_MSG,
        panic_screen_fn = sym panic_screen_wrapper,
    );
}

/// Dump full register state during #PF for debugging context corruption
#[no_mangle]
unsafe extern "C" fn dump_pf_context(cr2: u64, rip: u64, cs: u64, err: u64) {
    // Which task was running?
    let task_id = folkering_kernel::task::task::get_current_task();
    folkering_kernel::serial_println!("[PF_DUMP] Task {} at RIP={:#x} CR2={:#x} ERR={:#x} CS={:#x}",
        task_id, rip, cr2, err, cs);

    // If this is compositor (task 4), dump its saved Context for comparison
    if let Some(task_arc) = folkering_kernel::task::task::get_task(task_id) {
        let task_locked = task_arc.lock();
        let ctx = &task_locked.context;
        folkering_kernel::serial_println!("[PF_DUMP] Saved Context for task {}:", task_id);
        folkering_kernel::serial_println!("  ctx.RIP={:#x} ctx.RAX={:#x} ctx.RSP={:#x}",
            ctx.rip, ctx.rax, ctx.rsp);
        folkering_kernel::serial_println!("  ctx.RBX={:#x} ctx.RBP={:#x} ctx.RCX={:#x} ctx.RDX={:#x}",
            ctx.rbx, ctx.rbp, ctx.rcx, ctx.rdx);
        folkering_kernel::serial_println!("  ctx.R12={:#x} ctx.R13={:#x} ctx.R14={:#x} ctx.R15={:#x}",
            ctx.r12, ctx.r13, ctx.r14, ctx.r15);
        folkering_kernel::serial_println!("  ctx.RDI={:#x} ctx.RSI={:#x} ctx.CS={:#x} ctx.SS={:#x}",
            ctx.rdi, ctx.rsi, ctx.cs, ctx.ss);
    }
}

#[link_section = ".rodata"]
static PF_PANIC_MSG: &str = "#PF - Page Fault\0";

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
#[link_section = ".rodata"]
static SAVED_RDI_MSG: &[u8] = b"  RDI (self/arg0): \0";
#[link_section = ".rodata"]
static SAVED_RSI_MSG: &[u8] = b"  RSI (x/arg1): \0";
#[link_section = ".rodata"]
static CTX_R14_MSG: &[u8] = b"  Context.r14 (before restore): \0";
#[link_section = ".rodata"]
static CTX_RSP_MSG: &[u8] = b"  Context.rsp (user RSP in ctx): \0";
#[link_section = ".rodata"]
static NEXT_CTX_PTR_MSG: &[u8] = b"  [PREEMPT] next_ctx_ptr: \0";
#[link_section = ".rodata"]
static NEXT_CTX_CS_MSG: &[u8] = b"  [PREEMPT] next_ctx.cs: \0";
#[link_section = ".rodata"]
static NEXT_CTX_RIP_MSG: &[u8] = b"  [PREEMPT] next_ctx.rip: \0";

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

/// Wrapper to get Context.r14 value read just before R14 was restored (KEY DIAGNOSTIC)
#[no_mangle]
unsafe extern "C" fn get_debug_context_r14_local() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_context_r14()
}

/// Wrapper to get Context.rsp value read alongside Context.r14
#[no_mangle]
unsafe extern "C" fn get_debug_context_rsp_local() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_context_rsp()
}

/// Wrapper to get Context pointer returned by timer_preempt_handler
#[no_mangle]
unsafe extern "C" fn get_debug_next_ctx_ptr_local() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_next_ctx_ptr()
}

/// Wrapper to get context.cs captured by timer_preempt_handler before returning
#[no_mangle]
unsafe extern "C" fn get_debug_next_ctx_cs_local() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_next_ctx_cs()
}

/// Wrapper to get context.rip captured by timer_preempt_handler before returning
#[no_mangle]
unsafe extern "C" fn get_debug_next_ctx_rip_local() -> u64 {
    folkering_kernel::arch::x86_64::syscall::get_debug_next_ctx_rip()
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

// ============================================================================
// Kernel Panic Screen (BSOD replacement)
// ============================================================================

/// Minimal 8×8 bitmap font — printable ASCII 0x20..=0x7E.
/// Each char is 8 bytes (one byte per row, MSB = leftmost pixel).
static PANIC_FONT: [[u8; 8]; 95] = [
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // ' '
    [0x18,0x3C,0x3C,0x18,0x18,0x00,0x18,0x00], // '!'
    [0x36,0x36,0x00,0x00,0x00,0x00,0x00,0x00], // '"'
    [0x36,0x36,0x7F,0x36,0x7F,0x36,0x36,0x00], // '#'
    [0x0C,0x3E,0x03,0x1E,0x30,0x1F,0x0C,0x00], // '$'
    [0x00,0x63,0x33,0x18,0x0C,0x66,0x63,0x00], // '%'
    [0x1C,0x36,0x1C,0x6E,0x3B,0x33,0x6E,0x00], // '&'
    [0x06,0x06,0x03,0x00,0x00,0x00,0x00,0x00], // '\''
    [0x18,0x0C,0x06,0x06,0x06,0x0C,0x18,0x00], // '('
    [0x06,0x0C,0x18,0x18,0x18,0x0C,0x06,0x00], // ')'
    [0x00,0x66,0x3C,0xFF,0x3C,0x66,0x00,0x00], // '*'
    [0x00,0x0C,0x0C,0x3F,0x0C,0x0C,0x00,0x00], // '+'
    [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x06], // ','
    [0x00,0x00,0x00,0x3F,0x00,0x00,0x00,0x00], // '-'
    [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x00], // '.'
    [0x60,0x30,0x18,0x0C,0x06,0x03,0x01,0x00], // '/'
    [0x3E,0x63,0x73,0x7B,0x6F,0x67,0x3E,0x00], // '0'
    [0x0C,0x0E,0x0C,0x0C,0x0C,0x0C,0x3F,0x00], // '1'
    [0x1E,0x33,0x30,0x1C,0x06,0x33,0x3F,0x00], // '2'
    [0x1E,0x33,0x30,0x1C,0x30,0x33,0x1E,0x00], // '3'
    [0x38,0x3C,0x36,0x33,0x7F,0x30,0x78,0x00], // '4'
    [0x3F,0x03,0x1F,0x30,0x30,0x33,0x1E,0x00], // '5'
    [0x1C,0x06,0x03,0x1F,0x33,0x33,0x1E,0x00], // '6'
    [0x3F,0x33,0x30,0x18,0x0C,0x0C,0x0C,0x00], // '7'
    [0x1E,0x33,0x33,0x1E,0x33,0x33,0x1E,0x00], // '8'
    [0x1E,0x33,0x33,0x3E,0x30,0x18,0x0E,0x00], // '9'
    [0x00,0x0C,0x0C,0x00,0x00,0x0C,0x0C,0x00], // ':'
    [0x00,0x0C,0x0C,0x00,0x00,0x0C,0x0C,0x06], // ';'
    [0x18,0x0C,0x06,0x03,0x06,0x0C,0x18,0x00], // '<'
    [0x00,0x00,0x3F,0x00,0x00,0x3F,0x00,0x00], // '='
    [0x06,0x0C,0x18,0x30,0x18,0x0C,0x06,0x00], // '>'
    [0x1E,0x33,0x30,0x18,0x0C,0x00,0x0C,0x00], // '?'
    [0x3E,0x63,0x7B,0x7B,0x7B,0x03,0x1E,0x00], // '@'
    [0x0C,0x1E,0x33,0x33,0x3F,0x33,0x33,0x00], // 'A'
    [0x3F,0x66,0x66,0x3E,0x66,0x66,0x3F,0x00], // 'B'
    [0x3C,0x66,0x03,0x03,0x03,0x66,0x3C,0x00], // 'C'
    [0x1F,0x36,0x66,0x66,0x66,0x36,0x1F,0x00], // 'D'
    [0x7F,0x46,0x16,0x1E,0x16,0x46,0x7F,0x00], // 'E'
    [0x7F,0x46,0x16,0x1E,0x16,0x06,0x0F,0x00], // 'F'
    [0x3C,0x66,0x03,0x03,0x73,0x66,0x7C,0x00], // 'G'
    [0x33,0x33,0x33,0x3F,0x33,0x33,0x33,0x00], // 'H'
    [0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00], // 'I'
    [0x78,0x30,0x30,0x30,0x33,0x33,0x1E,0x00], // 'J'
    [0x67,0x66,0x36,0x1E,0x36,0x66,0x67,0x00], // 'K'
    [0x0F,0x06,0x06,0x06,0x46,0x66,0x7F,0x00], // 'L'
    [0x63,0x77,0x7F,0x7F,0x6B,0x63,0x63,0x00], // 'M'
    [0x63,0x67,0x6F,0x7B,0x73,0x63,0x63,0x00], // 'N'
    [0x1C,0x36,0x63,0x63,0x63,0x36,0x1C,0x00], // 'O'
    [0x3F,0x66,0x66,0x3E,0x06,0x06,0x0F,0x00], // 'P'
    [0x1E,0x33,0x33,0x33,0x3B,0x1E,0x38,0x00], // 'Q'
    [0x3F,0x66,0x66,0x3E,0x36,0x66,0x67,0x00], // 'R'
    [0x1E,0x33,0x07,0x0E,0x38,0x33,0x1E,0x00], // 'S'
    [0x3F,0x2D,0x0C,0x0C,0x0C,0x0C,0x1E,0x00], // 'T'
    [0x33,0x33,0x33,0x33,0x33,0x33,0x3F,0x00], // 'U'
    [0x33,0x33,0x33,0x33,0x33,0x1E,0x0C,0x00], // 'V'
    [0x63,0x63,0x63,0x6B,0x7F,0x77,0x63,0x00], // 'W'
    [0x63,0x63,0x36,0x1C,0x1C,0x36,0x63,0x00], // 'X'
    [0x33,0x33,0x33,0x1E,0x0C,0x0C,0x1E,0x00], // 'Y'
    [0x7F,0x63,0x31,0x18,0x4C,0x66,0x7F,0x00], // 'Z'
    [0x1E,0x06,0x06,0x06,0x06,0x06,0x1E,0x00], // '['
    [0x03,0x06,0x0C,0x18,0x30,0x60,0x40,0x00], // '\'
    [0x1E,0x18,0x18,0x18,0x18,0x18,0x1E,0x00], // ']'
    [0x08,0x1C,0x36,0x63,0x00,0x00,0x00,0x00], // '^'
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xFF], // '_'
    [0x0C,0x0C,0x18,0x00,0x00,0x00,0x00,0x00], // '`'
    [0x00,0x00,0x1E,0x30,0x3E,0x33,0x6E,0x00], // 'a'
    [0x07,0x06,0x06,0x3E,0x66,0x66,0x3B,0x00], // 'b'
    [0x00,0x00,0x1E,0x33,0x03,0x33,0x1E,0x00], // 'c'
    [0x38,0x30,0x30,0x3e,0x33,0x33,0x6E,0x00], // 'd'
    [0x00,0x00,0x1E,0x33,0x3f,0x03,0x1E,0x00], // 'e'
    [0x1C,0x36,0x06,0x0f,0x06,0x06,0x0F,0x00], // 'f'
    [0x00,0x00,0x6E,0x33,0x33,0x3E,0x30,0x1F], // 'g'
    [0x07,0x06,0x36,0x6E,0x66,0x66,0x67,0x00], // 'h'
    [0x0C,0x00,0x0E,0x0C,0x0C,0x0C,0x1E,0x00], // 'i'
    [0x30,0x00,0x30,0x30,0x30,0x33,0x33,0x1E], // 'j'
    [0x07,0x06,0x66,0x36,0x1E,0x36,0x67,0x00], // 'k'
    [0x0E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00], // 'l'
    [0x00,0x00,0x33,0x7F,0x7F,0x6B,0x63,0x00], // 'm'
    [0x00,0x00,0x1F,0x33,0x33,0x33,0x33,0x00], // 'n'
    [0x00,0x00,0x1E,0x33,0x33,0x33,0x1E,0x00], // 'o'
    [0x00,0x00,0x3B,0x66,0x66,0x3E,0x06,0x0F], // 'p'
    [0x00,0x00,0x6E,0x33,0x33,0x3E,0x30,0x78], // 'q'
    [0x00,0x00,0x3B,0x6E,0x66,0x06,0x0F,0x00], // 'r'
    [0x00,0x00,0x3E,0x03,0x1E,0x30,0x1F,0x00], // 's'
    [0x08,0x0C,0x3E,0x0C,0x0C,0x2C,0x18,0x00], // 't'
    [0x00,0x00,0x33,0x33,0x33,0x33,0x6E,0x00], // 'u'
    [0x00,0x00,0x33,0x33,0x33,0x1E,0x0C,0x00], // 'v'
    [0x00,0x00,0x63,0x6B,0x7F,0x7F,0x36,0x00], // 'w'
    [0x00,0x00,0x63,0x36,0x1C,0x36,0x63,0x00], // 'x'
    [0x00,0x00,0x33,0x33,0x33,0x3E,0x30,0x1F], // 'y'
    [0x00,0x00,0x3F,0x19,0x0C,0x26,0x3F,0x00], // 'z'
    [0x38,0x0C,0x0C,0x07,0x0C,0x0C,0x38,0x00], // '{'
    [0x18,0x18,0x18,0x00,0x18,0x18,0x18,0x00], // '|'
    [0x07,0x0C,0x0C,0x38,0x0C,0x0C,0x07,0x00], // '}'
    [0x6E,0x3B,0x00,0x00,0x00,0x00,0x00,0x00], // '~'
];

/// Draw a single character directly into the framebuffer.
/// `fb_ptr` is the virtual (HHDM-mapped) linear framebuffer address.
/// `pitch` is bytes per row, `fg`/`bg` are packed 32-bit pixel values.
unsafe fn panic_draw_char(
    fb_ptr: *mut u32,
    pitch: usize,   // in bytes
    x: usize, y: usize,
    ch: u8, fg: u32, bg: u32,
    width: usize, height: usize,
) {
    if ch < 0x20 || ch > 0x7E { return; }
    let glyph = &PANIC_FONT[(ch - 0x20) as usize];
    for row in 0..8 {
        let py = y + row;
        if py >= height { break; }
        for col in 0..8 {
            let px = x + col;
            if px >= width { continue; }
            let bit = (glyph[row] >> (7 - col)) & 1;
            let color = if bit != 0 { fg } else { bg };
            // pitch is in bytes, fb_ptr is *mut u32 (4 bytes each)
            let pixel_ptr = fb_ptr.add(py * (pitch / 4) + px);
            core::ptr::write_volatile(pixel_ptr, color);
        }
    }
}

/// Write a string to the framebuffer at (x, y) using the 8x8 panic font.
unsafe fn panic_draw_str(
    fb_ptr: *mut u32, pitch: usize,
    x: usize, y: usize,
    s: &str, fg: u32, bg: u32,
    width: usize, height: usize,
) -> usize {  // returns new X position
    let mut cx = x;
    for byte in s.bytes() {
        panic_draw_char(fb_ptr, pitch, cx, y, byte, fg, bg, width, height);
        cx += 8;
        if cx + 8 > width { break; }
    }
    cx
}

/// Write a hex number to framebuffer.
unsafe fn panic_draw_hex(
    fb_ptr: *mut u32, pitch: usize,
    x: usize, y: usize,
    mut val: u64, fg: u32, bg: u32,
    width: usize, height: usize,
) -> usize {
    let hex = b"0123456789ABCDEF";
    let mut buf = [0u8; 18]; // "0x" + 16 hex digits
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        buf[17 - i] = hex[(val & 0xF) as usize];
        val >>= 4;
    }
    let s = core::str::from_utf8_unchecked(&buf);
    panic_draw_str(fb_ptr, pitch, x, y, s, fg, bg, width, height)
}

/// Kernel panic screen — draws directly to VRAM, bypassing compositor.
///
/// Shows a dark red background with white text describing the fault.
/// `msg`  — short description (e.g., "#GP" or "#PF")
/// `rip`  — faulting RIP
/// `cs`   — code segment at fault
/// `err`  — error code (0 for exceptions without error codes)
/// `cr2`  — CR2 register (faulting address for #PF; 0 otherwise)
pub unsafe fn panic_screen(msg: &str, rip: u64, cs: u64, err: u64, cr2: u64) {
    use core::sync::atomic::Ordering::Relaxed;

    let fb_phys = FRAMEBUFFER_INFO.load(Relaxed);
    let fb_w    = FRAMEBUFFER_WIDTH.load(Relaxed);
    let fb_h    = FRAMEBUFFER_HEIGHT.load(Relaxed);
    let pitch   = FRAMEBUFFER_PITCH.load(Relaxed);

    if fb_phys == 0 || fb_w == 0 { return; }

    // Map to HHDM virtual address via phys_to_virt (framebuffer is mapped at kernel init)
    let fb_virt = folkering_kernel::phys_to_virt(fb_phys) as *mut u32;

    // Background: dark red (#8B0000)
    let bg: u32 = 0x008B_0000;
    let fg: u32 = 0x00FF_FFFF;   // white
    let yellow: u32 = 0x00FF_FF00;

    // Fill entire screen with dark red
    let total_pixels = (pitch / 4) * fb_h;
    for i in 0..total_pixels {
        core::ptr::write_volatile(fb_virt.add(i), bg);
    }

    let mut y = 20usize;
    let x0 = 20usize;

    // Title
    panic_draw_str(fb_virt, pitch, x0, y, "*** FOLKERING OS KERNEL PANIC ***", yellow, bg, fb_w, fb_h);
    y += 16;
    panic_draw_str(fb_virt, pitch, x0, y, msg, fg, bg, fb_w, fb_h);
    y += 20;

    // Register dump
    let nx = panic_draw_str(fb_virt, pitch, x0, y, "RIP: ", fg, bg, fb_w, fb_h);
    panic_draw_hex(fb_virt, pitch, nx, y, rip, yellow, bg, fb_w, fb_h);
    y += 12;

    let nx = panic_draw_str(fb_virt, pitch, x0, y, "CS:  ", fg, bg, fb_w, fb_h);
    panic_draw_hex(fb_virt, pitch, nx, y, cs, yellow, bg, fb_w, fb_h);
    y += 12;

    let nx = panic_draw_str(fb_virt, pitch, x0, y, "ERR: ", fg, bg, fb_w, fb_h);
    panic_draw_hex(fb_virt, pitch, nx, y, err, yellow, bg, fb_w, fb_h);
    y += 12;

    if cr2 != 0 {
        let nx = panic_draw_str(fb_virt, pitch, x0, y, "CR2: ", fg, bg, fb_w, fb_h);
        panic_draw_hex(fb_virt, pitch, nx, y, cr2, yellow, bg, fb_w, fb_h);
        y += 12;
    }

    let marker = folkering_kernel::arch::x86_64::syscall::get_debug_marker();
    let nx = panic_draw_str(fb_virt, pitch, x0, y, "MARKER: ", fg, bg, fb_w, fb_h);
    panic_draw_hex(fb_virt, pitch, nx, y, marker, yellow, bg, fb_w, fb_h);
    y += 20;

    panic_draw_str(fb_virt, pitch, x0, y, "System halted. Check serial log for details.", fg, bg, fb_w, fb_h);
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
    // IMPORTANT: Use `sym irq_timer` in inline asm to get the TRUE machine code
    // address of the naked function. Rust function pointers for naked functions
    // may point 28 bytes into the function body (past the RPL check), causing
    // the kernel-mode path to be entered instead of the userspace-path prologue.
    let irq_timer_addr: u64;
    core::arch::asm!(
        "lea {0}, [rip + {timer}]",
        out(reg) irq_timer_addr,
        timer = sym irq_timer,
        options(nostack, readonly)
    );
    serial_write("[IDT] irq_timer sym addr: ");
    write_hex(irq_timer_addr);
    serial_write(", fn ptr: ");
    write_hex(irq_timer as *const () as u64);
    serial_write("\n");
    IDT[32].set_handler_addr(irq_timer_addr); // Timer

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
