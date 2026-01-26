//! Resource Predictor
//!
//! Predicts future system resource usage based on historical patterns.
//!
//! Phase 1: Statistical methods (moving averages, exponential smoothing)
//! Phase 2: Time-series ML (Chronos-T5, Mamba)

use crate::types::*;
use std::collections::VecDeque;

/// Statistical resource predictor (Phase 1)
pub struct ResourcePredictor {
    /// Recent system metrics (circular buffer)
    history: VecDeque<SystemMetrics>,

    /// Maximum history size
    max_history: usize,

    /// Exponential smoothing factor (0.0 - 1.0)
    alpha: f32,

    /// Current smoothed values
    smoothed_cpu: f32,
    smoothed_memory: f32,
    smoothed_io: f32,
}

impl ResourcePredictor {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(max_history),
            max_history,
            alpha: 0.3, // 30% weight to new values, 70% to history
            smoothed_cpu: 0.0,
            smoothed_memory: 0.0,
            smoothed_io: 0.0,
        }
    }

    /// Record a new metrics snapshot
    pub fn observe(&mut self, metrics: SystemMetrics) {
        // Update exponential smoothing
        if self.history.is_empty() {
            // Initialize with first values
            self.smoothed_cpu = metrics.cpu_usage;
            self.smoothed_memory = metrics.memory_usage;
            self.smoothed_io = metrics.io_ops as f32;
        } else {
            // Exponential weighted moving average
            self.smoothed_cpu = self.alpha * metrics.cpu_usage
                + (1.0 - self.alpha) * self.smoothed_cpu;
            self.smoothed_memory = self.alpha * metrics.memory_usage
                + (1.0 - self.alpha) * self.smoothed_memory;
            self.smoothed_io = self.alpha * metrics.io_ops as f32
                + (1.0 - self.alpha) * self.smoothed_io;
        }

        // Add to history
        self.history.push_back(metrics);

        // Maintain window size
        if self.history.len() > self.max_history {
            self.history.pop_front();
        }
    }

    /// Predict resource usage at future timestamp
    pub fn predict(&self, future_timestamp: Timestamp) -> ResourcePrediction {
        if self.history.is_empty() {
            return ResourcePrediction {
                timestamp: future_timestamp,
                predicted_cpu: 0.5,
                predicted_memory: 0.5,
                predicted_io: 0.5,
                confidence: 0.0,
            };
        }

        // Calculate trend (simple linear regression on recent samples)
        let recent_window = 10.min(self.history.len());
        let recent: Vec<&SystemMetrics> = self.history
            .iter()
            .rev()
            .take(recent_window)
            .collect();

        let cpu_trend = self.calculate_trend(
            &recent.iter().map(|m| m.cpu_usage).collect::<Vec<_>>()
        );
        let memory_trend = self.calculate_trend(
            &recent.iter().map(|m| m.memory_usage).collect::<Vec<_>>()
        );

        // Predict based on smoothed values + trend
        let current_time = self.history.back().unwrap().timestamp;
        let time_delta = (future_timestamp - current_time) as f32 / 1000.0; // seconds

        let predicted_cpu = (self.smoothed_cpu + cpu_trend * time_delta)
            .max(0.0)
            .min(1.0);
        let predicted_memory = (self.smoothed_memory + memory_trend * time_delta)
            .max(0.0)
            .min(1.0);
        let predicted_io = (self.smoothed_io / 1000.0) // Normalize IO
            .max(0.0)
            .min(1.0);

        // Calculate confidence based on variance
        let cpu_variance = self.calculate_variance(
            &recent.iter().map(|m| m.cpu_usage).collect::<Vec<_>>()
        );
        let confidence = (1.0 - cpu_variance).max(0.1);

        ResourcePrediction {
            timestamp: future_timestamp,
            predicted_cpu,
            predicted_memory,
            predicted_io,
            confidence,
        }
    }

    /// Calculate linear trend (slope)
    fn calculate_trend(&self, values: &[f32]) -> f32 {
        if values.len() < 2 {
            return 0.0;
        }

        let n = values.len() as f32;
        let sum_x: f32 = (0..values.len()).map(|i| i as f32).sum();
        let sum_y: f32 = values.iter().sum();
        let sum_xy: f32 = values.iter().enumerate()
            .map(|(i, &y)| i as f32 * y)
            .sum();
        let sum_x2: f32 = (0..values.len())
            .map(|i| (i as f32).powi(2))
            .sum();

        // Slope = (n*sum_xy - sum_x*sum_y) / (n*sum_x2 - sum_x^2)
        let numerator = n * sum_xy - sum_x * sum_y;
        let denominator = n * sum_x2 - sum_x.powi(2);

        if denominator.abs() < 0.001 {
            return 0.0;
        }

        numerator / denominator
    }

    /// Calculate variance
    fn calculate_variance(&self, values: &[f32]) -> f32 {
        if values.is_empty() {
            return 0.0;
        }

        let mean: f32 = values.iter().sum::<f32>() / values.len() as f32;
        let variance: f32 = values.iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f32>() / values.len() as f32;

        variance.sqrt() // Return standard deviation
    }

    /// Detect CPU burst (sudden spike prediction)
    pub fn detect_burst(&self) -> bool {
        if self.history.len() < 5 {
            return false;
        }

        // Check if trend is sharply upward
        // Take last 5 samples in chronological order
        let skip_count = self.history.len().saturating_sub(5);
        let recent: Vec<f32> = self.history
            .iter()
            .skip(skip_count)
            .map(|m| m.cpu_usage)
            .collect();

        let trend = self.calculate_trend(&recent);

        // Burst if trend is > 0.1 (10% per second increase)
        trend > 0.1
    }

    /// Get current smoothed values
    pub fn get_smoothed_metrics(&self) -> (f32, f32, f32) {
        (self.smoothed_cpu, self.smoothed_memory, self.smoothed_io)
    }

    /// Get history length
    pub fn history_len(&self) -> usize {
        self.history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_metrics(timestamp: u64, cpu: f32, memory: f32) -> SystemMetrics {
        SystemMetrics {
            timestamp,
            cpu_usage: cpu,
            memory_usage: memory,
            io_ops: 100,
            network_throughput: 1024,
            active_tasks: 5,
            avg_task_duration: 10.0,
        }
    }

    #[test]
    fn test_predictor_initialization() {
        let predictor = ResourcePredictor::new(100);
        assert_eq!(predictor.history_len(), 0);
    }

    #[test]
    fn test_observation() {
        let mut predictor = ResourcePredictor::new(100);

        predictor.observe(create_test_metrics(1000, 0.5, 0.6));
        assert_eq!(predictor.history_len(), 1);

        predictor.observe(create_test_metrics(2000, 0.6, 0.7));
        assert_eq!(predictor.history_len(), 2);
    }

    #[test]
    fn test_trend_calculation() {
        let predictor = ResourcePredictor::new(100);

        // Upward trend
        let upward = vec![0.1, 0.2, 0.3, 0.4, 0.5];
        let trend = predictor.calculate_trend(&upward);
        assert!(trend > 0.0, "Should detect upward trend");

        // Downward trend
        let downward = vec![0.5, 0.4, 0.3, 0.2, 0.1];
        let trend = predictor.calculate_trend(&downward);
        assert!(trend < 0.0, "Should detect downward trend");

        // Flat
        let flat = vec![0.5, 0.5, 0.5, 0.5, 0.5];
        let trend = predictor.calculate_trend(&flat);
        assert!(trend.abs() < 0.01, "Should detect flat trend");
    }

    #[test]
    fn test_prediction() {
        let mut predictor = ResourcePredictor::new(100);

        // Add some history
        for i in 0..10 {
            let cpu = 0.5 + (i as f32 * 0.01);
            predictor.observe(create_test_metrics(i * 1000, cpu, 0.5));
        }

        // Predict 1 second ahead
        let prediction = predictor.predict(11000);

        assert!(prediction.predicted_cpu > 0.5, "Should predict higher CPU");
        assert!(prediction.confidence > 0.0, "Should have some confidence");
    }

    #[test]
    fn test_burst_detection() {
        let mut predictor = ResourcePredictor::new(100);

        // Normal load
        for i in 0..5 {
            predictor.observe(create_test_metrics(i * 1000, 0.3, 0.3));
        }
        assert!(!predictor.detect_burst(), "Should not detect burst with stable load");

        // Sudden spike
        for i in 5..10 {
            let cpu = 0.3 + ((i - 5) as f32 * 0.2);
            predictor.observe(create_test_metrics(i * 1000, cpu, 0.3));
        }
        assert!(predictor.detect_burst(), "Should detect CPU burst");
    }
}
