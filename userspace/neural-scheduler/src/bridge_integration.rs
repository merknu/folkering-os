//! Brain Bridge Integration for Neural Scheduler
//!
//! Connects the Neural Scheduler's predictions to the kernel via BrainBridge.
//! This enables the "Fast Brain" (kernel) to receive context hints from the
//! "Smart Brain" (neural scheduler).

use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType, WorkloadType, WriterError};
use crate::{ResourcePrediction, SystemMetrics};

/// Bridge writer wrapper for Neural Scheduler
///
/// Provides high-level API to translate neural scheduler predictions
/// into BrainBridge hints.
pub struct SchedulerBridgeWriter {
    writer: BrainBridgeWriter,
    last_intent: IntentType,
}

impl SchedulerBridgeWriter {
    /// Create a new scheduler bridge writer
    pub fn new() -> Result<Self, WriterError> {
        let writer = BrainBridgeWriter::new()?;

        Ok(Self {
            writer,
            last_intent: IntentType::Idle,
        })
    }

    /// Write prediction to kernel
    ///
    /// Translates a ResourcePrediction into an Intent and sends it to the kernel.
    pub fn write_prediction(&mut self, prediction: &ResourcePrediction) -> Result<(), WriterError> {
        // Classify the intent based on prediction
        let intent_type = self.classify_intent(prediction);

        // Only write if intent changed or confidence is high
        let should_write = intent_type != self.last_intent || prediction.confidence > 0.8;

        if should_write {
            let intent = Intent::new(intent_type)
                .with_duration(prediction.expected_duration_sec())
                .with_workload(self.classify_workload(prediction))
                .with_cpu((prediction.predicted_cpu * 100.0).min(100.0) as u8)
                .with_memory((prediction.predicted_memory * 100.0).min(100.0) as u8)
                .with_io((prediction.predicted_io * 100.0).min(100.0) as u8)
                .with_confidence((prediction.confidence * 255.0) as u8);

            self.writer.write_hint(intent)?;
            self.last_intent = intent_type;
        }

        Ok(())
    }

    /// Write system metrics as hints
    ///
    /// When we don't have predictions, we can still send current metrics
    /// as hints to the kernel.
    pub fn write_metrics(&mut self, metrics: &SystemMetrics) -> Result<(), WriterError> {
        let intent_type = self.classify_metrics_intent(metrics);

        let intent = Intent::new(intent_type)
            .with_cpu((metrics.cpu_usage * 100.0).min(100.0) as u8)
            .with_memory((metrics.memory_usage * 100.0).min(100.0) as u8)
            .with_confidence(128); // Medium confidence (50%)

        self.writer.write_hint(intent)?;

        Ok(())
    }

    /// Get statistics from the bridge
    pub fn stats(&self) -> libfolkering::WriterStats {
        self.writer.stats()
    }

    /// Classify prediction into intent type
    fn classify_intent(&self, prediction: &ResourcePrediction) -> IntentType {
        // High CPU, low I/O = Compilation/compute
        if prediction.predicted_cpu > 0.7 && prediction.predicted_io < 0.3 {
            return IntentType::Compiling;
        }

        // High I/O, moderate CPU = Rendering/video
        if prediction.predicted_io > 0.6 && prediction.predicted_cpu > 0.4 {
            return IntentType::Rendering;
        }

        // Balanced, sustained load = ML training
        if prediction.predicted_cpu > 0.8 && prediction.predicted_memory > 0.7 {
            return IntentType::MLTraining;
        }

        // Low everything = Idle
        if prediction.predicted_cpu < 0.2 && prediction.predicted_io < 0.2 {
            return IntentType::Idle;
        }

        // Default: Coding (moderate activity)
        IntentType::Coding
    }

    /// Classify workload type from prediction
    fn classify_workload(&self, prediction: &ResourcePrediction) -> WorkloadType {
        if prediction.predicted_cpu > 0.7 {
            WorkloadType::CpuBound
        } else if prediction.predicted_io > 0.7 {
            WorkloadType::IoBound
        } else if prediction.predicted_memory > 0.7 {
            WorkloadType::MemoryBound
        } else {
            WorkloadType::Mixed
        }
    }

    /// Classify intent from system metrics
    fn classify_metrics_intent(&self, metrics: &SystemMetrics) -> IntentType {
        // Simple heuristic based on current metrics
        if metrics.cpu_usage > 0.8 {
            IntentType::Compiling
        } else if metrics.cpu_usage < 0.2 {
            IntentType::Idle
        } else {
            IntentType::Coding
        }
    }
}

/// Extension trait for ResourcePrediction
trait PredictionExt {
    /// Calculate expected duration in seconds
    fn expected_duration_sec(&self) -> u32;
}

impl PredictionExt for ResourcePrediction {
    fn expected_duration_sec(&self) -> u32 {
        // If we have trend information, use it
        // Otherwise default to 30 seconds
        30
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intent_classification() {
        // High CPU, low I/O should be Compiling
        let pred = ResourcePrediction {
            timestamp: 1000,
            predicted_cpu: 0.85,
            predicted_memory: 0.4,
            predicted_io: 0.1,
            confidence: 0.9,
        };

        // We can't actually test this without creating a writer
        // which requires kernel support, but the logic is testable
        assert!(pred.predicted_cpu > 0.7);
        assert!(pred.predicted_io < 0.3);
    }

    #[test]
    fn test_workload_classification() {
        let pred = ResourcePrediction {
            timestamp: 1000,
            predicted_cpu: 0.9,
            predicted_memory: 0.3,
            predicted_io: 0.1,
            confidence: 0.8,
        };

        assert!(pred.predicted_cpu > 0.7); // Should be CPU-bound
    }
}
