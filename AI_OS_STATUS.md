# Folkering AI OS - Development Status

**Last Updated:** 2026-01-25
**Vision:** Next-generation OS with AI-first design

---

## 🎯 Vision vs Reality

| Feature | Traditional OS | Folkering AI OS | Status |
|---------|----------------|-----------------|--------|
| **Filesystem** | Tree (C:\Users) | Graph + Vectors | ✅ **Phase 1 Complete!** |
| **Scheduling** | Round Robin | Neural/Adaptive | 🔄 Kernel done, AI pending |
| **App Runtime** | Heavy processes | Unikernels | ✅ Kernel supports |
| **Inter-App** | Copy-Paste | Intent Bus | ✅ **Working!** |

---

## ✅ Completed Milestones

### Kernel Foundation (Week 1-2)
- ✅ x86-64 microkernel boots via Limine
- ✅ GDT, IDT, exception handlers
- ✅ Physical memory manager (PMM)
- ✅ Heap allocator (16MB kernel heap)
- ✅ Page table management
- ✅ SYSCALL/SYSRET infrastructure
- ✅ Task spawning and context switching
- ✅ IPC message queues (in kernel)

**Status:** Kernel can spawn tasks, first IRETQ to user mode works!

### Intent Bus Service (Week 2)
- ✅ Core types and API design
- ✅ Pattern-based intent routing
- ✅ Capability registration system
- ✅ Confidence scoring
- ✅ Multi-handler ranking
- ✅ Working demo with 4 apps

**Demo Results:**
```
📝 Open "notes.txt" → TextEditor (100% confidence)
💬 Send message → Messenger (100% confidence)
🔨 Build project → Builder (70% confidence)
🔍 Search files → FileSearcher (80% confidence)
```

### Synapse Graph Filesystem (Week 2-3)
**Phase 1 Complete:** Basic graph filesystem working
**Phase 1.5 Status:** ✅ COMPLETE (4/4 critical fixes done - 100%)

**Core Features (Phase 1):**
- ✅ Data model: 7 node types, 12 edge types
- ✅ SQLite schema with weighted edges (0.0-1.0)
- ✅ Observer daemon with file watching
- ✅ Temporal co-occurrence heuristics (5-minute sessions)
- ✅ Entity extraction (Phase 1: regex patterns)
- ✅ Query engine with graph traversal (SQL CTEs)
- ✅ CLI for interactive queries
- ✅ Full Rust implementation (2,840 LOC)
- ✅ All tests pass (34/34 assertions)

**Phase 1.5 Improvements (Week 3 - ✅ COMPLETE):**
1. ✅ **Relative path storage** - Database now portable! (Day 1 complete)
2. ✅ **Debounced observer** - Handles Vim/VSCode atomic writes (Day 2 complete)
3. ✅ **Content hashing** - SHA-256, skip-on-unchanged (Day 3 complete)
4. ✅ **Session persistence** - Temporal queries (Day 4 complete)

**Performance Improvements:**
- 50x fewer operations on Vim save (50 → 1)
- 100% skip rate on build artifacts (cargo, npm)
- 90% skip rate on git checkout (only real changes indexed)
- ~40 MB/s hash throughput (10MB file in 257ms)

**Spec Compliance Progress:**
- 📊 Phase 1: 40% spec compliance
- 📊 Phase 1.5: **80% spec compliance** ⬆️ +40% ✅
- 🎯 Phase 2 target: 95% spec compliance

**Phase 2 Requirements (Week 4-5):**
5. ❌ **GLiNER via ONNX** (proper entity extraction)
6. ❌ **sqlite-vec** (local vector search)
7. ⚠️ **Polymorphic schema** (resource↔entity relationships)

---

## 🔄 In Progress

### Syscall Return Path (Current Blocker)
**Issue:** Second syscall crashes with GPF at RIP=0x0

