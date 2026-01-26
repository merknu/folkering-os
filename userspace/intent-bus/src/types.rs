//! Intent Bus Core Types
//!
//! Defines the intent system that replaces traditional copy-paste
//! with semantic understanding and AI-powered routing.

use serde::{Deserialize, Serialize};

/// Unique identifier for a task/app in the system
pub type TaskId = u32;

/// Semantic intent that can be routed to appropriate handlers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Intent {
    /// Open or find a file/resource
    /// Examples:
    ///   - "my presentation from last week"
    ///   - "todo.txt"
    ///   - "#work #important documents"
    OpenFile {
        query: String,
        context: Option<Context>,
    },

    /// Send a message/communication
    /// Examples:
    ///   - "Send 'meeting at 3pm' to the team"
    ///   - "Tell John the report is ready"
    SendMessage {
        text: String,
        recipients: Vec<String>, // Can be names, @handles, or semantic ("team")
        medium: Option<MessageMedium>,
    },

    /// Execute a command or workflow
    /// Examples:
    ///   - "Compile the project"
    ///   - "Deploy to production"
    ///   - "Run tests"
    RunCommand {
        command: String,
        args: Vec<String>,
        context: Option<Context>,
    },

    /// Transform data between apps
    /// Examples:
    ///   - "Convert this CSV to a chart"
    ///   - "Summarize this document"
    ///   - "Translate to Spanish"
    Transform {
        data: Vec<u8>,
        from_format: String,
        to_format: String,
        operation: Option<String>,
    },

    /// Create new content
    /// Examples:
    ///   - "Create a new note"
    ///   - "Start a presentation about AI"
    Create {
        content_type: String,
        initial_content: Option<String>,
        metadata: Vec<(String, String)>,
    },

    /// Search across all apps/data
    /// Examples:
    ///   - "Find emails about the budget"
    ///   - "Show all TODO items due this week"
    Search {
        query: String,
        filters: Vec<SearchFilter>,
    },
}

/// Context for intent execution (time, location, user state, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    /// Timestamp when intent was created
    pub timestamp: u64,

    /// Current focused app (if any)
    pub focused_app: Option<TaskId>,

    /// Recently used apps (for context-aware routing)
    pub recent_apps: Vec<TaskId>,

    /// User-defined tags/labels
    pub tags: Vec<String>,

    /// Custom metadata
    pub metadata: Vec<(String, String)>,
}

/// Communication medium preference
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageMedium {
    Chat,
    Email,
    SMS,
    Notification,
    Any, // Let AI decide
}

/// Search filter for narrowing results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SearchFilter {
    TimeRange { start: u64, end: u64 },
    Tag(String),
    AppType(String),
    FileType(String),
}

/// Result of intent routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingResult {
    /// Tasks that can handle this intent (ranked by confidence)
    pub handlers: Vec<Handler>,

    /// Execution plan (for multi-step intents)
    pub execution_plan: Vec<IntentStep>,

    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
}

/// A task that can handle an intent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handler {
    pub task_id: TaskId,
    pub task_name: String,
    pub confidence: f32,
    pub capabilities: Vec<String>,
}

/// Single step in intent execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentStep {
    pub handler: TaskId,
    pub action: String,
    pub data: Option<Vec<u8>>,
}

/// Capability registration from apps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub task_id: TaskId,
    pub task_name: String,

    /// What this app can do
    /// Examples: ["open_file", "edit_text", "send_message"]
    pub actions: Vec<String>,

    /// File types it handles
    /// Examples: [".txt", ".md", ".rs"]
    pub file_types: Vec<String>,

    /// Semantic tags
    /// Examples: ["editor", "communication", "productivity"]
    pub tags: Vec<String>,
}

/// Message types for IPC with Intent Bus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IntentMessage {
    /// Register a capability
    Register(Capability),

    /// Unregister when app exits
    Unregister(TaskId),

    /// Submit an intent for routing
    SubmitIntent(Intent),

    /// Response with routing result
    RoutingResponse(RoutingResult),

    /// Execute an intent step
    ExecuteStep(IntentStep),

    /// Report execution result
    ExecutionResult {
        success: bool,
        output: Option<Vec<u8>>,
        error: Option<String>,
    },
}
