//! Tool framework and built-in tools for the headless Claude Code Rust rewrite.

pub mod builtin;
pub mod registry;
pub mod result_storage;
pub mod session;
pub mod trait_def;
pub mod util;

pub use registry::{ToolMetadata, ToolRegistry};
pub use result_storage::ToolResultStore;
pub use session::{SessionState, Task, TaskStatus, TodoItem, TodoStatus};
pub use trait_def::{AgentExecutor, PermissionResult, Tool, ToolRef, ToolResult, ToolUseContext};
