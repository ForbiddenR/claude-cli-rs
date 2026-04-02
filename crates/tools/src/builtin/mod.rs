mod agent;
mod ask_user;
mod bash;
mod file_edit;
mod file_read;
mod file_write;
mod glob;
mod grep;
mod notebook_edit;
mod task_create;
mod task_get;
mod task_list;
mod task_output;
mod task_stop;
mod task_update;
mod todo_write;
mod web_fetch;
mod web_search;

use std::sync::Arc;

use crate::ToolRef;

pub use agent::AgentTool;
pub use ask_user::AskUserQuestionTool;
pub use bash::BashTool;
pub use file_edit::EditTool;
pub use file_read::ReadTool;
pub use file_write::WriteTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use notebook_edit::NotebookEditTool;
pub use task_create::TaskCreateTool;
pub use task_get::TaskGetTool;
pub use task_list::TaskListTool;
pub use task_output::TaskOutputTool;
pub use task_stop::TaskStopTool;
pub use task_update::TaskUpdateTool;
pub use todo_write::TodoWriteTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;

pub fn default_builtin_tools() -> Vec<ToolRef> {
    vec![
        Arc::new(BashTool::default()),
        Arc::new(ReadTool::default()),
        Arc::new(WriteTool::default()),
        Arc::new(EditTool::default()),
        Arc::new(GlobTool::default()),
        Arc::new(GrepTool::default()),
        Arc::new(AgentTool::default()),
        Arc::new(WebSearchTool::default()),
        Arc::new(WebFetchTool::default()),
        Arc::new(TodoWriteTool::default()),
        Arc::new(TaskCreateTool::default()),
        Arc::new(TaskGetTool::default()),
        Arc::new(TaskUpdateTool::default()),
        Arc::new(TaskListTool::default()),
        Arc::new(TaskStopTool::default()),
        Arc::new(TaskOutputTool::default()),
        Arc::new(NotebookEditTool::default()),
        Arc::new(AskUserQuestionTool::default()),
    ]
}
