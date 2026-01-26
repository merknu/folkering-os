//! Query engine - Graph traversal and search

pub mod fts_search;
pub mod hybrid_search;
pub mod semantic;

use crate::models::{Node, Edge};
use sqlx::SqlitePool;
use anyhow::Result;

/// Query builder for graph traversal
pub struct QueryEngine {
    db: SqlitePool,
}

impl QueryEngine {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    /// Find files by tag (traverse TAGGED_WITH edges)
    pub async fn find_by_tag(&self, tag_name: &str) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            WITH tagged_files AS (
                SELECT e.source_id AS file_id
                FROM edges e
                JOIN nodes tag_node ON tag_node.id = e.target_id
                WHERE e.type = 'TAGGED_WITH'
                  AND tag_node.type = 'tag'
                  AND json_extract(tag_node.properties, '$.name') = ?
            )
            SELECT n.*
            FROM nodes n
            JOIN tagged_files tf ON n.id = tf.file_id
            ORDER BY n.updated_at DESC
            "#,
        )
        .bind(tag_name)
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Find files edited by a person
    /// Returns list of (node_id, node, weight) tuples
    pub async fn find_edited_by(&self, person_name: &str) -> Result<Vec<Node>> {
        let results = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN edges e ON n.id = e.source_id
            JOIN nodes person ON person.id = e.target_id
            WHERE e.type = 'EDITED_BY'
              AND person.type = 'person'
              AND json_extract(person.properties, '$.name') = ?
            ORDER BY e.weight DESC, n.updated_at DESC
            "#,
        )
        .bind(person_name)
        .fetch_all(&self.db)
        .await?;

        Ok(results)
    }

    /// Find files that co-occurred with a given file
    pub async fn find_co_occurring(&self, file_id: &str, min_weight: f32) -> Result<Vec<Node>> {
        let results = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN edges e ON (
                (e.source_id = ? AND e.target_id = n.id) OR
                (e.target_id = ? AND e.source_id = n.id)
            )
            WHERE e.type = 'CO_OCCURRED'
              AND e.weight >= ?
            ORDER BY e.weight DESC
            "#,
        )
        .bind(file_id)
        .bind(file_id)
        .bind(min_weight)
        .fetch_all(&self.db)
        .await?;

        Ok(results)
    }

    /// Find similar files (semantic similarity via vectors)
    pub async fn find_similar(&self, file_id: &str, min_similarity: f32) -> Result<Vec<Node>> {
        let results = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN edges e ON (
                (e.source_id = ? AND e.target_id = n.id) OR
                (e.target_id = ? AND e.source_id = n.id)
            )
            WHERE e.type = 'SIMILAR_TO'
              AND e.weight >= ?
            ORDER BY e.weight DESC
            "#,
        )
        .bind(file_id)
        .bind(file_id)
        .bind(min_similarity)
        .fetch_all(&self.db)
        .await?;

        Ok(results)
    }

    /// Find files in a project (traverse project hierarchy)
    pub async fn find_in_project(&self, project_name: &str) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            WITH RECURSIVE project_files AS (
                -- Find the project node
                SELECT id FROM nodes
                WHERE type = 'project'
                  AND json_extract(properties, '$.name') = ?

                UNION ALL

                -- Recursively find all contained files
                SELECT e.target_id
                FROM edges e
                JOIN project_files pf ON e.source_id = pf.id
                WHERE e.type = 'CONTAINS'
            )
            SELECT n.*
            FROM nodes n
            JOIN project_files pf ON n.id = pf.id
            WHERE n.type = 'file'
            ORDER BY n.updated_at DESC
            "#,
        )
        .bind(project_name)
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Find files by time window (what was I working on yesterday?)
    pub async fn find_by_timeframe(&self, start: &str, end: &str) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN session_events se ON n.id = se.file_id
            WHERE se.timestamp BETWEEN ? AND ?
            ORDER BY se.timestamp DESC
            "#,
        )
        .bind(start)
        .bind(end)
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Complex query: "Files I worked on today with Alice"
    pub async fn find_collaborative_files(&self, person_name: &str, date: &str) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            -- File was accessed today
            JOIN session_events se ON n.id = se.file_id
            -- File was edited by person
            JOIN edges e ON n.id = e.source_id
            JOIN nodes person ON person.id = e.target_id
            WHERE date(se.timestamp) = date(?)
              AND e.type = 'EDITED_BY'
              AND person.type = 'person'
              AND json_extract(person.properties, '$.name') = ?
            ORDER BY se.timestamp DESC
            "#,
        )
        .bind(date)
        .bind(person_name)
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Get all edges for a node (for visualization)
    pub async fn get_node_edges(&self, node_id: &str) -> Result<Vec<Edge>> {
        let edges = sqlx::query_as::<_, Edge>(
            r#"
            SELECT *
            FROM edges
            WHERE source_id = ? OR target_id = ?
            ORDER BY weight DESC
            "#,
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_all(&self.db)
        .await?;

        Ok(edges)
    }

    /// Get graph neighborhood (N hops from node)
    pub async fn get_neighborhood(&self, node_id: &str, max_hops: i32) -> Result<(Vec<Node>, Vec<Edge>)> {
        // Get nodes within N hops
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            WITH RECURSIVE neighborhood AS (
                -- Start with target node
                SELECT id, 0 AS hop
                FROM nodes
                WHERE id = ?

                UNION ALL

                -- Expand to neighbors
                SELECT
                    CASE
                        WHEN e.source_id = n.id THEN e.target_id
                        ELSE e.source_id
                    END AS id,
                    n.hop + 1 AS hop
                FROM neighborhood n
                JOIN edges e ON (e.source_id = n.id OR e.target_id = n.id)
                WHERE n.hop < ?
            )
            SELECT DISTINCT nodes.*
            FROM nodes
            JOIN neighborhood nb ON nodes.id = nb.id
            "#,
        )
        .bind(node_id)
        .bind(max_hops)
        .fetch_all(&self.db)
        .await?;

        // Get all edges between these nodes
        let node_ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();

        let edges = if !node_ids.is_empty() {
            // Build IN clause
            let placeholders = vec!["?"; node_ids.len()].join(",");
            let query = format!(
                r#"
                SELECT *
                FROM edges
                WHERE source_id IN ({}) AND target_id IN ({})
                ORDER BY weight DESC
                "#,
                placeholders, placeholders
            );

            let mut query_builder = sqlx::query_as::<_, Edge>(&query);
            for id in &node_ids {
                query_builder = query_builder.bind(id);
            }
            for id in &node_ids {
                query_builder = query_builder.bind(id);
            }

            query_builder.fetch_all(&self.db).await?
        } else {
            Vec::new()
        };

        Ok((nodes, edges))
    }

    /// Full-text search (fallback for now, will use FTS later)
    pub async fn search_files(&self, query: &str) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT *
            FROM nodes
            WHERE type = 'file'
              AND (
                properties LIKE ? OR
                json_extract(properties, '$.name') LIKE ?
              )
            ORDER BY updated_at DESC
            LIMIT 50
            "#,
        )
        .bind(format!("%{}%", query))
        .bind(format!("%{}%", query))
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    // ========================================================================
    // Phase 1.5 Day 4: Temporal Queries
    // ========================================================================

    /// Get all sessions in time range
    pub async fn get_sessions_in_timeframe(&self, start: &str, end: &str) -> Result<Vec<SessionInfo>> {
        let sessions: Vec<SessionInfo> = sqlx::query_as(
            r#"
            SELECT id, user_id, started_at, ended_at, is_active
            FROM sessions
            WHERE started_at BETWEEN ? AND ?
            ORDER BY started_at DESC
            "#
        )
        .bind(start)
        .bind(end)
        .fetch_all(&self.db)
        .await?;

        Ok(sessions)
    }

    /// Get files accessed in a specific session
    pub async fn get_files_in_session(&self, session_id: &str) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN session_events se ON n.id = se.file_id
            WHERE se.session_id = ?
            ORDER BY se.timestamp ASC
            "#
        )
        .bind(session_id)
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Get session events for a specific session
    pub async fn get_session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        let events: Vec<SessionEvent> = sqlx::query_as(
            r#"
            SELECT id, session_id, file_id, event_type, timestamp
            FROM session_events
            WHERE session_id = ?
            ORDER BY timestamp ASC
            "#
        )
        .bind(session_id)
        .fetch_all(&self.db)
        .await?;

        Ok(events)
    }

    /// Find files worked on today
    pub async fn find_files_today(&self) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN session_events se ON n.id = se.file_id
            WHERE date(se.timestamp) = date('now')
            ORDER BY se.timestamp DESC
            "#
        )
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Find files worked on yesterday
    pub async fn find_files_yesterday(&self) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN session_events se ON n.id = se.file_id
            WHERE date(se.timestamp) = date('now', '-1 day')
            ORDER BY se.timestamp DESC
            "#
        )
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Find files worked on this week
    pub async fn find_files_this_week(&self) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT DISTINCT n.*
            FROM nodes n
            JOIN session_events se ON n.id = se.file_id
            WHERE date(se.timestamp) >= date('now', 'weekday 0', '-6 days')
            ORDER BY se.timestamp DESC
            "#
        )
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Get session statistics
    pub async fn get_session_stats(&self) -> Result<SessionStats> {
        let total_sessions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions"
        )
        .fetch_one(&self.db)
        .await?;

        let active_sessions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE is_active = 1"
        )
        .fetch_one(&self.db)
        .await?;

        let total_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_events"
        )
        .fetch_one(&self.db)
        .await?;

        let avg_files_per_session: f32 = sqlx::query_scalar(
            r#"
            SELECT AVG(file_count)
            FROM (
                SELECT COUNT(DISTINCT file_id) as file_count
                FROM session_events
                GROUP BY session_id
            )
            "#
        )
        .fetch_one(&self.db)
        .await
        .unwrap_or(0.0);

        Ok(SessionStats {
            total_sessions: total_sessions as u64,
            active_sessions: active_sessions as u64,
            total_events: total_events as u64,
            avg_files_per_session,
        })
    }
}

/// Session information from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionInfo {
    pub id: String,
    pub user_id: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub is_active: i32,
}

/// Session event from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionEvent {
    pub id: i64,
    pub session_id: String,
    pub file_id: String,
    pub event_type: String,
    pub timestamp: String,
}

/// Session statistics
#[derive(Debug, Clone)]
pub struct SessionStats {
    pub total_sessions: u64,
    pub active_sessions: u64,
    pub total_events: u64,
    pub avg_files_per_session: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests would require DB setup
    // For now, just verify compilation
}
