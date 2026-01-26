# Neural Scheduler - The "Fast Brain" of Folkering OS

**Phase 1 Complete**: Statistical prediction methods
**Phase 2 Planned**: ML-based time-series forecasting (Chronos-T5, Mamba)

## Overview

The Neural Scheduler is the kernel-level intelligence component ("Fast Brain") that makes predictive scheduling decisions based on system resource usage patterns. It complements the userspace "Smart Brain" (Intent Bus, Synapse) by providing sub-millisecond predictions for CPU frequency scaling, power management, and task prefetching.

## Architecture

```
┌─────────────────────────────────────────┐
│  Resource Monitoring                    │
│  ┌───────────────────────────────────┐  │
│  │ System Metrics (every 1ms)        │  │
│  │ - CPU usage                       │  │
│  │ - Memory usage                    │  │
│  │ - I/O operations                  │  │
│  │ - Network throughput              │  │
│  └───────────────┬───────────────────┘  │
└──────────────────┼──────────────────────┘
                   │
┌──────────────────┼──────────────────────┐
│  Resource Predictor                     │
│  ┌───────────────▼───────────────────┐  │
│  │ Exponential Smoothing (α=0.3)    │  │
│  │ Linear Regression Trend          │  │
│  │ Variance-based Confidence        │  │
│  └───────────────┬───────────────────┘  │
└──────────────────┼──────────────────────┘
                   │
┌──────────────────┼──────────────────────┐
│  Neural Scheduler                       │
│  ┌───────────────▼───────────────────┐  │
│  │ Decision Making Engine           │  │
│  │ - CPU frequency scaling          │  │
│  │ - Burst detection                │  │
│  │ - Power management               │  │
│  │ - Task pattern learning          │  │
│  └───────────────┬───────────────────┘  │
└──────────────────┼──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│  Scheduling Decisions                   │
│  - ScaleCpuUp / ScaleCpuDown           │
│  - WakeCore / SleepCore                │
│  - PrefetchData                        │
│  - PreallocateBuffers                  │
└─────────────────────────────────────────┘
```

## Phase 1: Statistical Methods (Current)

### Implemented Features

1. **Exponential Smoothing**
   - Smooth out noise in system metrics
   - α=0.3 (30% weight to new values, 70% to history)
   - Tracks CPU, memory, and I/O independently

2. **Linear Trend Detection**
   - Simple linear regression on recent samples (window of 10)
   - Detects upward/downward trends
   - Used for predictive decisions

3. **Burst Detection**
   - Detects sudden CPU spikes (>10% per second increase)
   - Triggers proactive CPU frequency scaling
   - Prevents lag during compute-intensive tasks

4. **Pattern Learning**
   - Groups task executions by time of day and day of week
   - Learns recurring patterns (e.g., "Task 100 runs at 9 AM daily")
   - Enables predictive prefetching

5. **Dynamic Decision Making**
   - CPU scaling: Up if predicted +20%, down if predicted -30%
   - Power management: Sleep cores if predicted load <20%
   - Confidence thresholding: Only act if confidence >70%

### Performance Characteristics

- **Prediction latency**: <1ms (statistical methods)
- **Memory footprint**: <10KB (fixed-size circular buffer)
- **History window**: 1000 samples (configurable)
- **Prediction horizon**: 1 second (configurable)

## API

### Resource Predictor

```rust
use neural_scheduler::ResourcePredictor;

let mut predictor = ResourcePredictor::new(1000); // 1000 sample history

// Observe system metrics
let metrics = SystemMetrics {
    timestamp: 1000,
    cpu_usage: 0.5,
    memory_usage: 0.6,
    io_ops: 100,
    network_throughput: 1024,
    active_tasks: 5,
    avg_task_duration: 10.0,
};
predictor.observe(metrics);

// Predict future resource usage
let prediction = predictor.predict(2000); // 1 second ahead
println!("Predicted CPU: {:.1}%", prediction.predicted_cpu * 100.0);
println!("Confidence: {:.1}%", prediction.confidence * 100.0);

// Detect bursts
if predictor.detect_burst() {
    println!("CPU burst detected!");
}
```

### Neural Scheduler

```rust
use neural_scheduler::{NeuralScheduler, SchedulerConfig};

let mut scheduler = NeuralScheduler::new(SchedulerConfig::default());

// Process metrics
scheduler.observe_metrics(metrics);

// Record task events
let event = TaskEvent {
    task_id: 100,
    event_type: TaskEventType::Started,
    timestamp: 1000,
    cpu_time: 100,
    memory_used: 1024,
};
scheduler.observe_task_event(event);

// Get scheduling decisions
let decisions = scheduler.decide();
for decision in decisions {
    match decision {
        SchedulingDecision::ScaleCpuUp { target_freq_mhz } => {
            println!("Scale CPU to {}MHz", target_freq_mhz);
        },
        SchedulingDecision::PrefetchData { task_id, pages } => {
            println!("Prefetch {} pages for task {}", pages.len(), task_id);
        },
        _ => {},
    }
}

// Learn patterns from history
scheduler.learn_patterns();
let stats = scheduler.get_stats();
println!("Learned {} patterns", stats.learned_patterns);
```

