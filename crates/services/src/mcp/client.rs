use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use claude_core::config::mcp::McpServerConfig;

use crate::mcp::protocol::{
    CallToolResult, Implementation, InitializeRequestParams, InitializeResult, JsonRpcError,
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, LATEST_PROTOCOL_VERSION, ListToolsResult,
    SUPPORTED_PROTOCOL_VERSIONS, jsonrpc_notification, jsonrpc_request, jsonrpc_response_ok,
};
use crate::mcp::transport::{Transport, Transport as McpTransport};

#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    pub annotations: Option<crate::mcp::protocol::McpToolAnnotations>,
    pub meta: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct McpConnectedServer {
    pub name: String,
    pub protocol_version: String,
    pub capabilities: serde_json::Value,
    pub server_info: Implementation,
    pub instructions: Option<String>,
    client: McpClient,
}

impl McpConnectedServer {
    pub fn client(&self) -> McpClient {
        self.client.clone()
    }
}

#[derive(Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    outbound: mpsc::UnboundedSender<Outbound>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, JsonRpcError>>>>,
    next_id: AtomicU64,
    default_timeout: Duration,
}

#[derive(Debug)]
enum Outbound {
    Rpc(serde_json::Value),
    SetProtocolVersion(String),
    Close,
}

impl McpClient {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> anyhow::Result<McpConnectedServer> {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Mutex::new(HashMap::new());

        let transport = McpTransport::connect(name, cfg).await?;
        let default_timeout = transport.recommended_timeout();

        let inner = Arc::new(Inner {
            name: name.to_string(),
            outbound: tx,
            pending,
            next_id: AtomicU64::new(1),
            default_timeout,
        });

        tokio::spawn(run_transport_loop(inner.clone(), transport, rx));

        let client = McpClient { inner };

        let init = client.initialize().await?;

        Ok(McpConnectedServer {
            name: name.to_string(),
            protocol_version: init.protocol_version,
            capabilities: init.capabilities,
            server_info: init.server_info,
            instructions: init.instructions,
            client,
        })
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpTool>> {
        let result = self.request("tools/list", None).await?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;

        Ok(parsed
            .tools
            .into_iter()
            .map(|t| McpTool {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                annotations: t.annotations,
                meta: t._meta,
            })
            .collect())
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments.unwrap_or_else(|| serde_json::json!({})),
            "_meta": meta
        });

        let result = self.request("tools/call", Some(params)).await?;
        let parsed: CallToolResult = serde_json::from_value(result)?;
        Ok(parsed)
    }

    async fn initialize(&self) -> anyhow::Result<InitializeResult> {
        let params = InitializeRequestParams {
            protocol_version: LATEST_PROTOCOL_VERSION.to_string(),
            // Minimal capabilities: we can list/call tools.
            capabilities: serde_json::json!({
                "tools": {},
                "resources": {},
                "prompts": {}
            }),
            client_info: Implementation {
                name: "claude-rs".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        let result = self
            .request("initialize", Some(serde_json::to_value(params)?))
            .await?;

        let init: InitializeResult = serde_json::from_value(result)?;
        if !SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .any(|v| v == &init.protocol_version)
        {
            anyhow::bail!(
                "mcp server {}: protocol version not supported: {}",
                self.inner.name,
                init.protocol_version
            );
        }

        // Inform the transport (HTTP transports include it in headers).
        let _ = self
            .inner
            .outbound
            .send(Outbound::SetProtocolVersion(init.protocol_version.clone()));

        // Finish init handshake.
        self.notify("notifications/initialized", None).await?;

        Ok(init)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = jsonrpc_request(id, method, params);
        let value = serde_json::to_value(msg)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(id, tx);
        }

        self.inner
            .outbound
            .send(Outbound::Rpc(value))
            .map_err(|_| anyhow::anyhow!("mcp server {}: transport closed", self.inner.name))?;

        let res = tokio::time::timeout(self.inner.default_timeout, rx).await;
        let res = match res {
            Ok(Ok(r)) => r,
            Ok(Err(_closed)) => {
                anyhow::bail!("mcp server {}: request cancelled", self.inner.name)
            }
            Err(_elapsed) => {
                anyhow::bail!(
                    "mcp server {}: request timed out after {:?}",
                    self.inner.name,
                    self.inner.default_timeout
                )
            }
        };

        match res {
            Ok(v) => Ok(v),
            Err(e) => anyhow::bail!(
                "mcp server {}: request failed ({}) {}",
                self.inner.name,
                e.code,
                e.message
            ),
        }
    }

    pub async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let msg = jsonrpc_notification(method, params);
        let value = serde_json::to_value(msg)?;

        self.inner
            .outbound
            .send(Outbound::Rpc(value))
            .map_err(|_| anyhow::anyhow!("mcp server {}: transport closed", self.inner.name))?;
        Ok(())
    }

    pub async fn close(&self) {
        let _ = self.inner.outbound.send(Outbound::Close);
    }
}

async fn run_transport_loop(
    inner: Arc<Inner>,
    mut transport: Transport,
    mut outbound_rx: mpsc::UnboundedReceiver<Outbound>,
) {
    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(Outbound::Rpc(value)) => {
                        let _ = transport.send_json(&value).await;
                    }
                    Some(Outbound::SetProtocolVersion(v)) => {
                        transport.set_protocol_version(&v);
                    }
                    Some(Outbound::Close) | None => {
                        transport.close().await;
                        break;
                    }
                }
            }
            inbound = transport.next_json() => {
                let Some(inbound) = inbound else {
                    break;
                };
                let Ok(value) = inbound else {
                    // Transport-level error. Best-effort: fail all pending requests.
                    fail_all_pending(&inner).await;
                    break;
                };

                let msg: Result<JsonRpcMessage, _> = serde_json::from_value(value);
                let Ok(msg) = msg else {
                    // Ignore malformed messages.
                    continue;
                };

                handle_inbound(&inner, msg, &mut transport).await;
            }
        }
    }
}

async fn fail_all_pending(inner: &Inner) {
    let mut pending = inner.pending.lock().await;
    pending.clear();
}

async fn handle_inbound(inner: &Arc<Inner>, msg: JsonRpcMessage, transport: &mut Transport) {
    match msg {
        JsonRpcMessage::Response(resp) => handle_response(inner, resp).await,
        JsonRpcMessage::Request(req) => handle_request(inner, req, transport).await,
        JsonRpcMessage::Notification(_n) => {
            // Currently ignored.
        }
    }
}

async fn handle_response(inner: &Arc<Inner>, resp: JsonRpcResponse) {
    let id = match resp.id.as_u64() {
        Some(id) => id,
        None => return,
    };

    let tx = {
        let mut pending = inner.pending.lock().await;
        pending.remove(&id)
    };

    let Some(tx) = tx else { return };

    let result = match (resp.result, resp.error) {
        (Some(result), None) => Ok(result),
        (_, Some(err)) => Err(err),
        (None, None) => Ok(serde_json::Value::Null),
    };

    let _ = tx.send(result);
}

async fn handle_request(_inner: &Arc<Inner>, req: JsonRpcRequest, transport: &mut Transport) {
    if req.method == "ping" {
        // Respond promptly.
        let response = jsonrpc_response_ok(req.id, serde_json::json!({}));
        if let Ok(value) = serde_json::to_value(response) {
            let _ = transport.send_json(&value).await;
        }
        return;
    }

    // Unknown request: respond with empty result to avoid timeouts.
    let response = jsonrpc_response_ok(req.id, serde_json::json!({}));
    if let Ok(value) = serde_json::to_value(response) {
        let _ = transport.send_json(&value).await;
    }
}
