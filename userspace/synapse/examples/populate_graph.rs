//! Populate the graph with example data to test functionality

use synapse::{GraphDB, Node, Edge, NodeType, EdgeType, QueryEngine};
use sqlx::SqlitePool;
use anyhow::Result;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    println!("🧪 Testing Synapse Graph Filesystem\n");

    // Create in-memory database for testing
    let db = SqlitePool::connect("sqlite::memory:").await?;

    println!("📦 Running migrations...");
    run_migrations(&db).await?;
    println!("✅ Migrations complete\n");

    let graph = GraphDB::new(db.clone());
    let query = QueryEngine::new(db.clone());

    // Create test data
    println!("📝 Creating test data...\n");

    // Create people
    let alice = Node::new(
        NodeType::Person,
        json!({
            "name": "Alice",
            "email": "alice@example.com"
        })
    );
    graph.create_node(&alice).await?;
    println!("  Created person: Alice ({})", alice.id);

    let bob = Node::new(
        NodeType::Person,
        json!({
            "name": "Bob",
            "email": "bob@example.com"
        })
    );
    graph.create_node(&bob).await?;
    println!("  Created person: Bob ({})", bob.id);

    // Create tags
    let work_tag = Node::new(
        NodeType::Tag,
        json!({
            "name": "work",
            "color": "#3b82f6"
        })
    );
    graph.create_node(&work_tag).await?;
    println!("  Created tag: work ({})", work_tag.id);

    let ml_tag = Node::new(
        NodeType::Tag,
        json!({
            "name": "machine-learning",
            "color": "#10b981"
        })
    );
    graph.create_node(&ml_tag).await?;
    println!("  Created tag: machine-learning ({})", ml_tag.id);

    // Create project
    let project = Node::new(
        NodeType::Project,
        json!({
            "name": "ai-research",
            "description": "Machine learning research project",
            "status": "active"
        })
    );
    graph.create_node(&project).await?;
    println!("  Created project: ai-research ({})", project.id);

    // Create files
    let report = Node::new(
        NodeType::File,
        json!({
            "name": "report.pdf",
            "size": 2048,
            "mime_type": "application/pdf",
            "extension": ".pdf"
        })
    );
    graph.create_node(&report).await?;
    graph.register_path(&report.id, "/work/report.pdf").await?;
    println!("  Created file: report.pdf ({})", report.id);

    let dataset = Node::new(
        NodeType::File,
        json!({
            "name": "dataset.csv",
            "size": 10240,
            "mime_type": "text/csv",
            "extension": ".csv"
        })
    );
    graph.create_node(&dataset).await?;
    graph.register_path(&dataset.id, "/work/dataset.csv").await?;
    println!("  Created file: dataset.csv ({})", dataset.id);

    let model_py = Node::new(
        NodeType::File,
        json!({
            "name": "model.py",
            "size": 4096,
            "mime_type": "text/x-python",
            "extension": ".py"
        })
    );
    graph.create_node(&model_py).await?;
    graph.register_path(&model_py.id, "/work/model.py").await?;
    println!("  Created file: model.py ({})", model_py.id);

    let notebook = Node::new(
        NodeType::File,
        json!({
            "name": "analysis.ipynb",
            "size": 8192,
            "mime_type": "application/x-ipynb+json",
            "extension": ".ipynb"
        })
    );
    graph.create_node(&notebook).await?;
    graph.register_path(&notebook.id, "/work/analysis.ipynb").await?;
    println!("  Created file: analysis.ipynb ({})", notebook.id);

    println!("\n🔗 Creating edges...\n");

    // Alice edited report and model
    let edge1 = Edge::new(
        report.id.clone(),
        alice.id.clone(),
        EdgeType::EditedBy,
        0.9, // High edit frequency
        Some(json!({"edit_count": 15}))
    );
    graph.upsert_edge(&edge1).await?;
    println!("  report.pdf EDITED_BY Alice (0.9)");

    let edge2 = Edge::new(
        model_py.id.clone(),
        alice.id.clone(),
        EdgeType::EditedBy,
        0.95,
        Some(json!({"edit_count": 23}))
    );
    graph.upsert_edge(&edge2).await?;
    println!("  model.py EDITED_BY Alice (0.95)");

    // Bob edited dataset and notebook
    let edge3 = Edge::new(
        dataset.id.clone(),
        bob.id.clone(),
        EdgeType::EditedBy,
        0.8,
        Some(json!({"edit_count": 10}))
    );
    graph.upsert_edge(&edge3).await?;
    println!("  dataset.csv EDITED_BY Bob (0.8)");

    let edge4 = Edge::new(
        notebook.id.clone(),
        bob.id.clone(),
        EdgeType::EditedBy,
        0.7,
        Some(json!({"edit_count": 8}))
    );
    graph.upsert_edge(&edge4).await?;
    println!("  analysis.ipynb EDITED_BY Bob (0.7)");

    // Co-occurrence: model.py and dataset.csv are used together
    let edge5 = Edge::new(
        model_py.id.clone(),
        dataset.id.clone(),
        EdgeType::CoOccurred,
        0.85, // Used together 5+ times
        Some(json!({"session_count": 6}))
    );
    graph.upsert_edge(&edge5).await?;
    println!("  model.py CO_OCCURRED dataset.csv (0.85)");

    // report and model are similar (both ML-related)
    let edge6 = Edge::new(
        report.id.clone(),
        model_py.id.clone(),
        EdgeType::SimilarTo,
        0.75,
        Some(json!({"similarity_score": 0.75}))
    );
    graph.upsert_edge(&edge6).await?;
    println!("  report.pdf SIMILAR_TO model.py (0.75)");

    // Tag files
    let edge7 = Edge::new(
        report.id.clone(),
        work_tag.id.clone(),
        EdgeType::TaggedWith,
        1.0,
        None
    );
    graph.upsert_edge(&edge7).await?;
    println!("  report.pdf TAGGED_WITH work");

    let edge8 = Edge::new(
        model_py.id.clone(),
        ml_tag.id.clone(),
        EdgeType::TaggedWith,
        1.0,
        None
    );
    graph.upsert_edge(&edge8).await?;
    println!("  model.py TAGGED_WITH machine-learning");

    let edge9 = Edge::new(
        dataset.id.clone(),
        ml_tag.id.clone(),
        EdgeType::TaggedWith,
        1.0,
        None
    );
    graph.upsert_edge(&edge9).await?;
    println!("  dataset.csv TAGGED_WITH machine-learning");

    // Project contains files
    let edge10 = Edge::new(
        project.id.clone(),
        report.id.clone(),
        EdgeType::Contains,
        1.0,
        None
    );
    graph.upsert_edge(&edge10).await?;
    println!("  ai-research CONTAINS report.pdf");

    let edge11 = Edge::new(
        project.id.clone(),
        model_py.id.clone(),
        EdgeType::Contains,
        1.0,
        None
    );
    graph.upsert_edge(&edge11).await?;
    println!("  ai-research CONTAINS model.py");

    let edge12 = Edge::new(
        project.id.clone(),
        dataset.id.clone(),
        EdgeType::Contains,
        1.0,
        None
    );
    graph.upsert_edge(&edge12).await?;
    println!("  ai-research CONTAINS dataset.csv");

    // Print statistics
    println!("\n📊 Graph Statistics:");
    let stats = graph.get_stats().await?;
    println!("  Nodes: {}", stats.node_count);
    println!("  Edges: {}", stats.edge_count);
    println!("  Avg edge weight: {:.2}", stats.avg_edge_weight);

    // Test queries
    println!("\n🔍 Testing Queries:\n");

    // Query 1: Find files by tag
    println!("Query 1: Files tagged with 'machine-learning'");
    let ml_files = query.find_by_tag("machine-learning").await?;
    println!("  Found {} files:", ml_files.len());
    for file in &ml_files {
        let name = file.get_property("name").unwrap_or_default();
        println!("    - {}", name);
    }
    assert_eq!(ml_files.len(), 2, "Should find 2 ML files");

    // Query 2: Find files edited by Alice
    println!("\nQuery 2: Files edited by Alice");
    let alice_files = query.find_edited_by("Alice").await?;
    println!("  Found {} files:", alice_files.len());
    for file in &alice_files {
        let name = file.get_property("name").unwrap_or_default();
        println!("    - {}", name);
    }
    assert_eq!(alice_files.len(), 2, "Alice edited 2 files");

    // Query 3: Find files co-occurring with model.py
    println!("\nQuery 3: Files co-occurring with model.py");
    let cooccur = query.find_co_occurring(&model_py.id, 0.5).await?;
    println!("  Found {} files:", cooccur.len());
    for file in &cooccur {
        let name = file.get_property("name").unwrap_or_default();
        println!("    - {}", name);
    }
    assert_eq!(cooccur.len(), 1, "Should find dataset.csv");

    // Query 4: Find similar files to report.pdf
    println!("\nQuery 4: Files similar to report.pdf");
    let similar = query.find_similar(&report.id, 0.5).await?;
    println!("  Found {} files:", similar.len());
    for file in &similar {
        let name = file.get_property("name").unwrap_or_default();
        println!("    - {}", name);
    }
    assert_eq!(similar.len(), 1, "Should find model.py");

    // Query 5: Find files in project
    println!("\nQuery 5: Files in project 'ai-research'");
    let project_files = query.find_in_project("ai-research").await?;
    println!("  Found {} files:", project_files.len());
    for file in &project_files {
        let name = file.get_property("name").unwrap_or_default();
        println!("    - {}", name);
    }
    assert_eq!(project_files.len(), 3, "Project contains 3 files");

    // Query 6: Get neighborhood of report.pdf
    println!("\nQuery 6: Neighborhood of report.pdf (2 hops)");
    let (nodes, edges) = query.get_neighborhood(&report.id, 2).await?;
    println!("  Nodes in neighborhood: {}", nodes.len());
    println!("  Edges in neighborhood: {}", edges.len());
    assert!(nodes.len() >= 3, "Should find multiple connected nodes");

    // Query 7: Get node by path
    println!("\nQuery 7: Get node by path '/work/model.py'");
    let node_by_path = graph.get_node_by_path("/work/model.py").await?;
    assert!(node_by_path.is_some(), "Should find node by path");
    println!("  Found: {}", node_by_path.unwrap().get_property("name").unwrap_or_default());

    // Query 8: Strongest edges
    println!("\nQuery 8: Top 5 strongest edges");
    let strong = graph.get_strongest_edges(5).await?;
    println!("  Found {} strong edges:", strong.len());
    for edge in &strong {
        println!("    - {} -> {} ({}) [weight: {:.2}]",
            edge.source_id.chars().take(8).collect::<String>(),
            edge.target_id.chars().take(8).collect::<String>(),
            edge.r#type.as_str(),
            edge.weight
        );
    }

    // Test graph algorithms
    println!("\n🧮 Testing Graph Algorithms:\n");

    use synapse::graph::GraphAlgorithms;

    let all_edges = vec![edge1, edge2, edge3, edge4, edge5, edge6, edge7, edge8, edge9, edge10, edge11, edge12];
    let importance = GraphAlgorithms::calculate_importance(&all_edges);

    println!("Node importance scores:");
    let mut importance_vec: Vec<_> = importance.iter().collect();
    importance_vec.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
    for (node_id, score) in importance_vec.iter().take(5) {
        // Find node name
        if let Ok(Some(node)) = graph.get_node(node_id).await {
            let name = node.get_property("name").unwrap_or_default();
            println!("  {} ({}): {:.2}", name, node.r#type.as_str(), score);
        }
    }

    println!("\n✅ All tests passed! Synapse is fully functional.\n");

    Ok(())
}

