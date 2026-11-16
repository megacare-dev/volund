//! Streaming client for OpenAI Chat Completions, as the PLE model gateway and `kit/model_proxy` serve
//! it (`docs/models.md` in google-research/ple).
//!
//! - Every call streams: the egress proxy closes a connection that moves no bytes for 30 minutes,
//!   and reasoning models can think silently for minutes.
//! - Timeouts, `408`/`409`/`429`/`5xx`, dropped connections, mid-stream errors and streams that end
//!   without a finish reason are retried with backoff; the partial reply is thrown away.
//! - A `400` is never resent unchanged: the request is adjusted first (`max_tokens` in place of
//!   `max_completion_tokens`, no `reasoning_effort`, fewer output tokens) or the error is returned.
//! - Messages stay `serde_json::Value`, so provider fields survive the round trip ([`reply`]).

pub mod reply;
mod sse;

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::{Instant, timeout, timeout_at};

pub use reply::{Reply, ToolCall, Usage, visible};

/// Waits between retries of a failed call (then it gives up).
pub const RETRY_WAITS: [u64; 5] = [5, 15, 30, 60, 120];
/// The fewest output tokens a call asks for.
pub const MIN_OUTPUT: u32 = 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// Ends in `/v1`; requests go to `{base_url}/chat/completions`.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// `reasoning_effort` sent with every call; `None` sends none (the gateway then uses the model's
    /// highest level).
    pub effort: Option<String>,
    pub max_output_tokens: u32,
    /// Claude caches only at explicit breakpoints: mark the system prompt and the newest message.
    pub cache_marks: bool,
    /// Longest silence tolerated on a connection before the call counts as dropped.
    pub idle_timeout: Duration,
    pub retry_waits: Vec<Duration>,
}

impl Config {
    /// Reads the variables the evaluator sets (`MODEL_NAME`, `MODEL_BASE_URL`, `MODEL_API_KEY`, with
    /// the `OPENAI_*` twins as fallback) and the entry's own settings (`REASONING_EFFORT`,
    /// `MAX_OUTPUT_TOKENS`).
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let model = var("MODEL_NAME").ok_or("MODEL_NAME is not set")?;
        let base_url = var("MODEL_BASE_URL")
            .or_else(|| var("OPENAI_BASE_URL"))
            .ok_or("MODEL_BASE_URL is not set")?
            .trim_end_matches('/')
            .to_owned();
        let api_key = var("MODEL_API_KEY")
            .or_else(|| var("OPENAI_API_KEY"))
            .unwrap_or_else(|| "none".into());
        let effort = match var("REASONING_EFFORT").as_deref() {
            None => Some(highest_effort(&model).to_owned()),
            Some("omit") => None,
            Some(level) => Some(level.to_owned()),
        };
        let max_output_tokens = match var("MAX_OUTPUT_TOKENS") {
            None => 32_000,
            Some(n) => n
                .parse()
                .map_err(|_| format!("MAX_OUTPUT_TOKENS is not a number: {n}"))?,
        };
        Ok(Self {
            cache_marks: model.to_lowercase().contains("claude"),
            base_url,
            api_key,
            model,
            effort,
            max_output_tokens,
            idle_timeout: Duration::from_secs(25 * 60),
            retry_waits: RETRY_WAITS
                .iter()
                .map(|&s| Duration::from_secs(s))
                .collect(),
        })
    }
}

