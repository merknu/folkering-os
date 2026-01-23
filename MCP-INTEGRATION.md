# MCP Integration for Folkering OS

## Vision: Direct AI-to-OS Communication

The idea is to create MCP (Model Context Protocol) servers that allow Claude Code to directly interact with:
1. **The Folkering OS** (once booted with network)
2. **WSL Ubuntu environment** (for build/test automation)

This would enable iterative development with instant feedback loops.

---

## Part 1: MCP Server for WSL Ubuntu

### Purpose
Give Claude Code direct access to the WSL environment for:
- Building the kernel
- Running tests
- Creating ISOs
- Launching QEMU
- Reading logs

### Implementation

Create an MCP server that runs in WSL and exposes tools:

```json
{
  "tools": [
    {
      "name": "wsl_exec",
      "description": "Execute command in WSL",
      "parameters": {
        "command": "string",
        "timeout": "number"
      }
    },
    {
      "name": "wsl_build_kernel",
      "description": "Build Folkering OS kernel",
      "parameters": {
        "release": "boolean"
      }
    },
    {
      "name": "wsl_create_iso",
      "description": "Create bootable ISO with Limine"
    },
    {
      "name": "wsl_run_qemu",
      "description": "Boot kernel in QEMU",
      "parameters": {
        "memory": "string",
        "debug": "boolean",
        "gdb": "boolean"
      }
    },
    {
      "name": "wsl_read_file",
      "description": "Read file from WSL filesystem",
      "parameters": {
        "path": "string"
      }
    },
    {
      "name": "wsl_write_file",
      "description": "Write file to WSL filesystem",
      "parameters": {
        "path": "string",
        "content": "string"
      }
    }
  ]
}
```

### Setup Steps

1. **Install Node.js in WSL**:
   ```bash
   curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash -
   sudo apt install -y nodejs
   ```

2. **Create MCP Server**:
   ```bash
   cd ~/
   mkdir wsl-mcp-server
   cd wsl-mcp-server
   npm init -y
   npm install @anthropic/mcp
   ```

3. **Implement Server** (`index.js`):
   ```javascript
   #!/usr/bin/env node
   import { Server } from "@modelcontextprotocol/sdk/server/index.js";
   import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
   import { exec } from 'child_process';
   import { promisify } from 'util';
   import fs from 'fs/promises';

   const execAsync = promisify(exec);
   const server = new Server({
     name: "wsl-folkering-server",
     version: "1.0.0"
   }, {
     capabilities: {
       tools: {}
     }
   });

   // Tool: Execute command
   server.setRequestHandler("tools/call", async (request) => {
     const { name, arguments: args } = request.params;

     switch (name) {
       case "wsl_exec": {
         const { stdout, stderr } = await execAsync(args.command, {
           timeout: args.timeout || 30000
         });
         return { content: [{ type: "text", text: stdout || stderr }] };
       }

       case "wsl_build_kernel": {
         const cmd = args.release ?
           "cd ~/folkering/kernel && cargo build --release" :
           "cd ~/folkering/kernel && cargo build";
         const { stdout } = await execAsync(cmd, { timeout: 300000 });
         return { content: [{ type: "text", text: stdout }] };
       }

       case "wsl_run_qemu": {
         const memory = args.memory || "512M";
         const debug = args.debug ? "-d int,cpu_reset" : "";
         const gdb = args.gdb ? "-s -S" : "";
         const cmd = `cd ~/folkering/kernel && qemu-system-x86_64 -cdrom folkering.iso ${debug} ${gdb} -m ${memory} -serial stdio`;
         const { stdout } = await execAsync(cmd, { timeout: 60000 });
         return { content: [{ type: "text", text: stdout }] };
       }

       case "wsl_read_file": {
         const content = await fs.readFile(args.path, 'utf-8');
         return { content: [{ type: "text", text: content }] };
       }

       case "wsl_write_file": {
         await fs.writeFile(args.path, args.content, 'utf-8');
         return { content: [{ type: "text", text: "File written successfully" }] };
       }

       default:
         throw new Error(`Unknown tool: ${name}`);
     }
   });

   // Start server
   const transport = new StdioServerTransport();
   await server.connect(transport);
   ```

