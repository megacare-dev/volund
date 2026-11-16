//! A scripted stand-in for an OpenAI-compatible model. It is a test double: it lets the harness's
//! protocol, streaming, retry and cancel paths run end to end (in `cargo test`, and in `kit check`
//! before a paid model is set up) without calling a model. It is never part of the submitted entry.
//!
//! A script picks its turns by a phrase of the task (the first user message) and plays turn `n`
//! when the conversation holds `n` assistant messages:
//!
//! ```json
//! {
//!   "scripts": [
//!     {"when": "hello.txt", "turns": [
//!       {"calls": [{"name": "write_file", "arguments": {"path": "hello.txt", "content": "HELLO\n"}}]},
//!       {"text": "done"}
//!     ]},
//!     {"when": "design memo", "repeat_last": true, "turns": [
//!       {"calls": [{"name": "bash", "arguments": {"command": "sleep 20"}}]}
//!     ]}
//!   ],
//!   "default": [{"text": "OK"}],
//!   "summary": "Wrote memo.md.",
//!   "fail_calls": {"1": 429, "2": "cut"}
//! }
//! ```
//!
//! A request with `tool_choice: "none"` (the harness asking for its final summary) gets `summary`.
//!
//! `{{last_tool_line}}` in a call's arguments becomes the first line of the newest tool result, so a
//! script can pass on what a tool returned (the kit's MCP task hands back a word chosen at run time).
//!
//! `fail_calls` makes the n-th HTTP call (counting from 1) answer with that status, or `"cut"` it:
//! stream part of a reply and close without a finish reason.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Mock {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Mock {
    /// The request bodies received so far, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `http://{addr}/v1`, as `MODEL_BASE_URL` takes it.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }
}

/// Serves `script` on `listener` until the runtime stops.
pub fn start(listener: TcpListener, script: Value) -> Mock {
    let addr = listener
        .local_addr()
        .expect("a bound listener has an address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::new(script);
    let log = requests.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let script = shared.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let _ = handle(stream, &script, &log).await;
            });
        }
    });
    Mock { addr, requests }
}

