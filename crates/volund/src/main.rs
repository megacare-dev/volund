//! `volund`: listens on `0.0.0.0:$PORT` and serves one task per WebSocket connection.

use std::process::ExitCode;
use std::sync::Arc;

use volund::ablation::Level;
use volund::server::{Settings, serve};

#[tokio::main]
async fn main() -> ExitCode {
    let port: u16 = match std::env::var("PORT").ok().and_then(|p| p.parse().ok()) {
        Some(port) => port,
        None => {
            eprintln!("volund: PORT is not set to a port number");
            return ExitCode::from(2);
        }
    };
    // Listen first: the evaluator gives up on a server that has not accepted within 120 s. Missing
    // model settings are reported on each task instead.
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("volund: cannot listen on port {port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (mut llm, mut llm_problem) =
        match volund_llm::Config::from_env().and_then(volund_llm::Client::new) {
            Ok(client) => (Some(Arc::new(client)), String::new()),
            Err(problem) => (None, format!("the model is not configured: {problem}")),
        };
    // A level that is not one would run a different experiment than asked: refuse every task.
    let level = match Level::from_setting(std::env::var("VOLUND_ABLATION").ok().as_deref()) {
        Ok(level) => level,
        Err(problem) => {
            llm = None;
            llm_problem = format!("the harness is not configured: {problem}");
            Level::FULL
        }
    };
    let compact_at = std::env::var("COMPACT_AT_TOKENS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(150_000);
    eprintln!(
        "volund {} (protocol {}) listening on 0.0.0.0:{port}, model {}, ablation {}",
        volund::VERSION,
        volund_proto::PROTOCOL_VERSION,
        llm.as_ref().map_or("(not configured)", |c| c.model()),
        level.name(),
    );
    if !llm_problem.is_empty() {
        eprintln!("volund: {llm_problem}");
    }
    let settings = Settings {
        llm,
        llm_problem,
        compact_at,
        level,
    };
    match serve(listener, settings).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("volund: the listener failed: {e}");
            ExitCode::FAILURE
        }
    }
}
