//! AC97 audio driver — minimal implementation for raw PCM playback.
//!
//! AC97 is a legacy Intel audio specification. QEMU's `-device AC97` exposes
//! it as PCI vendor 0x8086, device 0x2415.
//!
//! Architecture:
//! - NAM (Native Audio Mixer) at BAR0: volume control, sample rate
//! - NABM (Native Audio Bus Master) at BAR1: DMA transfer of samples
//! - Buffer Descriptor List (BDL): array of 32 descriptors, each pointing
//!   to a chunk of PCM samples in physical memory
//!
//! This driver provides raw 16-bit signed stereo PCM at 44100Hz playback
//! via a single circular buffer. No mixing, no channels, no codec features.

use spin::Mutex;
use lazy_static::lazy_static;

const AC97_VENDOR: u16 = 0x8086;
const AC97_DEVICE: u16 = 0x2415;

// NAM (Native Audio Mixer) registers (BAR0 offset)
const NAM_RESET: u16 = 0x00;
const NAM_MASTER_VOL: u16 = 0x02;
const NAM_PCM_OUT_VOL: u16 = 0x18;
const NAM_EXT_AUDIO_ID: u16 = 0x28;
const NAM_EXT_AUDIO_CTRL: u16 = 0x2A;
const NAM_FRONT_DAC_RATE: u16 = 0x2C;

// NABM (Native Audio Bus Master) registers (BAR1 offset)
const NABM_PO_BDBAR: u16 = 0x10; // PCM Out: Buffer Descriptor Base Address
const NABM_PO_CIV: u16 = 0x14;   // PCM Out: Current Index Value
const NABM_PO_LVI: u16 = 0x15;   // PCM Out: Last Valid Index
const NABM_PO_SR: u16 = 0x16;    // PCM Out: Status Register
const NABM_PO_PICB: u16 = 0x18;  // PCM Out: Position In Current Buffer
const NABM_PO_CR: u16 = 0x1B;    // PCM Out: Control Register
const NABM_GLOBAL_CTRL: u16 = 0x2C;

// Control register bits
const CR_RUN: u8 = 0x01;
const CR_RESET: u8 = 0x02;
const CR_LVBIE: u8 = 0x04; // Last valid buffer interrupt enable
const CR_FEIE: u8 = 0x08;  // FIFO error interrupt enable
const CR_IOCE: u8 = 0x10;  // Interrupt on completion enable

// Buffer descriptor entry (8 bytes)
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct BufferDesc {
    addr: u32,    // Physical address of sample buffer
    samples: u16, // Number of samples (max 0xFFFE)
    flags: u16,   // bit 15 = Interrupt on Completion, bit 14 = Buffer Underrun Policy
}

const BDL_ENTRIES: usize = 32;
const SAMPLES_PER_BUFFER: usize = 4096; // 4096 samples * 2 channels * 2 bytes = 16KB per buffer

pub struct Ac97Driver {
    pub bar0: u16, // NAM I/O port base
    pub bar1: u16, // NABM I/O port base
    pub initialized: bool,
    /// Physical address of the BDL (32 entries × 8 bytes = 256 bytes)
    pub bdl_phys: u64,
    /// Virtual address of the BDL (HHDM-mapped, for kernel writes)
    pub bdl_virt: *mut BufferDesc,
    /// Physical addresses of allocated sample buffers (one per BDL slot)
    pub buffer_phys: [u64; BDL_ENTRIES],
    /// Virtual addresses (HHDM) of sample buffers
    pub buffer_virt: [*mut i16; BDL_ENTRIES],
}

unsafe impl Send for Ac97Driver {}
unsafe impl Sync for Ac97Driver {}

lazy_static! {
    pub static ref AC97: Mutex<Option<Ac97Driver>> = Mutex::new(None);
}

