//! ELF64 Binary Parser
//!
//! Parses ELF64 binaries for task spawning.
//! Validates binary format and extracts program segments.

/// ELF Magic number
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

/// ELF Class (32-bit or 64-bit)
const ELFCLASS64: u8 = 2;

/// ELF Data encoding (little-endian)
const ELFDATA2LSB: u8 = 1;

/// ELF Type (executable)
const ET_EXEC: u16 = 2;

/// ELF Machine (x86-64)
const EM_X86_64: u16 = 62;

/// Program header type: LOAD
const PT_LOAD: u32 = 1;

/// ELF64 Header
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64Header {
    pub e_ident: [u8; 16],      // Magic number and other info
    pub e_type: u16,            // Object file type
    pub e_machine: u16,         // Architecture
    pub e_version: u32,         // Object file version
    pub e_entry: u64,           // Entry point virtual address
    pub e_phoff: u64,           // Program header table file offset
    pub e_shoff: u64,           // Section header table file offset
    pub e_flags: u32,           // Processor-specific flags
    pub e_ehsize: u16,          // ELF header size
    pub e_phentsize: u16,       // Program header entry size
    pub e_phnum: u16,           // Program header entry count
    pub e_shentsize: u16,       // Section header entry size
    pub e_shnum: u16,           // Section header entry count
    pub e_shstrndx: u16,        // Section header string table index
}

/// ELF64 Program Header
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64ProgramHeader {
    pub p_type: u32,            // Segment type
    pub p_flags: u32,           // Segment flags
    pub p_offset: u64,          // Segment file offset
    pub p_vaddr: u64,           // Segment virtual address
    pub p_paddr: u64,           // Segment physical address
    pub p_filesz: u64,          // Segment size in file
    pub p_memsz: u64,           // Segment size in memory
    pub p_align: u64,           // Segment alignment
}

/// Program segment flags
pub mod pf {
    pub const X: u32 = 0x1; // Execute
    pub const W: u32 = 0x2; // Write
    pub const R: u32 = 0x4; // Read
}

/// ELF parsing error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Binary too small to contain ELF header
    TooSmall,
    /// Invalid ELF magic number
    InvalidMagic,
    /// Not a 64-bit ELF
    Not64Bit,
    /// Wrong endianness (not little-endian)
    WrongEndianness,
    /// Not an executable
    NotExecutable,
    /// Wrong architecture (not x86-64)
    WrongArch,
    /// Invalid program header offset
    InvalidPhoff,
    /// Segment vaddr/size lands in kernel address space
    KernelVaddr,
}

/// Parsed ELF binary
pub struct ElfBinary<'a> {
    pub header: &'a Elf64Header,
    pub binary: &'a [u8],
}

impl<'a> ElfBinary<'a> {
    /// Parse ELF binary
    ///
    /// Validates ELF header and returns parsed binary structure.
    ///
    /// # Safety
    /// Binary data must be valid for the lifetime 'a.
    pub fn parse(binary: &'a [u8]) -> Result<Self, ElfError> {
        // 1. Check minimum size
        if binary.len() < core::mem::size_of::<Elf64Header>() {
            return Err(ElfError::TooSmall);
        }

        // 2. Parse header (unsafe: we trust binary is properly aligned)
        let header = unsafe {
            &*(binary.as_ptr() as *const Elf64Header)
        };

        // 3. Validate magic number
        if &header.e_ident[0..4] != &ELF_MAGIC {
            return Err(ElfError::InvalidMagic);
        }

        // 4. Validate ELF class (64-bit)
        if header.e_ident[4] != ELFCLASS64 {
            return Err(ElfError::Not64Bit);
        }

        // 5. Validate data encoding (little-endian)
        if header.e_ident[5] != ELFDATA2LSB {
            return Err(ElfError::WrongEndianness);
        }

        // 6. Validate type (executable)
        if header.e_type != ET_EXEC {
            return Err(ElfError::NotExecutable);
        }

        // 7. Validate machine (x86-64)
        if header.e_machine != EM_X86_64 {
            return Err(ElfError::WrongArch);
        }

        Ok(Self { header, binary })
    }

    /// Get entry point address
    pub fn entry_point(&self) -> u64 {
        self.header.e_entry
    }

    /// Get program headers iterator
    pub fn program_headers(&self) -> ProgramHeaderIter<'a> {
        let phoff = self.header.e_phoff as usize;
        let phnum = self.header.e_phnum as usize;
        let phentsize = self.header.e_phentsize as usize;

        ProgramHeaderIter {
            binary: self.binary,
            offset: phoff,
            count: phnum,
            entsize: phentsize,
            current: 0,
        }
    }

    /// Get loadable segments
    pub fn loadable_segments(&self) -> impl Iterator<Item = &'a Elf64ProgramHeader> {
        self.program_headers()
            .filter(|ph| ph.p_type == PT_LOAD)
    }
}

/// Program header iterator
pub struct ProgramHeaderIter<'a> {
    binary: &'a [u8],
    offset: usize,
    count: usize,
    entsize: usize,
    current: usize,
}

impl<'a> Iterator for ProgramHeaderIter<'a> {
    type Item = &'a Elf64ProgramHeader;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.count {
            return None;
        }

        let ph_offset = self.offset + self.current * self.entsize;
        if ph_offset + core::mem::size_of::<Elf64ProgramHeader>() > self.binary.len() {
            return None;
        }

        let ph = unsafe {
            &*(self.binary.as_ptr().add(ph_offset) as *const Elf64ProgramHeader)
        };

        self.current += 1;
        Some(ph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elf_magic() {
        assert_eq!(&ELF_MAGIC, b"\x7FELF");
    }

    #[test]
    fn test_elf_constants() {
        assert_eq!(ELFCLASS64, 2);
        assert_eq!(ELFDATA2LSB, 1);
        assert_eq!(ET_EXEC, 2);
        assert_eq!(EM_X86_64, 62);
    }
}
