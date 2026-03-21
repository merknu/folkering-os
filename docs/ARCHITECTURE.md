# Folkering OS Architecture

## Design Principles

1. **Microkernel**: The kernel only handles memory, scheduling, and IPC. Everything else lives in userspace.
2. **Data-Driven UI**: Apps never touch pixels. They describe what they want; the Compositor renders it.
3. **Zero-Copy IPC**: Shared memory regions are mapped into multiple task page tables. No data copying between tasks.
4. **AI-First**: Synapse (the data kernel) provides SQLite, semantic search, and will eventually run on-device inference.

## Memory Model

### Physical Memory
- **Bootstrap allocator**: Bump allocator from boot, ~508 MB available
- **Buddy allocator**: Power-of-2 block allocation (not yet populated from bootstrap)
- **alloc_page()**: Single-page allocation via bootstrap (used by shmem + mmap)
- **free_pages()**: Returns pages to PMM

### Virtual Address Space

```
0x0000_0000_0020_0000  Userspace ELF segments (.text, .rodata, .data, .bss)
0x0000_0000_2000_0000  Shell shmem window (SHELL_SHMEM_VADDR)
0x0000_0000_3000_0000  Compositor shmem window (COMPOSITOR_SHMEM_VADDR)
0x0000_0000_4000_0000  Anonymous mmap region (SYS_MMAP base)
0x0000_0001_0000_0000  Compositor framebuffer (mapped from physical FB)
0x7FFF_FFFA_0000_0000  User stack top
0xFFFF_8000_0000_0000  HHDM (Higher Half Direct Map)
0xFFFF_FFFF_8000_0000  Kernel .text, .data, .bss
```

### Page Table Isolation
Each task has its own PML4. Kernel PML4 entries (upper half) are shared across all tasks. User PML4 entries (lower half) are task-private.

**Critical invariant**: `shmem_map` and `mmap` use `map_page_in_table(task_pml4, ...)` — never the global kernel MAPPER.

## IPC Model

### Synchronous IPC
```
Task A: syscall3(SYS_IPC_SEND, target_task, payload0, payload1)
  → Kernel: enqueue message, block sender, wake target
  → Target: recv_async() → process → reply_with_token(token, result, 0)
  → Kernel: unblock sender, set Context.rax = result
  → Task A resumes with return value in RAX
```

### CallerToken Pattern
Servers use `recv_async()` which returns a `CallerToken`. The server can do arbitrary work (including calling other services) before replying. The token encodes (sender_pid, msg_id) for secure reply routing.

### IPC via Intent Service
The Intent Service (Task 5) provides capability-based routing:
```
Compositor → Intent Service → Shell/Synapse → reply chain back
```
This adds 2 extra context switches but provides:
- Capability checking (CAP_FILE_OPS, CAP_PROCESS_OPS, etc.)
- Dynamic handler registration
- Future: load balancing, access control

## Shared Memory Protocol

### Lifecycle
```
Creator: shmem_create(size) → handle
Creator: shmem_map(handle, vaddr)
Creator: write data to vaddr
Creator: shmem_grant(handle, target_task)
Creator: shmem_unmap(handle, vaddr)
Creator: return handle to consumer via IPC

Consumer: shmem_map(handle, own_vaddr)
Consumer: read data
Consumer: shmem_unmap(handle, own_vaddr)
Consumer: shmem_destroy(handle) → frees physical pages
```

### Rule: Last consumer destroys.
The task that receives shmem as the "final stop" is responsible for cleanup.

## App Weaver (Native UI Schema)

### Wire Protocol
```
Header: [magic:"FKUI"][ver:1][title_len:1][width:2][height:2][title:N]
Widget: [tag:1][...type-specific...][children recursively]
```

Tags:
- `0x01` Label: `[text_len:1][color:4][text:N]`
- `0x02` Button: `[label_len:1][action_id:4][bg:4][fg:4][label:N]`
- `0x03` VStack: `[spacing:2][count:1][...children...]`
- `0x04` HStack: `[spacing:2][count:1][...children...]`
- `0x05` Spacer: `[height:2]`

### Flow
1. Shell allocates shmem, builds widget tree with `UiWriter`
2. Returns shmem handle to Compositor via IPC response
3. Compositor: `parse_header()` → `parse_widget_tree()` → creates `Window` with `WindowKind::App`
4. Compositor renders widgets each frame
5. Mouse click → `hit_test_widgets()` → `action_id` sent back to Shell via IPC

### Event Protocol
Button click sends `0xAC10 | (action_id << 16) | (win_id << 48)` to the window's `owner_task`.

## Synapse (Data Kernel)

### SQLite Backend
- Custom `libsqlite`: no_std B-tree reader (no C dependency)
- `files` table: `(id, name, kind, size, data BLOB)`
- Pre-cached at boot into `DIR_CACHE_STATE` for fast lookups
- `MAX_DB_SIZE`: 256 KB buffer for database loading

### Operations
| Opcode | Name | Purpose |
|--------|------|---------|
| 0x0001 | SYN_OP_LIST_FILES | List files via shmem |
| 0x0008 | SYN_OP_READ_FILE_SHMEM | Read file BLOB via shmem |
| 0x0020 | SYN_OP_VECTOR_SEARCH | Semantic vector search |

## Kernel Syscall Table

| Number | Name | Arguments | Returns |
|--------|------|-----------|---------|
| 0x00 | IPC_SEND | target, payload0, payload1 | reply value |
| 0x01 | IPC_RECEIVE | flags | packed(sender, payload) |
| 0x03 | SHMEM_CREATE | size | handle |
| 0x04 | SHMEM_MAP | handle, vaddr | 0/error |
| 0x07 | YIELD | — | 0 |
| 0x0D | FS_READ_DIR | buf_ptr, buf_size | count |
| 0x0E | FS_READ_FILE | name_ptr, buf_ptr, size | bytes_read |
| 0x20 | IPC_RECV_ASYNC | — | packed(token, payload) |
| 0x21 | IPC_REPLY_TOKEN | token, payload0, payload1 | 0/error |
| 0x26 | TASK_LIST_DETAILED | buf_ptr, buf_size | count |
| 0x30 | MMAP | hint_addr, size, flags | virt_addr |
| 0x31 | MUNMAP | vaddr, size | 0/error |

## Context Switch

### Timer Preemption (userspace only)
```
Timer IRQ → check CS RPL → if ring 3:
  push all GPRs → FXSAVE → call timer_preempt_handler
  → save context to task.context
  → schedule_next() → switch page table
  → update FXSAVE_CURRENT_PTR
  → FXRSTOR → restore GPRs from new context → IRETQ
```

### Voluntary Yield (from syscall)
```
ipc_send() blocks → yield_cpu() → schedule_next()
  → switch page table → FXSAVE_CURRENT_PTR
  → restore_context_only(target_ctx) → IRETQ to new task
```

## Key Lessons Learned

1. **shmem_map must use task's PML4** — mapping to kernel PML4 causes #PF in userspace
2. **receive() truncates to 32 bits** — use recv_async() for full 64-bit payloads
3. **B-tree right_pointer must be followed exactly once** — infinite loop if re-entered
4. **Panic handler needs recursion guard** — recursive #PF → #GP cascade
5. **Bootstrap allocator has all the pages** — buddy allocator starts empty
