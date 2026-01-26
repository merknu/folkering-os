//! Edge models - Relationships in the knowledge graph

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Edge type discriminator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeType {
    Contains,        // Folder contains file
    EditedBy,        // File edited by person
    OpenedWith,      // File opened with app
    Mentions,        // File mentions person/entity
    SharedWith,      // Shared between people
    HappenedDuring,  // Event occurred during time window
    CoOccurred,      // Files used together
    SimilarTo,       // Semantic similarity
    DependsOn,       // Code dependency
    References,      // Document references another
    ParentOf,        // Hierarchical relationship
    TaggedWith,      // Has tag
}

impl EdgeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeType::Contains => "CONTAINS",
            EdgeType::EditedBy => "EDITED_BY",
            EdgeType::OpenedWith => "OPENED_WITH",
            EdgeType::Mentions => "MENTIONS",
            EdgeType::SharedWith => "SHARED_WITH",
            EdgeType::HappenedDuring => "HAPPENED_DURING",
            EdgeType::CoOccurred => "CO_OCCURRED",
            EdgeType::SimilarTo => "SIMILAR_TO",
            EdgeType::DependsOn => "DEPENDS_ON",
            EdgeType::References => "REFERENCES",
            EdgeType::ParentOf => "PARENT_OF",
            EdgeType::TaggedWith => "TAGGED_WITH",
        }
    }
}

impl TryFrom<String> for EdgeType {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "CONTAINS" => Ok(EdgeType::Contains),
            "EDITED_BY" => Ok(EdgeType::EditedBy),
            "OPENED_WITH" => Ok(EdgeType::OpenedWith),
            "MENTIONS" => Ok(EdgeType::Mentions),
            "SHARED_WITH" => Ok(EdgeType::SharedWith),
            "HAPPENED_DURING" => Ok(EdgeType::HappenedDuring),
            "CO_OCCURRED" => Ok(EdgeType::CoOccurred),
            "SIMILAR_TO" => Ok(EdgeType::SimilarTo),
            "DEPENDS_ON" => Ok(EdgeType::DependsOn),
            "REFERENCES" => Ok(EdgeType::References),
            "PARENT_OF" => Ok(EdgeType::ParentOf),
            "TAGGED_WITH" => Ok(EdgeType::TaggedWith),
            _ => Err(format!("Invalid edge type: {}", value)),
        }
    }
}

/// Edge - Relationship between nodes
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Edge {
    pub id: Option<i64>,  // AUTOINCREMENT
    pub source_id: String,
    pub target_id: String,
    #[sqlx(try_from = "String")]
    pub r#type: EdgeType,
    pub weight: f32,  // 0.0 to 1.0
    pub properties: Option<String>,  // Optional JSON metadata
    pub created_at: String,  // ISO 8601
}

impl Edge {
    /// Create a new edge with weight validation
    pub fn new(
        source_id: String,
        target_id: String,
        edge_type: EdgeType,
        weight: f32,
        properties: Option<JsonValue>,
    ) -> Self {
        // Clamp weight to valid range
        let weight = weight.max(0.0).min(1.0);

        Self {
            id: None,
            source_id,
            target_id,
            r#type: edge_type,
            weight,
            properties: properties.map(|p| p.to_string()),
            created_at: Utc::now().to_rfc3339(),
        }
    }

    /// Create edge with default weight (0.5)
    pub fn new_default(
        source_id: String,
        target_id: String,
        edge_type: EdgeType,
    ) -> Self {
        Self::new(source_id, target_id, edge_type, 0.5, None)
    }

    /// Get properties as JSON
    pub fn get_properties(&self) -> Option<Result<JsonValue, serde_json::Error>> {
        self.properties.as_ref().map(|p| serde_json::from_str(p))
    }

    /// Set properties from JSON
    pub fn set_properties(&mut self, properties: Option<JsonValue>) {
        self.properties = properties.map(|p| p.to_string());
    }

    /// Check if edge is strong (weight > 0.7)
    pub fn is_strong(&self) -> bool {
        self.weight > 0.7
    }

    /// Check if edge is weak (weight < 0.3)
    pub fn is_weak(&self) -> bool {
        self.weight < 0.3
    }

    /// Strengthen edge (increase weight by factor, max 1.0)
    pub fn strengthen(&mut self, factor: f32) {
        self.weight = (self.weight * (1.0 + factor)).min(1.0);
    }

    /// Weaken edge (decrease weight by factor, min 0.0)
    pub fn weaken(&mut self, factor: f32) {
        self.weight = (self.weight * (1.0 - factor)).max(0.0);
    }
}

/// Helper for creating temporal co-occurrence edges
pub fn create_co_occurrence_edge(
    file1_id: String,
    file2_id: String,
    session_count: u32,  // How many times files were used together
) -> Edge {
    // Weight based on frequency: 1 session = 0.3, 5+ sessions = 1.0
    let weight = (0.3 + (session_count as f32 * 0.14)).min(1.0);

    Edge::new(
        file1_id,
        file2_id,
        EdgeType::CoOccurred,
        weight,
        Some(serde_json::json!({
            "session_count": session_count,
            "last_updated": Utc::now().to_rfc3339()
        })),
    )
}

/// Helper for creating similarity edges (from vector distance)
pub fn create_similarity_edge(
    file1_id: String,
    file2_id: String,
    cosine_similarity: f32,  // 0.0 to 1.0
) -> Edge {
    Edge::new(
        file1_id,
        file2_id,
        EdgeType::SimilarTo,
        cosine_similarity,
        Some(serde_json::json!({
            "similarity_score": cosine_similarity,
            "computed_at": Utc::now().to_rfc3339()
        })),
    )
}

/// Helper for creating file->person edit edges
pub fn create_edit_edge(
    file_id: String,
    person_id: String,
    edit_count: u32,
) -> Edge {
    // More edits = stronger relationship
    let weight = (0.5 + (edit_count as f32 * 0.1)).min(1.0);

    Edge::new(
        file_id,
        person_id,
        EdgeType::EditedBy,
        weight,
        Some(serde_json::json!({
            "edit_count": edit_count,
            "last_edit": Utc::now().to_rfc3339()
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_creation() {
        let edge = Edge::new(
            "file-1".to_string(),
            "person-1".to_string(),
            EdgeType::EditedBy,
            0.8,
            None,
        );

        assert_eq!(edge.r#type, EdgeType::EditedBy);
        assert_eq!(edge.weight, 0.8);
        assert!(edge.is_strong());
        assert!(!edge.is_weak());
    }

    #[test]
    fn test_weight_validation() {
        let edge = Edge::new(
            "file-1".to_string(),
            "file-2".to_string(),
            EdgeType::CoOccurred,
            1.5,  // Invalid, should clamp to 1.0
            None,
        );

        assert_eq!(edge.weight, 1.0);
    }

    #[test]
    fn test_edge_strengthening() {
        let mut edge = Edge::new_default(
            "file-1".to_string(),
            "file-2".to_string(),
            EdgeType::CoOccurred,
        );

        assert_eq!(edge.weight, 0.5);
        edge.strengthen(0.5);  // Increase by 50%
        assert_eq!(edge.weight, 0.75);
    }

    #[test]
    fn test_co_occurrence_helper() {
        let edge = create_co_occurrence_edge(
            "file-1".to_string(),
            "file-2".to_string(),
            5,
        );

        assert_eq!(edge.r#type, EdgeType::CoOccurred);
        assert!(edge.weight >= 0.9);  // 5 sessions = strong relationship
    }
}
