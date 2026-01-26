//! Node models - Entities in the knowledge graph

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

/// Node type discriminator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum NodeType {
    File,
    Person,
    App,
    Event,
    Tag,
    Project,
    Location,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::File => "file",
            NodeType::Person => "person",
            NodeType::App => "app",
            NodeType::Event => "event",
            NodeType::Tag => "tag",
            NodeType::Project => "project",
            NodeType::Location => "location",
        }
    }
}

impl TryFrom<String> for NodeType {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "file" => Ok(NodeType::File),
            "person" => Ok(NodeType::Person),
            "app" => Ok(NodeType::App),
            "event" => Ok(NodeType::Event),
            "tag" => Ok(NodeType::Tag),
            "project" => Ok(NodeType::Project),
            "location" => Ok(NodeType::Location),
            _ => Err(format!("Invalid node type: {}", value)),
        }
    }
}

/// Node - Core entity in the knowledge graph
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Node {
    pub id: String,  // UUID as string
    #[sqlx(try_from = "String")]
    pub r#type: NodeType,
    pub properties: String,  // JSON string (sqlx doesn't support JSONB well on SQLite)
    pub created_at: String,  // ISO 8601
    pub updated_at: String,
}

impl Node {
    /// Create a new node
    pub fn new(node_type: NodeType, properties: JsonValue) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: Uuid::new_v4().to_string(),
            r#type: node_type,
            properties: properties.to_string(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Get properties as JSON
    pub fn get_properties(&self) -> Result<JsonValue, serde_json::Error> {
        serde_json::from_str(&self.properties)
    }

    /// Set properties from JSON
    pub fn set_properties(&mut self, properties: JsonValue) {
        self.properties = properties.to_string();
        self.updated_at = Utc::now().to_rfc3339();
    }

    /// Get a specific property
    pub fn get_property(&self, key: &str) -> Option<JsonValue> {
        self.get_properties()
            .ok()
            .and_then(|props| props.get(key).cloned())
    }
}

/// File node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProperties {
    pub name: String,
    pub size: u64,
    pub mime_type: Option<String>,
    pub extension: Option<String>,
    pub content_hash: Option<String>,  // SHA-256
    pub vector_embedding: Option<Vec<f32>>,  // Phase 2
}

/// Person node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonProperties {
    pub name: String,
    pub email: Option<String>,
    pub avatar: Option<String>,
}

/// App node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppProperties {
    pub name: String,
    pub executable: String,
    pub version: Option<String>,
}

/// Event node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventProperties {
    pub timestamp: DateTime<Utc>,
    pub duration: Option<u64>,  // seconds
    pub event_type: String,
}

/// Tag node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagProperties {
    pub name: String,
    pub color: Option<String>,  // Hex color
    pub parent: Option<String>,  // Parent tag UUID
}

/// Project node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectProperties {
    pub name: String,
    pub description: Option<String>,
    pub status: String,  // active, archived, completed
}

/// Location node properties
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationProperties {
    pub name: String,
    pub path: String,  // Legacy filesystem path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_creation() {
        let props = serde_json::json!({
            "name": "test.txt",
            "size": 1024
        });

        let node = Node::new(NodeType::File, props);

        assert_eq!(node.r#type, NodeType::File);
        assert_eq!(node.get_property("name").unwrap(), "test.txt");
        assert_eq!(node.get_property("size").unwrap(), 1024);
    }

    #[test]
    fn test_file_properties() {
        let file_props = FileProperties {
            name: "document.pdf".to_string(),
            size: 2048,
            mime_type: Some("application/pdf".to_string()),
            extension: Some(".pdf".to_string()),
            content_hash: None,
            vector_embedding: None,
        };

        let json = serde_json::to_value(&file_props).unwrap();
        let node = Node::new(NodeType::File, json);

        let props: FileProperties = serde_json::from_str(&node.properties).unwrap();
        assert_eq!(props.name, "document.pdf");
        assert_eq!(props.size, 2048);
    }
}