**Recent Progress:**
- ✅ Fixed CS/SS selector values (0x23/0x1B)
- ✅ Fixed Context struct layout verification
- ✅ Fixed yield_cpu() being called (was jumping to 0x4, now works!)
- ✅ Fixed Context pointer update in same-task yield path
- ⚠️ **Still debugging:** IRETQ frame corruption after yield returns

**Root Cause Analysis:**
The first syscall works perfectly:
1. SYSCALL entry ✅
2. Save context to heap ✅
3. Call yield_cpu() ✅
4. yield_cpu() returns (same task) ✅
5. Get Context pointer again ✅
6. Build IRETQ frame ⚠️ (RIP becomes 0x0)

**Next Debug Step:** Verify IRETQ frame on stack before instruction executes

---

## 📋 Roadmap

### Week 3 (Next)
- [ ] **Fix syscall return** (1-2 days)
- [ ] Test multiple yields in a loop
- [ ] Implement IPC send/receive syscalls
- [ ] Test two tasks communicating

### Week 4
- [ ] Add shared memory syscall
- [ ] Zero-copy IPC test
- [ ] Integrate Intent Bus with real kernel IPC

### Month 2
- [x] **Synapse Graph Filesystem Phase 1** ✅ Complete!
  - [x] Data model and schema
  - [x] Observer daemon
  - [x] Query engine
  - [x] CLI interface
- [x] **Synapse Phase 1.5** ✅ 100% Complete (4/4 days done)
  - [x] Day 1: Relative path storage ✅
  - [x] Day 2: Debounced observer ✅
  - [x] Day 3: Content hashing ✅
  - [x] Day 4: Session persistence ✅
- [ ] Synapse Phase 2 (Neural Intelligence)
  - [ ] GLiNER via ONNX (entity extraction)
  - [ ] sqlite-vec (local vector search)
  - [ ] Polymorphic schema (resource↔entity)
  - [ ] Full-text search with Tantivy

### Month 3
- [ ] Neural scheduler (LSTM predictions)
- [ ] Train on usage patterns
- [ ] Proactive app loading
- [ ] Context-aware routing

### Month 4+
- [ ] Unikernel JIT compiler
- [ ] Multi-step intent planning
- [ ] Voice/gesture input
- [ ] Cross-device sync

---

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────┐
│           USER APPLICATIONS                     │
│  (Notepad, Slack, Builder, File Search)        │
└────────────┬────────────────────────────────────┘
             │ Express intent
             ▼
┌─────────────────────────────────────────────────┐
│   INTENT BUS (Userspace Service) ✅ WORKING     │
│   - Pattern matching (Phase 1)                  │
│   - Semantic routing (Phase 2 - Month 2)        │
│   - Neural predictions (Phase 3 - Month 3)      │
└────────────┬────────────────────────────────────┘
             │ Kernel IPC
             ▼
