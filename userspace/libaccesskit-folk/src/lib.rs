//! AccessKit-Compatible UI Tree Schema for Folkering OS
//!
//! This library provides zero-copy serializable UI tree structures compatible
//! with the AccessKit accessibility framework. It enables the "Semantic Mirror"
//! pattern where AI agents perceive UI through semantic trees instead of pixels.
//!
//! # Design Goals
//!
//! 1. **Zero-Copy IPC**: Using rkyv, the compositor can read UI updates directly
//!    from shared memory without deserialization overhead.
//!
//! 2. **Accessibility-First**: Every UI element has semantic meaning (role, name,
//!    value) that both assistive technologies and AI agents can understand.
//!
//! 3. **no_std Compatible**: Works in Folkering's userspace without std library.
//!
//! # Example Usage
//!
//! ```ignore
//! use libaccesskit_folk::{Node, Role, TreeUpdate, NodeId};
//!
//! // Create a simple button node
//! let button = Node {
//!     role: Role::Button,
//!     name: Some("Submit".into()),
//!     value: None,
//!     bounds: Some(Rect { x0: 10.0, y0: 10.0, x1: 100.0, y1: 40.0 }),
//!     children: Vec::new(),
//!     states: NodeStates::FOCUSABLE,
//! };
//!
//! // Create tree update
//! let update = TreeUpdate {
//!     root: 1,
//!     focus: Some(1),
//!     nodes: vec![(1, button)],
//! };
//!
//! // Serialize to shared memory (zero-copy on read side)
//! let bytes = rkyv::to_bytes::<_, 256>(&update).unwrap();
//! ```

//! AccessKit-Compatible UI Tree Schema for Folkering OS
//!
//! This library provides UI tree structures compatible with the AccessKit
//! accessibility framework. It enables the "Semantic Mirror" pattern where
//! AI agents perceive UI through semantic trees instead of pixels.
//!
//! # Design Goals
//!
//! 1. **Simple Serialization**: Plain data structures that can be copied
//!    directly to shared memory with minimal overhead.
//!
//! 2. **Accessibility-First**: Every UI element has semantic meaning (role, name,
//!    value) that both assistive technologies and AI agents can understand.
//!
//! 3. **no_std Compatible**: Works in Folkering's userspace without std library.
//!
//! # Serialization Strategy
//!
//! For Phase 6, we use a simple approach:
//! - Fixed-size header with counts and offsets
//! - Node data packed sequentially
//! - Strings as length-prefixed byte arrays
//!
//! Future phases may add rkyv zero-copy when alloc support is more mature.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use bitflags::bitflags;

/// Unique identifier for a node within a window's tree.
///
/// Node IDs are assigned by the application and must be stable across updates.
/// The compositor uses these to track which nodes changed.
pub type NodeId = u64;

/// Semantic role of a UI element.
///
/// Based on WAI-ARIA roles, these describe what kind of element this is
/// regardless of how it's visually rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    /// Unknown or unspecified role
    Unknown = 0,

    // === Containers ===
    /// Top-level application window
    Window = 1,
    /// Generic grouping container
    Group = 2,
    /// Scrollable content area
    ScrollView = 3,
    /// Tab panel container
    TabPanel = 4,
    /// Dialog or modal window
    Dialog = 5,
    /// Alert or notification
    Alert = 6,

    // === Interactive Elements ===
    /// Clickable button
    Button = 10,
    /// Checkbox (can be checked/unchecked)
    Checkbox = 11,
    /// Radio button (mutually exclusive selection)
    RadioButton = 12,
    /// Dropdown or combo box
    ComboBox = 13,
    /// Menu item
    MenuItem = 14,
    /// Hyperlink
    Link = 15,
    /// Slider for numeric range
    Slider = 16,
    /// Tab button
    Tab = 17,

    // === Text Elements ===
    /// Non-editable text
    StaticText = 20,
    /// Single-line text input
    TextInput = 21,
    /// Multi-line text area
    TextArea = 22,
    /// Label associated with another element
    Label = 23,
    /// Heading (h1-h6 equivalent)
    Heading = 24,

    // === Visual Elements ===
    /// Image or icon
    Image = 30,
    /// Progress indicator
    ProgressBar = 31,
    /// Separator or divider
    Separator = 32,

    // === Structural Elements ===
    /// List container
    List = 40,
    /// Item within a list
    ListItem = 41,
    /// Table container
    Table = 42,
    /// Table row
    TableRow = 43,
    /// Table cell
    TableCell = 44,
    /// Tree view container
    Tree = 45,
    /// Tree item
    TreeItem = 46,
}

bitflags! {
    /// State flags for UI nodes.
    ///
    /// These represent the current state of an element, which can change
    /// dynamically based on user interaction.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct NodeStates: u32 {
        /// Element can receive keyboard focus
        const FOCUSABLE = 1 << 0;
        /// Element currently has keyboard focus
        const FOCUSED = 1 << 1;
        /// Element is selected (in a list, tree, etc.)
        const SELECTED = 1 << 2;
        /// Checkbox/toggle is checked
        const CHECKED = 1 << 3;
        /// Element is disabled/non-interactive
        const DISABLED = 1 << 4;
        /// Element is expanded (tree node, accordion)
        const EXPANDED = 1 << 5;
        /// Element is collapsed
        const COLLAPSED = 1 << 6;
        /// Element is hidden/invisible
        const HIDDEN = 1 << 7;
        /// Element is read-only
        const READONLY = 1 << 8;
        /// Element is required (form validation)
        const REQUIRED = 1 << 9;
        /// Element has invalid input
        const INVALID = 1 << 10;
        /// Element is currently being pressed
        const PRESSED = 1 << 11;
        /// Element is busy/loading
        const BUSY = 1 << 12;
    }
}

