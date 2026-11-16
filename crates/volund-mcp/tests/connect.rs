//! The client against an in-memory server that answers like the kit's MCP stub (server-sent events,
//! a session id, `202` for notifications), with paging and a session that expires.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use volund_mcp::{Catalog, HttpRequest, HttpResponse, Server, Transport};

/// The headers and JSON body of one request the stub received.
type Seen = (Vec<(String, String)>, Value);

#[derive(Default)]
struct Stub {
    seen: Mutex<Vec<Seen>>,
    /// Answer the next session-bearing `tools/call` with 404, as a server that forgot the session.
    expire_once: AtomicBool,
    sessions: Mutex<u32>,
}

impl Stub {
    fn answer(&self, request: &HttpRequest) -> HttpResponse {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        self.seen
            .lock()
            .unwrap()
            .push((request.headers.clone(), body.clone()));
        let has_session = request.headers.iter().any(|(k, _)| k == "Mcp-Session-Id");
        let method = body["method"].as_str().unwrap_or("");
        if body.get("id").is_none() {
            return response(202, b"".to_vec());
        }
        if method == "tools/call" && has_session && self.expire_once.swap(false, Ordering::SeqCst) {
            return response(404, b"session expired".to_vec());
        }
        let result = match method {
            "initialize" => {
                *self.sessions.lock().unwrap() += 1;
                json!({"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "apps"}})
            }
            "tools/list" if body["params"]["cursor"].is_null() => json!({
                "tools": [{"name": "secret_word", "description": "Returns the secret word.", "inputSchema": {"type": "object"}}],
                "nextCursor": "page-2"
            }),
            "tools/list" => {
                json!({"tools": [{"name": "close_case", "description": "Close a case."}]})
            }
            "tools/call" if body["params"]["name"] == "secret_word" => {
                json!({"content": [{"type": "text", "text": "MCP-1A2B3C4D"}], "isError": false})
            }
            _ => {
                return sse(
                    json!({"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": "unknown tool"}}),
                );
            }
        };
        sse(json!({"jsonrpc": "2.0", "id": body["id"], "result": result}))
    }
}

fn response(status: u16, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Mcp-Session-Id".into(), "plebench-check".into()),
        ],
        body,
        truncated: false,
    }
}

fn sse(message: Value) -> HttpResponse {
    let mut r = response(
        200,
        format!("event: message\ndata: {message}\n\n").into_bytes(),
    );
    r.headers[0].1 = "text/event-stream".into();
    r
}

impl Transport for Stub {
    fn post(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, String>> + Send {
        let answer = self.answer(&request);
        async move { Ok(answer) }
    }
}

#[tokio::test]
async fn connects_lists_every_page_and_calls_a_tool() {
    let stub = Stub::default();
    let server = Server::connect(&stub, "apps", "http://127.0.0.1:8000/mcp/")
        .await
        .unwrap();
    let names: Vec<_> = server
        .tools
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["secret_word", "close_case"]);
    assert_eq!(
        server.call(&stub, "secret_word", json!({})).await.unwrap(),
        "MCP-1A2B3C4D"
    );

    let seen = stub.seen.lock().unwrap();
    let methods: Vec<_> = seen
        .iter()
        .map(|(_, b)| b["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/list",
            "tools/call"
        ]
    );
    let (first, _) = &seen[0];
    assert!(
        first
            .iter()
            .any(|(k, v)| k == "Accept" && v == "application/json, text/event-stream")
    );
    assert!(
        !first.iter().any(|(k, _)| k == "Mcp-Session-Id"),
        "no session before initialize"
    );
    let (later, _) = &seen[4];
    assert!(later.contains(&("Mcp-Session-Id".into(), "plebench-check".into())));
    assert!(
        later.contains(&("MCP-Protocol-Version".into(), "2025-03-26".into())),
        "the version the server answered with"
    );
}

#[tokio::test]
async fn an_expired_session_is_renewed_once() {
    let stub = Stub::default();
    let server = Server::connect(&stub, "apps", "http://127.0.0.1:8000/mcp/")
        .await
        .unwrap();
    stub.expire_once.store(true, Ordering::SeqCst);
    assert_eq!(
        server.call(&stub, "secret_word", json!({})).await.unwrap(),
        "MCP-1A2B3C4D"
    );
    assert_eq!(*stub.sessions.lock().unwrap(), 2);
}

#[tokio::test]
async fn refusals_come_back_as_errors() {
    let stub = Stub::default();
    let server = Server::connect(&stub, "apps", "http://127.0.0.1:8000/mcp/")
        .await
        .unwrap();
    let e = server
        .call(&stub, "no_such_tool", json!({}))
        .await
        .unwrap_err();
    assert!(e.contains("unknown tool"), "{e}");
}

#[tokio::test]
async fn the_kit_task_sees_one_direct_function() {
    let stub = Stub::default();
    let server = Server::connect(&stub, "apps", "http://127.0.0.1:8000/mcp/")
        .await
        .unwrap();
    let catalog = Catalog::new(
        vec![server],
        &["bash", "read_file", "write_file", "edit_file"],
    );
    assert!(catalog.is_direct());
    assert_eq!(
        catalog.functions()[0]["function"]["name"],
        "apps__secret_word"
    );
}