async fn handle(
    mut stream: TcpStream,
    script: &Value,
    log: &Mutex<Vec<Value>>,
) -> std::io::Result<()> {
    let Some((path, body)) = read_request(&mut stream).await? else {
        return Ok(());
    };
    if !path.ends_with("/chat/completions") {
        return respond(
            &mut stream,
            404,
            "application/json",
            r#"{"error":{"message":"not found"}}"#,
        )
        .await;
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let call_number = {
        let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
        log.push(request.clone());
        log.len()
    };
    let failure = script
        .get("fail_calls")
        .and_then(|f| f.get(call_number.to_string()))
        .cloned();
    if let Some(status) = failure.as_ref().and_then(Value::as_u64) {
        let body = json!({"error": {"message": format!("mock failure {status}")}}).to_string();
        return respond(&mut stream, status as u16, "application/json", &body).await;
    }
    let turn = fill(pick_turn(script, &request), &request);
    if request.get("stream").and_then(Value::as_bool) != Some(true) {
        let body = json!({
            "id": "mock", "object": "chat.completion", "model": request["model"],
            "choices": [{"index": 0, "message": message(&turn, call_number), "finish_reason": finish(&turn)}],
            "usage": usage(),
        });
        return respond(&mut stream, 200, "application/json", &body.to_string()).await;
    }
    let mut events = stream_events(&turn, call_number);
    if failure.as_ref().and_then(Value::as_str) == Some("cut") {
        events.truncate(2);
    }
    let mut out = String::new();
    for event in &events {
        out.push_str("data: ");
        out.push_str(event);
        out.push_str("\n\n");
    }
    respond(&mut stream, 200, "text/event-stream", &out).await
}

/// The turn the script plays for this request. Asked for an answer without tools (`tool_choice:
/// "none"`, the final summary), it answers in words, as a model does: the script's `summary`, or
/// `done`.
fn pick_turn(script: &Value, request: &Value) -> Value {
    if request["tool_choice"] == "none" {
        return json!({"text": script["summary"].as_str().unwrap_or("done")});
    }
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let task = messages
        .iter()
        .find(|m| m["role"] == "user")
        .map(|m| text_of(&m["content"]))
        .unwrap_or_default();
    let played = messages.iter().filter(|m| m["role"] == "assistant").count();
    let chosen = script["scripts"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["when"].as_str().is_some_and(|w| task.contains(w)));
    let (turns, repeat_last) = match chosen {
        Some(s) => (
            s["turns"].clone(),
            s["repeat_last"].as_bool().unwrap_or(false),
        ),
        None => (script["default"].clone(), false),
    };
    let turns = turns.as_array().cloned().unwrap_or_default();
    match turns.get(played) {
        Some(turn) => turn.clone(),
        None if repeat_last && !turns.is_empty() => turns[turns.len() - 1].clone(),
        None => json!({"text": "done"}),
    }
}

/// Replaces `{{last_tool_line}}` in the turn's call arguments.
fn fill(mut turn: Value, request: &Value) -> Value {
    let last_tool = request["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "tool"))
        .map(|m| text_of(&m["content"]))
        .unwrap_or_default();
    let line = last_tool.lines().next().unwrap_or("").trim().to_owned();
    fn walk(v: &mut Value, line: &str) {
        match v {
            Value::String(s) if s.contains("{{last_tool_line}}") => {
                *s = s.replace("{{last_tool_line}}", line)
            }
            Value::Array(items) => items.iter_mut().for_each(|i| walk(i, line)),
            Value::Object(map) => map.values_mut().for_each(|i| walk(i, line)),
            _ => {}
        }
    }
    if let Some(calls) = turn.get_mut("calls") {
        walk(calls, &line);
    }
    turn
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn calls(turn: &Value, call_number: usize) -> Vec<(String, String, String)> {
    turn["calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, c)| {
            (
                format!("call_mock_{call_number}_{i}"),
                c["name"].as_str().unwrap_or("").to_owned(),
                c["arguments"].to_string(),
            )
        })
        .collect()
}

fn finish(turn: &Value) -> &'static str {
    if turn["calls"].as_array().is_some_and(|c| !c.is_empty()) {
        "tool_calls"
    } else {
        "stop"
    }
}

fn usage() -> Value {
    json!({"prompt_tokens": 100, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 50}})
}

fn message(turn: &Value, call_number: usize) -> Value {
    let calls: Vec<Value> = calls(turn, call_number)
        .into_iter()
        .map(|(id, name, args)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}))
        .collect();
    let mut m = json!({"role": "assistant", "content": turn["text"].as_str()});
    if !calls.is_empty() {
        m["tool_calls"] = Value::Array(calls);
    }
    m
}

fn stream_events(turn: &Value, call_number: usize) -> Vec<String> {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "mock", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            .to_string()
    };
    let mut events = vec![chunk(json!({"role": "assistant"}), Value::Null)];
    if let Some(text) = turn["text"].as_str() {
        let mid = text
            .char_indices()
            .nth(text.chars().count() / 2)
            .map_or(text.len(), |(i, _)| i);
        for part in [&text[..mid], &text[mid..]] {
            events.push(chunk(json!({"content": part}), Value::Null));
        }
    }
    for (i, (id, name, args)) in calls(turn, call_number).into_iter().enumerate() {
        events.push(chunk(
            json!({"tool_calls": [{"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": ""}}]}),
            Value::Null,
        ));
        let mid = args
            .char_indices()
            .nth(args.chars().count() / 2)
            .map_or(args.len(), |(i, _)| i);
        for part in [&args[..mid], &args[mid..]] {
            events.push(chunk(
                json!({"tool_calls": [{"index": i, "function": {"arguments": part}}]}),
                Value::Null,
            ));
        }
    }
    events.push(chunk(json!({}), json!(finish(turn))));
    events.push(
        json!({"id": "mock", "object": "chat.completion.chunk", "choices": [], "usage": usage()})
            .to_string(),
    );
    events.push("[DONE]".into());
    events
}

/// Reads one HTTP/1.1 request: its path and body.
async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let length = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(Some((path, body)))
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} MOCK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}
