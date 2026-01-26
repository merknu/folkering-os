//! Test debounced file observer with atomic write simulation
//!
//! This test simulates how modern editors save files:
//! 1. Create temp file (.swp)
//! 2. Write to temp file
//! 3. Rename temp → actual file
//!
//! Expected: Only the final file should be indexed, not the temp files.

use synapse::observer::{Observer, FileEventType};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Synapse Debouncer Test ===\n");

    let observer = Observer::new();

    // ========================================================================
    // Test 1: Basic Debouncing
    // ========================================================================

    println!("Test 1: Basic Debouncing");
    println!("Simulating rapid events on same file...");

    let test_file = PathBuf::from("test.txt");

    // Simulate rapid events (within 1 second)
    observer.record_file_event(test_file.clone(), FileEventType::Created).await;
    sleep(Duration::from_millis(100)).await;

    observer.record_file_event(test_file.clone(), FileEventType::Modified).await;
    sleep(Duration::from_millis(100)).await;

    observer.record_file_event(test_file.clone(), FileEventType::Modified).await;
    sleep(Duration::from_millis(100)).await;

    // Check pending count
    let pending = observer.pending_event_count().await;
    println!("  Pending events after rapid changes: {}", pending);
    assert_eq!(pending, 1, "Should have coalesced into 1 event");

    // Wait for debounce interval
    println!("  Waiting for debounce interval (1 second)...");
    sleep(Duration::from_secs(1) + Duration::from_millis(100)).await;

    // Process settled events
    observer.process_settled_events().await;

    let pending_after = observer.pending_event_count().await;
    println!("  Pending events after settling: {}", pending_after);
    assert_eq!(pending_after, 0, "Should have processed all events");

    println!("✅ Test 1 passed: Events coalesced correctly\n");

    // ========================================================================
    // Test 2: Atomic Write Simulation (Vim-style)
    // ========================================================================

    println!("Test 2: Atomic Write Simulation (Vim-style)");
    println!("Simulating: CREATE .swp → MODIFY .swp → RENAME → file.txt");

    let vim_temp = PathBuf::from(".report.txt.swp");
    let final_file = PathBuf::from("report.txt");

    // Step 1: Vim creates temp file
    observer.record_file_event(vim_temp.clone(), FileEventType::Created).await;
    sleep(Duration::from_millis(50)).await;

    // Step 2: Vim writes to temp file (multiple times)
    observer.record_file_event(vim_temp.clone(), FileEventType::Modified).await;
    sleep(Duration::from_millis(50)).await;
    observer.record_file_event(vim_temp.clone(), FileEventType::Modified).await;
    sleep(Duration::from_millis(50)).await;

    // Step 3: Vim renames temp → final
    observer.record_file_event(final_file.clone(), FileEventType::Renamed).await;

    // Check: .swp file should be ignored (not in pending)
    let pending_vim = observer.pending_event_count().await;
    println!("  Pending events: {}", pending_vim);
    println!("  (Note: .swp file should be filtered out)");

    // The .swp events should be ignored due to filtering
    // Only the renamed file should be pending
    assert!(pending_vim <= 1, "Should have ignored .swp events");

    println!("✅ Test 2 passed: Atomic write handled correctly\n");

    // Wait for any pending events from Test 2 to settle
    sleep(Duration::from_secs(1) + Duration::from_millis(200)).await;
    observer.process_settled_events().await;

    // ========================================================================
    // Test 3: Ignore Patterns
    // ========================================================================

    println!("Test 3: Ignore Patterns");
    println!("Testing various ignored file types...");

    let ignored_files = vec![
        PathBuf::from("file.tmp"),
        PathBuf::from("backup~"),
        PathBuf::from(".test.swp"),
        PathBuf::from("node_modules/package/index.js"),
        PathBuf::from("target/debug/main.exe"),
        PathBuf::from(".git/objects/abc123"),
    ];

    use synapse::observer::Debouncer;
    for path in &ignored_files {
        let should_ignore = Debouncer::is_ignored(path);
        println!("  {:?} -> ignored: {}", path, should_ignore);
        observer.record_file_event(path.clone(), FileEventType::Created).await;
    }

    sleep(Duration::from_millis(100)).await;

    let pending_ignored = observer.pending_event_count().await;
    println!("  Pending events after recording {} ignored files: {}", ignored_files.len(), pending_ignored);
    assert_eq!(pending_ignored, 0, "All ignored files should be filtered");

    println!("✅ Test 3 passed: Ignore patterns working\n");

    // ========================================================================
    // Test 4: Multiple Files Simultaneously
    // ========================================================================

    println!("Test 4: Multiple Files Simultaneously");
    println!("Recording events on 3 different files...");

    let file1 = PathBuf::from("src/main.rs");
    let file2 = PathBuf::from("src/lib.rs");
    let file3 = PathBuf::from("README.md");

    observer.record_file_event(file1, FileEventType::Modified).await;
    observer.record_file_event(file2, FileEventType::Modified).await;
    observer.record_file_event(file3, FileEventType::Modified).await;

    let pending_multi = observer.pending_event_count().await;
    println!("  Pending events: {}", pending_multi);
    assert_eq!(pending_multi, 3, "Should have 3 separate events");

    println!("  Waiting for debounce interval...");
    sleep(Duration::from_secs(1) + Duration::from_millis(100)).await;

    observer.process_settled_events().await;

    let pending_multi_after = observer.pending_event_count().await;
    println!("  Pending events after settling: {}", pending_multi_after);
    assert_eq!(pending_multi_after, 0, "All events should be settled");

    println!("✅ Test 4 passed: Multiple files handled independently\n");

    // ========================================================================
    // Test 5: Real-World Scenario (VSCode Save)
    // ========================================================================

    println!("Test 5: Real-World Scenario (VSCode Atomic Save)");
    println!("Simulating VSCode save pattern...");

    let vscode_file = PathBuf::from("src/app.tsx");
    let vscode_temp = PathBuf::from("src/.app.tsx.tmp");

    // VSCode pattern:
    // 1. Create temp file
    observer.record_file_event(vscode_temp.clone(), FileEventType::Created).await;
    sleep(Duration::from_millis(10)).await;

    // 2. Write to temp
    observer.record_file_event(vscode_temp.clone(), FileEventType::Modified).await;
    sleep(Duration::from_millis(10)).await;

    // 3. Rename temp → actual
    // (This would be detected as DELETE temp + CREATE actual in real file watching)
    observer.record_file_event(vscode_file.clone(), FileEventType::Created).await;

    sleep(Duration::from_millis(100)).await;

    // Temp file should be ignored, only final file should be pending
    let pending_vscode = observer.pending_event_count().await;
    println!("  Pending events: {}", pending_vscode);
    // Note: .tmp files are ignored, so we should only have the final file
    assert!(pending_vscode <= 1, "Should have ignored temp file");

    println!("✅ Test 5 passed: VSCode atomic save handled\n");

    // ========================================================================
    // Summary
    // ========================================================================

    println!("=== All Debouncer Tests Passed! ===\n");
    println!("Key Results:");
    println!("✅ Event coalescing works (rapid changes → single event)");
    println!("✅ Atomic writes handled (.swp files filtered)");
    println!("✅ Ignore patterns working (temp files, build artifacts)");
    println!("✅ Multiple files processed independently");
    println!("✅ Real-world editor patterns supported");
    println!("\nThe debouncer successfully prevents:");
    println!("- Duplicate indexing of rapidly changing files");
    println!("- Indexing of temporary/swap files");
    println!("- Processing build artifacts and dependencies");

    Ok(())
}
