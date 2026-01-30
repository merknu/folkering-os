//! Boot Information Handoff
//!
//! Provides a mechanism to pass boot-time configuration from kernel to userspace.
//! The boot info page is mapped at a fixed virtual address accessible to the compositor.
//!
//! # Memory Layout
//!
//! The boot info page is mapped at `BOOT_INFO_VADDR` (0x200000) and contains:
//! - FolkeringBootInfo structure (read-only from userspace)
//! - Framebuffer configuration
//! - RSDP address for ACPI
//!
//! # Usage (Kernel)
//!
//! ```ignore
//! // During boot, after parsing Limine framebuffer response:
//! boot_info::init_framebuffer(fb_phys, width, height, pitch, bpp, ...);
//!
//! // When spawning compositor, map the boot info page:
//! boot_info::map_for_task(compositor_pml4);
//! ```
//!
//! # Usage (Userspace)
//!
//! ```ignore
//! let boot_info = unsafe { &*(0x200000 as *const FolkeringBootInfo) };
//! let fb_addr = boot_info.framebuffer.physical_address;
//! ```

use crate::memory::{paging, physical};
use spin::Mutex;

/// Fixed virtual address for boot info page in userspace
/// Note: Must not conflict with ELF load addresses (typically 0x200000+)
pub const BOOT_INFO_VADDR: usize = 0x0000_0000_0010_0000; // 1MB mark

/// Magic value to verify boot info integrity
pub const BOOT_INFO_MAGIC: u64 = 0x464F4C4B424F4F54; // "FOLKBOOT" in ASCII

/// Current ABI version
pub const BOOT_INFO_VERSION: u64 = 1;

/// Boot info flags
pub mod flags {
    /// Framebuffer is available
    pub const HAS_FRAMEBUFFER: u64 = 1 << 0;
    /// RSDP is available
    pub const HAS_RSDP: u64 = 1 << 1;
}

/// Framebuffer configuration passed to userspace.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FramebufferConfig {
    /// Physical address of the framebuffer
    pub physical_address: u64,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Bytes per scanline (may differ from width * bpp/8)
    pub pitch: u32,
    /// Bits per pixel (usually 32)
    pub bpp: u16,
    /// Memory model (1 = RGB)
    pub memory_model: u8,
    /// Red mask size in bits
    pub red_mask_size: u8,
    /// Red mask shift (bit position)
    pub red_mask_shift: u8,
    /// Green mask size in bits
    pub green_mask_size: u8,
    /// Green mask shift (bit position)
    pub green_mask_shift: u8,
    /// Blue mask size in bits
    pub blue_mask_size: u8,
    /// Blue mask shift (bit position)
    pub blue_mask_shift: u8,
    /// Reserved for alignment
    pub _reserved: [u8; 3],
}

/// Boot information structure shared with userspace.
///
/// This structure is placed at a fixed virtual address and provides
/// userspace services (like the compositor) with essential boot-time data.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FolkeringBootInfo {
    /// Magic number for verification (BOOT_INFO_MAGIC)
    pub magic: u64,
    /// ABI version (BOOT_INFO_VERSION)
    pub version: u64,
    /// Feature flags
    pub flags: u64,
    /// Framebuffer configuration
    pub framebuffer: FramebufferConfig,
    /// ACPI RSDP physical address (0 if not available)
    pub rsdp_address: u64,
    /// Physical address of the boot info page itself (for verification)
    pub self_phys_addr: u64,
    /// Reserved for future use
    pub _reserved: [u64; 8],
}

impl Default for FolkeringBootInfo {
    fn default() -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            flags: 0,
            framebuffer: FramebufferConfig::default(),
            rsdp_address: 0,
            self_phys_addr: 0,
            _reserved: [0; 8],
        }
    }
}

/// Global boot info state
struct BootInfoState {
    /// Physical address of the boot info page
    phys_addr: Option<usize>,
    /// The boot info data (stored here, then copied to physical page)
    data: FolkeringBootInfo,
}

static BOOT_INFO: Mutex<BootInfoState> = Mutex::new(BootInfoState {
    phys_addr: None,
    data: FolkeringBootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        flags: 0,
        framebuffer: FramebufferConfig {
            physical_address: 0,
            width: 0,
            height: 0,
            pitch: 0,
            bpp: 0,
            memory_model: 0,
            red_mask_size: 0,
            red_mask_shift: 0,
            green_mask_size: 0,
            green_mask_shift: 0,
            blue_mask_size: 0,
            blue_mask_shift: 0,
            _reserved: [0; 3],
        },
        rsdp_address: 0,
        self_phys_addr: 0,
        _reserved: [0; 8],
    },
});

/// Initialize the boot info page.
///
/// Allocates a physical page and initializes it with boot info data.
/// Must be called after physical memory manager is initialized.
pub fn init() -> Result<(), &'static str> {
    let mut state = BOOT_INFO.lock();

    // Allocate physical page for boot info
    let phys = physical::alloc_page().ok_or("Failed to allocate boot info page")?;
    state.phys_addr = Some(phys);
    state.data.self_phys_addr = phys as u64;

    // Write data to physical page
    let virt = crate::phys_to_virt(phys);
    unsafe {
        core::ptr::write(virt as *mut FolkeringBootInfo, state.data);
    }

    crate::serial_strln!("[BOOT_INFO] Boot info page allocated");
    crate::serial_str!("[BOOT_INFO] Physical address: ");
    crate::drivers::serial::write_hex(phys as u64);
    crate::drivers::serial::write_newline();

    Ok(())
}

