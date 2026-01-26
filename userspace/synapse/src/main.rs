//! Synapse - Graph Filesystem Service
//!
//! Replaces traditional hierarchical filesystem with a knowledge graph.
//! Files exist in a web of context, not just locations.

mod models;
mod observer;
mod query;
mod graph;

use anyhow::Result;
use sqlx::SqlitePool;
use std::path::PathBuf;
use tracing::{info, error};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    print_banner();

    // Parse command line args
    let args: Vec<String> = std::env::args().collect();
    let db_path = args.get(1).map(String::as_str).unwrap_or("synapse.db");
    let watch_path = args.get(2).map(String::as_str).unwrap_or(".");

    info!("Starting Synapse Graph Filesystem");
    info!("Database: {}", db_path);
    info!("Watch path: {}", watch_path);

    // Initialize database
    let db = init_database(db_path).await?;
    info!("Database initialized");

    // Run migrations
    run_migrations(&db).await?;
    info!("Migrations complete");

    // Print stats
    let graph_db = graph::GraphDB::new(db.clone());
    let stats = graph_db.get_stats().await?;
    info!(
        "Graph stats: {} nodes, {} edges, avg weight: {:.2}",
        stats.node_count, stats.edge_count, stats.avg_edge_weight
    );

    // Start observer daemon
    info!("Starting filesystem observer on: {}", watch_path);
    let observer = observer::Observer::new();

    match observer.start(PathBuf::from(watch_path)).await {
        Ok(_) => info!("Observer stopped"),
        Err(e) => error!("Observer error: {}", e),
    }

    Ok(())
}

async fn init_database(path: &str) -> Result<SqlitePool> {
    let url = format!("sqlite:{}?mode=rwc", path);
    let pool = SqlitePool::connect(&url).await?;
    Ok(pool)
}

async fn run_migrations(db: &SqlitePool) -> Result<()> {
    // Read and execute migration SQL
    let migration_sql = include_str!("../migrations/001_initial_schema.sql");

    // Split by semicolon and execute each statement
    for statement in migration_sql.split(';') {
        let statement = statement.trim();
        if !statement.is_empty() && !statement.starts_with("--") {
            sqlx::query(statement).execute(db).await?;
        }
    }

    Ok(())
}

fn print_banner() {
    println!("\n");
    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║                                                           ║");
    println!("║          🧠 Synapse - Graph Filesystem Service 🧠         ║");
    println!("║                                                           ║");
    println!("║  Knowledge Graph Filesystem                               ║");
    println!("║  Files exist in a web of context, not just locations     ║");
    println!("║                                                           ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!("\n");
}
