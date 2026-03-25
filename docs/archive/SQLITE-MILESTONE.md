# SQLite Universal Container Milestone

---
tags: [folkering-os, milestone, sqlite, data-kernel]
created: 2026-01-29
status: complete
---

## Summary

**Date**: 2026-01-29
**Commit**: `14142ee feat: SQLite-backed Filesystem (Universal Container milestone)`
**Lines**: ~1900 new/changed

Folkering OS now uses SQLite as the universal data format. The OS is no longer just running code - it is **serving structured knowledge**.

## What Was Built

### 1. libsqlite (userspace/libsqlite/)

A minimal `#![no_std]` SQLite B-tree reader that can parse standard SQLite databases in a bare-metal environment.

**Modules**:
- `header.rs` - 100-byte database header parsing
- `varint.rs` - SQLite variable-length integer decoding
- `page.rs` - B-tree page structures (interior/leaf, table/index)
- `btree.rs` - B-tree traversal with TableScanner iterator
- `record.rs` - Record deserialization (NULL, integers, text, BLOBs)

**Key API**:
```rust
let db = SqliteDb::open(data)?;
for record in db.table_scan("files")? {
    let name = record.get(1).as_text();
    let data = record.get(4).as_blob();
}
```

### 2. folk-pack create-sqlite

New subcommand for the folk-pack tool that creates standard SQLite databases:

```bash
folk-pack create-sqlite initrd.db \
  --add synapse:elf:path/to/synapse \
  --add shell:elf:path/to/shell \
  --add hello.txt:data:path/to/hello.txt
```

**Schema**:
```sql
CREATE TABLE files (
    id INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    kind INTEGER NOT NULL,  -- 0=ELF, 1=Data
    size INTEGER NOT NULL,
    data BLOB
);
```

### 3. Synapse SQLite Backend

The Data Kernel now auto-detects SQLite databases and uses them for file operations:

- Loads `files.db` from ramdisk at startup
- Falls back to FPK format if not found
- B-tree queries replace linear cache scans
- New `SYN_OP_SQL_QUERY` operation

### 4. Shell sql Command

Users can query the database directly:

```
folk> sql "SELECT name FROM files"
synapse
shell
hello.txt

folk> sql "SELECT name, size FROM files"
synapse          29952
shell            30232
hello.txt           25
```

## Architecture

```
┌─────────────┐     IPC      ┌─────────────┐     Syscall    ┌────────┐
│   Shell     │◄────────────►│   Synapse   │◄──────────────►│ Kernel │
│             │              │ (SQLite     │                │(FPK    │
│  sql "..."  │              │  Parser)    │                │ Boot)  │
└─────────────┘              └─────────────┘                └────────┘
```

**Boot Flow**:
1. Kernel loads `initrd.fpk` containing synapse, shell, files.db, hello.txt
2. Kernel spawns Synapse (Task 2) and Shell (Task 3)
3. Synapse finds `files.db` in ramdisk and initializes SQLite backend
4. Shell sends IPC requests to Synapse for file operations

## Why SQLite?

| Benefit | Description |
|---------|-------------|
| **Universal Container** | `initrd.db` is inspectable with `sqlite3` CLI |
| **Single Source of Truth** | Files + metadata in one queryable format |
| **Zero-Copy Ready** | BLOBs can be mapped directly to shared memory |
| **AI-Native Future** | Vector search (`vec_search()`) will live next to data |

## Verified Output

```
[RAMDISK] Found Folk-Pack image: 4 entries
[RAMDISK] Entry 0: "synapse" (ELF, 29952 bytes)
[RAMDISK] Entry 1: "shell" (ELF, 30232 bytes)
[RAMDISK] Entry 2: "files.db" (DATA, 69632 bytes)
[RAMDISK] Entry 3: "hello.txt" (DATA, 25 bytes)

[SYNAPSE] Data Kernel starting (PID: 2)
[SYNAPSE] Protocol version: 1.0
[SYNAPSE] SQLite backend initialized
[SYNAPSE] Ready - database: files.db (3 files)
[SYNAPSE] Entering service loop...

Folkering Shell v0.1.0 (PID: 3)
Type 'help' for available commands.

folk>
```

## Files Changed

| File | Lines | Description |
|------|-------|-------------|
| `userspace/libsqlite/` | +947 | New no_std SQLite parser |
| `tools/folk-pack/src/main.rs` | +197 | create-sqlite subcommand |
| `userspace/synapse-service/src/main.rs` | +438 | SQLite backend |
| `userspace/shell/src/main.rs` | +178 | sql command |
| `userspace/libfolk/src/sys/synapse.rs` | +9 | SYN_OP_SQL_QUERY |
| `tools/sqlite-boot.sh` | +116 | Boot test script |

## Next Steps

1. **Full SQL parsing** - Support WHERE clauses, JOINs
2. **Vector embeddings** - `vec_search(embedding, ?) < 0.5`
3. **Write support** - INSERT, UPDATE, DELETE
4. **Index queries** - Use B-tree indexes for fast lookups

---

**This milestone marks the transition from "running code" to "serving structured knowledge".**