/// Set framebuffer configuration.
///
/// Call this after parsing the Limine framebuffer response.
pub fn set_framebuffer(config: FramebufferConfig) {
    let mut state = BOOT_INFO.lock();
    state.data.framebuffer = config;
    state.data.flags |= flags::HAS_FRAMEBUFFER;

    // Update physical page if already allocated
    if let Some(phys) = state.phys_addr {
        let virt = crate::phys_to_virt(phys);
        unsafe {
            core::ptr::write(virt as *mut FolkeringBootInfo, state.data);
        }
    }

    crate::serial_strln!("[BOOT_INFO] Framebuffer config set:");
    crate::serial_str!("[BOOT_INFO]   Address: ");
    crate::drivers::serial::write_hex(config.physical_address);
    crate::drivers::serial::write_newline();
    crate::serial_str!("[BOOT_INFO]   Resolution: ");
    crate::drivers::serial::write_dec(config.width);
    crate::serial_str!("x");
    crate::drivers::serial::write_dec(config.height);
    crate::serial_str!(" @ ");
    crate::drivers::serial::write_dec(config.bpp as u32);
    crate::serial_strln!("bpp");
    crate::serial_str!("[BOOT_INFO]   Pitch: ");
    crate::drivers::serial::write_dec(config.pitch);
    crate::serial_strln!(" bytes/line");
}

/// Set RSDP address.
pub fn set_rsdp(addr: u64) {
    let mut state = BOOT_INFO.lock();
    state.data.rsdp_address = addr;
    if addr != 0 {
        state.data.flags |= flags::HAS_RSDP;
    }

    // Update physical page if already allocated
    if let Some(phys) = state.phys_addr {
        let virt = crate::phys_to_virt(phys);
        unsafe {
            core::ptr::write(virt as *mut FolkeringBootInfo, state.data);
        }
    }
}

/// Map the boot info page into a task's address space by task ID.
///
/// The page is mapped read-only at BOOT_INFO_VADDR.
///
/// # Arguments
/// * `task_id` - The task ID to map the boot info for
pub fn map_for_task(task_id: u32) -> Result<(), &'static str> {
    // Get the task's page table physical address
    let task = crate::task::task::get_task(task_id).ok_or("Task not found")?;
    let pml4_phys = task.lock().page_table_phys;

    map_for_pml4(pml4_phys)
}

/// Map the boot info page into a task's address space by PML4 physical address.
///
/// The page is mapped read-only at BOOT_INFO_VADDR.
///
/// # Arguments
/// * `pml4_phys` - Physical address of the task's PML4 page table
pub fn map_for_pml4(pml4_phys: u64) -> Result<(), &'static str> {
    let state = BOOT_INFO.lock();
    let phys = state.phys_addr.ok_or("Boot info not initialized")?;

    crate::drivers::serial::write_str("[BOOT_INFO] Mapping: pml4=");
    crate::drivers::serial::write_hex(pml4_phys);
    crate::drivers::serial::write_str(", virt=");
    crate::drivers::serial::write_hex(BOOT_INFO_VADDR as u64);
    crate::drivers::serial::write_str(", phys=");
    crate::drivers::serial::write_hex(phys as u64);
    crate::drivers::serial::write_newline();

    // Map as read-only, user-accessible, no-execute
    use x86_64::structures::paging::PageTableFlags as PTF;
    let flags = PTF::PRESENT
        .union(PTF::USER_ACCESSIBLE)
        .union(PTF::NO_EXECUTE);

    let result = paging::map_page_in_table(pml4_phys, BOOT_INFO_VADDR, phys, flags);

    if result.is_ok() {
        crate::drivers::serial::write_str("[BOOT_INFO] Mapped for task at virt ");
        crate::drivers::serial::write_hex(BOOT_INFO_VADDR as u64);
        crate::drivers::serial::write_newline();
        Ok(())
    } else {
        crate::drivers::serial::write_str("[BOOT_INFO] Map failed\n");
        Err("Failed to map boot info page")
    }
}

/// Get framebuffer physical address (for capability granting).
pub fn framebuffer_phys_addr() -> Option<u64> {
    let state = BOOT_INFO.lock();
    if state.data.flags & flags::HAS_FRAMEBUFFER != 0 {
        Some(state.data.framebuffer.physical_address)
    } else {
        None
    }
}

/// Get framebuffer size in bytes.
pub fn framebuffer_size() -> Option<usize> {
    let state = BOOT_INFO.lock();
    if state.data.flags & flags::HAS_FRAMEBUFFER != 0 {
        let fb = &state.data.framebuffer;
        Some(fb.pitch as usize * fb.height as usize)
    } else {
        None
    }
}

/// Get a copy of the current boot info data.
pub fn get_boot_info() -> FolkeringBootInfo {
    BOOT_INFO.lock().data
}
