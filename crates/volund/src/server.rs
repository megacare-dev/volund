//! The WebSocket server: one connection per task. Reads the task, runs the agent, and always ends
//! with `done` (or `error` when there is nothing to report), including after `cancel`, within the
//! evaluator's grace period.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use volund_proto::{Incoming, MAX_FRAME_BYTES, Outgoing};

use crate::ablation::Level;
use crate::agent::Agent;
use crate::driver::Driver;
use crate::link::{Link, Out};

/// After `cancel`, how long the agent may take to hand back its answer before it is stopped. The
/// evaluator waits [`volund_proto::CANCEL_GRACE_SEC`] for `done`.
const ANSWER_AFTER_CANCEL: Duration = Duration::from_secs(2);
/// The longest `final_message` sent.
const FINAL_MESSAGE_CHARS: usize = 100_000;

#[derive(Clone)]
pub struct Settings {
    /// `None` when the harness cannot work (the model variables are missing, or `VOLUND_ABLATION` is
    /// not a level); each task then ends with an `error` that says why.
    pub llm: Option<Arc<volund_llm::Client>>,
    pub llm_problem: String,
    pub compact_at: u64,
    /// Which rung of the ablation ladder works the task (`VOLUND_ABLATION`).
    pub level: Level,
}

/// Serves tasks until the listener fails.
pub async fn serve(listener: TcpListener, settings: Settings) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let settings = settings.clone();
        tokio::spawn(async move {
            if let Err(e) = session(stream, settings).await {
                eprintln!("volund: connection from {peer}: {e}");
            }
        });
    }
}

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;
type Source = SplitStream<WebSocketStream<TcpStream>>;

async fn session(stream: TcpStream, settings: Settings) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    // No keepalive pings of our own (protocol.md §2); frames up to the protocol's limit.
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(config))
        .await
        .map_err(|e| format!("websocket handshake: {e}"))?;
    let (mut sink, mut source) = ws.split();

    let Some(first) = first_text(&mut source).await else {
        return Ok(()); // closed without a task (a listen check)
    };
    let received = Instant::now();
    let task = match serde_json::from_str::<Incoming>(&first) {
        Ok(Incoming::Task(task)) => task,
        _ => {
            let refusal = Outgoing::Error {
                message: "expected a task message first".into(),
            };
            let _ = sink
                .send(Message::text(refusal.to_frame().unwrap_or_default()))
                .await;
            let _ = sink.close().await;
            return Ok(());
        }
    };

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let link = Arc::new(Link::new(task, out_tx, received));
    let writer = tokio::spawn(write_frames(sink, out_rx));
    let reader = tokio::spawn(read_frames(link.clone(), source));

    let mut agent = tokio::spawn({
        let link = link.clone();
        let settings = settings.clone();
        async move {
            match settings.llm {
                Some(llm) if settings.level.phases() => {
                    Driver::new(link, llm, settings.compact_at, settings.level)
                        .run()
                        .await
                }
                Some(llm) => {
                    Agent::new(link, llm, settings.compact_at, settings.level)
                        .run()
                        .await
                }
                None => Err(settings.llm_problem),
            }
        }
    });

    let finished = tokio::select! {
        biased;
        outcome = &mut agent => Some(outcome),
        () = link.cancelled() => None,
    };
    let ending = match finished {
        Some(Ok(Ok(answer))) => done(&link, Some(answer)),
        Some(Ok(Err(problem))) => failed(&link, &problem),
        Some(Err(join)) => failed(&link, &format!("the agent stopped: {join}")),
        None => {
            // cancelled (time is up) or the socket closed: answer within the grace period
            let answer = match timeout(ANSWER_AFTER_CANCEL, &mut agent).await {
                Ok(Ok(Ok(answer))) => Some(answer),
                _ => {
                    agent.abort();
                    None
                }
            };
            done(&link, answer)
        }
    };
    link.send(&ending);
    link.close();
    let _ = timeout(Duration::from_secs(5), writer).await;
    reader.abort();
    Ok(())
}

/// `done` with the agent's answer, or the best summary so far.
fn done(link: &Link, answer: Option<String>) -> Outgoing {
    let text = answer
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| link.final_message());
    let text = if text.trim().is_empty() {
        "(stopped before a summary was written)".to_owned()
    } else {
        text
    };
    Outgoing::Done {
        final_message: crate::tools::clip(&text, FINAL_MESSAGE_CHARS),
    }
}

/// A summary written before a failure still reaches the graders; without one, `error`.
fn failed(link: &Link, problem: &str) -> Outgoing {
    let summary = link.final_message();
    if summary.trim().is_empty() {
        eprintln!("volund: harness error: {problem}");
        Outgoing::Error {
            message: crate::tools::clip(problem, 2000),
        }
    } else {
        link.log(format!("harness error after its summary: {problem}"));
        done(link, Some(summary))
    }
}

async fn first_text(source: &mut Source) -> Option<String> {
    loop {
        match source.next().await? {
            Ok(Message::Text(text)) => return Some(text.to_string()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn read_frames(link: Arc<Link>, mut source: Source) {
    while let Some(message) = source.next().await {
        match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<Incoming>(&text) {
                Ok(Incoming::Result(reply)) => link.deliver(reply),
                Ok(Incoming::Cancel { reason }) => {
                    link.log(format!(
                        "cancel received ({})",
                        reason.as_deref().unwrap_or("no reason")
                    ));
                    link.cancel();
                }
                Ok(Incoming::Task(_)) => link.log("ignored a second task message"),
                Err(e) => eprintln!("volund: skipped a frame that is not a known message: {e}"),
            },
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // nothing more can run once the socket is closed
    link.cancel();
}

async fn write_frames(mut sink: Sink, mut frames: mpsc::UnboundedReceiver<Out>) {
    while let Some(out) = frames.recv().await {
        match out {
            Out::Frame(frame) => {
                if sink.send(Message::text(frame)).await.is_err() {
                    return;
                }
            }
            Out::Close => break,
        }
    }
    let _ = sink.close().await;
}
