//! The harness's side of one task connection: sends requests and matches their results by id, keeps
//! at most [`MAX_IN_FLIGHT`] requests running, refuses frames the evaluator would count as malformed,
//! and turns `cancel` (or a closed socket) into a signal every waiting request sees.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::time::Instant;
use volund_proto::{MAX_FRAME_BYTES, MAX_IN_FLIGHT, Outgoing, Reply, Task};

/// What the writer task sends on the socket, in order.
#[derive(Debug)]
pub enum Out {
    Frame(String),
    /// Close the socket once everything before it is sent.
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The task was cancelled or the socket closed; nothing more will run.
    Cancelled,
    /// Not sent: the evaluator would have counted it as malformed. Holds the reason.
    Refused(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled: the task is over"),
            Self::Refused(why) => write!(f, "not sent: {why}"),
        }
    }
}

pub struct Link {
    pub task: Task,
    out: mpsc::UnboundedSender<Out>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    slots: Semaphore,
    next_id: AtomicU64,
    cancelled: watch::Sender<bool>,
    deadline: Option<Instant>,
    final_message: Mutex<String>,
}

impl Link {
    /// `received` is when the task message arrived: its `agent_timeout_sec` counts from then.
    pub fn new(task: Task, out: mpsc::UnboundedSender<Out>, received: Instant) -> Self {
        let deadline = task
            .limits
            .agent_timeout_sec
            .filter(|&s| s > 0)
            .map(|s| received + Duration::from_secs(s));
        Self {
            task,
            out,
            pending: Mutex::new(HashMap::new()),
            slots: Semaphore::new(MAX_IN_FLIGHT),
            next_id: AtomicU64::new(1),
            cancelled: watch::channel(false).0,
            deadline,
            final_message: Mutex::new(String::new()),
        }
    }

    /// When the task's time runs out; `None` when untimed.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn time_left(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Resolves once the task is cancelled or the socket closed.
    pub async fn cancelled(&self) {
        let mut rx = self.cancelled.subscribe();
        let _ = rx.wait_for(|&c| c).await;
    }

