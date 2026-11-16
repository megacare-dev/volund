//! Whole tasks over a real WebSocket: a fake evaluator (an in-memory container) on one side, the
//! scripted model on the other.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use volund::server::{Settings, serve};

const KIT_SCRIPT: &str = include_str!("../../volund-mock/scripts/kit-check.json");
/// The smoke and cancel tasks of `kit check` (kit/check.py), word for word.
const SMOKE: &str = "Create a file named `hello.txt` in the current working directory (`/app`) whose entire \
content is the single line:\n\nHELLO\n\nDo not create any other files. When the file exists, reply with \
the word done and stop.";
const LONG: &str = "Write a thorough design memo, at least 3000 words, on sizing the cooling plant of a 10 \
MW data hall, into `/app/memo.md`. Work in several steps and check your numbers with shell commands as \
you go.";

async fn model(script: Value) -> volund_mock::Mock {
    volund_mock::start(TcpListener::bind("127.0.0.1:0").await.unwrap(), script)
}

async fn harness(llm: Option<&volund_mock::Mock>) -> SocketAddr {
    let llm = llm.map(|m| {
        let mut cfg = volund_llm::Config::from_vars(|name| match name {
            "MODEL_NAME" => Some("claude-opus-5-5".into()),
            "MODEL_BASE_URL" => Some(m.base_url()),
            _ => None,
        })
        .unwrap();
        cfg.retry_waits = vec![Duration::from_millis(10); 5];
        Arc::new(volund_llm::Client::new(cfg).unwrap())
    });
    let settings = Settings {
        llm,
        llm_problem: "the model is not configured: MODEL_NAME is not set".into(),
        compact_at: 150_000,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, settings));
    addr
}

fn task(instruction: &str, timeout_sec: Option<u64>) -> Value {
    json!({
        "type": "task", "protocol": 1, "session": "test", "instruction": instruction, "workdir": "/app",
        "limits": {"agent_timeout_sec": timeout_sec, "exec_timeout_sec": 600,
                   "max_output_bytes": 100000, "max_write_bytes": 10000000},
        "mcp_servers": []
    })
}

#[derive(Default)]
struct Behaviour {
    /// Reply to a `sleep` command after this long; `None` leaves it running.
    sleep_reply_after: Option<Duration>,
    /// Send `cancel` this long after the first `sleep` command arrives.
    cancel_after_sleep: Option<Duration>,
}

#[derive(Default)]
struct Run {
    end: Value,
    files: HashMap<String, Vec<u8>>,
    commands: Vec<String>,
    usage: usize,
    logs: Vec<String>,
    mcp_methods: Vec<String>,
    cancel_sent: Option<Instant>,
    ended: Option<Instant>,
}

