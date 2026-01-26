# Neural Architecture Plan for Folkering OS

**Date**: 2026-01-26
**Status**: Planning Document for Phase 3+
**Source**: Architectural guidance for next-generation OS intelligence

---

## Current State (Phase 2 Complete)

✅ **Vector Filesystem** - all-MiniLM-L6-v2 (384-dim embeddings)
- Semantic file search
- Entity extraction (GLiNER)
- Hybrid search (FTS5 + vector)

**Limitation**: This is an embedding model, not a predictive/generative model. It converts text → numbers for search, but cannot predict future system behavior or understand user intent.

---

## Future Architecture: "Two-Brain" System

### 1. Fast Brain (Kernel-Level Neural Scheduler)

**Goal**: Sub-millisecond latency CPU/memory/IO predictions

**Requirements**:
- Linear complexity O(N) - must not slow down as logs grow
- Process infinite stream of system events
- Predict CPU bursts before they happen
- Pre-ramp clock speed to prevent lag

**Recommended Models**:

| Model | Size | Why? |
|-------|------|------|
| **Mamba-2.8B** (or 130M distilled) | 130M-2.8B | Linear O(N) complexity<br/>State Space Model<br/>Remembers system state without re-reading full history |
| **Chronos-T5-Tiny/Mini** | <1B | Amazon's time-series forecasting model<br/>Specifically trained for "Will X spike in 5 seconds?" |

**Input Stream**:
```
[CPU_Load, RAM_Usage, Disk_IO, Network_IO, Temp, Interrupts]
```

**Output**:
```
Predicted next 1-5 seconds of system load
```

**Use Case**:
- If Chronos predicts heavy CPU spike → OS pre-ramps clock speed
- If predicts I/O burst → prepare cache/buffers
- If predicts idle → reduce power consumption

---

### 2. Smart Brain (User-Level Task Management)

**Goal**: 100-500ms latency intent understanding

**Requirements**:
- Understand user patterns: "User just closed VS Code, usually opens GitHub next"
- Reasoning capabilities for complex workflows
- Run locally on NPU without battery drain
- Optional: conversational assistance

**Recommended Models (SLMs - Small Language Models)**:

| Model | Size | Best For | Why? |
|-------|------|----------|------|
| **Phi-3.5 Mini** | 3.8B | Logic & Reasoning | Microsoft's "textbook quality" training<br/>King of small models for instructions |
| **Gemma 2** | 2B | General Assistant | Google's balance of speed + creativity<br/>Good if OS should "chat" with user |
| **Qwen 2.5** | 0.5B-1.5B | Coding & Scripts | Excellent at code generation<br/>Perfect for "Intent Bus" scripting |

**Use Cases**:
- Predictive app launching
- Workflow automation (detected pattern → suggest next step)
- Context-aware file search
- Natural language system control

---

## Proposed Architecture Diagram

```
┌─────────────────────────────────────────────────────┐
│                    USER SPACE                        │
│                                                      │
│  ┌──────────────────────────────────────────────┐  │
│  │  Smart Brain: Phi-3.5 Mini (3.8B)            │  │
│  │  Intent Understanding & Task Prediction      │  │
│  │  Latency: 100-500ms                          │  │
│  └──────────────────────────────────────────────┘  │
│           │                         │               │
│           ▼                         ▼               │
│  ┌──────────────┐        ┌──────────────────────┐  │
│  │ Vector FS    │        │ Intent Bus           │  │
│  │ (MiniLM-L6)  │        │ (Workflow Engine)    │  │
│  └──────────────┘        └──────────────────────┘  │
│                                                      │
└───────────────────────────┬──────────────────────────┘
                            │
┌───────────────────────────┴──────────────────────────┐
│                   KERNEL SPACE                        │
│                                                       │
│  ┌──────────────────────────────────────────────┐   │
│  │  Fast Brain: Chronos-T5-Tiny / Mamba-130M   │   │
│  │  System State Prediction                      │   │
│  │  Latency: <1ms                                │   │
│  └──────────────────────────────────────────────┘   │
│           │                                          │
│           ▼                                          │
│  ┌──────────────────────────────────────────────┐   │
│  │  Neural Scheduler                             │   │
│  │  - CPU frequency scaling                      │   │
│  │  - Memory prefetching                         │   │
│  │  - I/O prediction                             │   │
│  └──────────────────────────────────────────────┘   │
│                                                       │
└───────────────────────────────────────────────────────┘
```

---

## Implementation Priorities

### Phase 3 (Next): Smart Brain Prototype
1. Integrate Phi-3.5 Mini via ONNX Runtime
2. Pattern detection for app launch sequences
3. Context-aware file suggestions
4. Intent Bus integration

### Phase 4: Fast Brain Prototype
1. System metrics collection (CPU, RAM, I/O)
2. Chronos-T5 time-series prediction
3. Scheduler hook points in kernel
4. Predictive resource allocation

### Phase 5: Optimization
1. Distill/quantize models for production
2. NPU acceleration
3. Battery-aware inference scheduling
4. Continuous learning from user patterns

---

## Model Complexity Analysis

### Why Standard Transformers Don't Work

**Problem**: O(N²) quadratic complexity
- As system logs grow longer, processing slows down quadratically
- Cannot have OS freeze because scheduler is "thinking"
- Example: 1-hour log = 3600 seconds × events/sec = massive context

**Solution**: Linear models (Mamba) or specialized forecasting (Chronos)
- O(N) linear complexity
- Process streaming data without context window limits
- Real-time performance even with days of history

---

## Next Steps

1. **Research Phase** (Current)
   - ✅ Document architecture (this file)
   - ⏳ Evaluate ONNX support for Mamba/Chronos/Phi
   - ⏳ Benchmark inference latency on target hardware

2. **Prototype Phase** (Phase 3)
   - Implement Smart Brain for user-level predictions
   - Collect telemetry data for training/validation

3. **Production Phase** (Phase 4+)
   - Kernel integration for Fast Brain
   - Custom model fine-tuning on Folkering OS data
   - Multi-model orchestration

---

## Hardware Requirements

**For Development**:
- NPU support preferred (Intel AI Boost, AMD XDNA, Apple Neural Engine)
- Fallback: GPU (CUDA/ROCm) or CPU with AVX-512

**For Production**:
- Fast Brain: Must run in kernel space → extremely optimized ONNX/TensorRT
- Smart Brain: Can leverage NPU in user space

---

## References

- **Mamba**: https://arxiv.org/abs/2312.00752 (State Space Models)
- **Chronos**: https://github.com/amazon-science/chronos-forecasting
- **Phi-3.5**: https://huggingface.co/microsoft/Phi-3.5-mini-instruct
- **Gemma 2**: https://huggingface.co/google/gemma-2-2b
- **Qwen 2.5**: https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B

---

**Status**: ✅ Architecture Defined
**Next**: Phase 2 completion, then begin Phase 3 Smart Brain prototyping