    /// Marks the task as over and releases every waiting request.
    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
        self.lock_pending().clear();
    }

    /// Sends one request and waits for its result. `make` receives the id to use.
    pub async fn request(
        &self,
        make: impl FnOnce(String) -> Outgoing,
    ) -> Result<Reply, RequestError> {
        if self.is_cancelled() {
            return Err(RequestError::Cancelled);
        }
        let _slot = tokio::select! {
            slot = self.slots.acquire() => slot.map_err(|_| RequestError::Cancelled)?,
            () = self.cancelled() => return Err(RequestError::Cancelled),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let request = make(id.clone());
        let frame = self.frame(&request).map_err(RequestError::Refused)?;
        let (tx, rx) = oneshot::channel();
        self.lock_pending().insert(id.clone(), tx);
        if self.is_cancelled() || self.out.send(Out::Frame(frame)).is_err() {
            self.lock_pending().remove(&id);
            return Err(RequestError::Cancelled);
        }
        tokio::select! {
            reply = rx => reply.map_err(|_| RequestError::Cancelled),
            () = self.cancelled() => {
                self.lock_pending().remove(&id);
                Err(RequestError::Cancelled)
            }
        }
    }

    /// Hands a result frame to the request waiting for it.
    pub fn deliver(&self, reply: Reply) {
        let waiting = self.lock_pending().remove(&reply.id);
        match waiting {
            Some(tx) => {
                let _ = tx.send(reply);
            }
            None => eprintln!("volund: a result for no waiting request (id {})", reply.id),
        }
    }

    /// Sends a one-way message (`log`, `usage`, `done`, `error`). Returns false if it was not sent.
    pub fn send(&self, message: &Outgoing) -> bool {
        match self.frame(message) {
            Ok(frame) => self.out.send(Out::Frame(frame)).is_ok(),
            Err(why) => {
                eprintln!("volund: did not send a {message:?}: {why}");
                false
            }
        }
    }

    /// A `log` line for the run's log, also printed to the server's stderr.
    pub fn log(&self, text: impl Into<String>) {
        let text = text.into();
        eprintln!("volund: {text}");
        self.send(&Outgoing::Log { text });
    }

    pub fn usage(&self, model: &str, usage: &volund_llm::Usage) {
        self.send(&Outgoing::Usage {
            model: Some(model.to_owned()),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
        });
    }

    /// Asks the writer to close the socket after the frames already queued.
    pub fn close(&self) {
        let _ = self.out.send(Out::Close);
    }

    /// The best summary so far; sent as `final_message` if the task ends before the agent answers.
    pub fn final_message(&self) -> String {
        self.final_message
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_final_message(&self, text: &str) {
        if !text.is_empty() {
            *self.final_message.lock().unwrap_or_else(|e| e.into_inner()) = text.to_owned();
        }
    }

    /// Serializes a message after checking the rules whose breach the evaluator counts as malformed.
    fn frame(&self, message: &Outgoing) -> Result<String, String> {
        message.check()?;
        if let Outgoing::Http { url, .. } = message
            && !self.task.mcp_servers.iter().any(|s| under(url, &s.url))
        {
            return Err(format!("{url} is not one of the task's MCP server URLs"));
        }
        let frame = message.to_frame().map_err(|e| e.to_string())?;
        if frame.len() > MAX_FRAME_BYTES {
            return Err(format!(
                "the frame would be {} bytes, more than the {MAX_FRAME_BYTES} allowed",
                frame.len()
            ));
        }
        Ok(frame)
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Reply>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// True when `url` is `base` or a path or query under it.
fn under(url: &str, base: &str) -> bool {
    let Some(rest) = url.strip_prefix(base) else {
        return false;
    };
    rest.is_empty() || base.ends_with('/') || rest.starts_with('/') || rest.starts_with('?')
}

#[cfg(test)]
mod tests {
    use super::*;
    use volund_proto::{Limits, McpServer};

    fn task(timeout: Option<u64>) -> Task {
        Task {
            protocol: 1,
            session: "s".into(),
            instruction: "i".into(),
            workdir: "/app".into(),
            limits: Limits {
                agent_timeout_sec: timeout,
                ..Limits::default()
            },
            mcp_servers: vec![McpServer {
                name: "apps".into(),
                url: "http://127.0.0.1:8000/mcp/".into(),
                transport: None,
            }],
        }
    }

    #[tokio::test]
    async fn results_reach_their_request_and_ids_count_up() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let link = std::sync::Arc::new(Link::new(task(Some(60)), tx, Instant::now()));
        let l = link.clone();
        let waiting = tokio::spawn(async move {
            l.request(|id| Outgoing::Exec {
                id,
                command: "ls".into(),
                timeout_sec: None,
            })
            .await
        });
        let Some(Out::Frame(frame)) = rx.recv().await else {
            panic!("no frame")
        };
        assert!(frame.contains(r#""id":"1""#), "{frame}");
        link.deliver(serde_json::from_str(r#"{"id":"1","exit_code":0,"output":"a"}"#).unwrap());
        let reply = waiting.await.unwrap().unwrap();
        assert_eq!(reply.fields["output"], "a");
        assert!(link.time_left().unwrap() <= Duration::from_secs(60));
    }

    #[tokio::test]
    async fn malformed_requests_are_never_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let link = Link::new(task(None), tx, Instant::now());
        let empty = link
            .request(|id| Outgoing::Exec {
                id,
                command: String::new(),
                timeout_sec: None,
            })
            .await;
        assert!(matches!(empty, Err(RequestError::Refused(_))));
        let elsewhere = link
            .request(|id| Outgoing::Http {
                id,
                url: "http://example.com/".into(),
                method: None,
                headers: None,
                body_b64: None,
                timeout_sec: None,
            })
            .await;
        assert!(matches!(elsewhere, Err(RequestError::Refused(_))));
        assert!(rx.try_recv().is_err(), "nothing reached the socket");
        assert_eq!(link.time_left(), None);
    }

    #[tokio::test]
    async fn cancel_releases_waiting_requests() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let link = std::sync::Arc::new(Link::new(task(None), tx, Instant::now()));
        let l = link.clone();
        let waiting = tokio::spawn(async move {
            l.request(|id| Outgoing::ReadFile {
                id,
                path: "a".into(),
            })
            .await
        });
        tokio::task::yield_now().await;
        link.cancel();
        assert_eq!(waiting.await.unwrap(), Err(RequestError::Cancelled));
        assert!(link.is_cancelled());
    }

    #[test]
    fn mcp_urls_match_paths_under_a_server() {
        let base = "http://127.0.0.1:8000/mcp/";
        assert!(under(base, base));
        assert!(under("http://127.0.0.1:8000/mcp/x?y=1", base));
        assert!(!under("http://127.0.0.1:8001/mcp/", base));
        assert!(under("http://h/mcp?x", "http://h/mcp"));
        assert!(!under("http://h/mcpx", "http://h/mcp"));
    }
}
