//! Arrowhead deamon runtime library.
//!
//! This crate hosts the background watcher service responsible for keeping the
//! Arrowhead index warm. The current phase wires up filesystem monitoring,
//! event queue management, and a minimal JSON control interface supporting
//! `status` and `shutdown` commands.

mod control;
mod logging;
mod runtime;
mod watcher;

pub use crate::control::{ControlRequest, ControlResponse, send_control_request};
pub use crate::runtime::{DeamonConfig, DeamonHandle, DeamonRuntimeBuilder, cli_main};
pub use crate::watcher::WatcherStrategy;
