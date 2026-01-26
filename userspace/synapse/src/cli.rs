//! Synapse CLI - Query interface for the graph filesystem

mod models;
mod query;
mod graph;

use anyhow::Result;
use sqlx::SqlitePool;
use std::io::{self, Write};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (quieter for CLI)
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    // Connect to database
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "synapse.db".to_string());

    let url = format!("sqlite:{}?mode=ro", db_path);
    let db = SqlitePool::connect(&url).await?;

    let query_engine = query::QueryEngine::new(db.clone());
    let graph_db = graph::GraphDB::new(db);

    print_banner();

    // Print stats
    let stats = graph_db.get_stats().await?;
    println!(
        "Graph: {} nodes, {} edges (avg weight: {:.2})\n",
        stats.node_count, stats.edge_count, stats.avg_edge_weight
    );

    // Interactive REPL
    println!("Enter queries (or 'help' for commands, 'quit' to exit):\n");

    loop {
        print!("synapse> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        match input {
            "quit" | "exit" => break,
            "help" => print_help(),
            _ => {
                if let Err(e) = handle_query(&query_engine, &graph_db, input).await {
                    println!("Error: {}", e);
                }
            }
        }

        println!();
    }

    println!("Goodbye!");
    Ok(())
}

async fn handle_query(
    query_engine: &query::QueryEngine,
    graph_db: &graph::GraphDB,
    input: &str,
) -> Result<()> {
    let parts: Vec<&str> = input.split_whitespace().collect();

    if parts.is_empty() {
        return Ok(());
    }

    match parts[0] {
        "tag" => {
            if parts.len() < 2 {
                println!("Usage: tag <tag_name>");
                return Ok(());
            }
            let tag_name = parts[1];
            let files = query_engine.find_by_tag(tag_name).await?;
            println!("Files tagged with '{}':", tag_name);
            for file in files {
                let name = file.get_property("name");
                println!("  - {} ({})", name.unwrap_or_default(), file.id);
            }
        }

        "edited" => {
            if parts.len() < 2 {
                println!("Usage: edited <person_name>");
                return Ok(());
            }
            let person_name = parts[1];
            let results = query_engine.find_edited_by(person_name).await?;
            println!("Files edited by '{}':", person_name);
            for file in results {
                let name = file.get_property("name");
                println!("  - {} [{}]", name.unwrap_or_default(), file.id);
            }
        }

        "cooccur" => {
            if parts.len() < 2 {
                println!("Usage: cooccur <file_id> [min_weight]");
                return Ok(());
            }
            let file_id = parts[1];
            let min_weight = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.5);

            let results = query_engine.find_co_occurring(file_id, min_weight).await?;
            println!("Files co-occurring with '{}' (min weight {}):", file_id, min_weight);
            for file in results {
                let name = file.get_property("name");
                println!("  - {} [{}]", name.unwrap_or_default(), file.id);
            }
        }

        "similar" => {
            if parts.len() < 2 {
                println!("Usage: similar <file_id> [min_similarity]");
                return Ok(());
            }
            let file_id = parts[1];
            let min_similarity = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.7);

            let results = query_engine.find_similar(file_id, min_similarity).await?;
            println!(
                "Files similar to '{}' (min similarity {}):",
                file_id, min_similarity
            );
            for file in results {
                let name = file.get_property("name");
                println!("  - {} [{}]", name.unwrap_or_default(), file.id);
            }
        }

        "project" => {
            if parts.len() < 2 {
                println!("Usage: project <project_name>");
                return Ok(());
            }
            let project_name = parts[1];
            let files = query_engine.find_in_project(project_name).await?;
            println!("Files in project '{}':", project_name);
            for file in files {
                let name = file.get_property("name");
                println!("  - {} [{}]", name.unwrap_or_default(), file.id);
            }
        }

        "today" => {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let files = query_engine
                .find_by_timeframe(&format!("{}T00:00:00Z", today), &format!("{}T23:59:59Z", today))
                .await?;
            println!("Files accessed today:");
            for file in files {
                let name = file.get_property("name");
                println!("  - {} [{}]", name.unwrap_or_default(), file.id);
            }
        }

        "search" => {
            if parts.len() < 2 {
                println!("Usage: search <query>");
                return Ok(());
            }
            let query = parts[1..].join(" ");
            let files = query_engine.search_files(&query).await?;
            println!("Search results for '{}':", query);
            for file in files.iter().take(10) {
                let name = file.get_property("name");
                println!("  - {} [{}]", name.unwrap_or_default(), file.id);
            }
            if files.len() > 10 {
                println!("  ... and {} more", files.len() - 10);
            }
        }

        "neighborhood" => {
            if parts.len() < 2 {
                println!("Usage: neighborhood <node_id> [hops]");
                return Ok(());
            }
            let node_id = parts[1];
            let hops = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(2);

            let (nodes, edges) = query_engine.get_neighborhood(node_id, hops).await?;
            println!("Neighborhood of '{}' ({} hops):", node_id, hops);
            println!("  Nodes: {}", nodes.len());
            println!("  Edges: {}", edges.len());

            for node in nodes.iter().take(10) {
                let name = node.get_property("name").unwrap_or_default();
                println!("    - {} ({}) [{}]", name, node.r#type.as_str(), node.id);
            }
            if nodes.len() > 10 {
                println!("    ... and {} more nodes", nodes.len() - 10);
            }
        }

        "stats" => {
            let stats = graph_db.get_stats().await?;
            println!("Graph Statistics:");
            println!("  Nodes: {}", stats.node_count);
            println!("  Edges: {}", stats.edge_count);
            println!("  Avg edge weight: {:.2}", stats.avg_edge_weight);

            let strong_edges = graph_db.get_strongest_edges(10).await?;
            println!("\nStrongest edges:");
            for edge in strong_edges {
                println!(
                    "  {} -> {} ({}) [weight: {:.2}]",
                    edge.source_id,
                    edge.target_id,
                    edge.r#type.as_str(),
                    edge.weight
                );
            }
        }

        _ => {
            println!("Unknown command: {}", parts[0]);
            println!("Type 'help' for available commands.");
        }
    }

    Ok(())
}

fn print_banner() {
    println!("\n");
    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║                                                           ║");
    println!("║          🧠 Synapse CLI - Graph Filesystem Query 🧠       ║");
    println!("║                                                           ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!("\n");
}

fn print_help() {
    println!("Available commands:");
    println!();
    println!("  tag <tag_name>              - Find files with tag");
    println!("  edited <person_name>        - Find files edited by person");
    println!("  cooccur <file_id> [weight]  - Find files used together");
    println!("  similar <file_id> [score]   - Find semantically similar files");
    println!("  project <project_name>      - Find files in project");
    println!("  today                       - Files accessed today");
    println!("  search <query>              - Full-text search");
    println!("  neighborhood <id> [hops]    - Get graph neighborhood");
    println!("  stats                       - Show graph statistics");
    println!();
    println!("  help                        - Show this help");
    println!("  quit                        - Exit");
}
