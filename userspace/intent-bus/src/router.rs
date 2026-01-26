//! Intent Router
//!
//! Routes intents to appropriate handlers based on:
//! - Pattern matching (Phase 1 - Current)
//! - Semantic embeddings (Phase 2 - Current)
//! - Neural predictions (Phase 3 - Future)

use crate::types::*;
use crate::semantic_router::SemanticRouter;
use std::collections::HashMap;

/// Intent router with pluggable matching strategies
pub struct IntentRouter {
    /// Registered capabilities from apps
    capabilities: HashMap<TaskId, Capability>,

    /// Simple pattern matching (Phase 1)
    pattern_matcher: PatternMatcher,

    /// Semantic embedding-based routing (Phase 2)
    semantic_router: SemanticRouter,

    // TODO: Neural predictions (Phase 3)
    // predictive_router: Option<PredictiveRouter>,
}

impl IntentRouter {
    pub fn new() -> Self {
        Self {
            capabilities: HashMap::new(),
            pattern_matcher: PatternMatcher::new(),
            semantic_router: SemanticRouter::new(),
        }
    }

    /// Register a new app's capabilities
    pub fn register(&mut self, capability: Capability) {
        println!(
            "[INTENT] Registered task {} ({}) with {} actions",
            capability.task_id,
            capability.task_name,
            capability.actions.len()
        );

        // Register with semantic router (generate embeddings)
        self.semantic_router.register_capability(&capability);

        self.capabilities.insert(capability.task_id, capability);
    }

    /// Unregister an app
    pub fn unregister(&mut self, task_id: TaskId) {
        if let Some(cap) = self.capabilities.remove(&task_id) {
            self.semantic_router.unregister_capability(task_id);
            println!("[INTENT] Unregistered task {} ({})", task_id, cap.task_name);
        }
    }

    /// Route an intent to appropriate handlers
    pub fn route(&self, intent: &Intent) -> RoutingResult {
        // Phase 1: Pattern matching
        let mut pattern_handlers = self.pattern_matcher.match_intent(intent, &self.capabilities);

        // Phase 2: Semantic matching
        let semantic_handlers = self.semantic_router.match_intent(intent, &self.capabilities);

        // Merge results (combine scores, prefer semantic if available)
        let handlers = self.merge_handler_rankings(pattern_handlers, semantic_handlers);

        // Build execution plan
        let execution_plan = self.build_execution_plan(intent, &handlers);

        // Compute overall confidence
        let confidence = if !handlers.is_empty() {
            handlers[0].confidence
        } else {
            0.0
        };

        RoutingResult {
            handlers,
            execution_plan,
            confidence,
        }
    }

    /// Merge pattern and semantic handler rankings
    fn merge_handler_rankings(&self, mut pattern: Vec<Handler>, semantic: Vec<Handler>) -> Vec<Handler> {
        if semantic.is_empty() {
            // No semantic results, use pattern matching only
            return pattern;
        }

        // Create a map for merging
        let mut merged: HashMap<TaskId, Handler> = HashMap::new();

        // Add pattern handlers
        for handler in pattern {
            merged.insert(handler.task_id, handler);
        }

        // Merge semantic handlers (boost confidence if in both)
        for handler in semantic {
            if let Some(existing) = merged.get_mut(&handler.task_id) {
                // Average the confidences (could also use weighted average)
                existing.confidence = (existing.confidence + handler.confidence) / 2.0;
                // Boost slightly for appearing in both
                existing.confidence = (existing.confidence * 1.2).min(1.0);
            } else {
                merged.insert(handler.task_id, handler);
            }
        }

        // Convert back to vec and sort
        let mut result: Vec<Handler> = merged.into_values().collect();
        result.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
        result
    }

    /// Build multi-step execution plan for complex intents
    fn build_execution_plan(&self, intent: &Intent, handlers: &[Handler]) -> Vec<IntentStep> {
        if handlers.is_empty() {
            return vec![];
        }

        // For now, simple single-step execution
        vec![IntentStep {
            handler: handlers[0].task_id,
            action: self.get_action_for_intent(intent),
            data: None,
        }]

        // TODO: Multi-step planning
        // Example: "Convert CSV to chart"
        //   1. CSV Reader task opens file
        //   2. Data Transform task converts to series
        //   3. Chart Renderer task creates visualization
    }

    fn get_action_for_intent(&self, intent: &Intent) -> String {
        match intent {
            Intent::OpenFile { .. } => "open_file".to_string(),
            Intent::SendMessage { .. } => "send_message".to_string(),
            Intent::RunCommand { .. } => "run_command".to_string(),
            Intent::Transform { .. } => "transform_data".to_string(),
            Intent::Create { .. } => "create_content".to_string(),
            Intent::Search { .. } => "search".to_string(),
        }
    }
}

/// Simple pattern-based matching (Phase 1 implementation)
struct PatternMatcher {
    // Intent keywords mapped to capabilities
    keywords: HashMap<String, Vec<String>>,
}

