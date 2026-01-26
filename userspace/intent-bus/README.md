# Intent Bus - AI-Powered Application Router

## 🎯 Vision

Replace traditional copy-paste and manual app switching with **semantic intent routing**. Users express what they want to do, and the Intent Bus routes requests to appropriate applications intelligently.

## 🏗️ Architecture

```
┌──────────────────────────────────────────────────────────┐
│  User expresses intent (any app)                         │
│  "Send 'meeting at 3pm' to the team"                     │
└────────────────────┬─────────────────────────────────────┘
                     │ Kernel IPC
                     ▼
┌──────────────────────────────────────────────────────────┐
│  Intent Bus Service (Userspace Daemon)                   │
├──────────────────────────────────────────────────────────┤
│  Phase 1: Pattern Matching     ✅ Current                │
│  Phase 2: Semantic Embeddings  🔄 Month 2                │
│  Phase 3: Neural Predictions   🔄 Month 3                │
└────────────────────┬─────────────────────────────────────┘
                     │ Routes to best handler
                     ▼
┌──────────────────────────────────────────────────────────┐
│  Messenger App (task 101)                                │
│  Executes: send_message("meeting at 3pm", ["team"])     │
└──────────────────────────────────────────────────────────┘
```

## 🚀 Current Status

**Phase 1: Pattern Matching** (✅ Implemented)
- Apps register capabilities via IPC
- Simple keyword-based routing
- Confidence scoring
- Multi-handler ranking

**Phase 2: Semantic Understanding** (🔄 Next 2 months)
- Vector embeddings for intent queries
- Semantic similarity matching
- Context-aware routing (time, location, recent apps)

**Phase 3: Neural Routing** (🔄 Month 3-4)
- LSTM/Transformer for intent prediction
- Learn user patterns (9am = IDE, evening = entertainment)
- Proactive suggestions

## 📋 Intent Types

```rust
// File operations
Intent::OpenFile { query: "my presentation" }

// Communication
Intent::SendMessage {
    text: "meeting reminder",
    recipients: ["team"],
    medium: MessageMedium::Chat
}

// Commands
Intent::RunCommand {
    command: "build",
    args: ["--release"]
}

// Transformations
Intent::Transform {
    data: csv_bytes,
    from_format: "csv",
    to_format: "chart"
}

// Content creation
Intent::Create {
    content_type: "presentation",
    initial_content: "AI OS"
}

// Search
Intent::Search {
    query: "documents about AI",
    filters: [TimeRange, Tag("work")]
}
```

## 🔧 How Apps Register

```rust
// App sends registration message via IPC
kernel::ipc_send(INTENT_BUS_TASK, IntentMessage::Register(Capability {
    task_id: MY_TASK_ID,
    task_name: "MyApp",

    // What can this app do?
    actions: vec![
        "open_file",
        "edit_text",
        "send_message"
    ],

    // What file types?
    file_types: vec![
        ".txt",
        ".md"
    ],

    // Semantic tags for AI routing
    tags: vec![
        "editor",
        "productivity"
    ],
}));
```

## 🧪 Running the Demo

```bash
cd userspace/intent-bus
cargo run
```

**Demo scenarios:**
1. Open file → Routes to TextEditor
2. Send message → Routes to Messenger
3. Build project → Routes to Builder
4. Search files → Routes to FileSearcher

## 🔌 Kernel Integration (Future)

**Current:** Standalone demo with mock IPC (tokio channels)

**Future:** Integrated with kernel IPC
```rust
// Kernel side (when IPC works)
pub fn syscall_submit_intent(intent_json: &[u8]) -> Result<TaskId> {
    let intent: Intent = serde_json::from_slice(intent_json)?;
    ipc_send(INTENT_BUS_TASK, IntentMessage::SubmitIntent(intent))
}
```

## 📊 Performance Goals

| Operation | Target | Notes |
|-----------|--------|-------|
| Intent routing | <10ms | Pattern matching phase |
| Semantic search | <50ms | With vector index |
| Neural prediction | <100ms | LSTM inference |
| End-to-end | <150ms | Route + execute |

## 🛣️ Roadmap

### Week 1 (✅ Done)
- [x] Core types and API design
- [x] Pattern matcher implementation
- [x] Demo with 4 mock apps
- [x] Confidence scoring

### Week 2-3 (Next)
- [ ] Integrate with kernel IPC (when syscalls work)
- [ ] Real app registrations
- [ ] Execution result handling
- [ ] Error recovery

### Month 2
- [ ] Add semantic embeddings (sentence-transformers)
- [ ] Vector database for capability matching
- [ ] Context-aware routing
- [ ] A/B testing vs pattern matching

### Month 3
- [ ] Train neural router on usage data
- [ ] LSTM for next-app prediction
- [ ] Proactive intent suggestions
- [ ] Multi-step intent planning

### Month 4+
- [ ] Federated learning (privacy-preserving)
- [ ] Intent marketplace (third-party intents)
- [ ] Cross-device intent sync
- [ ] Voice/gesture intent input

## 🎓 Learn More

**Similar Systems:**
- **Android Intents:** App-to-app communication (but not AI-powered)
- **macOS Services:** App cooperation (but manual)
- **IFTTT:** Automation rules (but user-programmed)

**Our Innovation:** Combine all three with AI for zero-config intelligence!

## 🤝 Contributing

Once the kernel IPC works, we need:
1. Example apps that register capabilities
2. Real-world intent patterns for training
3. Neural model fine-tuning
4. Performance benchmarks

## 📝 License

Part of Folkering OS - Microkernel with AI-First Design
