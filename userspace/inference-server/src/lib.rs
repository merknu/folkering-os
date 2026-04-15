//! Inference Server library — Folkering OS Task 6.
//!
//! Loads a GGUF model from VirtIO disk via mmap, runs transformer inference,
//! and serves requests via IPC. The bin/main.rs holds only the IPC dispatch
//! loop; everything else lives here as testable modules.
//!
//! # Module overview
//!
//! - `consts`        — IPC opcodes, memory layout, sampling defaults, sectors
//! - `allocator`     — `BumpAllocator` (global allocator for GGUF parsing)
//! - `debug`         — tensor dump to disk mailbox + health telemetry
//! - `config`        — `SamplingConfig` + control sector reader
//! - `chat`          — ChatML template wrap + tokenizer helpers
//! - `stream`        — `TokenRing` shmem buffer for async streaming
//! - `weights`       — GGUF → ModelWeights mapping (`WeightsData`, `LayerData`, …)
//! - `gguf_loader`   — disk read + mmap + DMA burst loader
//! - `inference`     — `InferenceEngine` state, `build_weights_for_forward`,
//!                     pre-allocated `logits_buf` (Phase B3)
//! - `sampling`      — `sample_with_penalties` (uses pre-allocated buffer)
//! - `handlers`      — sync + async IPC inference handlers + shmem reply

#![no_std]

extern crate alloc;

pub mod consts;
pub mod allocator;
pub mod debug;
pub mod config;
pub mod chat;
pub mod stream;
pub mod weights;
pub mod gguf_loader;
pub mod inference;
pub mod sampling;
pub mod handlers;
