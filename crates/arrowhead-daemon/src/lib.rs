//! Arrowhead daemon runtime library.
//!
//! This crate hosts the background watcher service responsible for keeping the
//! Arrowhead index warm. The current phase wires up filesystem monitoring,
//! event queue management, and a JSON control interface supporting snapshot,
//! streaming status queries, and graceful shutdown commands.

mod control;
mod logging;
mod runtime;
mod watcher;

pub use crate::control::{
    ControlRequest, ControlResponse, StatusStream, send_control_request, status_stream,
};
pub use crate::runtime::{DaemonConfig, DaemonHandle, DaemonRuntimeBuilder, cli_main};
pub use crate::watcher::WatcherStrategy;
