//! Arrowhead MCP Library
//!
//! MCP (Model Context Protocol) implementation for Arrowhead.
//!
//! Provides both stdio and HTTP transport modes for AI agent integration.

#![warn(missing_docs)]

pub mod auth;
pub mod handlers;
pub mod http;
pub mod protocol;
pub mod runtime;
pub mod stdio;
pub mod tools;
pub mod transport;

// Re-export commonly used types
// pub use protocol::*;  // TODO: Uncomment when protocol types are implemented
pub use runtime::{DaemonClient, McpRuntime, RuntimeOptions};
