//! Folkering OS Compositor - "Blind Compositor" for AI-Native UI
//!
//! The compositor maintains the WorldTree: a unified view of all UI state
//! across all windows. It receives AccessKit TreeUpdates from applications
//! via shared memory IPC and merges them into the world state.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     TreeUpdate     ┌────────────────┐
//! │ Application │ ───────────────────▶│   Compositor   │
//! │  (slint)    │   (shmem IPC)      │   (WorldTree)  │
//! └─────────────┘                     └───────┬────────┘
//!                                             │
//!                                     Query   │  Notify
//!                                             ▼
//!                                    ┌────────────────┐
//!                                    │    Synapse     │
//!                                    │  (AI Agent)    │
//!                                    └────────────────┘
//! ```
//!
//! # Key Concepts
//!
//! - **WorldTree**: The compositor's persistent view of all UI state
//! - **Zero-Copy**: TreeUpdates are read directly from shared memory
//! - **Semantic Mirror**: AI sees UI through accessibility tree, not pixels
//!
//! # Phase 6.2 - "First Light"
//!
//! The compositor now has basic graphics capabilities:
//! - Framebuffer access via SYS_MAP_PHYSICAL syscall
//! - Software rasterizer with Write-Combining optimization
//! - VGA 8x16 font for text rendering
//!
//! # Example
//!
//! ```ignore
//! let mut compositor = Compositor::new();
//!
//! // Process incoming IPC message
//! let (window_id, shmem_bytes) = receive_ipc();
//! compositor.process_update(window_id, shmem_bytes);
//!
//! // Query the world tree
//! if let Some(button) = compositor.find_by_name("Submit") {
//!     println!("Found button at {:?}", button.bounds);
//! }
//! ```

#![no_std]

// Graphics modules (Phase 6.2)
pub mod blend;
pub mod damage;
pub mod gfx_consumer;
pub mod gfx_dispatch;
pub mod render_graph;
pub mod framebuffer;
pub mod font;
pub mod intent;
pub mod ui_serialize;
pub mod vector_font;
pub mod graphics;
pub mod wasm_runtime;
pub mod driver_runtime;
pub mod folkshell;
pub mod spatial;
pub mod state;
pub mod slm_runtime;
pub mod window_manager;
pub mod agent;
pub mod draug;
pub mod refactor_types;
pub mod briefing;

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use libaccesskit_folk::{Node, NodeId, Role, TreeUpdate};

// ============================================================================
// Utility Functions
// ============================================================================

/// FNV-1a hash for name matching (matches libfolk::sys::compositor::hash_name)
fn hash_name(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// Parse a "__hash_XXXXXX" formatted name to extract the hash value.
/// Returns None if the name is not in this format.
fn parse_hash_name(name: &str) -> Option<u32> {
    // Expected format: "__hash_" followed by 6 hex digits (24-bit truncated hash)
    let prefix = "__hash_";
    if !name.starts_with(prefix) {
        return None;
    }

    let hex_part = &name[prefix.len()..];
    // Parse hex string - should be exactly 6 characters for our format
    if hex_part.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex_part, 16).ok()
}

/// Window identifier (assigned by compositor).
pub type WindowId = u64;

/// The compositor's persistent view of all UI across all windows.
///
/// Each window has its own accessibility tree. The WorldTree merges
/// these into a unified queryable structure.
pub struct WorldTree {
    /// Per-window node trees
    windows: BTreeMap<WindowId, WindowTree>,
}

/// A single window's accessibility tree.
struct WindowTree {
    /// Root node ID
    root: NodeId,

    /// Current focus node (if any)
    focus: Option<NodeId>,

    /// Node storage: id -> node
    nodes: BTreeMap<NodeId, Node>,
}

