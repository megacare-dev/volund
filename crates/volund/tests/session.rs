//! Whole tasks over a real WebSocket: a fake evaluator (an in-memory container) on one side, the
//! scripted model on the other.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
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
use volund::ablation::Level;
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

async fn harness(llm: Option<&volund_mock::Mock>, level: Level) -> SocketAddr {
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
        level,
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
    /// The harness's copies of the deliverables, by the directory they were copied to.
    copies: HashMap<String, BTreeMap<String, Vec<u8>>>,
    commands: Vec<String>,
    usage: usize,
    logs: Vec<String>,
    mcp_methods: Vec<String>,
    cancel_sent: Option<Instant>,
    ended: Option<Instant>,
}

/// A 64-hex-digit stand-in for sha256.
fn hex_hash(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish()).repeat(4)
}

/// The single-quoted words of a shell list, as the harness quotes paths.
fn quoted_words(list: &str) -> Vec<String> {
    list.split_whitespace()
        .map(|w| w.trim_matches('\'').to_owned())
        .filter(|w| !w.is_empty())
        .collect()
}

/// The in-memory container's answer to the harness's scripted commands (and `cat` and `rm` from
/// the model); `None` for any other command, which then succeeds with no output. The container
/// starts empty, so every file in it is new since PROBE.
fn scripted(run: &mut Run, command: &str) -> Option<String> {
    if command.contains("### new files") {
        let mut paths: Vec<&String> = run.files.keys().collect();
        paths.sort();
        let listed: String = paths.iter().map(|p| format!("{p}\n")).collect();
        return Some(format!("### new files\n{listed}"));
    }
    if command.contains("### hashes") {
        let mut lines: Vec<String> = run
            .files
            .iter()
            .map(|(p, b)| format!("{}\t{p}\n", hex_hash(b)))
            .collect();
        lines.sort();
        return Some(format!("### hashes\n{}", lines.concat()));
    }
    if command.ends_with("&& echo copied") {
        let dir = command
            .strip_prefix("rm -rf '")?
            .split_once('\'')?
            .0
            .to_owned();
        let list = command.split_once("for p in ")?.1.split_once("; do")?.0;
        let wanted = quoted_words(list);
        let copy = run
            .files
            .iter()
            .filter(|(p, _)| {
                wanted
                    .iter()
                    .any(|w| *p == w || p.starts_with(&format!("{}/", w.trim_end_matches('/'))))
            })
            .map(|(p, b)| (p.clone(), b.clone()))
            .collect();
        run.copies.insert(dir, copy);
        return Some("copied\n".into());
    }
    if let Some(rest) = command.strip_prefix("cp -a '")
        && command.ends_with("&& echo restored")
    {
        let dir = rest.split_once('\'')?.0;
        let copy = run.copies.get(dir)?.clone();
        run.files.extend(copy);
        return Some("restored\n".into());
    }
    if let Some(list) = command.strip_prefix("rm ") {
        for path in list.split_whitespace().filter(|w| !w.starts_with('-')) {
            run.files.remove(path);
        }
        return Some(String::new());
    }
    None
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
                let output = match scripted(&mut run, &command) {
                    Some(output) => output,
                    None => command
                        .strip_prefix("cat ")
                        .and_then(|f| run.files.get(&path(&json!(f))))
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default(),
                };
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
    let addr = harness(Some(&m), Level::A1).await;
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
    let addr = harness(Some(&m), Level::A1).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end, json!({"type": "done", "final_message": "done"}));
    assert_eq!(
        run.files.get("/app/hello.txt").map(Vec::as_slice),
        Some(&b"HELLO\n"[..])
    );
    assert_eq!(run.files.len(), 1, "no other files");
    assert_eq!(run.commands, ["cat hello.txt"]);
    assert_eq!(run.usage, 3, "one usage frame per model call");
    let start = events(&run, "start");
    assert_eq!(start.len(), 1, "{:?}", run.logs);
    assert_eq!(
        (&start[0]["phase"], &start[0]["level"], &start[0]["workdir"]),
        (&json!("LOOP"), &json!("A1"), &json!("/app"))
    );
    assert_eq!(events(&run, "llm").len(), 3, "a log line per model call");

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
    let addr = harness(Some(&m), Level::A1).await;
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
    let addr = harness(Some(&m), Level::A1).await;
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
    let addr = harness(None, Level::A1).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end["type"], "error");
    assert!(run.end["message"].as_str().unwrap().contains("MODEL_NAME"));
}

