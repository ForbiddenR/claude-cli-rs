use std::collections::{HashMap, HashSet};

use crate::ToolRef;
use crate::builtin;

#[derive(Debug, Clone)]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct ToolPoolOpts {
    /// When non-empty, only these tools are included.
    pub base_tools: Vec<String>,

    /// When non-empty, only these tools are allowed.
    pub allowed_tools: Vec<String>,

    /// Always removed from the pool.
    pub disallowed_tools: Vec<String>,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<ToolRef>,
    index: HashMap<String, ToolRef>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<ToolRef>) -> anyhow::Result<Self> {
        let mut index: HashMap<String, ToolRef> = HashMap::new();

        for t in &tools {
            let name = t.name().to_string();
            if index.contains_key(&name) {
                anyhow::bail!("duplicate tool name: {name}");
            }
            index.insert(name, t.clone());

            for &alias in t.aliases() {
                if index.contains_key(alias) {
                    anyhow::bail!("duplicate tool alias: {alias}");
                }
                index.insert(alias.to_string(), t.clone());
            }
        }

        Ok(Self { tools, index })
    }

    pub fn tools(&self) -> &[ToolRef] {
        &self.tools
    }

    pub fn get(&self, name: &str) -> Option<ToolRef> {
        self.index.get(name).cloned()
    }

    pub fn metadata(&self) -> Vec<ToolMetadata> {
        self.tools
            .iter()
            .map(|t| ToolMetadata {
                name: t.name().to_string(),
                description: t.prompt(),
                input_schema: t.input_schema(),
            })
            .collect()
    }
}

pub fn parse_tool_list(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for s in raw {
        for part in s.split(|c: char| c == ',' || c.is_whitespace()) {
            let part = part.trim();
            if !part.is_empty() {
                out.push(part.to_string());
            }
        }
    }
    out
}

fn name_set(names: &[String]) -> HashSet<String> {
    names
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn assemble_tool_pool(opts: ToolPoolOpts) -> anyhow::Result<ToolRegistry> {
    assemble_tool_pool_with_extra(Vec::new(), opts)
}

pub fn assemble_tool_pool_with_extra(
    extra: Vec<ToolRef>,
    opts: ToolPoolOpts,
) -> anyhow::Result<ToolRegistry> {
    // `--tools` is intended to control the built-in tool set, not dynamically
    // discovered tools (e.g., MCP). So we apply `base_tools` only to builtins.
    let mut builtins = builtin::default_builtin_tools();

    let base = name_set(&parse_tool_list(&opts.base_tools));
    if !base.is_empty() {
        builtins.retain(|t| base.contains(&t.name().to_ascii_lowercase()));
    }

    let mut tools = builtins;
    tools.extend(extra);

    let allowed = name_set(&parse_tool_list(&opts.allowed_tools));
    if !allowed.is_empty() {
        tools.retain(|t| allowed.contains(&t.name().to_ascii_lowercase()));
    }

    let disallowed = name_set(&parse_tool_list(&opts.disallowed_tools));
    if !disallowed.is_empty() {
        tools.retain(|t| !disallowed.contains(&t.name().to_ascii_lowercase()));
    }

    // Ensure stable order and uniqueness by tool name.
    let mut seen: HashSet<String> = HashSet::new();
    tools.retain(|t| {
        let k = t.name().to_string();
        if seen.contains(&k) {
            false
        } else {
            seen.insert(k);
            true
        }
    });

    ToolRegistry::new(tools)
}
