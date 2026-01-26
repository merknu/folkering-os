//! WASM Runtime
//!
//! Loads and executes WASM modules with Intent Bus integration.

use crate::host::HostState;
use crate::types::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tracing::{debug, info};
use wasmtime::*;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

/// WASM Runtime for Folkering OS
pub struct WasmRuntime {
    /// Wasmtime engine
    engine: Engine,

    /// Host state (Intent Bus, etc.)
    pub host_state: HostState,

    /// Loaded modules
    modules: Arc<RwLock<HashMap<String, LoadedModule>>>,
}

/// A loaded WASM module instance
struct LoadedModule {
    /// Module metadata
    metadata: AppMetadata,

    /// Wasmtime store
    store: Store<WasmState>,

    /// Module instance
    instance: Instance,
}

/// Per-instance state
struct WasmState {
    /// WASI context
    wasi: WasiCtx,

    /// Host state reference
    host: HostState,

    /// App ID
    app_id: String,
}

impl WasmRuntime {
    /// Create a new WASM runtime
    pub fn new() -> Result<Self> {
        info!("Initializing WASM runtime");

        // Configure Wasmtime engine
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        config.consume_fuel(true); // Enable resource limits

        let engine = Engine::new(&config)?;
        let host_state = HostState::new();

        Ok(Self {
            engine,
            host_state,
            modules: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Load a WASM module
    pub fn load_module(&mut self, path: impl AsRef<Path>, metadata: AppMetadata) -> Result<()> {
        let path = path.as_ref();
        info!("Loading WASM module: {} from {}", metadata.app_id, path.display());

        // Read WASM file
        let wasm_bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read WASM file: {}", path.display()))?;

        // Compile module
        let module = Module::new(&self.engine, &wasm_bytes)
            .context("Failed to compile WASM module")?;

        debug!("Module compiled successfully");

        // Create WASI context
        let wasi = WasiCtxBuilder::new()
            .inherit_stdio()
            .inherit_env()
            .build();

        // Create store with state
        let mut store = Store::new(
            &self.engine,
            WasmState {
                wasi,
                host: self.host_state.clone(),
                app_id: metadata.app_id.clone(),
            },
        );

        // Set fuel limit (resource control)
        store.set_fuel(1_000_000)?;

        // Create linker
        let mut linker = Linker::new(&self.engine);

        // Add Intent Bus host functions
        self.add_host_functions(&mut linker)?;

        // Instantiate module
        let instance = linker.instantiate(&mut store, &module)
            .context("Failed to instantiate module")?;

        debug!("Module instantiated");

        // Register app with host state
        self.host_state.register_app(metadata.clone())?;

        // Store loaded module
        let mut modules = self.modules.write().unwrap();
        modules.insert(
            metadata.app_id.clone(),
            LoadedModule {
                metadata,
                store,
                instance,
            },
        );

        info!("Module loaded successfully");
        Ok(())
    }

    /// Add Intent Bus host functions to linker
    fn add_host_functions(&self, linker: &mut Linker<WasmState>) -> Result<()> {
        // intent-dispatcher::dispatch
        linker.func_wrap(
            "intent-dispatcher",
            "dispatch",
            |_caller: Caller<'_, WasmState>, action_ptr: i32, action_len: i32| -> i32 {
                debug!("WASM called dispatch({}, {})", action_ptr, action_len);
                // In real implementation, read from WASM memory and dispatch
                0 // Success
            },
        )?;

        // capability-registry::register
        linker.func_wrap(
            "capability-registry",
            "register",
            |_caller: Caller<'_, WasmState>, app_id_ptr: i32, app_id_len: i32| -> i32 {
                debug!("WASM called register({}, {})", app_id_ptr, app_id_len);
                0 // Success
            },
        )?;

        Ok(())
    }

    /// Dispatch an intent to loaded modules
    pub fn dispatch_intent(&self, intent: Intent) -> Result<RoutingResult> {
        self.host_state.dispatch_intent(&intent)
    }

    /// Get statistics
    pub fn stats(&self) -> RuntimeStats {
        let modules = self.modules.read().unwrap();
        RuntimeStats {
            loaded_modules: modules.len(),
            total_capabilities: modules.values()
                .map(|m| m.metadata.capabilities.len())
                .sum(),
        }
    }

    /// Unload a module
    pub fn unload_module(&self, app_id: &str) -> Result<()> {
        let mut modules = self.modules.write().unwrap();
        if modules.remove(app_id).is_some() {
            self.host_state.unregister_app(app_id)?;
            info!("Unloaded module: {}", app_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Module not found: {}", app_id))
        }
    }
}

/// Runtime statistics
#[derive(Debug, Clone)]
pub struct RuntimeStats {
    pub loaded_modules: usize,
    pub total_capabilities: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let runtime = WasmRuntime::new();
        assert!(runtime.is_ok());
    }

    #[test]
    fn test_runtime_stats() {
        let runtime = WasmRuntime::new().unwrap();
        let stats = runtime.stats();
        assert_eq!(stats.loaded_modules, 0);
        assert_eq!(stats.total_capabilities, 0);
    }
}
