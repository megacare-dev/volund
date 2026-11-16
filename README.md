# Völund

An agent harness for [Physical-Engineering's Last Exam (PLE)](https://github.com/google-research/ple): a WebSocket
server that drives one Google-sponsored model through long-horizon structural, civil, electrical and mechanical
engineering tasks. Written in Rust.

**Status:** M2. A phase driver works each task: the state machine in `volund-fsm` picks the phase, the model records
the task in the world model through `world_*` tools, scripted phases probe the container and check the deliverables,
and a fresh conversation verifies the work. The task's applications are reached over MCP. `VOLUND_ABLATION` picks a
lower rung of the ablation ladder (below). The design (in Thai) is in [DESIGN.md](DESIGN.md); a page for the team is
[docs/design.html](docs/design.html).

## Layout

| Crate | What it holds |
|---|---|
| [`volund`](crates/volund) | The server binary: one task per WebSocket connection, the phase driver (and the single loop), the tools |
| [`volund-mcp`](crates/volund-mcp) | MCP client over the protocol's `http` request, and how the task's tools are offered (directly, or through `mcp_find`/`mcp_call`) |
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
python3 -m unittest discover -s tools
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

With the phase driver (16 Nov) every stage passes against the scripted model, `mcp` included; only `artifacts` warns until the first eval run.

## Settings

Set under `env:` in `submission.yaml`.

| Variable | Default | Meaning |
|---|---|---|
| `VOLUND_ABLATION` | `A6` (the full harness) | `A1` to `A5` run a lower rung of the ablation ladder; any other value makes every task end with an `error` |
| `REASONING_EFFORT` | the model's highest (`max` for Claude and GLM, `high` otherwise) | Sent with every call; `omit` sends none |
| `MAX_OUTPUT_TOKENS` | `32000` | Output tokens asked for per call (halved automatically if the endpoint refuses) |
| `COMPACT_AT_TOKENS` | `150000` | Prompt size past which tool output the model has already read is trimmed |

## Ablation levels

Each level adds one part to the level below it (DESIGN.md §9), so two neighbouring levels differ in that part alone.

| Level | What works the task |
|---|---|
| `A1` | One tool-calling loop, like the organisers' `custom-openai` sample (A0) |
| `A2` | The loop, opening with what PROBE found: the input files and the container's tools |
| `A3` | The loop with the world: `world_*` tools, the record re-rendered when old turns are dropped, a summary built from it on cancel |
| `A4` | The phase driver: PROBE → INTAKE → WRITE → CHECK ↺ FIX → FINALIZE, finishing through the finish gate |
| `A5` | A4 with MODEL and SOLVE (the executable model) for tasks with constraints |
| `A6` | A5 with VERIFY in a fresh conversation and the Δ≥0 commit gate on every FIX: the entry |

## Run logs to CSV

The harness logs one JSON line per event (`kind`: `start`, `probe`, `transition`, `tool`, `llm`, `check`, `context`,
`model_tests`, `rollback`, `verify_changed`). After an `eval/run.py` run:

```bash
python3 tools/logs_to_csv.py runs/<run> -o csv/
```

It reads each trial's `agent/serve.log` (or the kit's `<name>.txt` when serve.log holds no events) and the run's
`results.json`, and writes `events.csv` (one row per event), `phases.csv` (per trial and phase: visits, seconds, tool
and model calls, tokens) and `tasks.csv` (per trial: level, model, score, the phases it went through, the last
transition's reason, FIX rounds, rollbacks, tokens).

## License

Apache-2.0. See [LICENSE](LICENSE).