impl PatternMatcher {
    fn new() -> Self {
        let mut keywords = HashMap::new();

        // Build keyword index
        keywords.insert("edit".to_string(), vec!["editor".to_string()]);
        keywords.insert("text".to_string(), vec!["editor".to_string()]);
        keywords.insert("write".to_string(), vec!["editor".to_string()]);

        keywords.insert("send".to_string(), vec!["communication".to_string()]);
        keywords.insert("message".to_string(), vec!["communication".to_string()]);
        keywords.insert("email".to_string(), vec!["communication".to_string()]);

        keywords.insert("compile".to_string(), vec!["development".to_string()]);
        keywords.insert("build".to_string(), vec!["development".to_string()]);
        keywords.insert("run".to_string(), vec!["development".to_string()]);

        Self { keywords }
    }

    fn match_intent(
        &self,
        intent: &Intent,
        capabilities: &HashMap<TaskId, Capability>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        match intent {
            Intent::OpenFile { query, .. } => {
                handlers.extend(self.match_file_handlers(query, capabilities));
            }
            Intent::SendMessage { text, medium, .. } => {
                handlers.extend(self.match_communication_handlers(capabilities, medium));
            }
            Intent::RunCommand { command, .. } => {
                handlers.extend(self.match_command_handlers(command, capabilities));
            }
            Intent::Transform { from_format, to_format, .. } => {
                handlers.extend(self.match_transform_handlers(
                    from_format,
                    to_format,
                    capabilities,
                ));
            }
            Intent::Create { content_type, .. } => {
                handlers.extend(self.match_create_handlers(content_type, capabilities));
            }
            Intent::Search { query, .. } => {
                handlers.extend(self.match_search_handlers(query, capabilities));
            }
        }

        // Sort by confidence (for now, just by number of matching tags)
        handlers.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
        handlers
    }

    fn match_file_handlers(
        &self,
        query: &str,
        capabilities: &HashMap<TaskId, Capability>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        // Extract file extension if present
        let extension = if query.contains('.') {
            query.split('.').last()
        } else {
            None
        };

        for (task_id, cap) in capabilities {
            let mut confidence = 0.0;

            // Check file type match
            if let Some(ext) = extension {
                if cap.file_types.iter().any(|ft| ft.contains(ext)) {
                    confidence += 0.5;
                }
            }

            // Check action support
            if cap.actions.contains(&"open_file".to_string()) {
                confidence += 0.3;
            }

            // Check semantic tags
            if cap.tags.contains(&"editor".to_string()) {
                confidence += 0.2;
            }

            if confidence > 0.0 {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        handlers
    }

    fn match_communication_handlers(
        &self,
        capabilities: &HashMap<TaskId, Capability>,
        medium: &Option<MessageMedium>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        for (task_id, cap) in capabilities {
            let mut confidence = 0.0;

            if cap.tags.contains(&"communication".to_string()) {
                confidence += 0.5;
            }

            if cap.actions.contains(&"send_message".to_string()) {
                confidence += 0.3;
            }

            // TODO: Match medium type (chat, email, sms)
            if medium.is_some() {
                confidence += 0.2;
            }

            if confidence > 0.0 {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        handlers
    }

    fn match_command_handlers(
        &self,
        command: &str,
        capabilities: &HashMap<TaskId, Capability>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        for (task_id, cap) in capabilities {
            let mut confidence = 0.0;

            // Check if command matches any keywords
            for keyword in self.keywords.keys() {
                if command.to_lowercase().contains(keyword) {
                    if let Some(tags) = self.keywords.get(keyword) {
                        for tag in tags {
                            if cap.tags.contains(tag) {
                                confidence += 0.3;
                            }
                        }
                    }
                }
            }

            if cap.actions.contains(&"run_command".to_string()) {
                confidence += 0.4;
            }

            if confidence > 0.0 {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        handlers
    }

    fn match_transform_handlers(
        &self,
        _from: &str,
        _to: &str,
        capabilities: &HashMap<TaskId, Capability>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        for (task_id, cap) in capabilities {
            if cap.actions.contains(&"transform_data".to_string()) {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence: 0.7,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        handlers
    }

    fn match_create_handlers(
        &self,
        content_type: &str,
        capabilities: &HashMap<TaskId, Capability>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        for (task_id, cap) in capabilities {
            let mut confidence = 0.0;

            if cap.actions.contains(&"create_content".to_string()) {
                confidence += 0.5;
            }

            // TODO: Match content type with file types
            if cap.file_types.iter().any(|ft| content_type.contains(ft)) {
                confidence += 0.3;
            }

            if confidence > 0.0 {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        handlers
    }

    fn match_search_handlers(
        &self,
        _query: &str,
        capabilities: &HashMap<TaskId, Capability>,
    ) -> Vec<Handler> {
        let mut handlers = Vec::new();

        for (task_id, cap) in capabilities {
            if cap.actions.contains(&"search".to_string()) {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence: 0.8,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        handlers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_matching() {
        let mut router = IntentRouter::new();

        // Register a text editor
        router.register(Capability {
            task_id: 1,
            task_name: "TextEditor".to_string(),
            actions: vec!["open_file".to_string(), "edit_text".to_string()],
            file_types: vec![".txt".to_string(), ".md".to_string()],
            tags: vec!["editor".to_string()],
        });

        // Test file opening
        let intent = Intent::OpenFile {
            query: "notes.txt".to_string(),
            context: None,
        };

        let result = router.route(&intent);
        assert!(!result.handlers.is_empty());
        assert_eq!(result.handlers[0].task_id, 1);
    }
}
