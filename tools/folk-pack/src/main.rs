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
    CreateSqlite(String, Vec<AddEntry>, bool, bool), // (embed, quantize)
}

fn print_usage() {
    eprintln!("folk-pack: Tool to create initrd images for Folkering OS");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  folk-pack create <output.fpk> --add <name>:<type>:<path> [--add ...]");
    eprintln!("  folk-pack create-sqlite <output.db> --add <name>:<type>:<path> [--add ...] [--embed] [--quantize]");
    eprintln!();
    eprintln!("Types: elf, data");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --embed     Generate semantic embeddings for vector search (requires Python)");
    eprintln!("  --quantize  Create quantized shadow tables for fast vector search (implies --embed)");
    eprintln!("              Creates shadow_bq (binary), shadow_sq8 (scalar), synapse_meta_index tables");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  folk-pack create initrd.fpk --add shell:elf:path/to/shell");
    eprintln!("  folk-pack create-sqlite initrd.db --add synapse:elf:path/to/synapse");
    eprintln!("  folk-pack create-sqlite initrd.db --add hello.txt:data:hello.txt --embed");
    eprintln!("  folk-pack create-sqlite initrd.db --add hello.txt:data:hello.txt --quantize");
    process::exit(1);
}

/// Parse result containing entries and optional flags
struct ParseResult {
    entries: Vec<AddEntry>,
    embed: bool,
    quantize: bool,
}

