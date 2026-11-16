//! The task's MCP servers, reached through the protocol's `http` request (the serve container has no
//! route to them), and their tools as the model sees them.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value};
use volund_mcp::{Catalog, HttpRequest, HttpResponse, Server, Transport};
use volund_proto::Outgoing;

use crate::link::Link;

/// How long one MCP request may take in the task container.
const HTTP_TIMEOUT_SEC: u64 = 300;
/// How long connecting to one server (initialize and list its tools) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);

impl Transport for Link {
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        {
            let headers: Map<String, Value> = request
                .headers
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect();
            let body = B64.encode(&request.body);
            let reply = self
                .request(|id| Outgoing::Http {
                    id,
                    url: request.url,
                    method: Some("POST".into()),
                    headers: Some(headers),
                    body_b64: Some(body),
                    timeout_sec: Some(HTTP_TIMEOUT_SEC),
                })
                .await
                .map_err(|e| e.to_string())?;
            if !reply.ok() {
                return Err(reply
                    .error()
                    .unwrap_or("the http request failed")
                    .to_owned());
            }
            let status = reply
                .fields
                .get("status")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16;
            let headers = reply
                .fields
                .get("headers")
                .and_then(Value::as_object)
                .map(|h| {
                    h.iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                v.as_str().map_or_else(|| v.to_string(), str::to_owned),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let body = B64
                .decode(
                    reply
                        .fields
                        .get("body_b64")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
                .map_err(|e| format!("the http reply came back undecodable: {e}"))?;
            let truncated = reply.fields.get("truncated").and_then(Value::as_bool) == Some(true);
            Ok(HttpResponse {
                status,
                headers,
                body,
                truncated,
            })
        }
    }
}

/// Connects to every MCP server of the task. A server that fails is logged and left out; the model is
/// told which ones are missing.
pub async fn connect_all(link: &Link, reserved: &[&str]) -> (Catalog, Vec<String>) {
    let mut servers = Vec::new();
    let mut missing = Vec::new();
    for spec in &link.task.mcp_servers {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            Server::connect(link, &spec.name, &spec.url),
        )
        .await
        {
            Ok(Ok(server)) => {
                link.log(format!("mcp {}: {} tools", server.name, server.tools.len()));
                servers.push(server);
            }
            Ok(Err(e)) => {
                link.log(format!("mcp {}: could not connect: {e}", spec.name));
                missing.push(spec.name.clone());
            }
            Err(_) => {
                link.log(format!(
                    "mcp {}: no answer within {} s",
                    spec.name,
                    CONNECT_TIMEOUT.as_secs()
                ));
                missing.push(spec.name.clone());
            }
        }
    }
    (Catalog::new(servers, reserved), missing)
}

/// Runs a call the model made to a task tool, directly or through `mcp_find`/`mcp_call`. `None` when
/// `name` is not an MCP function.
pub async fn run(
    link: &Link,
    catalog: &Catalog,
    name: &str,
    args: &Map<String, Value>,
) -> Option<String> {
    if catalog.is_empty() {
        return None;
    }
    let (target, arguments) = match name {
        "mcp_find" if !catalog.is_direct() => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            return Some(catalog.find(query));
        }
        "mcp_call" if !catalog.is_direct() => {
            let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
            let arguments = match args.get("arguments") {
                Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
                Some(v) => v.clone(),
                None => Value::Object(Map::new()),
            };
            if !arguments.is_object() {
                return Some("error: arguments must be a JSON object".into());
            }
            (tool.to_owned(), arguments)
        }
        _ if catalog.is_direct() && catalog.resolve(name).is_some() => {
            (name.to_owned(), Value::Object(args.clone()))
        }
        _ => return None,
    };
    let Some((server, tool)) = catalog.resolve(&target) else {
        return Some(format!(
            "error: there is no tool named {target}; use mcp_find"
        ));
    };
    Some(match server.call(link, tool, arguments).await {
        Ok(text) => text,
        Err(e) => format!("error: {e}"),
    })
}