4. **Configure in Claude Code**:
   Add to `~/.claude/mcp.json`:
   ```json
   {
     "mcpServers": {
       "wsl-folkering": {
         "command": "wsl",
         "args": ["-d", "Ubuntu-22.04", "node", "/home/knut/wsl-mcp-server/index.js"]
       }
     }
   }
   ```

---

## Part 2: MCP Server for Folkering OS

### Purpose
Once the OS boots with network connectivity, allow Claude to:
- Execute syscalls directly
- Test IPC between processes
- Monitor system stats
- Debug issues in real-time
- Deploy and test userspace programs

### Architecture

```
┌─────────────────┐
│  Claude Code    │
│   (Windows)     │
└────────┬────────┘
         │ MCP Protocol (TCP/WebSocket)
         │
    ┌────▼──────────┐
    │  Folkering OS │
    │  (QEMU/VM)    │
    │               │
    │  ┌──────────┐ │
    │  │  Network │ │
    │  │  Stack   │ │
    │  └─────┬────┘ │
    │        │      │
    │  ┌─────▼────┐ │
    │  │MCP Server│ │
    │  │(Userspace│ │
    │  │ Service) │ │
    │  └─────┬────┘ │
    │        │      │
    │  ┌─────▼────┐ │
    │  │  Kernel  │ │
    │  │ Syscalls │ │
    │  └──────────┘ │
    └───────────────┘
```

### Required OS Components

1. **Network Stack** (Priority High)
   - TCP/IP implementation
   - Virtio-net driver (for QEMU)
   - Socket API
   - DHCP client (or static IP)

2. **MCP Server Service** (Userspace)
   - Listen on port (e.g., 9000)
   - Parse MCP JSON-RPC messages
   - Execute syscalls on behalf of Claude
   - Return results as MCP responses

3. **System Call Interface**
   - Expose kernel operations as syscalls
   - Debug syscall for reading kernel state
   - IPC test syscalls
   - Memory inspection syscalls

### Implementation Phases

#### Phase 1: Network Stack (Weeks 2-4)
```rust
// In kernel/src/net/mod.rs
pub mod virtio;
pub mod tcp;
pub mod socket;

// Expose syscall
SYS_SOCKET = 100,
SYS_BIND = 101,
SYS_LISTEN = 102,
SYS_ACCEPT = 103,
SYS_SEND = 104,
SYS_RECV = 105,
```

#### Phase 2: MCP Server (Week 5)
```rust
// Userspace: /sbin/mcp-server

use folkering_std::net::{TcpListener, TcpStream};
use serde_json::{Value, json};

fn main() {
    let listener = TcpListener::bind("0.0.0.0:9000").unwrap();
    println!("[MCP] Listening on port 9000...");

    for stream in listener.incoming() {
        handle_client(stream.unwrap());
    }
}

fn handle_client(mut stream: TcpStream) {
    let request: Value = read_json_rpc(&stream);

    let response = match request["method"].as_str() {
        "os/syscall" => execute_syscall(&request["params"]),
        "os/ipc_send" => test_ipc_send(&request["params"]),
        "os/memory_stats" => get_memory_stats(),
        "os/task_list" => get_task_list(),
        _ => json!({"error": "Unknown method"})
    };

    write_json_rpc(&stream, response);
}
```

#### Phase 3: MCP Tools Definition

