# Völund

An agent harness for [Physical-Engineering's Last Exam (PLE)](https://github.com/google-research/ple): a WebSocket
server that drives one Google-sponsored model through long-horizon structural, civil, electrical and mechanical
engineering tasks. Written in Rust.

**Status:** M1. The server speaks the serve protocol, the model client streams and retries, and a single tool-calling
loop (ablation A1) works tasks end to end. The world model and phase state machine exist with tests but do not drive
the agent yet (M2). The design (in Thai) is in [DESIGN.md](DESIGN.md).

## Layout

| Crate | What it holds |
|---|---|
| [`volund`](crates/volund) | The server binary: one task per WebSocket connection, the agent loop, the tools |
| [`volund-llm`](crates/volund-llm) | Streaming OpenAI Chat Completions client: retries, request adjustments on `400`, provider fields kept as JSON |
| [`volund-proto`](crates/volund-proto) | Wire types for the PLE serve protocol, version 1, and the checks that keep malformed frames from being sent |
| [`volund-world`](crates/volund-world) | The world model: deliverables, sourced parameters, constraints, app actions, checks, issues; the finish gate and the quality estimate used by the commit gate |
| [`volund-fsm`](crates/volund-fsm) | The phase state machine (`PROBE → INTAKE → MODEL → SOLVE → WRITE → CHECK → VERIFY ↺ FIX → FINALIZE`) as a pure, tested transition function |
| [`volund-mock`](crates/volund-mock) | A scripted stand-in for an OpenAI-compatible model, for tests and for `kit check` without a paid model. Not part of the entry |

`submission.yaml` builds the `volund` binary into `/opt/harness/bin`; `serve.py` and `harness.py` only start it.

## Build and test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The toolchain is pinned in `rust-toolchain.toml` (1.98.1). The session tests in `crates/volund/tests` play whole tasks
over a real WebSocket: a fake evaluator with an in-memory container on one side, the scripted model on the other.

## Checking the entry with the kit, without a paid model

The kit's checks need a model. Until a Vertex AI project is set up, the scripted model can stand in for the proxy: it
answers the kit's model probe, writes `hello.txt` for the smoke task, and keeps working on the cancel task until it
is cancelled. It proves the build, the protocol and cancellation, not the agent's quality.

```bash
cargo build --release -p volund-mock
./target/release/volund-mock --script crates/volund-mock/scripts/kit-check.json --port 4010 &

# from a clone of google-research/ple
MODEL_PROXY_URL=http://localhost:4010 MODEL_PROXY_KEY=mock-local-only \
  uv run python -m kit check ../volund --cancel-test
```

The kit's `mcp` stage warns until the harness uses MCP (M2).

## Settings

Set under `env:` in `submission.yaml`.

| Variable | Default | Meaning |
|---|---|---|
| `REASONING_EFFORT` | the model's highest (`max` for Claude and GLM, `high` otherwise) | Sent with every call; `omit` sends none |
| `MAX_OUTPUT_TOKENS` | `32000` | Output tokens asked for per call (halved automatically if the endpoint refuses) |
| `COMPACT_AT_TOKENS` | `150000` | Prompt size past which tool output the model has already read is trimmed |

## License

Apache-2.0. See [LICENSE](LICENSE).
