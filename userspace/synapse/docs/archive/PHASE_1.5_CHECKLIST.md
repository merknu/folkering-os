# Synapse Phase 1.5: Critical Fixes Checklist

**Goal:** Achieve spec compliance for core storage and ingestion
**Timeline:** 4 days
**Blocker:** Must complete before Phase 2

---

## Day 1: Relative Path Resolution

### Task 1.1: Add Project Metadata Table
```bash
# File: migrations/002_project_metadata.sql
```

- [ ] Create `project_meta` table
- [ ] Add helper to get/set project root
- [ ] Default to current working directory

**Test:**
```bash
cargo run --example test_project_meta
# Expected: Can set and retrieve root path
```

### Task 1.2: Update GraphDB
```bash
# File: src/graph/mod.rs
```

- [ ] Add `project_root: PathBuf` field to GraphDB
- [ ] Load project root on initialization
- [ ] Implement `to_relative()` helper
- [ ] Implement `to_absolute()` helper

**Test:**
```bash
cargo run --example test_relative_paths
# Expected: Paths stored as relative, resolved to absolute
```

### Task 1.3: Update All Path Operations
```bash
# Files: src/graph/mod.rs, src/observer/mod.rs
```

- [ ] `register_path()` converts to relative
- [ ] `get_node_by_path()` resolves from relative
- [ ] Observer converts watch events to relative

**Test:**
```bash
cargo run --example test_portability
# 1. Create database with files
# 2. Move database to different directory
# 3. Query files
# Expected: All files still found
```

---

## Day 2: Debounced Observer

### Task 2.1: Create Debouncer Module
```bash
# File: src/observer/debouncer.rs
```

- [ ] Define `DEBOUNCE_INTERVAL = 1.0s`
- [ ] Define `IGNORE_EXTENSIONS`
- [ ] Define `IGNORE_DIRS`
- [ ] Implement `is_ignored()` logic

**Test:**
```bash
cargo test test_ignore_patterns
# Expected: .swp, .tmp, node_modules filtered
```

### Task 2.2: Implement Debouncing State Machine
```bash
# File: src/observer/debouncer.rs
```

- [ ] Add `pending: HashMap<PathBuf, Instant>`
- [ ] Implement `handle_event()` with timer
- [ ] Coalesce rapid events
- [ ] Only process after settling

**Test:**
```bash
cargo run --example test_debouncing
# Simulate: CREATE .swp, MODIFY .swp, RENAME → file.txt
# Expected: Only final file.txt processed
```

### Task 2.3: Handle Atomic Moves
```bash
# File: src/observer/mod.rs
```

- [ ] Detect `FileMovedEvent`
- [ ] Treat as DELETE(src) + CREATE(dest)
- [ ] Update database path mapping

**Test:**
```bash
cargo run --example test_atomic_write
# Use real editor to save file
# Expected: Single index event after save completes
```

---

## Day 3: Content Hashing

### Task 3.1: Add Hash Computation
```bash
# File: src/graph/hash.rs
```

- [ ] Add `sha2` dependency to Cargo.toml
- [ ] Implement `compute_file_hash(path) -> String`
- [ ] Use SHA-256 algorithm
- [ ] Stream file in 8KB chunks

**Test:**
```bash
cargo test test_hash_computation
# Expected: Same file = same hash, different file = different hash
```

### Task 3.2: Store Hashes in Database
```bash
# File: migrations/003_add_content_hash.sql
```

- [ ] Add `content_hash TEXT` to resources table
- [ ] Update on file index
- [ ] Compare before re-indexing

**Test:**
```bash
cargo run --example test_hash_comparison
# 1. Index file.txt
# 2. Touch file.txt (mtime changes)
# 3. Re-index
# Expected: Skipped (hash unchanged)
```

### Task 3.3: Optimize Index Pipeline
```bash
# File: src/observer/mod.rs
```

- [ ] Compute hash first
- [ ] Query existing hash from database
- [ ] Skip if hash matches
- [ ] Only run NER/embedding if hash changed

**Test:**
```bash
cargo run --example test_skip_unchanged
# Expected: Log shows "File unchanged, skipping"
```

---

## Day 4: Session Persistence

### Task 4.1: Persist Sessions to Database
```bash
# File: src/observer/mod.rs
```

- [ ] Insert new session on creation
- [ ] Update `is_active` when session expires
- [ ] Set `ended_at` timestamp

**Test:**
```bash
cargo test test_session_lifecycle
# Expected: Session exists in database with correct timestamps
```

### Task 4.2: Persist Session Events
```bash
# File: src/observer/mod.rs
```

- [ ] Insert event on file access
- [ ] Link to session_id
- [ ] Store event_type (open/edit/close)

**Test:**
```bash
cargo run --example test_session_events
# 1. Access file1.txt
# 2. Access file2.txt
# 3. Query session_events
# Expected: 2 rows with correct session_id
```

