//! Tool framework and built-in tools for the headless Claude Code Rust rewrite.

pub mod builtin;
pub mod registry;
pub mod trait_def;
pub mod util;

pub use registry::{ToolMetadata, ToolRegistry};
pub use trait_def::{PermissionResult, Tool, ToolRef, ToolResult, ToolUseContext};
