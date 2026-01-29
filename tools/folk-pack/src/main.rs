//! folk-pack: Host-side tool to create Folk-Pack (FPK) and SQLite initrd images
//!
//! Usage:
//!   folk-pack create <output.fpk> --add <name>:<type>:<path> [--add ...]
//!   folk-pack create-sqlite <output.db> --add <name>:<type>:<path> [--add ...]
//!
//! Example:
//!   folk-pack create initrd.fpk \
//!     --add shell:elf:userspace/target/x86_64-folkering-userspace/release/shell
//!
//!   folk-pack create-sqlite initrd.db \
//!     --add synapse:elf:path/to/synapse \
//!     --add shell:elf:path/to/shell \
//!     --add hello.txt:data:path/to/hello.txt

mod format;

use format::*;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

/// File type constants for SQLite 'kind' column
const KIND_ELF: i32 = 0;
const KIND_DATA: i32 = 1;

struct AddEntry {
    name: String,
    entry_type: u16,
    path: String,
}

enum Command {
    Create(String, Vec<AddEntry>),
    CreateSqlite(String, Vec<AddEntry>),
}

fn print_usage() {
    eprintln!("folk-pack: Tool to create initrd images for Folkering OS");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  folk-pack create <output.fpk> --add <name>:<type>:<path> [--add ...]");
    eprintln!("  folk-pack create-sqlite <output.db> --add <name>:<type>:<path> [--add ...]");
    eprintln!();
    eprintln!("Types: elf, data");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  folk-pack create initrd.fpk --add shell:elf:path/to/shell");
    eprintln!("  folk-pack create-sqlite initrd.db --add synapse:elf:path/to/synapse");
    process::exit(1);
}

fn parse_add_entries(args: &[String], start_index: usize) -> Vec<AddEntry> {
    let mut entries = Vec::new();
    let mut i = start_index;

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

    entries
}

fn parse_args() -> Command {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        print_usage();
    }

    match args[1].as_str() {
        "create" => {
            let output = args[2].clone();
            let entries = parse_add_entries(&args, 3);
            Command::Create(output, entries)
        }
        "create-sqlite" => {
            let output = args[2].clone();
            let entries = parse_add_entries(&args, 3);
            Command::CreateSqlite(output, entries)
        }
        other => {
            eprintln!("Unknown command: {}. Use 'create' or 'create-sqlite'.", other);
            process::exit(1);
        }
    }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn create_fpk(output_path: String, add_entries: Vec<AddEntry>) {
    println!("folk-pack: Creating FPK {} with {} entries", output_path, add_entries.len());

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
    let header_size = std::mem::size_of::<FpkHeader>();
    let entry_table_size = std::mem::size_of::<FpkEntry>() * entry_count;
    let data_start = align_up(header_size + entry_table_size, FPK_PAGE_SIZE);

    // Calculate per-entry offsets
    let mut current_offset = data_start;
    let mut entries_with_offsets: Vec<(usize, usize, [u8; 8])> = Vec::new();

    for (_entry, data) in &file_data {
        let offset = current_offset;
        let size = data.len();

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

        assert_eq!(current_file_pos, expected_offset,
            "Offset mismatch for entry {}: expected {}, got {}", i, expected_offset, current_file_pos);

        output.write_all(data).unwrap();

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

fn create_sqlite(output_path: String, add_entries: Vec<AddEntry>) {
    println!("folk-pack: Creating SQLite database {} with {} entries", output_path, add_entries.len());

    // Remove existing file if it exists
    if Path::new(&output_path).exists() {
        fs::remove_file(&output_path).unwrap_or_else(|e| {
            eprintln!("Error removing existing file '{}': {}", output_path, e);
            process::exit(1);
        });
    }

    // Create SQLite database
    let conn = Connection::open(&output_path).unwrap_or_else(|e| {
        eprintln!("Error creating database '{}': {}", output_path, e);
        process::exit(1);
    });

    // Set page size to 4096 for alignment with OS page size
    conn.execute_batch("PRAGMA page_size = 4096;").unwrap();

    // Create the files table
    conn.execute(
        "CREATE TABLE files (
            id INTEGER PRIMARY KEY,
            name TEXT UNIQUE NOT NULL,
            kind INTEGER NOT NULL,
            size INTEGER NOT NULL,
            data BLOB
        )",
        [],
    ).unwrap_or_else(|e| {
        eprintln!("Error creating table: {}", e);
        process::exit(1);
    });

    // Insert all files
    let mut total_size = 0usize;
    for (i, entry) in add_entries.iter().enumerate() {
        let path = Path::new(&entry.path);
        if !path.exists() {
            eprintln!("Error: File not found: {}", entry.path);
            process::exit(1);
        }

        let data = fs::read(path).unwrap_or_else(|e| {
            eprintln!("Error reading '{}': {}", entry.path, e);
            process::exit(1);
        });

        let kind = if entry.entry_type == ENTRY_TYPE_ELF { KIND_ELF } else { KIND_DATA };
        let size = data.len();
        total_size += size;

        conn.execute(
            "INSERT INTO files (id, name, kind, size, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![i as i64, entry.name, kind, size as i64, data],
        ).unwrap_or_else(|e| {
            eprintln!("Error inserting file '{}': {}", entry.name, e);
            process::exit(1);
        });

        println!("  [{}] {} ({} bytes, kind={})",
            i,
            entry.name,
            size,
            if kind == KIND_ELF { "elf" } else { "data" }
        );
    }

    // Force database to write all pages
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();

    // Close connection
    drop(conn);

    // Get final file size
    let db_size = fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    println!("folk-pack: Created {} ({} bytes, {} entries, {} bytes data)",
        output_path, db_size, add_entries.len(), total_size);
    println!();
    println!("Schema:");
    println!("  CREATE TABLE files (");
    println!("      id INTEGER PRIMARY KEY,");
    println!("      name TEXT UNIQUE NOT NULL,");
    println!("      kind INTEGER NOT NULL,  -- 0=ELF, 1=Data");
    println!("      size INTEGER NOT NULL,");
    println!("      data BLOB");
    println!("  );");
}

fn main() {
    match parse_args() {
        Command::Create(output, entries) => create_fpk(output, entries),
        Command::CreateSqlite(output, entries) => create_sqlite(output, entries),
    }
}