impl WorldTree {
    /// Create a new empty WorldTree.
    pub fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
        }
    }

    /// Process a TreeUpdate from an application.
    ///
    /// This is the main entry point for handling UI updates from applications.
    /// The update is passed directly (Phase 6 uses simple IPC, future phases
    /// may use zero-copy shared memory).
    ///
    /// # Arguments
    /// - `window_id`: Which window this update is for
    /// - `update`: The TreeUpdate to process
    ///
    /// # Returns
    /// - `Ok(())` on success
    /// - `Err(())` on error
    pub fn process_update(&mut self, window_id: WindowId, update: TreeUpdate) -> Result<(), ()> {
        // Get or create window tree
        let window = self.windows.entry(window_id).or_insert_with(|| WindowTree {
            root: update.root,
            focus: None,
            nodes: BTreeMap::new(),
        });

        // Update root and focus
        window.root = update.root;
        window.focus = update.focus;

        // Merge changed nodes into window tree
        for (id, node) in update.nodes {
            window.nodes.insert(id, node);
        }

        Ok(())
    }

    /// Process raw bytes from shared memory (placeholder for future zero-copy).
    ///
    /// In Phase 6, this just returns an error since we don't have rkyv.
    /// Future phases will implement proper zero-copy deserialization.
    #[allow(dead_code)]
    pub fn process_raw_payload(&mut self, _window_id: WindowId, _payload: &[u8]) -> Result<(), ()> {
        // TODO: Implement when rkyv no_std support is added
        Err(())
    }

    /// Find a node by name across all windows.
    ///
    /// Useful for AI queries like "find the Submit button".
    pub fn find_by_name(&self, name: &str) -> Option<(WindowId, NodeId, &Node)> {
        for (&window_id, window) in &self.windows {
            for (&node_id, node) in &window.nodes {
                if node.name.as_deref() == Some(name) {
                    return Some((window_id, node_id, node));
                }
            }
        }
        None
    }

    /// Find a node by name hash across all windows.
    ///
    /// Phase 6.1: Uses hash matching since full strings don't fit in IPC payload.
    /// Names are stored as "__hash_XXXXXXXX" format, so we extract and compare the hash.
    /// Returns (window_id, node_id, &Node) if found.
    pub fn find_by_name_hash(&self, target_hash: u32) -> Option<(WindowId, NodeId, &Node)> {
        for (&window_id, window) in &self.windows {
            for (&node_id, node) in &window.nodes {
                if let Some(ref name) = node.name {
                    // Check if name is in __hash_XXXXXXXX format
                    if let Some(stored_hash) = parse_hash_name(name) {
                        if stored_hash == target_hash {
                            return Some((window_id, node_id, node));
                        }
                    } else {
                        // Fall back to computing hash of actual name
                        if hash_name(name) == target_hash {
                            return Some((window_id, node_id, node));
                        }
                    }
                }
            }
        }
        None
    }

    /// Find nodes by role across all windows.
    ///
    /// Useful for queries like "list all buttons".
    pub fn find_by_role(&self, role: Role) -> Vec<(WindowId, NodeId, &Node)> {
        let mut results = Vec::new();
        for (&window_id, window) in &self.windows {
            for (&node_id, node) in &window.nodes {
                if node.role == role {
                    results.push((window_id, node_id, node));
                }
            }
        }
        results
    }

    /// Get the currently focused node (if any).
    pub fn get_focus(&self) -> Option<(WindowId, NodeId, &Node)> {
        for (&window_id, window) in &self.windows {
            if let Some(focus_id) = window.focus {
                if let Some(node) = window.nodes.get(&focus_id) {
                    return Some((window_id, focus_id, node));
                }
            }
        }
        None
    }

    /// Get a specific node by window and node ID.
    pub fn get_node(&self, window_id: WindowId, node_id: NodeId) -> Option<&Node> {
        self.windows.get(&window_id)?.nodes.get(&node_id)
    }

    /// Get the root node of a window.
    pub fn get_root(&self, window_id: WindowId) -> Option<(NodeId, &Node)> {
        let window = self.windows.get(&window_id)?;
        let node = window.nodes.get(&window.root)?;
        Some((window.root, node))
    }

    /// List all windows.
    pub fn windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.windows.keys().copied()
    }

    /// Get window tree for inspection.
    pub fn get_window(&self, window_id: WindowId) -> Option<WindowTreeView<'_>> {
        self.windows.get(&window_id).map(|w| WindowTreeView {
            root: w.root,
            focus: w.focus,
            nodes: &w.nodes,
        })
    }

    /// Remove a window (e.g., when it closes).
    pub fn remove_window(&mut self, window_id: WindowId) {
        self.windows.remove(&window_id);
    }

    /// Count total nodes across all windows.
    pub fn total_nodes(&self) -> usize {
        self.windows.values().map(|w| w.nodes.len()).sum()
    }
}

