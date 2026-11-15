//! volund: the WebSocket server that drives one model through each PLE task.
//!
//! Not wired yet. Milestone M1 in DESIGN.md adds the serve loop over `volund-proto`, the model client,
//! and the phase driver over `volund-fsm` and `volund-world`.

fn main() {
    eprintln!(
        "volund {} (protocol {}): the serve loop is not implemented yet; see DESIGN.md, M1. \
         A run would start in phase {:?}.",
        env!("CARGO_PKG_VERSION"),
        volund_proto::PROTOCOL_VERSION,
        volund_fsm::Phase::Probe,
    );
    std::process::exit(2);
}
