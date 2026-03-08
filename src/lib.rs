//! Library entry point for the Symphony service.
//!
//! The crate is intentionally split by runtime responsibility so the scheduler,
//! agent protocol client, workspace lifecycle, and HTTP surface can be tested
//! independently.

pub mod agent;
pub mod error;
pub mod http;
pub mod orchestrator;
pub mod tracker;
pub mod types;
pub mod workflow;
pub mod workspace;
