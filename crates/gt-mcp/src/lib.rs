use serde_json::{json, Value};

pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
pub const SERVER_NAME: &str = "gitforest-mcp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7878";

pub fn daemon_url() -> String {
    std::env::var("GITFOREST_DAEMON_URL").unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string())
}

pub fn api_token() -> Option<String> {
    if let Ok(token) = std::env::var("GITFOREST_API_TOKEN") {
        if !token.trim().is_empty() {
            return Some(token);
        }
    }
    let path = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".local/share/gitforest/api.token"))
        .unwrap_or_else(|| std::env::temp_dir().join("gitforest-api.token"));
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

fn str_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "daemon_status",
            "Network status of the local gitforest daemon: peer id, connections, repo counts.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "list_repos",
            "List repos hosted by this node.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "repo_details",
            "Details, refs, and branches for one hosted repo.",
            object_schema(json!({ "name": str_prop("Repo name") }), &["name"]),
        ),
        tool(
            "list_jobs",
            "List compute jobs submitted from this node.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "job_status",
            "Status and result (exit code, log tails, resource usage) for one compute job.",
            object_schema(json!({ "id": str_prop("Job id") }), &["id"]),
        ),
        tool(
            "list_offers",
            "Compute workers currently discovered on the network.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "credit_balance",
            "Mutual-credit balance: given/taken compute-seconds, deficit, allowance, per-peer table.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "list_blobs",
            "Blobs in the local large-file store.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "search_models",
            "Search shared model collections on the network.",
            object_schema(
                json!({
                    "text": str_prop("Free-text query (optional)"),
                    "architecture": str_prop("Architecture filter, e.g. llama (optional)"),
                    "quantization": str_prop("Quantization filter, e.g. q4_0 (optional)"),
                    "format": str_prop("Format filter, e.g. safetensors (optional)"),
                }),
                &[],
            ),
        ),
        tool(
            "submit_job",
            "Submit a CI job for a hosted repo. Needs the repo's .gitforest-ci.yml content as YAML text.",
            object_schema(
                json!({
                    "repo_name": str_prop("Hosted repo name"),
                    "commit_sha": str_prop("Commit to build (or HEAD)"),
                    "job_config_yaml": str_prop("CI config YAML text"),
                    "max_payment_sats": { "type": "integer", "description": "Max payment in sats (default 100)" },
                    "preferred_chain": str_prop("Payment chain (default lightning)"),
                }),
                &["repo_name", "commit_sha", "job_config_yaml"],
            ),
        ),
        tool(
            "approve_job",
            "Approve a job waiting on a protected environment.",
            object_schema(json!({ "id": str_prop("Job id") }), &["id"]),
        ),
        tool(
            "reject_job",
            "Reject a job waiting on a protected environment (cancels it).",
            object_schema(json!({ "id": str_prop("Job id") }), &["id"]),
        ),
        tool(
            "cancel_job",
            "Cancel a pending or running compute job.",
            object_schema(json!({ "id": str_prop("Job id") }), &["id"]),
        ),
        tool(
            "list_workflows",
            "Workflow files available in a hosted repo.",
            object_schema(json!({ "name": str_prop("Repo name") }), &["name"]),
        ),
        tool(
            "dispatch_workflow",
            "Dispatch a workflow run in a hosted repo.",
            object_schema(
                json!({
                    "name": str_prop("Repo name"),
                    "workflow": str_prop("Workflow file, e.g. ci.yml (optional if the trigger selects one)"),
                    "branch": str_prop("Branch for push triggers (optional)"),
                    "tag": str_prop("Tag for tag triggers (optional)"),
                    "commit_sha": str_prop("Commit to run at (optional)"),
                    "inputs": { "type": "object", "description": "workflow_dispatch inputs (optional)", "additionalProperties": { "type": "string" } },
                }),
                &["name"],
            ),
        ),
        tool(
            "list_workflow_runs",
            "Recent workflow runs across repos.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "get_workflow_run",
            "One workflow run: summary plus per-job status.",
            object_schema(json!({ "id": str_prop("Run id") }), &["id"]),
        ),
        tool(
            "get_model",
            "Manifest, shards, and local availability for one model.",
            object_schema(json!({ "id": str_prop("Manifest id") }), &["id"]),
        ),
        tool(
            "pull_model",
            "Fetch a model's weights from the network (async; poll get_model).",
            object_schema(json!({ "id": str_prop("Manifest id") }), &["id"]),
        ),
        tool(
            "infer_model",
            "Run text inference on a model, locally or proxied to a seeder.",
            object_schema(
                json!({
                    "id": str_prop("Manifest id"),
                    "prompt": str_prop("Prompt text"),
                    "max_tokens": { "type": "integer", "description": "Max tokens (optional)" },
                    "temperature": { "type": "number", "description": "Sampling temperature (optional)" },
                }),
                &["id", "prompt"],
            ),
        ),
        tool(
            "publish_model",
            "Publish a local collection as a shared model on the DHT.",
            object_schema(json!({ "collection_id": str_prop("Collection id") }), &["collection_id"]),
        ),
        tool(
            "add_repo",
            "Host a local repo directory on this node.",
            object_schema(
                json!({
                    "path": str_prop("Repo directory on the daemon host"),
                    "name": str_prop("Name to host it under (optional)"),
                }),
                &["path"],
            ),
        ),
        tool(
            "repo_log",
            "Recent commits for a hosted repo.",
            object_schema(json!({ "name": str_prop("Repo name") }), &["name"]),
        ),
        tool(
            "list_peers",
            "Bootstrap, discovered, and connected peers.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "list_caches",
            "Local and remote workflow caches.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "get_profile",
            "This node's peer id, display name, and peer name.",
            object_schema(json!({}), &[]),
        ),
    ]
}