/// Bounding rectangle for a UI element.
///
/// Coordinates are in window-local space with origin at top-left.
/// Units are logical pixels (may differ from physical pixels on HiDPI).
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Rect {
    /// Left edge X coordinate
    pub x0: f32,
    /// Top edge Y coordinate
    pub y0: f32,
    /// Right edge X coordinate
    pub x1: f32,
    /// Bottom edge Y coordinate
    pub y1: f32,
}

impl Rect {
    /// Create a new rectangle from edges.
    #[inline]
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// Width of the rectangle.
    #[inline]
    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }

    /// Height of the rectangle.
    #[inline]
    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }

    /// Check if a point is inside this rectangle.
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// A single node in the accessibility tree.
///
/// Each node represents one UI element with its semantic properties.
/// The tree structure is defined by the `children` field.
#[derive(Clone, Debug)]
pub struct Node {
    /// Semantic role of this element
    pub role: Role,

    /// Current state flags
    pub states: NodeStates,

    /// IDs of child nodes (defines tree structure)
    pub children: Vec<NodeId>,

    /// Human-readable name (e.g., button label, field name)
    /// This is what screen readers announce.
    pub name: Option<String>,

    /// Current value (e.g., text content, slider value)
    pub value: Option<String>,

    /// Description or help text
    pub description: Option<String>,

    /// Bounding rectangle in window coordinates
    pub bounds: Option<Rect>,

    /// For sliders/progress: minimum value
    pub value_min: Option<f64>,

    /// For sliders/progress: maximum value
    pub value_max: Option<f64>,

    /// For sliders/progress: current numeric value
    pub value_now: Option<f64>,
}

impl Node {
    /// Create a new node with the given role.
    pub fn new(role: Role) -> Self {
        Self {
            role,
            states: NodeStates::empty(),
            children: Vec::new(),
            name: None,
            value: None,
            description: None,
            bounds: None,
            value_min: None,
            value_max: None,
            value_now: None,
        }
    }

    /// Builder: set the name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Builder: set the value.
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// Builder: set the bounds.
    pub fn with_bounds(mut self, bounds: Rect) -> Self {
        self.bounds = Some(bounds);
        self
    }

    /// Builder: add states.
    pub fn with_states(mut self, states: NodeStates) -> Self {
        self.states |= states;
        self
    }

    /// Builder: add children.
    pub fn with_children(mut self, children: Vec<NodeId>) -> Self {
        self.children = children;
        self
    }
}

impl Default for Node {
    fn default() -> Self {
        Self::new(Role::Unknown)
    }
}

/// Atomic unit of UI tree change sent over IPC.
///
/// Applications send TreeUpdates to the compositor when their UI changes.
/// The compositor merges these into its WorldTree.
///
/// # Partial Updates
///
/// Only include nodes that changed since the last update. The compositor
/// patches its tree rather than replacing it entirely.
///
/// # Example
///
/// ```ignore
/// // User clicked a button, changing focus
/// let update = TreeUpdate {
///     root: 1,
///     focus: Some(42),  // New focus node
///     nodes: vec![
///         (42, button_node),  // Updated button state
///     ],
/// };
/// ```
#[derive(Clone, Debug)]
pub struct TreeUpdate {
    /// ID of the root node (defines tree top)
    pub root: NodeId,

    /// Currently focused node (keyboard focus)
    pub focus: Option<NodeId>,

    /// Changed nodes: (id, node) pairs
    /// Only include nodes that changed since last update
    pub nodes: Vec<(NodeId, Node)>,
}

impl TreeUpdate {
    /// Create an empty tree update with the given root.
    pub fn new(root: NodeId) -> Self {
        Self {
            root,
            focus: None,
            nodes: Vec::new(),
        }
    }

    /// Builder: set focus.
    pub fn with_focus(mut self, focus: NodeId) -> Self {
        self.focus = Some(focus);
        self
    }

    /// Builder: add a node.
    pub fn with_node(mut self, id: NodeId, node: Node) -> Self {
        self.nodes.push((id, node));
        self
    }
}

// ============================================================================
// Simple Serialization (Phase 6 - no rkyv)
// ============================================================================

// Note: Full zero-copy serialization with rkyv will be added in a future phase
// when we have proper alloc support in our no_std environment.
// For now, the compositor can receive TreeUpdate structs directly via IPC
// message passing (copying the small header, then reading node data from shmem).

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_builder() {
        let node = Node::new(Role::Button)
            .with_name("Submit")
            .with_bounds(Rect::new(10.0, 10.0, 100.0, 40.0))
            .with_states(NodeStates::FOCUSABLE | NodeStates::FOCUSED);

        assert_eq!(node.role, Role::Button);
        assert_eq!(node.name.as_deref(), Some("Submit"));
        assert!(node.states.contains(NodeStates::FOCUSABLE));
        assert!(node.states.contains(NodeStates::FOCUSED));
    }

    #[test]
    fn test_rect_methods() {
        let rect = Rect::new(10.0, 20.0, 110.0, 70.0);
        assert_eq!(rect.width(), 100.0);
        assert_eq!(rect.height(), 50.0);
        assert!(rect.contains(50.0, 40.0));
        assert!(!rect.contains(5.0, 40.0));
    }

    #[test]
    fn test_tree_update_builder() {
        let button = Node::new(Role::Button).with_name("OK");
        let update = TreeUpdate::new(1)
            .with_focus(2)
            .with_node(2, button);

        assert_eq!(update.root, 1);
        assert_eq!(update.focus, Some(2));
        assert_eq!(update.nodes.len(), 1);
    }
}
