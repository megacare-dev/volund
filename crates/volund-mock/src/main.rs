//! `volund-mock --script <file.json> [--port 4010]`: serves a script on 0.0.0.0, which the serve
//! container reaches as `host.docker.internal`. See the library docs for the script format.

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut script, mut port) = (None, 4010u16);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--script" => script = args.next(),
            "--port" => match args.next().and_then(|p| p.parse().ok()) {
                Some(p) => port = p,
                None => return usage("--port needs a number"),
            },
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    let Some(path) = script else {
        return usage("--script is required");
    };
    let script = match std::fs::read_to_string(&path).map(|t| serde_json::from_str(&t)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return usage(&format!("{path} is not JSON: {e}")),
        Err(e) => return usage(&format!("cannot read {path}: {e}")),
    };
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => return usage(&format!("cannot listen on port {port}: {e}")),
    };
    let mock = volund_mock::start(listener, script);
    eprintln!("volund-mock: serving {path} at {}", mock.base_url());
    std::future::pending::<()>().await;
    ExitCode::SUCCESS
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("volund-mock: {problem}\nusage: volund-mock --script <file.json> [--port 4010]");
    ExitCode::from(2)
}