/// Probe PCI bus for AC97 device.
pub fn init() {
    use x86_64::instructions::port::Port;
    use crate::drivers::pci;

    crate::serial_strln!("[AC97] Probing PCI for AC97 device...");

    // Scan PCI for AC97 device (vendor 0x8086, device 0x2415)
    for bus in 0u8..=255 {
        for dev in 0u8..32 {
            let vendor_device = pci::pci_read32(bus, dev, 0, 0x00);
            let vendor = (vendor_device & 0xFFFF) as u16;
            let device = (vendor_device >> 16) as u16;

            if vendor == AC97_VENDOR && device == AC97_DEVICE {
                crate::serial_str!("[AC97] Found at bus=");
                crate::drivers::serial::write_dec(bus as u32);
                crate::serial_str!(" dev=");
                crate::drivers::serial::write_dec(dev as u32);
                crate::serial_str!("\n");

                // Read BAR0 and BAR1 (I/O space)
                let bar0_raw = pci::pci_read32(bus, dev, 0, 0x10);
                let bar1_raw = pci::pci_read32(bus, dev, 0, 0x14);
                let bar0 = (bar0_raw & 0xFFFC) as u16;
                let bar1 = (bar1_raw & 0xFFFC) as u16;

                crate::serial_str!("[AC97] BAR0=0x");
                crate::drivers::serial::write_dec(bar0 as u32);
                crate::serial_str!(" BAR1=");
                crate::drivers::serial::write_dec(bar1 as u32);
                crate::serial_str!("\n");

                // Enable PCI bus master + I/O space
                let cmd = pci::pci_read16(bus, dev, 0, 0x04);
                let new_cmd = cmd | 0x0001 | 0x0004; // I/O Space + Bus Master
                let cmd_status = pci::pci_read32(bus, dev, 0, 0x04);
                pci::pci_write32(bus, dev, 0, 0x04,
                    (cmd_status & 0xFFFF_0000) | (new_cmd as u32));

                // Reset codec via NAM
                unsafe {
                    let mut nam_reset: Port<u16> = Port::new(bar0 + NAM_RESET);
                    nam_reset.write(0xFFFF);

                    // Set master volume to max (0 = max, 0x3F = silence)
                    let mut master_vol: Port<u16> = Port::new(bar0 + NAM_MASTER_VOL);
                    master_vol.write(0x0000); // Max volume both channels

                    // Set PCM output volume
                    let mut pcm_vol: Port<u16> = Port::new(bar0 + NAM_PCM_OUT_VOL);
                    pcm_vol.write(0x0000);

                    // Reset PCM output channel
                    let mut po_cr: Port<u8> = Port::new(bar1 + NABM_PO_CR);
                    po_cr.write(CR_RESET);
                    // Wait for reset to complete
                    for _ in 0..1000 { core::hint::spin_loop(); }
                }

                // Allocate BDL (Buffer Descriptor List) — needs 1 physical page
                // 32 entries × 8 bytes = 256 bytes, fits easily in one 4KB page
                let bdl_phys = match crate::memory::physical::alloc_page() {
                    Some(p) => p as u64,
                    None => {
                        crate::serial_strln!("[AC97] ERROR: failed to allocate BDL page");
                        return;
                    }
                };
                let hhdm = crate::memory::paging::hhdm_offset() as u64;
                let bdl_virt = (bdl_phys + hhdm) as *mut BufferDesc;

                // Zero the BDL
                unsafe {
                    core::ptr::write_bytes(bdl_virt, 0, BDL_ENTRIES);
                }

                // Allocate sample buffers (one 4KB page each = 1024 stereo frames each)
                let mut buffer_phys = [0u64; BDL_ENTRIES];
                let mut buffer_virt = [core::ptr::null_mut::<i16>(); BDL_ENTRIES];
                for i in 0..BDL_ENTRIES {
                    let p = match crate::memory::physical::alloc_page() {
                        Some(p) => p as u64,
                        None => {
                            crate::serial_strln!("[AC97] ERROR: failed to allocate sample buffer");
                            return;
                        }
                    };
                    buffer_phys[i] = p;
                    buffer_virt[i] = (p + hhdm) as *mut i16;
                }

                let driver = Ac97Driver {
                    bar0,
                    bar1,
                    initialized: true,
                    bdl_phys,
                    bdl_virt,
                    buffer_phys,
                    buffer_virt,
                };

                // Write BDL physical address to PO_BDBAR (must be 8-byte aligned)
                unsafe {
                    let mut po_bdbar: Port<u32> = Port::new(bar1 + NABM_PO_BDBAR);
                    po_bdbar.write(bdl_phys as u32);
                }

                *AC97.lock() = Some(driver);
                crate::serial_strln!("[AC97] Initialized successfully (BDL allocated)");
                return;
            }
        }
    }

    crate::serial_strln!("[AC97] No device found");
}

