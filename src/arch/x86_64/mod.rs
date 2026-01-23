//! x86_64 Architecture Support

pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod apic;
pub mod acpi;
pub mod syscall;
pub mod usermode;

// Re-export with module-specific names
pub use gdt::init as gdt_init;
pub use idt::init as idt_init;
pub use interrupts::enable as interrupts_enable;
pub use apic::init as apic_init;
pub use acpi::init as acpi_init;
pub use syscall::init as syscall_init;