impl Default for WorldTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Read-only view of a window's tree.
pub struct WindowTreeView<'a> {
    /// Root node ID
    pub root: NodeId,
    /// Current focus
    pub focus: Option<NodeId>,
    /// All nodes
    pub nodes: &'a BTreeMap<NodeId, Node>,
}

impl<'a> WindowTreeView<'a> {
    /// Iterate over all nodes.
    pub fn iter_nodes(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter().map(|(&id, node)| (id, node))
    }

    /// Get children of a node.
    pub fn children(&self, node_id: NodeId) -> Vec<(NodeId, &Node)> {
        let Some(node) = self.nodes.get(&node_id) else {
            return Vec::new();
        };

        node.children
            .iter()
            .filter_map(|&child_id| {
                self.nodes.get(&child_id).map(|n| (child_id, n))
            })
            .collect()
    }

    /// Walk the tree depth-first from root.
    pub fn walk_dfs(&self) -> Vec<(NodeId, &Node, usize)> {
        let mut result = Vec::new();
        let mut stack = vec![(self.root, 0usize)];

        while let Some((node_id, depth)) = stack.pop() {
            if let Some(node) = self.nodes.get(&node_id) {
                result.push((node_id, node, depth));

                // Add children in reverse order so they're processed in order
                for &child_id in node.children.iter().rev() {
                    stack.push((child_id, depth + 1));
                }
            }
        }

        result
    }
}

// ============================================================================
// Compositor Service (Main Entry Point)
// ============================================================================

/// The Compositor service that runs as a Folkering OS task.
///
/// Handles IPC messages from applications and maintains the WorldTree.
pub struct Compositor {
    /// The unified world state
    pub world: WorldTree,

    /// Window ID counter
    next_window_id: WindowId,
}

impl Compositor {
    /// Create a new Compositor service.
    pub fn new() -> Self {
        Self {
            world: WorldTree::new(),
            next_window_id: 1,
        }
    }

    /// Allocate a new window ID.
    pub fn create_window(&mut self) -> WindowId {
        let id = self.next_window_id;
        self.next_window_id += 1;
        id
    }

    /// Process a TreeUpdate from an application.
    ///
    /// # Arguments
    /// - `window_id`: Which window sent this update
    /// - `update`: The TreeUpdate to process
    pub fn handle_update(&mut self, window_id: WindowId, update: TreeUpdate) -> Result<(), ()> {
        self.world.process_update(window_id, update)
    }

    /// Handle window close.
    pub fn handle_close(&mut self, window_id: WindowId) {
        self.world.remove_window(window_id);
    }
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use libaccesskit_folk::{Node, Role, NodeStates, Rect};

    fn create_test_update() -> TreeUpdate {
        let root = Node::new(Role::Window)
            .with_name("Test Window")
            .with_children(vec![2, 3]);

        let button = Node::new(Role::Button)
            .with_name("Submit")
            .with_bounds(Rect::new(10.0, 10.0, 100.0, 40.0))
            .with_states(NodeStates::FOCUSABLE);

        let text = Node::new(Role::StaticText)
            .with_name("Hello")
            .with_value("World");

        TreeUpdate::new(1)
            .with_focus(2)
            .with_node(1, root)
            .with_node(2, button)
            .with_node(3, text)
    }

    #[test]
    fn test_compositor_create_window() {
        let mut compositor = Compositor::new();
        let w1 = compositor.create_window();
        let w2 = compositor.create_window();
        assert_eq!(w1, 1);
        assert_eq!(w2, 2);
    }

    #[test]
    fn test_world_tree_find_by_name() {
        let mut world = WorldTree::new();

        // We'd need to serialize/deserialize to test fully
        // For now just test the empty case
        assert!(world.find_by_name("Submit").is_none());
    }

    #[test]
    fn test_world_tree_find_by_role() {
        let world = WorldTree::new();
        let buttons = world.find_by_role(Role::Button);
        assert!(buttons.is_empty());
    }
}