/// Plays the evaluator for one task: answers requests from an in-memory container until `done` or
/// `error`.
async fn evaluate(addr: SocketAddr, first: Value, how: Behaviour) -> Run {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .unwrap();
    let (mut sink, mut source) = ws.split();
    sink.send(Message::text(first.to_string())).await.unwrap();
    let (late_tx, mut late) = mpsc::unbounded_channel::<Value>();
    let mut run = Run::default();
    let mut cancel_at: Option<Instant> = None;
    let mut running: Vec<String> = Vec::new();
    let far = Instant::now() + Duration::from_secs(3600);
    let give_up = Instant::now() + Duration::from_secs(30);
    loop {
        let frame: Value = tokio::select! {
            message = source.next() => match message {
                Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
                Some(Ok(_)) => continue,
                _ => panic!("the harness closed the socket without done or error"),
            },
            Some(reply) = late.recv() => {
                running.retain(|id| id != reply["id"].as_str().unwrap());
                sink.send(Message::text(reply.to_string())).await.unwrap();
                continue;
            }
            () = sleep_until(cancel_at.unwrap_or(far)) => {
                cancel_at = None;
                run.cancel_sent = Some(Instant::now());
                sink.send(Message::text(json!({"type": "cancel", "reason": "timeout"}).to_string())).await.unwrap();
                for id in running.drain(..) {
                    let reply = json!({"type": "result", "id": id, "ok": false, "error": "cancelled: the time is up"});
                    sink.send(Message::text(reply.to_string())).await.unwrap();
                }
                continue;
            }
            () = sleep_until(give_up) => panic!("no done within 30 s"),
        };
        let id = frame["id"].as_str().unwrap_or("").to_owned();
        let path = |p: &Value| {
            let p = p.as_str().unwrap();
            if p.starts_with('/') {
                p.to_owned()
            } else {
                format!("/app/{p}")
            }
        };
        let reply = match frame["type"].as_str().unwrap() {
            "exec" => {
                let command = frame["command"].as_str().unwrap().to_owned();
                run.commands.push(command.clone());
                if command.contains("sleep") {
                    if run.cancel_sent.is_none()
                        && cancel_at.is_none()
                        && let Some(after) = how.cancel_after_sleep
                    {
                        cancel_at = Some(Instant::now() + after);
                    }
                    running.push(id.clone());
                    if let Some(after) = how.sleep_reply_after {
                        let tx = late_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(after).await;
                            let _ = tx.send(json!({"type": "result", "id": id, "exit_code": 0,
                                "output": "slept", "truncated": false, "timed_out": false}));
                        });
                    }
                    continue;
                }
                let output = command
                    .strip_prefix("cat ")
                    .and_then(|f| run.files.get(&path(&json!(f))))
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                json!({"type": "result", "id": id, "exit_code": 0, "output": output,
                       "truncated": false, "timed_out": false})
            }
            "write_file" => {
                let bytes = B64.decode(frame["content_b64"].as_str().unwrap()).unwrap();
                let file = run.files.entry(path(&frame["path"])).or_default();
                if frame["append"] != true {
                    file.clear();
                }
                file.extend(bytes);
                json!({"type": "result", "id": id, "ok": true, "size": file.len()})
            }
            "read_file" => match run.files.get(&path(&frame["path"])) {
                Some(b) => {
                    json!({"type": "result", "id": id, "ok": true, "content_b64": B64.encode(b),
                                  "size": b.len(), "truncated": false})
                }
                None => json!({"type": "result", "id": id, "ok": false, "error": "no such file"}),
            },
            "http" => {
                assert_eq!(frame["url"], MCP_URL);
                let body: Value = serde_json::from_slice(
                    &B64.decode(frame["body_b64"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
                run.mcp_methods
                    .push(body["method"].as_str().unwrap().to_owned());
                let answer = mcp_stub(&body);
                let kind = if answer.is_empty() {
                    "application/json"
                } else {
                    "text/event-stream"
                };
                json!({"type": "result", "id": id, "ok": true, "status": if answer.is_empty() { 202 } else { 200 },
                       "headers": {"Content-Type": kind, "Mcp-Session-Id": "plebench-check"},
                       "body_b64": B64.encode(answer), "truncated": false})
            }
            "log" => {
                run.logs.push(frame["text"].as_str().unwrap().to_owned());
                continue;
            }
            "usage" => {
                run.usage += 1;
                continue;
            }
            "done" | "error" => {
                run.ended = Some(Instant::now());
                run.end = frame;
                return run;
            }
            other => panic!("unexpected frame type {other}"),
        };
        sink.send(Message::text(reply.to_string())).await.unwrap();
    }
}

fn kit_script() -> Value {
    serde_json::from_str(KIT_SCRIPT).unwrap()
}

const MCP_URL: &str = "http://127.0.0.1:8000/mcp/";
/// The kit's MCP task (kit/check.py), word for word.
const MCP_TASK: &str = "The task's MCP server `apps` has a tool named `secret_word`. Call it, then create a \
file named `mcp.txt` in the current working directory (`/app`) whose entire content is the single word the \
tool returns. Do not create any other files. When the file exists, reply with the word done and stop.";
const SECRET: &str = "MCP-7E57AB12";

/// Answers like the kit's MCP stub: server-sent events, `202` (empty) for notifications.
fn mcp_stub(message: &Value) -> String {
    let Some(id) = message.get("id") else {
        return String::new();
    };
    let result = match message["method"].as_str().unwrap() {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                               "serverInfo": {"name": "apps", "version": "1"}}),
        "tools/list" => {
            json!({"tools": [{"name": "secret_word", "description": "Returns the secret word.",
                                          "inputSchema": {"type": "object", "properties": {}}}]})
        }
        "tools/call" => json!({"content": [{"type": "text", "text": SECRET}], "isError": false}),
        other => panic!("unexpected MCP method {other}"),
    };
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
    format!("event: message\ndata: {reply}\n\n")
}

#[tokio::test]
async fn the_kit_mcp_task_gets_its_word_through_http() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m)).await;
    let mut first = task(MCP_TASK, Some(420));
    first["mcp_servers"] =
        json!([{"name": "apps", "url": MCP_URL, "transport": "streamable-http"}]);
    let run = evaluate(addr, first, Behaviour::default()).await;
    assert_eq!(run.end, json!({"type": "done", "final_message": "done"}));
    assert_eq!(
        run.files
            .get("/app/mcp.txt")
            .map(|b| String::from_utf8_lossy(b).trim().to_owned()),
        Some(SECRET.to_owned())
    );
    assert_eq!(
        run.mcp_methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    let offered: Vec<String> = m.requests()[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        offered,
        [
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "apps__secret_word"
        ]
    );
    assert!(
        run.logs.iter().any(|l| l == "mcp apps: 1 tools"),
        "{:?}",
        run.logs
    );
}

