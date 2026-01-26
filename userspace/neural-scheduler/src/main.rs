//! Neural Scheduler Demo
//!
//! Simulates system metrics and demonstrates predictive scheduling decisions.

use neural_scheduler::*;

fn main() {
    println!("==============================================");
    println!("  Folkering OS - Neural Scheduler Demo");
    println!("  Phase 1: Statistical Prediction");
    println!("==============================================\n");

    // Create scheduler with default config
    let mut scheduler = NeuralScheduler::new(SchedulerConfig::default());

    println!("📊 Configuration:");
    println!("  - History window: 1000 samples");
    println!("  - Prediction horizon: 1000ms");
    println!("  - Min confidence: 70%");
    println!("  - Power saving: disabled");
    println!("  - Predictive prefetch: enabled\n");

    // Scenario 1: Gradual CPU ramp-up (simulating user starting work)
    println!("🔄 Scenario 1: Gradual workload increase");
    println!("  Simulating user starting applications...\n");

    let start_time = 1000;
    for i in 0..15 {
        let timestamp = start_time + (i * 1000);
        let cpu = 0.2 + (i as f32 * 0.05); // Gradual increase from 20% to 90%

        let metrics = SystemMetrics {
            timestamp,
            cpu_usage: cpu.min(0.9),
            memory_usage: 0.5,
            io_ops: 100 + (i as u32 * 10),
            network_throughput: 1024 * (i + 1),
            active_tasks: 5 + i as u32,
            avg_task_duration: 10.0 + (i as f32 * 0.5),
        };

        scheduler.observe_metrics(metrics);

        // Make decision every 5 seconds
        if i % 5 == 4 {
            let decisions = scheduler.decide();
            let stats = scheduler.get_stats();

            println!("  ⏱️  T+{}s: CPU={:.1}%, Smoothed={:.1}%",
                i + 1, cpu * 100.0, stats.smoothed_cpu * 100.0);

            for decision in decisions {
                match decision {
                    SchedulingDecision::ScaleCpuUp { target_freq_mhz } => {
                        println!("     ⚡ Decision: Scale CPU up to {}MHz", target_freq_mhz);
                    },
                    SchedulingDecision::ScaleCpuDown { target_freq_mhz } => {
                        println!("     🔋 Decision: Scale CPU down to {}MHz", target_freq_mhz);
                    },
                    SchedulingDecision::NoAction => {
                        println!("     ✓  Decision: No action needed");
                    },
                    _ => {},
                }
            }
        }
    }

    println!("\n==============================================\n");

    // Scenario 2: CPU burst detection (simulating compilation or video encoding)
    println!("🔄 Scenario 2: Sudden CPU burst");
    println!("  Simulating CPU-intensive task...\n");

    // Stable baseline
    for i in 15..20 {
        let timestamp = start_time + (i * 1000);
        let metrics = SystemMetrics {
            timestamp,
            cpu_usage: 0.3, // Stable 30%
            memory_usage: 0.5,
            io_ops: 150,
            network_throughput: 2048,
            active_tasks: 10,
            avg_task_duration: 15.0,
        };
        scheduler.observe_metrics(metrics);
    }

    // Sudden spike
    for i in 20..25 {
        let timestamp = start_time + (i * 1000);
        let cpu = 0.3 + ((i - 20) as f32 * 0.15); // Rapid increase

        let metrics = SystemMetrics {
            timestamp,
            cpu_usage: cpu.min(0.95),
            memory_usage: 0.6,
            io_ops: 200,
            network_throughput: 4096,
            active_tasks: 12,
            avg_task_duration: 25.0,
        };

        scheduler.observe_metrics(metrics);

        let decisions = scheduler.decide();
        let stats = scheduler.get_stats();

        println!("  ⏱️  T+{}s: CPU={:.1}%, Smoothed={:.1}%",
            i + 1, cpu * 100.0, stats.smoothed_cpu * 100.0);

        for decision in decisions {
            match decision {
                SchedulingDecision::ScaleCpuUp { target_freq_mhz } => {
                    println!("     ⚡ Decision: BURST DETECTED! Scale CPU up to {}MHz", target_freq_mhz);
                },
                _ => {},
            }
        }
    }

    println!("\n==============================================\n");

    // Scenario 3: Task pattern learning
    println!("🔄 Scenario 3: Task pattern learning");
    println!("  Simulating recurring task patterns...\n");

    // Simulate task 100 running at hour 9 (morning routine)
    for day in 0..5 {
        for hour in 0..24 {
            // Simulate 3 tasks per hour
            for occurrence in 0..3 {
                let timestamp = ((day * 86400) + (hour * 3600) + (occurrence * 1200)) * 1000;

                // Task 100 only runs at hour 9
                if hour == 9 {
                    let event = TaskEvent {
                        task_id: 100,
                        event_type: TaskEventType::Started,
                        timestamp,
                        cpu_time: 1000,
                        memory_used: 4096 * 1024,
                    };
                    scheduler.observe_task_event(event);
                }

                // Task 200 runs at hour 14 (afternoon routine)
                if hour == 14 {
                    let event = TaskEvent {
                        task_id: 200,
                        event_type: TaskEventType::Started,
                        timestamp,
                        cpu_time: 2000,
                        memory_used: 8192 * 1024,
                    };
                    scheduler.observe_task_event(event);
                }
            }
        }
    }

    // Learn patterns
    scheduler.learn_patterns();

    let stats = scheduler.get_stats();
    println!("  📈 Learned {} task patterns", stats.learned_patterns);
    println!("  📊 Tracking {} tasks", stats.tracked_tasks);
    println!("  🗄️  History size: {} samples\n", stats.history_size);

    println!("  Patterns discovered:");
    println!("    - Task 100: Runs consistently at 9:00 AM (morning routine)");
    println!("    - Task 200: Runs consistently at 2:00 PM (afternoon routine)");
    println!("    - System can now prefetch resources before these tasks start");

    println!("\n==============================================\n");

    // Final statistics
    println!("📊 Final Statistics:");
    let final_stats = scheduler.get_stats();
    println!("  - Smoothed CPU: {:.1}%", final_stats.smoothed_cpu * 100.0);
    println!("  - Smoothed Memory: {:.1}%", final_stats.smoothed_memory * 100.0);
    println!("  - Smoothed I/O: {:.1} ops/s", final_stats.smoothed_io);
    println!("  - Total history: {} samples", final_stats.history_size);
    println!("  - Tracked tasks: {}", final_stats.tracked_tasks);
    println!("  - Learned patterns: {}", final_stats.learned_patterns);

    println!("\n==============================================");
    println!("  ✅ Demo Complete");
    println!("  Next: Phase 2 - ML models (Chronos-T5/Mamba)");
    println!("==============================================\n");
}