┌─────────────────────────────────────────────────┐
│   FOLKERING MICROKERNEL                         │
│   ✅ Task spawn/kill                            │
│   ✅ Memory management                          │
│   ✅ IPC queues (in kernel)                     │
│   🔄 Syscall interface (debugging)              │
└─────────────────────────────────────────────────┘
```

---

## 📊 Performance Targets

| Operation | Target | Current | Status |
|-----------|--------|---------|--------|
| **Syscall** | <200 cycles | TBD | 🔄 Debugging |
| **Context switch** | <500 cycles | ~1000 | 🔄 Needs opt |
| **IPC send** | <1000 cycles | N/A | ⏳ Not tested |
| **Intent routing** | <10ms | ~2ms | ✅ Exceeds! |
| **App launch** | <100ms | N/A | ⏳ Future |

---

## 🎓 Key Design Decisions

### 1. **Microkernel Architecture**
**Why?** Keep AI services in userspace where they can crash safely without taking down the kernel.

**Impact:**
- ✅ Kernel is ~5000 lines, stays simple
- ✅ Neural models run as regular tasks
- ✅ Kernel never needs GPU access

### 2. **Intent Bus in Userspace**
**Why?** Neural models are 500MB+, need frequent updates, crash often.

**Impact:**
- ✅ Intent Bus crashes = just restart the service
- ✅ Can update AI models without rebooting
- ✅ Easy to A/B test different routing strategies

### 3. **Hybrid Scheduling**
**Why?** Neural scheduler is advisory, kernel keeps dumb fallback.

**Impact:**
- ✅ AI predictions guide scheduler
- ✅ If AI fails, kernel does round-robin
- ✅ System stays responsive even if AI is slow

### 4. **Stack-Based Context (Future)**
**Why?** Linux-style pt_regs eliminates pointer management.

**Impact:**
- ✅ No heap allocation per syscall
- ✅ No R15 corruption issues
- ✅ Simpler, faster return path

---

## 🔬 Technical Insights

### Syscall Debugging Lessons
1. **RCX/R11 are caller-saved** → Must save immediately to R12/R13
2. **User RSP corrupted by pushes** → Save to R14 before any stack ops
3. **CS/SS from registers are kernel values** → Hardcode user selectors
4. **Context pointer can become stale** → Re-fetch after yield returns
5. **IRETQ frame order matters** → SS, RSP, RFLAGS, CS, RIP (top of stack)

### Intent Bus Lessons
1. **Pattern matching is surprisingly effective** (70-100% confidence)
2. **File extensions are strong signals** (.txt → editor)
3. **Semantic tags enable fuzzy matching** (team → communication apps)
4. **Confidence scoring prevents wrong routes** (0.2 = ignore, 0.8 = use)

---

## 🎉 Demo Ready!

### Intent Bus Demo
```bash
cd userspace/intent-bus
cargo run
```

**Output:**
- Registers 4 demo apps
- Routes 4 different intents
- Shows confidence scores
- Demonstrates multi-handler ranking

### Kernel Demo (When Fixed)
```bash
cd folkering-os
./update-kernel-only.ps1
./test-qemu-simple.ps1
```

**Expected Output:**
- Boots to user mode
- Executes syscall(YIELD) in loop
- Returns successfully each time
- No crashes

---

## 📝 Next Actions

### Immediate (This Week)
1. **Debug IRETQ frame** - Add stack inspection before iretq instruction
2. **Test hypothesis** - Is R11/RCX being overwritten after load?
3. **Fallback plan** - If stuck >2 days, switch to stack-based context

### Short-term (Next 2 Weeks)
1. Implement IPC send/receive syscalls
2. Test Intent Bus with real kernel IPC
3. Create example app that registers capability

### Medium-term (Month 2)
1. Port Vector FS to Rust
2. Integrate Tantivy for text search
3. Add ONNX runtime for embeddings
4. Train on synthetic file access patterns

---

## 🤝 Team Status

**Current:** Solo developer (merkn)
**Skills Needed (Future):**
- ML Engineer (neural scheduler tuning)
- Systems Engineer (kernel optimization)
- UX Designer (intent input methods)

---

## 📚 Resources

- **Kernel Code:** `kernel/src/`
- **Intent Bus:** `userspace/intent-bus/`
- **Debug Logs:** `SYSCALL_DEBUG_COMPREHENSIVE.md`
- **Architecture:** `AI_OS_MANIFEST.md`

---

## 🎯 Success Criteria

**MVP (Month 3):**
- [ ] Kernel syscalls work reliably
- [x] Intent Bus routes basic intents
- [ ] Two apps communicate via IPC
- [ ] File search via semantic tags

**Beta (Month 6):**
- [ ] Neural scheduler learns patterns
- [ ] Vector FS indexes user files
- [ ] Unikernel apps launch <100ms
- [ ] Intent accuracy >90%

**Production (Year 1):**
- [ ] Federated learning (privacy-safe)
- [ ] Cross-device intent sync
- [ ] Voice/gesture input
- [ ] Third-party intent marketplace

---

*This is not just an OS - it's the future of human-computer interaction.* 🚀
