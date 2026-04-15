//! Search commands: keyword, semantic similarity, and hybrid (RRF) search.

use libfolk::println;
use libfolk::sys::fs::DirEntry;
use libfolk::sys::synapse::{embedding_count, get_embedding, vector_search, SYNAPSE_TASK_ID};
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap};

/// Embedding size in bytes (384 dimensions × 4 bytes)
const EMBEDDING_SIZE: usize = 1536;

/// Virtual address for vector search query embedding
const VECTOR_QUERY_VADDR: usize = 0x21000000;

/// Virtual address for vector search results
const VECTOR_RESULTS_VADDR: usize = 0x22000000;

/// RRF constant (standard value from literature)
const RRF_K: u32 = 60;

/// Maximum results for hybrid search
const MAX_HYBRID_RESULTS: usize = 16;

/// Hybrid search result entry
#[derive(Clone, Copy)]
struct HybridResult {
    file_id: u32,
    keyword_rank: u32,
    semantic_rank: u32,
    semantic_sim: f32,
    rrf_score: u32,
}

impl Default for HybridResult {
    fn default() -> Self {
        Self {
            file_id: 0,
            keyword_rank: 0,
            semantic_rank: 0,
            semantic_sim: 0.0,
            rrf_score: 0,
        }
    }
}

/// Top-level search dispatcher.
pub fn cmd_search<'a>(args: impl Iterator<Item = &'a str>) {
    let mut keyword: Option<&str> = None;
    let mut similar_file: Option<&str> = None;
    let mut collected_args: [&str; 8] = [""; 8];
    let mut arg_count = 0;

    for arg in args {
        if arg_count < 8 {
            collected_args[arg_count] = arg;
            arg_count += 1;
        }
    }

    if arg_count == 0 {
        println!("usage: search <keyword>");
        println!("       search -s <filename>  (semantic search)");
        println!("       search <keyword> -s <file>  (hybrid search)");
        return;
    }

    let mut i = 0;
    while i < arg_count {
        let arg = collected_args[i];
        if arg == "-s" || arg == "--similar" {
            if i + 1 < arg_count {
                similar_file = Some(collected_args[i + 1]);
                i += 2;
            } else {
                println!("search: -s requires a filename");
                return;
            }
        } else {
            keyword = Some(arg);
            i += 1;
        }
    }

    match (keyword, similar_file) {
        (Some(kw), Some(sf)) => cmd_search_hybrid(kw, sf),
        (None, Some(sf)) => cmd_search_similar(sf),
        (Some(kw), None) => cmd_search_keyword(kw),
        (None, None) => {
            println!("usage: search <keyword>");
            println!("       search -s <filename>  (semantic search)");
            println!("       search <keyword> -s <file>  (hybrid search)");
        }
    }
}

/// Keyword-only search.
fn cmd_search_keyword(query: &str) {
    let mut query_lower = [0u8; 64];
    let mut query_len = 0;
    for &b in query.as_bytes() {
        if query_len < query_lower.len() - 1 {
            query_lower[query_len] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
            query_len += 1;
        }
    }
    let query_str = unsafe { core::str::from_utf8_unchecked(&query_lower[..query_len]) };

    let mut entries = [DirEntry { id: 0, entry_type: 0, name: [0u8; 32], size: 0 }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);

    if dir_count == 0 {
        println!("No files available.");
        return;
    }

    let mut found = 0;
    println!();

    for i in 0..dir_count {
        let entry = &entries[i];
        let name = entry.name_str();
        let name_lower = name_to_lowercase(name);

        if contains_substring(&name_lower, query_str) {
            let kind = if entry.is_elf() { "ELF " } else { "DATA" };
            println!("  {} {:>8} {} (keyword match)", kind, entry.size, name);
            found += 1;
        }
    }

    if found == 0 {
        println!("  No files matching '{}'", query);
        if let Ok(emb_count) = embedding_count() {
            if emb_count > 0 {
                println!("\n  Tip: Try 'search {} -s <file>' for hybrid search", query);
                println!("       ({} files have embeddings)", emb_count);
            }
        }
    } else {
        println!("\n{} file(s) found", found);
    }
}