fn parse_add_entries(args: &[String], start_index: usize) -> ParseResult {
    let mut entries = Vec::new();
    let mut embed = false;
    let mut quantize = false;
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
        } else if args[i] == "--quantize" {
            quantize = true;
            embed = true; // --quantize implies --embed
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

    ParseResult { entries, embed, quantize }
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
            if result.embed || result.quantize {
                eprintln!("Warning: --embed/--quantize are only supported with create-sqlite");
            }
            Command::Create(output, result.entries)
        }
        "create-sqlite" => {
            let output = args[2].clone();
            let result = parse_add_entries(&args, 3);
            Command::CreateSqlite(output, result.entries, result.embed, result.quantize)
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

// ============================================================================
// Quantization for Shadow Tables
// ============================================================================

/// Size of binary quantized vector (384 bits / 8 = 48 bytes)
const BQ_SIZE: usize = EMBEDDING_DIM / 8;

/// Size of scalar quantized vector (384 i8 + scale f32 + offset f32 = 392 bytes)
const SQ8_SERIALIZED_SIZE: usize = EMBEDDING_DIM + 8;

/// Number of BQ vectors per chunk (64 × 48 = 3072 bytes, fits in 4KB page)
const BQ_CHUNK_SIZE: usize = 64;

/// Number of SQ8 vectors per chunk (8 × 392 = 3136 bytes, fits in 4KB page)
const SQ8_CHUNK_SIZE: usize = 8;

/// Quantize embedding to binary (sign-based)
fn quantize_binary(embedding: &[f32]) -> Vec<u8> {
    let mut bits = vec![0u8; BQ_SIZE];

    for (i, &value) in embedding.iter().enumerate() {
        if value >= 0.0 {
            let byte_idx = i / 8;
            let bit_idx = i % 8;
            bits[byte_idx] |= 1 << bit_idx;
        }
    }

    bits
}

/// Quantize embedding to scalar (min/max calibration)
fn quantize_scalar(embedding: &[f32]) -> Vec<u8> {
    // Find min and max
    let mut min_val = embedding[0];
    let mut max_val = embedding[0];
    for &v in &embedding[1..] {
        if v < min_val { min_val = v; }
        if v > max_val { max_val = v; }
    }

    let range = max_val - min_val;
    let (scale, offset) = if range < 1e-10 {
        (1.0f32, min_val)
    } else {
        let offset = (min_val + max_val) / 2.0;
        let scale = 254.0 / range;
        (scale, offset)
    };

    // Quantize values
    let mut result = Vec::with_capacity(SQ8_SERIALIZED_SIZE);

    for &value in embedding {
        let normalized = (value - offset) * scale;
        let clamped = normalized.clamp(-127.0, 127.0) as i8;
        result.push(clamped as u8);
    }

    // Append scale and offset
    result.extend_from_slice(&scale.to_le_bytes());
    result.extend_from_slice(&offset.to_le_bytes());

    result
}

fn create_sqlite(output_path: String, add_entries: Vec<AddEntry>, generate_embeddings: bool, generate_quantized: bool) {
    println!("folk-pack: Creating SQLite database {} with {} entries", output_path, add_entries.len());

    if generate_quantized {
        println!("folk-pack: Quantized index generation enabled (--quantize)");
    }
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

    // Create shadow tables for quantized search (if --quantize)
    if generate_quantized {
        // BQ shadow table: 64 vectors × 48 bytes = 3072 bytes per chunk
        conn.execute(
            "CREATE TABLE shadow_bq (
                chunk_id INTEGER PRIMARY KEY,
                data BLOB NOT NULL
            )",
            [],
        ).unwrap_or_else(|e| {
            eprintln!("Error creating shadow_bq table: {}", e);
            process::exit(1);
        });

        // SQ8 shadow table: 8 vectors × 392 bytes = 3136 bytes per chunk
        conn.execute(
            "CREATE TABLE shadow_sq8 (
                chunk_id INTEGER PRIMARY KEY,
                data BLOB NOT NULL
            )",
            [],
        ).unwrap_or_else(|e| {
            eprintln!("Error creating shadow_sq8 table: {}", e);
            process::exit(1);
        });

        // Meta index: maps user rowid to chunk positions
        conn.execute(
            "CREATE TABLE synapse_meta_index (
                user_rowid INTEGER PRIMARY KEY,
                bq_chunk_id INTEGER NOT NULL,
                bq_offset_idx INTEGER NOT NULL,
                sq8_chunk_id INTEGER NOT NULL,
                sq8_offset_idx INTEGER NOT NULL
            )",
            [],
        ).unwrap_or_else(|e| {
            eprintln!("Error creating synapse_meta_index table: {}", e);
            process::exit(1);
        });

        println!("folk-pack: Shadow tables created (shadow_bq, shadow_sq8, synapse_meta_index)");
    }

    // Insert all files
    let mut total_size = 0usize;
    let mut embedding_count = 0usize;

    // Collect embeddings for quantization (if --quantize)
    let mut all_embeddings: Vec<(i64, Vec<f32>)> = Vec::new();

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

                    // Store embedding for quantization
                    if generate_quantized {
                        all_embeddings.push((i as i64, embedding.clone()));
                    }

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

    // Generate quantized shadow tables
    if generate_quantized && !all_embeddings.is_empty() {
        println!();
        println!("folk-pack: Generating quantized shadow tables...");

        // Build BQ chunks
        let mut bq_chunks: Vec<Vec<u8>> = Vec::new();
        let mut current_bq_chunk: Vec<u8> = Vec::new();
        let mut bq_chunk_count = 0;

        // Build SQ8 chunks
        let mut sq8_chunks: Vec<Vec<u8>> = Vec::new();
        let mut current_sq8_chunk: Vec<u8> = Vec::new();
        let mut sq8_chunk_count = 0;

        // Track meta index entries
        let mut meta_entries: Vec<(i64, i64, i64, i64, i64)> = Vec::new(); // (rowid, bq_chunk, bq_off, sq8_chunk, sq8_off)

        for (file_id, embedding) in &all_embeddings {
            // Quantize to BQ
            let bq = quantize_binary(embedding);
            let bq_offset = current_bq_chunk.len() / BQ_SIZE;

            // Check if BQ chunk is full
            if bq_offset >= BQ_CHUNK_SIZE {
                bq_chunks.push(current_bq_chunk);
                current_bq_chunk = Vec::new();
                bq_chunk_count += 1;
            }

            let bq_chunk_id = bq_chunk_count as i64;
            let bq_offset_in_chunk = (current_bq_chunk.len() / BQ_SIZE) as i64;
            current_bq_chunk.extend_from_slice(&bq);

            // Quantize to SQ8
            let sq8 = quantize_scalar(embedding);
            let sq8_offset = current_sq8_chunk.len() / SQ8_SERIALIZED_SIZE;

            // Check if SQ8 chunk is full
            if sq8_offset >= SQ8_CHUNK_SIZE {
                sq8_chunks.push(current_sq8_chunk);
                current_sq8_chunk = Vec::new();
                sq8_chunk_count += 1;
            }

            let sq8_chunk_id = sq8_chunk_count as i64;
            let sq8_offset_in_chunk = (current_sq8_chunk.len() / SQ8_SERIALIZED_SIZE) as i64;
            current_sq8_chunk.extend_from_slice(&sq8);

            // Record meta entry
            meta_entries.push((*file_id, bq_chunk_id, bq_offset_in_chunk, sq8_chunk_id, sq8_offset_in_chunk));
        }

        // Flush remaining chunks
        if !current_bq_chunk.is_empty() {
            // Pad to full chunk size for consistent reads
            while current_bq_chunk.len() < BQ_CHUNK_SIZE * BQ_SIZE {
                current_bq_chunk.push(0);
            }
            bq_chunks.push(current_bq_chunk);
        }
        if !current_sq8_chunk.is_empty() {
            while current_sq8_chunk.len() < SQ8_CHUNK_SIZE * SQ8_SERIALIZED_SIZE {
                current_sq8_chunk.push(0);
            }
            sq8_chunks.push(current_sq8_chunk);
        }

        // Insert BQ chunks
        for (chunk_id, chunk_data) in bq_chunks.iter().enumerate() {
            conn.execute(
                "INSERT INTO shadow_bq (chunk_id, data) VALUES (?1, ?2)",
                params![chunk_id as i64, chunk_data],
            ).unwrap_or_else(|e| {
                eprintln!("Error inserting BQ chunk {}: {}", chunk_id, e);
                process::exit(1);
            });
        }

        // Insert SQ8 chunks
        for (chunk_id, chunk_data) in sq8_chunks.iter().enumerate() {
            conn.execute(
                "INSERT INTO shadow_sq8 (chunk_id, data) VALUES (?1, ?2)",
                params![chunk_id as i64, chunk_data],
            ).unwrap_or_else(|e| {
                eprintln!("Error inserting SQ8 chunk {}: {}", chunk_id, e);
                process::exit(1);
            });
        }

        // Insert meta index entries
        for (rowid, bq_chunk, bq_off, sq8_chunk, sq8_off) in &meta_entries {
            conn.execute(
                "INSERT INTO synapse_meta_index (user_rowid, bq_chunk_id, bq_offset_idx, sq8_chunk_id, sq8_offset_idx) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![rowid, bq_chunk, bq_off, sq8_chunk, sq8_off],
            ).unwrap_or_else(|e| {
                eprintln!("Error inserting meta index: {}", e);
                process::exit(1);
            });
        }

        println!("folk-pack: Quantized index created:");
        println!("  BQ chunks: {} ({} bytes each, {} total vectors)",
                 bq_chunks.len(), BQ_CHUNK_SIZE * BQ_SIZE, all_embeddings.len());
        println!("  SQ8 chunks: {} ({} bytes each)",
                 sq8_chunks.len(), SQ8_CHUNK_SIZE * SQ8_SERIALIZED_SIZE);
        println!("  Meta index: {} entries", meta_entries.len());
    }

    // Force database to write all pages
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();

    // Close connection
    drop(conn);

    // Get final file size
    let db_size = fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    if generate_quantized {
        println!();
        println!("folk-pack: Created {} ({} bytes, {} entries, {} bytes data, {} embeddings, quantized)",
            output_path, db_size, add_entries.len(), total_size, embedding_count);
    } else if generate_embeddings {
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

    if generate_quantized {
        println!();
        println!("  -- Quantized shadow tables for fast vector search");
        println!("  CREATE TABLE shadow_bq (");
        println!("      chunk_id INTEGER PRIMARY KEY,");
        println!("      data BLOB NOT NULL  -- {} vectors × {} bytes = {} bytes/chunk", BQ_CHUNK_SIZE, BQ_SIZE, BQ_CHUNK_SIZE * BQ_SIZE);
        println!("  );");
        println!();
        println!("  CREATE TABLE shadow_sq8 (");
        println!("      chunk_id INTEGER PRIMARY KEY,");
        println!("      data BLOB NOT NULL  -- {} vectors × {} bytes = {} bytes/chunk", SQ8_CHUNK_SIZE, SQ8_SERIALIZED_SIZE, SQ8_CHUNK_SIZE * SQ8_SERIALIZED_SIZE);
        println!("  );");
        println!();
        println!("  CREATE TABLE synapse_meta_index (");
        println!("      user_rowid INTEGER PRIMARY KEY,");
        println!("      bq_chunk_id INTEGER, bq_offset_idx INTEGER,");
        println!("      sq8_chunk_id INTEGER, sq8_offset_idx INTEGER");
        println!("  );");
    }
}

fn main() {
    match parse_args() {
        Command::Create(output, entries) => create_fpk(output, entries),
        Command::CreateSqlite(output, entries, embed, quantize) => create_sqlite(output, entries, embed, quantize),
    }
}