## Running the Demo

```bash
cargo run --release
```

**Output**:
```
==============================================
  Folkering OS - Neural Scheduler Demo
  Phase 1: Statistical Prediction
==============================================

📊 Configuration:
  - History window: 1000 samples
  - Prediction horizon: 1000ms
  - Min confidence: 70%
  - Power saving: disabled
  - Predictive prefetch: enabled

🔄 Scenario 1: Gradual workload increase
  Simulating user starting applications...

  ⏱️  T+5s: CPU=40.0%, Smoothed=31.1%
     ✓  Decision: No action needed
  ...
```

## Tests

Run the test suite:

```bash
cargo test
```

**Test coverage**:
- ✅ Resource predictor initialization
- ✅ Metric observation and history management
- ✅ Trend calculation (upward, downward, flat)
- ✅ Prediction accuracy
- ✅ Burst detection
- ✅ Scheduler initialization
- ✅ Decision making
- ✅ Task event tracking
- ✅ Pattern learning

**Test results**: 10/10 passing

## Configuration

```rust
SchedulerConfig {
    history_window: 1000,              // Samples to keep
    prediction_horizon_ms: 1000,       // How far ahead to predict
    min_confidence: 0.7,                // Minimum confidence (70%)
    aggressive_power_saving: false,     // Enable core sleep/wake
    predictive_prefetch: true,          // Enable task prefetching
}
```

## Decision Types

| Decision | Trigger | Action |
|----------|---------|--------|
| `ScaleCpuUp` | Predicted CPU increase >20% | Increase CPU frequency to 3.5GHz |
| `ScaleCpuDown` | Predicted CPU decrease >30% | Decrease CPU frequency to 2.0GHz |
| `WakeCore` | Predicted load >70% | Wake up sleeping CPU core |
| `SleepCore` | Predicted load <20% | Put CPU core to sleep (power saving) |
| `PrefetchData` | Predicted task start | Prefetch memory pages for task |
| `PreallocateBuffers` | Predicted I/O spike | Preallocate I/O buffers |
| `NoAction` | Confidence <70% or stable load | Do nothing |

## Phase 2 Roadmap: ML-Based Prediction

### Planned Features

1. **Chronos-T5-Tiny Integration**
   - Time-series forecasting for CPU/memory/I/O
   - Multi-step prediction (next 10 time steps)
   - Better accuracy than statistical methods

2. **Mamba-2.8B State Space Model**
   - Linear time complexity O(N) vs transformer O(N²)
   - Constant state size (no memory allocation in kernel)
   - Sub-millisecond inference on CPU

3. **Hardware Performance Counter Integration**
   - Read PMU (Performance Monitoring Unit) counters
   - Instructions retired, cache misses, branch misses
   - More granular prediction signals

4. **Dynamic Timeslice Adjustment**
   - Adjust scheduler quantum based on predicted load
   - Longer timeslices (10ms) for compute-intensive tasks
   - Shorter timeslices (1ms) for interactive tasks

### Model Selection Criteria

| Model | Latency | Memory | Accuracy | Kernel-Safe? |
|-------|---------|--------|----------|--------------|
| **Statistical (Phase 1)** | <1ms | 10KB | Good | ✅ Yes |
| **Chronos-T5-Tiny** | ~5-10ms | 50MB | Excellent | ❌ No (transformer) |
| **Mamba-2.8B** | <1ms | 20MB | Excellent | ✅ Yes (linear) |
| **Custom MLP** | <0.5ms | 5MB | Good | ✅ Yes |

**Recommendation**: Mamba-2.8B for kernel-level prediction (Fast Brain)

## Integration with Kernel

### Planned Hook Points

```rust
// In kernel/src/task/scheduler.rs

fn schedule_next() -> Option<&'static Task> {
    // 1. Read hardware performance counters
    let pmu_data = read_pmu_counters();

    // 2. Get prediction from neural scheduler
    let prediction = NEURAL_SCHEDULER.predict(pmu_data);

    // 3. Adjust timeslice based on prediction
    let quantum = if prediction.predicted_load > 0.8 {
        10_000_000 // 10ms for compute
    } else {
        1_000_000  // 1ms for interactive
    };

    // 4. Select next task
    select_task_with_quantum(quantum)
}
```

## Dependencies

- `serde`: Serialization for metrics and decisions
- `tokio`: Async runtime (for future IPC integration)
- `ndarray`: Array operations for ML (Phase 2)
- `ndarray-stats`: Statistical functions

**Future**:
- `chronos`: Time-series forecasting (Phase 2)
- `onnxruntime`: ONNX model inference (Phase 2)
- `mamba-rs`: Mamba state space model (Phase 2)

## License

Part of Folkering OS - AI-Native Operating System

## See Also

- **Synapse**: Neural knowledge graph filesystem (Smart Brain)
- **Intent Bus**: Semantic app routing (Smart Brain)
- **NEURAL_ARCHITECTURE_PLAN.md**: Two-brain system architecture
- **PROGRESS_SUMMARY.md**: Overall project status
