use async_trait::async_trait;
use reqwest::Url;

use crate::{PermissionResult, Tool, ToolResult, ToolUseContext};

const TOOL_NAME: &str = "WebSearch";

#[derive(Debug, Default, Clone)]
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> serde_json::Value {
        // Matches the TS CLI schema (subset).
        serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "query": { "type": "string", "minLength": 2, "description": "The search query to use" },
            "allowed_domains": { "type": "array", "items": { "type": "string" }, "description": "Only include results from these domains" },
            "blocked_domains": { "type": "array", "items": { "type": "string" }, "description": "Never include results from these domains" }
          },
          "required": ["query"]
        })
    }

    fn prompt(&self) -> String {
        "Search the public web for up-to-date information. Returns a short list of links."
            .to_string()
    }

    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        if ctx.allows_dangerous_tools() {
            PermissionResult::Allow
        } else {
            PermissionResult::deny(
                "WebSearch is disabled in this permission mode. Re-run with --permission-mode acceptEdits, dontAsk, or bypassPermissions.",
            )
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &mut ToolUseContext,
    ) -> anyhow::Result<ToolResult> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if query.len() < 2 {
            return Ok(ToolResult::err_text("query must be at least 2 characters"));
        }

        let allowed = input
            .get("allowed_domains")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let blocked = input
            .get("blocked_domains")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let http = reqwest::Client::new();
        let url = Url::parse_with_params(
            "https://api.duckduckgo.com/",
            &[
                ("q", query.as_str()),
                ("format", "json"),
                ("no_redirect", "1"),
                ("no_html", "1"),
                ("skip_disambig", "1"),
            ],
        )?;

        let json: serde_json::Value = http
            .get(url)
            .header(reqwest::header::USER_AGENT, "claude-rs/0.1 (web_search)")
            .send()
            .await?
            .json()
            .await?;

        let mut hits: Vec<(String, String)> = Vec::new();
        collect_ddg_hits(&json, &mut hits);

        hits.retain(|(_title, link)| domain_allowed(link, &allowed, &blocked));

        hits.truncate(8);

        if hits.is_empty() {
            return Ok(ToolResult::ok_text(format!(
                "No results found for query: {query}"
            )));
        }

        let mut out = String::new();
        out.push_str(&format!("Results for: {query}\n"));
        for (i, (title, link)) in hits.iter().enumerate() {
            out.push_str(&format!("{}. {} — {}\n", i + 1, title, link));
        }

        Ok(ToolResult::ok_text(out.trim_end().to_string()))
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
}

fn domain_allowed(url: &str, allowed: &[String], blocked: &[String]) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();

    if blocked
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    {
        return false;
    }
    if allowed.is_empty() {
        return true;
    }
    allowed
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

fn collect_ddg_hits(v: &serde_json::Value, out: &mut Vec<(String, String)>) {
    if let Some(results) = v.get("Results").and_then(|v| v.as_array()) {
        for r in results {
            let url = r.get("FirstURL").and_then(|v| v.as_str());
            let text = r.get("Text").and_then(|v| v.as_str());
            if let (Some(url), Some(text)) = (url, text) {
                out.push((text.to_string(), url.to_string()));
            }
        }
    }

    if let Some(topics) = v.get("RelatedTopics").and_then(|v| v.as_array()) {
        for t in topics {
            if let Some(nested) = t.get("Topics").and_then(|v| v.as_array()) {
                collect_ddg_hits(&serde_json::json!({ "RelatedTopics": nested }), out);
                continue;
            }
            let url = t.get("FirstURL").and_then(|v| v.as_str());
            let text = t.get("Text").and_then(|v| v.as_str());
            if let (Some(url), Some(text)) = (url, text) {
                out.push((text.to_string(), url.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_allowed_respects_allow_and_block_lists() {
        assert!(domain_allowed("https://example.com/path", &[], &[]));

        assert!(!domain_allowed(
            "https://example.com/path",
            &[],
            &["example.com".to_string()]
        ));

        assert!(!domain_allowed(
            "https://example.com/path",
            &["allowed.com".to_string()],
            &[]
        ));

        assert!(domain_allowed(
            "https://example.com/path",
            &["example.com".to_string()],
            &[]
        ));

        // Blocking a parent domain blocks subdomains as well.
        assert!(!domain_allowed(
            "https://sub.example.com/path",
            &[],
            &["example.com".to_string()]
        ));
    }

    #[test]
    fn collect_ddg_hits_collects_results_and_related_topics() {
        let json = serde_json::json!({
            "Results": [
                { "FirstURL": "https://a.example/a", "Text": "A result" }
            ],
            "RelatedTopics": [
                { "FirstURL": "https://b.example/b", "Text": "B result" },
                { "Topics": [
                    { "FirstURL": "https://c.example/c", "Text": "C result" }
                ]}
            ]
        });

        let mut hits = Vec::new();
        collect_ddg_hits(&json, &mut hits);

        assert_eq!(hits.len(), 3);
        assert!(
            hits.iter()
                .any(|(t, u)| t == "A result" && u == "https://a.example/a")
        );
        assert!(
            hits.iter()
                .any(|(t, u)| t == "B result" && u == "https://b.example/b")
        );
        assert!(
            hits.iter()
                .any(|(t, u)| t == "C result" && u == "https://c.example/c")
        );
    }
}
