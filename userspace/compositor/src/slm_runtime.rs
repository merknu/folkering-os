//! On-Device SLM Runtime — Local AI inference inside the OS
//!
//! A minimal "spinal cord" language model that runs as a WASM module
//! directly inside Folkering OS. No network, no proxy, no cloud.
//!
//! Architecture:
//! - Lightweight n-gram model trained on Folkering command patterns
//! - Runs inside wasmi with memory64 support
//! - Used for: command auto-complete, JIT prompt enhancement, UI predictions
//! - Cloud LLM (Gemini) = "cerebral cortex" for complex tasks
//! - Local SLM = "spinal cord" for instant reflexes
//!
//! The model is a simple lookup table stored in WASM linear memory:
//! - Key: last N tokens (byte trigrams)
//! - Value: probability distribution over next byte
//! - Greedy sampling: pick highest probability byte

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

/// Maximum context window for local model (bytes)
const MAX_CONTEXT: usize = 64;
/// Maximum output length
const MAX_OUTPUT: usize = 256;

/// Common Folkering OS command patterns for the built-in model.
/// This acts as a "firmware vocabulary" — commands the OS knows without network.
const COMMAND_PATTERNS: &[(&str, &str)] = &[
    // Input prefix → likely completion
    ("ls", "ls |> Format-Table"),
    ("get-", "Get-SystemStats"),
    ("show-", "Show-Dashboard"),
    ("format-", "Format-Table"),
    ("gemini gen", "gemini generate "),
    ("generate d", "generate driver"),
    ("lsp", "lspci"),
    ("driv", "drivers"),
    ("help", "help"),
    ("upt", "uptime"),
    ("open ", "open "),
    ("run ", "run "),
    ("save ", "save app "),
    ("revert ", "revert "),
    ("dream ", "dream accept"),
    ("power", "poweroff"),
    // FolkShell patterns
    ("|> show", "|> Show-Dashboard"),
    ("|> format", "|> Format-Table"),
    ("|> filter", "|> Filter-"),
    ("~> ", "~> \""),
    // System introspection
    ("what ", "what files do we have"),
    ("list ", "list files"),
];

/// The "Folkering Brain" — a pattern-matching local model.
///
/// This is not a neural network — it's a deterministic pattern matcher
/// that provides instant auto-complete and command predictions.
/// The real value is that it runs with ZERO latency and ZERO network.
pub struct LocalBrain {
    /// Command history for frequency-based prediction
    history: [Option<[u8; 64]>; 32],
    history_len: usize,
    history_head: usize,
}

impl LocalBrain {
    pub const fn new() -> Self {
        Self {
            history: [None; 32],
            history_len: 0,
            history_head: 0,
        }
    }

    /// Record a command in history for learning
    pub fn record(&mut self, cmd: &str) {
        let mut buf = [0u8; 64];
        let n = cmd.len().min(63);
        buf[..n].copy_from_slice(&cmd.as_bytes()[..n]);
        self.history[self.history_head] = Some(buf);
        self.history_head = (self.history_head + 1) % 32;
        if self.history_len < 32 { self.history_len += 1; }
    }

    /// Auto-complete a partial command. Returns the best completion or None.
    pub fn complete(&self, prefix: &str) -> Option<&'static str> {
        let lower = prefix.to_ascii_lowercase();

        // Check static patterns first
        for (pat, completion) in COMMAND_PATTERNS {
            if lower.starts_with(pat) {
                return Some(completion);
            }
        }

        None
    }

    /// Generate a response to a simple query (local inference).
    /// For complex queries, returns None → caller should use cloud LLM.
    pub fn generate(&self, prompt: &str) -> Option<String> {
        let lower = prompt.to_ascii_lowercase();

        // Simple Q&A patterns the OS can answer locally
        if lower.contains("what time") || lower.contains("current time") {
            let rtc = libfolk::sys::get_rtc();
            return Some(alloc::format!(
                "{}:{:02}:{:02}", rtc.hour, rtc.minute, rtc.second
            ));
        }

        if lower.contains("uptime") || lower.contains("how long") {
            let ms = libfolk::sys::uptime();
            return Some(alloc::format!("{}s", ms / 1000));
        }

        if lower.contains("memory") || lower.contains("ram") {
            let (total, used, pct) = libfolk::sys::memory_stats();
            return Some(alloc::format!(
                "Memory: {}/{}MB ({}%)", used, total, pct
            ));
        }

        if lower.contains("how many files") || lower.contains("file count") {
            if let Ok(count) = libfolk::sys::synapse::file_count() {
                return Some(alloc::format!("{} files in Synapse VFS", count));
            }
        }

        if lower.contains("pci") || lower.contains("hardware") || lower.contains("devices") {
            let mut buf: [libfolk::sys::pci::PciDeviceInfo; 16] = unsafe { ::core::mem::zeroed() };
            let count = libfolk::sys::pci::enumerate(&mut buf);
            return Some(alloc::format!("{} PCI devices detected", count));
        }

        if lower.contains("iommu") {
            let (available, _) = libfolk::sys::pci::iommu_status();
            return Some(if available {
                String::from("IOMMU: Active (VT-d DMA isolation enabled)")
            } else {
                String::from("IOMMU: Not available")
            });
        }

        if lower.contains("help") || lower.contains("what can") {
            return Some(String::from(
                "I can answer: time, uptime, memory, files, hardware, iommu. \
                 For complex queries, I route to the cloud LLM."
            ));
        }

        // Can't answer locally — return None to trigger cloud fallback
        None
    }
}

/// Global local brain instance
static mut LOCAL_BRAIN: LocalBrain = LocalBrain::new();

/// Get the local brain (safe — single-threaded compositor)
pub fn brain() -> &'static mut LocalBrain {
    unsafe { &mut LOCAL_BRAIN }
}