#[tokio::test]
async fn a_first_frame_that_is_not_a_task_is_refused() {
    let addr = harness(None, Level::A1).await;
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
    let addr = harness(Some(&m), Level::A1).await;
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
    let addr = harness(Some(&m), Level::A1).await;
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

// The phase driver on the same tasks: PROBE, INTAKE, WRITE, CHECK, VERIFY, FINALIZE.

/// The run's JSON log lines of one kind.
fn events(run: &Run, kind: &str) -> Vec<Value> {
    run.logs
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == kind)
        .collect()
}

fn transitions(run: &Run) -> Vec<(String, String)> {
    events(run, "transition")
        .iter()
        .map(|v| {
            (
                v["phase"].as_str().unwrap().to_owned(),
                v["to"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The newest opening of `phase` in any request the model received.
fn opening(m: &volund_mock::Mock, phase: &str) -> Option<String> {
    let start = format!("Phase: {phase}");
    m.requests().iter().rev().find_map(|r| {
        r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find_map(|msg| {
                let text = msg["content"]
                    .as_str()
                    .or_else(|| msg["content"][0]["text"].as_str())?;
                text.starts_with(&start).then(|| text.to_owned())
            })
    })
}

#[tokio::test]
async fn phases_the_kit_smoke_task_runs_every_phase_in_order() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m), Level::A6).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end, json!({"type": "done", "final_message": "done"}));
    assert_eq!(
        run.files.get("/app/hello.txt").map(Vec::as_slice),
        Some(&b"HELLO\n"[..])
    );
    assert_eq!(run.files.len(), 1, "no other files");
    let path: Vec<String> = transitions(&run)
        .into_iter()
        .map(|(from, _)| from)
        .collect();
    assert_eq!(path, ["PROBE", "INTAKE", "WRITE", "CHECK", "VERIFY"]);
    let requests = m.requests();
    let tools = |n: usize| -> Vec<String> {
        requests[n]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(
        tools(0).contains(&"world_deliverable".to_owned()),
        "INTAKE records"
    );
    assert!(!tools(0).contains(&"world_issue".to_owned()));
    let verify = requests
        .iter()
        .position(|r| {
            r["messages"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("checking another engineer"))
        })
        .expect("a fresh VERIFY conversation");
    assert_eq!(
        requests[verify]["messages"].as_array().unwrap().len(),
        3,
        "system, task, phase opening"
    );
    assert_eq!(
        requests.last().unwrap()["tool_choice"],
        "none",
        "FINALIZE asks for the summary"
    );
}

#[tokio::test]
async fn phases_the_kit_mcp_task_works_through_the_driver() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m), Level::A6).await;
    let mut first = task(MCP_TASK, Some(420));
    first["mcp_servers"] =
        json!([{"name": "apps", "url": MCP_URL, "transport": "streamable-http"}]);
    let run = evaluate(addr, first, Behaviour::default()).await;
    assert_eq!(run.end["type"], "done");
    assert_eq!(
        run.files
            .get("/app/mcp.txt")
            .map(|b| String::from_utf8_lossy(b).trim().to_owned()),
        Some(SECRET.to_owned())
    );
    assert!(run.mcp_methods.contains(&"tools/call".to_owned()));
}

#[tokio::test]
async fn phases_cancel_still_gets_done_in_time_with_the_record() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m), Level::A6).await;
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
    let message = run.end["final_message"].as_str().unwrap();
    assert!(
        message.contains("memo.md"),
        "the summary comes from the record: {message}"
    );
    let calls_at_cancel = m.requests().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        m.requests().len(),
        calls_at_cancel,
        "no model call after the cancel"
    );
}