/// The highest reasoning level a model takes: `max` for Claude and GLM, `high` for the others
/// (Gemini refuses `max`, and Argon reasons less at `MAX` than at `high`).
pub fn highest_effort(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("claude") || m.contains("glm") {
        "max"
    } else {
        "high"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    /// Ask for an answer in words (the final summary). Claude refuses forced tool choices, so these
    /// two are the only ones.
    None,
}

pub struct Call<'a> {
    pub messages: &'a [Value],
    pub tools: &'a [Value],
    pub tool_choice: ToolChoice,
    /// The call must end by then: the stream is cut and no retry waits past it.
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The deadline came while the reply was streaming; holds the text streamed so far.
    CutOff { partial: String },
    /// A failure worth retrying, with no time left to wait.
    OutOfTime(String),
    /// The conversation does not fit the model's context window.
    ContextFull(String),
    /// A `400` that no adjustment fixed.
    BadRequest(String),
    /// Anything else: `401`, `403` (a model other than the declared one), `404`, retries spent.
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CutOff { .. } => write!(f, "the reply was cut off at the deadline"),
            Self::OutOfTime(e) => write!(f, "no time left to retry: {e}"),
            Self::ContextFull(e) => write!(f, "the conversation does not fit: {e}"),
            Self::BadRequest(e) => write!(f, "the model refused the request: {e}"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// What went wrong in one attempt.
#[derive(Debug)]
enum Attempt {
    CutOff,
    Status(u16, String),
    /// A bad request reported inside the stream (OpenAI streams carry `type`/`code`, not a status).
    StreamBad(String),
    Transient(String),
}

/// Request settings learned from the endpoint's refusals.
#[derive(Debug)]
struct Adapt {
    token_param: &'static str,
    send_effort: bool,
    max_output: u32,
}

pub struct Client {
    http: reqwest::Client,
    cfg: Config,
    adapt: Mutex<Adapt>,
}

impl Client {
    pub fn new(cfg: Config) -> Result<Self, String> {
        // reqwest is built without a crypto provider; ring needs only a C compiler to build.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("could not build the HTTP client: {e}"))?;
        let adapt = Mutex::new(Adapt {
            token_param: "max_completion_tokens",
            send_effort: cfg.effort.is_some(),
            max_output: cfg.max_output_tokens,
        });
        Ok(Self { http, cfg, adapt })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Streams one completion, retrying and adjusting as the module docs describe. `log` receives a
    /// line for every retry or adjustment.
    pub async fn complete(
        &self,
        call: Call<'_>,
        log: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Reply, Error> {
        let mut waits = self.cfg.retry_waits.iter();
        let mut adjustments = 0;
        loop {
            let mut reply = Reply::default();
            let failure = match self.attempt(&call, &mut reply).await {
                Ok(()) => return Ok(reply),
                Err(Attempt::CutOff) => {
                    return Err(Error::CutOff {
                        partial: reply.text,
                    });
                }
                Err(Attempt::Status(400, body) | Attempt::StreamBad(body)) => {
                    let body = clip(&body, 2000);
                    if adjustments < 4
                        && let Some(change) = self.remedy(&body)
                    {
                        adjustments += 1;
                        log(format!(
                            "model refused the request ({}); {change}",
                            clip(&body, 300)
                        ));
                        continue;
                    }
                    if context_full(&body) {
                        return Err(Error::ContextFull(body));
                    }
                    return Err(Error::BadRequest(body));
                }
                Err(Attempt::Status(code, body)) if retryable(code) => {
                    format!("HTTP {code}: {}", clip(&body, 300))
                }
                Err(Attempt::Status(code, body)) => {
                    return Err(Error::Failed(format!("HTTP {code}: {}", clip(&body, 2000))));
                }
                Err(Attempt::Transient(e)) => e,
            };
            let Some(&wait) = waits.next() else {
                return Err(Error::Failed(format!("gave up after retries: {failure}")));
            };
            if call.deadline.is_some_and(|d| Instant::now() + wait >= d) {
                log(format!(
                    "model call failed ({failure}); no time left to wait {}s",
                    wait.as_secs()
                ));
                return Err(Error::OutOfTime(failure));
            }
            log(format!(
                "model call failed ({failure}); again in {}s",
                wait.as_secs()
            ));
            tokio::time::sleep(wait).await;
        }
    }

    async fn attempt(&self, call: &Call<'_>, reply: &mut Reply) -> Result<(), Attempt> {
        let body = self.body(call);
        let idle = self.cfg.idle_timeout;
        let work = async {
            let response = timeout(
                idle,
                self.http
                    .post(format!("{}/chat/completions", self.cfg.base_url))
                    .bearer_auth(&self.cfg.api_key)
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .body(body)
                    .send(),
            )
            .await
            .map_err(|_| Attempt::Transient("no response within the idle timeout".into()))?
            .map_err(|e| Attempt::Transient(format!("the request failed: {e}")))?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                let text = timeout(Duration::from_secs(60), response.text())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                return Err(Attempt::Status(status, text));
            }
            let mut response = response;
            let mut decoder = sse::Decoder::default();
            loop {
                let chunk = match timeout(idle, response.chunk()).await {
                    Err(_) => return Err(Attempt::Transient("the stream went silent".into())),
                    Ok(Err(e)) => return Err(Attempt::Transient(format!("the stream broke: {e}"))),
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(bytes))) => bytes,
                };
                for event in decoder.push(&chunk) {
                    take_event(reply, &event)?;
                }
            }
            if let Some(event) = decoder.finish() {
                take_event(reply, &event)?;
            }
            if reply.finish.is_none() {
                return Err(Attempt::Transient(
                    "the stream ended without a finish reason".into(),
                ));
            }
            Ok(())
        };
        match call.deadline {
            None => work.await,
            Some(deadline) => timeout_at(deadline, work)
                .await
                .unwrap_or(Err(Attempt::CutOff)),
        }
    }

    fn body(&self, call: &Call<'_>) -> Vec<u8> {
        let adapt = self.adapt.lock().unwrap_or_else(|e| e.into_inner());
        let messages = if self.cfg.cache_marks {
            Value::Array(cache_marked(call.messages))
        } else {
            Value::Array(call.messages.to_vec())
        };
        let mut body = json!({
            "model": self.cfg.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !call.tools.is_empty() {
            body["tools"] = Value::Array(call.tools.to_vec());
            body["tool_choice"] = json!(match call.tool_choice {
                ToolChoice::Auto => "auto",
                ToolChoice::None => "none",
            });
        }
        body[adapt.token_param] = json!(adapt.max_output);
        if adapt.send_effort
            && let Some(effort) = &self.cfg.effort
        {
            body["reasoning_effort"] = json!(effort);
        }
        serde_json::to_vec(&body).unwrap_or_default()
    }

    /// Adjusts the request after a `400`, returning what changed, or `None` when nothing applies.
    fn remedy(&self, error: &str) -> Option<&'static str> {
        let lower = error.to_lowercase();
        let mut adapt = self.adapt.lock().unwrap_or_else(|e| e.into_inner());
        if adapt.token_param == "max_completion_tokens"
            && lower.contains("max_completion_tokens")
            && unsupported(&lower)
        {
            adapt.token_param = "max_tokens";
            return Some("sending max_tokens instead");
        }
        if adapt.send_effort
            && (lower.contains("reasoning_effort") || lower.contains("reasoning.effort"))
        {
            adapt.send_effort = false;
            return Some("no longer sending reasoning_effort");
        }
        let names_output = [
            "max_tokens",
            "max_completion_tokens",
            "maxoutputtokens",
            "output tokens",
        ]
        .iter()
        .any(|p| lower.contains(p));
        if names_output && !unsupported(&lower) && adapt.max_output / 2 >= MIN_OUTPUT {
            adapt.max_output /= 2;
            return Some("asking for fewer output tokens");
        }
        None
    }
}

/// Reads one event of the stream into `reply`.
fn take_event(reply: &mut Reply, event: &str) -> Result<(), Attempt> {
    let event = event.trim();
    if event.is_empty() || event == "[DONE]" {
        return Ok(());
    }
    let chunk: Value = serde_json::from_str(event).map_err(|e| {
        Attempt::Transient(format!("the stream sent something that is not JSON: {e}"))
    })?;
    if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_owned);
        let named = ["type", "code"].iter().any(|k| {
            error.get(k).and_then(Value::as_str).is_some_and(|v| {
                matches!(
                    v,
                    "invalid_request_error"
                        | "context_length_exceeded"
                        | "invalid_value"
                        | "invalid_prompt"
                )
            })
        });
        return Err(if named || context_full(&message.to_lowercase()) {
            Attempt::StreamBad(message)
        } else {
            Attempt::Transient(format!(
                "the stream reported an error: {}",
                clip(&message, 300)
            ))
        });
    }
    reply.add_chunk(&chunk);
    Ok(())
}

