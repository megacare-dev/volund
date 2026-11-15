# Völund

An agent harness for [Physical-Engineering's Last Exam (PLE)](https://github.com/google-research/ple): a WebSocket
server that drives one Google-sponsored model through long-horizon structural, civil, electrical and mechanical
engineering tasks. Written in Rust.

**Status:** skeleton. The world model, the phase state machine and the serve-protocol types exist with tests; the
server, model client and phase driver do not yet. The design (in Thai) is in [DESIGN.md](DESIGN.md).

## Layout

| Crate | What it holds |
|---|---|
| [`volund-world`](crates/volund-world) | The world model: deliverables, sourced parameters, constraints, app actions, checks, issues; the finish gate and the quality estimate used by the commit gate |
| [`volund-fsm`](crates/volund-fsm) | The phase state machine (`PROBE → INTAKE → MODEL → SOLVE → WRITE → CHECK → VERIFY ↺ FIX → FINALIZE`) as a pure, tested transition function |
| [`volund-proto`](crates/volund-proto) | Wire types for the PLE serve protocol, version 1 |
| [`volund`](crates/volund) | The server binary (stub) |

## Build and test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The toolchain is pinned in `rust-toolchain.toml` (1.98.1).

## License

Apache-2.0. See [LICENSE](LICENSE).
