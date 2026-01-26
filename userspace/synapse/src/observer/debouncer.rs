//! File event debouncing and filtering
//!
//! Modern text editors use atomic write patterns that generate multiple
//! filesystem events. This module coalesces rapid events into single
//! meaningful updates.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::collections::HashMap;

/// Debounce interval - wait this long after last event before processing
pub const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(1);

/// File extensions to ignore (temporary files, caches, etc.)
pub const IGNORE_EXTENSIONS: &[&str] = &[
    // Editor temp files
    ".swp", ".swo", ".swn",     // Vim
    ".tmp", ".temp",             // Generic temp
    "~",                         // Emacs backup
    ".bak", ".backup",           // Backup files

    // IDE files
    ".idea",                     // JetBrains
    ".vscode",                   // VSCode

    // Build artifacts
    ".o", ".obj",                // Object files
    ".a", ".lib",                // Static libraries
    ".so", ".dll", ".dylib",     // Dynamic libraries
    ".exe",                      // Executables

    // Package manager
    ".pyc", ".pyo",              // Python bytecode
    ".class",                    // Java bytecode

    // Version control
    ".git",                      // Git internals
];

/// Directories to ignore (entire subtrees)
pub const IGNORE_DIRS: &[&str] = &[
    // Dependencies
    "node_modules",
    "vendor",
    "target",                    // Rust build
    "build",
    "dist",

    // Virtual environments
    "venv",
    ".venv",
    "__pycache__",

    // IDE
    ".idea",
    ".vscode",
    ".vs",

    // Version control
    ".git",
    ".svn",
    ".hg",

    // OS
    ".DS_Store",
    "Thumbs.db",
];

/// Event types we care about
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventType {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// Pending event waiting to be processed
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub path: PathBuf,
    pub event_type: FileEventType,
    pub last_event_time: Instant,
}

/// Debouncer state machine
pub struct Debouncer {
    /// Pending events that haven't settled yet
    pending: HashMap<PathBuf, PendingEvent>,
}

impl Debouncer {
    /// Create new debouncer
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Check if a path should be ignored
    pub fn is_ignored(path: &Path) -> bool {
        // Check if any component is an ignored directory
        for component in path.components() {
            if let Some(name) = component.as_os_str().to_str() {
                if IGNORE_DIRS.contains(&name) {
                    return true;
                }
            }
        }

        // Check file extension
        if let Some(ext) = path.extension() {
            if let Some(ext_str) = ext.to_str() {
                let ext_with_dot = format!(".{}", ext_str);
                if IGNORE_EXTENSIONS.contains(&ext_with_dot.as_str()) {
                    return true;
                }
            }
        }

        // Check filename patterns
        if let Some(filename) = path.file_name() {
            if let Some(name) = filename.to_str() {
                // Filter backup files ending in ~
                if name.ends_with('~') {
                    return true;
                }

                // Filter editor temp files
                if name.ends_with(".swp") || name.ends_with(".swo") || name.ends_with(".swn") {
                    return true;
                }

                // Filter hidden temp files (start with .)
                if name.starts_with('.') && name.len() > 1 {
                    // Allow .env, .gitignore, etc. but filter if it looks like a temp file
                    if name.contains(".tmp") || name.contains(".temp") || name.contains(".swp") {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Record an event (adds to pending queue)
    pub fn record_event(&mut self, path: PathBuf, event_type: FileEventType) {
        // Check if should be ignored
        if Self::is_ignored(&path) {
            return;
        }

        // Update or insert pending event
        self.pending.insert(path.clone(), PendingEvent {
            path,
            event_type,
            last_event_time: Instant::now(),
        });
    }

    /// Get events that have settled (no activity for DEBOUNCE_INTERVAL)
    pub fn get_settled_events(&mut self) -> Vec<PendingEvent> {
        let now = Instant::now();
        let mut settled = Vec::new();

        // Find events that have settled
        let settled_paths: Vec<PathBuf> = self.pending
            .iter()
            .filter(|(_, event)| now.duration_since(event.last_event_time) >= DEBOUNCE_INTERVAL)
            .map(|(path, _)| path.clone())
            .collect();

        // Remove settled events from pending and return them
        for path in settled_paths {
            if let Some(event) = self.pending.remove(&path) {
                settled.push(event);
            }
        }

        settled
    }

    /// Get count of pending events
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Clear all pending events (for testing)
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ignore_temp_files() {
        assert!(Debouncer::is_ignored(Path::new("file.swp")));
        assert!(Debouncer::is_ignored(Path::new("file.tmp")));
        assert!(Debouncer::is_ignored(Path::new("file~")));
        assert!(Debouncer::is_ignored(Path::new(".file.swp")));
    }

    #[test]
    fn test_ignore_directories() {
        assert!(Debouncer::is_ignored(Path::new("node_modules/package/index.js")));
        assert!(Debouncer::is_ignored(Path::new("target/debug/main.exe")));
        assert!(Debouncer::is_ignored(Path::new(".git/objects/abc123")));
        assert!(Debouncer::is_ignored(Path::new("src/.git/HEAD")));
    }

    #[test]
    fn test_allow_normal_files() {
        assert!(!Debouncer::is_ignored(Path::new("src/main.rs")));
        assert!(!Debouncer::is_ignored(Path::new("README.md")));
        assert!(!Debouncer::is_ignored(Path::new(".env")));
        assert!(!Debouncer::is_ignored(Path::new(".gitignore")));
    }

    #[test]
    fn test_debouncing() {
        let mut debouncer = Debouncer::new();

        // Record event
        debouncer.record_event(PathBuf::from("test.txt"), FileEventType::Modified);

        // Immediately check - should not be settled yet
        assert_eq!(debouncer.get_settled_events().len(), 0);
        assert_eq!(debouncer.pending_count(), 1);

        // Wait for debounce interval
        std::thread::sleep(DEBOUNCE_INTERVAL + Duration::from_millis(100));

        // Now should be settled
        let settled = debouncer.get_settled_events();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].path, PathBuf::from("test.txt"));
        assert_eq!(settled[0].event_type, FileEventType::Modified);
        assert_eq!(debouncer.pending_count(), 0);
    }

    #[test]
    fn test_event_coalescing() {
        let mut debouncer = Debouncer::new();

        // Simulate rapid events on same file
        debouncer.record_event(PathBuf::from("test.txt"), FileEventType::Created);
        std::thread::sleep(Duration::from_millis(100));
        debouncer.record_event(PathBuf::from("test.txt"), FileEventType::Modified);
        std::thread::sleep(Duration::from_millis(100));
        debouncer.record_event(PathBuf::from("test.txt"), FileEventType::Modified);

        // Should have only 1 pending event (coalesced)
        assert_eq!(debouncer.pending_count(), 1);

        // Wait for settling
        std::thread::sleep(DEBOUNCE_INTERVAL + Duration::from_millis(100));

        // Should get single settled event (latest event type)
        let settled = debouncer.get_settled_events();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].event_type, FileEventType::Modified);
    }

    #[test]
    fn test_ignore_filtered_events() {
        let mut debouncer = Debouncer::new();

        // Try to record ignored file
        debouncer.record_event(PathBuf::from("file.swp"), FileEventType::Created);
        debouncer.record_event(PathBuf::from("node_modules/pkg/index.js"), FileEventType::Modified);

        // Should have 0 pending (all ignored)
        assert_eq!(debouncer.pending_count(), 0);
    }
}
