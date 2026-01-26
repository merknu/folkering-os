//! Observer daemon - Watches filesystem and creates edges automatically

mod debouncer;

pub use debouncer::{Debouncer, FileEventType, PendingEvent};

use chrono::{DateTime, Utc, Duration};
use notify::{Watcher, RecursiveMode, Event, EventKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use anyhow::Result;

/// Session tracking for temporal co-occurrence
#[derive(Debug, Clone)]
pub struct FileAccessSession {
    pub session_id: String,
    pub user_id: Option<String>,
    pub accessed_files: Vec<String>,  // Node IDs
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

impl FileAccessSession {
    pub fn new(user_id: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            user_id,
            accessed_files: Vec::new(),
            started_at: now,
            last_activity: now,
        }
    }

    /// Check if session is still active (< 5 minutes since last activity)
    pub fn is_active(&self) -> bool {
        let now = Utc::now();
        let elapsed = now.signed_duration_since(self.last_activity);
        elapsed < Duration::minutes(5)
    }

    /// Add file to session
    pub fn add_file(&mut self, file_id: String) {
        self.last_activity = Utc::now();
        if !self.accessed_files.contains(&file_id) {
            self.accessed_files.push(file_id);
        }
    }
}

/// Observer daemon state
pub struct Observer {
    /// Current session (if any)
    current_session: Arc<Mutex<Option<FileAccessSession>>>,

    /// File access history for co-occurrence analysis
    /// Maps (file1_id, file2_id) -> session_count
    co_occurrence_counts: Arc<Mutex<HashMap<(String, String), u32>>>,

    /// File edit counts by user
    /// Maps (file_id, user_id) -> edit_count
    edit_counts: Arc<Mutex<HashMap<(String, String), u32>>>,

    /// Database connection for creating edges
    db: Option<sqlx::SqlitePool>,

    /// Event debouncer (NEW: Phase 1.5)
    debouncer: Arc<Mutex<Debouncer>>,
}

impl Observer {
    pub fn new() -> Self {
        Self {
            current_session: Arc::new(Mutex::new(None)),
            co_occurrence_counts: Arc::new(Mutex::new(HashMap::new())),
            edit_counts: Arc::new(Mutex::new(HashMap::new())),
            db: None,
            debouncer: Arc::new(Mutex::new(Debouncer::new())),
        }
    }

    pub fn with_db(db: sqlx::SqlitePool) -> Self {
        Self {
            current_session: Arc::new(Mutex::new(None)),
            co_occurrence_counts: Arc::new(Mutex::new(HashMap::new())),
            edit_counts: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db),
            debouncer: Arc::new(Mutex::new(Debouncer::new())),
        }
    }

    /// Start observing file system events
    pub async fn start(&self, watch_path: PathBuf) -> Result<()> {
        println!("[OBSERVER] Starting filesystem observer on: {:?}", watch_path);

        // Create file watcher
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })?;

        // Watch directory recursively
        watcher.watch(&watch_path, RecursiveMode::Recursive)?;

        println!("[OBSERVER] Watching for file events...");

        // Event loop
        while let Some(event) = rx.recv().await {
            self.handle_event(event).await;
        }

        Ok(())
    }

    /// Handle a file system event
    async fn handle_event(&self, event: Event) {
        match event.kind {
            EventKind::Access(_) => {
                // File was opened/accessed
                for path in event.paths {
                    self.handle_file_access(path).await;
                }
            }
            EventKind::Modify(_) => {
                // File was modified
                for path in event.paths {
                    self.handle_file_edit(path).await;
                }
            }
            EventKind::Create(_) => {
                // New file created
                for path in event.paths {
                    self.handle_file_creation(path).await;
                }
            }
            _ => {}
        }
    }

    /// Handle file access (opening)
    async fn handle_file_access(&self, path: PathBuf) {
        println!("[OBSERVER] File accessed: {:?}", path);

        // TODO: Convert path to node_id via DB lookup
        let file_id = path.to_string_lossy().to_string();

        // Get or create session
        let mut session_lock = self.current_session.lock().await;

        if session_lock.is_none() || !session_lock.as_ref().unwrap().is_active() {
            // Start new session
            *session_lock = Some(FileAccessSession::new(None));
            println!("[OBSERVER] Started new session");
        }

        let session = session_lock.as_mut().unwrap();
        session.add_file(file_id.clone());

        println!(
            "[OBSERVER] Session {} now has {} files",
            session.session_id,
            session.accessed_files.len()
        );

        // Update co-occurrence counts for all pairs in session
        if session.accessed_files.len() > 1 {
            self.update_co_occurrence(session).await;
        }
    }

    /// Update co-occurrence counts for all file pairs in session
    async fn update_co_occurrence(&self, session: &FileAccessSession) {
        let mut counts = self.co_occurrence_counts.lock().await;

        // For each pair of files in session
        for i in 0..session.accessed_files.len() {
            for j in (i + 1)..session.accessed_files.len() {
                let file1 = &session.accessed_files[i];
                let file2 = &session.accessed_files[j];

                // Create sorted pair to avoid (A,B) vs (B,A) duplication
                let pair = if file1 < file2 {
                    (file1.clone(), file2.clone())
                } else {
                    (file2.clone(), file1.clone())
                };

                *counts.entry(pair.clone()).or_insert(0) += 1;

                let count = counts[&pair];
                println!(
                    "[OBSERVER] Co-occurrence: {} <-> {} (count: {})",
                    pair.0, pair.1, count
                );

                // Create/update CO_OCCURRED edge in database
                if let Some(db) = &self.db {
                    self.create_cooccurrence_edge(db, &pair.0, &pair.1, count).await;
                }
            }
        }
    }

    /// Create or update a CO_OCCURRED edge
    async fn create_cooccurrence_edge(&self, db: &sqlx::SqlitePool, file1_id: &str, file2_id: &str, session_count: u32) {
        // Weight based on frequency: 1 session = 0.3, 5+ sessions = 1.0
        let weight = (0.3 + (session_count as f32 * 0.14)).min(1.0);

        let properties = serde_json::json!({
            "session_count": session_count,
            "last_updated": chrono::Utc::now().to_rfc3339()
        });

        let result = sqlx::query(
            r#"
            INSERT INTO edges (source_id, target_id, type, weight, properties, created_at)
            VALUES (?, ?, 'CO_OCCURRED', ?, ?, datetime('now'))
            ON CONFLICT(source_id, target_id, type)
            DO UPDATE SET weight = excluded.weight, properties = excluded.properties
            "#
        )
        .bind(file1_id)
        .bind(file2_id)
        .bind(weight)
        .bind(properties.to_string())
        .execute(db)
        .await;

        if let Err(e) = result {
            eprintln!("[OBSERVER] Failed to create CO_OCCURRED edge: {}", e);
        }
    }

    /// Handle file edit
    async fn handle_file_edit(&self, path: PathBuf) {
        println!("[OBSERVER] File edited: {:?}", path);

        // TODO: Get current user
        let user_id = "default-user".to_string();
        let file_id = path.to_string_lossy().to_string();

        // Update edit count
        let mut counts = self.edit_counts.lock().await;
        let pair = (file_id.clone(), user_id.clone());
        *counts.entry(pair.clone()).or_insert(0) += 1;

        let count = counts[&pair];
        println!(
            "[OBSERVER] Edit count: {} by {} = {}",
            file_id, user_id, count
        );

        // Create/update EDITED_BY edge
        if let Some(db) = &self.db {
            self.create_edit_edge(db, &file_id, &user_id, count).await;
        }
    }

    /// Create or update an EDITED_BY edge
    async fn create_edit_edge(&self, db: &sqlx::SqlitePool, file_id: &str, user_id: &str, edit_count: u32) {
        // More edits = stronger relationship
        let weight = (0.5 + (edit_count as f32 * 0.1)).min(1.0);

        let properties = serde_json::json!({
            "edit_count": edit_count,
            "last_edit": chrono::Utc::now().to_rfc3339()
        });

        let result = sqlx::query(
            r#"
            INSERT INTO edges (source_id, target_id, type, weight, properties, created_at)
            VALUES (?, ?, 'EDITED_BY', ?, ?, datetime('now'))
            ON CONFLICT(source_id, target_id, type)
            DO UPDATE SET weight = excluded.weight, properties = excluded.properties
            "#
        )
        .bind(file_id)
        .bind(user_id)
        .bind(weight)
        .bind(properties.to_string())
        .execute(db)
        .await;

        if let Err(e) = result {
            eprintln!("[OBSERVER] Failed to create EDITED_BY edge: {}", e);
        }
    }

    /// Handle file creation
    async fn handle_file_creation(&self, path: PathBuf) {
        println!("[OBSERVER] File created: {:?}", path);

        // TODO: Create file node in database
        // TODO: Extract entities from filename (NER)
        // TODO: Create MENTIONS edges for detected entities
    }

    /// Extract entities from text using simple pattern matching (Phase 1)
    /// Phase 2 will use proper NER models
    fn extract_entities(&self, text: &str) -> Vec<String> {
        let mut entities = Vec::new();

        // Simple email detection
        let email_regex = regex::Regex::new(r"\b[\w._%+-]+@[\w.-]+\.[A-Z]{2,}\b")
            .unwrap();
        for cap in email_regex.captures_iter(text) {
            entities.push(cap[0].to_string());
        }

        // Simple @mention detection
        let mention_regex = regex::Regex::new(r"@(\w+)").unwrap();
        for cap in mention_regex.captures_iter(text) {
            entities.push(cap[1].to_string());
        }

        entities
    }

    /// Get current session stats
    pub async fn get_session_stats(&self) -> Option<(usize, usize)> {
        let session = self.current_session.lock().await;
        session.as_ref().map(|s| {
            let files = s.accessed_files.len();
            let pairs = if files > 1 { files * (files - 1) / 2 } else { 0 };
            (files, pairs)
        })
    }

    /// Public API for simulating file access (for testing)
    pub async fn handle_file_access_with_id(&self, file_id: String) {
        let mut session_lock = self.current_session.lock().await;

        // Create or reuse session
        let needs_new_session = session_lock.is_none() || !session_lock.as_ref().unwrap().is_active();

        if needs_new_session {
            // End old session if exists
            if let Some(old_session) = session_lock.as_ref() {
                if let Some(db) = &self.db {
                    self.end_session_in_db(db, &old_session.session_id).await;
                }
            }

            // Create new session
            let new_session = FileAccessSession::new(None);

            // Persist to database
            if let Some(db) = &self.db {
                self.persist_session_to_db(db, &new_session).await;
            }

            *session_lock = Some(new_session);
        }

        let session = session_lock.as_mut().unwrap();
        let session_id = session.session_id.clone();
        session.add_file(file_id.clone());

        // Record session event to database
        if let Some(db) = &self.db {
            self.record_session_event_to_db(db, &session_id, &file_id, "access").await;
        }

        if session.accessed_files.len() > 1 {
            drop(session_lock);
            self.update_co_occurrence_from_current_session().await;
        }
    }

    /// Public API for simulating file edit (for testing)
    pub async fn handle_file_edit_with_user(&self, file_id: String, user_id: String) {
        let mut counts = self.edit_counts.lock().await;
        let pair = (file_id.clone(), user_id.clone());
        *counts.entry(pair.clone()).or_insert(0) += 1;
        let count = counts[&pair];
        drop(counts);

        if let Some(db) = &self.db {
            self.create_edit_edge(db, &file_id, &user_id, count).await;
        }
    }

    /// Public API for clearing session (for testing)
    pub async fn clear_session(&self) {
        let mut session_lock = self.current_session.lock().await;
        *session_lock = None;
    }

    async fn update_co_occurrence_from_current_session(&self) {
        let session_lock = self.current_session.lock().await;
        if let Some(session) = session_lock.as_ref() {
            let mut counts = self.co_occurrence_counts.lock().await;

            for i in 0..session.accessed_files.len() {
                for j in (i + 1)..session.accessed_files.len() {
                    let file1 = &session.accessed_files[i];
                    let file2 = &session.accessed_files[j];

                    let pair = if file1 < file2 {
                        (file1.clone(), file2.clone())
                    } else {
                        (file2.clone(), file1.clone())
                    };

                    *counts.entry(pair.clone()).or_insert(0) += 1;
                    let count = counts[&pair];

                    if let Some(db) = &self.db {
                        self.create_cooccurrence_edge(db, &pair.0, &pair.1, count).await;
                    }
                }
            }
        }
    }

    // ========================================================================
    // Phase 1.5: Debounced Event Handling
    // ========================================================================

    /// Record a raw filesystem event (will be debounced)
    pub async fn record_file_event(&self, path: PathBuf, event_type: FileEventType) {
        let mut debouncer = self.debouncer.lock().await;
        debouncer.record_event(path, event_type);
    }

    /// Process all settled events (call this periodically in a loop)
    pub async fn process_settled_events(&self) {
        let mut debouncer = self.debouncer.lock().await;
        let settled_events = debouncer.get_settled_events();
        drop(debouncer); // Release lock before processing

        for event in settled_events {
            self.handle_settled_event(event).await;
        }
    }

    /// Handle a single settled event
    async fn handle_settled_event(&self, event: PendingEvent) {
        match event.event_type {
            FileEventType::Created | FileEventType::Modified => {
                // File was created or modified - index it
                println!("[OBSERVER] File settled: {:?} ({})", event.path, match event.event_type {
                    FileEventType::Created => "created",
                    FileEventType::Modified => "modified",
                    _ => "unknown",
                });

                // TODO: Lookup or create node_id for this path
                // TODO: Call handle_file_access_with_id()
                // For now, just log
            }
            FileEventType::Deleted => {
                println!("[OBSERVER] File deleted: {:?}", event.path);
                // TODO: Mark node as deleted in database
            }
            FileEventType::Renamed => {
                println!("[OBSERVER] File renamed: {:?}", event.path);
                // TODO: Update file_paths table
            }
        }
    }

    /// Start debounce processing loop (runs in background)
    pub async fn start_debounce_processor(&self) {
        let debouncer = self.debouncer.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;

                // Process settled events
                let settled = {
                    let mut deb = debouncer.lock().await;
                    deb.get_settled_events()
                };

                for event in settled {
                    // Note: This is a simplified version for testing
                    // In production, you'd want proper error handling
                    println!("[DEBOUNCER] Settled event: {:?} ({})",
                        event.path,
                        match event.event_type {
                            FileEventType::Created => "created",
                            FileEventType::Modified => "modified",
                            FileEventType::Deleted => "deleted",
                            FileEventType::Renamed => "renamed",
                        }
                    );
                }
            }
        });
    }

    /// Get count of pending (not yet settled) events
    pub async fn pending_event_count(&self) -> usize {
        let debouncer = self.debouncer.lock().await;
        debouncer.pending_count()
    }

    // ========================================================================
    // Phase 1.5 Day 4: Session Persistence
    // ========================================================================

    /// Persist session to database (INSERT)
    async fn persist_session_to_db(&self, db: &sqlx::SqlitePool, session: &FileAccessSession) {
        let user_id_str = session.user_id.as_ref().map(|s| s.as_str());

        let result = sqlx::query(
            r#"
            INSERT INTO sessions (id, user_id, started_at, ended_at, is_active)
            VALUES (?, ?, ?, NULL, 1)
            "#
        )
        .bind(&session.session_id)
        .bind(user_id_str)
        .bind(session.started_at.to_rfc3339())
        .execute(db)
        .await;

        if let Err(e) = result {
            eprintln!("[OBSERVER] Failed to persist session: {}", e);
        }
    }

    /// End session in database (UPDATE)
    async fn end_session_in_db(&self, db: &sqlx::SqlitePool, session_id: &str) {
        let result = sqlx::query(
            r#"
            UPDATE sessions
            SET ended_at = datetime('now'), is_active = 0
            WHERE id = ?
            "#
        )
        .bind(session_id)
        .execute(db)
        .await;

        if let Err(e) = result {
            eprintln!("[OBSERVER] Failed to end session: {}", e);
        }
    }

    /// Record session event to database
    async fn record_session_event_to_db(
        &self,
        db: &sqlx::SqlitePool,
        session_id: &str,
        file_id: &str,
        event_type: &str,
    ) {
        let result = sqlx::query(
            r#"
            INSERT INTO session_events (session_id, file_id, event_type, timestamp)
            VALUES (?, ?, ?, datetime('now'))
            "#
        )
        .bind(session_id)
        .bind(file_id)
        .bind(event_type)
        .execute(db)
        .await;

        if let Err(e) = result {
            eprintln!("[OBSERVER] Failed to record session event: {}", e);
        }
    }

    /// Get current active session info (for debugging)
    pub async fn get_current_session_info(&self) -> Option<(String, usize, String)> {
        let session_lock = self.current_session.lock().await;
        session_lock.as_ref().map(|s| {
            (
                s.session_id.clone(),
                s.accessed_files.len(),
                s.started_at.to_rfc3339()
            )
        })
    }

    /// Force end current session and persist to database
    pub async fn force_end_current_session(&self) {
        let mut session_lock = self.current_session.lock().await;

        if let Some(session) = session_lock.take() {
            if let Some(db) = &self.db {
                self.end_session_in_db(db, &session.session_id).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let session = FileAccessSession::new(Some("user-123".to_string()));
        assert!(session.is_active());
        assert_eq!(session.accessed_files.len(), 0);
    }

    #[test]
    fn test_session_file_tracking() {
        let mut session = FileAccessSession::new(None);
        session.add_file("file-1".to_string());
        session.add_file("file-2".to_string());
        session.add_file("file-1".to_string());  // Duplicate

        assert_eq!(session.accessed_files.len(), 2);  // No duplicates
    }

    #[test]
    fn test_entity_extraction() {
        let observer = Observer::new();

        let text = "Email john@example.com and @alice about the meeting";
        let entities = observer.extract_entities(text);

        assert!(entities.contains(&"john@example.com".to_string()));
        assert!(entities.contains(&"alice".to_string()));
    }
}