```json
{
  "tools": [
    {
      "name": "folkering_syscall",
      "description": "Execute syscall in Folkering OS",
      "parameters": {
        "syscall": "enum[ipc_send, ipc_receive, spawn_task, ...]",
        "args": "array"
      }
    },
    {
      "name": "folkering_ipc_test",
      "description": "Send IPC message between tasks",
      "parameters": {
        "from_task": "number",
        "to_task": "number",
        "payload": "array[u64]"
      }
    },
    {
      "name": "folkering_memory_stats",
      "description": "Get memory allocation statistics"
    },
    {
      "name": "folkering_task_list",
      "description": "List all running tasks"
    },
    {
      "name": "folkering_deploy_binary",
      "description": "Deploy and run userspace binary",
      "parameters": {
        "name": "string",
        "elf_binary": "base64_string"
      }
    },
    {
      "name": "folkering_read_serial",
      "description": "Read kernel serial output (COM1)"
    }
  ]
}
```

### Network Setup for Testing

1. **QEMU with User Network**:
   ```bash
   qemu-system-x86_64 -cdrom folkering.iso \
     -netdev user,id=net0,hostfwd=tcp::9000-:9000 \
     -device virtio-net-pci,netdev=net0 \
     -serial stdio -m 512M
   ```

2. **Connect from Claude Code**:
   ```javascript
   // In MCP server (Windows side)
   const net = require('net');
   const client = net.connect(9000, 'localhost', () => {
     console.log('Connected to Folkering OS!');
   });
   ```

### Security Considerations

- **Authentication**: Add API key for MCP connections
- **Sandboxing**: Limit syscalls to non-destructive operations
- **Rate Limiting**: Prevent DOS from rapid syscalls
- **Audit Logging**: Log all Claude-initiated operations

---

## Part 3: Development Workflow with MCP

### Scenario: Testing IPC Performance

```javascript
// Claude executes:
const result = await folkering_syscall({
  syscall: "ipc_send",
  args: [task_a, task_b, [0x42, 0x0, 0x0, 0x0]]
});

console.log(`IPC latency: ${result.cycles} cycles`);
```

### Scenario: Iterative Debugging

```javascript
// 1. Deploy test binary
await folkering_deploy_binary({
  name: "/bin/test_cap",
  elf_binary: "base64_encoded_elf..."
});

// 2. Run it
const output = await folkering_syscall({
  syscall: "spawn_task",
  args: ["/bin/test_cap"]
});

// 3. Check results
const stats = await folkering_memory_stats();
console.log(`Heap usage: ${stats.heap_used} bytes`);
```

### Scenario: Automated Testing

```javascript
// Run full test suite
const tests = [
  "test_ipc_latency",
  "test_context_switch",
  "test_capability_mint",
  "test_shared_memory"
];

for (const test of tests) {
  const result = await folkering_syscall({
    syscall: "spawn_task",
    args: [`/tests/${test}`]
  });

  if (result.exit_code !== 0) {
    console.error(`Test ${test} failed!`);
    const logs = await folkering_read_serial();
    analyze_failure(logs);
  }
}
```

---

## Benefits

1. **Rapid Iteration**: Fix bugs and test instantly without manual intervention
2. **Automated Testing**: Run comprehensive test suites on every change
3. **Performance Profiling**: Measure syscall latency in real-time
4. **Interactive Debugging**: Claude can explore the OS state dynamically
5. **Continuous Integration**: Automated builds + boot tests + benchmarks

---

## Implementation Timeline

| Week | Task | Deliverable |
|------|------|-------------|
| 1 | WSL MCP Server | Claude can build kernel in WSL |
| 2-3 | Network Stack | TCP/IP working in Folkering OS |
| 4 | Virtio Driver | QEMU networking functional |
| 5 | MCP Server (OS) | Basic syscall execution from Claude |
| 6 | Testing Tools | Automated IPC benchmarks |
| 7 | Binary Deployment | Deploy userspace programs remotely |
| 8 | Full Integration | E2E workflow: code → build → test → debug |

---

## Next Steps

1. ✅ Document this vision
2. ⬜ Implement WSL MCP server (1-2 days)
3. ⬜ Test WSL MCP with kernel builds
4. ⬜ Begin network stack implementation
5. ⬜ Create MCP server for Folkering OS

---

**Author**: Claude Code + User Collaboration
**Date**: 2026-01-22
**Status**: Design Document
