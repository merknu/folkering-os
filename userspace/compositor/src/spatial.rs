//! Spatial Pipelining — Node-based visual pipeline editor
//!
//! Users drag connections between window I/O ports with the mouse.
//! The compositor translates visual connections into Tick-Tock
//! streaming pipelines running inside their respective windows.

extern crate alloc;
use alloc::vec::Vec;

/// A connection between two windowed WASM nodes.
/// Data flows from source's output port to dest's input port.
#[derive(Clone, Debug)]
pub struct NodeConnection {
    pub source_win_id: u32,
    pub dest_win_id: u32,
}

/// Active connection drag state (user is dragging a cable from an output port).
#[derive(Clone, Debug)]
pub struct ConnectionDrag {
    pub source_win_id: u32,
    pub current_x: i32,
    pub current_y: i32,
}

/// Port visual constants
pub const PORT_RADIUS: i32 = 6;
pub const PORT_COLOR_IDLE: u32 = 0x00888888;    // gray when unconnected
pub const PORT_COLOR_CONNECTED: u32 = 0x0044FF44; // green when connected
pub const PORT_COLOR_DRAG: u32 = 0x003498db;     // blue during drag
pub const CONNECTION_COLOR: u32 = 0x009b59b6;     // purple connection line

/// Check if a window is connected as source in any connection
pub fn is_source(connections: &[NodeConnection], win_id: u32) -> bool {
    connections.iter().any(|c| c.source_win_id == win_id)
}

/// Check if a window is connected as dest in any connection
pub fn is_dest(connections: &[NodeConnection], win_id: u32) -> bool {
    connections.iter().any(|c| c.dest_win_id == win_id)
}

/// Remove all connections involving a window (when window is closed)
pub fn remove_connections_for(connections: &mut Vec<NodeConnection>, win_id: u32) {
    connections.retain(|c| c.source_win_id != win_id && c.dest_win_id != win_id);
}