async fn run_migrations(db: &SqlitePool) -> Result<()> {
    // Create tables manually (more reliable than parsing SQL)

    // Nodes table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS nodes (
            id TEXT PRIMARY KEY NOT NULL,
            type TEXT NOT NULL,
            properties TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            CHECK (type IN ('file', 'person', 'app', 'event', 'tag', 'project', 'location'))
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(type)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_nodes_created ON nodes(created_at)").execute(db).await?;

    // Edges table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id TEXT NOT NULL,
            target_id TEXT NOT NULL,
            type TEXT NOT NULL,
            weight REAL NOT NULL DEFAULT 0.5,
            properties TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (source_id) REFERENCES nodes(id) ON DELETE CASCADE,
            FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE CASCADE,
            UNIQUE(source_id, target_id, type),
            CHECK (type IN (
                'CONTAINS', 'EDITED_BY', 'OPENED_WITH', 'MENTIONS',
                'SHARED_WITH', 'HAPPENED_DURING', 'CO_OCCURRED',
                'SIMILAR_TO', 'DEPENDS_ON', 'REFERENCES', 'PARENT_OF', 'TAGGED_WITH'
            ))
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_type ON edges(type)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_weight ON edges(weight DESC)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_created ON edges(created_at)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_both ON edges(source_id, target_id)").execute(db).await?;

    // Sessions table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY NOT NULL,
            user_id TEXT,
            started_at TEXT NOT NULL,
            ended_at TEXT,
            is_active INTEGER DEFAULT 1,
            CHECK (is_active IN (0, 1))
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_active ON sessions(is_active, started_at)").execute(db).await?;

    // Session events table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS session_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            file_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY (file_id) REFERENCES nodes(id) ON DELETE CASCADE,
            CHECK (event_type IN ('open', 'edit', 'close', 'save'))
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_session ON session_events(session_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_file ON session_events(file_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_timestamp ON session_events(timestamp)").execute(db).await?;

    // Project metadata table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS project_meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
    "#).execute(db).await?;

    // File paths table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS file_paths (
            node_id TEXT PRIMARY KEY NOT NULL,
            path TEXT NOT NULL UNIQUE,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_paths_path ON file_paths(path)").execute(db).await?;

    // Saved queries table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS saved_queries (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL UNIQUE,
            description TEXT,
            query_pattern TEXT NOT NULL,
            parameters TEXT,
            created_at TEXT NOT NULL
        )
    "#).execute(db).await?;

    // Vector embeddings table
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS vector_embeddings (
            node_id TEXT PRIMARY KEY NOT NULL,
            vector_id TEXT NOT NULL,
            model TEXT NOT NULL,
            embedding_dim INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
    "#).execute(db).await?;

    Ok(())
}
