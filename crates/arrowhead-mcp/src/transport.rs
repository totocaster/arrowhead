//! Shared transport abstractions for MCP servers.
//!
//! Exposes the [`MessageHandler`] trait used by both stdio and HTTP transports.

use async_trait::async_trait;

use crate::protocol::{Notification, ProtocolError, Request};

/// Trait implemented by request dispatchers consumed by MCP transports.
#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    /// Handle an RPC request and return the JSON result payload.
    async fn handle_request(
        &self,
        request: Request,
    ) -> std::result::Result<serde_json::Value, ProtocolError>;

    /// Handle a JSON-RPC notification (no response emitted on success).
    async fn handle_notification(
        &self,
        notification: Notification,
    ) -> std::result::Result<(), ProtocolError> {
        let _ = notification;
        Ok(())
    }
}
