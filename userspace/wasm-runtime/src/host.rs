//! WASM Host Implementation
//!
//! Provides Intent Bus and OS services to WASM applications.

use crate::types::*;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info};

/// Host state shared across WASM instances
#[derive(Clone)]
pub struct HostState {
    /// Registered applications
    apps: Arc<RwLock<HashMap<String, AppMetadata>>>,

    /// Intent Bus router (in real implementation, this would connect to the actual Intent Bus)
    router: Arc<IntentRouter>,
}

impl HostState {
    pub fn new() -> Self {
        Self {
            apps: Arc::new(RwLock::new(HashMap::new())),
            router: Arc::new(IntentRouter::new()),
        }
    }

    /// Register an application
    pub fn register_app(&self, metadata: AppMetadata) -> Result<()> {
        let mut apps = self.apps.write().unwrap();
        info!("Registering app: {} ({})", metadata.name, metadata.app_id);

        for cap in &metadata.capabilities {
            debug!("  - Capability: {} ({})", cap.action, cap.description);
        }

        apps.insert(metadata.app_id.clone(), metadata);
        Ok(())
    }

    /// Unregister an application
    pub fn unregister_app(&self, app_id: &str) -> Result<()> {
        let mut apps = self.apps.write().unwrap();
        if apps.remove(app_id).is_some() {
            info!("Unregistered app: {}", app_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("App not found: {}", app_id))
        }
    }

    /// Dispatch an intent
    pub fn dispatch_intent(&self, intent: &Intent) -> Result<RoutingResult> {
        debug!("Dispatching intent: {} from {}", intent.action, intent.metadata.source_app);

        // Route the intent
        let result = self.router.route(intent, &self.apps.read().unwrap())?;

        info!("Routed to {} apps with {:.0}% confidence",
            result.matched_apps.len(), result.confidence * 100.0);

        Ok(result)
    }

    /// Send intent to specific app
    pub fn send_to(&self, app_id: &str, intent: &Intent) -> Result<bool> {
        let apps = self.apps.read().unwrap();
        if !apps.contains_key(app_id) {
            return Err(anyhow::anyhow!("Target app not found: {}", app_id));
        }

        info!("Sending intent '{}' to app '{}'", intent.action, app_id);
        // In real implementation, this would deliver to the WASM instance
        Ok(true)
    }

    /// Query capabilities
    pub fn query_capabilities(&self, action: &str) -> Vec<String> {
        let apps = self.apps.read().unwrap();
        apps.iter()
            .filter(|(_, metadata)| {
                metadata.capabilities.iter().any(|cap| cap.action == action)
            })
            .map(|(app_id, _)| app_id.clone())
            .collect()
    }
}

/// Simple intent router (simplified version of the actual Intent Bus)
struct IntentRouter {
    // In real implementation, this would use the semantic router
}

impl IntentRouter {
    fn new() -> Self {
        Self {}
    }

    fn route(&self, intent: &Intent, apps: &HashMap<String, AppMetadata>) -> Result<RoutingResult> {
        let start = std::time::Instant::now();

        // Find apps that can handle this action
        let mut matched_apps = Vec::new();
        let mut max_confidence = 0.0f32;

        for (app_id, metadata) in apps {
            for capability in &metadata.capabilities {
                // Simple pattern matching
                if capability.action == intent.action {
                    matched_apps.push(app_id.clone());
                    max_confidence = 0.9; // High confidence for exact match
                    break;
                }

                // Check if action matches any patterns
                for pattern in &capability.patterns {
                    if pattern_matches(pattern, &intent.action) {
                        matched_apps.push(app_id.clone());
                        max_confidence = max_confidence.max(0.7); // Medium confidence for pattern match
                        break;
                    }
                }
            }
        }

        // If we have a specific target, filter to just that app
        if let Some(ref target) = intent.metadata.target_app {
            matched_apps.retain(|app_id| app_id == target);
        }

        let latency_ms = start.elapsed().as_secs_f32() * 1000.0;

        Ok(RoutingResult {
            matched_apps,
            confidence: max_confidence,
            latency_ms,
        })
    }
}

/// Simple pattern matching (supports wildcards)
fn pattern_matches(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len() - 1];
        return action.starts_with(prefix);
    }

    pattern == action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_state_creation() {
        let state = HostState::new();
        assert!(state.query_capabilities("test").is_empty());
    }

    #[test]
    fn test_app_registration() {
        let state = HostState::new();

        let metadata = AppMetadata {
            app_id: "test-app".to_string(),
            name: "Test App".to_string(),
            version: "0.1.0".to_string(),
            capabilities: vec![
                Capability {
                    action: "edit".to_string(),
                    description: "Edit text".to_string(),
                    patterns: vec!["edit*".to_string()],
                    examples: vec!["edit file".to_string()],
                }
            ],
        };

        state.register_app(metadata).unwrap();

        let apps = state.query_capabilities("edit");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0], "test-app");
    }

    #[test]
    fn test_intent_dispatch() {
        let state = HostState::new();

        // Register app
        let metadata = AppMetadata {
            app_id: "editor".to_string(),
            name: "Text Editor".to_string(),
            version: "1.0.0".to_string(),
            capabilities: vec![
                Capability {
                    action: "edit".to_string(),
                    description: "Edit files".to_string(),
                    patterns: vec!["edit*".to_string()],
                    examples: vec![],
                }
            ],
        };
        state.register_app(metadata).unwrap();

        // Create intent
        let intent = Intent::text("edit", "shell", "Edit my notes");

        // Dispatch
        let result = state.dispatch_intent(&intent).unwrap();
        assert_eq!(result.matched_apps.len(), 1);
        assert_eq!(result.matched_apps[0], "editor");
        assert!(result.confidence > 0.5);
    }

    #[test]
    fn test_pattern_matching() {
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("edit*", "edit"));
        assert!(pattern_matches("edit*", "edit-file"));
        assert!(!pattern_matches("edit*", "save"));
        assert!(pattern_matches("save", "save"));
        assert!(!pattern_matches("save", "save-as"));
    }
}
