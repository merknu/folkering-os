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
//!
//! Phase 5 Vector Search:
//!   The SQLite database includes tables for semantic search embeddings:
//!   - embeddings: 384-dimensional vectors for each file
//!   - file_metadata: extracted text and content hashes

mod format;

use format::*;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{self, Command as ProcessCommand, Stdio};

/// File type constants for SQLite 'kind' column
const KIND_ELF: i32 = 0;
const KIND_DATA: i32 = 1;

/// Embedding dimension for all-MiniLM-L6-v2 model
const EMBEDDING_DIM: usize = 384;

/// Size of embedding in bytes (384 floats × 4 bytes)
const EMBEDDING_SIZE: usize = EMBEDDING_DIM * 4;

struct AddEntry {
    name: String,
    entry_type: u16,
    path: String,
}

enum Command {
    Create(String, Vec<AddEntry>),
    CreateSqlite(String, Vec<AddEntry>, bool), // bool = generate embeddings
}

fn print_usage() {
    eprintln!("folk-pack: Tool to create initrd images for Folkering OS");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  folk-pack create <output.fpk> --add <name>:<type>:<path> [--add ...]");
    eprintln!("  folk-pack create-sqlite <output.db> --add <name>:<type>:<path> [--add ...] [--embed]");
    eprintln!();
    eprintln!("Types: elf, data");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --embed    Generate semantic embeddings for vector search (requires Python)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  folk-pack create initrd.fpk --add shell:elf:path/to/shell");
    eprintln!("  folk-pack create-sqlite initrd.db --add synapse:elf:path/to/synapse");
    eprintln!("  folk-pack create-sqlite initrd.db --add hello.txt:data:hello.txt --embed");
    process::exit(1);
}

/// Parse result containing entries and optional flags
struct ParseResult {
    entries: Vec<AddEntry>,
    embed: bool,
}

fn parse_add_entries(args: &[String], start_index: usize) -> ParseResult {
    let mut entries = Vec::new();
    let mut embed = false;
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
        } else if args[i] == "--embed" {
            embed = true;
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

    ParseResult { entries, embed }
}

fn parse_args() -> Command {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        print_usage();
    }

    match args[1].as_str() {
        "create" => {
            let output = args[2].clone();
            let result = parse_add_entries(&args, 3);
            if result.embed {
                eprintln!("Warning: --embed is only supported with create-sqlite");
            }
            Command::Create(output, result.entries)
        }
        "create-sqlite" => {
            let output = args[2].clone();
            let result = parse_add_entries(&args, 3);
            Command::CreateSqlite(output, result.entries, result.embed)
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

/// Extract text content from file data for embedding generation
fn extract_text_for_embedding(name: &str, data: &[u8], is_elf: bool) -> String {
    if is_elf {
        // For ELF binaries, use filename + description
        format!("{} executable program binary", name)
    } else {
        // For data files, try to extract text content
        let text = match std::str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => {
                // Binary data, use filename + type description
                return format!("{} binary data file", name);
            }
        };

        // Truncate to reasonable length for embedding (max ~2000 chars)
        let max_len = 2000;
        if text.len() > max_len {
            format!("{} {}", name, &text[..max_len])
        } else {
            format!("{} {}", name, text)
        }
    }
}

/// Generate embedding using Python sentence-transformers
fn generate_embedding(text: &str) -> Result<Vec<f32>, String> {
    // Python script to generate embedding
    let python_script = r#"
import sys
import json

try:
    from sentence_transformers import SentenceTransformer

    # Load model (cached after first load)
    model = SentenceTransformer('all-MiniLM-L6-v2')

    # Read input text from stdin
    input_data = json.loads(sys.stdin.read())
    text = input_data['text']

    # Generate embedding
    embedding = model.encode(text, normalize_embeddings=True)

    # Output as JSON
    print(json.dumps({'embedding': embedding.tolist(), 'error': None}))
except Exception as e:
    print(json.dumps({'embedding': None, 'error': str(e)}))
"#;

    // Find Python executable
    let python = if cfg!(windows) {
        // Try python first on Windows
        if ProcessCommand::new("python")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            "python"
        } else {
            "python3"
        }
    } else {
        "python3"
    };

    // Prepare input JSON
    let input_json = serde_json::json!({ "text": text }).to_string();

    // Run Python subprocess
    let mut child = ProcessCommand::new(python)
        .arg("-c")
        .arg(python_script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn Python: {}", e))?;

    // Write input to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input_json.as_bytes())
            .map_err(|e| format!("Failed to write to Python stdin: {}", e))?;
    }

    // Wait for output
    let output = child.wait_with_output()
        .map_err(|e| format!("Failed to wait for Python: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Python failed: {}", stderr));
    }

    // Parse output JSON
    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("Failed to parse Python output: {}", e))?;

    if let Some(error) = response.get("error").and_then(|e| e.as_str()) {
        if !error.is_empty() {
            return Err(format!("Embedding error: {}", error));
        }
    }

    let embedding = response.get("embedding")
        .and_then(|e| e.as_array())
        .ok_or_else(|| "No embedding in response".to_string())?;

    let values: Vec<f32> = embedding.iter()
        .filter_map(|v| v.as_f64().map(|f| f as f32))
        .collect();

    if values.len() != EMBEDDING_DIM {
        return Err(format!(
            "Wrong embedding dimension: expected {}, got {}",
            EMBEDDING_DIM, values.len()
        ));
    }

    Ok(values)
}

