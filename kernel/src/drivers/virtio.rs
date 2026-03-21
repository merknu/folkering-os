//! VirtIO Transport Layer (Legacy PCI)
//!
//! Implements the split virtqueue format used by VirtIO legacy (0.9.5) devices.
//! The queue consists of three parts:
//! - Descriptor table: array of buffer descriptors
//! - Available ring: driver→device (what's available for device to consume)
//! - Used ring: device→driver (what device has finished with)

use core::sync::atomic::{AtomicU16, Ordering, fence};

/// Virtqueue descriptor flags
pub const VRING_DESC_F_NEXT: u16 = 1;     // Buffer continues via `next` field
pub const VRING_DESC_F_WRITE: u16 = 2;    // Buffer is device-writable (device writes here)

/// Virtqueue descriptor (16 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VirtqDesc {
    /// Physical address of the buffer
    pub addr: u64,
    /// Length of the buffer in bytes
    pub len: u32,
    /// Descriptor flags (NEXT, WRITE)
    pub flags: u16,
    /// Next descriptor index if NEXT flag set
    pub next: u16,
}

/// Available ring element layout
/// The available ring sits right after the descriptor table in memory.
/// Layout: flags(u16) + idx(u16) + ring[queue_size](u16) + used_event(u16)
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    // ring[queue_size] follows (u16 each)
    // used_event follows ring
}

/// Used ring element
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VirtqUsedElem {
    /// Descriptor chain head index
    pub id: u32,
    /// Number of bytes device wrote
    pub len: u32,
}

/// Used ring layout
/// Layout: flags(u16) + idx(u16) + ring[queue_size](VirtqUsedElem) + avail_event(u16)
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    // ring[queue_size] follows (VirtqUsedElem each)
}

/// Split virtqueue
pub struct Virtqueue {
    /// Virtual address of descriptor table
    pub desc_virt: usize,
    /// Virtual address of available ring
    pub avail_virt: usize,
    /// Virtual address of used ring
    pub used_virt: usize,
    /// Physical address of the queue base (for device)
    pub queue_phys: usize,
    /// Queue size (number of descriptors)
    pub queue_size: u16,
    /// Next free descriptor index
    pub free_head: u16,
    /// Number of free descriptors
    pub num_free: u16,
    /// Last seen used index (for detecting new completions)
    pub last_used_idx: u16,
    /// Next available ring index (driver side)
    next_avail: u16,
}

impl Virtqueue {
    /// Allocate and initialize a virtqueue
    ///
    /// `queue_size` must be a power of 2 (as required by VirtIO spec).
    /// Returns the queue and its physical base address (for Queue PFN register).
    pub fn new(queue_size: u16) -> Option<Self> {
        let qs = queue_size as usize;

        // Calculate memory layout (legacy VirtIO):
        // Descriptors: 16 * queue_size bytes
        // Available ring: 6 + 2 * queue_size bytes
        // Padding to next page boundary
        // Used ring: 6 + 8 * queue_size bytes
        let desc_size = 16 * qs;
        let avail_size = 6 + 2 * qs;
        let avail_end = desc_size + avail_size;
        let used_offset = (avail_end + 4095) & !4095; // Align to page
        let used_size = 6 + 8 * qs;
        let total_size = used_offset + used_size;
        let pages_needed = (total_size + 4095) / 4096;

        // Allocate contiguous physical pages for the virtqueue.
        // alloc_page() goes through bootstrap allocator which is reliable.
        // We allocate pages one-by-one and verify contiguity.
        let alloc_pages_count = pages_needed.next_power_of_two().max(1);

        let first_page = crate::memory::physical::alloc_page()?;
        let mut all_contiguous = true;

        for i in 1..alloc_pages_count {
            let page = crate::memory::physical::alloc_page()?;
            if page != first_page + i * 4096 {
                all_contiguous = false;
                crate::serial_str!("[VIRTQ] Page ");
                crate::drivers::serial::write_dec(i as u32);
                crate::serial_str!(" not contiguous: expected ");
                crate::drivers::serial::write_hex((first_page + i * 4096) as u64);
                crate::serial_str!(", got ");
                crate::drivers::serial::write_hex(page as u64);
                crate::drivers::serial::write_newline();
            }
        }

        if !all_contiguous {
            crate::serial_strln!("[VIRTQ] WARNING: Pages not physically contiguous!");
            crate::serial_strln!("[VIRTQ] VirtIO legacy requires contiguous queue memory");
            // Continue anyway — often works if pages are close
        }

        let queue_phys = first_page;
        let queue_virt = crate::phys_to_virt(queue_phys);

        // Zero all memory
        unsafe {
            core::ptr::write_bytes(queue_virt as *mut u8, 0, alloc_pages_count * 4096);
        }

        let desc_virt = queue_virt;
        let avail_virt = queue_virt + desc_size;
        let used_virt = queue_virt + used_offset;

        // Initialize free descriptor chain
        unsafe {
            let descs = desc_virt as *mut VirtqDesc;
            for i in 0..qs {
                (*descs.add(i)).next = (i + 1) as u16;
            }
            // Last descriptor points to itself (sentinel)
            (*descs.add(qs - 1)).next = 0xFFFF;
        }

        crate::serial_str!("[VIRTQ] Queue memory: pages_needed=");
        crate::drivers::serial::write_dec(pages_needed as u32);
        crate::serial_str!(", used_offset=");
        crate::drivers::serial::write_dec(used_offset as u32);
        crate::drivers::serial::write_newline();

        crate::serial_str!("[VIRTQ] Allocated queue: size=");
        crate::drivers::serial::write_dec(queue_size as u32);
        crate::serial_str!(", phys=");
        crate::drivers::serial::write_hex(queue_phys as u64);
        crate::serial_str!(", pages=");
        crate::drivers::serial::write_dec(pages_needed as u32);
        crate::drivers::serial::write_newline();

        Some(Virtqueue {
            desc_virt,
            avail_virt,
            used_virt,
            queue_phys,
            queue_size,
            free_head: 0,
            num_free: queue_size,
            last_used_idx: 0,
            next_avail: 0,
        })
    }

