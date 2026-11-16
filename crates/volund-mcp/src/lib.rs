//! MCP client for the task's apps (documents, spreadsheets, email, CAD) over Streamable HTTP.
//!
//! The harness cannot reach the apps itself: every HTTP request goes through the serve protocol's
//! `http` request and runs inside the task container ([`Transport`]). Per `docs/protocol.md` §2:
//! JSON-RPC `POST`s (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`) with
//! `Accept: application/json, text/event-stream`, the returned `Mcp-Session-Id` and the negotiated
//! `MCP-Protocol-Version` on later requests, and replies in either JSON or server-sent events.
//!
//! [`Catalog`] decides how the model sees the tools: as functions when the catalog is small, or
//! through `mcp_find` and `mcp_call` when it is large (a task can hold hundreds of tools, ~500 KB).

mod catalog;

use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

pub use catalog::Catalog;

/// The protocol version asked for in `initialize`; the server's answer is used afterwards.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Characters of a tool result the model sees from one embedded resource.
const RESOURCE_CHARS: usize = 30_000;

pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

impl HttpResponse {
    /// A header's value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one HTTP `POST` from inside the task container.
pub trait Transport: Sync {
    fn post(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, String>> + Send;
}

/// One connected MCP server and its tools.
pub struct Server {
    pub name: String,
    pub url: String,
    /// The tools as `tools/list` returned them (`name`, `description`, `inputSchema`, ...).
    pub tools: Vec<Value>,
    session: Mutex<Option<String>>,
    version: Mutex<String>,
    next_id: AtomicU64,
}

impl Server {
    /// Initializes a session and lists every tool (following `nextCursor`).
    pub async fn connect<T: Transport>(
        transport: &T,
        name: &str,
        url: &str,
    ) -> Result<Self, String> {
        let mut server = Self {
            name: name.to_owned(),
            url: url.to_owned(),
            tools: Vec::new(),
            session: Mutex::new(None),
            version: Mutex::new(PROTOCOL_VERSION.to_owned()),
            next_id: AtomicU64::new(1),
        };
        server.initialize(transport).await?;
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = server.rpc(transport, "tools/list", params).await?;
            if let Some(tools) = page.get("tools").and_then(Value::as_array) {
                server
                    .tools
                    .extend(tools.iter().filter(|t| t["name"].is_string()).cloned());
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(server)
    }

    async fn initialize<T: Transport>(&self, transport: &T) -> Result<(), String> {
        *self.lock_session() = None;
        let result = self
            .rpc_once(
                transport,
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "volund", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        if let Some(v) = result.get("protocolVersion").and_then(Value::as_str) {
            *self.version.lock().unwrap_or_else(|e| e.into_inner()) = v.to_owned();
        }
        self.notify(transport, "notifications/initialized").await
    }

    /// Calls a tool and formats its result as text for the model; a tool-level failure (`isError`)
    /// comes back as `error: …` text, a transport failure as `Err`.
    pub async fn call<T: Transport>(
        &self,
        transport: &T,
        tool: &str,
        arguments: Value,
    ) -> Result<String, String> {
        let result = self
            .rpc(
                transport,
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
            )
            .await?;
        Ok(format_result(&result))
    }

    /// One JSON-RPC request; when the server has forgotten the session (`404` with a session id), a
    /// new session is started once and the request sent again.
    async fn rpc<T: Transport>(
        &self,
        transport: &T,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        match self.rpc_once(transport, method, params.clone()).await {
            Err(e) if e.starts_with("HTTP 404") && self.lock_session().is_some() => {
                self.initialize(transport).await?;
                self.rpc_once(transport, method, params).await
            }
            other => other,
        }
    }

    async fn rpc_once<T: Transport>(
        &self,
        transport: &T,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = transport.post(self.request(&body)).await?;
        if let Some(session) = response.header("mcp-session-id") {
            *self.lock_session() = Some(session.to_owned());
        }
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "HTTP {} from {}: {}",
                response.status,
                self.name,
                clip(&String::from_utf8_lossy(&response.body), 500)
            ));
        }
        if response.truncated {
            return Err(format!(
                "the reply from {} was larger than one http request returns",
                self.name
            ));
        }
        let message = find_reply(&response, id).ok_or_else(|| {
            format!(
                "{} sent no JSON-RPC reply to {method}: {}",
                self.name,
                clip(&String::from_utf8_lossy(&response.body), 300)
            )
        })?;
        if let Some(error) = message.get("error").filter(|e| !e.is_null()) {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            return Err(format!("{} refused {method}: {text}", self.name));
        }
        Ok(message.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify<T: Transport>(&self, transport: &T, method: &str) -> Result<(), String> {
        let body = json!({"jsonrpc": "2.0", "method": method});
        let response = transport.post(self.request(&body)).await?;
        if response.status >= 400 {
            return Err(format!(
                "HTTP {} from {} on {method}",
                response.status, self.name
            ));
        }
        Ok(())
    }

    fn request(&self, body: &Value) -> HttpRequest {
        let mut headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            (
                "Accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
        ];
        if let Some(session) = self.lock_session().clone() {
            headers.push(("Mcp-Session-Id".to_owned(), session));
            let version = self
                .version
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            headers.push(("MCP-Protocol-Version".to_owned(), version));
        }
        HttpRequest {
            url: self.url.clone(),
            headers,
            body: body.to_string().into_bytes(),
        }
    }

    fn lock_session(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The JSON-RPC message answering `id`, from a JSON body or a stream of server-sent events (where it
/// may sit among notifications, or in a batch).
fn find_reply(response: &HttpResponse, id: u64) -> Option<Value> {
    let text = String::from_utf8_lossy(&response.body);
    let is_sse = response
        .header("content-type")
        .is_some_and(|t| t.contains("text/event-stream"))
        || text.trim_start().starts_with("event:")
        || text.trim_start().starts_with("data:");
    let candidates: Vec<Value> = if is_sse {
        sse_payloads(&text)
            .iter()
            .filter_map(|p| serde_json::from_str(p).ok())
            .collect()
    } else {
        serde_json::from_str(&text).ok().into_iter().collect()
    };
    candidates
        .into_iter()
        .flat_map(|v| match v {
            Value::Array(items) => items,
            other => vec![other],
        })
        .find(|m| m.get("id").and_then(Value::as_u64) == Some(id))
}

/// The `data` of each event in a server-sent event stream.
fn sse_payloads(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut data: Option<String> = None;
    for line in text.lines().chain(std::iter::once("")) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(d) = data.take() {
                out.push(d);
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(rest);
                }
                None => data = Some(rest.to_owned()),
            }
        }
    }
    out
}

/// A `tools/call` result as text: text parts joined, resources and links as JSON, other content
/// named; `structuredContent` when there is nothing else; `error: ` first when `isError`.
pub fn format_result(result: &Value) -> String {
    let mut parts = Vec::new();
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(item["text"].as_str().unwrap_or("").to_owned()),
            Some("resource") => parts.push(clip(&item["resource"].to_string(), RESOURCE_CHARS)),
            Some("resource_link") => parts.push(clip(&item.to_string(), RESOURCE_CHARS)),
            Some(kind) => parts.push(format!("[{kind} content not shown]")),
            None => parts.push("[unknown content not shown]".to_owned()),
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent").filter(|s| !s.is_null())
    {
        parts.push(structured.to_string());
    }
    let text = if parts.is_empty() {
        "(no content)".to_owned()
    } else {
        parts.join("\n")
    };
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        format!("error: {text}")
    } else {
        text
    }
}

fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_found_in_json_sse_and_batches() {
        let sse = HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":1}}\n\n".to_vec(),
            truncated: false,
        };
        assert_eq!(find_reply(&sse, 3).unwrap()["result"]["ok"], 1);
        assert!(find_reply(&sse, 4).is_none());

        let batch = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: br#"[{"jsonrpc":"2.0","id":1,"result":{}},{"jsonrpc":"2.0","id":2,"result":{"n":2}}]"#.to_vec(),
            truncated: false,
        };
        assert_eq!(find_reply(&batch, 2).unwrap()["result"]["n"], 2);
    }

    #[test]
    fn results_format_like_the_samples() {
        let r = json!({"content": [{"type": "text", "text": "a"}, {"type": "image", "data": "…"}], "isError": false});
        assert_eq!(format_result(&r), "a\n[image content not shown]");
        let e = json!({"content": [{"type": "text", "text": "no such case"}], "isError": true});
        assert_eq!(format_result(&e), "error: no such case");
        let s = json!({"content": [], "structuredContent": {"id": 7}});
        assert_eq!(format_result(&s), "{\"id\":7}");
        assert_eq!(format_result(&json!({})), "(no content)");
    }
}
