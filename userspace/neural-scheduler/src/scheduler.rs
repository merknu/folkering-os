//! Neural Scheduler
//!
//! Makes scheduling decisions based on resource predictions.

use crate::types::*;
use crate::predictor::ResourcePredictor;
use std::collections::HashMap;

/// Neural scheduler with predictive capabilities
pub struct NeuralScheduler {
    /// Configuration
    config: SchedulerConfig,

    /// Resource predictor
    predictor: ResourcePredictor,

    /// Task execution history
    task_history: HashMap<TaskId, Vec<TaskEvent>>,

    /// Learned task patterns (time of day → common tasks)
    patterns: HashMap<(u8, u8), TaskPattern>,

    /// Current timestamp
    current_time: Timestamp,
}

impl NeuralScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            predictor: ResourcePredictor::new(config.history_window),
            config,
            task_history: HashMap::new(),
            patterns: HashMap::new(),
            current_time: 0,
        }
    }

    /// Process new system metrics
    pub fn observe_metrics(&mut self, metrics: SystemMetrics) {
        self.current_time = metrics.timestamp;
        self.predictor.observe(metrics);
    }

    /// Process task event
    pub fn observe_task_event(&mut self, event: TaskEvent) {
        let task_id = event.task_id;
        self.task_history
            .entry(task_id)
            .or_insert_with(Vec::new)
            .push(event);

        // Limit history per task
        if let Some(events) = self.task_history.get_mut(&task_id) {
            if events.len() > 100 {
                events.drain(0..50); // Keep last 50
            }
        }
    }

    /// Make scheduling decision for next interval
    pub fn decide(&mut self) -> Vec<SchedulingDecision> {
        let mut decisions = Vec::new();

        // Predict resource usage in near future
        let future_time = self.current_time + self.config.prediction_horizon_ms;
        let prediction = self.predictor.predict(future_time);

        if prediction.confidence < self.config.min_confidence {
            // Low confidence, don't make aggressive decisions
            return vec![SchedulingDecision::NoAction];
        }

        // CPU frequency scaling
        if let Some(cpu_decision) = self.decide_cpu_scaling(&prediction) {
            decisions.push(cpu_decision);
        }

        // Burst detection
        if self.predictor.detect_burst() {
            // Pre-ramp CPU before spike
            decisions.push(SchedulingDecision::ScaleCpuUp {
                target_freq_mhz: 3500,
            });
            println!("[SCHEDULER] Detected CPU burst, ramping up frequency");
        }

        // Power management
        if self.config.aggressive_power_saving {
            if let Some(power_decision) = self.decide_power_management(&prediction) {
                decisions.push(power_decision);
            }
        }

        // Predictive prefetching
        if self.config.predictive_prefetch {
            if let Some(prefetch_decision) = self.decide_prefetch() {
                decisions.push(prefetch_decision);
            }
        }

        if decisions.is_empty() {
            decisions.push(SchedulingDecision::NoAction);
        }

        decisions
    }

    /// Decide CPU frequency scaling
    fn decide_cpu_scaling(&self, prediction: &ResourcePrediction) -> Option<SchedulingDecision> {
        // Get current smoothed CPU
        let (current_cpu, _, _) = self.predictor.get_smoothed_metrics();

        // If predicted CPU is significantly higher, scale up
        if prediction.predicted_cpu > current_cpu + 0.2 {
            return Some(SchedulingDecision::ScaleCpuUp {
                target_freq_mhz: 3500,
            });
        }

        // If predicted CPU is significantly lower, scale down (save power)
        if prediction.predicted_cpu < current_cpu - 0.3 && current_cpu > 0.3 {
            return Some(SchedulingDecision::ScaleCpuDown {
                target_freq_mhz: 2000,
            });
        }

        None
    }

    /// Decide power management (core sleep/wake)
    fn decide_power_management(&self, prediction: &ResourcePrediction) -> Option<SchedulingDecision> {
        // If predicted load is very low, sleep a core
        if prediction.predicted_cpu < 0.2 && prediction.confidence > 0.8 {
            return Some(SchedulingDecision::SleepCore { core_id: 3 });
        }

        // If predicted load is high, wake cores
        if prediction.predicted_cpu > 0.7 {
            return Some(SchedulingDecision::WakeCore { core_id: 3 });
        }

        None
    }

    /// Decide prefetching based on task patterns
    fn decide_prefetch(&self) -> Option<SchedulingDecision> {
        // Get current time of day
        let hour = ((self.current_time / 1000 / 3600) % 24) as u8;
        let day_of_week = (((self.current_time / 1000 / 86400) + 4) % 7) as u8; // Unix epoch was Thursday

        // Check if we have learned patterns for this time
        if let Some(pattern) = self.patterns.get(&(hour, day_of_week)) {
            if pattern.confidence > 0.7 && !pattern.common_tasks.is_empty() {
                // Predict most likely next task
                let next_task = pattern.common_tasks[0];

                // In a real system, we'd know which pages this task needs
                // For now, return a placeholder
                return Some(SchedulingDecision::PrefetchData {
                    task_id: next_task,
                    pages: vec![0x1000, 0x2000, 0x3000],
                });
            }
        }

        None
    }

    /// Learn patterns from task history
    pub fn learn_patterns(&mut self) {
        // Group task events by time of day
        let mut time_buckets: HashMap<(u8, u8), Vec<TaskId>> = HashMap::new();

        for (task_id, events) in &self.task_history {
            for event in events {
                if event.event_type == TaskEventType::Started {
                    let timestamp_sec = event.timestamp / 1000;
                    let hour = ((timestamp_sec / 3600) % 24) as u8;
                    let day_of_week = (((timestamp_sec / 86400) + 4) % 7) as u8;

                    time_buckets
                        .entry((hour, day_of_week))
                        .or_insert_with(Vec::new)
                        .push(*task_id);
                }
            }
        }

        // Create patterns from buckets
        for ((hour, day_of_week), tasks) in time_buckets {
            if tasks.len() < 3 {
                continue; // Need more data
            }

            // Count task frequencies
            let mut task_counts: HashMap<TaskId, u32> = HashMap::new();
            for task_id in &tasks {
                *task_counts.entry(*task_id).or_insert(0) += 1;
            }

            // Sort by frequency
            let mut common_tasks: Vec<(TaskId, u32)> = task_counts.into_iter().collect();
            common_tasks.sort_by(|a, b| b.1.cmp(&a.1));

            let pattern = TaskPattern {
                hour,
                day_of_week,
                common_tasks: common_tasks.iter().map(|(id, _)| *id).take(5).collect(),
                avg_load: 0.5, // TODO: Calculate actual average load
                confidence: (tasks.len() as f32 / 10.0).min(1.0),
            };

            self.patterns.insert((hour, day_of_week), pattern);
        }

        println!("[SCHEDULER] Learned {} time-based patterns", self.patterns.len());
    }

    /// Get statistics
    pub fn get_stats(&self) -> SchedulerStats {
        let (cpu, mem, io) = self.predictor.get_smoothed_metrics();

        SchedulerStats {
            history_size: self.predictor.history_len(),
            tracked_tasks: self.task_history.len(),
            learned_patterns: self.patterns.len(),
            smoothed_cpu: cpu,
            smoothed_memory: mem,
            smoothed_io: io,
        }
    }
}