#[tokio::test]
async fn phases_a_missing_deliverable_fails_check_and_fix_writes_it() {
    let m = model(json!({"scripts": [{"when": "lazy", "summary": "Wrote out.md.", "phases": {
        "INTAKE": [
            {"calls": [{"name": "world_deliverable", "arguments": {"path": "out.md"}}]},
            {"text": "Recorded."}
        ],
        "WRITE": [{"text": "All done."}],
        "FIX": [
            {"calls": [{"name": "write_file", "arguments": {"path": "out.md", "content": "# Result\n"}}]},
            {"text": "Wrote the missing file."}
        ],
        "VERIFY": [{"text": "No issues."}]
    }}]}))
    .await;
    let addr = harness(Some(&m), Level::A6).await;
    let run = evaluate(
        addr,
        task("a lazy task: write out.md", Some(600)),
        Behaviour::default(),
    )
    .await;
    assert_eq!(
        run.end,
        json!({"type": "done", "final_message": "Wrote out.md."})
    );
    assert_eq!(
        run.files.get("/app/out.md").map(Vec::as_slice),
        Some(&b"# Result\n"[..])
    );
    let steps = transitions(&run);
    assert!(steps.contains(&("CHECK".into(), "FIX".into())), "{steps:?}");
    assert!(steps.contains(&("FIX".into(), "CHECK".into())), "{steps:?}");
    assert_eq!(
        steps.last().unwrap(),
        &("VERIFY".to_owned(), "FINALIZE".to_owned())
    );
    let fix_opening = m.requests().iter().find_map(|r| {
        r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find_map(|msg| {
                let text = msg["content"]
                    .as_str()
                    .or_else(|| msg["content"][0]["text"].as_str())?;
                text.starts_with("Phase: FIX").then(|| text.to_owned())
            })
    });
    let fix_opening = fix_opening.expect("a FIX opening");
    assert!(
        fix_opening.contains("Failed checks:\n- out.md: missing"),
        "{fix_opening}"
    );
}

#[tokio::test]
async fn phases_without_model_settings_the_task_ends_with_an_error() {
    let addr = harness(None, Level::A6).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end["type"], "error");
}

#[tokio::test]
async fn phases_a_model_that_never_records_moves_on_after_three_tries() {
    let m = model(
        json!({"scripts": [{"when": "forgetful", "summary": "Nothing recorded.", "phases": {
            "INTAKE": [{"text": "I read the task."}],
            "VERIFY": [{"text": "No issues."}]
        }}]}),
    )
    .await;
    let addr = harness(Some(&m), Level::A6).await;
    let run = evaluate(
        addr,
        task("a forgetful task", Some(600)),
        Behaviour::default(),
    )
    .await;
    assert_eq!(run.end["type"], "done");
    let intake_calls = m
        .requests()
        .iter()
        .filter(|r| {
            r["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find_map(|msg| {
                    let text = msg["content"]
                        .as_str()
                        .or_else(|| msg["content"][0]["text"].as_str())?;
                    text.starts_with("Phase: ")
                        .then(|| text.starts_with("Phase: INTAKE"))
                })
                == Some(true)
        })
        .count();
    assert_eq!(intake_calls, 3, "INTAKE re-entered at most three times");
    let steps = transitions(&run);
    assert!(
        steps.contains(&("INTAKE".into(), "WRITE".into())),
        "{steps:?}"
    );
}