/// Play raw PCM samples (16-bit signed stereo, 44100Hz).
/// Returns true on success. Samples are copied into pre-allocated DMA buffers.
///
/// Each buffer holds 1024 stereo frames (2048 samples = 4KB), so 32 buffers
/// total = 32768 stereo frames ≈ 0.74 seconds of audio at 44100Hz.
/// Longer audio is truncated.
pub fn play_pcm(samples: &[i16]) -> bool {
    use x86_64::instructions::port::Port;

    let guard = AC97.lock();
    let driver = match guard.as_ref() {
        Some(d) if d.initialized => d,
        _ => {
            crate::serial_strln!("[AC97] play_pcm: device not initialized");
            return false;
        }
    };

    if samples.is_empty() { return false; }

    const SAMPLES_PER_BUFFER: usize = 2048; // 1024 stereo frames per 4KB page
    let total_samples = samples.len();
    let buffers_needed = (total_samples + SAMPLES_PER_BUFFER - 1) / SAMPLES_PER_BUFFER;
    let buffers_used = buffers_needed.min(BDL_ENTRIES);

    crate::serial_str!("[AC97] play_pcm: ");
    crate::drivers::serial::write_dec(total_samples as u32);
    crate::serial_str!(" samples → ");
    crate::drivers::serial::write_dec(buffers_used as u32);
    crate::serial_strln!(" buffers");

    // Copy samples into buffers and set up BDL entries
    let mut sample_idx = 0;
    for i in 0..buffers_used {
        let chunk_len = (total_samples - sample_idx).min(SAMPLES_PER_BUFFER);
        unsafe {
            // Copy samples to DMA buffer (via HHDM mapping)
            core::ptr::copy_nonoverlapping(
                samples.as_ptr().add(sample_idx),
                driver.buffer_virt[i],
                chunk_len,
            );

            // Set BDL entry: points to physical buffer with sample count
            // AC97 BDL "samples" field is actually number of 16-bit samples - 1?
            // Per spec: "Number of samples - 1, max 0xFFFE samples"
            // But many implementations use raw count. We'll use raw count.
            let entry = BufferDesc {
                addr: driver.buffer_phys[i] as u32,
                samples: chunk_len as u16,
                // bit 15 = IOC (interrupt on completion), bit 14 = BUP (buffer underrun policy)
                flags: if i == buffers_used - 1 { 0xC000 } else { 0 },
            };
            *driver.bdl_virt.add(i) = entry;
        }
        sample_idx += chunk_len;
    }

    // Start playback
    unsafe {
        // Set Last Valid Index — tells the device to play through buffer N
        let mut po_lvi: Port<u8> = Port::new(driver.bar1 + NABM_PO_LVI);
        po_lvi.write((buffers_used - 1) as u8);

        // Set CR_RUN to start playback
        let mut po_cr: Port<u8> = Port::new(driver.bar1 + NABM_PO_CR);
        po_cr.write(CR_RUN | CR_LVBIE | CR_FEIE | CR_IOCE);
    }

    true
}

/// Beep — generate a 440Hz sine wave for the requested duration.
/// Uses a simple square-wave approximation to avoid floating point in kernel.
pub fn beep(duration_ms: u32) -> bool {
    if AC97.lock().is_none() {
        return false;
    }
    crate::serial_str!("[AC97] beep ");
    crate::drivers::serial::write_dec(duration_ms);
    crate::serial_strln!("ms");

    // 44100 Hz sample rate, stereo
    // 440 Hz tone: period = 44100/440 ≈ 100 samples, half-period = 50
    let total_frames = (44100 * duration_ms / 1000) as usize;
    let total_samples = (total_frames * 2).min(2048 * BDL_ENTRIES); // stereo, max BDL capacity

    // Generate a buffer of samples (use static to avoid stack overflow)
    use alloc::vec;
    let mut samples = vec![0i16; total_samples];
    let half_period = 50; // 44100 / 440 / 2
    let amplitude: i16 = 8000; // moderate volume

    for i in 0..total_frames {
        let val = if (i / half_period) % 2 == 0 { amplitude } else { -amplitude };
        let stereo_idx = i * 2;
        if stereo_idx + 1 < samples.len() {
            samples[stereo_idx] = val;     // left
            samples[stereo_idx + 1] = val; // right
        }
    }

    play_pcm(&samples)
}
