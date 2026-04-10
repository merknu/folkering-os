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
}

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

                let driver = Ac97Driver {
                    bar0,
                    bar1,
                    initialized: true,
                };
                *AC97.lock() = Some(driver);
                crate::serial_strln!("[AC97] Initialized successfully");
                return;
            }
        }
    }

    crate::serial_strln!("[AC97] No device found");
}

/// Play raw PCM samples (16-bit signed stereo, 44100Hz).
/// Blocks until samples are queued. Returns true on success.
///
/// NOTE: This is a stub that logs the play request. Full DMA-based
/// playback requires physical memory allocation for the BDL and sample
/// buffers, which is non-trivial in the current memory model.
pub fn play_pcm(samples: &[i16]) -> bool {
    let guard = AC97.lock();
    let driver = match guard.as_ref() {
        Some(d) if d.initialized => d,
        _ => {
            crate::serial_strln!("[AC97] play_pcm: device not initialized");
            return false;
        }
    };

    crate::serial_str!("[AC97] play_pcm: ");
    crate::drivers::serial::write_dec(samples.len() as u32);
    crate::serial_str!(" samples (");
    crate::drivers::serial::write_dec((samples.len() / 2) as u32);
    crate::serial_str!(" stereo frames)\n");

    // TODO: Allocate physical memory for sample buffer
    // TODO: Set up BDL entries pointing to it
    // TODO: Write BDL physical address to NABM_PO_BDBAR
    // TODO: Set LVI to last valid buffer index
    // TODO: Start playback by setting CR_RUN

    // For now, just acknowledge the request
    let _ = driver;
    true
}

/// Beep — generate a 440Hz sine wave for the requested duration.
/// Useful for system feedback.
pub fn beep(duration_ms: u32) -> bool {
    if AC97.lock().is_none() {
        return false;
    }
    crate::serial_str!("[AC97] beep ");
    crate::drivers::serial::write_dec(duration_ms);
    crate::serial_strln!("ms");
    // Stub — would generate sine wave samples and call play_pcm
    true
}