#[tokio::test]
async fn phases_a_stray_file_fails_check_and_fix_moves_it() {
    let m = model(json!({"scripts": [{"when": "untidy", "summary": "Wrote out.md.", "phases": {
        "INTAKE": [
            {"calls": [{"name": "world_deliverable", "arguments": {"path": "out.md"}}]},
            {"text": "Recorded."}
        ],
        "WRITE": [
            {"calls": [{"name": "write_file", "arguments": {"path": "notes.txt", "content": "scratch\n"}}]},
            {"calls": [{"name": "write_file", "arguments": {"path": "out.md", "content": "# Result\n"}}]},
            {"text": "Wrote out.md."}
        ],
        "FIX": [
            {"calls": [{"name": "bash", "arguments": {"command": "rm /app/notes.txt"}}]},
            {"text": "Removed the scratch file."}
        ],
        "VERIFY": [{"text": "No issues."}]
    }}]}))
    .await;
    let addr = harness(Some(&m), Level::A6).await;
    let run = evaluate(
        addr,
        task("an untidy task: write out.md", Some(600)),
        Behaviour::default(),
    )
    .await;
    assert_eq!(run.end["type"], "done");
    let mut files: Vec<&String> = run.files.keys().collect();
    files.sort();
    assert_eq!(files, ["/app/out.md"], "the scratch file is gone");
    let fix = opening(&m, "FIX").expect("a FIX opening");
    assert!(
        fix.contains(
            "- no stray files in the task's folders: 1 file(s) added that are not recorded deliverables: /app/notes.txt."
        ),
        "{fix}"
    );
    let steps = transitions(&run);
    assert_eq!(
        steps.last().unwrap(),
        &("VERIFY".to_owned(), "FINALIZE".to_owned()),
        "{steps:?}"
    );
}

#[tokio::test]
async fn phases_a_deliverable_verify_deletes_is_restored() {
    let m = model(json!({"scripts": [{"when": "careless", "summary": "Wrote out.md.", "phases": {
        "INTAKE": [
            {"calls": [{"name": "world_deliverable", "arguments": {"path": "out.md"}}]},
            {"text": "Recorded."}
        ],
        "WRITE": [
            {"calls": [{"name": "write_file", "arguments": {"path": "out.md", "content": "# Result\n"}}]},
            {"text": "Wrote out.md."}
        ],
        "VERIFY": [
            {"calls": [{"name": "bash", "arguments": {"command": "rm /app/out.md"}}]},
            {"text": "Checked."}
        ]
    }}]}))
    .await;
    let addr = harness(Some(&m), Level::A6).await;
    let run = evaluate(
        addr,
        task("a careless verifier: write out.md", Some(600)),
        Behaviour::default(),
    )
    .await;
    assert_eq!(run.end["type"], "done");
    assert_eq!(
        run.files.get("/app/out.md").map(Vec::as_slice),
        Some(&b"# Result\n"[..]),
        "restored from the copy made before VERIFY"
    );
    let changed = events(&run, "verify_changed");
    assert_eq!(
        changed,
        [
            json!({"t": changed[0]["t"], "phase": "VERIFY", "kind": "verify_changed",
                "changed": ["/app/out.md"], "restored": true})
        ]
    );
    assert!(
        run.commands
            .iter()
            .any(|c| c == "cp -a '/tmp/volund/before-verify'/. / && echo restored")
    );
    assert_eq!(
        transitions(&run).last().unwrap(),
        &("VERIFY".to_owned(), "FINALIZE".to_owned()),
        "a restored change does not hold the task"
    );
}

#[tokio::test]
async fn level_a4_finishes_through_the_finish_gate_without_verify() {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m), Level::A4).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    assert_eq!(run.end, json!({"type": "done", "final_message": "done"}));
    let steps = transitions(&run);
    let path: Vec<&str> = steps.iter().map(|(from, _)| from.as_str()).collect();
    assert_eq!(path, ["PROBE", "INTAKE", "WRITE", "CHECK"]);
    let finish = events(&run, "transition").pop().unwrap();
    assert_eq!(
        (&finish["to"], &finish["reason"]),
        (
            &json!("FINALIZE"),
            &json!("checks pass; nothing blocks finishing")
        )
    );
    assert_eq!(events(&run, "start")[0]["level"], "A4");
    assert!(
        !m.requests()
            .iter()
            .any(|r| r["messages"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("checking another engineer"))),
        "no VERIFY conversation"
    );
}