fn retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 429) || status >= 500
}

/// How endpoints word refusing a parameter outright (as opposed to naming a smaller maximum).
fn unsupported(lower: &str) -> bool {
    [
        "unsupported",
        "unrecognized",
        "unrecognised",
        "unknown",
        "not supported",
        "does not support",
        "not allowed",
        "not permitted",
        "extra inputs",
        "extra fields",
        "unexpected",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// How endpoints word "the conversation no longer fits".
pub fn context_full(text: &str) -> bool {
    let lower = text.to_lowercase();
    let context = [
        "context length",
        "context_length",
        "context window",
        "context size",
        "context limit",
    ]
    .iter()
    .any(|p| lower.contains(p));
    context
        || [
            "maximum context",
            "too many tokens",
            "prompt is too long",
            "maximum prompt length",
            "input is too long",
            "input too long",
        ]
        .iter()
        .any(|p| lower.contains(p))
}

/// A copy of `messages` whose system prompt and newest message carry `cache_control` (two of
/// Claude's four breakpoints), so each turn reads the conversation so far from the cache.
fn cache_marked(messages: &[Value]) -> Vec<Value> {
    let mut out = messages.to_vec();
    let last = out.len().saturating_sub(1);
    let mark = json!({"type": "ephemeral"});
    for i in [0, last] {
        let Some(Value::Object(m)) = out.get_mut(i) else {
            continue;
        };
        match m.get("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                let part = json!([{"type": "text", "text": text, "cache_control": mark}]);
                m.insert("content".into(), part);
            }
            _ => {
                m.insert("cache_control".into(), mark.clone());
            }
        }
    }
    out
}

/// Cuts `text` to at most `limit` characters, keeping the start.
fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str) -> Config {
        Config::from_vars(|name| match name {
            "MODEL_NAME" => Some(model.into()),
            "MODEL_BASE_URL" => Some("http://127.0.0.1:9/v1/".into()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn settings_from_the_environment() {
        let c = cfg("claude-opus-5-5");
        assert_eq!(c.base_url, "http://127.0.0.1:9/v1");
        assert_eq!(c.effort.as_deref(), Some("max"));
        assert!(c.cache_marks);
        assert_eq!(c.max_output_tokens, 32_000);
        let g = cfg("gemini-4-argon");
        assert_eq!(g.effort.as_deref(), Some("high"));
        assert!(!g.cache_marks);
        assert!(Config::from_vars(|_| None).is_err());
    }

    #[test]
    fn claude_requests_mark_the_system_prompt_and_newest_message() {
        let client = Client::new(cfg("claude-opus-5-5")).unwrap();
        let messages = [
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": null, "tool_calls": []}),
            json!({"role": "tool", "tool_call_id": "a", "content": "out"}),
        ];
        let body: Value = serde_json::from_slice(&client.body(&Call {
            messages: &messages,
            tools: &[json!({"type": "function"})],
            tool_choice: ToolChoice::Auto,
            deadline: None,
        }))
        .unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            body["messages"][1]["content"], "task",
            "only two breakpoints"
        );
        assert_eq!(body["messages"][3]["content"][0]["text"], "out");
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["max_completion_tokens"], 32_000);
        assert_eq!(body["tool_choice"], "auto");
        assert!(
            body.get("temperature").is_none(),
            "Opus refuses temperature"
        );
    }

    #[test]
    fn remedies_change_the_request_once_each() {
        let client = Client::new(cfg("gemma-4-26b")).unwrap();
        let e = "Unsupported parameter: 'max_completion_tokens' is not supported with this model.";
        assert!(client.remedy(e).is_some());
        assert_eq!(client.adapt.lock().unwrap().token_param, "max_tokens");
        assert!(client.remedy(e).is_none(), "already switched");
        assert!(
            client
                .remedy("reasoning_effort is not valid for this model")
                .is_some()
        );
        assert!(!client.adapt.lock().unwrap().send_effort);
        assert!(
            client
                .remedy("max_tokens: 32000 > 8192, which is the maximum")
                .is_some()
        );
        assert_eq!(client.adapt.lock().unwrap().max_output, 16_000);
        assert!(client.remedy("something else entirely").is_none());
    }

    #[test]
    fn context_errors_are_recognised() {
        assert!(context_full(
            "This model's maximum context length is 131072 tokens"
        ));
        assert!(context_full(
            "prompt is too long: 250000 tokens > 200000 maximum"
        ));
        assert!(!context_full("rate limit exceeded"));
    }
}
