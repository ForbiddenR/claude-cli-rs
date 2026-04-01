use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
pub enum PermissionMode {
    #[serde(rename = "acceptEdits")]
    #[value(name = "acceptEdits")]
    AcceptEdits,

    #[serde(rename = "bypassPermissions")]
    #[value(name = "bypassPermissions")]
    BypassPermissions,

    #[serde(rename = "default")]
    #[value(name = "default")]
    Default,

    #[serde(rename = "dontAsk")]
    #[value(name = "dontAsk")]
    DontAsk,

    #[serde(rename = "plan")]
    #[value(name = "plan")]
    Plan,

    // Internal / conditional modes in the TS implementation.
    #[serde(rename = "auto")]
    #[value(name = "auto")]
    Auto,

    #[serde(rename = "bubble")]
    #[value(name = "bubble")]
    Bubble,
}

