//! CPU Frequency Scaling
//!
//! Dynamic voltage and frequency scaling (DVFS) for power management
//! and performance optimization.
//!
//! Supports Intel SpeedStep and AMD PowerNow technologies via P-states.

use x86_64::registers::model_specific::Msr;

/// CPU frequency target (MHz)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrequencyTarget {
    /// Minimum frequency (power-saving)
    PowerSave,
    /// Base frequency (normal operation)
    Base,
    /// Turbo boost (maximum performance)
    Turbo,
    /// Custom frequency in MHz
    Custom(u32),
}

/// P-State (Performance State) descriptor
#[derive(Debug, Clone, Copy)]
struct PState {
    frequency_mhz: u32,
    voltage_mv: u32,
    control_value: u64,
}

/// CPU frequency controller
pub struct FrequencyController {
    current_freq_mhz: u32,
    base_freq_mhz: u32,
    max_freq_mhz: u32,
    min_freq_mhz: u32,
    supports_speedstep: bool,
    supports_turbo: bool,
}

impl FrequencyController {
    /// Initialize frequency controller
    pub fn new() -> Self {
        let (base, min, max, speedstep, turbo) = detect_capabilities();

        crate::serial_println!("[CPU_FREQ] Detected capabilities:");
        crate::serial_println!("[CPU_FREQ]   Base: {} MHz", base);
        crate::serial_println!("[CPU_FREQ]   Range: {} - {} MHz", min, max);
        crate::serial_println!("[CPU_FREQ]   SpeedStep: {}", speedstep);
        crate::serial_println!("[CPU_FREQ]   Turbo: {}", turbo);

        Self {
            current_freq_mhz: base,
            base_freq_mhz: base,
            max_freq_mhz: max,
            min_freq_mhz: min,
            supports_speedstep: speedstep,
            supports_turbo: turbo,
        }
    }

    /// Set CPU frequency target
    pub fn set_frequency(&mut self, target: FrequencyTarget) -> Result<(), FreqError> {
        if !self.supports_speedstep {
            return Err(FreqError::NotSupported);
        }

        let target_mhz = match target {
            FrequencyTarget::PowerSave => self.min_freq_mhz,
            FrequencyTarget::Base => self.base_freq_mhz,
            FrequencyTarget::Turbo => {
                if !self.supports_turbo {
                    return Err(FreqError::TurboNotSupported);
                }
                self.max_freq_mhz
            }
            FrequencyTarget::Custom(mhz) => {
                if mhz < self.min_freq_mhz || mhz > self.max_freq_mhz {
                    return Err(FreqError::InvalidFrequency);
                }
                mhz
            }
        };

        self.apply_frequency(target_mhz)?;
        self.current_freq_mhz = target_mhz;

        crate::serial_println!("[CPU_FREQ] Set frequency to {} MHz", target_mhz);
        Ok(())
    }

    /// Get current frequency
    pub fn current_frequency(&self) -> u32 {
        self.current_freq_mhz
    }

    /// Apply frequency via P-state
    fn apply_frequency(&self, target_mhz: u32) -> Result<(), FreqError> {
        // Calculate P-state control value
        // Intel: IA32_PERF_CTL MSR (0x199)
        // AMD: Similar via FIDVID_CTL

        const IA32_PERF_CTL: u32 = 0x199;

        unsafe {
            let mut perf_ctl = Msr::new(IA32_PERF_CTL);

            // Read current value
            let current = perf_ctl.read();

            // Calculate target P-state based on frequency
            // P-state = (target_freq / bus_speed) - offset
            // For simplicity, use linear scaling between min and max
            let range = self.max_freq_mhz - self.min_freq_mhz;
            let offset = target_mhz - self.min_freq_mhz;
            let ratio = if range > 0 {
                (offset as u64 * 0xFF) / range as u64
            } else {
                0x80 // Middle value if range is 0
            };

            // Construct new control value
            // Bits 15:8 = Target performance state
            // Bit 32 = Turbo disable (0 = enabled, 1 = disabled)
            let new_value = (current & !0xFF00) | ((ratio & 0xFF) << 8);

            // Write new P-state
            perf_ctl.write(new_value);

            // Wait for frequency transition (typically <10μs)
            // Simple spin delay
            for _ in 0..1000 {
                core::hint::spin_loop();
            }
        }

        Ok(())
    }
}

