//! The client against the scripted model: retries, refusals and deadlines.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::Instant;
use volund_llm::{Call, Client, Config, Error, ToolChoice};

async fn mock(script: Value) -> volund_mock::Mock {
    volund_mock::start(TcpListener::bind("127.0.0.1:0").await.unwrap(), script)
}

fn client(base_url: String, model: &str) -> Client {
    let mut cfg = Config::from_vars(|name| match name {
        "MODEL_NAME" => Some(model.into()),
        "MODEL_BASE_URL" => Some(base_url.clone()),
        "MODEL_API_KEY" => Some("test-key".into()),
        _ => None,
    })
    .unwrap();
    cfg.retry_waits = vec![Duration::from_millis(10); 5];
    Client::new(cfg).unwrap()
}

fn task(text: &str) -> Vec<Value> {
    vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": text}),
    ]
}

fn call(messages: &[Value]) -> Call<'_> {
    Call {
        messages,
        tools: &[],
        tool_choice: ToolChoice::Auto,
        deadline: None,
    }
}

fn recorder() -> (Arc<Mutex<Vec<String>>>, impl Fn(String) + Send + Sync) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    (lines, move |line| sink.lock().unwrap().push(line))
}

#[tokio::test]
async fn rate_limits_and_cut_streams_are_retried_without_keeping_the_partial_reply() {
    let m = mock(json!({
        "default": [{"text": "the answer"}],
        "fail_calls": {"1": 429, "2": "cut", "3": 503}
    }))
    .await;
    let (lines, log) = recorder();
    let messages = task("anything");
    let reply = client(m.base_url(), "gemini-4-argon")
        .complete(call(&messages), &log)
        .await
        .unwrap();
    assert_eq!(reply.text, "the answer", "no text from the cut stream");
    assert_eq!(reply.finish.as_deref(), Some("stop"));
    assert_eq!(reply.usage.unwrap().input_tokens, 100);
    assert_eq!(m.requests().len(), 4);
    assert_eq!(
        lines.lock().unwrap().len(),
        3,
        "{:?}",
        lines.lock().unwrap()
    );
}

#[tokio::test]
async fn a_bad_request_is_not_sent_again_unchanged() {
    let m = mock(json!({"default": [{"text": "x"}], "fail_calls": {"1": 400}})).await;
    let (_, log) = recorder();
    let messages = task("anything");
    let result = client(m.base_url(), "gemini-4-argon")
        .complete(call(&messages), &log)
        .await;
    assert!(matches!(result, Err(Error::BadRequest(_))), "{result:?}");
    assert_eq!(m.requests().len(), 1);
}

#[tokio::test]
async fn claude_requests_stream_with_effort_and_cache_marks_and_no_sampling_parameters() {
    let m = mock(json!({"default": [{"text": "ok"}]})).await;
    let messages = task("anything");
    let (_, log) = recorder();
    let c = client(m.base_url(), "claude-opus-5-5");
    c.complete(call(&messages), &log).await.unwrap();
    let sent = &m.requests()[0];
    assert_eq!(sent["reasoning_effort"], "max");
    assert_eq!(sent["max_completion_tokens"], 32_000);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(
        sent["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(sent.get("temperature").is_none() && sent.get("top_p").is_none());
}

#[tokio::test]
async fn forbidden_fails_at_once() {
    let m = mock(json!({"default": [{"text": "x"}], "fail_calls": {"1": 403}})).await;
    let (_, log) = recorder();
    let messages = task("anything");
    let result = client(m.base_url(), "gemini-4-argon")
        .complete(call(&messages), &log)
        .await;
    assert!(
        matches!(result, Err(Error::Failed(ref e)) if e.contains("403")),
        "{result:?}"
    );
    assert_eq!(m.requests().len(), 1);
}

#[tokio::test]
async fn no_retry_waits_past_the_deadline() {
    let m = mock(json!({"default": [{"text": "x"}], "fail_calls": {"1": 500}})).await;
    let mut cfg = Config::from_vars(|name| match name {
        "MODEL_NAME" => Some("glm-5.2".into()),
        "MODEL_BASE_URL" => Some(m.base_url()),
        _ => None,
    })
    .unwrap();
    cfg.retry_waits = vec![Duration::from_secs(30)];
    let c = Client::new(cfg).unwrap();
    let (_, log) = recorder();
    let messages = task("anything");
    let started = Instant::now();
    let result = c
        .complete(
            Call {
                deadline: Some(Instant::now() + Duration::from_secs(5)),
                ..call(&messages)
            },
            &log,
        )
        .await;
    assert!(matches!(result, Err(Error::OutOfTime(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn tool_calls_stream_back_whole() {
    let m = mock(json!({"scripts": [{"when": "files", "turns": [
        {"calls": [
            {"name": "bash", "arguments": {"command": "ls -la /app"}},
            {"name": "read_file", "arguments": {"path": "/app/ข้อมูล.csv"}}
        ]}
    ]}]}))
    .await;
    let (_, log) = recorder();
    let messages = task("list the files");
    let mut reply = client(m.base_url(), "gpt-oss-120b")
        .complete(call(&messages), &log)
        .await
        .unwrap();
    reply.finalize(0);
    let args = reply.parse_calls();
    assert_eq!(reply.finish.as_deref(), Some("tool_calls"));
    assert_eq!(args[0].as_ref().unwrap()["command"], "ls -la /app");
    assert_eq!(args[1].as_ref().unwrap()["path"], "/app/ข้อมูล.csv");
}