#[tokio::test]
async fn the_kit_smoke_task_writes_hello_and_reports_done() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m)).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end, json!({"type": "done", "final_message": "done"}));
    assert_eq!(
        run.files.get("/app/hello.txt").map(Vec::as_slice),
        Some(&b"HELLO\n"[..])
    );
    assert_eq!(run.files.len(), 1, "no other files");
    assert_eq!(run.commands, ["cat hello.txt"]);
    assert_eq!(run.usage, 3, "one usage frame per model call");
    assert!(
        run.logs
            .iter()
            .any(|l| l.starts_with("task test: workdir /app")),
        "{:?}",
        run.logs
    );

    let requests = m.requests();
    assert_eq!(requests.len(), 3);
    let seen = |n: usize| {
        requests[n]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone()
    };
    let first_result = seen(1)["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(
        first_result.starts_with("hello.txt now holds 6 bytes"),
        "{first_result}"
    );
    assert!(first_result.contains("minutes left]"), "{first_result}");
    assert!(
        seen(2)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("HELLO")
    );
    assert_eq!(
        requests[1]["messages"][2]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
}

#[tokio::test]
async fn cancel_gets_done_within_the_grace_period_and_stops_model_calls() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m)).await;
    let how = Behaviour {
        cancel_after_sleep: Some(Duration::from_millis(500)),
        ..Behaviour::default()
    };
    let run = evaluate(addr, task(LONG, Some(21_600)), how).await;
    let waited = run.ended.unwrap() - run.cancel_sent.expect("cancel was sent");
    assert_eq!(run.end["type"], "done");
    assert!(
        waited < Duration::from_secs(3),
        "done {waited:?} after cancel"
    );
    assert!(!run.end["final_message"].as_str().unwrap().is_empty());
    assert_eq!(m.requests().len(), 2, "no model call after the cancel");
    assert!(
        run.logs
            .iter()
            .any(|l| l.contains("cancel received (timeout)"))
    );
}

#[tokio::test]
async fn near_the_deadline_it_asks_for_a_summary_without_tools() {
    let m = model(json!({
        "scripts": [{"when": "slow", "repeat_last": true, "turns": [
            {"calls": [{"name": "bash", "arguments": {"command": "sleep 1"}}]}
        ]}],
        "summary": "Summary from the model."
    }))
    .await;
    let addr = harness(Some(&m)).await;
    let how = Behaviour {
        sleep_reply_after: Some(Duration::from_millis(300)),
        ..Behaviour::default()
    };
    let started = Instant::now();
    let run = evaluate(addr, task("a slow task", Some(4)), how).await;
    assert_eq!(
        run.end,
        json!({"type": "done", "final_message": "Summary from the model."})
    );
    assert!(
        run.ended.unwrap() - started < Duration::from_secs(4),
        "before the deadline"
    );
    let requests = m.requests();
    assert_eq!(requests.last().unwrap()["tool_choice"], "none");
    assert!(requests.len() >= 3, "it worked before wrapping up");
}

#[tokio::test]
async fn without_model_settings_the_task_ends_with_an_error() {
    let addr = harness(None).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end["type"], "error");
    assert!(run.end["message"].as_str().unwrap().contains("MODEL_NAME"));
}

#[tokio::test]
async fn a_first_frame_that_is_not_a_task_is_refused() {
    let addr = harness(None).await;
    let run = evaluate(addr, json!({"type": "cancel"}), Behaviour::default()).await;
    assert_eq!(
        run.end,
        json!({"type": "error", "message": "expected a task message first"})
    );
}

#[tokio::test]
async fn a_malformed_request_from_the_model_never_reaches_the_evaluator() {
    let m = model(json!({"scripts": [{"when": "empty", "turns": [
        {"calls": [{"name": "bash", "arguments": {"command": ""}}]},
        {"text": "Nothing to do."}
    ]}]}))
    .await;
    let addr = harness(Some(&m)).await;
    let run = evaluate(
        addr,
        task("run an empty command", None),
        Behaviour::default(),
    )
    .await;
    assert!(run.commands.is_empty(), "the empty command was not sent");
    assert_eq!(run.end["final_message"], "Nothing to do.");
    let tool_result = m.requests()[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        tool_result.starts_with("error: not sent: command is empty"),
        "{tool_result}"
    );
}

#[tokio::test]
async fn a_connection_that_sends_nothing_is_closed_quietly() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m)).await;
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .unwrap();
    drop(ws);
    // the server keeps serving
    let run = timeout(
        Duration::from_secs(10),
        evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()),
    )
    .await
    .unwrap();
    assert_eq!(run.end["type"], "done");
}