pub struct McpServer {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl McpServer {
    pub fn new(base_url: String, token: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(McpServer {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        Self::new(daemon_url(), api_token())
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn daemon_get(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .authed(self.client.get(&url))
            .send()
            .await
            .map_err(|e| format!("daemon unreachable: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("daemon HTTP {status}: {}", truncate(&body, 500)));
        }
        serde_json::from_str(&body).map_err(|e| format!("daemon sent invalid JSON: {e}"))
    }

    async fn daemon_post(&self, path: &str, body: Value) -> Result<Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .authed(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| format!("daemon unreachable: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("daemon HTTP {status}: {}", truncate(&text, 500)));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| format!("daemon sent invalid JSON: {e}"))
    }

    fn arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
        args.get(name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("missing required string argument: {name}"))
    }

    async fn call_tool(&self, name: &str, args: &Value) -> Result<Value, String> {
        let null = Value::Null;
        let args = if args.is_object() { args } else { &null };
        match name {
            "daemon_status" => self.daemon_get("/api/status").await,
            "list_repos" => self.daemon_get("/api/repos").await,
            "repo_details" => {
                let name = Self::arg(args, "name")?;
                self.daemon_get(&format!("/api/repos/{}", url_encode(name)))
                    .await
            }
            "list_jobs" => self.daemon_get("/api/compute/jobs").await,
            "job_status" => {
                let id = Self::arg(args, "id")?;
                self.daemon_get(&format!("/api/compute/jobs/{}", url_encode(id)))
                    .await
            }
            "list_offers" => self.daemon_get("/api/compute/offers").await,
            "credit_balance" => self.daemon_get("/api/compute/credit").await,
            "list_blobs" => self.daemon_get("/api/blobs").await,
            "search_models" => {
                let mut q = Vec::new();
                for key in ["text", "architecture", "quantization", "format"] {
                    if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                        q.push(format!("{key}={}", url_encode(v)));
                    }
                }
                let path = if q.is_empty() {
                    "/api/models/search".to_string()
                } else {
                    format!("/api/models/search?{}", q.join("&"))
                };
                self.daemon_get(&path).await
            }
            "submit_job" => {
                let body = json!({
                    "repo_name": Self::arg(args, "repo_name")?,
                    "commit_sha": Self::arg(args, "commit_sha")?,
                    "job_config_yaml": Self::arg(args, "job_config_yaml")?,
                    "max_payment_sats": args.get("max_payment_sats").and_then(|v| v.as_u64()).unwrap_or(100),
                    "preferred_chain": args.get("preferred_chain").and_then(|v| v.as_str()).unwrap_or("lightning"),
                });
                self.daemon_post("/api/compute/jobs", body).await
            }
            "approve_job" => {
                let id = Self::arg(args, "id")?;
                self.daemon_post(
                    &format!("/api/compute/jobs/{}/approve", url_encode(id)),
                    json!({}),
                )
                .await
            }
            "reject_job" => {
                let id = Self::arg(args, "id")?;
                self.daemon_post(
                    &format!("/api/compute/jobs/{}/reject", url_encode(id)),
                    json!({}),
                )
                .await
            }
            "cancel_job" => {
                let id = Self::arg(args, "id")?;
                self.daemon_post(
                    &format!("/api/compute/jobs/{}/cancel", url_encode(id)),
                    json!({}),
                )
                .await
            }
            "list_workflows" => {
                let name = Self::arg(args, "name")?;
                self.daemon_get(&format!("/api/repos/{}/workflows", url_encode(name)))
                    .await
            }
            "dispatch_workflow" => {
                let name = Self::arg(args, "name")?;
                let opt = |key: &str| args.get(key).and_then(|v| v.as_str());
                let body = json!({
                    "workflow": opt("workflow"),
                    "branch": opt("branch"),
                    "tag": opt("tag"),
                    "commit_sha": opt("commit_sha"),
                    "inputs": args.get("inputs").cloned().unwrap_or(json!({})),
                });
                self.daemon_post(
                    &format!("/api/repos/{}/workflows/dispatch", url_encode(name)),
                    body,
                )
                .await
            }
            "list_workflow_runs" => self.daemon_get("/api/workflows/runs").await,
            "get_workflow_run" => {
                let id = Self::arg(args, "id")?;
                self.daemon_get(&format!("/api/workflows/runs/{}", url_encode(id)))
                    .await
            }
            "get_model" => {
                let id = Self::arg(args, "id")?;
                self.daemon_get(&format!("/api/models/{}", url_encode(id)))
                    .await
            }
            "pull_model" => {
                let id = Self::arg(args, "id")?;
                self.daemon_post(&format!("/api/models/{}/pull", url_encode(id)), json!({}))
                    .await
            }
            "infer_model" => {
                let id = Self::arg(args, "id")?;
                let body = json!({
                    "prompt": Self::arg(args, "prompt")?,
                    "max_tokens": args.get("max_tokens").and_then(|v| v.as_u64()),
                    "temperature": args.get("temperature").and_then(|v| v.as_f64()),
                });
                self.daemon_post(&format!("/api/models/{}/infer", url_encode(id)), body)
                    .await
            }
            "publish_model" => {
                let body = json!({ "collection_id": Self::arg(args, "collection_id")? });
                self.daemon_post("/api/models/publish", body).await
            }
            "add_repo" => {
                let mut body = json!({ "path": Self::arg(args, "path")? });
                if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
                    body["name"] = json!(name);
                }
                self.daemon_post("/api/repos", body).await
            }
            "repo_log" => {
                let name = Self::arg(args, "name")?;
                self.daemon_get(&format!("/api/repos/{}/log", url_encode(name)))
                    .await
            }
            "list_peers" => self.daemon_get("/api/peers").await,
            "list_caches" => self.daemon_get("/api/caches/list").await,
            "get_profile" => self.daemon_get("/api/profile").await,
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn ok(id: &Value, result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    fn err(id: &Value, code: i64, message: String) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
    }

    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        let method = msg.get("method").and_then(|m| m.as_str())?;
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let is_notification = msg.get("id").is_none();
        if method.starts_with("notifications/") {
            return None;
        }
        if is_notification {
            return None;
        }
        let result = match method {
            "initialize" => Self::ok(
                &id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                }),
            ),
            "ping" => Self::ok(&id, json!({})),
            "tools/list" => Self::ok(&id, json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let params = msg.get("params").unwrap_or(&Value::Null);
                let name = match params.get("name").and_then(|n| n.as_str()) {
                    Some(n) => n,
                    None => {
                        return Some(Self::err(&id, -32602, "missing tool name".to_string()));
                    }
                };
                let args = params.get("arguments").unwrap_or(&Value::Null);
                match self.call_tool(name, args).await {
                    Ok(value) => Self::ok(
                        &id,
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }] }),
                    ),
                    Err(e) => Self::err(&id, -32603, e),
                }
            }
            _ => Self::err(&id, -32601, format!("method not found: {method}")),
        };
        Some(result)
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn mock_daemon(body: &'static str, status: u16) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let reason = if status == 200 { "OK" } else { "Error" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn test_initialize_and_tools_list() {
        let server = McpServer::new("http://127.0.0.1:9".into(), None).unwrap();
        let init: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        )
        .unwrap();
        let resp = server.handle(&init).await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "gitforest-mcp");
        let list: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let resp = server.handle(&list).await.unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"daemon_status"));
        assert!(names.contains(&"submit_job"));
        assert!(names.contains(&"search_models"));
        assert!(names.contains(&"dispatch_workflow"));
        assert!(names.contains(&"get_workflow_run"));
        assert!(names.contains(&"approve_job"));
        assert!(names.contains(&"reject_job"));
        assert!(names.contains(&"cancel_job"));
        assert!(names.contains(&"infer_model"));
        assert!(names.contains(&"pull_model"));
        assert!(names.contains(&"publish_model"));
        assert!(names.contains(&"list_peers"));
        assert!(names.contains(&"list_caches"));
        assert!(names.contains(&"get_profile"));
        assert_eq!(names.len(), 26);
    }

    #[tokio::test]
    async fn test_notifications_get_no_response() {
        let server = McpServer::new("http://127.0.0.1:9".into(), None).unwrap();
        let note: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(server.handle(&note).await.is_none());
        let unknown: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#).unwrap();
        let resp = server.handle(&unknown).await.unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn test_tool_call_round_trip() {
        let (url, handle) = mock_daemon(r#"{"peer_id":"abc","repos":2}"#, 200).await;
        let server = McpServer::new(url, None).unwrap();
        let call: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"daemon_status"}}"#,
        )
        .unwrap();
        let resp = server.handle(&call).await.unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("abc"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_post_tool_round_trip() {
        let (url, handle) = mock_daemon(r#"{"status":"complete","completion":"hi"}"#, 200).await;
        let server = McpServer::new(url, None).unwrap();
        let call: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"infer_model","arguments":{"id":"m1","prompt":"hello"}}}"#,
        )
        .unwrap();
        let resp = server.handle(&call).await.unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("complete"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_daemon_error_becomes_tool_error() {
        let (url, handle) = mock_daemon(r#"nope"#, 500).await;
        let server = McpServer::new(url, None).unwrap();
        let call: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_jobs"}}"#,
        )
        .unwrap();
        let resp = server.handle(&call).await.unwrap();
        assert_eq!(resp["error"]["code"], -32603);
        assert!(resp["error"]["message"].as_str().unwrap().contains("500"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_missing_arg_is_invalid_params() {
        let server = McpServer::new("http://127.0.0.1:9".into(), None).unwrap();
        let call: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"job_status","arguments":{}}}"#,
        )
        .unwrap();
        let resp = server.handle(&call).await.unwrap();
        assert_eq!(resp["error"]["code"], -32603);
        assert!(resp["error"]["message"].as_str().unwrap().contains("id"));
    }
}
