//! x86_64 Architecture Support

pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod apic;
pub mod acpi;
pub mod syscall;
pub mod usermode;
pub mod cpu_freq;
pub mod cpu_local;
pub mod interrupt_frame;
pub mod pat;
pub mod pic;
pub mod ioapic;
pub mod smp;

// Re-export with module-specific names
pub use gdt::init as gdt_init;
pub use idt::init as idt_init;
pub use interrupts::enable as interrupts_enable;
pub use apic::{init as apic_init, enable_timer, disable_timer, tick, send_eoi, get_ticks};
pub use acpi::init as acpi_init;
pub use syscall::init as syscall_init;
pub use cpu_freq::{init as cpu_freq_init, set_cpu_freq, set_power_save, set_base, set_turbo, current_frequency};
pub use pat::init as pat_init;
pub use pic::{init as pic_init, enable_irq, disable_irq, send_eoi as pic_send_eoi};
pub use ioapic::{init as ioapic_init, enable_irq as ioapic_enable_irq, enable_irq_level as ioapic_enable_irq_level};
