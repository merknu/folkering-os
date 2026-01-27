//! folk-pack: Host-side tool to create Folk-Pack (FPK) initrd images
//!
//! Usage:
//!   folk-pack create <output> --add <name>:<type>:<path> [--add ...]
//!
//! Example:
//!   folk-pack create initrd.fpk \
//!     --add shell:elf:userspace/target/x86_64-folkering-userspace/release/shell \
//!     --add hello:elf:userspace/target/x86_64-folkering-userspace/release/hello

mod format;

use format::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

struct AddEntry {
    name: String,
    entry_type: u16,
    path: String,
}

fn parse_args() -> (String, Vec<AddEntry>) {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage: folk-pack create <output.fpk> --add <name>:<type>:<path> [--add ...]");
        eprintln!();
        eprintln!("Types: elf, data");
        eprintln!();
        eprintln!("Example:");
        eprintln!("  folk-pack create initrd.fpk --add shell:elf:path/to/shell");
        process::exit(1);
    }

    if args[1] != "create" {
        eprintln!("Unknown command: {}. Only 'create' is supported.", args[1]);
        process::exit(1);
    }

    let output = args[2].clone();
    let mut entries = Vec::new();

    let mut i = 3;
    while i < args.len() {
        if args[i] == "--add" {
            i += 1;
            if i >= args.len() {
                eprintln!("--add requires an argument: <name>:<type>:<path>");
                process::exit(1);
            }
            let parts: Vec<&str> = args[i].splitn(3, ':').collect();
            if parts.len() != 3 {
                eprintln!("Invalid --add format '{}'. Expected <name>:<type>:<path>", args[i]);
                process::exit(1);
            }

            let name = parts[0].to_string();
            if name.len() >= FPK_NAME_LEN {
                eprintln!("Name '{}' too long (max {} bytes)", name, FPK_NAME_LEN - 1);
                process::exit(1);
            }

            let entry_type = match parts[1] {
                "elf" => ENTRY_TYPE_ELF,
                "data" => ENTRY_TYPE_DATA,
                other => {
                    eprintln!("Unknown type '{}'. Use 'elf' or 'data'.", other);
                    process::exit(1);
                }
            };

            let path = parts[2].to_string();
            entries.push(AddEntry { name, entry_type, path });
        } else {
            eprintln!("Unknown argument: {}", args[i]);
            process::exit(1);
        }
        i += 1;
    }

    if entries.is_empty() {
        eprintln!("No entries specified. Use --add <name>:<type>:<path>");
        process::exit(1);
    }

    (output, entries)
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn main() {
    let (output_path, add_entries) = parse_args();

    println!("folk-pack: Creating {} with {} entries", output_path, add_entries.len());

    // Read all input files
    let mut file_data: Vec<(AddEntry, Vec<u8>)> = Vec::new();
    for entry in add_entries {
        let path = Path::new(&entry.path);
        if !path.exists() {
            eprintln!("Error: File not found: {}", entry.path);
            process::exit(1);
        }
        let data = fs::read(path).unwrap_or_else(|e| {
            eprintln!("Error reading '{}': {}", entry.path, e);
            process::exit(1);
        });
        println!("  [{}] {} ({} bytes, type={})",
            file_data.len(),
            entry.name,
            data.len(),
            if entry.entry_type == ENTRY_TYPE_ELF { "elf" } else { "data" }
        );
        file_data.push((entry, data));
    }

    let entry_count = file_data.len();

    // Calculate offsets
    // Header: 64 bytes
    // Entry table: 64 bytes × N
    // Then page-aligned data for each entry
    let header_size = std::mem::size_of::<FpkHeader>();
    let entry_table_size = std::mem::size_of::<FpkEntry>() * entry_count;
    let data_start = align_up(header_size + entry_table_size, FPK_PAGE_SIZE);

    // Calculate per-entry offsets
    let mut current_offset = data_start;
    let mut entries_with_offsets: Vec<(usize, usize, [u8; 8])> = Vec::new(); // (offset, size, hash)

    for (_entry, data) in &file_data {
        let offset = current_offset;
        let size = data.len();

        // Compute truncated SHA-256 hash
        let mut hasher = Sha256::new();
        hasher.update(data);
        let full_hash = hasher.finalize();
        let mut hash = [0u8; 8];
        hash.copy_from_slice(&full_hash[..8]);

        entries_with_offsets.push((offset, size, hash));
        current_offset = align_up(current_offset + size, FPK_PAGE_SIZE);
    }

    let total_size = current_offset;

    // Build header
    let header = FpkHeader {
        magic: FPK_MAGIC,
        version: FPK_VERSION,
        entry_count: entry_count as u16,
        total_size: total_size as u64,
        reserved: [0u8; 48],
    };

    // Build entry table
    let mut fpk_entries: Vec<FpkEntry> = Vec::new();
    for (i, ((add_entry, _data), (offset, size, hash))) in
        file_data.iter().zip(entries_with_offsets.iter()).enumerate()
    {
        let mut name = [0u8; FPK_NAME_LEN];
        let name_bytes = add_entry.name.as_bytes();
        name[..name_bytes.len()].copy_from_slice(name_bytes);

        fpk_entries.push(FpkEntry {
            id: i as u16,
            entry_type: add_entry.entry_type,
            name,
            offset: *offset as u64,
            size: *size as u64,
            hash: *hash,
        });
    }

    // Write output file
    let mut output = fs::File::create(&output_path).unwrap_or_else(|e| {
        eprintln!("Error creating '{}': {}", output_path, e);
        process::exit(1);
    });

    // Write header
    output.write_all(header.as_bytes()).unwrap();

    // Write entry table
    for entry in &fpk_entries {
        output.write_all(entry.as_bytes()).unwrap();
    }

    // Pad to data_start
    let current_pos = header_size + entry_table_size;
    if current_pos < data_start {
        let padding = vec![0u8; data_start - current_pos];
        output.write_all(&padding).unwrap();
    }

    // Write data blobs (page-aligned)
    for (i, (_entry, data)) in file_data.iter().enumerate() {
        let (expected_offset, _, _) = entries_with_offsets[i];
        let current_file_pos = if i == 0 {
            data_start
        } else {
            let (prev_offset, prev_size, _) = entries_with_offsets[i - 1];
            align_up(prev_offset + prev_size, FPK_PAGE_SIZE)
        };

        // Verify alignment
        assert_eq!(current_file_pos, expected_offset,
            "Offset mismatch for entry {}: expected {}, got {}", i, expected_offset, current_file_pos);

        output.write_all(data).unwrap();

        // Pad to next page boundary (except for last entry)
        if i + 1 < file_data.len() {
            let next_offset = entries_with_offsets[i + 1].0;
            let pad_size = next_offset - (expected_offset + data.len());
            if pad_size > 0 {
                let padding = vec![0u8; pad_size];
                output.write_all(&padding).unwrap();
            }
        }
    }

    println!("folk-pack: Created {} ({} bytes, {} entries)", output_path, total_size, entry_count);
    println!("  Header: {} bytes", header_size);
    println!("  Entry table: {} bytes ({} entries × 64)", entry_table_size, entry_count);
    println!("  Data start: offset {:#x}", data_start);
    for (i, entry) in fpk_entries.iter().enumerate() {
        let name_len = entry.name.iter().position(|&b| b == 0).unwrap_or(FPK_NAME_LEN);
        let name = std::str::from_utf8(&entry.name[..name_len]).unwrap_or("?");
        println!("  Entry {}: \"{}\" at offset {:#x} ({} bytes, hash {:02x}{:02x}{:02x}{:02x}...)",
            i, name, entry.offset, entry.size,
            entry.hash[0], entry.hash[1], entry.hash[2], entry.hash[3]);
    }
}
