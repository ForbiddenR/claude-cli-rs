//! Minimal Model Context Protocol (MCP) client implementation.
//!
//! This is intentionally small and headless-focused: it supports connecting to
//! MCP servers via stdio, SSE, and WebSocket, discovering tools, and calling
//! those tools via JSON-RPC 2.0.

pub mod client;
pub mod protocol;
pub mod transport;

pub use client::{McpClient, McpConnectedServer, McpTool};