    /// Get a pointer to descriptor `i`
    pub fn desc(&self, i: u16) -> *mut VirtqDesc {
        unsafe { (self.desc_virt as *mut VirtqDesc).add(i as usize) }
    }

    /// Get the available ring's `ring[i]` entry
    fn avail_ring_ptr(&self, i: u16) -> *mut u16 {
        // avail layout: flags(2) + idx(2) + ring[](2 each)
        unsafe {
            (self.avail_virt as *mut u16).add(2 + i as usize)
        }
    }

    /// Get the available ring's `idx` field
    fn avail_idx_ptr(&self) -> *mut u16 {
        unsafe { (self.avail_virt as *mut u16).add(1) }
    }

    /// Get the available ring's `flags` field
    fn avail_flags_ptr(&self) -> *mut u16 {
        self.avail_virt as *mut u16
    }

    /// Get the used ring's `idx` field
    fn used_idx(&self) -> u16 {
        unsafe { core::ptr::read_volatile((self.used_virt as *const u16).add(1)) }
    }

    /// Get used ring element at index i
    fn used_elem(&self, i: u16) -> VirtqUsedElem {
        // used layout: flags(2) + idx(2) + ring[](8 each)
        let base = self.used_virt + 4; // skip flags + idx
        let elem_ptr = (base + (i as usize) * 8) as *const VirtqUsedElem;
        unsafe { core::ptr::read_volatile(elem_ptr) }
    }

    /// Allocate a descriptor from the free list
    pub fn alloc_desc(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        let idx = self.free_head;
        let desc = unsafe { &*self.desc(idx) };
        self.free_head = desc.next;
        self.num_free -= 1;
        Some(idx)
    }

    /// Free a descriptor back to the free list
    pub fn free_desc(&mut self, idx: u16) {
        unsafe {
            (*self.desc(idx)).next = self.free_head;
        }
        self.free_head = idx;
        self.num_free += 1;
    }

    /// Free a chain of descriptors starting from head
    pub fn free_chain(&mut self, mut head: u16) {
        loop {
            let desc = unsafe { &*self.desc(head) };
            let has_next = desc.flags & VRING_DESC_F_NEXT != 0;
            let next = desc.next;
            self.free_desc(head);
            if has_next {
                head = next;
            } else {
                break;
            }
        }
    }

    /// Submit a descriptor chain to the available ring
    pub fn submit(&mut self, head: u16) {
        let avail_idx = self.next_avail;
        let ring_idx = avail_idx % self.queue_size;

        // Write descriptor head index to available ring
        unsafe {
            core::ptr::write_volatile(self.avail_ring_ptr(ring_idx), head);
        }

        // Memory barrier: ensure ring entry is visible before updating idx
        fence(Ordering::Release);

        // Increment available index
        self.next_avail = avail_idx.wrapping_add(1);
        unsafe {
            core::ptr::write_volatile(self.avail_idx_ptr(), self.next_avail);
        }

        // Memory barrier: ensure idx is visible to device
        fence(Ordering::SeqCst);
    }

    /// Check if device has completed any requests
    pub fn has_used(&self) -> bool {
        fence(Ordering::Acquire);
        self.used_idx() != self.last_used_idx
    }

    /// Pop a completed request from the used ring
    /// Returns (descriptor chain head, bytes written) or None
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        fence(Ordering::Acquire);
        let used_idx = self.used_idx();
        if used_idx == self.last_used_idx {
            return None;
        }

        let ring_idx = self.last_used_idx % self.queue_size;
        let elem = self.used_elem(ring_idx);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        Some((elem.id as u16, elem.len))
    }

    /// Disable interrupts from this queue (set NO_INTERRUPT flag in avail ring)
    pub fn disable_interrupts(&self) {
        unsafe {
            core::ptr::write_volatile(self.avail_flags_ptr(), 1);
        }
    }

    /// Enable interrupts from this queue
    pub fn enable_interrupts(&self) {
        unsafe {
            core::ptr::write_volatile(self.avail_flags_ptr(), 0);
        }
    }
}
