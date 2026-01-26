//! Intent Bus Service
//!
//! This service acts as a central hub for routing intents between applications.
//! It replaces traditional copy-paste with semantic understanding.
//!
//! Architecture:
//! - Runs as a userspace daemon
//! - Apps register their capabilities via IPC
//! - Receives intents and routes to appropriate handlers
//! - Eventually will use neural networks for smart routing

mod router;
mod types;
mod semantic_router;

use router::IntentRouter;
use types::*;

use tokio::sync::mpsc;
use std::collections::HashMap;

/// Main Intent Bus service
pub struct IntentBusService {
    /// Core router
    router: IntentRouter,

    /// IPC channels to connected apps (simulated for now)
    /// In production, these will be kernel IPC channels
    app_channels: HashMap<TaskId, mpsc::UnboundedSender<IntentMessage>>,
}

impl IntentBusService {
    pub fn new() -> Self {
        println!("[INTENT-BUS] Starting Intent Bus service...");
        Self {
            router: IntentRouter::new(),
            app_channels: HashMap::new(),
        }
    }

    /// Main service loop
    pub async fn run(mut self) {
        // Create message receiver (simulates kernel IPC)
        let (tx, mut rx) = mpsc::unbounded_channel::<(TaskId, IntentMessage)>();

        println!("[INTENT-BUS] Service ready, waiting for messages...");

        // Main message loop
        while let Some((sender_task_id, message)) = rx.recv().await {
            match message {
                IntentMessage::Register(capability) => {
                    self.handle_registration(capability);
                }

                IntentMessage::Unregister(task_id) => {
                    self.handle_unregistration(task_id);
                }

                IntentMessage::SubmitIntent(intent) => {
                    self.handle_intent(sender_task_id, intent).await;
                }

                IntentMessage::ExecutionResult { success, output, error } => {
                    self.handle_execution_result(sender_task_id, success, output, error);
                }

                _ => {
                    println!("[INTENT-BUS] Unhandled message type from task {}", sender_task_id);
                }
            }
        }
    }

    fn handle_registration(&mut self, capability: Capability) {
        println!(
            "[INTENT-BUS] Registering app: {} (task_id: {})",
            capability.task_name, capability.task_id
        );
        println!("  Actions: {:?}", capability.actions);
        println!("  File types: {:?}", capability.file_types);
        println!("  Tags: {:?}", capability.tags);

        self.router.register(capability);
    }

    fn handle_unregistration(&mut self, task_id: TaskId) {
        println!("[INTENT-BUS] Unregistering task {}", task_id);
        self.router.unregister(task_id);
        self.app_channels.remove(&task_id);
    }

    async fn handle_intent(&mut self, sender_task_id: TaskId, intent: Intent) {
        println!("\n[INTENT-BUS] ========== NEW INTENT ==========");
        println!("  From task: {}", sender_task_id);
        println!("  Intent: {:?}", intent);

        // Route the intent
        let result = self.router.route(&intent);

        println!("\n[INTENT-BUS] Routing result:");
        println!("  Confidence: {:.2}", result.confidence);
        println!("  Handlers found: {}", result.handlers.len());

        for (i, handler) in result.handlers.iter().enumerate() {
            println!(
                "    {}. {} (task {}) - confidence: {:.2}",
                i + 1,
                handler.task_name,
                handler.task_id,
                handler.confidence
            );
        }

        // Execute the intent (send to best handler)
        if let Some(best_handler) = result.handlers.first() {
            println!("\n[INTENT-BUS] Executing via: {}", best_handler.task_name);

            // In production, this would use kernel IPC:
            // kernel::ipc_send(best_handler.task_id, ExecuteStep(...))

            // For demo, just log
            if let Some(step) = result.execution_plan.first() {
                println!("  Action: {}", step.action);
            }
        } else {
            println!("[INTENT-BUS] ❌ No handler found for intent!");
        }

        println!("[INTENT-BUS] =============================\n");
    }

    fn handle_execution_result(
        &self,
        task_id: TaskId,
        success: bool,
        output: Option<Vec<u8>>,
        error: Option<String>,
    ) {
        if success {
            println!(
                "[INTENT-BUS] ✅ Task {} completed successfully",
                task_id
            );
            if let Some(data) = output {
                println!("  Output: {} bytes", data.len());
            }
        } else {
            println!("[INTENT-BUS] ❌ Task {} failed", task_id);
            if let Some(err) = error {
                println!("  Error: {}", err);
            }
        }
    }

}