/// Hybrid search: keyword + semantic with Reciprocal Rank Fusion.
fn cmd_search_hybrid(keyword: &str, similar_file: &str) {
    let mut embedding_handle: Option<u32> = None;
    let mut query_handle: Option<u32> = None;
    let mut results_handle: Option<u32> = None;

    let emb_count = match embedding_count() {
        Ok(c) => c,
        Err(_) => { println!("search: Synapse not available"); return; }
    };
    if emb_count == 0 {
        println!("search: No embeddings for hybrid search, falling back to keyword");
        cmd_search_keyword(keyword);
        return;
    }

    let mut entries = [DirEntry { id: 0, entry_type: 0, name: [0u8; 32], size: 0 }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);
    if dir_count == 0 { println!("No files available."); return; }

    let source_entry = entries[..dir_count].iter().find(|e| e.name_str() == similar_file);
    let source_file_id = match source_entry {
        Some(e) => e.id as u32,
        None => { println!("search: reference file '{}' not found", similar_file); return; }
    };

    // === STEP 1: Keyword Search ===
    let mut results: [HybridResult; MAX_HYBRID_RESULTS] = [HybridResult::default(); MAX_HYBRID_RESULTS];
    let mut result_count = 0;

    let mut keyword_lower = [0u8; 64];
    let mut keyword_len = 0;
    for &b in keyword.as_bytes() {
        if keyword_len < keyword_lower.len() - 1 {
            keyword_lower[keyword_len] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
            keyword_len += 1;
        }
    }
    let keyword_str = unsafe { core::str::from_utf8_unchecked(&keyword_lower[..keyword_len]) };

    let mut keyword_rank = 1u32;
    for i in 0..dir_count {
        let entry = &entries[i];
        let name_lower = name_to_lowercase(entry.name_str());
        if contains_substring(&name_lower, keyword_str) {
            if result_count < MAX_HYBRID_RESULTS {
                results[result_count].file_id = entry.id as u32;
                results[result_count].keyword_rank = keyword_rank;
                result_count += 1;
                keyword_rank += 1;
            }
        }
    }

    // === STEP 2: Semantic Search ===
    let embedding_response = match get_embedding(source_file_id) {
        Ok(r) => r,
        Err(_) => {
            println!("search: reference file '{}' has no embedding", similar_file);
            println!("Falling back to keyword-only search.\n");
            cmd_search_keyword(keyword);
            return;
        }
    };

    if shmem_map(embedding_response.shmem_handle, VECTOR_QUERY_VADDR).is_err() {
        println!("search: failed to map embedding");
        return;
    }
    embedding_handle = Some(embedding_response.shmem_handle);

    let query_shmem = match shmem_create(4096) {
        Ok(h) => h,
        Err(_) => {
            println!("search: failed to create query buffer");
            cleanup_shmem(embedding_handle, None, None);
            return;
        }
    };
    query_handle = Some(query_shmem);

    if shmem_grant(query_shmem, SYNAPSE_TASK_ID).is_err() {
        println!("search: failed to grant query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    if shmem_map(query_shmem, VECTOR_RESULTS_VADDR).is_err() {
        println!("search: failed to map query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    unsafe {
        let src = VECTOR_QUERY_VADDR as *const u8;
        let dst = VECTOR_RESULTS_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, EMBEDDING_SIZE);
    }

    let k = 10;
    let search_response = match vector_search(query_shmem, k) {
        Ok(r) => r,
        Err(_) => {
            println!("search: vector search failed, falling back to keyword");
            cleanup_shmem(embedding_handle, query_handle, None);
            cmd_search_keyword(keyword);
            return;
        }
    };

    if search_response.count > 0 {
        if shmem_map(search_response.shmem_handle, VECTOR_RESULTS_VADDR).is_err() {
            println!("search: failed to map results");
            cleanup_shmem(embedding_handle, query_handle, None);
            return;
        }
        results_handle = Some(search_response.shmem_handle);

        let results_ptr = VECTOR_RESULTS_VADDR as *const u8;
        let mut semantic_rank = 1u32;

        for i in 0..search_response.count {
            let offset = i * 8;
            let file_id = unsafe {
                let ptr = results_ptr.add(offset) as *const u32;
                *ptr
            };
            let similarity = unsafe {
                let ptr = results_ptr.add(offset + 4) as *const f32;
                *ptr
            };

            if file_id == source_file_id { continue; }

            let existing = results[..result_count]
                .iter_mut()
                .find(|r| r.file_id == file_id);

            if let Some(result) = existing {
                result.semantic_rank = semantic_rank;
                result.semantic_sim = similarity;
            } else if result_count < MAX_HYBRID_RESULTS {
                results[result_count].file_id = file_id;
                results[result_count].semantic_rank = semantic_rank;
                results[result_count].semantic_sim = similarity;
                result_count += 1;
            }

            semantic_rank += 1;
        }
    }

    // === STEP 3: Calculate RRF Scores ===
    for result in results[..result_count].iter_mut() {
        let mut score = 0u32;
        if result.keyword_rank > 0 {
            score += 1000 / (RRF_K + result.keyword_rank);
        }
        if result.semantic_rank > 0 {
            score += 1000 / (RRF_K + result.semantic_rank);
        }
        result.rrf_score = score;
    }

    // === STEP 4: Sort by RRF Score (descending) ===
    for i in 0..result_count {
        for j in (i + 1)..result_count {
            if results[j].rrf_score > results[i].rrf_score {
                let tmp = results[i];
                results[i] = results[j];
                results[j] = tmp;
            }
        }
    }

    // === STEP 5: Display Results ===
    if result_count == 0 {
        println!("\nNo files match '{}' or are similar to '{}'", keyword, similar_file);
        cleanup_shmem(embedding_handle, query_handle, results_handle);
        return;
    }

    println!("\nHybrid search: '{}' + similar to '{}':\n", keyword, similar_file);
    let display_count = result_count.min(8);
    for result in results[..display_count].iter() {
        let name = entries[..dir_count]
            .iter()
            .find(|e| e.id as u32 == result.file_id)
            .map(|e| e.name_str())
            .unwrap_or("???");

        let match_type = match (result.keyword_rank > 0, result.semantic_rank > 0) {
            (true, true) => "K+S",
            (true, false) => "K  ",
            (false, true) => "  S",
            (false, false) => "   ",
        };

        if result.semantic_rank > 0 {
            let sim_pct = (result.semantic_sim * 100.0) as u32;
            println!("  [{}] {:<16} {:>3}% sim  (RRF: {})",
                match_type, name, sim_pct, result.rrf_score);
        } else {
            println!("  [{}] {:<16}          (RRF: {})",
                match_type, name, result.rrf_score);
        }
    }

    println!("\n{} result(s) - [K]=keyword [S]=semantic", display_count);
    cleanup_shmem(embedding_handle, query_handle, results_handle);
}

/// Search for files semantically similar to a given file.
fn cmd_search_similar(filename: &str) {
    let mut embedding_handle: Option<u32> = None;
    let mut query_handle: Option<u32> = None;
    let mut results_handle: Option<u32> = None;

    let emb_count = match embedding_count() {
        Ok(c) => c,
        Err(_) => { println!("search: Synapse not available"); return; }
    };
    if emb_count == 0 {
        println!("search: No embeddings available");
        println!("        Build with 'folk-pack create-sqlite --embed'");
        return;
    }

    let mut entries = [DirEntry { id: 0, entry_type: 0, name: [0u8; 32], size: 0 }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);
    let source_file = entries[..dir_count].iter().find(|e| e.name_str() == filename);
    let source_entry = match source_file {
        Some(e) => e,
        None => { println!("search: '{}' not found", filename); return; }
    };
    let file_id = source_entry.id as u32;

    let embedding_response = match get_embedding(file_id) {
        Ok(r) => r,
        Err(_) => { println!("search: '{}' has no embedding", filename); return; }
    };

    if shmem_map(embedding_response.shmem_handle, VECTOR_QUERY_VADDR).is_err() {
        println!("search: failed to map embedding");
        return;
    }
    embedding_handle = Some(embedding_response.shmem_handle);

    let query_shmem = match shmem_create(4096) {
        Ok(h) => h,
        Err(_) => {
            println!("search: failed to create query buffer");
            cleanup_shmem(embedding_handle, None, None);
            return;
        }
    };
    query_handle = Some(query_shmem);

    if shmem_grant(query_shmem, SYNAPSE_TASK_ID).is_err() {
        println!("search: failed to grant query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    if shmem_map(query_shmem, VECTOR_RESULTS_VADDR).is_err() {
        println!("search: failed to map query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    unsafe {
        let src = VECTOR_QUERY_VADDR as *const u8;
        let dst = VECTOR_RESULTS_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, EMBEDDING_SIZE);
    }

    let k = 5;
    let search_response = match vector_search(query_shmem, k) {
        Ok(r) => r,
        Err(_) => {
            println!("search: vector search failed");
            cleanup_shmem(embedding_handle, query_handle, None);
            return;
        }
    };

    if search_response.count == 0 {
        println!("No similar files found.");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    if shmem_map(search_response.shmem_handle, VECTOR_RESULTS_VADDR).is_err() {
        println!("search: failed to map results");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }
    results_handle = Some(search_response.shmem_handle);

    println!("\nFiles similar to '{}':\n", filename);

    let results_ptr = VECTOR_RESULTS_VADDR as *const u8;
    for i in 0..search_response.count {
        let offset = i * 8;
        let result_file_id = unsafe { *(results_ptr.add(offset) as *const u32) };
        let similarity = unsafe { *(results_ptr.add(offset + 4) as *const f32) };

        if result_file_id == file_id { continue; }

        let result_name = entries[..dir_count]
            .iter()
            .find(|e| e.id as u32 == result_file_id)
            .map(|e| e.name_str())
            .unwrap_or("???");

        let sim_pct = (similarity * 100.0) as u32;
        println!("  {:<16} ({:>3}% similar)", result_name, sim_pct);
    }
    println!();

    cleanup_shmem(embedding_handle, query_handle, results_handle);
}

/// Helper to clean up shared memory after search operations.
fn cleanup_shmem(embedding: Option<u32>, query: Option<u32>, results: Option<u32>) {
    if let Some(h) = embedding {
        let _ = shmem_unmap(h, VECTOR_QUERY_VADDR);
    }
    if let Some(h) = query {
        let _ = shmem_unmap(h, VECTOR_RESULTS_VADDR);
        let _ = shmem_destroy(h);
    }
    if let Some(h) = results {
        let _ = shmem_unmap(h, VECTOR_RESULTS_VADDR);
    }
}

/// Convert filename to lowercase (in-place buffer)
fn name_to_lowercase(name: &str) -> [u8; 32] {
    let mut lower = [0u8; 32];
    for (i, &b) in name.as_bytes().iter().enumerate() {
        if i >= 32 { break; }
        lower[i] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
    }
    lower
}

/// Check if haystack contains needle (case-insensitive on both sides)
fn contains_substring(haystack: &[u8; 32], needle: &str) -> bool {
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() { return true; }

    let mut haystack_len = 0;
    for &b in haystack.iter() {
        if b == 0 { break; }
        haystack_len += 1;
    }
    if haystack_len < needle_bytes.len() { return false; }

    for i in 0..=(haystack_len - needle_bytes.len()) {
        let mut matches = true;
        for (j, &nb) in needle_bytes.iter().enumerate() {
            if haystack[i + j] != nb { matches = false; break; }
        }
        if matches { return true; }
    }
    false
}