/// Convert embedding vector to raw bytes (little-endian f32)
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(EMBEDDING_SIZE);
    for &value in embedding {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

/// Check if Python and sentence-transformers are available
fn check_embedding_dependencies() -> Result<(), String> {
    let python = if cfg!(windows) { "python" } else { "python3" };

    let output = ProcessCommand::new(python)
        .arg("-c")
        .arg("from sentence_transformers import SentenceTransformer; print('ok')")
        .output()
        .map_err(|e| format!("Python not found: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "sentence-transformers not installed.\n\
             Install with: pip install sentence-transformers\n\
             Error: {}", stderr
        ));
    }

    Ok(())
}

fn create_sqlite(output_path: String, add_entries: Vec<AddEntry>, generate_embeddings: bool) {
    println!("folk-pack: Creating SQLite database {} with {} entries", output_path, add_entries.len());

    if generate_embeddings {
        println!("folk-pack: Embedding generation enabled (--embed)");
        if let Err(e) = check_embedding_dependencies() {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
        println!("folk-pack: Python and sentence-transformers available");
    }

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
        eprintln!("Error creating files table: {}", e);
        process::exit(1);
    });

    // Create the embeddings table for vector search (Phase 5)
    // Vectors are stored as raw BLOB: 384 × f32 = 1536 bytes
    conn.execute(
        "CREATE TABLE embeddings (
            file_id INTEGER PRIMARY KEY,
            vector BLOB NOT NULL,
            model TEXT DEFAULT 'minilm-l6',
            created_at TEXT,
            FOREIGN KEY (file_id) REFERENCES files(id)
        )",
        [],
    ).unwrap_or_else(|e| {
        eprintln!("Error creating embeddings table: {}", e);
        process::exit(1);
    });

    // Create file_metadata table for semantic content extraction
    conn.execute(
        "CREATE TABLE file_metadata (
            file_id INTEGER PRIMARY KEY,
            content_hash TEXT,
            extracted_text TEXT,
            FOREIGN KEY (file_id) REFERENCES files(id)
        )",
        [],
    ).unwrap_or_else(|e| {
        eprintln!("Error creating file_metadata table: {}", e);
        process::exit(1);
    });

    // Create index on embeddings for faster lookups
    conn.execute(
        "CREATE INDEX idx_embeddings_file_id ON embeddings(file_id)",
        [],
    ).unwrap_or_else(|e| {
        eprintln!("Error creating embeddings index: {}", e);
        process::exit(1);
    });

    // Insert all files
    let mut total_size = 0usize;
    let mut embedding_count = 0usize;

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

        let is_elf = entry.entry_type == ENTRY_TYPE_ELF;
        let kind = if is_elf { KIND_ELF } else { KIND_DATA };
        let size = data.len();
        total_size += size;

        // Insert file
        conn.execute(
            "INSERT INTO files (id, name, kind, size, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![i as i64, entry.name, kind, size as i64, data],
        ).unwrap_or_else(|e| {
            eprintln!("Error inserting file '{}': {}", entry.name, e);
            process::exit(1);
        });

        // Generate embedding if requested
        if generate_embeddings {
            let text = extract_text_for_embedding(&entry.name, &data, is_elf);

            // Compute content hash
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let content_hash = format!("{:x}", hasher.finalize());

            // Insert file metadata
            conn.execute(
                "INSERT INTO file_metadata (file_id, content_hash, extracted_text) VALUES (?1, ?2, ?3)",
                params![i as i64, content_hash, text],
            ).unwrap_or_else(|e| {
                eprintln!("Error inserting metadata for '{}': {}", entry.name, e);
                process::exit(1);
            });

            // Generate embedding
            print!("  [{}] {} ({} bytes, kind={}) ... ", i, entry.name, size,
                if kind == KIND_ELF { "elf" } else { "data" });
            std::io::stdout().flush().ok();

            match generate_embedding(&text) {
                Ok(embedding) => {
                    let blob = embedding_to_blob(&embedding);
                    let now = chrono::Utc::now().to_rfc3339();

                    conn.execute(
                        "INSERT INTO embeddings (file_id, vector, model, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![i as i64, blob, "minilm-l6", now],
                    ).unwrap_or_else(|e| {
                        eprintln!("Error inserting embedding for '{}': {}", entry.name, e);
                        process::exit(1);
                    });

                    embedding_count += 1;
                    println!("embedded ({} bytes)", blob.len());
                }
                Err(e) => {
                    println!("FAILED: {}", e);
                    eprintln!("Warning: Could not generate embedding for '{}': {}", entry.name, e);
                }
            }
        } else {
            println!("  [{}] {} ({} bytes, kind={})",
                i,
                entry.name,
                size,
                if kind == KIND_ELF { "elf" } else { "data" }
            );
        }
    }

    // Force database to write all pages
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();

    // Close connection
    drop(conn);

    // Get final file size
    let db_size = fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    if generate_embeddings {
        println!();
        println!("folk-pack: Created {} ({} bytes, {} entries, {} bytes data, {} embeddings)",
            output_path, db_size, add_entries.len(), total_size, embedding_count);
    } else {
        println!("folk-pack: Created {} ({} bytes, {} entries, {} bytes data)",
            output_path, db_size, add_entries.len(), total_size);
    }
    println!();
    println!("Schema:");
    println!("  CREATE TABLE files (");
    println!("      id INTEGER PRIMARY KEY,");
    println!("      name TEXT UNIQUE NOT NULL,");
    println!("      kind INTEGER NOT NULL,  -- 0=ELF, 1=Data");
    println!("      size INTEGER NOT NULL,");
    println!("      data BLOB");
    println!("  );");
    println!();
    println!("  CREATE TABLE embeddings (");
    println!("      file_id INTEGER PRIMARY KEY,");
    println!("      vector BLOB NOT NULL,     -- {} bytes ({} × f32)", EMBEDDING_SIZE, EMBEDDING_DIM);
    println!("      model TEXT DEFAULT 'minilm-l6',");
    println!("      created_at TEXT,");
    println!("      FOREIGN KEY (file_id) REFERENCES files(id)");
    println!("  );");
    println!();
    println!("  CREATE TABLE file_metadata (");
    println!("      file_id INTEGER PRIMARY KEY,");
    println!("      content_hash TEXT,");
    println!("      extracted_text TEXT,");
    println!("      FOREIGN KEY (file_id) REFERENCES files(id)");
    println!("  );");
}

fn main() {
    match parse_args() {
        Command::Create(output, entries) => create_fpk(output, entries),
        Command::CreateSqlite(output, entries, embed) => create_sqlite(output, entries, embed),
    }
}
