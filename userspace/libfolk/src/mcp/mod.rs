//! Model Context Protocol (MCP) — Bare-Metal Implementation
//!
//! Defines the wire protocol between the Rust OS (MCP Server) and
//! the Python LLM proxy (MCP Host) over the COM2 serial bridge.
//!
//! Serialization: Postcard (no_std, zero-alloc binary format)
//! Framing: COBS (Consistent Overhead Byte Stuffing) + CRC-16
//! Transport: COM2 serial (UART 0x2F8, TCP socket in QEMU)

pub mod types;
pub mod frame;
pub mod client;