/// Register demo apps for testing
fn register_demo_apps(service: &mut IntentBusService) {
    // Text Editor
    service.router.register(Capability {
            task_id: 100,
            task_name: "TextEditor".to_string(),
            actions: vec![
                "open_file".to_string(),
                "edit_text".to_string(),
                "create_content".to_string(),
            ],
            file_types: vec![".txt".to_string(), ".md".to_string(), ".rs".to_string()],
            tags: vec!["editor".to_string(), "productivity".to_string()],
        });

    // Slack-like messenger
    service.router.register(Capability {
        task_id: 101,
        task_name: "Messenger".to_string(),
        actions: vec!["send_message".to_string()],
        file_types: vec![],
        tags: vec!["communication".to_string(), "chat".to_string()],
    });

    // Compiler/Builder
    service.router.register(Capability {
        task_id: 102,
        task_name: "Builder".to_string(),
        actions: vec!["run_command".to_string(), "transform_data".to_string()],
        file_types: vec![".rs".to_string(), ".c".to_string()],
        tags: vec!["development".to_string(), "compiler".to_string()],
    });

    // File searcher
    service.router.register(Capability {
        task_id: 103,
        task_name: "FileSearcher".to_string(),
        actions: vec!["search".to_string(), "open_file".to_string()],
        file_types: vec![],
        tags: vec!["search".to_string(), "filesystem".to_string()],
    });
}

#[tokio::main]
async fn main() {
    println!("\n");
    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║                                                           ║");
    println!("║          🚀 Folkering OS - Intent Bus Service 🚀          ║");
    println!("║                                                           ║");
    println!("║  Next-Gen Application Communication                       ║");
    println!("║  No more copy-paste - Just express your intent!          ║");
    println!("║                                                           ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!("\n");

    // Create service and register demo apps
    let mut service = IntentBusService::new();

    // Register demo apps
    println!("\n[INTENT-BUS] Registering demo applications...\n");
    register_demo_apps(&mut service);
    println!("[INTENT-BUS] Demo apps registered\n");

    // Run demo scenarios
    run_demo_scenarios(&mut service).await;

    // In production, service would run forever handling IPC
    // service.run().await;
}

/// Run demo scenarios to show Intent Bus capabilities
async fn run_demo_scenarios(service: &mut IntentBusService) {

    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║                  DEMO SCENARIOS                           ║");
    println!("╚═══════════════════════════════════════════════════════════╝\n");

    // Scenario 1: Open a file
    println!("📝 Scenario 1: User wants to open a file");
    println!("   Traditional: User finds app, clicks File→Open, navigates...");
    println!("   Intent Bus:  User just says 'open notes.txt'\n");

    service.handle_intent(
        999, // Simulated sender
        Intent::OpenFile {
            query: "notes.txt".to_string(),
            context: None,
        },
    ).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Scenario 2: Send a message
    println!("💬 Scenario 2: User wants to notify team");
    println!("   Traditional: Open Slack, find channel, type...");
    println!("   Intent Bus:  'Send meeting reminder to team'\n");

    service.handle_intent(
        999,
        Intent::SendMessage {
            text: "Meeting at 3pm today!".to_string(),
            recipients: vec!["team".to_string()],
            medium: Some(MessageMedium::Chat),
        },
    ).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Scenario 3: Build project
    println!("🔨 Scenario 3: Developer wants to compile");
    println!("   Traditional: cd to dir, cargo build, wait...");
    println!("   Intent Bus:  'Build the project'\n");

    service.handle_intent(
        999,
        Intent::RunCommand {
            command: "build".to_string(),
            args: vec!["--release".to_string()],
            context: None,
        },
    ).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Scenario 4: Find files
    println!("🔍 Scenario 4: User searches for work documents");
    println!("   Traditional: Open explorer, search, filter by date...");
    println!("   Intent Bus:  'Find work documents from last week'\n");

    service.handle_intent(
        999,
        Intent::Search {
            query: "work documents".to_string(),
            filters: vec![SearchFilter::Tag("work".to_string())],
        },
    ).await;

    println!("\n╔═══════════════════════════════════════════════════════════╗");
    println!("║                                                           ║");
    println!("║  ✅ Demo complete!                                        ║");
    println!("║                                                           ║");
    println!("║  Next steps:                                             ║");
    println!("║    1. Integrate with kernel IPC (when syscalls work)     ║");
    println!("║    2. Add Vector FS service for semantic file search     ║");
    println!("║    3. Train neural router on user behavior               ║");
    println!("║                                                           ║");
    println!("╚═══════════════════════════════════════════════════════════╝\n");
}