#[tokio::test]
async fn level_a4_writes_without_modelling_the_constraints() {
    let m = model(json!({"scripts": [{"when": "limit", "summary": "Done.", "phases": {
        "INTAKE": [
            {"calls": [
                {"name": "world_deliverable", "arguments": {"path": "calc.md"}},
                {"name": "world_constraint", "arguments": {"id": "C1", "text": "drop <= 3%", "severity": "hard", "source_file": "spec.pdf"}}
            ]},
            {"text": "Recorded."}
        ],
        "WRITE": [{"text": "Nothing written."}]
    }}]}))
    .await;
    let addr = harness(Some(&m), Level::A4).await;
    let run = evaluate(
        addr,
        task("a limit to meet", Some(600)),
        Behaviour::default(),
    )
    .await;
    assert_eq!(run.end["type"], "done");
    let intake = events(&run, "transition")
        .into_iter()
        .find(|t| t["phase"] == "INTAKE")
        .unwrap();
    assert_eq!(
        (&intake["to"], &intake["reason"]),
        (
            &json!("WRITE"),
            &json!("constraints recorded; MODEL is off")
        )
    );
}

/// The first request's messages and offered tool names, at `level`.
async fn loop_opening(level: Level) -> (Vec<Value>, Vec<String>, Run) {
    let m = model(kit_script()).await;
    let addr = harness(Some(&m), level).await;
    let run = evaluate(addr, task(SMOKE, Some(600)), Behaviour::default()).await;
    let first = m.requests()[0].clone();
    let tools = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    (first["messages"].as_array().unwrap().clone(), tools, run)
}

fn text(message: &Value) -> String {
    message["content"]
        .as_str()
        .or_else(|| message["content"][0]["text"].as_str())
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn levels_a1_to_a3_add_the_probe_then_the_world_to_the_loop() {
    let (a1, a1_tools, a1_run) = loop_opening(Level::A1).await;
    assert_eq!(a1.len(), 2, "system and task");
    assert!(!text(&a1[0]).contains("Keep a record"));
    assert!(!a1_tools.iter().any(|t| t.starts_with("world_")));
    assert!(events(&a1_run, "probe").is_empty());

    let (a2, a2_tools, a2_run) = loop_opening(Level::A2).await;
    assert_eq!(text(&a1[0]), text(&a2[0]), "the same system prompt");
    assert!(
        text(&a2[2]).starts_with(
            "Before you started, the harness looked around the container.\nContainer: "
        ),
        "{}",
        text(&a2[2])
    );
    assert_eq!(a1_tools, a2_tools);
    assert_eq!(events(&a2_run, "probe").len(), 1);
    assert!(a2_run.commands[0].starts_with("tab="), "PROBE runs first");

    let (a3, a3_tools, a3_run) = loop_opening(Level::A3).await;
    assert!(text(&a3[0]).contains("Keep a record of the task with the world_* tools"));
    assert!(
        text(&a3[2]).starts_with(
            "The record so far (the world_* tools add to it):\nDeliverables:\n- (none recorded yet)"
        ),
        "{}",
        text(&a3[2])
    );
    assert!(a3_tools.contains(&"world_deliverable".to_owned()));
    assert_eq!(a3_run.end, json!({"type": "done", "final_message": "done"}));
}

#[tokio::test]
async fn level_a3_sends_the_record_when_cancelled() {
    let m = model(json!({"scripts": [{"when": "design memo", "repeat_last": true, "turns": [
        {"calls": [{"name": "world_deliverable", "arguments": {"path": "memo.md", "status": "written"}}]},
        {"calls": [{"name": "write_file", "arguments": {"path": "memo.md", "content": "# Draft\n"}}]},
        {"calls": [{"name": "bash", "arguments": {"command": "sleep 20"}}]}
    ]}]}))
    .await;
    let addr = harness(Some(&m), Level::A3).await;
    let how = Behaviour {
        cancel_after_sleep: Some(Duration::from_millis(300)),
        ..Behaviour::default()
    };
    let run = evaluate(addr, task(LONG, Some(21_600)), how).await;
    assert_eq!(run.end["type"], "done");
    assert_eq!(
        run.end["final_message"], "Deliverables:\n- memo.md (written)",
        "the summary comes from the record"
    );
}
