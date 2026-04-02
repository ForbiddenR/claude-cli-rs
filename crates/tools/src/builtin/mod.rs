mod bash;
mod file_edit;
mod file_read;
mod file_write;
mod glob;
mod grep;

use std::sync::Arc;

use crate::ToolRef;

pub use bash::BashTool;
pub use file_edit::EditTool;
pub use file_read::ReadTool;
pub use file_write::WriteTool;
pub use glob::GlobTool;
pub use grep::GrepTool;

pub fn default_builtin_tools() -> Vec<ToolRef> {
    vec![
        Arc::new(BashTool::default()),
        Arc::new(ReadTool::default()),
        Arc::new(WriteTool::default()),
        Arc::new(EditTool::default()),
        Arc::new(GlobTool::default()),
        Arc::new(GrepTool::default()),
    ]
}