### Task 4.3: Enable Temporal Queries
```bash
# File: src/query/mod.rs
```

- [ ] Implement `find_by_timeframe()`
- [ ] Use session_events table
- [ ] Filter by timestamp range

**Test:**
```bash
cargo run --example test_temporal_query
# Expected: "Files accessed today" returns correct results
```

---

## Verification Tests

### End-to-End Test Suite

**Test 1: Portability**
```bash
cargo run --example test_full_portability
```
1. Create project at `/tmp/project1`
2. Add 10 files
3. Index all files
4. Move database to `/tmp/project2`
5. Query all files
6. **Expected:** All 10 files found

**Test 2: Real Editor Integration**
```bash
cargo run --example test_vscode_integration
```
1. Start observer
2. Open VSCode
3. Edit file.txt
4. Save (atomic write)
5. **Expected:** Single "modified" event, no .swp entries

**Test 3: Performance (Hash Skipping)**
```bash
cargo run --example test_performance
```
1. Index 100 files
2. Touch all 100 files (change mtime)
3. Re-index
4. **Expected:** All 100 skipped (hash unchanged), completes <1s

**Test 4: Session Analysis**
```bash
cargo run --example test_session_analysis
```
1. Access file1, file2, file3 within 1 minute
2. Wait 6 minutes
3. Access file4, file5
4. Query "files from first session"
5. **Expected:** file1, file2, file3

---

## Success Criteria

After Day 4, the following must be true:

- [ ] **Portability:** Database works after moving to new directory
- [ ] **Robustness:** Atomic writes handled correctly (no .swp files indexed)
- [ ] **Performance:** Unchanged files not re-indexed (hash comparison)
- [ ] **Queries:** Can query "what did I work on yesterday"
- [ ] **All Tests Pass:** 10/10 verification tests green

---

## Known Issues & Workarounds

### Issue 1: sqlite-vec Not Available
**Problem:** sqlite-vec requires C compilation

**Workaround:** Skip Phase 2 vector embeddings for now, focus on Phase 1.5

**Long-term:** Bundle pre-compiled extension or use libsql

### Issue 2: ONNX Runtime in Rust
**Problem:** No mature Rust ONNX bindings for GLiNER

**Workaround:** Create Python subprocess for NER (Phase 2)

**Long-term:** Port to tract or burn if pure-Rust required

### Issue 3: Windows Path Separator
**Problem:** `\` vs `/` in relative paths

**Workaround:** Always convert to `/` before storing:
```rust
let relative = relative.to_string_lossy().replace('\\', '/');
```

---

## Rollback Plan

If Phase 1.5 breaks existing functionality:

1. **Backup current code:**
   ```bash
   git checkout -b phase-1-stable
   git tag v0.1.0-phase1
   ```

2. **Revert changes:**
   ```bash
   git checkout main
   git revert <commit-hash>
   ```

3. **Re-run Phase 1 tests:**
   ```bash
   cargo run --example populate_graph
   cargo run --example watch_files
   ```

4. **Fix forward instead of reverting:**
   - Identify failing test
   - Fix specific issue
   - Iterate

---

## Daily Progress Tracker

### Day 1 (Relative Paths)
- [x] Started: 2026-01-25
- [x] Completed: 2026-01-25
- [x] Tests Pass: 5/5 (portability, populate_graph, watch_files, path normalization, cross-platform)
- [x] Notes: All tests passed! Database now portable. See PHASE_1.5_DAY_1_COMPLETE.md

### Day 2 (Debouncing)
- [x] Started: 2026-01-25
- [x] Completed: 2026-01-25
- [x] Tests Pass: 5/5 (coalescing, atomic writes, ignore patterns, multiple files, VSCode)
- [x] Notes: All tests passed! Handles Vim, VSCode, Emacs. See PHASE_1.5_DAY_2_COMPLETE.md

### Day 3 (Hashing)
- [x] Started: 2026-01-25
- [x] Completed: 2026-01-25
- [x] Tests Pass: 14/14 (6 unit tests + 8 integration tests)
- [x] Notes: All tests passed! SHA-256 hashing, skip-on-unchanged working. See PHASE_1.5_DAY_3_COMPLETE.md

### Day 4 (Sessions)
- [x] Started: 2026-01-25
- [x] Completed: 2026-01-25
- [x] Tests Pass: 8/8 (session creation, events, queries, lifecycle, temporal, stats, multiple)
- [x] Notes: All tests passed! Sessions persisted, temporal queries working. See PHASE_1.5_DAY_4_COMPLETE.md

---

## Next Steps After Phase 1.5

Once all checkboxes complete:

1. **Code Review:** Read all changes
2. **Documentation:** Update README with new features
3. **Benchmarks:** Run performance tests
4. **Merge:** Merge to main with tag `v0.2.0-phase1.5`
5. **Plan Phase 2:** Review REVISED_PLAN.md for neural integration

**DO NOT proceed to Phase 2 until Phase 1.5 complete.**