/// Scheduler statistics
#[derive(Debug, Clone)]
pub struct SchedulerStats {
    pub history_size: usize,
    pub tracked_tasks: usize,
    pub learned_patterns: usize,
    pub smoothed_cpu: f32,
    pub smoothed_memory: f32,
    pub smoothed_io: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_metrics(timestamp: u64, cpu: f32) -> SystemMetrics {
        SystemMetrics {
            timestamp,
            cpu_usage: cpu,
            memory_usage: 0.5,
            io_ops: 100,
            network_throughput: 1024,
            active_tasks: 5,
            avg_task_duration: 10.0,
        }
    }

    #[test]
    fn test_scheduler_initialization() {
        let scheduler = NeuralScheduler::new(SchedulerConfig::default());
        let stats = scheduler.get_stats();

        assert_eq!(stats.history_size, 0);
        assert_eq!(stats.tracked_tasks, 0);
    }

    #[test]
    fn test_observe_metrics() {
        let mut scheduler = NeuralScheduler::new(SchedulerConfig::default());

        scheduler.observe_metrics(create_test_metrics(1000, 0.5));
        scheduler.observe_metrics(create_test_metrics(2000, 0.6));

        let stats = scheduler.get_stats();
        assert_eq!(stats.history_size, 2);
    }

    #[test]
    fn test_observe_task_events() {
        let mut scheduler = NeuralScheduler::new(SchedulerConfig::default());

        let event = TaskEvent {
            task_id: 1,
            event_type: TaskEventType::Started,
            timestamp: 1000,
            cpu_time: 100,
            memory_used: 1024,
        };

        scheduler.observe_task_event(event);

        let stats = scheduler.get_stats();
        assert_eq!(stats.tracked_tasks, 1);
    }

    #[test]
    fn test_decision_making() {
        let mut scheduler = NeuralScheduler::new(SchedulerConfig::default());

        // Add some history
        for i in 0..10 {
            scheduler.observe_metrics(create_test_metrics(i * 1000, 0.5));
        }

        let decisions = scheduler.decide();
        assert!(!decisions.is_empty());
    }

    #[test]
    fn test_pattern_learning() {
        let mut scheduler = NeuralScheduler::new(SchedulerConfig::default());

        // Simulate task pattern: task 100 runs every hour at :00
        for hour in 0..24 {
            for _ in 0..5 {
                let event = TaskEvent {
                    task_id: 100,
                    event_type: TaskEventType::Started,
                    timestamp: (hour * 3600 * 1000) as u64,
                    cpu_time: 100,
                    memory_used: 1024,
                };
                scheduler.observe_task_event(event);
            }
        }

        scheduler.learn_patterns();

        let stats = scheduler.get_stats();
        assert!(stats.learned_patterns > 0, "Should learn at least one pattern");
    }
}
