//! WASM Runtime Demo
//!
//! Demonstrates the WASM/WASI runtime for Folkering OS applications.

use wasm_runtime::*;
use tracing::{info, Level};
use tracing_subscriber;

fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    println!("==============================================");
    println!("  Folkering OS - WASM Runtime Demo");
    println!("  Phase 3.5: Application Runtime");
    println!("==============================================\n");

    // Create runtime
    info!("Creating WASM runtime...");
    let runtime = WasmRuntime::new()?;

    println!("📦 WASM Runtime initialized");
    println!("  - Engine: wasmtime");
    println!("  - Component Model: enabled");
    println!("  - WASI: Preview 2");
    println!("  - Intent Bus: integrated\n");

    // Demo: Register mock applications
    println!("🔄 Scenario 1: Registering Applications\n");

    // Text Editor
    let editor_metadata = AppMetadata {
        app_id: "text-editor".to_string(),
        name: "Text Editor".to_string(),
        version: "1.0.0".to_string(),
        capabilities: vec![
            Capability {
                action: "edit".to_string(),
                description: "Edit text files".to_string(),
                patterns: vec!["edit*".to_string(), "open*".to_string()],
                examples: vec![
                    "edit my notes".to_string(),
                    "open document.txt".to_string(),
                ],
            },
            Capability {
                action: "view".to_string(),
                description: "View text files".to_string(),
                patterns: vec!["view*".to_string(), "show*".to_string()],
                examples: vec!["view readme".to_string()],
            },
        ],
    };

    // Email Client
    let email_metadata = AppMetadata {
        app_id: "email-client".to_string(),
        name: "Email Client".to_string(),
        version: "2.1.3".to_string(),
        capabilities: vec![
            Capability {
                action: "send-email".to_string(),
                description: "Send emails".to_string(),
                patterns: vec!["send*".to_string(), "email*".to_string()],
                examples: vec![
                    "send email to alice@example.com".to_string(),
                    "email my report".to_string(),
                ],
            },
        ],
    };

    // Messenger
    let messenger_metadata = AppMetadata {
        app_id: "messenger".to_string(),
        name: "Messenger".to_string(),
        version: "3.0.1".to_string(),
        capabilities: vec![
            Capability {
                action: "send-message".to_string(),
                description: "Send instant messages".to_string(),
                patterns: vec!["send*".to_string(), "message*".to_string(), "chat*".to_string()],
                examples: vec![
                    "send message to team".to_string(),
                    "chat with Bob".to_string(),
                ],
            },
        ],
    };

    // Register apps with host (in real implementation, these would be WASM modules)
    let host_state = runtime.host_state.clone();
    host_state.register_app(editor_metadata)?;
    host_state.register_app(email_metadata)?;
    host_state.register_app(messenger_metadata)?;

    println!("  ✅ Registered 3 applications:");
    println!("     - Text Editor (edit, view)");
    println!("     - Email Client (send-email)");
    println!("     - Messenger (send-message)\n");

    // Demo: Dispatch intents
    println!("🔄 Scenario 2: Intent Routing\n");

    let test_intents = vec![
        ("edit", "Edit my notes", vec!["text-editor"]),
        ("send-email", "Send report to team", vec!["email-client"]),
        ("send-message", "Chat with Alice", vec!["messenger"]),
        ("send", "Send document", vec!["email-client", "messenger"]), // Ambiguous
    ];

    for (action, description, _expected_apps) in test_intents {
        let intent = Intent::text(action, "shell", description);

        println!("  📨 Intent: \"{}\" (action: {})", description, action);

        let result = runtime.dispatch_intent(intent)?;

        println!("     Matched: {} app(s)", result.matched_apps.len());
        for app_id in &result.matched_apps {
            println!("       - {}", app_id);
        }
        println!("     Confidence: {:.0}%", result.confidence * 100.0);
        println!("     Latency: {:.2}ms\n", result.latency_ms);

        // Verify expected apps (for demo purposes)
        assert!(result.matched_apps.len() > 0, "No apps matched");
    }

    println!("==============================================\n");

    // Demo: Query capabilities
    println!("🔄 Scenario 3: Capability Discovery\n");

    let queries = vec![
        "edit",
        "send-email",
        "send-message",
        "view",
    ];

    for action in queries {
        let apps = host_state.query_capabilities(action);
        println!("  🔍 Apps with '{}' capability:", action);
        for app_id in apps {
            println!("       - {}", app_id);
        }
        println!();
    }

    println!("==============================================\n");

    // Statistics
    let stats = runtime.stats();
    println!("📊 Runtime Statistics:");
    println!("  - Loaded modules: {} (mock - no actual WASM files)", stats.loaded_modules);
    println!("  - Total capabilities: {}", stats.total_capabilities);

    println!("\n==============================================");
    println!("  ✅ Demo Complete");
    println!("  Next: Implement actual WASM module loading");
    println!("==============================================\n");

    Ok(())
}