/// Detect CPU frequency scaling capabilities
fn detect_capabilities() -> (u32, u32, u32, bool, bool) {
    // CPUID function 0x16: Processor Frequency Information
    const CPUID_FREQ_INFO: u32 = 0x16;

    // Check if CPUID 0x16 is supported
    let max_basic = unsafe {
        let cpuid_result = core::arch::x86_64::__cpuid(0);
        cpuid_result.eax
    };

    if max_basic >= CPUID_FREQ_INFO {
        unsafe {
            let freq_info = core::arch::x86_64::__cpuid(CPUID_FREQ_INFO);

            // EAX: Base frequency (MHz)
            // EBX: Maximum frequency (MHz)
            // ECX: Bus (reference) frequency (MHz)
            let base_mhz = freq_info.eax as u32;
            let max_mhz = freq_info.ebx as u32;
            let bus_mhz = freq_info.ecx as u32;

            // Estimate minimum (typically 50-60% of base)
            let min_mhz = if base_mhz > 0 {
                (base_mhz * 6) / 10 // 60% of base
            } else {
                1000 // Fallback: 1 GHz
            };

            let max_mhz_actual = if max_mhz > 0 { max_mhz } else { base_mhz };

            // Check for SpeedStep/PowerNow support
            // CPUID.01H:ECX[7] = EIST (Enhanced Intel SpeedStep Technology)
            let features = core::arch::x86_64::__cpuid(1);
            let supports_speedstep = (features.ecx & (1 << 7)) != 0;

            // Check for Turbo Boost support
            // CPUID.06H:EAX[1] = Intel Turbo Boost Technology
            let power_mgmt = core::arch::x86_64::__cpuid(6);
            let supports_turbo = (power_mgmt.eax & (1 << 1)) != 0;

            if base_mhz > 0 {
                return (base_mhz, min_mhz, max_mhz_actual, supports_speedstep, supports_turbo);
            }
        }
    }

    // Fallback: use defaults if CPUID doesn't provide frequency info
    // Typical values for a modern CPU
    (2400, 1200, 3500, false, false)
}

/// Frequency scaling error
#[derive(Debug, Clone, Copy)]
pub enum FreqError {
    /// CPU doesn't support frequency scaling
    NotSupported,
    /// Turbo boost not available
    TurboNotSupported,
    /// Invalid frequency requested
    InvalidFrequency,
    /// Hardware error during frequency transition
    HardwareError,
}

/// Global frequency controller
static mut FREQ_CONTROLLER: Option<FrequencyController> = None;

/// Initialize CPU frequency scaling
pub fn init() {
    unsafe {
        FREQ_CONTROLLER = Some(FrequencyController::new());
    }

    crate::serial_println!("[CPU_FREQ] Frequency scaling initialized");
}

/// Set CPU frequency
///
/// Called by scheduler to adjust CPU frequency based on workload hints.
///
/// # Examples
///
/// ```no_run
/// // Boost CPU for compilation workload
/// set_cpu_freq(3500); // 3.5 GHz
///
/// // Return to power-saving mode
/// set_cpu_freq(1200); // 1.2 GHz
/// ```
pub fn set_cpu_freq(target_mhz: u32) {
    unsafe {
        if let Some(ref mut controller) = FREQ_CONTROLLER {
            let result = controller.set_frequency(FrequencyTarget::Custom(target_mhz));

            match result {
                Ok(()) => {
                    crate::serial_println!("[CPU_FREQ] Successfully set frequency to {} MHz", target_mhz);
                }
                Err(FreqError::NotSupported) => {
                    // Only warn once
                    static mut WARNED: bool = false;
                    if !WARNED {
                        crate::serial_println!("[CPU_FREQ] WARNING: Frequency scaling not supported on this CPU");
                        WARNED = true;
                    }
                }
                Err(err) => {
                    crate::serial_println!("[CPU_FREQ] ERROR: Failed to set frequency: {:?}", err);
                }
            }
        } else {
            crate::serial_println!("[CPU_FREQ] ERROR: Controller not initialized");
        }
    }
}

/// Set CPU to power-saving mode
pub fn set_power_save() {
    unsafe {
        if let Some(ref mut controller) = FREQ_CONTROLLER {
            let _ = controller.set_frequency(FrequencyTarget::PowerSave);
        }
    }
}

/// Set CPU to base frequency
pub fn set_base() {
    unsafe {
        if let Some(ref mut controller) = FREQ_CONTROLLER {
            let _ = controller.set_frequency(FrequencyTarget::Base);
        }
    }
}

/// Enable turbo boost (maximum performance)
pub fn set_turbo() {
    unsafe {
        if let Some(ref mut controller) = FREQ_CONTROLLER {
            let _ = controller.set_frequency(FrequencyTarget::Turbo);
        }
    }
}

/// Get current CPU frequency (MHz)
pub fn current_frequency() -> u32 {
    unsafe {
        FREQ_CONTROLLER
            .as_ref()
            .map(|c| c.current_frequency())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frequency_targets() {
        let mut controller = FrequencyController {
            current_freq_mhz: 2400,
            base_freq_mhz: 2400,
            max_freq_mhz: 3500,
            min_freq_mhz: 1200,
            supports_speedstep: true,
            supports_turbo: true,
        };

        // Test valid custom frequency
        assert!(controller.set_frequency(FrequencyTarget::Custom(3000)).is_ok());
        assert_eq!(controller.current_frequency(), 3000);

        // Test invalid frequency (too high)
        assert!(controller.set_frequency(FrequencyTarget::Custom(4000)).is_err());

        // Test power save
        assert!(controller.set_frequency(FrequencyTarget::PowerSave).is_ok());
        assert_eq!(controller.current_frequency(), 1200);
    }
}
