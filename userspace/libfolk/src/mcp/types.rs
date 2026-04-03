//! MCP Protocol Types — Layer 4 Transport with Session Multiplexing
//!
//! Wire format per frame:
//!   [COBS-encoded { FrameHeader + Postcard payload + CRC-16 }] [0x00]
//!
//! FrameHeader (8 bytes, prepended to every frame):
//!   session_id: u32  — random ID generated at OS boot, zombie killer
//!   seq_id:     u32  — monotonic counter, enables ACK/NACK/retransmit

use serde::{Serialize, Deserialize};
use heapless::String;

/// Maximum payload sizes
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_PROMPT_LEN: usize = 16384;
pub const MAX_RESULT_LEN: usize = 8192;
pub const MAX_TOOLS: usize = 32;
pub const MAX_CHUNK_SIZE: usize = 3072; // ~3KB per WASM chunk (fits in 4KB COBS frame)

// ── Frame Header (prepended to every wire frame) ────────────────────────

/// Transport header — included in every frame for session tracking + delivery guarantee.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct FrameHeader {
    /// Random session ID generated at OS boot. Frames with wrong session are dropped.
    pub session_id: u32,
    /// Monotonically increasing sequence number for ACK/NACK correlation.
    pub seq_id: u32,
}

// ── Request: Host (Python) → Server (Rust OS) ──────────────────────────

/// Messages from the LLM proxy to the OS.
/// Every variant is wrapped in a FrameHeader at the wire level.
#[derive(Serialize, Deserialize, Debug)]
pub enum McpRequest {
    /// Initial capability negotiation
    Initialize { protocol_version: u16 },
    /// Discover available OS tools
    ListTools,
    /// Execute a specific tool
    CallTool {
        name: String<MAX_NAME_LEN>,
        args: heapless::Vec<u8, 2048>,
    },
    /// LLM chat response
    ChatResponse {
        text: heapless::Vec<u8, MAX_RESULT_LEN>,
    },
    /// Time sync response from host
    TimeSync {
        year: u16, month: u8, day: u8,
        hour: u8, minute: u8, second: u8,
        utc_offset_minutes: i16,
    },
    /// Single WASM chunk (large binaries are split into <=3KB chunks)
    WasmChunk {
        total_chunks: u16,
        chunk_index: u16,
        data: heapless::Vec<u8, MAX_CHUNK_SIZE>,
    },
    /// Heartbeat
    Ping { seq: u32 },
    /// Acknowledgment of a received frame
    Ack,
    /// Negative acknowledgment (frame was corrupt or unparseable)
    Nack { reason: u8 },
    /// Notification (no response expected)
    Notification { kind: NotificationKind },
}

/// NACK reason codes
pub mod nack {
    pub const CRC_MISMATCH: u8 = 1;
    pub const PARSE_ERROR: u8 = 2;
    pub const SESSION_MISMATCH: u8 = 3;
    pub const CHUNK_OUT_OF_ORDER: u8 = 4;
}

/// Notification types
#[derive(Serialize, Deserialize, Debug)]
pub enum NotificationKind {
    Cancel,
    Shutdown,
}

// ── Response: Server (Rust OS) → Host (Python) ─────────────────────────

/// Messages from the OS to the LLM proxy.
#[derive(Serialize, Deserialize, Debug)]
pub enum McpResponse {
    /// Initialization with OS capabilities
    InitResult {
        os_version: u16,
        tool_count: u8,
    },
    /// List of available tools
    ToolsList {
        tools: heapless::Vec<ToolDescriptor, MAX_TOOLS>,
    },
    /// Result of a tool execution
    ToolResult {
        success: bool,
        data: heapless::Vec<u8, MAX_RESULT_LEN>,
    },
    /// Chat request to LLM
    ChatRequest {
        prompt: heapless::Vec<u8, MAX_PROMPT_LEN>,
    },
    /// Time sync request
    TimeSyncRequest,
    /// WASM generation request
    WasmGenRequest {
        description: String<256>,
    },
    /// Sampling request (reverse MCP: Server → Host)
    SamplingRequest {
        prompt: heapless::Vec<u8, MAX_PROMPT_LEN>,
        max_tokens: u16,
    },
    /// Pong response to heartbeat
    Pong { seq: u32 },
    /// Acknowledgment
    Ack,
    /// Negative acknowledgment
    Nack { reason: u8 },
    /// Error
    Error {
        code: u16,
        message: String<128>,
    },
}

/// Tool descriptor
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDescriptor {
    pub name: String<MAX_NAME_LEN>,
    pub description: String<128>,
    pub param_count: u8,
}

// ── Error Codes ─────────────────────────────────────────────────────────

pub mod error {
    pub const UNKNOWN_TOOL: u16 = 1;
    pub const PERMISSION_DENIED: u16 = 2;
    pub const INVALID_ARGS: u16 = 3;
    pub const EXECUTION_FAILED: u16 = 4;
    pub const TIMEOUT: u16 = 5;
    pub const FRAMING_ERROR: u16 = 6;
    pub const CRC_MISMATCH: u16 = 7;
    pub const BUFFER_OVERFLOW: u16 = 8;
}
